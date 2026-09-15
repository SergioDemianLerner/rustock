//! Measure the miss rate of a sliding-window trie cache against window length.
//!
//! The segmented-build design keeps the last few block-ranges' nodes in RAM and
//! falls back to disk (or the 131 GB archive) when a read reaches further back.
//! Whether that is fast or a disaster turns on one number nobody has measured:
//! what fraction of reads touch state older than the resident window.
//!
//! For every read this records the *age* of the node -- how many blocks ago it
//! was last written during this replay -- and histograms it by power of two. A
//! window of W blocks then misses on everything with age >= W, plus everything
//! never written during the replay at all. Those two are reported separately
//! because they mean different things: the first shrinks as you enlarge the
//! window, the second is genuinely cold state and is the floor.
//!
//! The floor measured over an M-block sample is an *upper bound* on what a real
//! worker would see, since a worker running over a 1.4M-block range will have
//! written much of that state itself.
//!
//! Usage: trie_window <block-dir> <trie-dir> <start> <count> <warmup>

use rustock_core::Block;
use rustock_execution::{BlockProcessor, RskHardforkConfig};
use rustock_storage::{BlockStore, RocksDbTrieStore};
use rustock_trie::{TrieNode, TrieStore};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

const BUCKETS: usize = 40;

struct WindowProbe {
    inner: Arc<dyn TrieStore>,
    /// key prefix -> block at which it was last written in this replay.
    written: Mutex<HashMap<u64, u32>>,
    block: AtomicU64,
    /// Counting starts only after warm-up, so the cold start is not charged to
    /// the steady state.
    counting: AtomicU64,
    /// age histogram, bucket i = ages in [2^i, 2^(i+1))
    ages: Mutex<Vec<u64>>,
    never: AtomicU64,
    reads: AtomicU64,
    writes: AtomicU64,
}

impl WindowProbe {
    fn new(inner: Arc<dyn TrieStore>) -> Self {
        Self {
            inner,
            written: Mutex::new(HashMap::new()),
            block: AtomicU64::new(0),
            counting: AtomicU64::new(0),
            ages: Mutex::new(vec![0; BUCKETS]),
            never: AtomicU64::new(0),
            reads: AtomicU64::new(0),
            writes: AtomicU64::new(0),
        }
    }
    fn prefix(key: &[u8]) -> u64 {
        let mut b = [0u8; 8];
        let n = key.len().min(8);
        b[..n].copy_from_slice(&key[..n]);
        u64::from_be_bytes(b)
    }
}

impl TrieStore for WindowProbe {
    fn get(&self, key: &[u8]) -> Option<Vec<u8>> {
        if self.counting.load(Ordering::Relaxed) == 1 {
            self.reads.fetch_add(1, Ordering::Relaxed);
            let now = self.block.load(Ordering::Relaxed) as u32;
            match self.written.lock().unwrap().get(&Self::prefix(key)) {
                Some(&b) => {
                    let age = now.saturating_sub(b) as u64;
                    // age 0 (written this block) lands in bucket 0 alongside
                    // age 1; both are hits for every window of interest.
                    let idx = (64 - (age + 1).leading_zeros()) as usize - 1;
                    self.ages.lock().unwrap()[idx.min(BUCKETS - 1)] += 1;
                }
                None => { self.never.fetch_add(1, Ordering::Relaxed); }
            }
        }
        self.inner.get(key)
    }

    fn put(&self, key: &[u8], _value: &[u8]) {
        self.writes.fetch_add(1, Ordering::Relaxed);
        let now = self.block.load(Ordering::Relaxed) as u32;
        self.written.lock().unwrap().insert(Self::prefix(key), now);
    }
}

fn main() -> anyhow::Result<()> {
    let mut a = std::env::args().skip(1);
    let block_dir = a.next().expect("usage: trie_window <block-dir> <trie-dir> <start> <count> <warmup>");
    let trie_dir = a.next().expect("trie-dir");
    let start: u64 = a.next().expect("start").parse()?;
    let count: u64 = a.next().expect("count").parse()?;
    let warmup: u64 = a.next().expect("warmup").parse()?;

    let store = Arc::new(BlockStore::open_read_only(&block_dir)?);
    let backing: Arc<dyn TrieStore> = Arc::new(RocksDbTrieStore::open_read_only(&trie_dir)?);
    let probe = Arc::new(WindowProbe::new(backing.clone()));

    let first = store.header(store.canonical_hash(start)?.expect("hash"))?.expect("header");
    let parent = store.header(first.parent_hash)?.expect("parent");
    let data = backing.get(parent.state_root.as_slice())
        .ok_or_else(|| anyhow::anyhow!("state root for #{} not in {trie_dir}", parent.number))?;
    let mut root = TrieNode::from_message(&data, backing.as_ref());

    let processor = BlockProcessor::new(RskHardforkConfig::mainnet(), store.clone());
    let probe_dyn: Arc<dyn TrieStore> = probe.clone();
    let t0 = std::time::Instant::now();

    for n in start..start + count {
        probe.block.store(n - start, Ordering::Relaxed);
        if n - start == warmup {
            probe.counting.store(1, Ordering::Relaxed);
            eprintln!("warm-up done at #{n}; counting from here");
        }
        let hash = match store.canonical_hash(n)? { Some(h) => h, None => break };
        let header = match store.header(hash)? { Some(h) => h, None => break };
        let (txs, oms) = match store.body(hash)? { Some(b) => b, None => break };
        let p = processor.execute_block(&Block { header, transactions: txs, ommers: oms }, &root, probe_dyn.clone())?;
        root = p.new_state_root;
        if (n - start) % 5000 == 0 {
            eprintln!("#{n} ({}/{count}) {:.2} blk/s, {} tracked keys",
                      n - start, (n - start + 1) as f64 / t0.elapsed().as_secs_f64(),
                      probe.written.lock().unwrap().len());
        }
    }

    let ages = probe.ages.lock().unwrap().clone();
    let never = probe.never.load(Ordering::Relaxed);
    let reads = probe.reads.load(Ordering::Relaxed);
    let total: u64 = ages.iter().sum::<u64>() + never;
    println!("\n=== #{start} + {count} blocks (stats after {warmup} warm-up) ===");
    println!("counted reads {reads}, writes {}", probe.writes.load(Ordering::Relaxed));
    println!("never written in this replay: {never} ({:.3}% -- the cold floor)",
             never as f64 * 100.0 / total.max(1) as f64);
    println!("\n{:>12} {:>14} {:>12}", "window W", "miss rate", "misses/block");
    for k in 8..=18 {
        let w = 1u64 << k;
        let older: u64 = ages.iter().skip(k).sum();
        let miss = older + never;
        println!("{:>12} {:>13.3}% {:>12.1}",
                 format!("{} blk", w), miss as f64 * 100.0 / total.max(1) as f64,
                 miss as f64 / (count - warmup) as f64);
    }
    Ok(())
}
