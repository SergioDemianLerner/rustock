//! Offset addressing over the unitrie, for snapshot sync.
//!
//! # Why an offset is a property of the trie, not of a serialization
//!
//! Every non-terminal unitrie node carries `children_size` -- the total
//! serialized size of everything below it -- varint-encoded **into the node's
//! own message** and therefore committed in its hash (RSKIP107). So the size
//! of any subtree is readable from the chain data itself, and two
//! implementations holding the same state root necessarily agree on it.
//!
//! That makes a byte offset into the in-order traversal a well-defined
//! address that any peer can resolve, without the peers having to agree on a
//! serialization format beyond the consensus one. It is the property that
//! makes rskj's offset-based chunk protocol sound, and it is why this keeps
//! that protocol rather than replacing it with key ranges.
//!
//! # Two sizes, and the difference matters
//!
//! * [`total_size`] -- rskj `TrieDTO.getTotalSize()`: the node's own message,
//!   its long value if it has one, and `children_size`. The whole subtree.
//! * [`stream_size`] -- rskj `TrieDTO.getSize()`: the bytes this node
//!   contributes to the traversal *as one entry*, which absorbs any
//!   **embedded** children, because an embedded child is serialized inside
//!   its parent and is never a separate entry.
//!
//! For a node whose children are both separate, `total_size(node) =
//! total_size(left) + stream_size(node) + total_size(right)` -- which is the
//! in-order decomposition the seek below relies on.

use crate::node::{NodeRef, TrieNode};
use crate::store::TrieStore;

/// The whole subtree's serialized size (rskj `getTotalSize`).
pub fn total_size(node: &TrieNode, store: &dyn TrieStore) -> u64 {
    let external = if node.has_long_value() { node.value_length() as u64 } else { 0 };
    node.children_size + external + node.message_length(store) as u64
}

/// The bytes this node occupies as a single traversal entry (rskj `getSize`).
///
/// Embedded children are inside this node's message, so their sizes are part
/// of this entry rather than entries of their own.
pub fn stream_size(node: &TrieNode, store: &dyn TrieStore) -> u64 {
    let external = if node.has_long_value() { node.value_length() as u64 } else { 0 };
    external + node.message_length(store) as u64 + embedded_size(&node.left, store)
        + embedded_size(&node.right, store)
}

fn embedded_size(child: &NodeRef, store: &dyn TrieStore) -> u64 {
    match child {
        NodeRef::Node(n) if n.is_embeddable(store) => total_size(n, store),
        _ => 0,
    }
}

/// A child's subtree size, counting only children that are their own entries.
///
/// An embedded child contributes zero here because its bytes are already
/// counted in its parent's [`stream_size`].
fn branch_size(child: &NodeRef, store: &dyn TrieStore) -> u64 {
    match child {
        NodeRef::Empty => 0,
        NodeRef::Node(n) if n.is_embeddable(store) => {
            let _ = n;
            0
        }
        other => other.resolve(store).map(|n| total_size(&n, store)).unwrap_or(0),
    }
}

/// One step of the descent to an offset, remembered so the traversal can
/// continue upward afterwards.
#[derive(Debug, Clone)]
pub struct Step {
    pub node: TrieNode,
    /// True when the descent went left, so this node and its right subtree
    /// are still ahead in the traversal.
    pub went_left: bool,
}

/// Where an offset lands, and the path taken to get there.
#[derive(Debug, Clone)]
pub struct Seek {
    /// Ancestors of the landing node, root first. Every entry with
    /// `went_left` is still owed to the traversal.
    pub path: Vec<Step>,
    /// The node the offset falls in.
    pub node: TrieNode,
    /// How far into that node's own entry the offset lands. Non-zero means
    /// the offset is **inside** a node rather than on its boundary, and the
    /// node is emitted whole -- see `chunk_from`.
    pub within: u64,
}

