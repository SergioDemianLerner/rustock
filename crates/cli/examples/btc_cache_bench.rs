//! Measure BTC main-chain block retrieval with and without the block cache,
//! against a real database.
//!
//! **Everything is opened read-only.** The block store and every epoch of the
//! trie use RocksDB's read-only mode, which takes no lock, replays no WAL and
//! writes nothing. This cannot damage the database it is pointed at, which is
//! the only acceptable property for a tool aimed at a live 130 GB store.
//!
//! ```text
//! cargo run --release --example btc_cache_bench -- \
//!     --data-dir /var/lib/rustock --trie-dir /var/lib/rustock/trie-epochs
//! ```
//!
//! The node must be **stopped**: read-only mode does not replay the
//! write-ahead log, so a running node's most recent writes would be invisible.

use alloy_primitives::B256;
use anyhow::{Context, Result};
use rustock_execution::bridge::btc_chain::{main_chain_block_at_height, MainChainLookup};
use rustock_execution::BtcBlockCache;
use rustock_storage::epoch_store::{EpochConfig, EpochTrieStore};
use rustock_storage::BlockStore;
use rustock_trie::{TrieNode, TrieStore};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Instant;

/// Counts reads so the table can report trie nodes touched, not just wall time.
/// Wall time on a warm page cache says little; node count is what the snapshot
/// work actually cares about.
struct Counting {
    inner: Arc<dyn TrieStore>,
    reads: AtomicUsize,
}

impl Counting {
    fn new(inner: Arc<dyn TrieStore>) -> Self {
        Self { inner, reads: AtomicUsize::new(0) }
    }
    fn take(&self) -> usize {
        self.reads.swap(0, Ordering::Relaxed)
    }
}

impl TrieStore for Counting {
    fn get(&self, key: &[u8]) -> Option<Vec<u8>> {
        self.reads.fetch_add(1, Ordering::Relaxed);
        self.inner.get(key)
    }
    fn put(&self, _key: &[u8], _value: &[u8]) {
        panic!("the benchmark must never write");
    }
}

fn arg(name: &str, default: &str) -> String {
    let args: Vec<String> = std::env::args().collect();
    args.windows(2)
        .find(|w| w[0] == format!("--{name}"))
        .map(|w| w[1].clone())
        .unwrap_or_else(|| default.to_string())
}

fn main() -> Result<()> {
    let data_dir = arg("data-dir", "/var/lib/rustock");
    let trie_dir = arg("trie-dir", "/var/lib/rustock/trie-epochs");

    println!("Opening READ-ONLY:\n  blocks {data_dir}\n  trie   {trie_dir}\n");

    let store = BlockStore::open_read_only(&data_dir)
        .with_context(|| format!("opening {data_dir} read-only"))?;

    // The epoch config only affects rotation and sweeping, neither of which a
    // read-only store performs; the values just have to pass validation.
    let cfg = EpochConfig { epochs: 4, burial_depth: 4_000, rotate_bytes: 1 << 30 };
    let epochs = EpochTrieStore::open_read_only(&trie_dir, cfg)
        .with_context(|| format!("opening {trie_dir} read-only"))?;
    let raw: Arc<dyn TrieStore> = Arc::new(epochs);
    let counting = Arc::new(Counting::new(raw));
    let trie: Arc<dyn TrieStore> = counting.clone();

    let (exec_hash, state_root_hash) = store
        .exec_head()?
        .context("the database has no executed head")?;
    let header = store.header(exec_hash)?.context("executed head header missing")?;
    println!("Executed head #{} state root {state_root_hash}", header.number);

    // Read-only mode does not replay the write-ahead log, so the newest state
    // roots may still be in a memtable and invisible here. Walk back to the
    // newest root that is actually present in the SSTs -- any committed root
    // answers the question this benchmark asks, and none of this writes.
    let mut probe = header.clone();
    let mut root_data = None;
    for _ in 0..5_000 {
        if let Some(d) = trie.get(probe.state_root.as_slice()) {
            root_data = Some((d, probe.number, probe.state_root));
            break;
        }
        let Some(parent) = store.header(probe.parent_hash)? else { break };
        probe = parent;
    }
    let (root_data, root_number, root_hash) = root_data.context(
        "no state root within 5,000 blocks of the executed head is present in the trie \
         store; is the node stopped and flushed?",
    )?;
    if root_number != header.number {
        println!(
            "  (newest root in the SSTs is #{root_number}, {} blocks back: read-only mode \
             does not replay the WAL)",
            header.number - root_number
        );
    }
    let _ = root_hash;
    let state_root = TrieNode::from_message(&root_data, trie.as_ref());
    counting.take();

    // The BTC chain head, so depths can be turned into heights.
    let btc_head = {
        let mut ctx = ctx_over(trie.clone(), state_root.clone(), None);
        rustock_execution::bridge::btc_store::load_chain_head(&mut ctx)
            .context("no BTC chain head in Bridge storage at this state root")?
    };
    counting.take();
    println!("BTC chain head #{}\n", btc_head.height);

    let depths: Vec<u32> = [1u32, 10, 100, 1_000, 5_000]
        .into_iter()
        .filter(|d| *d < btc_head.height)
        .collect();

    println!(
        "{:>8} {:>10} {:>14} {:>14} {:>14} {:>14}",
        "depth", "height", "reads (cold)", "reads (warm)", "ms (cold)", "ms (warm)"
    );
    println!("{}", "-".repeat(80));

    for depth in &depths {
        let height = btc_head.height - depth;

        // Cold: no cache at all — today's behaviour.
        let mut plain = ctx_over(trie.clone(), state_root.clone(), None);
        counting.take();
        let t0 = Instant::now();
        let a = main_chain_block_at_height(&mut plain, height, false, None);
        let cold_ms = t0.elapsed().as_secs_f64() * 1000.0;
        let cold_reads = counting.take();

        // Warm: a cache, queried twice. The first fills it, the second is what
        // a repeated or nearby query costs.
        let cache = Arc::new(BtcBlockCache::with_default_capacity());
        let mut cached = ctx_over(trie.clone(), state_root.clone(), Some(cache.clone()));
        let _ = main_chain_block_at_height(&mut cached, height, false, None);
        counting.take();
        let t1 = Instant::now();
        let b = main_chain_block_at_height(&mut cached, height, false, None);
        let warm_ms = t1.elapsed().as_secs_f64() * 1000.0;
        let warm_reads = counting.take();

        assert_eq!(
            describe(&a),
            describe(&b),
            "cached and uncached disagreed at height {height}"
        );

        println!(
            "{depth:>8} {height:>10} {cold_reads:>14} {warm_reads:>14} {cold_ms:>14.2} {warm_ms:>14.2}"
        );
    }

    println!(
        "\nThe warm floor is the chain-head read, which is deliberately NOT cached: it is\n\
         mutable, so a reorg would have to invalidate it. Everything above that floor is\n\
         the walk, and the walk is what the cache removes.\n\
         Every cached entry was read because consensus asked for it: nothing is pre-loaded."
    );
    Ok(())
}

fn describe(l: &MainChainLookup) -> String {
    match l {
        MainChainLookup::Found(b) => format!("Found(#{}, {})", b.height, b.header.block_hash()),
        MainChainLookup::Absent => "Absent".into(),
        MainChainLookup::StoreError => "StoreError".into(),
    }
}

fn ctx_over(
    trie: Arc<dyn TrieStore>,
    root: TrieNode,
    cache: Option<Arc<BtcBlockCache>>,
) -> impl rustock_execution::RskContextTr {
    rustock_execution::bridge_read_context(trie, root, cache)
}
