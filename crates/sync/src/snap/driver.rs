//! Turning a [`SnapSession`]'s decisions into messages, and answers back into
//! decisions.
//!
//! The session says *what* it wants; this says *to whom*, and remembers what
//! was asked so an answer can be matched to a question.
//!
//! # Why the request id matters
//!
//! A chunk response echoes the offset it claims to answer. Reading that field
//! would let a peer answer a question nobody asked -- serving the cheap start
//! of the trie over and over while a range it was actually assigned goes
//! unfilled. So the offset is taken from this node's own record of the
//! request, keyed by id, and the echoed field is used for nothing.

use super::session::{Action, Phase, SnapSession};
use alloy_primitives::{B256, B512, U256};
use rustock_core::{Block, Header};
use rustock_networking::protocol::snap::{
    ChunkPayload, SnapBlocksRequest, SnapChunkRequest, SnapStatusRequest,
};
use rustock_networking::protocol::{
    BlockHeadersQuery, BlockHeadersRequest, P2pMessage, RskMessage, RskSubMessage,
};
use std::collections::HashMap;
use std::time::{Duration, Instant};
use tracing::{debug, warn};

/// How long to wait on a snap request before offering the work elsewhere.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// rskj reads request ids through Java's signed long, so an id above 2^63
/// comes back sign-mangled.
fn fresh_id() -> u64 {
    rand::random::<u64>() & 0x7FFF_FFFF_FFFF_FFFF
}

/// What an outstanding request was for, so a late or missing answer can be
/// put back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pending {
    Status,
    Headers,
    Chunk { from: u64 },
    Blocks,
}

struct InFlight {
    what: Pending,
    peer: B512,
    sent: Instant,
}

/// A message to send, and the peer to send it to.
pub struct Outbound {
    pub peer: B512,
    pub message: P2pMessage,
}

pub struct SnapDriver {
    session: SnapSession,
    in_flight: HashMap<u64, InFlight>,
    /// Rotated through so one peer is not asked for everything.
    next_peer: usize,
}

impl SnapDriver {
    pub fn new(session: SnapSession) -> Self {
        Self { session, in_flight: HashMap::new(), next_peer: 0 }
    }

    pub fn phase(&self) -> Phase {
        self.session.phase()
    }

    pub fn session(&self) -> &SnapSession {
        &self.session
    }

    pub fn is_finished(&self) -> bool {
        matches!(self.session.phase(), Phase::Done | Phase::Failed)
    }

    /// Bytes of state downloaded, and the total once it is known.
    pub fn state_progress(&self) -> (u64, Option<u64>) {
        self.session.state_progress()
    }

    /// The block whose state this session is downloading.
    pub fn checkpoint(&self) -> Option<&Header> {
        self.session.checkpoint()
    }

    /// Give up on requests nobody answered, so the work can go to someone
    /// else. Returns how many were reclaimed.
    pub fn expire(&mut self, now: Instant) -> usize {
        let stale: Vec<u64> = self
            .in_flight
            .iter()
            .filter(|(_, r)| now.duration_since(r.sent) > REQUEST_TIMEOUT)
            .map(|(id, _)| *id)
            .collect();

        for id in &stale {
            if let Some(request) = self.in_flight.remove(id) {
                debug!(
                    target: "rustock::snap",
                    "snap request {id} to {:?} timed out", &request.peer.0[..4]
                );
                self.release(request.what);
            }
        }
        stale.len()
    }

    fn release(&mut self, what: Pending) {
        match what {
            Pending::Status => {}
            Pending::Headers => self.session.release_headers(),
            Pending::Chunk { from } => self.session.release_chunk(from),
            Pending::Blocks => self.session.release_blocks(),
        }
    }

    /// Drop everything outstanding with a peer that has gone away.
    pub fn forget_peer(&mut self, peer: &B512) {
        let theirs: Vec<u64> = self
            .in_flight
            .iter()
            .filter(|(_, r)| r.peer == *peer)
            .map(|(id, _)| *id)
            .collect();
        for id in theirs {
            if let Some(request) = self.in_flight.remove(&id) {
                self.release(request.what);
            }
        }
    }

    /// Ask for whatever the session wants next, spreading the requests over
    /// the peers given.
    pub fn poll(&mut self, peers: &[B512]) -> Vec<Outbound> {
        if peers.is_empty() || self.is_finished() {
            return Vec::new();
        }
        let actions = self.session.poll();
        self.dispatch(actions, peers)
    }

