//! Checking a unitrie proof received from somebody you do not trust.
//!
//! A light client holds validated headers and nothing else. Every answer it
//! gets about state arrives from a peer with no reason to be honest, so the
//! only thing that makes such an answer usable is this check: the nodes the
//! peer sent must hash up to the state root the client already has in a
//! header it validated itself.
//!
//! # How the check works
//!
//! Not by walking the array and comparing child references by hand, which is
//! easy to write and easy to write *almost* correctly. Instead the proof
//! nodes are loaded into a scratch store under their own hashes, and the
//! ordinary trie lookup is run against that store from the claimed root.
//!
//! That single step establishes everything at once:
//!
//! - **the root is the right one** — the walk starts from `root_hash`, so a
//!   proof rooted anywhere else cannot be used;
//! - **every node is linked** — a child reference resolves only if some
//!   provided node hashes to exactly it, so an unrelated node cannot be
//!   spliced in;
//! - **the path is the key's** — the lookup descends by the key's own bits,
//!   so a valid path to a *different* key resolves nothing. This is the one
//!   a hand-rolled verifier tends to miss, and it is the one that matters:
//!   without it a peer answers "your balance" with somebody else's.
//!
//! # Absence
//!
//! [`Outcome::Absent`] is a real result, not a failure: the walk reached a
//! point where the trie genuinely ends, and every node it needed to get there
//! was supplied and hash-linked. That is a proof that the key is not in the
//! trie under this root.
//!
//! It is different from [`ProofError::Incomplete`], which means the walk
//! needed a node the peer did not send. A peer can always withhold, and the
//! client must read that as "I learned nothing, ask someone else" rather than
//! as "the key is absent". Collapsing the two would let any peer deny the
//! existence of any account by staying quiet.

use crate::node::empty_trie_hash;
use crate::{MemoryTrieStore, TrieKeySlice, TrieNode, TrieStore};
use alloy_primitives::B256;
use sha3::{Digest, Keccak256};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProofError {
    /// The proof carried no nodes at all, and the root is not the empty trie.
    Empty,
    /// None of the supplied nodes hashes to the root being proved against.
    RootNotPresent { expected: B256 },
    /// The walk needed a node the peer did not supply. **Not** an absence
    /// proof: the peer withheld, or sent a path for a different key.
    Incomplete { missing: B256 },
}

impl std::fmt::Display for ProofError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => write!(f, "the proof is empty"),
            Self::RootNotPresent { expected } => {
                write!(f, "no node in the proof hashes to the root {expected:?}")
            }
            Self::Incomplete { missing } => write!(
                f,
                "the proof does not reach the key: node {missing:?} was not supplied"
            ),
        }
    }
}

impl std::error::Error for ProofError {}

/// What a verified proof established.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The key is in the trie under this root, holding this value.
    Value(Vec<u8>),
    /// The key is provably *not* in the trie under this root.
    Absent,
}

