//! Proving a chunk belongs to a state root, and checking that proof.
//!
//! # What has to be proved
//!
//! A chunk is a contiguous run of the in-order traversal. Two things must
//! hold before a client may keep it:
//!
//! 1. **Inclusion** -- every node really is in the trie under the expected
//!    root.
//! 2. **Completeness** -- the peer did not quietly skip a node inside the
//!    range it claims to have sent. Inclusion alone does not give this: a
//!    peer could send a truthful subset and the client would build state with
//!    holes in it, which is worse than an outright failure because it looks
//!    like success.
//!
//! # How both are proved at once
//!
//! The verifier **re-runs the server's own traversal** over nothing but the
//! bytes the peer supplied, and requires it to produce exactly the chunk that
//! arrived.
//!
//! That works because the traversal is a pure function of hash-committed
//! data. Offsets come from `children_size`, which is varint-encoded into each
//! node's message and therefore fixed by the node's hash (RSKIP107); node
//! identity is the keccak of that message. So if the replay starts from the
//! expected root hash, uses only messages that hash to what their parents
//! say, and yields the same sequence, then that sequence *is* the trie's
//! in-order run at that offset. There is no freedom left for a peer to
//! exploit: omitting a node changes the replay, and changing a node changes
//! a hash.
//!
//! The extra data a peer must send for the replay to get off the ground is
//! the [`Witness`] -- the ancestors between the root and the chunk, and the
//! roots of the sibling subtrees the traversal skips over on the way down.
//! Their sizes are what let the offset arithmetic choose a branch. It is
//! O(depth) nodes, a few kilobytes, and it is derived rather than guessed:
//! the server records precisely what its own traversal read.
//!
//! # Why the chunk carries full consensus messages
//!
//! rskj's snapshot serializer strips child hashes, recovering roughly 40% of
//! the bytes, and rebuilds them on the client. This does not, and the reason
//! is that stripping forces the client to rebuild the trie bottom-up before
//! it can hash anything -- the step the PoC report measures as quadratic and
//! lists as future work to replace.
//!
//! Keeping the consensus message means a verified node is *already* what the
//! store holds: the client writes `put(hash, message)` and is done. No
//! reconstruction pass, no intermediate representation, and every node is
//! independently checkable the moment it arrives. The trade is bandwidth for
//! CPU, memory and a whole class of bugs. A stripped mode can be added later
//! as a negotiated option without touching the protocol, since only the entry
//! encoding would change.

use crate::node::TrieNode;
use crate::snapshot::{chunk_from_limited, StreamNode};
use crate::store::TrieStore;
use alloy_primitives::B256;
use std::collections::HashMap;
use std::sync::Mutex;

/// Node messages a client needs in order to replay the traversal, beyond the
/// chunk itself: the path from the root down to the chunk, and the roots of
/// the subtrees skipped along the way.
pub type Witness = Vec<Vec<u8>>;

/// A chunk together with everything needed to prove it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProvenChunk {
    /// Offset the chunk starts at, in the in-order traversal.
    pub from: u64,
    /// The chunk's entries, in order.
    pub nodes: Vec<StreamNode>,
    /// Extra node messages the replay needs. Not stored by the client; used
    /// once and discarded.
    pub witness: Witness,
}

impl ProvenChunk {
    /// Offset just past the last entry: where the next chunk begins.
    pub fn end(&self) -> u64 {
        self.nodes.last().map_or(self.from, |n| n.end())
    }

    pub fn wire_len(&self) -> u64 {
        self.nodes.iter().map(|n| n.wire_len()).sum::<u64>()
            + self.witness.iter().map(|w| w.len() as u64).sum::<u64>()
    }
}

/// A store that remembers every key it served.
///
/// Used to derive the witness by construction rather than by reasoning about
/// which nodes *ought* to be needed -- if the traversal read it, the replay
/// will read it too, because it is the same traversal.
struct Recorder<'a> {
    inner: &'a dyn TrieStore,
    seen: Mutex<HashMap<B256, Vec<u8>>>,
}

impl TrieStore for Recorder<'_> {
    fn get(&self, key: &[u8]) -> Option<Vec<u8>> {
        let value = self.inner.get(key)?;
        if key.len() == 32 {
            self.seen
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(B256::from_slice(key), value.clone());
        }
        Some(value)
    }
    fn put(&self, key: &[u8], value: &[u8]) {
        self.inner.put(key, value)
    }
}