    fn dispatch(&mut self, actions: Vec<Action>, peers: &[B512]) -> Vec<Outbound> {
        let mut out = Vec::new();
        for action in actions {
            let peer = peers[self.next_peer % peers.len()];
            self.next_peer = self.next_peer.wrapping_add(1);

            let id = fresh_id();
            let (what, sub) = match action {
                Action::RequestStatus => (
                    Pending::Status,
                    RskSubMessage::SnapStatusRequest(SnapStatusRequest { id }),
                ),
                Action::RequestHeaders { from, count } => (
                    Pending::Headers,
                    RskSubMessage::BlockHeadersRequest(BlockHeadersRequest {
                        id,
                        query: BlockHeadersQuery { hash: from, count },
                    }),
                ),
                Action::RequestChunk { block_number, state_root, from, budget } => (
                    Pending::Chunk { from },
                    RskSubMessage::SnapChunkRequest(SnapChunkRequest {
                        id,
                        block_number,
                        from,
                        chunk_size: budget,
                        state_root: Some(state_root),
                    }),
                ),
                Action::RequestBlocks { block_number } => (
                    Pending::Blocks,
                    RskSubMessage::SnapBlocksRequest(SnapBlocksRequest { id, block_number }),
                ),
            };

            self.in_flight.insert(id, InFlight { what, peer, sent: Instant::now() });
            out.push(Outbound {
                peer,
                message: P2pMessage::RskMessage(RskMessage::new(sub)),
            });
        }
        out
    }

    pub fn on_status(
        &mut self,
        id: u64,
        blocks: &[Block],
        difficulties: &[U256],
        trie_size: u64,
        peers: &[B512],
    ) -> Vec<Outbound> {
        if !self.answers(id, |p| matches!(p, Pending::Status)) {
            return Vec::new();
        }
        let actions = self.session.on_status(blocks, difficulties, trie_size);
        self.dispatch(actions, peers)
    }

    pub fn on_headers(
        &mut self,
        id: u64,
        headers: &[Header],
        peers: &[B512],
    ) -> Vec<Outbound> {
        if !self.answers(id, |p| matches!(p, Pending::Headers)) {
            return Vec::new();
        }
        let actions = self.session.on_headers(headers);
        self.dispatch(actions, peers)
    }

    /// A chunk answer.
    ///
    /// The offset comes from this node's record of the request, never from
    /// the response.
    pub fn on_chunk(
        &mut self,
        id: u64,
        payload: &ChunkPayload,
        peers: &[B512],
    ) -> Vec<Outbound> {
        let Some(request) = self.in_flight.remove(&id) else {
            debug!(target: "rustock::snap", "chunk answer to unknown request {id}");
            return Vec::new();
        };
        let Pending::Chunk { from } = request.what else {
            warn!(target: "rustock::snap", "request {id} was not a chunk request");
            self.release(request.what);
            return Vec::new();
        };

        let actions = self.session.on_chunk(from, payload);
        self.dispatch(actions, peers)
    }

    pub fn on_blocks(
        &mut self,
        id: u64,
        blocks: &[Block],
        difficulties: &[U256],
        peers: &[B512],
    ) -> Vec<Outbound> {
        if !self.answers(id, |p| matches!(p, Pending::Blocks)) {
            return Vec::new();
        }
        let actions = self.session.on_blocks(blocks, difficulties);
        self.dispatch(actions, peers)
    }

    /// Consumes an outstanding request if it is the kind expected.
    fn answers(&mut self, id: u64, want: impl Fn(Pending) -> bool) -> bool {
        match self.in_flight.get(&id) {
            Some(request) if want(request.what) => {
                self.in_flight.remove(&id);
                true
            }
            Some(_) => {
                debug!(target: "rustock::snap", "request {id} answered with the wrong message");
                false
            }
            None => false,
        }
    }

    /// Headers arriving through the ordinary sync path may or may not belong
    /// to this session; this says whether the id is one of ours.
    pub fn awaits_headers(&self, id: u64) -> bool {
        matches!(self.in_flight.get(&id).map(|r| r.what), Some(Pending::Headers))
    }

    /// The state root being downloaded, once one has been offered.
    pub fn state_root(&self) -> Option<B256> {
        self.session.checkpoint().map(|h| h.state_root)
    }
}
