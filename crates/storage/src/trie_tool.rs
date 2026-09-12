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
use crate::trie_snapshot::SnapshotWriter;
use rustock_trie::{TrieKeySlice, TrieNode, TrieStore};
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
    pub fn label(self) -> &'static str {
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
pub fn classify(key: &[u8]) -> LeafKind {
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

/// What kind of repetition a shared reference represents.
///
/// These mean quite different things. A repeated storage cell is two contracts
/// holding the same slot value; a repeated code reference is the same contract
/// bytecode deployed twice; a repeated branch high in the keyspace would mean
/// two accounts with structurally identical sub-tries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShareKind {
    /// Leaf sharing, by what the leaf holds.
    Leaf(LeafKind),
    /// A branch shared while still inside the account index -- the path is
    /// shorter than one account key, so the subtree spans several accounts.
    BranchAccountRegion,
    /// A branch shared below an account key, i.e. within one account's storage.
    BranchStorageRegion,
}

impl ShareKind {
    pub fn label(self) -> String {
        match self {
            ShareKind::Leaf(k) => format!("leaf: {}", k.label()),
            ShareKind::BranchAccountRegion => "branch: account region".into(),
            ShareKind::BranchStorageRegion => "branch: storage region".into(),
        }
    }
}

/// Bits in an account key for a standard 20-byte address: 0x00 || 10 || 20.
const ACCOUNT_KEY_BITS: usize = 31 * 8;

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
    /// References to a node already visited via another parent. Each one is a
    /// subtree the trie stores once but the tree structure reaches twice.
    pub shared_refs: u64,
    /// Bytes that sharing avoids storing: the subtree size of each repeat
    /// reference.
    pub shared_bytes: u64,
    /// Repeat references broken down by kind: (kind, count, bytes saved).
    pub shares_by_kind: Vec<(ShareKind, u64, u64)>,
    /// Long values (over 32 bytes, stored as their own entry keyed by hash)
    /// referenced by more than one leaf, and the bytes that saves.
    pub shared_values: u64,
    pub shared_value_bytes: u64,
    pub distinct_long_values: u64,
    pub by_kind: Vec<(LeafKind, u64, u64)>, // kind, count, value bytes
    /// Embedded nodes broken down the same way sharing is: (kind, count,
    /// bytes). An embedded node has no entry of its own -- it is serialised
    /// inside its parent -- so this says what the trie chose to inline.
    pub embedded_by_kind: Vec<(ShareKind, u64, u64)>,
    /// Every node carrying a value, by what its key says it holds, whether the
    /// node is a leaf or a branch.
    ///
    /// `by_kind` counts leaves, and for accounts that is an undercount: an
    /// account key is a strict prefix of that account's storage and code keys,
    /// so **an account with any storage or code is a branch that carries a
    /// value**, not a terminal node. Counting only leaves silently drops
    /// exactly the accounts that are interesting.
    pub values_by_kind: Vec<(LeafKind, u64, u64)>,
    /// The most-referenced nodes, most first. Each carries a path that reaches
    /// it, so the node can be looked up afterwards.
    pub top_duplicates: Vec<DupNode>,
}

/// A node reached from more than one parent.
#[derive(Debug, Clone)]
pub struct DupNode {
    pub hash: B256,
    /// How many references point at it in total (1 original + its repeats).
    pub refs: u64,
    /// Subtree bytes each extra reference avoids storing.
    pub bytes: u64,
    /// One path that reaches it. A shared node has several by definition; this
    /// is the one seen when the repeat was first noticed, which is enough to
    /// address it.
    pub path: Vec<u8>,
    pub kind: ShareKind,
}

/// Classifies a node for the sharing and embedding breakdowns.
///
/// `path_before` is the path length above the node's own shared path, which is
/// what decides whether a branch sits in the account index or inside one
/// account's storage. `full_path` includes the shared path and is the whole key
/// when the node is terminal.
fn node_kind(node: &TrieNode, path_before: usize, full_path: &[u8]) -> ShareKind {
    if node.is_terminal() {
        ShareKind::Leaf(match pack_bits(full_path) {
            Some(key) => classify(&key),
            None => LeafKind::Unknown,
        })
    } else if path_before < ACCOUNT_KEY_BITS {
        ShareKind::BranchAccountRegion
    } else {
        ShareKind::BranchStorageRegion
    }
}

