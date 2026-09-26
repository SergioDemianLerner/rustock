//! Downloading a state trie from several peers at once.
//!
//! # The shape of the problem
//!
//! The trie has a single linear address space: every node sits at an offset
//! in the in-order traversal, and offsets come from sizes each node's hash
//! commits to. So "download the state" is "cover `0..total`", and any two
//! ranges can be fetched independently, from unrelated peers, in any order.
//!
//! That is the whole of the parallelism, and it needs no coordination between
//! peers: a chunk proves itself against the state root, so a peer serving the
//! middle of the trie neither knows nor affects what another is serving.
//!
//! # Not knowing `total` at the start
//!
//! The snap status message offers a trie size, but it is a peer's claim and
//! the download cannot rest on it. It is used only to fan out: the space is
//! split into as many slices as there are requests allowed in flight, and
//! workers start on each.
//!
//! The first chunk that verifies -- from any offset, from any peer -- carries
//! the root node, which commits to the size of the whole trie. At that point
//! the real total replaces the hint and the slices are trimmed or extended to
//! fit. A peer that overstates the size costs a few wasted requests at the
//! end; one that understates it cannot truncate the download, because the
//! last slice grows to the size the root proves.
//!
//! # What a bad peer can do
//!
//! Serve nothing, serve slowly, or serve something that fails verification.
//! All three end the same way: the range goes back in the queue and someone
//! else is asked. Nothing a peer sends is written to the store before it is
//! checked against the root, so a failed chunk leaves no trace.

use super::SnapConfig;
use alloy_primitives::{keccak256, B256};
use rustock_networking::protocol::snap::{ChunkPayload, SnapEntry};
use rustock_trie::snapshot::total_size;
use rustock_trie::snapshot_proof::{verify_chunk, ChunkProof, Entry, VerifyError};
use rustock_trie::{TrieNode, TrieStore};
use std::sync::Arc;
use tracing::{debug, trace};

/// A contiguous stretch of the traversal that one worker walks front to back.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Slice {
    /// First offset this slice is responsible for.
    start: u64,
    /// Next offset to ask for. Everything before this, within the slice, is
    /// downloaded and stored.
    cursor: u64,
    /// One past the last offset this slice is responsible for.
    end: u64,
    /// Whether a request for `cursor` is outstanding.
    in_flight: bool,
}

impl Slice {
    fn wants_work(&self) -> bool {
        !self.in_flight && self.cursor < self.end
    }
}

/// A range to ask some peer for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChunkRequest {
    /// Offset to start at.
    pub from: u64,
    /// Bytes of nodes wanted. A server may send fewer.
    pub budget: u64,
}

/// What a verified chunk added to the download.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Progress {
    /// Nodes written to the store.
    pub nodes: usize,
    /// Offset space newly covered.
    pub advanced: u64,
    /// Whether that was the last of it.
    pub complete: bool,
}