/// Find the node containing byte `offset` of the in-order traversal.
///
/// Returns `None` when the offset is past the end of the trie.
///
/// The arithmetic is rskj's `TrieDTOInOrderIterator.findByChildrenSize`: at
/// each node compare the offset against the left branch, then against this
/// node's own entry, then recurse right with the offset rebased.
pub fn seek(root: &TrieNode, offset: u64, store: &dyn TrieStore) -> Option<Seek> {
    if root.is_empty_trie() {
        return None;
    }
    let mut path = Vec::new();
    let mut node = root.clone();
    let mut offset = offset;

    // The trie is at most ~256 levels deep by construction (one level per key
    // bit); the bound is a guard against a malformed store, not a real limit.
    for _ in 0..1024 {
        let left = branch_size(&node.left, store);
        if offset < left {
            // `branch_size` is zero for an embedded child, so reaching here
            // means the left child is a separate entry and resolvable.
            let next = node.left.resolve(store)?;
            path.push(Step { node, went_left: true });
            node = next;
            continue;
        }
        let here = stream_size(&node, store);
        if offset < left + here {
            return Some(Seek { path, node, within: offset - left });
        }
        // Past this node. An embedded right child is *inside* `here`, so if
        // the offset is beyond that, it is beyond this whole subtree -- and
        // descending into the embedded child would invent an entry that the
        // traversal never produces.
        if node.right.is_empty() || is_embedded(&node.right, store) {
            return None;
        }
        let next = node.right.resolve(store)?;
        offset -= left + here;
        path.push(Step { node, went_left: false });
        node = next;
    }
    None
}

/// A node as it appears in a chunk, with the offset it starts at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamNode {
    /// Byte offset of this entry in the whole-trie in-order traversal.
    pub offset: u64,
    /// How much of the offset space this entry occupies.
    ///
    /// **Not the number of bytes on the wire**, and the difference is not an
    /// inconsistency. The offset space is defined by RSKIP107's
    /// `children_size`, which counts an embedded child as a subtree of its
    /// own; the wire encoding writes that child *inside* its parent's
    /// message. So for a node with embedded children this is larger than
    /// `message.len()`, and that is exactly what keeps offsets agreeing with
    /// the `children_size` every peer reads out of the chain data.
    ///
    /// Conflating the two is what makes a chunk loop forever: advancing by
    /// wire bytes never reaches the advertised total.
    pub span: u64,
    /// The node's consensus serialization (RSKIP107).
    pub message: Vec<u8>,
    /// Every long value this entry needs, including those belonging to
    /// **embedded** children.
    ///
    /// A node's message carries a long value's hash and length, never its
    /// bytes, so the bytes travel beside it. The subtlety is embedding: a
    /// long-value leaf is terminal and its message is usually under the
    /// 44-byte embedding limit, so it is frequently written *inside* its
    /// parent and is not a stream entry of its own. Collecting only the
    /// entry node's own value would leave the client holding a value hash it
    /// can never resolve -- state that looks complete and is not.
    pub long_values: Vec<Vec<u8>>,
}

impl StreamNode {
    /// Bytes this entry costs on the wire.
    pub fn wire_len(&self) -> u64 {
        self.message.len() as u64
            + self.long_values.iter().map(|v| v.len() as u64).sum::<u64>()
    }

    /// The offset just past this entry: where the next chunk should start.
    pub fn end(&self) -> u64 {
        self.offset + self.span
    }
}

/// Collect in-order entries starting at `offset`, until adding another would
/// exceed `budget` bytes.
///
/// **A node straddling the start offset is emitted whole.** An offset can
/// land inside a node's entry -- it is a byte offset, not a node index -- and
/// the only sound response is to send the node it lands in. The cost is that
/// consecutive chunks can repeat at most one node at each boundary, which the
/// client deduplicates by offset.
///
/// At least one node is always emitted when the offset is in range, so a
/// budget smaller than a single node still makes progress rather than
/// stalling the transfer.
pub fn chunk_from(
    root: &TrieNode,
    offset: u64,
    budget: u64,
    store: &dyn TrieStore,
) -> Vec<StreamNode> {
    chunk_from_limited(root, offset, budget, usize::MAX, store)
}

/// The run of nodes covering `from..to` in offset space.
///
/// This is the shape a *grid* wants: cell `i` is `chunk_until(i*G, (i+1)*G)`,
/// which depends on nothing but the trie and the two numbers, so two servers
/// asked for the same cell produce the same bytes and the answer can be
/// cached.
///
/// A node straddling either boundary is emitted whole, so it appears in both
/// neighbouring cells. That is the only duplication, it is at most one node
/// per boundary, and it is what makes every node land in some cell: a node
/// beginning at `a` belongs to cell `a / G` whether or not it ends there.
pub fn chunk_until(
    root: &TrieNode,
    from: u64,
    to: u64,
    store: &dyn TrieStore,
) -> Vec<StreamNode> {
    collect(root, from, Stop::Offset(to), usize::MAX, store)
}