fn bump_share(v: &mut Vec<(ShareKind, u64, u64)>, kind: ShareKind, bytes: u64) {
    if let Some(e) = v.iter_mut().find(|e| e.0 == kind) {
        e.1 += 1;
        e.2 += bytes;
    } else {
        v.push((kind, 1, bytes));
    }
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

/// The RSKIP107 subtree size: this node's serialised bytes, plus everything
/// below it, plus any value held outside the node.
///
/// `children_size` is summed at insert time, so a subtree shared by several
/// parents is counted in each of them. That makes the root's value the
/// *tree-expanded* total -- the size the trie would occupy if nothing were
/// shared -- available without walking anything.
fn subtree_size(node: &TrieNode, store: &dyn TrieStore) -> u64 {
    let external = if node.has_long_value() { node.value_length() as u64 } else { 0 };
    node.children_size + external + node.message_length(store) as u64
}

/// Walks the trie reachable from `root_hash`, visiting each distinct node once.
///
/// Linear in the number of unique nodes. An earlier version followed every edge
/// so that the non-deduplicated total would be measured rather than derived;
/// that is quadratic on a content-addressed trie, because every shared subtree
/// is re-walked once per reference. On RSK mainnet it made no observable
/// progress in 22 minutes: the set of seen hashes stopped growing while the
/// process kept decompressing nodes it had already visited.
///
/// The expanded total does not need traversal at all -- the root's
/// `children_size` already carries it, for the reason described on
/// [`subtree_size`]. So traversal now stops at any node already seen, and both
/// totals are still exact:
///
/// - deduplicated: summed over distinct nodes as they are visited;
/// - expanded: read from the root.
///
/// `interval_secs` sets how often every counter is printed; 0 disables it.
///
/// Progress is reported against the expanded total, crediting a skipped
/// subtree's whole size from the memoised value rather than descending into it.
pub fn scan(store: &dyn TrieStore, root_hash: B256, interval_secs: u64) -> Result<TrieStats> {
    scan_with_options(store, root_hash, ScanOptions { interval_secs, ..Default::default() })
}

/// Knobs for a scan. Defaults match `scan`.
pub struct ScanOptions<'a> {
    /// Seconds between progress dumps. 0 disables them.
    pub interval_secs: u64,
    /// References buffered before a read pass.
    pub batch_size: usize,
    /// When set, every distinct node is also written here as it is visited, so
    /// one traversal both measures the state and extracts it.
    pub snapshot: Option<&'a mut SnapshotWriter>,
}

impl Default for ScanOptions<'_> {
    fn default() -> Self {
        Self { interval_secs: 0, batch_size: DEFAULT_READ_BATCH, snapshot: None }
    }
}

/// Back-compatible entry point for a scan with an explicit batch size.
pub fn scan_with_batch(
    store: &dyn TrieStore,
    root_hash: B256,
    interval_secs: u64,
    batch_size: usize,
) -> Result<TrieStats> {
    scan_with_options(
        store,
        root_hash,
        ScanOptions { interval_secs, batch_size, snapshot: None },
    )
}

/// How many pending references to accumulate before sorting and reading them.
///
/// Bounds memory: each entry carries a hash plus the path to it, and a storage
/// key path runs to ~600 bits, so entries are on the order of 100 bytes. A
/// strict level-by-level traversal would instead hold an entire level, which in
/// a binary trie can approach half the nodes.
pub const DEFAULT_READ_BATCH: usize = 262_144;

