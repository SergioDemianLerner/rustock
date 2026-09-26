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