/// Check `nodes` against `root_hash` and report what they prove about `key`.
///
/// `nodes` are raw unitrie node messages in any order — the deepest-first
/// order the wire uses is not required, because the nodes are indexed by hash
/// rather than followed positionally.
pub fn verify_path(
    nodes: &[impl AsRef<[u8]>],
    key: &[u8],
    root_hash: B256,
) -> Result<Outcome, ProofError> {
    if nodes.is_empty() {
        // An empty trie legitimately proves every key absent.
        return if root_hash == empty_trie_hash() {
            Ok(Outcome::Absent)
        } else {
            Err(ProofError::Empty)
        };
    }

    // Index by hash. A node the peer invented is still indexed, and is still
    // unreachable: nothing refers to a hash nobody committed to.
    let scratch = MemoryTrieStore::new();
    let mut root_message: Option<&[u8]> = None;
    for node in nodes {
        let bytes = node.as_ref();
        let hash = B256::from_slice(&Keccak256::digest(bytes));
        scratch.put(hash.as_slice(), bytes);
        if hash == root_hash {
            root_message = Some(bytes);
        }
    }

    let Some(root_message) = root_message else {
        return Err(ProofError::RootNotPresent { expected: root_hash });
    };

    let root = TrieNode::from_message(root_message, &scratch);
    match root.try_get(&TrieKeySlice::from_key(key), &scratch) {
        Ok(Some(value)) => Ok(Outcome::Value(value)),
        Ok(None) => Ok(Outcome::Absent),
        Err(missing) => Err(ProofError::Incomplete { missing }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account_key;
    use alloy_primitives::{Address, U256};

    /// Build a small trie and take a real proof out of it, the way a server
    /// would.
    fn fixture() -> (MemoryTrieStore, TrieNode, Vec<Address>) {
        let store = MemoryTrieStore::new();
        let mut root = TrieNode::empty();
        let addrs: Vec<Address> = (1u8..=6).map(Address::repeat_byte).collect();
        for (i, a) in addrs.iter().enumerate() {
            let acct = crate::AccountState::new(U256::from(i as u64), U256::from(1000 + i as u64));
            root = root.put(&TrieKeySlice::from_key(&account_key(a)), &acct.encode(), &store);
        }
        root.save(&store, true);
        (store, root, addrs)
    }

    fn proof_for(root: &TrieNode, store: &dyn TrieStore, key: &[u8]) -> Vec<Vec<u8>> {
        root.nodes_on_path(&TrieKeySlice::from_key(key), store)
            .map(|nodes| nodes.iter().map(|n| n.to_message(store)).collect())
            .unwrap_or_default()
    }

    #[test]
    fn a_real_proof_verifies_and_yields_the_value() {
        let (store, root, addrs) = fixture();
        let root_hash = root.compute_hash(&store);
        let key = account_key(&addrs[2]);
        let proof = proof_for(&root, &store, &key);

        let expected = root.get(&TrieKeySlice::from_key(&key), &store).unwrap();
        assert_eq!(verify_path(&proof, &key, root_hash), Ok(Outcome::Value(expected)));
    }

    /// The check a hand-rolled verifier forgets. Every node here is genuine
    /// and correctly linked -- it is simply the path to somebody else's
    /// account, and answering with it would report the wrong balance.
    #[test]
    fn a_valid_proof_for_another_key_proves_nothing_about_this_one() {
        let (store, root, addrs) = fixture();
        let root_hash = root.compute_hash(&store);

        let their_proof = proof_for(&root, &store, &account_key(&addrs[0]));
        let my_key = account_key(&addrs[4]);

        match verify_path(&their_proof, &my_key, root_hash) {
            Err(ProofError::Incomplete { .. }) => {}
            other => panic!("another key's path must not answer for mine, got {other:?}"),
        }
    }

    #[test]
    fn a_proof_rooted_elsewhere_is_refused() {
        let (store, root, addrs) = fixture();
        let key = account_key(&addrs[1]);
        let proof = proof_for(&root, &store, &key);

        assert_eq!(
            verify_path(&proof, &key, B256::repeat_byte(0xEE)),
            Err(ProofError::RootNotPresent { expected: B256::repeat_byte(0xEE) })
        );
    }

    /// Withholding must not look like absence, or any peer could deny any
    /// account simply by staying quiet.
    #[test]
    fn a_withheld_node_is_incomplete_and_not_absence() {
        let (store, root, addrs) = fixture();
        let root_hash = root.compute_hash(&store);
        let key = account_key(&addrs[3]);
        let proof = proof_for(&root, &store, &key);
        assert!(proof.len() > 1, "need a path with something to withhold");

        // Keep only the root. Dropping the *leaf* would withhold nothing:
        // an account node is small enough to be embedded in its parent, so
        // its bytes travel inside that parent whether or not it also appears
        // in the array. What has to be withheld is a node the trie reaches
        // by hash.
        let only_root = vec![proof[proof.len() - 1].clone()];

        match verify_path(&only_root, &key, root_hash) {
            Err(ProofError::Incomplete { .. }) => {}
            other => panic!("a withheld node must not read as absence, got {other:?}"),
        }
    }

    /// A key the trie really does not hold, proved with the nodes that show
    /// where the search ends.
    #[test]
    fn a_genuinely_absent_key_is_proved_absent() {
        let (store, root, _) = fixture();
        let root_hash = root.compute_hash(&store);
        let absent = account_key(&Address::repeat_byte(0x99));

        // Everything in the trie, so nothing can be said to be missing.
        let mut all = Vec::new();
        collect(&root, &store, &mut all);

        assert_eq!(verify_path(&all, &absent, root_hash), Ok(Outcome::Absent));
    }

    fn collect(node: &TrieNode, store: &dyn TrieStore, out: &mut Vec<Vec<u8>>) {
        out.push(node.to_message(store));
        for child in [&node.left, &node.right] {
            if let crate::Resolution::Found(n) = child.try_resolve(store) {
                collect(&n, store, out);
            }
        }
    }

    /// The server-side counterpart: `nodes_to_divergence` supplies exactly
    /// the nodes that let a client verify an absence, where `nodes_on_path`
    /// supplies nothing and leaves the client taking the peer's word.
    #[test]
    fn divergence_nodes_prove_an_absence_and_a_bare_path_does_not() {
        let (store, root, _) = fixture();
        let root_hash = root.compute_hash(&store);
        let absent = account_key(&Address::repeat_byte(0x99));
        let slice = TrieKeySlice::from_key(&absent);

        // What `nodes_on_path` offers for an absent key: nothing.
        assert!(root.nodes_on_path(&slice, &store).is_none());

        let proof: Vec<Vec<u8>> = root
            .nodes_to_divergence(&slice, &store)
            .expect("the walk must reach a divergence")
            .iter()
            .map(|n| n.to_message(&store))
            .collect();

        assert_eq!(
            verify_path(&proof, &absent, root_hash),
            Ok(Outcome::Absent),
            "the divergence nodes must prove the key is not there"
        );
    }

    /// And the same nodes must not be able to claim an absence for a key
    /// that IS present -- otherwise a peer could erase an account by
    /// answering with somebody else's divergence.
    #[test]
    fn a_divergence_proof_cannot_deny_a_key_that_exists() {
        let (store, root, addrs) = fixture();
        let root_hash = root.compute_hash(&store);
        let absent = account_key(&Address::repeat_byte(0x99));

        let proof: Vec<Vec<u8>> = root
            .nodes_to_divergence(&TrieKeySlice::from_key(&absent), &store)
            .unwrap()
            .iter()
            .map(|n| n.to_message(&store))
            .collect();

        let real_key = account_key(&addrs[2]);
        match verify_path(&proof, &real_key, root_hash) {
            Ok(Outcome::Absent) => panic!("an existing account was denied"),
            Ok(Outcome::Value(_)) | Err(ProofError::Incomplete { .. }) => {}
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn an_empty_proof_proves_absence_only_against_the_empty_trie() {
        let empty: Vec<Vec<u8>> = Vec::new();
        assert_eq!(verify_path(&empty, b"anything", empty_trie_hash()), Ok(Outcome::Absent));
        assert_eq!(
            verify_path(&empty, b"anything", B256::repeat_byte(7)),
            Err(ProofError::Empty)
        );
    }
}