/// Walks the trie reachable from `root_hash`, visiting each distinct node once.
///
/// Linear in the number of unique nodes, and reads in sorted key order.
///
/// # Why reads are batched and sorted
///
/// Following references one at a time is a chain of *dependent* random reads:
/// each address is unknown until the previous read returns, so the device sees
/// one outstanding request and nothing prefetches. Measured on RSK mainnet,
/// that traversal decayed from 2,658 to 763 nodes/s as the working set outgrew
/// the page cache -- see `docs/trie-scan-baseline.md`.
///
/// Instead of descending immediately, discovered references are buffered. When
/// the buffer fills it is sorted by key and read in order, which the storage
/// engine can serve as a forward-moving scan rather than scattered seeks. The
/// set visited is identical; only the order changes.
///
/// This is the bounded-memory form of the frontier-batched mark in
/// `docs/trie-gc-design.md` §10.2. A strict level-by-level frontier would give
/// slightly better locality but has to hold a whole level in memory, and this
/// scan -- unlike a GC mark -- carries the root-to-node path with every pending
/// entry, because leaf classification needs the full key.
///
/// # Totals
///
/// - deduplicated: summed over distinct nodes as they are visited;
/// - expanded: read from the root's `children_size`, which already counts a
///   shared subtree once per reference, so it needs no traversal.
pub fn scan_with_options(
    store: &dyn TrieStore,
    root_hash: B256,
    opts: ScanOptions<'_>,
) -> Result<TrieStats> {
    let ScanOptions { interval_secs, batch_size, mut snapshot } = opts;
    let data = store
        .get(root_hash.as_slice())
        .with_context(|| format!("state root {root_hash:?} not found in the trie store"))?;
    let root = TrieNode::from_message(&data, store);

    let mut st = TrieStats::default();
    st.size_with_dup = subtree_size(&root, store);
    let total_expanded = st.size_with_dup.max(1);

    let mut seen: std::collections::HashMap<B256, u64> = std::collections::HashMap::new();
    let mut seen_values: std::collections::HashSet<B256> = std::collections::HashSet::new();
    let mut processed: u64 = 0;
    let start = Instant::now();
    let mut last_report = Instant::now();

    // Pending references, sorted by key before each read pass. Embedded
    // children are already materialised, so they carry their node and skip the
    // read entirely.
    struct Pending {
        hash: B256,
        node: Option<TrieNode>, // Some => already materialised, no read needed
        /// The node's stored bytes when we hold them. Distinguishes the root --
        /// materialised because we just read it -- from a genuinely embedded
        /// child, which has no entry of its own. Without this the root counts
        /// as embedded and, worse, a snapshot would omit the one node that
        /// makes the rest reachable.
        raw: Option<Vec<u8>>,
        path: Vec<u8>,
        depth: usize,
    }
    let mut pending: Vec<Pending> = vec![Pending {
        hash: root_hash,
        node: Some(root),
        raw: Some(data),
        path: Vec::new(),
        depth: 1,
    }];

    /// A node reached more than once, tracked only from its second reference so
    /// the map holds duplicates rather than every node.
    struct DupEntry {
        repeats: u64,
        bytes: u64,
        path: Vec<u8>,
        kind: ShareKind,
    }
    let mut dups: std::collections::HashMap<B256, DupEntry> = std::collections::HashMap::new();
    let mut next: Vec<Pending> = Vec::new();

    while !pending.is_empty() {
        // Hand the batch to the store as one request so the reads can overlap.
        // Sorting first was tried and lost: at 0.87% live density the keys are
        // too sparse to share blocks, and an iterator seek cannot use the bloom
        // filters that point lookups get. Queue depth is what pays here, not
        // order. Embedded children are already in hand and are not read at all.
        let to_read: Vec<Vec<u8>> = pending
            .iter()
            .filter(|p| p.node.is_none())
            .map(|p| p.hash.as_slice().to_vec())
            .collect();
        let fetched = store.get_many(&to_read);
        let mut fetched_iter = fetched.into_iter();

        for p in pending.drain(..) {
            let was_embedded = p.node.is_some() && p.raw.is_none();
            let (node, raw) = match p.node {
                Some(n) => (n, p.raw),
                None => match fetched_iter.next().flatten() {
                    Some(d) => {
                        let n = TrieNode::from_message(&d, store);
                        (n, Some(d))
                    }
                    None => continue, // dangling reference: counted by absence
                },
            };
            let size = node.message_length(store) as u64;
            let sub = subtree_size(&node, store);

            if let Some(prev) = seen.get(&p.hash) {
                st.shared_refs += 1;
                st.shared_bytes = st.shared_bytes.saturating_add(*prev);
                let mut full = p.path.clone();
                let sp: &TrieKeySlice = &node.shared_path;
                for i in 0..sp.length() {
                    full.push(sp.get(i));
                }
                let kind = node_kind(&node, p.path.len(), &full);
                bump_share(&mut st.shares_by_kind, kind, *prev);
                let entry = dups.entry(p.hash).or_insert_with(|| DupEntry {
                    repeats: 0,
                    bytes: *prev,
                    path: full,
                    kind,
                });
                entry.repeats += 1;
                processed = processed.saturating_add(*prev);
                continue;
            }
            seen.insert(p.hash, sub);

            st.visited += 1;
            st.unique += 1;
            st.size_dedup += size;
            processed = processed.saturating_add(size);
            st.max_depth = st.max_depth.max(p.depth);

            // Copy before descending. Only nodes with bytes of their own are
            // written: an embedded child lives inside its parent's message, so
            // writing it would add a key the source does not have.
            if let (Some(w), Some(bytes)) = (snapshot.as_deref_mut(), raw.as_ref()) {
                w.put_node(p.hash, bytes)?;
            }

            let path_before = p.path.len();
            let mut path = p.path;
            let sp: &TrieKeySlice = &node.shared_path;
            for i in 0..sp.length() {
                path.push(sp.get(i));
            }

            if was_embedded {
                st.embedded += 1;
                let kind = node_kind(&node, path_before, &path);
                bump_share(&mut st.embedded_by_kind, kind, size);
            }

            if node.is_terminal() {
                st.leaves += 1;
                st.leaf_size += size;
                st.depth_sum += p.depth as u64;
                let vlen = node.value_length() as u64;
                st.value_bytes += vlen;
                if node.has_long_value() {
                    st.long_values += 1;
                    processed = processed.saturating_add(vlen);
                    let vh = node.value_hash.unwrap_or_default();
                    if seen_values.insert(vh) {
                        st.distinct_long_values += 1;
                        // Over 32 bytes, so the trie keeps it as its own entry
                        // keyed by the value hash. A snapshot without it would
                        // hold leaves pointing at values that are not there.
                        if let (Some(w), Some(v)) = (snapshot.as_deref_mut(), node.value.as_ref()) {
                            w.put_long_value(vh, v)?;
                        }
                    } else {
                        st.shared_values += 1;
                        st.shared_value_bytes = st.shared_value_bytes.saturating_add(vlen);
                    }
                }
                let kind = match pack_bits(&path) {
                    Some(key) => classify(&key),
                    None => LeafKind::Unknown,
                };
                bump(&mut st.by_kind, kind, vlen);
                bump(&mut st.values_by_kind, kind, vlen);
            } else {
                st.branches += 1;
                st.branch_size += size;
                if node.value.is_some() {
                    st.value_bytes += node.value_length() as u64;
                    // A branch with a value is almost always an account that
                    // owns storage or code: its key is a prefix of theirs.
                    let kind = match pack_bits(&path) {
                        Some(key) => classify(&key),
                        None => LeafKind::Unknown,
                    };
                    bump(&mut st.values_by_kind, kind, node.value_length() as u64);
                    // Rare, but a branch can hold a value, and a long one is
                    // stored separately exactly like a leaf's.
                    if node.has_long_value() {
                        let vh = node.value_hash.unwrap_or_default();
                        if seen_values.insert(vh) {
                            if let (Some(w), Some(v)) =
                                (snapshot.as_deref_mut(), node.value.as_ref())
                            {
                                w.put_long_value(vh, v)?;
                            }
                        }
                    }
                }
                for (bit, child) in [(0u8, &node.left), (1u8, &node.right)] {
                    let mut p2 = path.clone();
                    p2.push(bit);
                    match child {
                        rustock_trie::NodeRef::Node(n) => next.push(Pending {
                            hash: n.compute_hash(store),
                            node: Some((**n).clone()),
                            raw: None, // embedded: no entry of its own
                            path: p2,
                            depth: p.depth + 1,
                        }),
                        rustock_trie::NodeRef::Hash(h) => next.push(Pending {
                            hash: *h,
                            node: None,
                            raw: None,
                            path: p2,
                            depth: p.depth + 1,
                        }),
                        rustock_trie::NodeRef::Empty => {}
                    }
                }
            }

            if interval_secs > 0 && last_report.elapsed().as_secs_f64() >= interval_secs as f64 {
                let secs = start.elapsed().as_secs_f64().max(0.001);
                // Queue depth: `pending` is being drained here and cannot be
                // inspected, so report the buffer being filled for the next pass.
                report_partial(&st, processed, total_expanded, secs, next.len());
                last_report = Instant::now();
            }

        }

        // The whole batch is consumed each pass, so `pending` is empty here and
        // the discovered references become the next pass. Breaking early would
        // desynchronise the fetched values from the entries still queued.
        std::mem::swap(&mut pending, &mut next);

        // Cap how much is read at once: each entry carries its root-to-node
        // path, and a storage key path runs to ~600 bits. Anything above the
        // cap is deferred to a later pass rather than held.
        if pending.len() > batch_size {
            let deferred = pending.split_off(batch_size);
            next = deferred;
        }
    }

    // Rank the duplicates. `refs` counts the original reference as well as the
    // repeats, so a node seen twice reports 2.
    let mut ranked: Vec<DupNode> = dups
        .into_iter()
        .map(|(hash, e)| DupNode {
            hash,
            refs: e.repeats + 1,
            bytes: e.bytes,
            path: e.path,
            kind: e.kind,
        })
        .collect();
    ranked.sort_unstable_by(|a, b| b.refs.cmp(&a.refs).then_with(|| b.bytes.cmp(&a.bytes)));
    ranked.truncate(TOP_DUPLICATES);
    st.top_duplicates = ranked;

    Ok(st)
}

