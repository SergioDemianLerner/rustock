//! Inserting a batch of entries into an existing trie in one descent.
//!
//! [`TrieNode::put`] rebuilds the path from the root for every key. A
//! block's commit does that once per account it touched -- about 2,400
//! accounts over roughly 23 levels, some 55,000 node constructions, where
//! every shared ancestor is rebuilt once for each descendant that passes
//! through it.
//!
//! Given the whole batch at once, the trie can be rebuilt by *union of
//! paths* instead of *sum of paths*: descend once, split the sorted keys
//! between the two sides at each branch, and build each node on the way
//! back up. A node on the way down is constructed once however many keys
//! pass through it.
//!
//! The saving is the gap between those two counts, and it is worth being
//! honest about its size: for keys scattered across a nine-million-entry
//! trie most of a path is *not* shared, because below about the twelfth
//! level the keys have all separated. Counting it out for 2,381 keys gives
//! roughly 55,000 constructions against 32,700, so about 1.7x -- rising
//! with the size of the batch, since the union grows sublinearly while the
//! sum does not.
//!
//! This differs from [`crate::bulk`], which builds a trie from nothing
//! with every key the same length and writes as it goes. Here there is an
//! existing trie whose children may be on disk, keys of mixed length, and
//! nothing is written: a new root comes back, as `put` does.
//!
//! # What it does not do
//!
//! **Deletions.** An empty value removes a key, and removal coalesces a
//! node left with a single child into that child -- a second shape of
//! rewrite, on a path a block takes rarely. Those stay on `put`, one at a
//! time, and the caller splits its batch.
//!
//! # The case that makes this harder than it looks
//!
//! A key may be a strict prefix of another, and in the unitrie it always
//! is: an account's key is a prefix of every one of its storage keys. So
//! a key in the batch can terminate *at a node* rather than below one, and
//! becomes that node's value. `crate::bulk` never meets this because it
//! requires every key to be the same length.

use crate::node::{NodeRef, TrieNode};
use crate::path::TrieKeySlice;
use crate::store::TrieStore;

/// One entry to insert: a key and the value it takes.
pub type Entry<'a> = (&'a TrieKeySlice, &'a [u8]);