/// Build a chunk and the witness that proves it.
pub fn prove_chunk(
    root: &TrieNode,
    from: u64,
    budget: u64,
    store: &dyn TrieStore,
) -> ProvenChunk {
    let recorder = Recorder { inner: store, seen: Mutex::new(HashMap::new()) };

    // Round-trip the root through its own message before traversing. A node
    // held in memory carries its children as live objects, so descending it
    // would never touch the store and the recorder would see nothing; a node
    // parsed from a message carries them as hashes, so every step down is a
    // store read and the recorder observes exactly what a client lacks.
    let root_message = root.to_message(store);
    let root = TrieNode::from_message(&root_message, &recorder);

    let nodes = chunk_from_limited(&root, from, budget, usize::MAX, &recorder);

    // Whatever the traversal read that is not already travelling with the
    // chunk is what the client will be missing. Long values are excluded:
    // they are carried by their own entry and are never needed to navigate.
    let mut carried: std::collections::HashSet<&[u8]> =
        nodes.iter().map(|n| n.message.as_slice()).collect();
    carried.extend(nodes.iter().flat_map(|n| n.long_values.iter().map(|v| v.as_slice())));

    let seen = recorder.seen.into_inner().unwrap_or_else(|e| e.into_inner());
    let mut witness: Witness =
        seen.into_values().filter(|m| !carried.contains(m.as_slice())).collect();

    // The root's own message is read by nobody -- the traversal starts from
    // it -- so the recorder cannot have seen it. Without it the client has
    // nothing to anchor against the expected state root.
    if !carried.contains(root_message.as_slice()) && !witness.contains(&root_message) {
        witness.push(root_message);
    }

    // Deterministic order: a proof should be a function of what it proves,
    // not of a hash map's iteration order.
    witness.sort_unstable();

    ProvenChunk { from, nodes, witness }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum VerifyError {
    #[error("the chunk is empty")]
    Empty,
    #[error("no node in the proof hashes to the expected root {0}")]
    RootMissing(B256),
    #[error("the replayed traversal produced {replayed} nodes, the chunk has {received}")]
    LengthMismatch { replayed: usize, received: usize },
    #[error("entry {index} differs from what the trie actually holds at that offset")]
    NodeMismatch { index: usize },
    #[error("entry {index} claims offset {claimed}, the traversal puts it at {actual}")]
    OffsetMismatch { index: usize, claimed: u64, actual: u64 },
    #[error("entry {index} carries a long value that is not the one its node commits to")]
    BadLongValue { index: usize },
    #[error("the chunk does not start at the requested offset {requested}")]
    WrongStart { requested: u64 },
    #[error("entry {index} is not a well-formed trie node message")]
    Malformed { index: usize },
}

/// A store backed only by what a peer sent.
struct ProofStore(HashMap<B256, Vec<u8>>);

impl TrieStore for ProofStore {
    fn get(&self, key: &[u8]) -> Option<Vec<u8>> {
        if key.len() != 32 {
            return None;
        }
        self.0.get(&B256::from_slice(key)).cloned()
    }
    fn put(&self, _key: &[u8], _value: &[u8]) {}
}

/// Check a chunk against the expected state root.
///
/// On success the caller may write every returned entry straight into its
/// trie store: the messages are the consensus serialization, keyed by their
/// own keccak.
pub fn verify_chunk(root_hash: B256, chunk: &ProvenChunk) -> Result<(), VerifyError> {
    if chunk.nodes.is_empty() {
        return Err(VerifyError::Empty);
    }
    if chunk.nodes[0].offset > chunk.from {
        // A straddled node legitimately starts *before* the requested offset;
        // starting after it would mean a gap.
        return Err(VerifyError::WrongStart { requested: chunk.from });
    }

    // A node that commits to a long value cannot even be parsed without it:
    // the value's length is part of the node's message length and therefore
    // of its hash. So check the values first -- both that each is the one its
    // node commits to and that none is missing -- and only then replay. This
    // also means a lie about a value is reported as a lie about a value,
    // rather than surfacing later as an unexplained node mismatch.
    for (index, entry) in chunk.nodes.iter().enumerate() {
        if TrieNode::try_from_message(&entry.message, &EmptyStore).is_none() {
            return Err(VerifyError::Malformed { index });
        }
        check_long_values(entry).map_err(|_| VerifyError::BadLongValue { index })?;
    }

    // Everything the peer sent, addressed the only way it can be: by hash.
    // A message filed under a key it does not hash to simply is not found
    // later, so there is nothing to check separately here.
    let mut map: HashMap<B256, Vec<u8>> = HashMap::new();
    for message in chunk.witness.iter().chain(chunk.nodes.iter().map(|n| &n.message)) {
        map.insert(alloy_primitives::keccak256(message), message.clone());
    }
    for value in chunk.nodes.iter().flat_map(|n| n.long_values.iter()) {
        map.insert(alloy_primitives::keccak256(value), value.clone());
    }

    let store = ProofStore(map);
    let root_message =
        store.get(root_hash.as_slice()).ok_or(VerifyError::RootMissing(root_hash))?;
    // Safe to unwrap only because the witness has already been screened: a
    // message that does not parse cannot be under its own keccak here.
    let root = TrieNode::try_from_message(&root_message, &store)
        .ok_or(VerifyError::RootMissing(root_hash))?;

    // Replay the server's traversal over only these bytes. Stop where the
    // peer stopped, so a short chunk is judged on what it claims rather than
    // on what a full one would have held.
    let replay =
        chunk_from_limited(&root, chunk.from, u64::MAX, chunk.nodes.len(), &store);

    if replay.len() != chunk.nodes.len() {
        return Err(VerifyError::LengthMismatch {
            replayed: replay.len(),
            received: chunk.nodes.len(),
        });
    }
    for (index, (expected, got)) in replay.iter().zip(chunk.nodes.iter()).enumerate() {
        if expected.message != got.message {
            return Err(VerifyError::NodeMismatch { index });
        }
        if expected.offset != got.offset {
            return Err(VerifyError::OffsetMismatch {
                index,
                claimed: got.offset,
                actual: expected.offset,
            });
        }
    }

    Ok(())
}

/// A store that knows nothing.
///
/// Used to read a node's *commitments* without consulting anyone's copy of
/// what it commits to. A long-value node parsed this way keeps its value hash
/// and simply has no value attached, which is precisely the question being
/// asked: what should this entry be carrying?
struct EmptyStore;

impl TrieStore for EmptyStore {
    fn get(&self, _key: &[u8]) -> Option<Vec<u8>> {
        None
    }
    fn put(&self, _key: &[u8], _value: &[u8]) {}
}

/// An entry must carry exactly the long values its node, and its node's
/// embedded children, commit to: no forgeries, no omissions, no extras.
///
/// Omissions matter as much as forgeries. A node whose long value is absent
/// parses as a node with no value at all -- a different node, of a different
/// length, with a different hash -- so letting one through would corrupt the
/// state rather than merely leave a gap in it.
fn check_long_values(entry: &StreamNode) -> Result<(), ()> {
    let node = TrieNode::try_from_message(&entry.message, &EmptyStore).ok_or(())?;
    let mut wanted: Vec<B256> = Vec::new();
    collect_value_hashes(&node, &mut wanted);

    if wanted.len() != entry.long_values.len() {
        return Err(());
    }
    for value in &entry.long_values {
        let h = alloy_primitives::keccak256(value);
        match wanted.iter().position(|w| *w == h) {
            Some(i) => {
                wanted.swap_remove(i);
            }
            None => return Err(()),
        }
    }
    Ok(())
}

/// Long-value hashes committed by a node and by its embedded children.
///
/// Reads the commitment, not the value: parsed against [`EmptyStore`], a node
/// that holds a hash but no value is exactly a node with a long value stored
/// elsewhere. A short value is inline, so it arrives with both.
///
/// In a node parsed from a message the in-memory children are precisely the
/// embedded ones -- anything else is a hash -- so recursing into them needs no
/// embeddability test.
fn collect_value_hashes(node: &TrieNode, out: &mut Vec<B256>) {
    if node.value.is_none() {
        if let Some(h) = node.value_hash {
            out.push(h);
        }
    }
    for child in [&node.left, &node.right] {
        if let crate::node::NodeRef::Node(n) = child {
            collect_value_hashes(n, out);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snapshot::total_size;
    use crate::{MemoryTrieStore, TrieKeySlice};

    fn toy_trie(n: usize) -> (TrieNode, MemoryTrieStore, B256) {
        let store = MemoryTrieStore::new();
        let mut root = TrieNode::empty();
        for i in 0..n {
            let key = [(i >> 8) as u8, i as u8, (i * 7) as u8, (i * 13) as u8];
            let value: Vec<u8> = if i % 5 == 0 {
                (0..64u8).map(|b| b.wrapping_add(i as u8)).collect()
            } else {
                vec![i as u8, (i >> 8) as u8]
            };
            root = root.put(&TrieKeySlice::from_key(&key), &value, &store);
        }
        root.save(&store, true);
        let hash = root.compute_hash(&store);
        (root, store, hash)
    }

    /// Honest chunks verify, at every budget, across the whole trie.
    #[test]
    fn an_honest_chunk_verifies() {
        for (n, budget) in [(1usize, 64u64), (40, 128), (200, 100), (200, 4096)] {
            let (root, store, root_hash) = toy_trie(n);
            let total = total_size(&root, &store);
            let mut offset = 0u64;
            let mut seen = 0usize;
            while offset < total {
                let chunk = prove_chunk(&root, offset, budget, &store);
                assert!(!chunk.nodes.is_empty(), "n={n} budget={budget}: stalled");
                verify_chunk(root_hash, &chunk)
                    .unwrap_or_else(|e| panic!("n={n} budget={budget} at {offset}: {e}"));
                seen += chunk.nodes.len();
                offset = chunk.end();
            }
            assert!(seen > 0);
        }
    }

    /// **The completeness property.** A peer that drops a node from the
    /// middle of the range it claims must be caught.
    ///
    /// This is the attack inclusion proofs alone do not stop: every node the
    /// peer sends is genuine, so each one proves its own membership. What is
    /// wrong is the *set* -- and the client that accepts it builds state with
    /// a hole in it, which is worse than a failure because it looks like
    /// success and fails later, somewhere else.
    #[test]
    fn dropping_a_node_from_the_middle_is_caught() {
        let (root, store, root_hash) = toy_trie(120);
        let chunk = prove_chunk(&root, 0, 4096, &store);
        assert!(chunk.nodes.len() > 4, "need a chunk worth cutting a hole in");

        for victim in 1..chunk.nodes.len() - 1 {
            let mut tampered = chunk.clone();
            // The dropped node's bytes are still in the proof, so the peer is
            // not even hiding data -- only omitting an entry.
            let removed = tampered.nodes.remove(victim);
            tampered.witness.push(removed.message);
            assert!(
                verify_chunk(root_hash, &tampered).is_err(),
                "dropping entry {victim} went undetected"
            );
        }
    }

    /// Reordering entries is caught, even though every node is genuine.
    #[test]
    fn reordering_entries_is_caught() {
        let (root, store, root_hash) = toy_trie(120);
        let chunk = prove_chunk(&root, 0, 4096, &store);
        assert!(chunk.nodes.len() > 3);

        let mut tampered = chunk.clone();
        tampered.nodes.swap(1, 2);
        assert!(verify_chunk(root_hash, &tampered).is_err(), "a swap went undetected");
    }

    /// A node from elsewhere in the same trie cannot be spliced in.
    #[test]
    fn substituting_a_genuine_node_from_elsewhere_is_caught() {
        let (root, store, root_hash) = toy_trie(200);
        let first = prove_chunk(&root, 0, 2048, &store);
        let later = prove_chunk(&root, first.end(), 2048, &store);
        assert!(!first.nodes.is_empty() && !later.nodes.is_empty());

        let mut tampered = first.clone();
        let index = tampered.nodes.len() / 2;
        tampered.nodes[index] = StreamNode {
            offset: tampered.nodes[index].offset,
            ..later.nodes[0].clone()
        };
        tampered.witness.extend(later.witness.clone());
        assert!(verify_chunk(root_hash, &tampered).is_err(), "a splice went undetected");
    }

    /// Editing a node's bytes is caught -- it no longer hashes to what its
    /// parent commits to, so the replay cannot find it.
    #[test]
    fn mutating_a_node_is_caught() {
        let (root, store, root_hash) = toy_trie(80);
        let chunk = prove_chunk(&root, 0, 4096, &store);

        for victim in 0..chunk.nodes.len().min(12) {
            let mut tampered = chunk.clone();
            let last = tampered.nodes[victim].message.len() - 1;
            tampered.nodes[victim].message[last] ^= 0x01;
            assert!(
                verify_chunk(root_hash, &tampered).is_err(),
                "a flipped bit in entry {victim} went undetected"
            );
        }
    }

    /// A chunk proved against one root does not verify against another.
    #[test]
    fn a_chunk_from_a_different_trie_is_rejected() {
        let (root_a, store_a, _) = toy_trie(100);
        let (_, _, hash_b) = toy_trie(101);
        let chunk = prove_chunk(&root_a, 0, 4096, &store_a);
        assert_eq!(verify_chunk(hash_b, &chunk), Err(VerifyError::RootMissing(hash_b)));
    }

    /// Lying about an entry's offset is caught even when the node is real and
    /// in the right place -- the client uses that number to decide where the
    /// next chunk starts, so a lie here opens a gap.
    #[test]
    fn a_false_offset_is_caught() {
        let (root, store, root_hash) = toy_trie(100);
        let chunk = prove_chunk(&root, 0, 4096, &store);
        let mut tampered = chunk.clone();
        let last = tampered.nodes.len() - 1;
        tampered.nodes[last].offset += 7;
        assert!(matches!(
            verify_chunk(root_hash, &tampered),
            Err(VerifyError::OffsetMismatch { .. })
        ));
    }

    /// A substituted long value is caught: it must hash to what its node
    /// commits to. Without this the state would carry attacker-chosen bytes
    /// under a genuine key.
    #[test]
    fn a_forged_long_value_is_caught() {
        let (root, store, root_hash) = toy_trie(60);
        let chunk = prove_chunk(&root, 0, u64::MAX, &store);
        let index = chunk
            .nodes
            .iter()
            .position(|n| !n.long_values.is_empty())
            .expect("the fixture holds long values");

        let mut tampered = chunk.clone();
        tampered.nodes[index].long_values[0] = vec![0xAA; 64];
        assert_eq!(
            verify_chunk(root_hash, &tampered),
            Err(VerifyError::BadLongValue { index })
        );

        // Dropping it is caught too.
        let mut missing = chunk.clone();
        missing.nodes[index].long_values.clear();
        assert_eq!(
            verify_chunk(root_hash, &missing),
            Err(VerifyError::BadLongValue { index })
        );
    }

    /// Withholding the witness makes the chunk unverifiable rather than
    /// accepted on trust.
    #[test]
    fn an_absent_witness_fails_closed() {
        let (root, store, root_hash) = toy_trie(200);
        let chunk = prove_chunk(&root, 3000, 2048, &store);
        assert!(!chunk.witness.is_empty(), "a mid-trie chunk needs a witness");

        let mut stripped = chunk.clone();
        stripped.witness.clear();
        assert!(verify_chunk(root_hash, &stripped).is_err(), "verified without a witness");
    }

    /// The witness is small: a few nodes on the path, not a second copy of
    /// the trie.
    #[test]
    fn the_witness_is_proportional_to_depth_not_to_size() {
        let (root, store, _) = toy_trie(400);
        let total = total_size(&root, &store);
        let chunk = prove_chunk(&root, total / 2, 4096, &store);
        let witness_bytes: u64 = chunk.witness.iter().map(|w| w.len() as u64).sum();
        let chunk_bytes: u64 = chunk.nodes.iter().map(|n| n.wire_len()).sum();
        assert!(
            witness_bytes < chunk_bytes,
            "witness {witness_bytes}B should be smaller than the chunk {chunk_bytes}B"
        );
    }

    /// A verified chunk is directly storable: its messages are the consensus
    /// serialization, keyed by their own hash. This is what removes the
    /// reconstruction pass entirely.
    #[test]
    fn verified_chunks_rebuild_the_trie_by_plain_insertion() {
        let (root, store, root_hash) = toy_trie(200);
        let total = total_size(&root, &store);

        let rebuilt = MemoryTrieStore::new();
        let mut offset = 0u64;
        while offset < total {
            let chunk = prove_chunk(&root, offset, 1024, &store);
            verify_chunk(root_hash, &chunk).expect("honest chunk");
            for entry in &chunk.nodes {
                let h = alloy_primitives::keccak256(&entry.message);
                rebuilt.put(h.as_slice(), &entry.message);
                for value in &entry.long_values {
                    rebuilt.put(alloy_primitives::keccak256(value).as_slice(), value);
                }
            }
            offset = chunk.end();
        }

        // The rebuilt store must answer every key the original does.
        let root2 = TrieNode::from_message(&rebuilt.get(root_hash.as_slice()).unwrap(), &rebuilt);
        assert_eq!(root2.compute_hash(&rebuilt), root_hash);
        for i in 0..200usize {
            let key = [(i >> 8) as u8, i as u8, (i * 7) as u8, (i * 13) as u8];
            let k = TrieKeySlice::from_key(&key);
            assert_eq!(
                root2.get(&k, &rebuilt),
                root.get(&k, &store),
                "key {i} differs after rebuild"
            );
        }
    }
}

#[cfg(test)]
mod fuzz {
    use super::*;
    use crate::MemoryTrieStore;

    /// A verifier that can be made to panic is a verifier that can be used to
    /// stop the node. Nothing a peer can say may do worse than return an
    /// error, so throw junk at every entry point.
    #[test]
    fn no_peer_input_can_panic_the_verifier() {
        let mut seed = 0x9E3779B97F4A7C15u64;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };

        let store = MemoryTrieStore::new();
        for case in 0..3000 {
            let len = (next() % 70) as usize;
            let junk: Vec<u8> = (0..len).map(|_| next() as u8).collect();

            // The parser itself: any answer is fine, a panic is not.
            let _ = TrieNode::try_from_message(&junk, &store);

            // And the whole verification path, with junk in every field.
            let chunk = ProvenChunk {
                from: next(),
                nodes: vec![StreamNode {
                    offset: next(),
                    span: next(),
                    message: junk.clone(),
                    long_values: vec![junk.clone()],
                }],
                witness: vec![junk.clone()],
            };
            let root = alloy_primitives::keccak256(&junk);
            assert!(verify_chunk(root, &chunk).is_err(), "case {case} verified junk");
        }
    }

    /// Junk grafted onto a real proof is still junk.
    #[test]
    fn no_corruption_of_a_real_proof_can_panic_the_verifier() {
        let store = MemoryTrieStore::new();
        let mut root = TrieNode::empty();
        for i in 0..150u32 {
            let key = i.to_be_bytes();
            let value: Vec<u8> =
                if i % 4 == 0 { vec![i as u8; 64] } else { vec![i as u8, 1, 2] };
            root = root.put(&crate::TrieKeySlice::from_key(&key), &value, &store);
        }
        root.save(&store, true);
        let root_hash = root.compute_hash(&store);
        let honest = prove_chunk(&root, 0, 3000, &store);

        let mut seed = 0xD1B54A32D192ED03u64;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };

        for _ in 0..4000 {
            let mut chunk = honest.clone();
            match next() % 5 {
                0 => {
                    let i = (next() as usize) % chunk.nodes.len();
                    let m = &mut chunk.nodes[i].message;
                    if !m.is_empty() {
                        let at = (next() as usize) % m.len();
                        m[at] = next() as u8;
                    }
                }
                1 => {
                    let i = (next() as usize) % chunk.nodes.len();
                    chunk.nodes[i].message.truncate((next() as usize) % 40);
                }
                2 => {
                    let i = (next() as usize) % chunk.nodes.len();
                    chunk.nodes[i].long_values.push(vec![next() as u8; 40]);
                }
                3 if !chunk.witness.is_empty() => {
                    let i = (next() as usize) % chunk.witness.len();
                    let w = &mut chunk.witness[i];
                    let at = (next() as usize) % w.len();
                    w[at] ^= 0xFF;
                }
                _ => {
                    chunk.nodes.truncate(1 + (next() as usize) % chunk.nodes.len());
                    chunk.from = next() % 5000;
                }
            }
            // Only the outcome is asserted; the point is that it returns one.
            let _ = verify_chunk(root_hash, &chunk);
        }
    }
}