/// How many of the most-referenced nodes to keep and report.
pub const TOP_DUPLICATES: usize = 3;

/// One progress line: how far along, how fast, and the running totals.
///
/// The per-type breakdowns belong in the final report rather than here. They
/// were printed every interval while a full scan took the better part of an
/// hour and finishing was not a given; now that a snapshot re-scans in under
/// two minutes, repeating four tables every thirty seconds is noise that
/// buries the one line saying whether the scan is progressing.
fn report_partial(st: &TrieStats, processed: u64, total: u64, secs: f64, stack_depth: usize) {
    let frac = processed as f64 / total.max(1) as f64;
    let eta = if frac > 0.001 {
        let remaining = secs / frac - secs;
        if remaining >= 3600.0 {
            format!("{}h{}m", remaining as u64 / 3600, (remaining as u64 % 3600) / 60)
        } else {
            format!("{}m{}s", remaining as u64 / 60, remaining as u64 % 60)
        }
    } else {
        "--".into()
    };
    info!(
        target: "rustock::trietool",
        "[{:.2}%] {} nodes ({:.0}/s) | leaves {} branches {} embedded {} | dedup {} of {} | \
         shared: {} refs / {} | long values {} ({} shared) | depth {} | stack {} | ETA {}",
        (frac * 100.0).min(100.0), st.unique, st.unique as f64 / secs,
        st.leaves, st.branches, st.embedded,
        human(st.size_dedup), human(total),
        st.shared_refs, human(st.shared_bytes),
        st.distinct_long_values, st.shared_values,
        st.max_depth, stack_depth, eta
    );
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
    println!("  {:<34}{:>14}", "expanded (shared counted per ref)", human(st.size_with_dup));
    println!("  {:<34}{:>14}", "total, deduplicated", human(st.size_dedup));
    let saved = st.size_with_dup.saturating_sub(st.size_dedup);
    println!("  {:<34}{:>14}  ({:.1}%)", "saved by sharing", human(saved), pct(saved, st.size_with_dup.max(1)));
    println!("  {:<34}{:>14}", "value payload", human(st.value_bytes));
    println!();

    println!("NODES");
    println!("  {:<34}{:>14}", "distinct nodes", st.unique);
    println!("  {:<34}{:>14}", "shared references (not re-walked)", st.shared_refs);
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

    println!("SHARING (repeat references, not re-walked)");
    println!("  {:<34}{:>14}", "repeat references", st.shared_refs);
    println!("  {:<34}{:>14}", "bytes they would have cost", human(st.shared_bytes));
    if !st.shares_by_kind.is_empty() {
        println!("  {:<26}{:>12}{:>9}{:>14}", "  what repeats", "count", "share", "bytes");
        let mut shares = st.shares_by_kind.clone();
        shares.sort_by_key(|k| std::cmp::Reverse(k.1));
        for (kind, count, bytes) in &shares {
            println!("  {:<26}{:>12}{:>8.1}%{:>14}",
                format!("  {}", kind.label()), count,
                pct(*count, st.shared_refs.max(1)), human(*bytes));
        }
    }
    println!();

    println!("LONG VALUES (over 32 bytes, stored by hash)");
    println!("  {:<34}{:>14}", "distinct", st.distinct_long_values);
    println!("  {:<34}{:>14}  ({:.1}%)", "repeat references", st.shared_values,
        pct(st.shared_values, (st.shared_values + st.distinct_long_values).max(1)));
    println!("  {:<34}{:>14}", "bytes saved by sharing", human(st.shared_value_bytes));
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

    println!("VALUE-BEARING NODES BY TYPE (leaves and branches)");
    println!("  {:<20}{:>12}{:>9}{:>14}", "type", "count", "share", "value bytes");
    let total_values: u64 = st.values_by_kind.iter().map(|k| k.1).sum();
    let mut vkinds = st.values_by_kind.clone();
    vkinds.sort_by_key(|k| std::cmp::Reverse(k.1));
    for (kind, count, bytes) in &vkinds {
        println!(
            "  {:<20}{:>12}{:>8.1}%{:>14}",
            kind.label(), count, pct(*count, total_values.max(1)), human(*bytes)
        );
    }
    let leaf_accounts = st.by_kind.iter().find(|k| k.0 == LeafKind::Account).map_or(0, |k| k.1);
    let all_accounts = st.values_by_kind.iter().find(|k| k.0 == LeafKind::Account).map_or(0, |k| k.1);
    println!(
        "  (accounts with storage or code are branches: {} of {} accounts are not leaves)",
        all_accounts.saturating_sub(leaf_accounts), all_accounts
    );
    println!();

    println!("EMBEDDED NODES BY TYPE (serialised inside the parent)");
    println!("  {:<26}{:>12}{:>9}{:>14}", "type", "count", "share", "bytes");
    if st.embedded_by_kind.is_empty() {
        println!("  {:<26}{:>12}", "none", 0);
    } else {
        let mut emb = st.embedded_by_kind.clone();
        emb.sort_by_key(|k| std::cmp::Reverse(k.1));
        for (kind, count, bytes) in &emb {
            println!(
                "  {:<26}{:>12}{:>8.1}%{:>14}",
                kind.label(), count, pct(*count, st.embedded.max(1)), human(*bytes)
            );
        }
    }
    println!("  {:<26}{:>12}{:>8.1}%", "  total embedded", st.embedded,
        pct(st.embedded, st.unique.max(1)));
    println!();

    println!("TOP {} MOST-REFERENCED NODES", TOP_DUPLICATES);
    if st.top_duplicates.is_empty() {
        println!("  none: no node was reached twice");
    } else {
        for (i, d) in st.top_duplicates.iter().enumerate() {
            println!("  #{}  {} references, {} per extra reference, {}",
                i + 1, d.refs, human(d.bytes), d.kind.label());
            println!("      path  {}", crate::trie_inspect::format_path(&d.path));
            println!("      hash  {:?}", d.hash);
        }
    }
    println!();
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustock_trie::{MemoryTrieStore, TrieKeySlice};

    /// Builds a small trie with account-shaped keys, storage cells, and a value
    /// long enough to be stored separately.
    fn build() -> (MemoryTrieStore, B256) {
        let store = MemoryTrieStore::new();
        let mut root = TrieNode::empty();

        // Account keys: 0x00 || 10 bytes || 20 bytes = 31 bytes.
        for i in 0u8..6 {
            let mut key = vec![0x00];
            key.extend_from_slice(&[0x11; 10]);
            key.extend_from_slice(&[i; 20]);
            root = root.put(&TrieKeySlice::from_key(&key), &[i, i, i], &store);
        }
        // Storage cells: longer than an account key plus its marker.
        for i in 0u8..4 {
            let mut key = vec![0x00];
            key.extend_from_slice(&[0x11; 10]);
            key.extend_from_slice(&[0x01; 20]);
            key.push(0x00);
            key.extend_from_slice(&[i; 32]);
            root = root.put(&TrieKeySlice::from_key(&key), &[0xEE; 8], &store);
        }
        // One value over 32 bytes, so it lands in the store under its own hash.
        let mut key = vec![0x00];
        key.extend_from_slice(&[0x11; 10]);
        key.extend_from_slice(&[0x02; 20]);
        key.push(0x80); // code marker
        root = root.put(&TrieKeySlice::from_key(&key), &[0xAB; 200], &store);

        root.save(&store, true);
        let hash = root.compute_hash(&store);
        (store, hash)
    }

    #[test]
    fn scan_counts_a_small_trie() {
        let (store, root) = build();
        let st = scan(&store, root, 0).unwrap();
        assert!(st.unique > 0);
        assert_eq!(st.leaves + st.branches, st.unique);
        assert_eq!(st.long_values, 1, "the 200-byte value is stored separately");

        // Four of the six accounts are leaves. The other two were given storage
        // and code, and an account key is a strict prefix of those, so those
        // accounts are branches carrying a value -- which is exactly why
        // `values_by_kind` exists alongside `by_kind`.
        let leaf_accounts = st.by_kind.iter().find(|k| k.0 == LeafKind::Account);
        assert!(
            leaf_accounts.is_some_and(|a| a.1 == 4),
            "four childless accounts are leaves: {:?}", st.by_kind
        );
        let all_accounts = st.values_by_kind.iter().find(|k| k.0 == LeafKind::Account);
        assert!(
            all_accounts.is_some_and(|a| a.1 == 6),
            "all six accounts carry a value: {:?}", st.values_by_kind
        );
        let cells = st.by_kind.iter().find(|k| k.0 == LeafKind::StorageCell);
        assert!(cells.is_some_and(|c| c.1 >= 4), "four storage cells: {:?}", st.by_kind);
    }

    #[test]
    fn embedded_nodes_are_classified_not_just_counted() {
        let (store, root) = build();
        let st = scan(&store, root, 0).unwrap();
        let total: u64 = st.embedded_by_kind.iter().map(|e| e.1).sum();
        assert_eq!(
            total, st.embedded,
            "every embedded node lands in exactly one bucket: {:?}",
            st.embedded_by_kind
        );
    }

    #[test]
    fn the_root_is_not_counted_as_embedded() {
        // The root is materialised before the loop starts, which once made it
        // look embedded -- and would have left it out of a snapshot, taking the
        // whole trie with it.
        let (store, root) = build();
        let st = scan(&store, root, 0).unwrap();
        assert!(st.embedded < st.unique);
        let root_node = TrieNode::from_message(&store.get(root.as_slice()).unwrap(), &store);
        assert!(!root_node.is_terminal(), "test trie should branch at the root");
    }

    #[test]
    fn snapshot_round_trips_and_reproduces_the_same_statistics() {
        use crate::trie_snapshot::{read_meta, SnapshotWriter};

        let (store, root) = build();
        let before = scan(&store, root, 0).unwrap();

        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("snap");
        let target = target.to_str().unwrap();

        let mut w = SnapshotWriter::create(target, false).unwrap();
        let during = scan_with_options(
            &store,
            root,
            ScanOptions { interval_secs: 0, snapshot: Some(&mut w), ..Default::default() },
        )
        .unwrap();
        let meta = w.finish(root, Some(42), "test").unwrap();

        assert_eq!(meta.root, root);
        assert_eq!(meta.block, Some(42));
        assert_eq!(read_meta(target).unwrap().root, root);
        assert_eq!(meta.long_values, 1);

        // Re-scan the copy. If a single node or long value were missing, the
        // traversal would come up short -- this is the check that the snapshot
        // is complete, not merely non-empty.
        let copy = crate::RocksDbTrieStore::open(std::path::Path::new(target)).unwrap();
        let after = scan(&copy, root, 0).unwrap();

        assert_eq!(after.unique, before.unique, "node count");
        assert_eq!(after.leaves, before.leaves, "leaf count");
        assert_eq!(after.branches, before.branches, "branch count");
        assert_eq!(after.size_dedup, before.size_dedup, "deduplicated bytes");
        assert_eq!(after.value_bytes, before.value_bytes, "value bytes");
        assert_eq!(after.long_values, before.long_values, "long values");
        assert_eq!(after.embedded, during.embedded, "embedded count");

        let mut a = after.by_kind.clone();
        let mut b = before.by_kind.clone();
        a.sort_by_key(|k| format!("{:?}", k.0));
        b.sort_by_key(|k| format!("{:?}", k.0));
        assert_eq!(a, b, "leaves by type");
    }

    #[test]
    fn duplicate_paths_address_the_node_they_name() {
        use crate::trie_inspect::{find_by_path, parse_path, format_path};

        let (store, root) = build();
        let st = scan(&store, root, 0).unwrap();
        for d in &st.top_duplicates {
            let parsed = parse_path(&format_path(&d.path)).unwrap();
            assert_eq!(parsed, d.path, "path notation round trips");
            let found = find_by_path(&store, root, &d.path).unwrap();
            assert!(found.is_some(), "path {} should resolve", format_path(&d.path));
            assert_eq!(found.unwrap().hash, d.hash, "path leads to the named node");
        }
    }
}
