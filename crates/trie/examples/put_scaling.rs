//! Does the cost of a block's puts grow with the number of puts in it?
//!
//! `apply_state_changes` makes every account's put against the trie the
//! previous put returned. Each put rebuilds its path, and at every level
//! `internal_put` both *resolves* the child it descends into and *clones*
//! the sibling. For a `NodeRef::Node` -- which is what the previous puts
//! in this block left behind -- each of those is a deep copy of a whole
//! in-memory subtree. So put number k copies what puts 1..k built.
//!
//! If that is what dominates, microseconds per put rise linearly with the
//! number of puts, and the block is quadratic. If puts are independent,
//! the figure is flat.
//!
//! Usage: cargo run --release --example put_scaling [base_keys]

use rustock_trie::{MemoryTrieStore, TrieKeySlice, TrieNode, TrieStore};
use sha3::{Digest, Keccak256};
use std::time::Instant;

fn key(tag: &[u8], i: u64) -> Vec<u8> {
    let mut h = Keccak256::new();
    h.update(tag);
    h.update(i.to_be_bytes());
    h.finalize()[..31].to_vec()
}

fn main() {
    let base_keys: u64 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(200_000);

    // A base trie deep enough to be representative, saved and then dropped
    // to a bare hash so every child starts as `NodeRef::Hash` -- the state
    // a block actually begins from.
    let store = MemoryTrieStore::new();
    eprint!("building a base trie of {base_keys} keys... ");
    let at = Instant::now();
    let mut root = TrieNode::empty();
    #[allow(unused_assignments)]
    let mut root_hash = alloy_primitives::B256::ZERO;
    for i in 0..base_keys {
        root = root.put(&TrieKeySlice::from_key(&key(b"base", i)), &[1u8; 40], &store);
        // Saving does not collapse in-memory children back to hashes, so the
        // build would hit the very quadratic it is here to measure. Reloading
        // from the store every so often keeps it linear.
        if i % 2_000 == 1_999 {
            root.save(&store, true);
            root_hash = root.compute_hash(&store);
            root = TrieNode::from_message(&store.get(root_hash.as_slice()).unwrap(), &store);
        }
    }
    root.save(&store, true);
    root_hash = root.compute_hash(&store);
    drop(root);
    eprintln!("{:.1}s, depth ~{:.0} bits", at.elapsed().as_secs_f64(), (base_keys as f64).log2());

    println!("{:>7}  {:>10}  {:>12}", "puts", "total ms", "us per put");
    // Interleaved: every repetition walks the whole ladder, so drift in the
    // machine does not land on one rung of it.
    let ladder = [50u64, 100, 200, 400, 800, 1600];
    let mut best: Vec<Option<f64>> = vec![None; ladder.len()];
    for _rep in 0..3 {
        for (slot, &n) in ladder.iter().enumerate() {
            let fresh = TrieNode::from_message(&store.get(root_hash.as_slice()).unwrap(), &store);
            let at = Instant::now();
            let mut t = fresh;
            for i in 0..n {
                // Existing keys, scattered: an update, never an insertion.
                let idx = i.wrapping_mul(2_654_435_761) % base_keys;
                t = t.put(&TrieKeySlice::from_key(&key(b"base", idx)), &[2u8; 40], &store);
            }
            let us_per_put = at.elapsed().as_secs_f64() * 1e6 / n as f64;
            std::hint::black_box(&t);
            best[slot] = Some(best[slot].map_or(us_per_put, |b: f64| b.min(us_per_put)));
        }
    }
    for (slot, &n) in ladder.iter().enumerate() {
        let us = best[slot].unwrap();
        println!("{n:>7}  {:>10.1}  {us:>12.1}", us * n as f64 / 1000.0);
    }
}