/// When to stop emitting.
#[derive(Debug, Clone, Copy)]
enum Stop {
    /// Once the wire bytes emitted would exceed this.
    Budget(u64),
    /// Once the next node would begin at or past this offset.
    Offset(u64),
}

/// As [`chunk_from`], but stopping after `max_nodes` entries.
///
/// The verifier needs this: it replays the traversal over only what a peer
/// sent, and has to stop where the peer's chunk stopped rather than run off
/// the end of the data it was given.
pub fn chunk_from_limited(
    root: &TrieNode,
    offset: u64,
    budget: u64,
    max_nodes: usize,
    store: &dyn TrieStore,
) -> Vec<StreamNode> {
    collect(root, offset, Stop::Budget(budget), max_nodes, store)
}

fn collect(
    root: &TrieNode,
    offset: u64,
    stop: Stop,
    max_nodes: usize,
    store: &dyn TrieStore,
) -> Vec<StreamNode> {
    if root.is_empty_trie() {
        return Vec::new();
    }
    let Some(start) = seek(root, offset, store) else { return Vec::new() };

    let mut out: Vec<StreamNode> = Vec::new();
    let mut used = 0u64;
    // Offset of the entry we are about to emit: the seek landed `within`
    // bytes into it, so it begins that far back.
    let mut at = offset - start.within;

    // The stack holds ancestors still owing their own entry and right
    // subtree, innermost last.
    let mut stack: Vec<TrieNode> = start
        .path
        .iter()
        .filter(|s| s.went_left)
        .map(|s| s.node.clone())
        .collect();

    let mut pending = Some(start.node);
    let mut descend_right: Option<NodeRef> = None;

    loop {
        let node = match pending.take() {
            Some(n) => n,
            None => match descend_right.take() {
                // Walk to the leftmost node of a right subtree, stacking the
                // way down, which is the in-order successor.
                Some(r) => {
                    let Some(mut n) = r.resolve(store) else { continue };
                    while !n.left.is_empty() && !is_embedded(&n.left, store) {
                        let Some(next) = n.left.resolve(store) else { break };
                        stack.push(n);
                        n = next;
                    }
                    n
                }
                None => match stack.pop() {
                    Some(n) => n,
                    None => break,
                },
            },
        };

        let entry = StreamNode {
            offset: at,
            span: stream_size(&node, store),
            message: node.to_message(store),
            long_values: long_values_of(&node, store),
        };
        // Budgeted in wire bytes, because that is what the message costs;
        // advanced in span, because that is what the offset space counts.
        let cost = entry.wire_len();
        let full = match stop {
            Stop::Budget(budget) => used.saturating_add(cost) > budget,
            // The node that straddles the far boundary belongs to this cell
            // too, so the test is on where it *begins*, not where it ends.
            Stop::Offset(to) => at >= to,
        };
        if !out.is_empty() && (full || out.len() >= max_nodes) {
            break;
        }
        at += entry.span;
        used += cost;
        out.push(entry);

        if !node.right.is_empty() && !is_embedded(&node.right, store) {
            descend_right = Some(node.right.clone());
        }
    }

    out
}

/// Every long value carried by this stream entry: the node's own, plus any
/// belonging to children embedded inside its message.
///
/// A node built in memory carries its value inline; one resolved from the
/// store does not -- `from_message` keeps only the hash and length, because
/// `save` wrote the bytes under that hash as a separate record. Either way
/// the bytes have to travel, or the client rebuilds a node whose value it
/// cannot resolve.
fn long_values_of(node: &TrieNode, store: &dyn TrieStore) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    collect_long_values(node, store, &mut out);
    out
}

fn collect_long_values(node: &TrieNode, store: &dyn TrieStore, out: &mut Vec<Vec<u8>>) {
    if node.has_long_value() {
        if let Some(v) = node
            .value
            .clone()
            .or_else(|| node.value_hash.and_then(|h| store.get(h.as_slice())))
        {
            out.push(v);
        }
    }
    for child in [&node.left, &node.right] {
        if let NodeRef::Node(n) = child {
            if n.is_embeddable(store) {
                collect_long_values(n, store, out);
            }
        }
    }
}

