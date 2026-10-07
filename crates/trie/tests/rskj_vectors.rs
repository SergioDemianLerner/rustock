//! Cross-checks against rskj's own code.
//!
//! The vectors in `rskj-vectors/chunks.txt` were produced by running rskj's
//! `TrieDTO` and `TrieDTOInOrderIterator` (see `Chunk.java` beside them), so a
//! disagreement here is a disagreement with the implementation that defines
//! the format, not with a reading of it.

use rustock_trie::snapshot::total_size;
use rustock_trie::{MemoryTrieStore, TrieKeySlice, TrieNode};

/// The same trie the Java generator builds.
fn trie(keys: usize) -> (TrieNode, MemoryTrieStore) {
    let store = MemoryTrieStore::new();
    let mut root = TrieNode::empty();
    for i in 0..keys {
        let key = [(i >> 8) as u8, i as u8, (i * 7) as u8, (i * 13) as u8];
        let value: Vec<u8> = if i % 4 == 0 {
            (0..64u8).map(|j| j.wrapping_add(i as u8)).collect()
        } else {
            vec![i as u8, 2, 3]
        };
        root = root.put(&TrieKeySlice::from_key(&key), &value, &store);
    }
    root.save(&store, true);
    (root, store)
}

fn vectors() -> Vec<(usize, u64, String, u64)> {
    let text = include_str!("rskj-vectors/chunks.txt");
    let mut out = Vec::new();
    #[allow(unused_assignments)] // the initialiser is required; the file supplies the value
    let (mut keys, mut root, mut total) = (0usize, String::new(), 0u64);
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("KEYS ") {
            keys = rest.split_whitespace().next().unwrap().parse().unwrap();
        } else if let Some(rest) = line.strip_prefix("ROOT ") {
            root = rest.trim().to_string();
        } else if let Some(rest) = line.strip_prefix("TOTAL ") {
            total = rest.trim().parse().unwrap();
            out.push((keys, 0, root.clone(), total));
        }
    }
    out
}

/// **The same keys must give the same trie.** If rustock and rskj disagree
/// here, nothing downstream -- offsets, chunks, proofs -- can agree either.
#[test]
fn rustock_builds_the_same_trie_as_rskj() {
    let mut checked = 0;
    for (keys, _, expected_root, expected_total) in vectors() {
        let (root, store) = trie(keys);
        let got = root.compute_hash(&store);
        assert_eq!(
            format!("{:x}", got),
            expected_root,
            "{keys} keys: rustock's root differs from rskj's"
        );
        assert_eq!(
            total_size(&root, &store),
            expected_total,
            "{keys} keys: rustock's trie size differs from rskj's"
        );
        checked += 1;
    }
    assert!(checked >= 4, "the vector file should hold several tries");
}

