//! Building a trie from a known set of entries, bottom-up.
//!
//! [`TrieNode::put`] is the wrong tool for loading a large trie, and not by
//! a small margin. It returns a *new* root rather than mutating, which is
//! what makes the trie persistent and is exactly right for execution — but
//! rebuilding the path means cloning the siblings along it, and
//! `NodeRef::Node` owns its subtree, so cloning one deep-copies everything
//! under it. While the tree is in memory that is O(n) per insertion and
//! O(n²) for the load. Measured on this machine: 10,000 accounts in 26
//! seconds, 20,000 in 63, which extrapolates to hours for a few million and
//! keeps getting worse.
//!
//! Given all the entries up front the trie can be built the other way --
//! leaves first, each node written once and never touched again. Nothing is
//! cloned because nothing is modified. The cost is one pass, and the memory
//! is the depth of the recursion rather than the size of the tree.
//!
//! The entries must be **sorted by key**. That is what makes a subtree a
//! contiguous range, which is what lets the build close a subtree and forget
//! it.

use crate::node::{NodeRef, TrieNode};
use crate::path::TrieKeySlice;
use crate::store::TrieStore;
use alloy_primitives::B256;

/// A finished subtree: what its parent needs to know about it.
///
/// The reference goes in the parent's child slot and the size into the
/// parent's `children_size`, which RSKIP-107 makes part of the parent's own
/// message and therefore of its hash. Carrying the size up avoids reading
/// every child back out of the store to measure it.
///
/// The reference is a hash for a node written to the store and the node
/// itself for one small enough to be **embedded** in its parent. That
/// distinction is not an optimisation to take or leave: an embeddable child
/// is written into the parent's message rather than referenced by hash, so
/// handing the parent a hash for it produces a different message and a
/// different root. It cost a wrong root at two entries while five hundred
/// still matched, because at five hundred almost nothing is small enough to
/// embed.
#[derive(Debug)]
struct Written {
    reference: NodeRef,
    /// RSKIP-107 subtree size: the node's own message, its `children_size`,
    /// and any value held outside the node.
    size: u64,
}

/// Build a trie from entries sorted by key, returning the root hash.
///
/// `keys` must be sorted and free of duplicates; every key must be the same
/// length, which is what a trie of one kind of cell (all accounts, say)
/// looks like. `value_of` is called once per key, at the leaf.
///
/// An empty `keys` gives the empty trie's root.
///
/// # Panics
///
/// In debug builds, if the keys are not sorted. A build from unsorted keys
/// produces a structurally valid trie that holds the wrong things, which is
/// a worse outcome than a panic.
pub fn build_sorted(
    keys: &[&[u8]],
    value_of: &dyn Fn(&[u8]) -> Vec<u8>,
    store: &dyn TrieStore,
) -> B256 {
    if keys.is_empty() {
        return crate::node::empty_trie_hash();
    }
    debug_assert!(
        keys.windows(2).all(|w| w[0] < w[1]),
        "build_sorted needs keys in ascending order and without duplicates"
    );

    let total_bits = keys[0].len() * 8;
    debug_assert!(
        keys.iter().all(|k| k.len() * 8 == total_bits),
        "every key must be the same length"
    );

    let written = build(keys, 0, total_bits, value_of, store);
    // The root is written whatever its size: there is no parent to embed it
    // in, and a caller with only a hash has to be able to read it back.
    let hash = match written.reference {
        NodeRef::Node(node) => {
            let message = node.to_message(store);
            let hash = node.compute_hash(store);
            store.put(hash.as_slice(), &message);
            hash
        }
        NodeRef::Hash(hash) => hash,
        NodeRef::Empty => crate::node::empty_trie_hash(),
    };
    store.flush();
    hash
}

/// Build the subtree holding `keys`, whose bits below `depth` are already
/// accounted for by ancestors.
fn build(
    keys: &[&[u8]],
    depth: usize,
    total_bits: usize,
    value_of: &dyn Fn(&[u8]) -> Vec<u8>,
    store: &dyn TrieStore,
) -> Written {
    debug_assert!(!keys.is_empty());

    if keys.len() == 1 {
        let key = keys[0];
        let path = TrieKeySlice::from_key(key).slice(depth, total_bits);
        let leaf = TrieNode::new_leaf(path, value_of(key));
        return finish(leaf, store);
    }

    // Sorted, so the whole range's common prefix is the first and last
    // key's: anything between them agrees wherever those two do.
    let branch_at = depth + common_prefix_len(keys[0], keys[keys.len() - 1], depth, total_bits);
    debug_assert!(branch_at < total_bits, "distinct keys of equal length must branch");

    // The first key whose branching bit is 1. Sorted order makes the zeros
    // a prefix of the range, so this is a partition point.
    let split = keys.partition_point(|k| bit_at(k, branch_at) == 0);
    debug_assert!(split > 0 && split < keys.len(), "both sides are non-empty at a branch");

    // The branching bit itself is consumed by the child's position, so the
    // children start one bit further down.
    let left = build(&keys[..split], branch_at + 1, total_bits, value_of, store);
    let right = build(&keys[split..], branch_at + 1, total_bits, value_of, store);

    let mut node = TrieNode::from_parts(
        TrieKeySlice::from_key(keys[0]).slice(depth, branch_at),
        None,
        None,
        left.reference,
        right.reference,
    );
    node.children_size = left.size + right.size;
    finish(node, store)
}