fn is_embedded(child: &NodeRef, store: &dyn TrieStore) -> bool {
    matches!(child, NodeRef::Node(n) if n.is_embeddable(store))
}

#[cfg(test)]
mod tests {
    /// A grid covers the trie exactly: every node lands in the cell its start
    /// offset falls in, and nothing is missed.
    #[test]
    fn a_grid_covers_every_node_exactly_once() {
        for (n, g) in [(1usize, 64u64), (40, 128), (300, 500), (300, 5000)] {
            let (root, store) = toy_trie(n);
            let total = total_size(&root, &store);

            // Walk the whole trie once, as the ground truth.
            let mut truth: Vec<(u64, Vec<u8>)> = Vec::new();
            let mut offset = 0;
            while offset < total {
                let chunk = chunk_from(&root, offset, 1 << 30, &store);
                for e in &chunk {
                    truth.push((e.offset, e.message.clone()));
                }
                offset = chunk.last().expect("non-empty").end();
            }

            // Now take the same trie cell by cell.
            let mut seen: std::collections::BTreeMap<u64, Vec<u8>> =
                std::collections::BTreeMap::new();
            let cells = total.div_ceil(g);
            for i in 0..cells {
                for e in chunk_until(&root, i * g, (i + 1) * g, &store) {
                    // A straddling node appears in two cells; it must be the
                    // same node both times.
                    if let Some(prev) = seen.get(&e.offset) {
                        assert_eq!(prev, &e.message, "n={n} g={g}: same offset, different node");
                    }
                    seen.insert(e.offset, e.message);
                }
            }

            assert_eq!(
                seen.len(),
                truth.len(),
                "n={n} g={g}: the grid found {} nodes, the trie has {}",
                seen.len(),
                truth.len()
            );
            for (offset, message) in truth {
                assert_eq!(seen.get(&offset), Some(&message), "n={n} g={g}: node at {offset}");
            }
        }
    }

    /// A cell is a function of the trie and its two bounds and nothing else,
    /// which is what makes it cacheable: ask twice, get the same bytes.
    #[test]
    fn a_cell_is_the_same_however_it_is_asked_for() {
        let (root, store) = toy_trie(300);
        let g = 4096;

        for i in 0..6u64 {
            let once = chunk_until(&root, i * g, (i + 1) * g, &store);
            let twice = chunk_until(&root, i * g, (i + 1) * g, &store);
            assert_eq!(once, twice, "cell {i} is not deterministic");
            assert!(!once.is_empty(), "cell {i} is empty");
        }
    }

    /// Duplication at a boundary is at most the one node that straddles it.
    #[test]
    fn at_most_one_node_is_shared_between_neighbouring_cells() {
        let (root, store) = toy_trie(300);
        let g = 3000;
        let total = total_size(&root, &store);

        for i in 0..(total / g).min(20) {
            let left = chunk_until(&root, i * g, (i + 1) * g, &store);
            let right = chunk_until(&root, (i + 1) * g, (i + 2) * g, &store);
            let shared = left
                .iter()
                .filter(|a| right.iter().any(|b| b.offset == a.offset))
                .count();
            assert!(shared <= 1, "cells {i} and {} share {shared} nodes", i + 1);
        }
    }


    use super::*;
    use crate::{MemoryTrieStore, TrieKeySlice};

    /// A trie with `n` keys, plus some long values so the external-value
    /// accounting is exercised rather than assumed away.
    fn toy_trie(n: usize) -> (TrieNode, MemoryTrieStore) {
        let store = MemoryTrieStore::new();
        let mut root = TrieNode::empty();
        for i in 0..n {
            let key = [(i >> 8) as u8, i as u8, (i * 7) as u8, (i * 13) as u8];
            // Every fifth value is long enough to be stored externally, which
            // changes both `stream_size` and the serialization.
            let value: Vec<u8> = if i % 5 == 0 {
                (0..64u8).map(|b| b.wrapping_add(i as u8)).collect()
            } else {
                vec![i as u8, (i >> 8) as u8]
            };
            root = root.put(&TrieKeySlice::from_key(&key), &value, &store);
        }
        root.save(&store, true);
        (root, store)
    }