/// How much more than the requested budget a chunk may be before it is thrown
/// away unread.
///
/// A server is free to round up, and a straddling node at the end can push a
/// chunk over. What this stops is a peer answering a 100KB request with the
/// 16MB the transport allows, repeatedly, to spend the client's CPU on
/// verification. Rejecting on size costs nothing; verifying does not.
const OVERSIZE_FACTOR: u64 = 4;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ChunkError {
    #[error("the chunk failed verification: {0}")]
    Invalid(#[from] VerifyError),
    #[error("no request for offset {0} was outstanding")]
    Unsolicited(u64),
    #[error("the peer serves rskj's older chunk format, which this node does not read")]
    LegacyFormat,
    #[error("the answer is {got} bytes against a {asked} byte request")]
    Oversized { asked: u64, got: u64 },
}

/// Downloading one state trie.
///
/// Holds no peers and does no I/O: it decides what to ask for and judges what
/// comes back. Which peer to ask is the caller's business, which keeps peer
/// selection, scoring and timeouts out of the part that has to be right.
pub struct StateDownload {
    root_hash: B256,
    store: Arc<dyn TrieStore>,
    budget: u64,
    slices: Vec<Slice>,
    /// The size the root proves, once a chunk has carried it.
    total: Option<u64>,
    /// The size a peer claimed, used until then.
    hinted_total: u64,
    stored_nodes: u64,
    stored_bytes: u64,
}

impl StateDownload {
    pub fn new(
        root_hash: B256,
        hinted_total: u64,
        config: &SnapConfig,
        store: Arc<dyn TrieStore>,
    ) -> Self {
        let workers = config.max_in_flight.max(1);
        let hinted_total = hinted_total.max(1);

        let mut download = Self {
            root_hash,
            store,
            budget: config.chunk_bytes,
            slices: Vec::new(),
            total: None,
            hinted_total,
            stored_nodes: 0,
            stored_bytes: 0,
        };
        download.slices = download.split(0, hinted_total, workers);
        download
    }

    /// Cuts `from..to` into `count` slices of roughly equal size.
    fn split(&self, from: u64, to: u64, count: usize) -> Vec<Slice> {
        let span = to.saturating_sub(from);
        let count = (count as u64).min(span.max(1)) as usize;
        let step = span / count as u64;

        (0..count)
            .map(|i| {
                let start = from + step * i as u64;
                let end = if i + 1 == count { to } else { from + step * (i as u64 + 1) };
                Slice { start, cursor: start, end, in_flight: false }
            })
            .collect()
    }

    /// The next range to ask a peer for, if any is free.
    ///
    /// Returns `None` when every slice is either finished or already out with
    /// someone -- meaning the caller should wait for an answer rather than
    /// open another request.
    pub fn next_request(&mut self) -> Option<ChunkRequest> {
        let slice = self.slices.iter_mut().find(|s| s.wants_work())?;
        slice.in_flight = true;
        Some(ChunkRequest { from: slice.cursor, budget: self.budget })
    }

    /// Give up on an outstanding request so someone else can be asked.
    ///
    /// The range is untouched: nothing was written, so there is nothing to
    /// undo. Called on a timeout, a disconnect, or a chunk that failed to
    /// verify.
    pub fn release(&mut self, from: u64) {
        if let Some(slice) = self.slices.iter_mut().find(|s| s.in_flight && s.cursor == from) {
            slice.in_flight = false;
        }
    }

    /// Check a chunk and, if it holds up, keep it.
    ///
    /// The order matters: verify first, store second. A chunk that fails
    /// leaves the store exactly as it was.
    pub fn accept(&mut self, from: u64, proof: &ChunkProof) -> Result<Progress, ChunkError> {
        if !self.slices.iter().any(|s| s.in_flight && s.cursor == from) {
            return Err(ChunkError::Unsolicited(from));
        }

        // Judged on size before anything is read: an answer wildly larger than
        // the question is refused without paying to verify it.
        //
        // A chunk of one node is exempt. A server always sends at least one,
        // whatever the budget, because a node bigger than the budget is the
        // only way past it -- a contract's code, say. Refusing those on size
        // would make them permanently undownloadable, which is a worse failure
        // than the one this guards against. The transport's own limit still
        // bounds a single node.
        let allowed = self.budget.saturating_mul(OVERSIZE_FACTOR).max(1 << 18);
        let got = proof.wire_len();
        if got > allowed && proof.entries.len() > 1 {
            self.release(from);
            return Err(ChunkError::Oversized { asked: self.budget, got });
        }

        let verified = match verify_chunk(self.root_hash, from, proof) {
            Ok(v) => v,
            Err(e) => {
                self.release(from);
                return Err(ChunkError::Invalid(e));
            }
        };

        // The root proves the size of the whole trie. Once that is in hand
        // the peer's claim is no longer needed anywhere.
        if self.total.is_none() {
            self.adopt_total(verified.total);
        }

        for node in &verified.nodes {
            self.store.put(keccak256(&node.message).as_slice(), &node.message);
            self.stored_bytes += node.message.len() as u64;
            for value in &node.long_values {
                self.store.put(keccak256(value).as_slice(), value);
                self.stored_bytes += value.len() as u64;
            }
        }
        self.stored_nodes += verified.nodes.len() as u64;

        let reached = verified.nodes.last().expect("verified chunks are non-empty").end();
        let mut advanced = 0;
        if let Some(slice) = self.slices.iter_mut().find(|s| s.in_flight && s.cursor == from) {
            advanced = reached.saturating_sub(slice.cursor).min(slice.end - slice.cursor);
            slice.cursor = reached.max(slice.cursor);
            slice.in_flight = false;
        }

        trace!(
            target: "rustock::snap",
            "accepted {} nodes from offset {from}, reaching {reached}",
            verified.nodes.len()
        );

        Ok(Progress { nodes: verified.nodes.len(), advanced, complete: self.is_complete() })
    }

    /// Replace the hinted size with the one the root proves, and reshape the
    /// slices around it.
    fn adopt_total(&mut self, total: u64) {
        self.total = Some(total);
        if total == self.hinted_total {
            return;
        }
        debug!(
            target: "rustock::snap",
            "trie size is {total}, peer hinted {}; adjusting slices", self.hinted_total
        );

        // Anything wholly past the end is finished by definition; the slice
        // that straddles the end stops there; the last one grows if the trie
        // turned out bigger than advertised.
        let last = self.slices.len().saturating_sub(1);
        for (i, slice) in self.slices.iter_mut().enumerate() {
            if i == last {
                slice.end = total.max(slice.cursor);
            } else {
                slice.end = slice.end.min(total);
            }
            slice.start = slice.start.min(slice.end);
            slice.cursor = slice.cursor.clamp(slice.start, slice.end);
        }
    }

    /// Every offset accounted for.
    pub fn is_complete(&self) -> bool {
        self.total.is_some() && self.slices.iter().all(|s| s.cursor >= s.end)
    }

    /// Offset space covered so far, and the total once it is known.
    pub fn progress(&self) -> (u64, Option<u64>) {
        // A slice's cursor can run past its end: the last chunk of a slice
        // usually straddles the boundary, and the whole node comes with it.
        // Those bytes belong to the next slice, which will fetch them again,
        // so counting them here would put progress over 100%.
        let covered: u64 = self
            .slices
            .iter()
            .map(|s| s.cursor.min(s.end).saturating_sub(s.start))
            .sum();
        (covered, self.total)
    }

    pub fn stored_nodes(&self) -> u64 {
        self.stored_nodes
    }

    pub fn stored_bytes(&self) -> u64 {
        self.stored_bytes
    }

    pub fn root_hash(&self) -> B256 {
        self.root_hash
    }

    /// Walk the assembled trie in the local store and confirm it is all
    /// there.
    ///
    /// Each chunk was already proved against the root, so this is not a second
    /// opinion on the peers -- it is a check on this node: that every verified
    /// node reached the disk and can be read back. Cheap next to the download,
    /// and it fails here rather than during the first block that touches a
    /// missing node.
    pub fn verify_stored(&self) -> Result<u64, String> {
        let message = self
            .store
            .get(self.root_hash.as_slice())
            .ok_or_else(|| format!("state root {:?} is not in the store", self.root_hash))?;
        let root = TrieNode::try_from_message(&message, self.store.as_ref())
            .ok_or_else(|| "the stored state root does not parse".to_string())?;

        let total = total_size(&root, self.store.as_ref());
        if let Some(expected) = self.total {
            if total != expected {
                return Err(format!("stored trie spans {total} bytes, expected {expected}"));
            }
        }

        // Walking it chunk by chunk visits every node and resolves every
        // hash, which is exactly the question being asked.
        let mut offset = 0u64;
        let mut nodes = 0u64;
        while offset < total {
            let chunk = rustock_trie::snapshot::chunk_from(
                &root,
                offset,
                1 << 20,
                self.store.as_ref(),
            );
            let Some(last) = chunk.last() else {
                return Err(format!("the stored trie has a hole at offset {offset}"));
            };
            nodes += chunk.len() as u64;
            offset = last.end();
        }
        Ok(nodes)
    }
}

/// Turns a wire payload into the proof the verifier reads.
///
/// rskj's older format is refused by name rather than guessed at.
pub fn proof_from_payload(payload: &ChunkPayload) -> Result<ChunkProof, ChunkError> {
    match payload {
        ChunkPayload::Legacy(_) => Err(ChunkError::LegacyFormat),
        ChunkPayload::Proved { entries, witness } => Ok(ChunkProof {
            entries: entries.iter().map(entry_from_wire).collect(),
            witness: witness.iter().map(|w| w.to_vec()).collect(),
        }),
    }
}

fn entry_from_wire(entry: &SnapEntry) -> Entry {
    Entry {
        message: entry.message.to_vec(),
        long_values: entry.long_values.iter().map(|v| v.to_vec()).collect(),
    }
}
