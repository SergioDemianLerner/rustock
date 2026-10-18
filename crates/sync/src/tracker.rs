use alloy_primitives::B512;
use rustock_core::types::header::Header;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::time::{Duration, Instant};

/// Number of chunk requests to keep in flight per peer.
pub(crate) const PIPELINE_DEPTH: usize = 4;

/// Tracks chunk assignment and ordering across multiple peers.
#[derive(Debug)]
pub struct PeerChunkTracker {
    /// Per-peer FIFO of chunk indices that are in flight (in request order).
    pub(crate) in_flight: HashMap<B512, VecDeque<usize>>,
    /// Next skeleton index to assign to a peer.
    pub(crate) next_to_assign: usize,
    /// Next chunk index to process (must process in order for correct TD chain).
    pub(crate) next_to_process: usize,
    /// Buffer for responses that arrived ahead of processing order, with the
    /// peer that supplied each.
    ///
    /// The peer is carried because a chunk that fails to validate is a
    /// **punishing** scoring event, and chunks are processed in skeleton order
    /// rather than arrival order -- so the peer whose response happened to
    /// trigger the drain is usually not the one that sent the bad chunk.
    /// Punishing that peer would be worse than not punishing at all.
    pub(crate) buffered: BTreeMap<usize, (B512, Vec<Header>)>,
    /// Total number of chunks in this skeleton.
    pub(crate) total_chunks: usize,
    /// Per-peer instant since which we have been waiting for a response
    /// (set on first outstanding request, refreshed on each response).
    pub(crate) waiting_since: HashMap<B512, Instant>,
}

impl PeerChunkTracker {
    /// Creates a new tracker starting at chunk index 1 (index 0 is the known
    /// starting point in the skeleton).
    pub fn new(skeleton_len: usize) -> Self {
        Self {
            in_flight: HashMap::new(),
            next_to_assign: 1,
            next_to_process: 1,
            buffered: BTreeMap::new(),
            total_chunks: skeleton_len,
            waiting_since: HashMap::new(),
        }
    }

    /// Returns the next chunk index to assign and advances the counter.
    /// Returns `None` if all chunks have been assigned.
    pub fn next_assignment(&mut self) -> Option<usize> {
        if self.next_to_assign >= self.total_chunks {
            return None;
        }
        let idx = self.next_to_assign;
        self.next_to_assign += 1;
        Some(idx)
    }

    /// Records that `chunk_idx` was sent to `peer`.
    pub fn record_sent(&mut self, peer: B512, chunk_idx: usize) {
        self.in_flight
            .entry(peer)
            .or_default()
            .push_back(chunk_idx);
        self.waiting_since.entry(peer).or_insert_with(Instant::now);
    }

    /// Identifies which chunk a response from `peer` corresponds to
    /// by popping the front of that peer's in-flight queue.
    pub fn identify_response(&mut self, peer: &B512) -> Option<usize> {
        let queue = self.in_flight.get_mut(peer)?;
        let idx = queue.pop_front();
        if queue.is_empty() {
            self.waiting_since.remove(peer);
        } else {
            self.waiting_since.insert(*peer, Instant::now());
        }
        idx
    }

    /// Buffers a response for the given chunk index, remembering its source.
    /// Returns false if the slot was already filled, leaving it untouched.
    ///
    /// First answer wins. Overwriting would let a later response displace an
    /// earlier one for the same chunk, which is how one peer's work replaces
    /// another's after both were asked.
    pub fn buffer_response(&mut self, peer: B512, chunk_idx: usize, headers: Vec<Header>) -> bool {
        match self.buffered.entry(chunk_idx) {
            std::collections::btree_map::Entry::Occupied(_) => false,
            std::collections::btree_map::Entry::Vacant(v) => {
                v.insert((peer, headers));
                true
            }
        }
    }