    /// Walk the whole trie in order, the simple way, for comparison.
    fn reference_inorder(
        node: &TrieNode,
        store: &dyn TrieStore,
        out: &mut Vec<(u64, Vec<u8>)>,
        at: &mut u64,
    ) {
        if !node.left.is_empty() && !is_embedded(&node.left, store) {
            if let Some(l) = node.left.resolve(store) {
                reference_inorder(&l, store, out, at);
            }
        }
        let msg = node.to_message(store);
        let size = stream_size(node, store);
        out.push((*at, msg));
        *at += size;
        if !node.right.is_empty() && !is_embedded(&node.right, store) {
            if let Some(r) = node.right.resolve(store) {
                reference_inorder(&r, store, out, at);
            }
        }
    }

    /// `total_size` must equal what the traversal actually produces.
    ///
    /// This is the invariant the whole offset scheme rests on: if the size a
    /// node advertises disagreed with the bytes its subtree yields, every
    /// offset past it would address the wrong node.
    #[test]
    fn total_size_equals_the_traversal_length() {
        for n in [1usize, 2, 7, 64, 300] {
            let (root, store) = toy_trie(n);
            let mut seen = Vec::new();
            let mut at = 0u64;
            reference_inorder(&root, &store, &mut seen, &mut at);
            assert_eq!(
                at,
                total_size(&root, &store),
                "n={n}: advertised size disagrees with the traversal"
            );
        }
    }

    /// Seeking to a node's own start offset lands exactly on it.
    #[test]
    fn seeking_a_node_boundary_lands_on_that_node() {
        let (root, store) = toy_trie(120);
        let mut expected = Vec::new();
        let mut at = 0u64;
        reference_inorder(&root, &store, &mut expected, &mut at);

        for (offset, message) in &expected {
            let found = seek(&root, *offset, &store).expect("offset is in range");
            assert_eq!(found.within, 0, "a boundary offset must land at within=0");
            assert_eq!(&found.node.to_message(&store), message, "wrong node at {offset}");
        }
    }

    /// An offset *inside* a node resolves to that node, with `within` saying
    /// how far in. This is the case the protocol has to tolerate, because a
    /// client picking round-numbered offsets will almost always land here.
    #[test]
    fn an_offset_inside_a_node_resolves_to_that_node() {
        let (root, store) = toy_trie(120);
        let mut expected = Vec::new();
        let mut at = 0u64;
        reference_inorder(&root, &store, &mut expected, &mut at);
        let total = at;

        let mut checked_interior = 0;
        for offset in 0..total {
            let found = seek(&root, offset, &store).expect("in range");
            // The entry this offset belongs to is the last one starting at or
            // before it.
            let (start, message) = expected
                .iter()
                .rev()
                .find(|(s, _)| *s <= offset)
                .expect("some entry starts at or before every offset");
            assert_eq!(&found.node.to_message(&store), message, "wrong node at {offset}");
            assert_eq!(found.within, offset - start, "wrong within at {offset}");
            if found.within > 0 {
                checked_interior += 1;
            }
        }
        assert!(checked_interior > 0, "the trie should have multi-byte nodes");
    }

    #[test]
    fn seeking_past_the_end_finds_nothing() {
        let (root, store) = toy_trie(30);
        let total = total_size(&root, &store);
        assert!(seek(&root, total, &store).is_none());
        assert!(seek(&root, total + 1_000, &store).is_none());
    }

    /// **The property the protocol depends on**: chunks requested back to
    /// back cover the trie exactly once, in order, with no gap.
    ///
    /// The client asks for the next chunk at the offset after the last node
    /// it received, so boundary duplication never arises in the normal flow;
    /// this checks the sequence that flow produces.
    #[test]
    fn consecutive_chunks_tile_the_trie() {
        for (n, budget) in [(1usize, 16u64), (7, 64), (64, 128), (300, 100), (300, 4096)] {
            let (root, store) = toy_trie(n);
            let mut expected = Vec::new();
            let mut at = 0u64;
            reference_inorder(&root, &store, &mut expected, &mut at);

            let mut got: Vec<(u64, Vec<u8>)> = Vec::new();
            let mut offset = 0u64;
            let total = total_size(&root, &store);
            while offset < total {
                let chunk = chunk_from(&root, offset, budget, &store);
                assert!(!chunk.is_empty(), "n={n} budget={budget}: no progress at {offset}");
                for entry in &chunk {
                    got.push((entry.offset, entry.message.clone()));
                }
                let last = chunk.last().unwrap();
                offset = last.end();
            }

            assert_eq!(got.len(), expected.len(), "n={n} budget={budget}: node count");
            assert_eq!(got, expected, "n={n} budget={budget}: chunks do not tile the trie");
        }
    }

