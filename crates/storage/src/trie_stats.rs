//! Walks the Unitrie reachable from one state root and reports what is in it.
//!
//! This scans a *single* root's tree, not the whole node database. The database
//! holds every historical state version -- 1.2 billion nodes for RSK mainnet --
//! whereas one root reaches only the state as of that block.
//!
//! # How node types are told apart
//!
//! The Unitrie puts accounts, code and storage in one keyspace, distinguished by
//! key shape (see `rustock_trie::key_mapper`):
//!
//! ```text
//! account   0x00 || keccak(addr)[0:10] || addr                       31 bytes
//! code      account_key || 0x80                                      32 bytes
//! storage   account_key || 0x00 || keccak(slot)[0:10] || slot'       43+ bytes
//! storage   account_key || 0x00                                      32 bytes
//!   root marker
//! ```
//!
//! Keys are bit paths, so a leaf's key is recovered by concatenating the shared
//! path of every node from the root plus the branch bit taken at each step.
//! REMASC is the exception worth knowing about: its address is a single byte, so
//! its keys are 19 bytes shorter than everything else.
//!
//! # Embedded nodes
//!
//! A terminal node whose serialised form is at most 44 bytes is stored inside
//! its parent rather than as its own database entry. Those are detected by the
//! parent holding `NodeRef::Node` instead of `NodeRef::Hash` -- the child is
//! already materialised, with no separate lookup.

use alloy_primitives::B256;
use anyhow::{Context, Result};
use rustock_trie::{TrieKeySlice, TrieNode, TrieStore};
use std::collections::HashSet;
use std::time::Instant;
use tracing::info;

/// What a leaf's key says it holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LeafKind {
    Account,
    ContractCode,
    StorageRoot,
    StorageCell,
    /// A key shape none of the above explain -- reported rather than hidden, so
    /// a mapping change shows up instead of being silently miscounted.
    Unknown,
}

impl LeafKind {
    fn label(self) -> &'static str {
        match self {
            LeafKind::Account => "accounts",
            LeafKind::ContractCode => "contract code",
            LeafKind::StorageRoot => "storage roots",
            LeafKind::StorageCell => "storage cells",
            LeafKind::Unknown => "unknown",
        }
    }
}

/// Classifies a leaf from its full key.
///
/// Length alone is enough: only the account-key length varies, and it varies
/// with the address, so the trailing marker byte disambiguates code from the
/// storage-root marker.
fn classify(key: &[u8]) -> LeafKind {
    const ACCOUNT_20: usize = 1 + 10 + 20; // 31
    const ACCOUNT_1: usize = 1 + 10 + 1; //  12, REMASC
    match key.len() {
        ACCOUNT_20 | ACCOUNT_1 => LeafKind::Account,
        n if n == ACCOUNT_20 + 1 || n == ACCOUNT_1 + 1 => match key[n - 1] {
            0x80 => LeafKind::ContractCode,
            0x00 => LeafKind::StorageRoot,
            _ => LeafKind::Unknown,
        },
        n if n > ACCOUNT_20 + 1 => LeafKind::StorageCell,
        _ => LeafKind::Unknown,
    }
}

#[derive(Default)]
pub struct TrieStats {
    pub visited: u64,
    pub unique: u64,
    pub size_with_dup: u64,
    pub size_dedup: u64,
    pub leaves: u64,
    pub branches: u64,
    pub embedded: u64,
    pub branch_size: u64,
    pub leaf_size: u64,
    pub max_depth: usize,
    pub depth_sum: u64,
    pub value_bytes: u64,
    pub long_values: u64,
    pub by_kind: Vec<(LeafKind, u64, u64)>, // kind, count, value bytes
}

fn bump(v: &mut Vec<(LeafKind, u64, u64)>, kind: LeafKind, bytes: u64) {
    if let Some(e) = v.iter_mut().find(|e| e.0 == kind) {
        e.1 += 1;
        e.2 += bytes;
    } else {
        v.push((kind, 1, bytes));
    }
}

/// Packs a bit path (one byte per bit) into bytes. Returns None if the path is
/// not byte-aligned, which a well-formed leaf key always is.
fn pack_bits(bits: &[u8]) -> Option<Vec<u8>> {
    if bits.len() % 8 != 0 {
        return None;
    }
    Some(
        bits.chunks(8)
            .map(|c| c.iter().fold(0u8, |acc, &b| (acc << 1) | (b & 1)))
            .collect(),
    )
}