    /// Whether this peer has any chunk request outstanding.
    ///
    /// Header responses are matched to skeleton entries by content rather
    /// than by a request id, so without this a peer that was never asked can
    /// supply a chunk simply by sending headers that end on a skeleton
    /// boundary -- which every peer can compute.
    pub fn has_outstanding(&self, peer: &B512) -> bool {
        self.in_flight.get(peer).is_some_and(|q| !q.is_empty())
    }

    /// Drains all consecutive ready chunks starting from `next_to_process`.
    /// Returns them in order.
    /// Each entry is `(chunk index, supplying peer, headers)`.
    pub fn drain_ready(&mut self) -> Vec<(usize, B512, Vec<Header>)> {
        let mut ready = Vec::new();
        while let Some((peer, headers)) = self.buffered.remove(&self.next_to_process) {
            ready.push((self.next_to_process, peer, headers));
            self.next_to_process += 1;
        }
        ready
    }

    /// Returns true if all chunks have been processed.
    pub fn is_complete(&self) -> bool {
        self.next_to_process >= self.total_chunks
    }

    /// Returns how many requests a peer can still accept before hitting
    /// the pipeline depth limit.
    pub fn peer_capacity(&self, peer: &B512) -> usize {
        let current = self.in_flight.get(peer).map_or(0, |q| q.len());
        PIPELINE_DEPTH.saturating_sub(current)
    }

    /// Peers that have chunks in flight but haven't responded within `timeout`.
    pub fn stalled_peers(&self, timeout: Duration) -> Vec<B512> {
        self.waiting_since
            .iter()
            .filter(|(peer, since)| {
                since.elapsed() > timeout
                    && self.in_flight.get(*peer).is_some_and(|q| !q.is_empty())
            })
            .map(|(peer, _)| *peer)
            .collect()
    }

    /// Reassigns all in-flight chunks from a disconnected peer back to the
    /// assignment pool by resetting `next_to_assign` to the minimum.
    pub fn handle_peer_disconnect(&mut self, peer: &B512) {
        self.waiting_since.remove(peer);
        if let Some(queue) = self.in_flight.remove(peer) {
            for idx in queue {
                if idx >= self.next_to_process && !self.buffered.contains_key(&idx) && idx < self.next_to_assign {
                    self.next_to_assign = idx;
                }
            }
        }
    }
}

#[cfg(test)]
mod peer_binding_tests {
    use super::*;

    /// Header responses are matched to skeleton entries by content, which
    /// says *which* chunk a response is for but nothing about who was
    /// entitled to answer. Every peer can work out where the skeleton
    /// boundaries fall, so without an explicit test of outstanding requests
    /// any connected peer could supply a chunk.
    #[test]
    fn a_peer_with_no_request_outstanding_is_not_owed_a_chunk() {
        let mut t = PeerChunkTracker::new(4);
        let asked = B512::repeat_byte(1);
        let stranger = B512::repeat_byte(2);

        t.in_flight.entry(asked).or_default().push_back(1);

        assert!(t.has_outstanding(&asked));
        assert!(!t.has_outstanding(&stranger), "nothing was asked of this peer");
    }

    /// A peer whose queue has drained is in the same position as one never
    /// asked: its earlier request has already been answered.
    #[test]
    fn a_drained_queue_leaves_nothing_outstanding() {
        let mut t = PeerChunkTracker::new(4);
        let peer = B512::repeat_byte(1);
        t.in_flight.entry(peer).or_default().push_back(1);

        assert_eq!(t.identify_response(&peer), Some(1));
        assert!(!t.has_outstanding(&peer));
    }

    /// First answer wins. Two peers can hold a request for the same chunk --
    /// after a retry -- and the later answer must not displace the stored one.
    #[test]
    fn a_filled_chunk_slot_is_not_overwritten() {
        let mut t = PeerChunkTracker::new(4);
        let first = B512::repeat_byte(1);
        let second = B512::repeat_byte(2);

        assert!(t.buffer_response(first, 1, Vec::new()));
        assert!(!t.buffer_response(second, 1, Vec::new()), "slot already filled");

        let ready = t.drain_ready();
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].1, first, "the first answer is the one kept");
    }
}