/// **Byte-for-byte against rskj's own encoder.**
///
/// The stripped format has two traps a careful reading of the spec would not
/// catch: an embedded child's length becomes three bytes where the consensus
/// format uses one, and a long value stops being a 32-byte reference and
/// becomes the value itself. Vectors from rskj decide the matter.
#[test]
fn stripped_nodes_match_rskj_byte_for_byte() {
    use rustock_trie::snapshot::chunk_from;
    use rustock_trie::snapshot_legacy::strip;

    let text = include_str!("rskj-vectors/nodes.txt");
    let mut expected: std::collections::HashMap<usize, Vec<(String, String)>> =
        std::collections::HashMap::new();
    let mut keys = 0usize;

    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("KEYS ") {
            keys = rest.trim().parse().unwrap();
        } else if line.starts_with("NODE ") {
            let field = |name: &str| -> String {
                line.split_whitespace()
                    .find_map(|t| t.strip_prefix(name))
                    .unwrap_or_default()
                    .to_string()
            };
            expected.entry(keys).or_default().push((field("source="), field("encoded=")));
        }
    }
    assert!(expected.len() >= 3, "expected vectors for several tries");

    let mut compared = 0;
    for (keys, nodes) in &expected {
        let (root, store) = trie(*keys);
        let ours = chunk_from(&root, 0, u64::MAX, &store);
        assert_eq!(
            ours.len(),
            nodes.len(),
            "{keys} keys: rustock walks {} nodes, rskj walks {}",
            ours.len(),
            nodes.len()
        );

        for (i, (entry, (src, enc))) in ours.iter().zip(nodes.iter()).enumerate() {
            // The consensus message first: if these differ, nothing else can
            // be compared meaningfully.
            assert_eq!(
                hex(&entry.message),
                *src,
                "{keys} keys, node {i}: consensus message differs from rskj's"
            );

            let stripped = strip(&entry.message, &store)
                .unwrap_or_else(|| panic!("{keys} keys, node {i}: could not strip"));
            assert_eq!(
                hex(&stripped),
                *enc,
                "{keys} keys, node {i}: stripped form differs from rskj's"
            );
            compared += 1;
        }
    }
    assert!(compared >= 50, "only {compared} nodes compared");
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// **The whole chunk, byte for byte against rskj's.**
///
/// The node encoding is only half of it: the walk also decides which
/// ancestors land in which list, what counts as a node's offset, and when the
/// node straddling the far boundary is dropped. None of that is visible until
/// an rskj client rejects a chunk, so it is checked against blobs rskj
/// produced.
#[test]
fn legacy_chunks_match_rskj_byte_for_byte() {
    use rustock_trie::snapshot_legacy::legacy_chunk;

    let text = include_str!("rskj-vectors/chunks.txt");
    let (mut keys, mut from, mut to) = (0usize, 0u64, 0u64);
    let mut checked = 0;

    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("KEYS ") {
            let f: Vec<&str> = rest.split_whitespace().collect();
            keys = f[0].parse().unwrap();
            from = f[2].parse().unwrap();
            to = f[4].parse().unwrap();
        } else if let Some(rest) = line.strip_prefix("BLOB ") {
            let (root, store) = trie(keys);
            let root_hash: [u8; 32] = root.compute_hash(&store).into();

            let chunk = legacy_chunk(&root_hash, from, to, &store)
                .unwrap_or_else(|| panic!("{keys} keys {from}..{to}: could not build"));

            assert_eq!(
                hex(&rustock_trie::snapshot_legacy::encode_blob(&chunk)),
                rest.trim(),
                "{keys} keys, {from}..{to}: blob differs from rskj's"
            );
            checked += 1;
        }
    }
    assert!(checked >= 6, "only {checked} blobs compared");
}


/// **Reading rskj's chunks.** Take the blob rskj produced, rebuild it, and
/// require the nodes to be exactly what our own trie holds.
///
/// This is the direction that needs the child hashes rskj dropped to be
/// recomputed, and the tree shape to be derived rather than replayed. If the
/// rebuild were wrong in any way -- a hash in the wrong slot, a subtree the
/// wrong shape, a long value mis-handled -- the root would not match, and if
/// it somehow did, the messages would not.
#[test]
fn rskj_chunks_rebuild_to_the_nodes_we_hold() {
    use rustock_trie::snapshot::chunk_from;
    use rustock_trie::snapshot_legacy::{decode_blob, rebuild};

    let text = include_str!("rskj-vectors/chunks.txt");
    let (mut keys, mut from, mut to) = (0usize, 0u64, 0u64);
    let mut checked = 0;

    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("KEYS ") {
            let f: Vec<&str> = rest.split_whitespace().collect();
            keys = f[0].parse().unwrap();
            from = f[2].parse().unwrap();
            to = f[4].parse().unwrap();
        } else if let Some(rest) = line.strip_prefix("BLOB ") {
            let (root, store) = trie(keys);
            let root_hash = root.compute_hash(&store);

            let blob = unhex(rest.trim());
            let chunk = decode_blob(&blob)
                .unwrap_or_else(|| panic!("{keys} keys {from}..{to}: blob did not decode"));

            let rebuilt = rebuild(&chunk, root_hash)
                .unwrap_or_else(|e| panic!("{keys} keys {from}..{to}: {e}"));

            assert_eq!(rebuilt.root, root_hash);

            // Every node rebuilt must be a node the trie actually holds, and
            // hash to the key it is filed under.
            let truth: std::collections::HashMap<String, Vec<u8>> =
                chunk_from(&root, 0, u64::MAX, &store)
                    .into_iter()
                    .map(|e| (hex(&alloy_primitives::keccak256(&e.message).0), e.message))
                    .collect();

            for (hash, message) in &rebuilt.nodes {
                assert_eq!(
                    alloy_primitives::keccak256(message),
                    *hash,
                    "{keys} keys {from}..{to}: a rebuilt node does not hash to its key"
                );
                // Every rebuilt node must be one the trie really holds --
                // the stubs included, since they are ancestors of the range
                // and not inventions of the rebuild.
                let expected = truth.get(&hex(&hash.0)).unwrap_or_else(|| {
                    panic!("{keys} keys {from}..{to}: rebuilt a node the trie does not hold")
                });
                assert_eq!(
                    message, expected,
                    "{keys} keys {from}..{to}: rebuilt node differs from ours"
                );
            }

            // The chunk's own nodes must all be there, not only the stubs.
            assert!(
                rebuilt.nodes.len() >= chunk.nodes.len(),
                "{keys} keys {from}..{to}: rebuilt {} nodes from {} sent",
                rebuilt.nodes.len(),
                chunk.nodes.len()
            );
            checked += 1;
        }
    }
    assert!(checked >= 6, "only {checked} chunks rebuilt");
}

