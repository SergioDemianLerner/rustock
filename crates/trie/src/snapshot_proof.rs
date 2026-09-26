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
//! Because the replay computes the offsets, the peer never states them. It
//! sends nodes; where they sit is not its opinion to give. rskj's chunk
//! response carries `from` and `to` as claims the client must then check --
//! fields that cannot be wrong here because they do not exist on the wire.
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
//! the bytes, and rebuilds them on the client: `TrieDTOInOrderRecoverer`
//! guesses each subtree root by scanning for the largest `children_size` in
//! the range, recursively. That is the step the PoC report measures as
//! quadratic and lists as future work to replace, and it is also a heuristic
//! standing where a definition belongs.
//!
//! This does not strip them. The replay walks *forward* from an offset, which
//! is linear and needs no guessing, because the trie says where its children
//! are rather than being asked to reveal it.
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

/// One node as it travels: its consensus message, plus any values too long to
/// live inside it.
///
/// Note what is *not* here. The entry does not say where in the traversal it
/// belongs. A peer is not asked where a node is, it is told -- the verifier
/// derives every offset itself, from sizes the node's own hash commits to. A
/// field a peer does not fill in is a field a peer cannot lie about.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Entry {
    /// RSKIP107 node message: exactly the bytes the store holds under this
    /// node's hash.
    pub message: Vec<u8>,
    /// Values over 32 bytes, which the trie keeps outside the node under
    /// their own hash. Carried with the node that commits to them, including
    /// on behalf of its embedded children.
    pub long_values: Vec<Vec<u8>>,
}

/// A chunk together with everything needed to check it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ChunkProof {
    /// The chunk's nodes, in traversal order.
    pub entries: Vec<Entry>,
    /// Node messages the replay needs but that the client is not being given
    /// to keep: the path down to the chunk and the subtree roots skipped on
    /// the way. Used once and discarded.
    pub witness: Witness,
}

/// Node messages a client needs in order to replay the traversal, beyond the
/// chunk itself.
pub type Witness = Vec<Vec<u8>>;

impl ChunkProof {
    /// Bytes on the wire, near enough: the payload without RLP framing.
    pub fn wire_len(&self) -> u64 {
        let entries: u64 = self
            .entries
            .iter()
            .map(|e| {
                e.message.len() as u64
                    + e.long_values.iter().map(|v| v.len() as u64).sum::<u64>()
            })
            .sum();
        entries + self.witness.iter().map(|w| w.len() as u64).sum::<u64>()
    }
}

/// A store that remembers every key it served.
///
/// Used to derive the witness by construction rather than by reasoning about
/// which nodes *ought* to be needed: if the traversal read it, the replay will
/// read it too, because it is the same traversal.
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

