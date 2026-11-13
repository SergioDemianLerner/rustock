//! Asking a peer to prove a piece of state, and believing none of it.
//!
//! A light client holds validated headers and no state. When something asks
//! it for an account — an `eth_getProof` call, or a wallet walking an
//! account's `lastModifiedBlock` backwards — it has to get the answer from a
//! peer, and the peer has no reason to be honest.
//!
//! The arrangement here keeps that honest by construction:
//!
//! 1. the caller names a block and an address;
//! 2. the request goes to a peer, correlated by request id;
//! 3. the answer comes back as raw trie nodes and is handed to the caller
//!    **unverified**;
//! 4. the caller checks it with [`rustock_trie::proof::verify_path`] against
//!    the state root in a header it validated itself.
//!
//! Step 3 is deliberate. This module does not verify, because it does not
//! hold the headers — and a verifier that cannot name the root it is checking
//! against is worse than none, since it looks like a guarantee and is not.
//! The root comes from the caller, who has it.
//!
//! A peer that answers with nonsense therefore costs a round trip, not a
//! wrong answer. A peer that does not answer at all costs a timeout; the
//! caller retries elsewhere.

use alloy_primitives::{Address, Bytes, B256, B512};
use rustock_networking::protocol::rsk::{RskMessage, RskSubMessage, TrieProofRequest};
use rustock_networking::P2pMessage;
use std::time::Instant;
use tokio::sync::{mpsc, oneshot};

/// What a peer sent back, still unchecked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawProof {
    pub peer: B512,
    pub block_number: u64,
    /// Nodes on the path to the account, deepest first. Empty is the peer's
    /// *claim* that the account is absent, which verification must test
    /// rather than accept.
    pub account_proof: Vec<Bytes>,
    pub storage_proofs: Vec<Vec<Bytes>>,
}

/// A request waiting for its answer.
pub struct PendingProof {
    pub reply: oneshot::Sender<RawProof>,
    pub sent: Instant,
    pub peer: B512,
}

/// What the RPC layer sends to the sync service to get a proof.
pub struct ProofCommand {
    pub block_number: u64,
    pub address: Address,
    pub storage_keys: Vec<B256>,
    pub reply: oneshot::Sender<RawProof>,
}

#[derive(Debug, thiserror::Error)]
pub enum ProofError {
    #[error("this node is not running a sync service that can fetch proofs")]
    Unavailable,
    #[error("no peer answered within the deadline")]
    Timeout,
}

/// A handle the RPC layer holds to ask for proofs.
#[derive(Clone)]
pub struct ProofClient {
    tx: mpsc::Sender<ProofCommand>,
}

impl ProofClient {
    pub fn new(tx: mpsc::Sender<ProofCommand>) -> Self {
        Self { tx }
    }

    /// Ask some peer to prove `address` at `block_number`.
    ///
    /// Returns the peer's raw answer. **It has not been checked.** The caller
    /// must verify it against the state root of its own validated header for
    /// that block before using anything in it.
    pub async fn fetch(
        &self,
        block_number: u64,
        address: Address,
        storage_keys: Vec<B256>,
        deadline: std::time::Duration,
    ) -> Result<RawProof, ProofError> {
        let (reply, rx) = oneshot::channel();
        let cmd = ProofCommand { block_number, address, storage_keys, reply };
        self.tx.send(cmd).await.map_err(|_| ProofError::Unavailable)?;
        match tokio::time::timeout(deadline, rx).await {
            Ok(Ok(proof)) => Ok(proof),
            // The service dropped the sender: no peer to ask, or it gave up.
            Ok(Err(_)) => Err(ProofError::Unavailable),
            Err(_) => Err(ProofError::Timeout),
        }
    }
}

/// Build a proof request with a fresh id.
///
/// Masked to 63 bits for the same reason every other request here is: rskj
/// carries ids as Java signed longs, and anything above 2^63 comes back
/// sign-mangled.
pub fn create_trie_proof_request(
    block_number: u64,
    address: Address,
    storage_keys: Vec<B256>,
) -> (u64, P2pMessage) {
    let id: u64 = rand::random::<u64>() & 0x7FFF_FFFF_FFFF_FFFF;
    let req = TrieProofRequest { id, block_number, address, storage_keys };
    (id, P2pMessage::RskMessage(RskMessage::new(RskSubMessage::TrieProofRequest(req))))
}