    /// A chunk asked for at an interior offset repeats the straddled node
    /// rather than losing it -- and the repeat is detectable, because the
    /// entry carries its true start offset.
    #[test]
    fn a_chunk_starting_inside_a_node_emits_that_node_whole() {
        let (root, store) = toy_trie(80);
        let mut expected = Vec::new();
        let mut at = 0u64;
        reference_inorder(&root, &store, &mut expected, &mut at);

        // Find a node that spans more than one byte and start inside it.
        let (start, message) = expected
            .iter()
            .find(|(s, m)| *s > 0 && m.len() > 1)
            .expect("some node is longer than a byte");
        let chunk = chunk_from(&root, start + 1, 4096, &store);

        assert_eq!(&chunk[0].message, message, "the straddled node must be sent whole");
        assert_eq!(chunk[0].offset, *start, "and must report its true start offset");
    }

    /// A budget too small for even one node still yields one, or the transfer
    /// would stall forever asking for the same offset.
    #[test]
    fn a_tiny_budget_still_makes_progress() {
        let (root, store) = toy_trie(50);
        let chunk = chunk_from(&root, 0, 1, &store);
        assert_eq!(chunk.len(), 1, "exactly one node, despite the budget");
        assert!(chunk[0].wire_len() > 1);
    }

    /// **Every long value in the trie is carried, including those inside
    /// embedded children.**
    ///
    /// This is the case that is easy to miss. A long-value leaf is terminal
    /// and its message is usually under the 44-byte embedding limit, so it is
    /// written inside its parent and never appears as a stream entry. Its
    /// parent has no long value of its own, so an implementation that asked
    /// each entry node for *its* value would ship none of them -- and the
    /// client would rebuild a trie full of value hashes it cannot resolve,
    /// which looks complete right up until something reads one.
    ///
    /// The first version of this code did exactly that: the fixture holds six
    /// long values and it carried zero.
    #[test]
    fn every_long_value_is_carried_including_embedded_ones() {
        let (root, store) = toy_trie(30);

        let expected: Vec<Vec<u8>> = (0..30usize)
            .filter(|i| i % 5 == 0)
            .map(|i| (0..64u8).map(|b| b.wrapping_add(i as u8)).collect())
            .collect();
        assert_eq!(expected.len(), 6, "the fixture should hold six long values");

        let all = chunk_from(&root, 0, u64::MAX, &store);
        let carried: Vec<Vec<u8>> = all.iter().flat_map(|e| e.long_values.clone()).collect();

        for value in &expected {
            assert!(
                carried.contains(value),
                "a long value of {} bytes never reached the client",
                value.len()
            );
        }
        assert_eq!(carried.len(), expected.len(), "no value should be sent twice");

        // And the bytes are beside the message, not inside it.
        for entry in all.iter().filter(|e| !e.long_values.is_empty()) {
            for value in &entry.long_values {
                assert!(
                    !entry.message.windows(value.len()).any(|w| w == value.as_slice()),
                    "the value must not be inside the node message"
                );
            }
        }
    }

    /// Chunking must not drop or duplicate a long value at a boundary.
    #[test]
    fn long_values_survive_chunking_at_every_budget() {
        for budget in [1u64, 32, 100, 512, 4096] {
            let (root, store) = toy_trie(60);
            let total = total_size(&root, &store);
            let mut carried: Vec<Vec<u8>> = Vec::new();
            let mut offset = 0u64;
            while offset < total {
                let chunk = chunk_from(&root, offset, budget, &store);
                assert!(!chunk.is_empty(), "budget={budget}: stalled at {offset}");
                for entry in &chunk {
                    carried.extend(entry.long_values.iter().cloned());
                }
                offset = chunk.last().unwrap().end();
            }
            let expected = (0..60usize).filter(|i| i % 5 == 0).count();
            assert_eq!(carried.len(), expected, "budget={budget}: long values lost or repeated");
        }
    }

    #[test]
    fn an_empty_trie_yields_nothing() {
        let store = MemoryTrieStore::new();
        let root = TrieNode::empty();
        assert!(chunk_from(&root, 0, 4096, &store).is_empty());
    }
}