/// Insert `entries` into the trie rooted here, returning the new root.
///
/// `entries` must be **sorted by key bits** and free of duplicate keys,
/// and no value may be empty -- an empty value means deletion, which this
/// does not do. Debug builds assert all three.
///
/// Equivalent to calling [`TrieNode::put`] once per entry in any order,
/// and that equivalence is what the tests in this module check.
pub fn put_bulk(root: &TrieNode, entries: &[Entry<'_>], store: &dyn TrieStore) -> TrieNode {
    debug_assert!(
        entries.windows(2).all(|w| cmp_keys(w[0].0, w[1].0).is_lt()),
        "entries must be sorted by key and free of duplicates"
    );
    debug_assert!(entries.iter().all(|(_, v)| !v.is_empty()), "bulk put does not delete");

    if entries.is_empty() {
        return root.clone();
    }

    // The empty trie is not a node with an empty path to hang children
    // under -- `put` *replaces* it, so a single key becomes a leaf
    // carrying the whole key rather than a root with one child below it.
    // Two different tries, two different roots. Found by the test for a
    // single entry into an empty trie.
    if root.is_empty_trie() {
        return match build(entries, 0, store) {
            NodeRef::Node(n) => (*n).clone(),
            _ => TrieNode::empty(),
        };
    }
    merge_node(root, entries, 0, store)
}

/// Order two keys the way the trie does: by their bits, and a prefix
/// before what extends it.
fn cmp_keys(a: &TrieKeySlice, b: &TrieKeySlice) -> std::cmp::Ordering {
    let n = a.length().min(b.length());
    for i in 0..n {
        match a.get(i).cmp(&b.get(i)) {
            std::cmp::Ordering::Equal => {}
            other => return other,
        }
    }
    a.length().cmp(&b.length())
}

/// Bits of `key` from `depth` that match `path`, stopping at either end.
fn match_len(key: &TrieKeySlice, depth: usize, path: &TrieKeySlice) -> usize {
    let available = key.length().saturating_sub(depth).min(path.length());
    let mut n = 0;
    while n < available && key.get(depth + n) == path.get(n) {
        n += 1;
    }
    n
}

/// Merge `entries` into whatever `child` refers to.
///
/// **The empty case is the whole point of this module**: a subtree no key
/// in the batch reaches is handed back untouched, which with an `Arc` is a
/// refcount bump rather than any work at all.
fn merge(child: &NodeRef, entries: &[Entry<'_>], depth: usize, store: &dyn TrieStore) -> NodeRef {
    if entries.is_empty() {
        return child.clone();
    }
    match child.resolve(store) {
        Some(node) => NodeRef::Node(std::sync::Arc::new(merge_node(&node, entries, depth, store))),
        None => build(entries, depth, store),
    }
}

/// Merge `entries` into `node`, whose `shared_path` starts at `depth`.
fn merge_node(
    node: &TrieNode,
    entries: &[Entry<'_>],
    depth: usize,
    store: &dyn TrieStore,
) -> TrieNode {
    let path = &node.shared_path;
    let p = path.length();

    // How much of the shared path *every* entry agrees with. The minimum
    // over the two ends is the answer for the whole range: the keys
    // agreeing with `path[..k]` all share that prefix, so they form a
    // contiguous run, so if both ends reach k everything between does.
    let agree = match_len(entries[0].0, depth, path)
        .min(match_len(entries[entries.len() - 1].0, depth, path));

    if agree == p {
        // Every entry passes through the whole shared path. One of them
        // may end exactly here, in which case it is this node's value.
        let (value, rest) = take_terminating(entries, depth + p);
        let at = depth + p;
        let split = rest.partition_point(|(k, _)| k.get(at) == 0);
        let (left, right) = rest.split_at(split);

        return rebuild(
            node,
            path.clone(),
            value.or_else(|| node.value.clone()),
            merge(&node.left, left, at + 1, store),
            merge(&node.right, right, at + 1, store),
            left.is_empty(),
            right.is_empty(),
            store,
        );
    }

    // The path diverges at `agree`, so this node is pushed down and a new
    // parent takes its place. The pushed-down node keeps everything the
    // old one had below the branching bit.
    let pushed = TrieNode::from_parts(
        path.slice(agree + 1, p),
        node.value.clone(),
        node.value_hash,
        node.left.clone(),
        node.right.clone(),
    );
    let pushed = with_children_size(pushed, node.children_size);
    let pushed_bit = path.get(agree);

    let (value, rest) = take_terminating(entries, depth + agree);
    let at = depth + agree;
    let split = rest.partition_point(|(k, _)| k.get(at) == 0);
    let (left, right) = rest.split_at(split);

    // The side the old node fell on gets merged into; the other is new.
    let old_ref = NodeRef::Node(std::sync::Arc::new(pushed));
    let (new_left, new_right) = if pushed_bit == 0 {
        (merge(&old_ref, left, at + 1, store), build(right, at + 1, store))
    } else {
        (build(left, at + 1, store), merge(&old_ref, right, at + 1, store))
    };

    let mut parent = TrieNode::from_parts(
        path.slice(0, agree),
        value.clone(),
        value.as_deref().map(crate::node::keccak),
        new_left,
        new_right,
    );
    parent.children_size = parent.left.reference_size(store) + parent.right.reference_size(store);
    parent
}

/// Split off the entry whose key ends exactly at `at`, if there is one.
///
/// Sorted order puts a prefix before everything it extends, so it can only
/// be the first.
fn take_terminating<'e, 'a>(
    entries: &'e [Entry<'a>],
    at: usize,
) -> (Option<Vec<u8>>, &'e [Entry<'a>]) {
    match entries.first() {
        Some((key, value)) if key.length() == at => (Some(value.to_vec()), &entries[1..]),
        _ => (None, entries),
    }
}

/// Build a fresh subtree from `entries`, whose bits below `depth` are
/// already accounted for by ancestors.
fn build(entries: &[Entry<'_>], depth: usize, store: &dyn TrieStore) -> NodeRef {
    if entries.is_empty() {
        return NodeRef::Empty;
    }
    if entries.len() == 1 {
        let (key, value) = entries[0];
        let leaf = TrieNode::new_leaf(key.slice(depth, key.length()), value.to_vec());
        return NodeRef::Node(std::sync::Arc::new(leaf));
    }

    // Sorted, so the range's common prefix is the first and last key's.
    let first = entries[0].0;
    let last = entries[entries.len() - 1].0;
    let limit = first.length().min(last.length());
    let mut n = depth;
    while n < limit && first.get(n) == last.get(n) {
        n += 1;
    }

    let (value, rest) = take_terminating(entries, n);
    debug_assert!(!rest.is_empty(), "a branch needs something below it");
    let split = rest.partition_point(|(k, _)| k.get(n) == 0);
    let (left, right) = rest.split_at(split);

    let mut node = TrieNode::from_parts(
        first.slice(depth, n),
        value.clone(),
        value.as_deref().map(crate::node::keccak),
        build(left, n + 1, store),
        build(right, n + 1, store),
    );
    node.children_size = node.left.reference_size(store) + node.right.reference_size(store);
    NodeRef::Node(std::sync::Arc::new(node))
}

fn with_children_size(mut node: TrieNode, size: u64) -> TrieNode {
    node.children_size = size;
    node
}

/// Assemble the replacement for `node`, keeping `children_size` in the
/// subtract-then-add form the per-key path uses.
///
/// Recomputing both sides from scratch would be simpler and would read
/// every untouched child back out of the store to measure it, which is
/// exactly the work this module exists to avoid.
#[allow(clippy::too_many_arguments)]
fn rebuild(
    node: &TrieNode,
    path: TrieKeySlice,
    value: Option<Vec<u8>>,
    new_left: NodeRef,
    new_right: NodeRef,
    left_untouched: bool,
    right_untouched: bool,
    store: &dyn TrieStore,
) -> TrieNode {
    let mut children_size = node.children_size;
    if !left_untouched {
        children_size = children_size.saturating_sub(node.left.reference_size(store))
            + new_left.reference_size(store);
    }
    if !right_untouched {
        children_size = children_size.saturating_sub(node.right.reference_size(store))
            + new_right.reference_size(store);
    }

    let mut out = TrieNode::from_parts(
        path,
        value.clone(),
        value.as_deref().map(crate::node::keccak),
        new_left,
        new_right,
    );
    out.children_size = children_size;
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::MemoryTrieStore;

    /// The whole contract, in one function: a bulk insert must produce the
    /// trie that the same entries inserted one at a time produce.
    ///
    /// Compared by root hash, which covers structure, values, every
    /// `children_size` and every embedding decision at once -- RSKIP-107
    /// folds the subtree size into the node's own message, so a node that
    /// got its size wrong hashes differently even with the right children.
    fn assert_same(base: &[(Vec<u8>, Vec<u8>)], batch: &[(Vec<u8>, Vec<u8>)], what: &str) {
        let store = MemoryTrieStore::new();

        let mut start = TrieNode::empty();
        for (k, v) in base {
            start = start.put(&TrieKeySlice::from_key(k), v, &store);
        }

        let mut one_at_a_time = start.clone();
        for (k, v) in batch {
            one_at_a_time = one_at_a_time.put(&TrieKeySlice::from_key(k), v, &store);
        }

        let mut sorted: Vec<(TrieKeySlice, Vec<u8>)> =
            batch.iter().map(|(k, v)| (TrieKeySlice::from_key(k), v.clone())).collect();
        sorted.sort_by(|a, b| cmp_keys(&a.0, &b.0));
        sorted.dedup_by(|a, b| cmp_keys(&a.0, &b.0).is_eq());
        let entries: Vec<Entry> = sorted.iter().map(|(k, v)| (k, v.as_slice())).collect();
        let bulk = put_bulk(&start, &entries, &store);

        assert_eq!(
            bulk.compute_hash(&store),
            one_at_a_time.compute_hash(&store),
            "{what}: bulk and sequential disagree"
        );

        // And the values must actually be readable back, which the root
        // hash alone would not catch if both paths were wrong the same way.
        for (k, v) in batch {
            assert_eq!(
                bulk.get(&TrieKeySlice::from_key(k), &store).as_deref(),
                Some(v.as_slice()),
                "{what}: key {k:?} does not read back"
            );
        }
    }

    /// A key of any length, since a storage key is longer than the 32
    /// bytes one keccak gives.
    fn key(tag: u8, i: u64, len: usize) -> Vec<u8> {
        use sha3::{Digest, Keccak256};
        let mut out = Vec::with_capacity(len);
        let mut round = 0u8;
        while out.len() < len {
            let mut h = Keccak256::new();
            h.update([tag, round]);
            h.update(i.to_be_bytes());
            out.extend_from_slice(&h.finalize());
            round += 1;
        }
        out.truncate(len);
        out
    }

    fn pairs(tag: u8, range: std::ops::Range<u64>, len: usize) -> Vec<(Vec<u8>, Vec<u8>)> {
        range.map(|i| (key(tag, i, len), vec![tag, (i % 251) as u8])).collect()
    }

    #[test]
    fn into_an_empty_trie() {
        assert_same(&[], &pairs(1, 0..64, 31), "empty base");
    }

    #[test]
    fn one_entry_into_an_empty_trie() {
        assert_same(&[], &pairs(1, 0..1, 31), "single into empty");
    }

    #[test]
    fn an_empty_batch_changes_nothing() {
        assert_same(&pairs(1, 0..32, 31), &[], "empty batch");
    }

    #[test]
    fn all_new_keys() {
        assert_same(&pairs(1, 0..200, 31), &pairs(2, 0..50, 31), "disjoint");
    }

    #[test]
    fn all_existing_keys_get_new_values() {
        let base = pairs(1, 0..200, 31);
        let batch: Vec<_> =
            base.iter().take(50).map(|(k, _)| (k.clone(), vec![0xAA, 0xBB])).collect();
        assert_same(&base, &batch, "pure updates");
    }

    #[test]
    fn a_mix_of_new_and_existing() {
        let base = pairs(1, 0..200, 31);
        let mut batch: Vec<_> =
            base.iter().step_by(3).map(|(k, _)| (k.clone(), vec![0xCC])).collect();
        batch.extend(pairs(2, 0..40, 31));
        assert_same(&base, &batch, "mixed");
    }

    /// The case `crate::bulk` cannot meet: a key that is a strict prefix
    /// of another, which is what an account key is to its storage keys.
    #[test]
    fn a_key_that_is_a_prefix_of_another() {
        let account = key(1, 7, 20);
        let mut storage = account.clone();
        storage.extend_from_slice(&[0xFF; 11]);
        let mut storage2 = account.clone();
        storage2.extend_from_slice(&[0x00; 11]);
        assert_same(
            &[],
            &[
                (account.clone(), vec![1]),
                (storage, vec![2]),
                (storage2, vec![3]),
            ],
            "prefix key in the batch",
        );
    }

    /// The prefix already in the trie, extended by the batch.
    #[test]
    fn a_batch_that_extends_an_existing_prefix() {
        let account = key(1, 9, 20);
        let mut storage = account.clone();
        storage.extend_from_slice(&[0x5A; 11]);
        assert_same(&[(account.clone(), vec![1])], &[(storage, vec![2])], "extend a prefix");
    }

    /// The prefix supplied by the batch, under keys already in the trie.
    #[test]
    fn a_batch_that_inserts_above_existing_keys() {
        let account = key(1, 11, 20);
        let mut a = account.clone();
        a.extend_from_slice(&[0x01; 11]);
        let mut b = account.clone();
        b.extend_from_slice(&[0x02; 11]);
        assert_same(
            &[(a, vec![1]), (b, vec![2])],
            &[(account.clone(), vec![3])],
            "insert a prefix above",
        );
    }

    /// Splitting a compressed path, which is the branch `merge_node`
    /// reaches only when the batch diverges inside a node's `shared_path`.
    #[test]
    fn keys_that_split_a_compressed_path() {
        // Two keys sharing a long prefix make a deep compressed path; a
        // third diverging inside it forces the split.
        let a = vec![0x00; 31];
        let mut b = vec![0x00; 31];
        b[30] = 0x01;
        let mut c = vec![0x00; 31];
        c[15] = 0x80;
        assert_same(&[(a, vec![1]), (b, vec![2])], &[(c, vec![3])], "split a path");
    }

    /// Values long enough to live outside their node, which changes both
    /// the node's message and its measured size.
    #[test]
    fn long_values() {
        let base: Vec<_> = (0u64..20).map(|i| (key(1, i, 31), vec![i as u8; 2000])).collect();
        let batch: Vec<_> = (0u64..20).map(|i| (key(2, i, 31), vec![i as u8; 3000])).collect();
        assert_same(&base, &batch, "long values");
    }

    /// Values short enough that their node is embedded in its parent
    /// rather than referenced, which is a different message again.
    #[test]
    fn embeddable_nodes() {
        let base: Vec<_> = (0u64..8).map(|i| (key(1, i, 31), vec![i as u8])).collect();
        let batch: Vec<_> = (0u64..8).map(|i| (key(2, i, 31), vec![i as u8])).collect();
        assert_same(&base, &batch, "embeddable");
    }

    /// Keys of several lengths at once, prefixes among them, against a
    /// base built the same way -- the shape a real commit produces.
    #[test]
    fn mixed_lengths_and_prefixes() {
        let mut base = Vec::new();
        let mut batch = Vec::new();
        for i in 0u64..40 {
            let account = key(1, i, 20);
            base.push((account.clone(), vec![1, i as u8]));
            for j in 0u64..3 {
                let mut slot = account.clone();
                slot.extend_from_slice(&key(2, i * 10 + j, 11));
                if j == 0 { base.push((slot, vec![2, j as u8])) } else { batch.push((slot, vec![3, j as u8])) }
            }
        }
        for i in 40u64..60 {
            let account = key(1, i, 20);
            batch.push((account.clone(), vec![4, i as u8]));
            let mut slot = account.clone();
            slot.extend_from_slice(&key(2, i, 11));
            batch.push((slot, vec![5]));
        }
        assert_same(&base, &batch, "mixed lengths");
    }

    /// The same comparison, but against a base that has been **saved and
    /// read back**, so its children are `NodeRef::Hash` rather than nodes
    /// held in memory.
    ///
    /// This is the shape a block's commit actually sees, and none of the
    /// tests above reach it: they build their base in memory and leave it
    /// there. Three things only happen on this path -- resolving a child
    /// out of the store, measuring a hash reference (which reads the node
    /// back), and a node whose long value was left behind on parse, so it
    /// carries `long_value_len` with `value: None`. That last one is
    /// pushed down by a path split, and whether its length survives the
    /// push decides its hash.
    fn assert_same_reloaded(
        base: &[(Vec<u8>, Vec<u8>)],
        batch: &[(Vec<u8>, Vec<u8>)],
        what: &str,
    ) {
        let store = MemoryTrieStore::new();
        let mut built = TrieNode::empty();
        for (k, v) in base {
            built = built.put(&TrieKeySlice::from_key(k), v, &store);
        }
        built.save(&store, true);
        let base_hash = built.compute_hash(&store);
        drop(built);

        let reload = || match store.get(base_hash.as_slice()) {
            Some(bytes) => TrieNode::from_message(&bytes, &store),
            None => TrieNode::empty(),
        };

        let mut one_at_a_time = reload();
        for (k, v) in batch {
            one_at_a_time = one_at_a_time.put(&TrieKeySlice::from_key(k), v, &store);
        }

        let mut sorted: Vec<(TrieKeySlice, Vec<u8>)> =
            batch.iter().map(|(k, v)| (TrieKeySlice::from_key(k), v.clone())).collect();
        sorted.sort_by(|a, b| cmp_keys(&a.0, &b.0));
        sorted.dedup_by(|a, b| cmp_keys(&a.0, &b.0).is_eq());
        let entries: Vec<Entry> = sorted.iter().map(|(k, v)| (k, v.as_slice())).collect();
        let bulk = put_bulk(&reload(), &entries, &store);

        assert_eq!(
            bulk.compute_hash(&store),
            one_at_a_time.compute_hash(&store),
            "{what}: bulk and sequential disagree on a reloaded trie"
        );
        for (k, v) in batch {
            assert_eq!(
                bulk.get(&TrieKeySlice::from_key(k), &store).as_deref(),
                Some(v.as_slice()),
                "{what}: key {k:?} does not read back"
            );
        }
    }

    #[test]
    fn against_a_trie_reloaded_from_the_store() {
        assert_same_reloaded(&pairs(1, 0..200, 31), &pairs(2, 0..60, 31), "reloaded base");
    }

    #[test]
    fn long_values_on_a_reloaded_trie() {
        let base: Vec<_> = (0u64..30).map(|i| (key(1, i, 31), vec![i as u8; 2000])).collect();
        let mut batch: Vec<_> = (0u64..15).map(|i| (key(2, i, 31), vec![i as u8; 2500])).collect();
        // Rewrite some of the long-valued keys too, so a node carrying a
        // long value is on the descent rather than only beside it.
        batch.extend(base.iter().take(8).map(|(k, _)| (k.clone(), vec![0x7E; 1800])));
        batch.sort();
        batch.dedup_by(|a, b| a.0 == b.0);
        assert_same_reloaded(&base, &batch, "long values, reloaded");
    }

    /// A long-valued node that a path split has to push down, read back
    /// from the store.
    ///
    /// Worth knowing what this does and does not reach. `from_message`
    /// sets **both** `value` and `long_value_len` when the store holds
    /// the value, so the pushed-down node still measures correctly from
    /// `value` even though the push drops the length -- which is why
    /// `TrieNode::split` gets away with dropping it, and why matching
    /// `split` here is the right thing rather than an oversight.
    ///
    /// `value: None` with `long_value_len: Some(n)` happens only on an
    /// *incomplete* store -- a snapshot witness carrying node messages
    /// without their values. Both paths drop the length there, so they
    /// still agree with each other; both would differ from the true
    /// trie. That is a property of `split`, not of this module, and a
    /// commit never runs against such a store.
    #[test]
    fn a_split_that_pushes_down_a_reloaded_long_value() {
        let mut a = vec![0u8; 31];
        a[0] = 0b1010_1010;
        let mut b = a.clone();
        b[30] = 1;
        // Diverges from a and b inside the compressed path they share.
        let mut c = a.clone();
        c[10] = 0xFF;
        assert_same_reloaded(
            &[(a, vec![0xA1; 3000]), (b, vec![0xB2; 3000])],
            &[(c, vec![0xC3; 2800])],
            "split pushing down a long value",
        );
    }

    #[test]
    fn mixed_lengths_and_prefixes_reloaded() {
        let mut base = Vec::new();
        let mut batch = Vec::new();
        for i in 0u64..40 {
            let account = key(1, i, 20);
            base.push((account.clone(), vec![1, i as u8]));
            let mut slot = account.clone();
            slot.extend_from_slice(&key(2, i, 11));
            base.push((slot, vec![2; 1200]));
            let mut slot2 = account.clone();
            slot2.extend_from_slice(&key(3, i, 11));
            batch.push((slot2, vec![3; 900]));
        }
        base.sort();
        base.dedup_by(|a, b| a.0 == b.0);
        batch.sort();
        batch.dedup_by(|a, b| a.0 == b.0);
        assert_same_reloaded(&base, &batch, "mixed lengths, reloaded");
    }

    /// Randomised, which is what actually finds the case nobody thought
    /// of. Deterministic seeds so a failure can be re-run.
    #[test]
    fn random_batches_against_random_tries() {
        for seed in 0u64..400 {
            let mut state = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            let mut next = move || {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state
            };
            let base_n = next() % 400;
            let batch_n = 1 + next() % 120;

            let mut base = Vec::new();
            for i in 0..base_n {
                let len = [20usize, 31, 31, 42, 52][(next() % 5) as usize];
                base.push((key(1, i ^ seed, len), vec![(next() % 256) as u8; 1 + (next() % 40) as usize]));
            }
            let mut batch = Vec::new();
            for i in 0..batch_n {
                // Half the batch reuses a base key, so updates and
                // insertions are both exercised.
                let k = if !base.is_empty() && next() % 2 == 0 {
                    base[(next() as usize) % base.len()].0.clone()
                } else {
                    let len = [20usize, 31, 42][(next() % 3) as usize];
                    key(2, i ^ seed, len)
                };
                batch.push((k, vec![(next() % 256) as u8; 1 + (next() % 50) as usize]));
            }
            // A sequential replay needs the batch free of duplicates too,
            // or the two paths are not being given the same work.
            batch.sort();
            batch.dedup_by(|a, b| a.0 == b.0);
            assert_same(&base, &batch, &format!("seed {seed}"));
        }
    }

    /// The same, but with keys that deliberately share long prefixes.
    ///
    /// Keys drawn straight from a hash diverge within the first few bits,
    /// so they almost never split a compressed path -- the branch of
    /// `merge_node` that pushes a node down would go untested however
    /// many random seeds were run. These are built as a short random stem
    /// with a long shared tail, which is also the shape the unitrie has.
    #[test]
    fn random_batches_that_share_prefixes() {
        for seed in 0u64..400 {
            let mut state = seed.wrapping_mul(2862933555777941757).wrapping_add(3037000493);
            let mut next = move || {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state
            };
            // A handful of stems, each extended many ways, so most pairs
            // of keys agree for a long way and then diverge.
            let stems: Vec<Vec<u8>> = (0..4).map(|i| key(9, seed ^ i, 8)).collect();
            let make = |n: u64, tag: u8, next: &mut dyn FnMut() -> u64| {
                let mut out = Vec::new();
                for i in 0..n {
                    let mut k = stems[(next() % stems.len() as u64) as usize].clone();
                    let extra = (next() % 24) as usize;
                    k.extend_from_slice(&key(tag, i, extra));
                    out.push((k, vec![(next() % 256) as u8; 1 + (next() % 60) as usize]));
                }
                out
            };
            let mut base = make(next() % 60, 1, &mut next);
            base.sort();
            base.dedup_by(|a, b| a.0 == b.0);
            let mut batch = make(1 + next() % 40, 2, &mut next);
            // Half the time, also rewrite keys the base already holds.
            if !base.is_empty() && seed % 2 == 0 {
                for (i, (k, _)) in base.iter().take(10).enumerate() {
                    batch.push((k.clone(), vec![0xEE; 1 + i]));
                }
            }
            batch.sort();
            batch.dedup_by(|a, b| a.0 == b.0);
            assert_same(&base, &batch, &format!("prefix seed {seed}"));
        }
    }
}
