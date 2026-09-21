//! Replay a block range against one trie store and report throughput.
//!
//! Exists to answer one question on the same terms as the rskj side's
//! `ReplayBenchmarkTest`: how fast does *this* node re-execute a given range
//! reading state from a given store? Everything that is not that is switched
//! off --
//!
//!   - supply conservation checks (both per-transaction and per-block), which
//!     rskj does not perform and which would load the comparison,
//!   - trie writes, discarded rather than persisted,
//!   - receipts, bodies indexing, alerts: never touched.
//!
//! The run fails loudly on the first state-root mismatch. A throughput number
//! from a replay that diverged is worthless.
//!
//! Usage: replay_bench <block-dir> <trie-dir> <from> <to> [--cache N]
//!   --cache N   put an N-entry node cache in front of the store, to mirror
//!               rskj's `--Xcache.states.max-elements`. Omit for none.

use rustock_core::Block;
use rustock_execution::{BlockProcessor, RskHardforkConfig};
use rustock_storage::{BlockStore, RocksDbTrieStore};
use rustock_trie::{TrieNode, TrieStore};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// Reads through to the real store; silently drops every write.
struct ReadOnly(Arc<dyn TrieStore>);

impl TrieStore for ReadOnly {
    fn get(&self, key: &[u8]) -> Option<Vec<u8>> {
        self.0.get(key)
    }
    fn put(&self, _key: &[u8], _value: &[u8]) {}
    fn flush(&self) {}
}

/// A bounded node cache, the analogue of rskj's states cache.
///
/// Flat cap with a wholesale clear rather than an LRU: an eviction policy
/// costs more to maintain than it saves when the working set re-warms in a
/// block or two, and the point here is to measure the store, not the cache.
struct NodeCache {
    inner: Arc<dyn TrieStore>,
    map: Mutex<HashMap<Vec<u8>, Vec<u8>>>,
    cap: usize,
    hits: AtomicU64,
    reads: AtomicU64,
}

impl TrieStore for NodeCache {
    fn get(&self, key: &[u8]) -> Option<Vec<u8>> {
        self.reads.fetch_add(1, Ordering::Relaxed);
        {
            let m = self.map.lock().unwrap();
            if let Some(v) = m.get(key) {
                self.hits.fetch_add(1, Ordering::Relaxed);
                return Some(v.clone());
            }
        }
        let v = self.inner.get(key)?;
        let mut m = self.map.lock().unwrap();
        if m.len() >= self.cap {
            m.clear();
        }
        m.insert(key.to_vec(), v.clone());
        Some(v)
    }
    fn put(&self, _key: &[u8], _value: &[u8]) {}
    fn flush(&self) {}
}

fn main() -> anyhow::Result<()> {
    let a: Vec<String> = std::env::args().collect();
    if a.len() < 5 {
        eprintln!("usage: replay_bench <block-dir> <trie-dir> <from> <to> [--cache N]");
        std::process::exit(2);
    }
    let (block_dir, trie_dir) = (a[1].clone(), a[2].clone());
    let from: u64 = a[3].parse()?;
    let to: u64 = a[4].parse()?;
    let cache: Option<usize> = a.iter().position(|x| x == "--cache")
        .and_then(|i| a.get(i + 1)).and_then(|s| s.parse().ok());

    // The whole point of this binary: measure replay, not verification extras.
    rustock_execution::supply::set_per_transaction(false);
    rustock_execution::supply::set_per_block(false);

    let t_open = Instant::now();
    let store = Arc::new(BlockStore::open_read_only(&block_dir)?);
    let backing: Arc<dyn TrieStore> = Arc::new(RocksDbTrieStore::open_read_only(&trie_dir)?);
    let cached: Option<Arc<NodeCache>> = cache.map(|cap| {
        Arc::new(NodeCache {
            inner: backing.clone(),
            map: Mutex::new(HashMap::with_capacity(cap / 4)),
            cap,
            hits: AtomicU64::new(0),
            reads: AtomicU64::new(0),
        })
    });
    let source: Arc<dyn TrieStore> = match &cached {
        Some(c) => c.clone(),
        None => backing.clone(),
    };
    let trie_store: Arc<dyn TrieStore> = Arc::new(ReadOnly(source.clone()));
    let open_secs = t_open.elapsed().as_secs_f64();

    // Seed from the parent of the first block, as any consumer must.
    let seed_hash = store.canonical_hash(from - 1)?.expect("canonical hash");
    let seed = store.header(seed_hash)?.expect("header");
    let root_data = source.get(seed.state_root.as_slice()).unwrap_or_else(|| {
        panic!("seed state root {} for #{} is not in {trie_dir}", seed.state_root, from - 1)
    });
    let mut root = TrieNode::from_message(&root_data, trie_store.as_ref());

    let processor = BlockProcessor::new(RskHardforkConfig::mainnet(), store.clone());
    let started = Instant::now();
    let mut done = 0u64;

    for n in from..=to {
        let hash = store.canonical_hash(n)?.expect("canonical hash");
        let header = store.header(hash)?.expect("header");
        let (transactions, ommers) = store.body(hash)?.expect("body");
        let expected = header.state_root;
        let block = Block { header, transactions, ommers };
        let p = processor.execute_block(&block, &root, trie_store.clone())?;
        if p.state_root_hash.as_slice() != expected.as_slice() {
            eprintln!("#{n}: STATE ROOT MISMATCH computed {} header {}", p.state_root_hash, expected);
            std::process::exit(1);
        }
        root = p.new_state_root;
        done += 1;
    }

    let secs = started.elapsed().as_secs_f64();
    let total = secs + open_secs;
    println!("store      {trie_dir}");
    println!("range      #{from}..#{to}  ({done} blocks, all roots matched)");
    println!("cache      {}", cache.map(|c| format!("{c} nodes")).unwrap_or("none".into()));
    println!("open       {open_secs:.1} s");
    println!("replay     {secs:.1} s");
    println!("wall       {total:.1} s");
    println!("blocks/s   {:.2}   (replay only: {:.2})", done as f64 / total, done as f64 / secs);
    if let Some(c) = &cached {
        let (r, h) = (c.reads.load(Ordering::Relaxed), c.hits.load(Ordering::Relaxed));
        println!("node reads {r}, cache hits {h} ({:.1}%)", 100.0 * h as f64 / r.max(1) as f64);
    }
    Ok(())
}