/// Close a finished node: write it unless its parent will embed it, and
/// measure it either way.
fn finish(node: TrieNode, store: &dyn TrieStore) -> Written {
    let message = node.to_message(store);
    let external = if node.has_long_value() { node.value_length() as u64 } else { 0 };
    let size = node.children_size + external + message.len() as u64;

    // A long value lives outside its node whether or not the node itself is
    // embedded, so it is written here regardless.
    if node.has_long_value() {
        if let Some(value) = &node.value {
            store.put(node.get_value_hash().as_slice(), value);
        }
    }

    if node.is_embeddable(store) {
        return Written { reference: NodeRef::Node(std::sync::Arc::new(node)), size };
    }

    let hash = node.compute_hash(store);
    store.put(hash.as_slice(), &message);
    Written { reference: NodeRef::Hash(hash), size }
}

/// How many bits `a` and `b` share from `depth` onwards.
fn common_prefix_len(a: &[u8], b: &[u8], depth: usize, total_bits: usize) -> usize {
    let mut n = 0;
    while depth + n < total_bits && bit_at(a, depth + n) == bit_at(b, depth + n) {
        n += 1;
    }
    n
}

/// The `i`th bit of `key`, most significant first -- the order the trie
/// descends in.
fn bit_at(key: &[u8], i: usize) -> u8 {
    (key[i / 8] >> (7 - (i % 8))) & 1
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::MemoryTrieStore;

    /// The whole point: a trie built bottom-up is the *same trie* as one
    /// built by repeated insertion. Same root hash, so same everything.
    #[test]
    fn a_bulk_build_matches_insertion_one_at_a_time() {
        for count in [1usize, 2, 3, 17, 256, 1000] {
            let keys: Vec<Vec<u8>> = (0..count).map(|i| test_key(i as u64)).collect();
            let mut sorted: Vec<&[u8]> = keys.iter().map(|k| k.as_slice()).collect();
            sorted.sort_unstable();

            let bulk_store = MemoryTrieStore::new();
            let bulk = build_sorted(&sorted, &test_value, &bulk_store);

            let one_by_one_store = MemoryTrieStore::new();
            let mut root = TrieNode::empty();
            for key in &keys {
                root = root.put(
                    &TrieKeySlice::from_key(key),
                    &test_value(key),
                    &one_by_one_store,
                );
            }
            let expected = root.compute_hash(&one_by_one_store);

            assert_eq!(bulk, expected, "{count} entries");
        }
    }

    /// And the nodes are actually there: a trie whose root hash is right but
    /// whose children were never written reads back as a hole.
    #[test]
    fn every_node_of_a_bulk_build_is_in_the_store() {
        let keys: Vec<Vec<u8>> = (0..500u64).map(test_key).collect();
        let mut sorted: Vec<&[u8]> = keys.iter().map(|k| k.as_slice()).collect();
        sorted.sort_unstable();

        let store = MemoryTrieStore::new();
        let root_hash = build_sorted(&sorted, &test_value, &store);
        let root = TrieNode::from_message(
            &store.get(root_hash.as_slice()).expect("the root is in the store"),
            &store,
        );

        for key in &keys {
            let found = root.get(&TrieKeySlice::from_key(key), &store);
            assert_eq!(found.as_deref(), Some(test_value(key).as_slice()), "key {key:?}");
        }
    }

    #[test]
    fn an_empty_build_is_the_empty_trie() {
        let store = MemoryTrieStore::new();
        assert_eq!(build_sorted(&[], &test_value, &store), crate::node::empty_trie_hash());
    }

    /// Values long enough to live outside the node are written too, and
    /// counted in the parent's subtree size.
    #[test]
    fn long_values_are_stored_and_measured() {
        let keys: Vec<Vec<u8>> = (0..50u64).map(test_key).collect();
        let mut sorted: Vec<&[u8]> = keys.iter().map(|k| k.as_slice()).collect();
        sorted.sort_unstable();
        let long = |key: &[u8]| -> Vec<u8> { vec![key[0]; 128] };

        let bulk_store = MemoryTrieStore::new();
        let bulk = build_sorted(&sorted, &long, &bulk_store);

        let store = MemoryTrieStore::new();
        let mut root = TrieNode::empty();
        for key in &keys {
            root = root.put(&TrieKeySlice::from_key(key), &long(key), &store);
        }
        assert_eq!(bulk, root.compute_hash(&store));
    }

    fn test_key(i: u64) -> Vec<u8> {
        use sha3::{Digest, Keccak256};
        let mut hasher = Keccak256::new();
        hasher.update(b"bulk test");
        hasher.update(i.to_be_bytes());
        hasher.finalize()[..31].to_vec()
    }

    fn test_value(key: &[u8]) -> Vec<u8> {
        vec![key[0], key[1], key[2]]
    }
}