/// A chunk that does not rebuild to the expected root is refused, which is the
/// whole of the security argument for this format.
#[test]
fn a_chunk_that_rebuilds_to_the_wrong_root_is_refused() {
    use rustock_trie::snapshot_legacy::{decode_blob, rebuild, RebuildError};
    use alloy_primitives::B256;

    let text = include_str!("rskj-vectors/chunks.txt");
    let blob = text
        .lines()
        .find_map(|l| l.strip_prefix("BLOB "))
        .map(|h| unhex(h.trim()))
        .expect("a vector");
    let chunk = decode_blob(&blob).expect("decodes");

    let wrong = B256::repeat_byte(0xAB);
    match rebuild(&chunk, wrong) {
        Err(RebuildError::WrongRoot { expected, .. }) => assert_eq!(expected, wrong),
        other => panic!("a chunk verified against the wrong root: {other:?}"),
    }
}

/// Nothing an rskj peer sends may panic the rebuild.
#[test]
fn no_junk_blob_can_panic_the_rebuild() {
    use rustock_trie::snapshot_legacy::{decode_blob, rebuild};
    use alloy_primitives::B256;

    let mut seed = 0x853C49E6748FEA9Bu64;
    let mut next = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };

    for _ in 0..4000 {
        let len = (next() % 160) as usize;
        let junk: Vec<u8> = (0..len).map(|_| next() as u8).collect();
        if let Some(chunk) = decode_blob(&junk) {
            let _ = rebuild(&chunk, B256::repeat_byte(1));
        }
    }
}

fn unhex(text: &str) -> Vec<u8> {
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16).unwrap())
        .collect()
}

/// The sizes `docs/snapshot-sync.md` states are the sizes the code computes,
/// and they are rskj's too.
///
/// A document that drifts from the code is worse than no document, and these
/// two numbers are the ones every offset in the protocol is built from.
#[test]
fn the_documented_size_formulas_hold() {
    use rustock_trie::snapshot::{chunk_from, stream_size, total_size};
    use rustock_trie::TrieStore;

    let (root, store) = trie(12);

    // total_size(root) is the whole trie, and rskj said 915 for this one.
    let total = total_size(&root, &store);
    assert_eq!(total, 915, "rskj's TrieDTO.getTotalSize() for this trie");

    // A node's offset is the sum of stream_size over everything before it,
    // and the run ends exactly at the total.
    let nodes = chunk_from(&root, 0, u64::MAX, &store);
    let mut at = 0u64;
    for entry in &nodes {
        assert_eq!(entry.offset, at, "offsets are not the running sum of stream_size");
        let node = rustock_trie::TrieNode::from_message(&entry.message, &store);
        assert_eq!(
            entry.span,
            stream_size(&node, &store),
            "a node's span is not its stream_size"
        );
        at += entry.span;
    }
    assert_eq!(at, total, "the traversal does not span the trie exactly");

    // total_size = children_size + external + message_len, as documented.
    for entry in &nodes {
        let node = rustock_trie::TrieNode::from_message(&entry.message, &store);
        let external =
            if node.has_long_value() { node.value_length() as u64 } else { 0 };
        assert_eq!(
            total_size(&node, &store),
            node.children_size + external + node.message_length(&store) as u64
        );
    }
    let _ = store.get(&[0u8; 32]);
}
