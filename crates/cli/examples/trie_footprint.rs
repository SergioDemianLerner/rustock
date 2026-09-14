//! Measure the trie working set of a range of blocks.
//!
//! Answers the question a segmented ("locality") trie store has to be sized
//! against: if segment D(i) is built by copying forward every node the blocks
//! in its range touch, how big does it get, and how fast?
//!
//! For each block it records the set of distinct node keys read or written
//! since the range started -- exactly what a segment would have accumulated --
//! and reports the cumulative count and byte total. The shape of that curve is
//! the whole answer: an initial jump as the hot working set is pulled in,
//! then a roughly linear climb at the marginal cost of one block.
//!
//! Nothing is written. Blocks come from a read-only handle so a live node can
//! keep running.
//!
//! Usage:
//!   trie_footprint <block-dir> <trie-dir> <start-block> <count> [csv-out]

use rustock_core::Block;
use rustock_execution::{BlockProcessor, RskHardforkConfig};
use rustock_storage::{BlockStore, RocksDbTrieStore};
use rustock_trie::{TrieNode, TrieStore};
use std::collections::HashSet;
use std::sync::{Arc, Mutex};

/// Wraps a store and records which keys were touched.
///
/// Keys are 32-byte hashes; the set holds the leading 8 bytes instead, which
/// costs a quarter of the memory and, at the tens of millions of keys this
/// walks, collides with probability on the order of 1e-5 -- immaterial for a
/// measurement whose other error bars are percentages.
struct Recorder {
    inner: Arc<dyn TrieStore>,
    touched: Mutex<HashSet<u64>>,
    bytes: Mutex<u64>,
    reads: Mutex<u64>,
    misses: Mutex<u64>,
}

impl Recorder {
    fn new(inner: Arc<dyn TrieStore>) -> Self {
        Self {
            inner,
            touched: Mutex::new(HashSet::new()),
            bytes: Mutex::new(0),
            reads: Mutex::new(0),
            misses: Mutex::new(0),
        }
    }

    fn prefix(key: &[u8]) -> u64 {
        let mut b = [0u8; 8];
        let n = key.len().min(8);
        b[..n].copy_from_slice(&key[..n]);
        u64::from_be_bytes(b)
    }

    fn record(&self, key: &[u8], len: usize) {
        if self.touched.lock().unwrap().insert(Self::prefix(key)) {
            // A segment stores the value plus its 32-byte key, and RocksDB adds
            // per-entry overhead on top; count key + value and report the raw
            // figure, leaving the store's own overhead to be applied after.
            *self.bytes.lock().unwrap() += (len + key.len()) as u64;
        }
    }

    fn distinct(&self) -> usize {
        self.touched.lock().unwrap().len()
    }
}

impl TrieStore for Recorder {
    fn get(&self, key: &[u8]) -> Option<Vec<u8>> {
        *self.reads.lock().unwrap() += 1;
        match self.inner.get(key) {
            Some(v) => {
                self.record(key, v.len());
                Some(v)
            }
            None => {
                *self.misses.lock().unwrap() += 1;
                None
            }
        }
    }

    fn put(&self, key: &[u8], value: &[u8]) {
        // Writes land in the segment too, so they count towards its size.
        self.record(key, value.len());
    }
}

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let block_dir = args.next().expect("usage: trie_footprint <block-dir> <trie-dir> <start> <count> [csv]");
    let trie_dir = args.next().expect("trie-dir");
    let start: u64 = args.next().expect("start-block").parse()?;
    let count: u64 = args.next().expect("count").parse()?;
    let csv_path = args.next();

    let store = Arc::new(BlockStore::open_read_only(&block_dir)?);
    // Read-only: several of these run at once over disjoint block ranges, and
    // a read-write open would take the directory lock and exclude the others.
    let backing: Arc<dyn TrieStore> = Arc::new(RocksDbTrieStore::open_read_only(&trie_dir)?);
    let recorder = Arc::new(Recorder::new(backing.clone()));

    // Start from the parent's committed state root, which the header names.
    let first_hash = store.canonical_hash(start)?.expect("canonical hash at start");
    let first = store.header(first_hash)?.expect("header at start");
    let parent = store.header(first.parent_hash)?.expect("parent header");
    let root_data = backing
        .get(parent.state_root.as_slice())
        .ok_or_else(|| anyhow::anyhow!("state root {:?} for #{} is not in {trie_dir}",
                                        parent.state_root, parent.number))?;

    // Read the root through the *backing* store so the range's own first touch
    // is recorded by the replay, not by this setup step.
    let mut root = TrieNode::from_message(&root_data, backing.as_ref());

    let processor = BlockProcessor::new(RskHardforkConfig::mainnet(), store.clone());
    let recorder_dyn: Arc<dyn TrieStore> = recorder.clone();

    let mut csv = String::from("block,txs,cum_nodes,cum_bytes,new_nodes,new_bytes,elapsed_s\n");
    let mut prev_nodes = 0usize;
    let mut prev_bytes = 0u64;
    let t0 = std::time::Instant::now();
    let mut mismatches = 0u64;

    for n in start..start + count {
        let hash = match store.canonical_hash(n)? {
            Some(h) => h,
            None => { eprintln!("no canonical hash at #{n}; stopping"); break; }
        };
        let header = match store.header(hash)? { Some(h) => h, None => break };
        let (transactions, ommers) = match store.body(hash)? { Some(b) => b, None => {
            eprintln!("no body for #{n}; stopping"); break; } };
        let txs = transactions.len();
        let block = Block { header, transactions, ommers };

        let processed = processor.execute_block(&block, &root, recorder_dyn.clone())?;
        if processed.state_root_hash.as_slice() != block.header.state_root.as_slice() {
            mismatches += 1;
            eprintln!("#{n} STATE ROOT MISMATCH computed={:?} header={:?}",
                      processed.state_root_hash, block.header.state_root);
        }
        root = processed.new_state_root;

        let nodes = recorder.distinct();
        let bytes = *recorder.bytes.lock().unwrap();
        csv.push_str(&format!(
            "{n},{txs},{nodes},{bytes},{},{},{:.1}\n",
            nodes - prev_nodes, bytes - prev_bytes, t0.elapsed().as_secs_f64()
        ));
        prev_nodes = nodes;
        prev_bytes = bytes;

        if (n - start) % 100 == 0 || n == start + count - 1 {
            let done = n - start + 1;
            eprintln!(
                "#{n} ({done}/{count}) | cumulative {} nodes, {:.1} MB | {:.2} blocks/s",
                nodes, bytes as f64 / 1e6, done as f64 / t0.elapsed().as_secs_f64()
            );
        }
    }

    let nodes = recorder.distinct();
    let bytes = *recorder.bytes.lock().unwrap();
    let reads = *recorder.reads.lock().unwrap();
    let misses = *recorder.misses.lock().unwrap();
    eprintln!("\n=== footprint of #{start}..#{} ===", start + count - 1);
    eprintln!("distinct nodes touched : {nodes}");
    eprintln!("raw key+value bytes    : {bytes} ({:.2} GB)", bytes as f64 / 1e9);
    eprintln!("mean bytes per node    : {:.1}", bytes as f64 / nodes.max(1) as f64);
    eprintln!("total get() calls      : {reads} ({misses} missing)");
    eprintln!("state root mismatches  : {mismatches}");

    if let Some(path) = csv_path {
        std::fs::write(&path, csv)?;
        eprintln!("per-block CSV written to {path}");
    }
    Ok(())
}
