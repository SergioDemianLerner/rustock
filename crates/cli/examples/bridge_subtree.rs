//! How big is the Bridge's storage subtree, and how much of it is missing from
//! a chunk?
//!
//! Enumerating keys cannot cover a keyspace indexed by Bitcoin block hash, so
//! the question is whether copying the whole subtree into each chunk is
//! affordable. Two answers, cheap first:
//!
//!   1. `children_size` is carried in the node message -- the serialized size of
//!      everything below a node. Descending to the subtree root and reading one
//!      field sizes it without touching the subtree at all.
//!   2. With `WALK=1`, actually enumerate it, reporting progress and rate so a
//!      run too slow to finish still yields an extrapolation rather than
//!      nothing. The first version of this tool had neither the fast path nor
//!      any progress output, so two long runs produced no number at all.
//!
//! Usage: bridge_subtree <block-dir> <chunk-dir> <archive-trie> <block> [<block>...]
//!        WALK=1 to enumerate, WALK_CAP=<n> to stop after n nodes.

use rustock_storage::{BlockStore, RocksDbTrieStore};
use rustock_trie::{account_key, NodeRef, TrieKeySlice, TrieNode, TrieStore};
use alloy_primitives::Address;
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Instant;

const BRIDGE: Address = Address::new([0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,1,0,0,6]);
const STORAGE_PREFIX: u8 = 0x00;

/// Descend to the node covering `key`, which is a *prefix* of real trie keys.
///
/// Mirrors `TrieNode::find`, except that running out of key is success rather
/// than failure: a node whose shared path extends past the prefix is exactly
/// the subtree root being looked for.
fn find_prefix(node: &TrieNode, key: &TrieKeySlice, store: &dyn TrieStore) -> Option<TrieNode> {
    if key.length() == 0 {
        return Some(node.clone());
    }
    if node.shared_path.length() >= key.length() {
        let common = key.common_path(&node.shared_path);
        return if common.length() == key.length() { Some(node.clone()) } else { None };
    }
    let common = key.common_path(&node.shared_path);
    if common.length() < node.shared_path.length() {
        return None;
    }
    let bit = key.get(common.length());
    let child = if bit == 0 { node.left.resolve(store) } else { node.right.resolve(store) }?;
    let rest = key.slice(common.length() + 1, key.length());
    find_prefix(&child, &rest, store)
}

struct Walk<'a> {
    store: &'a dyn TrieStore,
    seen: HashSet<[u8; 32]>,
    cap: usize,
    t0: Instant,
    next_report: usize,
}

impl<'a> Walk<'a> {
    /// Iterative, not recursive: this subtree is deep enough to be worth not
    /// trusting the stack with.
    fn run(&mut self, root: &TrieNode) {
        let mut stack: Vec<TrieNode> = vec![root.clone()];
        while let Some(node) = stack.pop() {
            if self.seen.len() >= self.cap {
                eprintln!("  stopped at the cap of {} nodes", self.cap);
                return;
            }
            for r in [&node.left, &node.right] {
                if let NodeRef::Hash(h) = r {
                    if !self.seen.insert(h.0) {
                        continue;
                    }
                }
                if let Some(c) = r.resolve(self.store) {
                    stack.push(c);
                }
            }
            if self.seen.len() >= self.next_report {
                let secs = self.t0.elapsed().as_secs_f64();
                eprintln!("  {} nodes in {:.0}s ({:.0}/s), frontier {}",
                          self.seen.len(), secs,
                          self.seen.len() as f64 / secs.max(0.001), stack.len());
                self.next_report += 100_000;
            }
        }
    }
}

fn main() -> anyhow::Result<()> {
    let a: Vec<String> = std::env::args().collect();
    let t = Instant::now();
    let blocks = BlockStore::open_read_only(&a[1])?;
    let chunk: Arc<dyn TrieStore> =
        Arc::new(RocksDbTrieStore::open_read_only(&format!("{}/sealed", a[2]))?);
    let archive: Arc<dyn TrieStore> = Arc::new(RocksDbTrieStore::open_read_only(&a[3])?);
    eprintln!("stores open in {:.1}s", t.elapsed().as_secs_f64());

    let acct = account_key(&BRIDGE);
    let mut storage_pre = acct.clone();
    storage_pre.push(STORAGE_PREFIX);

    let walk = std::env::var("WALK").is_ok();
    let cap: usize = std::env::var("WALK_CAP").ok().and_then(|v| v.parse().ok())
        .unwrap_or(usize::MAX);

    for arg in &a[4..] {
        let n: u64 = arg.parse()?;
        let h = blocks.header(blocks.canonical_hash(n)?.expect("hash"))?.expect("hdr");
        let root_bytes = archive.get(h.state_root.as_slice()).expect("root in archive");
        let root = TrieNode::from_message(&root_bytes, archive.as_ref());

        for (label, prefix) in [("bridge account subtree", &acct),
                                ("bridge STORAGE subtree", &storage_pre)] {
            let slice = TrieKeySlice::from_key(prefix);
            match find_prefix(&root, &slice, archive.as_ref()) {
                None => println!("#{n} {label}: not present"),
                Some(sub) => println!("#{n} {label}: children_size = {:.1} MB",
                                      sub.children_size as f64 / 1e6),
            }
        }

        if walk {
            let slice = TrieKeySlice::from_key(&storage_pre);
            if let Some(sub) = find_prefix(&root, &slice, archive.as_ref()) {
                let mut w = Walk { store: archive.as_ref(), seen: HashSet::new(),
                                   cap, t0: Instant::now(), next_report: 100_000 };
                w.run(&sub);
                let total = w.seen.len();
                let (mut missing, mut mbytes) = (0usize, 0usize);
                for hh in &w.seen {
                    if chunk.get(hh).is_none() {
                        missing += 1;
                        mbytes += archive.get(hh).map(|v| v.len()).unwrap_or(0) + 32;
                    }
                }
                println!("#{n} walked {total} nodes; {missing} missing from this chunk ({:.1} MB)",
                         mbytes as f64 / 1e6);
            }
        }
    }
    Ok(())
}