/// Serve a chunk of the traversal starting at `from`, with the witness that
/// proves it.
///
/// `budget` is a soft byte limit: at least one node always comes back, so a
/// client asking for less than a single node still makes progress rather than
/// stalling.
pub fn prove_chunk(
    root: &TrieNode,
    from: u64,
    budget: u64,
    store: &dyn TrieStore,
) -> ChunkProof {
    let recorder = Recorder { inner: store, seen: Mutex::new(HashMap::new()) };

    // Round-trip the root through its own message before traversing. A node
    // held in memory carries its children as live objects, so descending it
    // would never touch the store and the recorder would see nothing; a node
    // parsed from a message carries them as hashes, so every step down is a
    // store read and the recorder observes exactly what a client lacks.
    let root_message = root.to_message(store);
    let root = TrieNode::from_message(&root_message, &recorder);

    let nodes = chunk_from_limited(&root, from, budget, usize::MAX, &recorder);

    let entries: Vec<Entry> = nodes
        .into_iter()
        .map(|n| Entry { message: n.message, long_values: n.long_values })
        .collect();

    // Whatever the traversal read that is not already travelling with the
    // chunk is what the client will be missing.
    let mut carried: std::collections::HashSet<&[u8]> =
        entries.iter().map(|e| e.message.as_slice()).collect();
    carried.extend(entries.iter().flat_map(|e| e.long_values.iter().map(|v| v.as_slice())));

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

    ChunkProof { entries, witness }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum VerifyError {
    #[error("the chunk is empty")]
    Empty,
    #[error("no node in the proof hashes to the expected root {0}")]
    RootMissing(B256),
    #[error("entry {index} is not a well-formed trie node message")]
    Malformed { index: usize },
    #[error("entry {index} carries long values that are not the ones its node commits to")]
    BadLongValue { index: usize },
    #[error("the trie holds {replayed} nodes from offset {from}, the chunk claims {received}")]
    LengthMismatch { from: u64, replayed: usize, received: usize },
    #[error("entry {index} is not the node the trie holds at that point in the traversal")]
    NodeMismatch { index: usize },
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

/// Check a chunk against the state root the client independently trusts, and
/// return the nodes with the offsets the trie actually puts them at.
///
/// `from` is the offset the *client* asked for, never a number the peer sent
/// back. On success every returned node may be written straight into the trie
/// store under the keccak of its message, and the next chunk starts at the
/// last node's [`StreamNode::end`].
pub fn verify_chunk(
    root_hash: B256,
    from: u64,
    proof: &ChunkProof,
) -> Result<Vec<StreamNode>, VerifyError> {
    if proof.entries.is_empty() {
        return Err(VerifyError::Empty);
    }

    // A node that commits to a long value cannot even be parsed without it:
    // the value's length is part of the node's message and therefore of its
    // hash. So settle the values first -- each is the one its node commits to,
    // and none is missing -- and only then replay. This also means a lie about
    // a value is reported as a lie about a value rather than surfacing later
    // as an unexplained mismatch.
    for (index, entry) in proof.entries.iter().enumerate() {
        if TrieNode::try_from_message(&entry.message, &EmptyStore).is_none() {
            return Err(VerifyError::Malformed { index });
        }
        check_long_values(entry).map_err(|_| VerifyError::BadLongValue { index })?;
    }

    // Everything the peer sent, addressed the only way it can be: by hash. A
    // message filed under a key it does not hash to is simply never found, so
    // there is nothing to check separately here.
    let mut map: HashMap<B256, Vec<u8>> = HashMap::new();
    for message in proof.witness.iter().chain(proof.entries.iter().map(|e| &e.message)) {
        map.insert(alloy_primitives::keccak256(message), message.clone());
    }
    for value in proof.entries.iter().flat_map(|e| e.long_values.iter()) {
        map.insert(alloy_primitives::keccak256(value), value.clone());
    }

    let store = ProofStore(map);
    let root_message =
        store.get(root_hash.as_slice()).ok_or(VerifyError::RootMissing(root_hash))?;
    let root = TrieNode::try_from_message(&root_message, &store)
        .ok_or(VerifyError::RootMissing(root_hash))?;

    // Replay the server's traversal over nothing but these bytes. Stop where
    // the peer stopped, so a short chunk is judged on what it claims rather
    // than on what a full one would have held.
    let replay = chunk_from_limited(&root, from, u64::MAX, proof.entries.len(), &store);

    if replay.len() != proof.entries.len() {
        return Err(VerifyError::LengthMismatch {
            from,
            replayed: replay.len(),
            received: proof.entries.len(),
        });
    }
    for (index, (expected, got)) in replay.iter().zip(proof.entries.iter()).enumerate() {
        if expected.message != got.message {
            return Err(VerifyError::NodeMismatch { index });
        }
    }

    Ok(replay)
}

/// An entry must carry exactly the long values its node, and its node's
/// embedded children, commit to: no forgeries, no omissions, no extras.
///
/// Omissions matter as much as forgeries. A node whose long value is absent
/// parses as a node with no value at all -- a different node, of a different
/// length, with a different hash -- so letting one through would corrupt the
/// state rather than merely leave a gap in it.
fn check_long_values(entry: &Entry) -> Result<(), ()> {
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
/// holding a hash but no value is exactly a node whose value lives elsewhere.
/// A short value is inline, so it arrives with both.
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
                let proof = prove_chunk(&root, offset, budget, &store);
                assert!(!proof.entries.is_empty(), "n={n} budget={budget}: stalled");
                let nodes = verify_chunk(root_hash, offset, &proof)
                    .unwrap_or_else(|e| panic!("n={n} budget={budget} at {offset}: {e}"));
                seen += nodes.len();
                let next = nodes.last().expect("non-empty").end();
                assert!(next > offset, "n={n} budget={budget}: no progress at {offset}");
                offset = next;
            }
            assert!(seen > 0);
        }
    }

    /// The offsets come out of the trie, not out of the message: what the
    /// verifier returns is what an honest traversal of the real trie gives.
    #[test]
    fn the_verifier_derives_the_true_offsets() {
        let (root, store, root_hash) = toy_trie(150);
        let truth = crate::snapshot::chunk_from(&root, 0, 2048, &store);
        let proof = prove_chunk(&root, 0, 2048, &store);
        let got = verify_chunk(root_hash, 0, &proof).expect("honest chunk");

        assert_eq!(got.len(), truth.len());
        for (a, b) in got.iter().zip(truth.iter()) {
            assert_eq!(a.offset, b.offset);
            assert_eq!(a.span, b.span);
            assert_eq!(a.message, b.message);
            assert_eq!(a.long_values, b.long_values);
        }
    }

    /// **The completeness property.** A peer that drops a node from the
    /// middle of the range it claims must be caught.
    ///
    /// This is the attack inclusion proofs alone do not stop: every node the
    /// peer sends is genuine, so each one proves its own membership. What is
    /// wrong is the *set* -- and the client that accepts it builds state with
    /// a hole in it, which is worse than a failure because it looks like
    /// success and fails later, somewhere else entirely.
    #[test]
    fn dropping_a_node_from_the_middle_is_caught() {
        let (root, store, root_hash) = toy_trie(120);
        let proof = prove_chunk(&root, 0, 4096, &store);
        assert!(proof.entries.len() > 4, "need a chunk worth cutting a hole in");

        for victim in 1..proof.entries.len() - 1 {
            let mut tampered = proof.clone();
            // The dropped node's bytes stay in the proof, so the peer is not
            // even hiding data -- only omitting an entry.
            let removed = tampered.entries.remove(victim);
            tampered.witness.push(removed.message);
            assert!(
                verify_chunk(root_hash, 0, &tampered).is_err(),
                "dropping entry {victim} went undetected"
            );
        }
    }

    /// A peer may always send fewer nodes than asked for -- a smaller chunk
    /// is a legitimate answer, not an attack. What it cannot do is mislead
    /// the client about where the short chunk stopped, because it never says:
    /// the client resumes from an offset it derived itself, so the node that
    /// was left out is the next one it asks for.
    #[test]
    fn a_short_chunk_resumes_exactly_where_it_stopped() {
        let (root, store, root_hash) = toy_trie(120);
        let full = prove_chunk(&root, 0, 4096, &store);
        assert!(full.entries.len() > 4);

        let small = prove_chunk(&root, 0, 200, &store);
        assert!(small.entries.len() < full.entries.len(), "budget was not binding");

        let nodes = verify_chunk(root_hash, 0, &small).expect("a short chunk is legitimate");
        let next = nodes.last().unwrap().end();

        let rest = prove_chunk(&root, next, 4096, &store);
        assert_eq!(
            rest.entries[0].message,
            full.entries[nodes.len()].message,
            "resuming skipped or repeated a node"
        );
    }

    /// Cutting entries off the end of an honest chunk cannot produce a *wrong*
    /// answer, only a shorter one or none at all. The offsets the client keeps
    /// are true whatever the peer withholds.
    #[test]
    fn truncating_an_honest_chunk_never_yields_wrong_offsets() {
        let (root, store, root_hash) = toy_trie(120);
        let full = prove_chunk(&root, 0, 4096, &store);
        let truth = verify_chunk(root_hash, 0, &full).expect("honest");

        for keep in 1..full.entries.len() {
            let mut cut = full.clone();
            cut.entries.truncate(keep);
            // The witness is left untouched, which is the best a peer could
            // manage: it may withhold, not invent.
            if let Ok(nodes) = verify_chunk(root_hash, 0, &cut) {
                assert_eq!(nodes.len(), keep);
                for (a, b) in nodes.iter().zip(truth.iter()) {
                    assert_eq!(a.offset, b.offset, "truncation moved an offset");
                    assert_eq!(a.message, b.message);
                }
            }
        }
    }

    /// Reordering entries is caught, even though every node is genuine.
    #[test]
    fn reordering_entries_is_caught() {
        let (root, store, root_hash) = toy_trie(120);
        let proof = prove_chunk(&root, 0, 4096, &store);
        assert!(proof.entries.len() > 3);

        let mut tampered = proof.clone();
        tampered.entries.swap(1, 2);
        assert!(verify_chunk(root_hash, 0, &tampered).is_err(), "a swap went undetected");
    }

    /// A node from elsewhere in the same trie cannot be spliced in.
    #[test]
    fn substituting_a_genuine_node_from_elsewhere_is_caught() {
        let (root, store, root_hash) = toy_trie(200);
        let first = prove_chunk(&root, 0, 2048, &store);
        let first_end =
            verify_chunk(root_hash, 0, &first).expect("honest").last().unwrap().end();
        let later = prove_chunk(&root, first_end, 2048, &store);

        let mut tampered = first.clone();
        let index = tampered.entries.len() / 2;
        tampered.entries[index] = later.entries[0].clone();
        tampered.witness.extend(later.witness.clone());
        assert!(
            verify_chunk(root_hash, 0, &tampered).is_err(),
            "a splice went undetected"
        );
    }

    /// Editing a node's bytes is caught -- it no longer hashes to what its
    /// parent commits to, so the replay cannot reach it.
    #[test]
    fn mutating_a_node_is_caught() {
        let (root, store, root_hash) = toy_trie(80);
        let proof = prove_chunk(&root, 0, 4096, &store);

        for victim in 0..proof.entries.len().min(12) {
            let mut tampered = proof.clone();
            let last = tampered.entries[victim].message.len() - 1;
            tampered.entries[victim].message[last] ^= 0x01;
            assert!(
                verify_chunk(root_hash, 0, &tampered).is_err(),
                "a flipped bit in entry {victim} went undetected"
            );
        }
    }

    /// A chunk proved against one root does not verify against another.
    #[test]
    fn a_chunk_from_a_different_trie_is_rejected() {
        let (root_a, store_a, _) = toy_trie(100);
        let (_, _, hash_b) = toy_trie(101);
        let proof = prove_chunk(&root_a, 0, 4096, &store_a);
        assert_eq!(
            verify_chunk(hash_b, 0, &proof),
            Err(VerifyError::RootMissing(hash_b))
        );
    }

    /// A chunk is bound to the offset the client asked for. Serving the start
    /// of the trie to someone resuming in the middle is caught.
    #[test]
    fn a_chunk_for_the_wrong_offset_is_caught() {
        let (root, store, root_hash) = toy_trie(200);
        let proof = prove_chunk(&root, 0, 2048, &store);
        let total = total_size(&root, &store);
        assert!(verify_chunk(root_hash, total / 2, &proof).is_err(), "wrong offset accepted");
    }

    /// A substituted long value is caught: it must hash to what its node
    /// commits to. Without this the state would carry attacker-chosen bytes
    /// under a genuine key -- a balance, a nonce, a contract's code.
    #[test]
    fn a_forged_long_value_is_caught() {
        let (root, store, root_hash) = toy_trie(60);
        let proof = prove_chunk(&root, 0, u64::MAX, &store);
        let index = proof
            .entries
            .iter()
            .position(|e| !e.long_values.is_empty())
            .expect("the fixture holds long values");

        let mut tampered = proof.clone();
        tampered.entries[index].long_values[0] = vec![0xAA; 64];
        assert_eq!(
            verify_chunk(root_hash, 0, &tampered),
            Err(VerifyError::BadLongValue { index })
        );

        // Dropping it is caught too: a node whose long value is missing is a
        // different node, not an incomplete one.
        let mut missing = proof.clone();
        missing.entries[index].long_values.clear();
        assert_eq!(
            verify_chunk(root_hash, 0, &missing),
            Err(VerifyError::BadLongValue { index })
        );

        // And so is an extra one nobody asked for.
        let mut extra = proof.clone();
        extra.entries[index].long_values.push(vec![0x11; 64]);
        assert_eq!(
            verify_chunk(root_hash, 0, &extra),
            Err(VerifyError::BadLongValue { index })
        );
    }

    /// Withholding the witness makes the chunk unverifiable rather than
    /// accepted on trust.
    #[test]
    fn an_absent_witness_fails_closed() {
        let (root, store, root_hash) = toy_trie(200);
        let total = total_size(&root, &store);
        let from = total / 2;
        let proof = prove_chunk(&root, from, 2048, &store);
        assert!(!proof.witness.is_empty(), "a mid-trie chunk needs a witness");

        let mut stripped = proof.clone();
        stripped.witness.clear();
        assert!(
            verify_chunk(root_hash, from, &stripped).is_err(),
            "verified without a witness"
        );
    }

    /// The witness is a path, not a second copy of the trie.
    #[test]
    fn the_witness_is_proportional_to_depth_not_to_size() {
        let (root, store, _) = toy_trie(400);
        let total = total_size(&root, &store);
        let proof = prove_chunk(&root, total / 2, 4096, &store);
        let witness_bytes: u64 = proof.witness.iter().map(|w| w.len() as u64).sum();
        let chunk_bytes = proof.wire_len() - witness_bytes;
        assert!(
            witness_bytes < chunk_bytes,
            "witness {witness_bytes}B should be smaller than the chunk {chunk_bytes}B"
        );
    }

    /// A verified chunk is directly storable: the messages are the consensus
    /// serialization, keyed by their own hash. This is what removes the
    /// reconstruction pass rskj needs -- there is nothing left to rebuild.
    #[test]
    fn verified_chunks_rebuild_the_trie_by_plain_insertion() {
        let (root, store, root_hash) = toy_trie(200);
        let total = total_size(&root, &store);

        let rebuilt = MemoryTrieStore::new();
        let mut offset = 0u64;
        while offset < total {
            let proof = prove_chunk(&root, offset, 1024, &store);
            let nodes = verify_chunk(root_hash, offset, &proof).expect("honest chunk");
            for node in &nodes {
                rebuilt.put(alloy_primitives::keccak256(&node.message).as_slice(), &node.message);
                for value in &node.long_values {
                    rebuilt.put(alloy_primitives::keccak256(value).as_slice(), value);
                }
            }
            offset = nodes.last().expect("non-empty").end();
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

    fn xorshift(seed: u64) -> impl FnMut() -> u64 {
        let mut s = seed;
        move || {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            s
        }
    }

    /// A verifier that can be made to panic is a verifier that can be used to
    /// stop the node. Nothing a peer can say may do worse than return an
    /// error, so throw junk at every entry point.
    #[test]
    fn no_peer_input_can_panic_the_verifier() {
        let mut next = xorshift(0x9E3779B97F4A7C15);
        let store = MemoryTrieStore::new();

        for case in 0..3000 {
            let len = (next() % 70) as usize;
            let junk: Vec<u8> = (0..len).map(|_| next() as u8).collect();

            // The parser itself: any answer is fine, a panic is not.
            let _ = TrieNode::try_from_message(&junk, &store);

            let proof = ChunkProof {
                entries: vec![Entry {
                    message: junk.clone(),
                    long_values: vec![junk.clone()],
                }],
                witness: vec![junk.clone()],
            };
            let root = alloy_primitives::keccak256(&junk);
            assert!(
                verify_chunk(root, next(), &proof).is_err(),
                "case {case} verified junk"
            );
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

        let mut next = xorshift(0xD1B54A32D192ED03);
        for _ in 0..4000 {
            let mut proof = honest.clone();
            match next() % 5 {
                0 => {
                    let i = (next() as usize) % proof.entries.len();
                    let m = &mut proof.entries[i].message;
                    if !m.is_empty() {
                        let at = (next() as usize) % m.len();
                        m[at] = next() as u8;
                    }
                }
                1 => {
                    let i = (next() as usize) % proof.entries.len();
                    proof.entries[i].message.truncate((next() as usize) % 40);
                }
                2 => {
                    let i = (next() as usize) % proof.entries.len();
                    proof.entries[i].long_values.push(vec![next() as u8; 40]);
                }
                3 if !proof.witness.is_empty() => {
                    let i = (next() as usize) % proof.witness.len();
                    let w = &mut proof.witness[i];
                    let at = (next() as usize) % w.len();
                    w[at] ^= 0xFF;
                }
                _ => {
                    proof.entries.truncate(1 + (next() as usize) % proof.entries.len());
                }
            }
            // Only that it returns an outcome at all is asserted here.
            let _ = verify_chunk(root_hash, next() % 5000, &proof);
        }
    }
}