/// Walks every node reachable from `root_hash`.
///
/// Traversal follows every edge rather than stopping at nodes already seen, so
/// both totals are real: `size_with_dup` counts a shared subtree once per
/// reference, `size_dedup` once overall. Identical subtrees do occur -- the trie
/// is content-addressed, so two accounts with identical storage share nodes.
///
/// Memory is dominated by the set of seen hashes: 32 bytes per unique node plus
/// set overhead, so tens of millions of nodes cost a few GB.
pub fn scan(store: &dyn TrieStore, root_hash: B256, progress_every: u64) -> Result<TrieStats> {
    let data = store
        .get(root_hash.as_slice())
        .with_context(|| format!("state root {root_hash:?} not found in the trie store"))?;
    let root = TrieNode::from_message(&data, store);

    let mut st = TrieStats::default();
    let mut seen: HashSet<B256> = HashSet::new();
    let start = Instant::now();

    // (node, bit path so far, depth, was it embedded in its parent)
    let mut stack: Vec<(TrieNode, Vec<u8>, usize, bool)> = vec![(root, Vec::new(), 1, false)];

    while let Some((node, mut path, depth, was_embedded)) = stack.pop() {
        let size = node.message_length(store) as u64;
        st.visited += 1;
        st.size_with_dup += size;
        st.max_depth = st.max_depth.max(depth);
        if was_embedded {
            st.embedded += 1;
        }

        // Deduplicate by content hash. Embedded nodes have no independent
        // database entry, but they still have a hash and can recur.
        let hash = node.compute_hash(store);
        if seen.insert(hash) {
            st.unique += 1;
            st.size_dedup += size;
        }

        // Extend the path by this node's shared prefix.
        let sp: &TrieKeySlice = &node.shared_path;
        for i in 0..sp.length() {
            path.push(sp.get(i));
        }

        if node.is_terminal() {
            st.leaves += 1;
            st.leaf_size += size;
            st.depth_sum += depth as u64;
            let vlen = node.value_length() as u64;
            st.value_bytes += vlen;
            if node.has_long_value() {
                st.long_values += 1;
            }
            let kind = match pack_bits(&path) {
                Some(key) => classify(&key),
                None => LeafKind::Unknown,
            };
            bump(&mut st.by_kind, kind, vlen);
        } else {
            st.branches += 1;
            st.branch_size += size;
            // A node may carry a value and still have children.
            if node.value.is_some() {
                st.value_bytes += node.value_length() as u64;
            }
            for (bit, child) in [(0u8, &node.left), (1u8, &node.right)] {
                let embedded = matches!(child, rustock_trie::NodeRef::Node(_));
                if let Some(c) = child.resolve(store) {
                    let mut p = path.clone();
                    p.push(bit);
                    stack.push((c, p, depth + 1, embedded));
                }
            }
        }

        if progress_every > 0 && st.visited % progress_every == 0 {
            let secs = start.elapsed().as_secs_f64().max(0.001);
            info!(
                target: "rustock::triestats",
                "scanned {} nodes ({:.0}/s), {} unique, depth {}, stack {}",
                st.visited, st.visited as f64 / secs, st.unique, st.max_depth, stack.len()
            );
        }
    }
    Ok(st)
}

fn pct(part: u64, whole: u64) -> f64 {
    if whole == 0 { 0.0 } else { part as f64 / whole as f64 * 100.0 }
}

fn human(bytes: u64) -> String {
    const U: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = bytes as f64;
    let mut i = 0;
    while v >= 1000.0 && i < U.len() - 1 {
        v /= 1000.0;
        i += 1;
    }
    if i == 0 { format!("{} {}", bytes, U[0]) } else { format!("{v:.2} {}", U[i]) }
}

/// Prints the report.
pub fn report(st: &TrieStats, root: B256, block: Option<u64>, elapsed: f64) {
    let n = st.visited;
    println!();
    println!("Unitrie statistics");
    println!("  state root   {root:?}");
    if let Some(b) = block {
        println!("  block        #{b}");
    }
    println!("  scan time    {elapsed:.1}s  ({:.0} nodes/s)", n as f64 / elapsed.max(0.001));
    println!();

    println!("SIZE");
    println!("  {:<34}{:>14}", "total, counting shared subtrees", human(st.size_with_dup));
    println!("  {:<34}{:>14}", "total, deduplicated", human(st.size_dedup));
    let saved = st.size_with_dup.saturating_sub(st.size_dedup);
    println!("  {:<34}{:>14}  ({:.1}%)", "saved by sharing", human(saved), pct(saved, st.size_with_dup.max(1)));
    println!("  {:<34}{:>14}", "value payload", human(st.value_bytes));
    println!();

    println!("NODES");
    println!("  {:<34}{:>14}", "reachable (edges followed)", n);
    println!("  {:<34}{:>14}  ({:.1}%)", "unique by content hash", st.unique, pct(st.unique, n));
    println!("  {:<34}{:>14}  ({:.1}%)", "leaf (terminal)", st.leaves, pct(st.leaves, n));
    println!("  {:<34}{:>14}  ({:.1}%)", "non-leaf (branch)", st.branches, pct(st.branches, n));
    println!("  {:<34}{:>14}  ({:.1}%)", "embedded in parent", st.embedded, pct(st.embedded, n));
    println!("  {:<34}{:>14}  ({:.1}%)", "values over 32 bytes", st.long_values, pct(st.long_values, st.leaves.max(1)));
    println!();

    println!("AVERAGE SIZE");
    println!("  {:<34}{:>11.1} B", "any node", st.size_with_dup as f64 / n.max(1) as f64);
    println!("  {:<34}{:>11.1} B", "non-leaf node", st.branch_size as f64 / st.branches.max(1) as f64);
    println!("  {:<34}{:>11.1} B", "leaf node", st.leaf_size as f64 / st.leaves.max(1) as f64);
    println!();

    println!("DEPTH (nodes from root)");
    println!("  {:<34}{:>14}", "maximum", st.max_depth);
    println!("  {:<34}{:>11.1}", "average over leaves", st.depth_sum as f64 / st.leaves.max(1) as f64);
    println!();

    println!("LEAVES BY TYPE");
    println!("  {:<20}{:>12}{:>9}{:>14}", "type", "count", "share", "value bytes");
    let mut kinds = st.by_kind.clone();
    kinds.sort_by_key(|k| std::cmp::Reverse(k.1));
    for (kind, count, bytes) in &kinds {
        println!(
            "  {:<20}{:>12}{:>8.1}%{:>14}",
            kind.label(), count, pct(*count, st.leaves.max(1)), human(*bytes)
        );
    }
    println!();
}
