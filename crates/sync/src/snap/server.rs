//! Serving snapshots to other nodes.
//!
//! Everything here is a read of this node's own storage, shaped into a reply.
//! The server holds no session state per peer beyond a small cache, so a peer
//! disappearing mid-download costs nothing and several peers downloading at
//! once do not interact.
//!
//! # What a request cannot make this node do
//!
//! Serving state is the one place a peer chooses how much work this node
//! does, so each choice is bounded:
//!
//! - The chunk size is clamped to [`SnapConfig::max_chunk_bytes`], so a peer
//!   asking for the whole trie in one message gets a chunk.
//! - A chunk request must name a root this node would have offered anyway,
//!   not any root at all, so the server cannot be used as an oracle for
//!   arbitrary historical states.
//! - Snap status is cached per checkpoint, because every client asks for the
//!   same one and computing it walks 400 blocks.

use super::SnapConfig;
use alloy_primitives::{B256, U256};
use rustock_networking::protocol::snap::{
    ChunkPayload, Refusal, SnapBlocksRequest, SnapBlocksResponse, SnapChunkRequest,
    SnapChunkResponse, SnapEntry, SnapStatusRequest, SnapStatusResponse,
};
use rustock_core::Block;
use rustock_storage::BlockStore;
use rustock_trie::snapshot::total_size;
use rustock_trie::snapshot_legacy::{encode_blob as encode_legacy_blob, legacy_chunk};
use rustock_trie::snapshot_legacy::rebuild as rebuild_legacy;
use rustock_trie::snapshot_proof::{prove_cell, verify_chunk};
use rustock_trie::{TrieNode, TrieStore};
use alloy_primitives::B512;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use tracing::{debug, trace, warn};

/// Which encoding a cached cell holds. Part of its key: the same range in two
/// dialects is two different answers.
const FORMAT_PROVED: u8 = 0;
const FORMAT_LEGACY: u8 = 1;

/// A snap status answer, kept so the hundredth client to ask costs nothing.
struct CachedStatus {
    checkpoint: B256,
    state_root: B256,
    response: SnapStatusResponse,
}

pub struct SnapServer {
    store: Arc<BlockStore>,
    trie: Arc<dyn TrieStore>,
    config: SnapConfig,
    status_cache: Mutex<Option<CachedStatus>>,
    /// Requests currently being served, per peer.
    serving: Mutex<HashMap<B512, usize>>,
    /// How fast state may leave, per peer and in total.
    rate: super::rate::RateLimiter,
    /// States this node holds only in part, so cannot serve.
    ///
    /// Not a cache of a slow answer -- there is no cheap way to ask the
    /// question. Proving a state complete means walking all ten million of its
    /// nodes, which is minutes, and a peer asking for a chunk must not pay
    /// that. So completeness is learned where it bites: a cell that does not
    /// verify against its own root means some node under it is gone, and the
    /// whole state is struck off. See #143.
    unservable: Mutex<HashSet<B256>>,
}

impl SnapServer {
    pub fn new(store: Arc<BlockStore>, trie: Arc<dyn TrieStore>, config: SnapConfig) -> Self {
        Self {
            store,
            trie,
            status_cache: Mutex::new(None),
            serving: Mutex::new(HashMap::new()),
            unservable: Mutex::new(HashSet::new()),
            rate: super::rate::RateLimiter::new(
                config.peer_bytes_per_second,
                config.total_bytes_per_second,
                // Longer than this and a peer is better off asking elsewhere
                // than holding one of its three slots open on us.
                std::time::Duration::from_secs(5),
                1024,
            ),
            config,
        }
    }

    /// How long this peer should wait before being served `bytes`, if it may
    /// be served at all.
    ///
    /// Charged before the work is done rather than after, so a peer cannot
    /// spend what it has not got by pipelining. Repeat requests for the same
    /// cell are not treated differently from any other: they cost the peer the
    /// same allowance, and remembering which cells a peer already had would
    /// cost more memory than the behaviour costs bandwidth.
    pub fn allowance(&self, peer: B512, bytes: u64) -> super::rate::Allowance {
        self.rate.allow(peer, bytes)
    }

    /// What a chunk answer is likely to weigh, for charging before it is
    /// built. One cell, near enough: they are within a few percent of the
    /// grid by construction.
    pub fn chunk_cost(&self) -> u64 {
        self.config.chunk_grid.max(1)
    }

    /// Take a slot to serve this peer, or refuse.
    ///
    /// One peer may not occupy the whole server: a snapshot request is the
    /// most expensive thing a stranger can ask this node to do, and a peer
    /// that pipelines them would otherwise be able to keep every worker busy
    /// on its own behalf. rskj bounds the same thing with `maxSenderRequests`.
    pub fn admit(&self, peer: B512) -> bool {
        let Ok(mut serving) = self.serving.lock() else { return false };
        let slot = serving.entry(peer).or_insert(0);
        if *slot >= self.config.max_requests_per_peer {
            return false;
        }
        *slot += 1;
        true
    }

    /// Give the slot back, whatever the outcome.
    pub fn release(&self, peer: B512) {
        if let Ok(mut serving) = self.serving.lock() {
            if let Some(slot) = serving.get_mut(&peer) {
                *slot = slot.saturating_sub(1);
                if *slot == 0 {
                    serving.remove(&peer);
                }
            }
        }
    }

    /// How many requests this peer has in flight here.
    #[cfg(test)]
    pub(crate) fn serving_for(&self, peer: B512) -> usize {
        self.serving.lock().map(|s| s.get(&peer).copied().unwrap_or(0)).unwrap_or(0)
    }

    pub fn config(&self) -> &SnapConfig {
        &self.config
    }

    /// Strike a state off: this node holds it only in part.
    ///
    /// Serving half a state is worse than serving none of it: the client's
    /// replay disagrees with the root hash, it cannot tell an incomplete
    /// server from a lying one, and it scores an honest peer as malicious.
    ///
    /// Reached from the chunk path, which is where the evidence is. The
    /// collector reports the same thing about a pinned root it could not mark
    /// in full, but only to the log: it has no cheaper knowledge than this
    /// does, and one place deciding is better than two.
    pub fn mark_unservable(&self, root: B256) {
        let fresh = self.unservable.lock().map(|mut u| u.insert(root)).unwrap_or(false);
        if !fresh {
            return;
        }
        warn!(
            target: "rustock::snap",
            "state {root:?} is incomplete; this node will not serve snapshots of it"
        );
        // The status answer names a checkpoint, so it has to be recomputed
        // before anyone is told about this one again.
        if let Ok(mut cache) = self.status_cache.lock() {
            if cache.as_ref().is_some_and(|c| c.state_root == root) {
                *cache = None;
            }
        }
    }

    fn is_unservable(&self, root: &B256) -> bool {
        self.unservable.lock().map(|u| u.contains(root)).unwrap_or(false)
    }

    /// What this node can currently offer, for reporting at startup.
    ///
    /// A pruned node holds no state at its checkpoint and so serves nothing.
    /// That is correct behaviour, but silent: peers would ask and get no
    /// reply. Saying so once, where an operator will see it, is worth the
    /// line.
    pub fn offer(&self) -> Option<(u64, B256)> {
        let block = self.checkpoint()?;
        Some((block.header.number, block.header.state_root))
    }

    /// The block this node offers as a snapshot point, and its header.
    ///
    /// `None` when there is no such block or its state is no longer on disk --
    /// a pruned node cannot serve snapshots of states it has discarded, and
    /// should say nothing rather than serve a state it cannot complete.
    fn checkpoint(&self) -> Option<Block> {
        let head_hash = self.store.head().ok()??;
        let head = self.store.header(head_hash).ok()??;
        // The simulated chain end, when there is one: a server claiming the
        // chain stops at N must not offer a state above N.
        let head_number = match self.config.serve_ceiling {
            Some(c) => head.number.min(c),
            None => head.number,
        };
        let number = self.config.checkpoint_for(head_number);

        let hash = self.store.canonical_hash(number).ok()??;
        let block = self.store.block(hash).ok()??;

        // The root has to be here. That is the cheap half of the question and
        // nowhere near the whole of it: `children_size` is hash-committed
        // inside the root's own message, so a root alone will happily report
        // the size of a subtree that is no longer on disk.
        //
        // The other half is learned rather than proved. Walking ten million
        // nodes to answer a status request is not an option, so a state is
        // offered until a cell built from it fails to verify, and struck off
        // from then on.
        self.trie.get(block.header.state_root.as_slice())?;
        if self.is_unservable(&block.header.state_root) {
            return None;
        }
        Some(block)
    }

    /// Blocks `from..=to` with their cumulative difficulties, oldest first.
    ///
    /// Walks back by parent hash rather than by canonical height, so the run
    /// returned is a chain even if the height index is mid-repair.
    fn blocks_back_from(&self, to: u64, from: u64) -> (Vec<Block>, Vec<U256>) {
        let mut blocks = Vec::new();
        let mut difficulties = Vec::new();

        let Ok(Some(mut hash)) = self.store.canonical_hash(to) else {
            return (blocks, difficulties);
        };
        loop {
            let Ok(Some(block)) = self.store.block(hash) else { break };
            let Ok(Some(td)) = self.store.total_difficulty(hash) else { break };
            let number = block.header.number;
            let parent = block.header.parent_hash;

            blocks.push(block);
            difficulties.push(td);

            if number <= from {
                break;
            }
            hash = parent;
        }

        blocks.reverse();
        difficulties.reverse();
        (blocks, difficulties)
    }

    /// "What can you serve?" -- the checkpoint block, the 400 blocks before
    /// it, and how big its state is.
    pub fn status(&self, request: &SnapStatusRequest) -> Option<SnapStatusResponse> {
        if !self.config.server_enabled {
            return None;
        }
        let checkpoint = self.checkpoint()?;
        let hash = checkpoint.header.hash();

        if let Some(cached) = self.status_cache.lock().ok()?.as_ref() {
            if cached.checkpoint == hash {
                trace!(target: "rustock::snap", "serving cached snap status for {hash:?}");
                return Some(SnapStatusResponse { id: request.id, ..cached.response.clone() });
            }
        }

        let number = checkpoint.header.number;
        let (blocks, difficulties) =
            self.blocks_back_from(number, number.saturating_sub(self.config.block_chunk_size));
        if blocks.is_empty() {
            warn!(target: "rustock::snap", "no blocks around checkpoint #{number}");
            return None;
        }

        let root_message = self.trie.get(checkpoint.header.state_root.as_slice())?;
        let root = TrieNode::try_from_message(&root_message, self.trie.as_ref())?;
        let trie_size = total_size(&root, self.trie.as_ref());

        debug!(
            target: "rustock::snap",
            "serving snap status: checkpoint #{number} {hash:?}, {} blocks, trie {trie_size} bytes",
            blocks.len()
        );

        let response = SnapStatusResponse {
            id: request.id,
            blocks,
            difficulties,
            trie_size,
            chunk_grid: self.config.chunk_grid,
        };
        if let Ok(mut cache) = self.status_cache.lock() {
            let rolled = cache.as_ref().is_some_and(|c| c.checkpoint != hash);
            *cache = Some(CachedStatus {
                checkpoint: hash,
                state_root: checkpoint.header.state_root,
                response: response.clone(),
            });
            if rolled {
                // The checkpoint moved. Keep this generation and the one
                // before it; drop anything older.
                self.drop_stale_cells(&self.servable_roots());
            }
        }
        Some(response)
    }

    /// The states this server will still answer for: the checkpoint it now
    /// offers, and the one before it.
    ///
    /// The previous generation is kept because `chunk` does not require a
    /// request to name the current checkpoint -- it serves any canonical block
    /// whose state resolves -- so a client that took its status just before a
    /// roll goes on asking for the older block and goes on being answered.
    /// Dropping that state's cells would put it on the cold path for the rest
    /// of its download, and letting the state itself be collected would end the
    /// download outright. See #144.
    ///
    /// Oldest last, so callers that want just the current one take the first.
    pub fn servable_roots(&self) -> Vec<B256> {
        servable_roots(&self.store, &self.config)
    }

    /// Forget the cached cells of every state this server no longer answers
    /// for.
    ///
    /// Cells are a cache, so losing them costs only recomputation; keeping
    /// them costs about a gigabyte per generation, and the checkpoint moves
    /// every 5000 blocks.
    pub(crate) fn drop_stale_cells(&self, keep: &[B256]) {
        let Ok(roots) = self.store.cached_snap_roots() else { return };
        for root in roots {
            if keep.contains(&root) {
                continue;
            }
            match self.store.clear_snap_chunks(root) {
                Ok(()) => debug!(
                    target: "rustock::snap",
                    "dropped cached snapshot cells for the retired state {root:?}"
                ),
                Err(e) => warn!(
                    target: "rustock::snap",
                    "could not drop cached cells for {root:?}: {e}"
                ),
            }
        }
    }

    /// A chunk of the checkpoint state, with the proof that anchors it.
    ///
    /// An answer with no entries means "I cannot serve this": wrong root,
    /// unknown block, or past the end of the trie. Saying so costs one small
    /// message and saves the client a timeout.
    pub fn chunk(&self, request: &SnapChunkRequest) -> Option<SnapChunkResponse> {
        if !self.config.server_enabled {
            return None;
        }

        let refuse = |reason: Refusal| {
            debug!(
                target: "rustock::snap",
                "declining a chunk of #{} at offset {}: {reason:?}",
                request.block_number, request.from
            );
            Some(SnapChunkResponse {
                id: request.id,
                payload: ChunkPayload::Proved { entries: Vec::new(), witness: Vec::new() },
                block_number: request.block_number,
                from: request.from,
                to: request.from,
                complete: false,
                refusal: reason,
            })
        };

        // Which dialect is asking. An rskj client never names a state root --
        // its request has three elements where ours has four -- and it cannot
        // read our chunk format, so it gets rskj's, on rskj's grid, which is a
        // constant on their side and therefore not ours to choose.
        let legacy = request.state_root.is_none();
        let grid = if legacy { super::RSKJ_CHUNK_GRID } else { self.config.chunk_grid.max(1) };

        // A request off the grid cannot be cached and cannot be compared with
        // anyone else's, so it is refused by name. The client's answer to this
        // is to realign, not to look for another peer.
        if !request.from.is_multiple_of(grid) {
            return refuse(Refusal::OffsetNotOnGrid);
        }

        let Ok(Some(hash)) = self.store.canonical_hash(request.block_number) else {
            return refuse(Refusal::UnknownBlock);
        };
        let Ok(Some(header)) = self.store.header(hash) else {
            return refuse(Refusal::UnknownBlock);
        };

        // A client that named a root is telling us which state it means. If
        // ours differs, the two are on different chains and there is nothing
        // useful to send -- serving our state under their question would look
        // to them like a peer contradicting itself much later.
        if let Some(wanted) = request.state_root {
            if wanted != header.state_root {
                return refuse(Refusal::StateRootMismatch);
            }
        }

        if self.is_unservable(&header.state_root) {
            return refuse(Refusal::StateNotStored);
        }
        let Some(root_message) = self.trie.get(header.state_root.as_slice()) else {
            return refuse(Refusal::StateNotStored);
        };
        let Some(root) = TrieNode::try_from_message(&root_message, self.trie.as_ref()) else {
            return refuse(Refusal::StateNotStored);
        };

        let total = total_size(&root, self.trie.as_ref());
        if request.from >= total {
            return refuse(Refusal::PastTheEnd);
        }

        // The cell this offset names. `chunk_size` is now advisory: the grid
        // decides how much a cell holds, because a cell that varied with the
        // asker could not be shared between askers.
        let index = request.from / grid;
        let from = index * grid;
        let to = from + grid;

        if legacy {
            return self.serve_legacy(request, &header.state_root, index, from, to, total);
        }

        // Already computed, for this state, by whoever asked first.
        if let Ok(Some(cached)) = self.store.snap_chunk(header.state_root, grid, FORMAT_PROVED, index) {
            if let Ok(payload) = ChunkPayload::decode_bytes(&cached) {
                trace!(
                    target: "rustock::snap",
                    "serving cell {index} of {:?} from cache", header.state_root
                );
                return Some(SnapChunkResponse {
                    id: request.id,
                    payload,
                    block_number: request.block_number,
                    from,
                    to: to.min(total),
                    complete: to >= total,
                    refusal: Refusal::None,
                });
            }
        }

        let proof = prove_cell(&root, from, to, self.trie.as_ref());
        if proof.entries.is_empty() {
            // Not past the end -- that was settled above, against a size the
            // root's own message commits to. An empty cell below the end means
            // the traversal could not resolve a node, which is a hole in the
            // state, not a question about the offset.
            self.mark_unservable(header.state_root);
            return refuse(Refusal::StateNotStored);
        }

        // Replay our own chunk before anyone else does.
        //
        // The traversal skips a node it cannot resolve and carries on without
        // advancing the offset, so a collected subtree turns into a chunk that
        // looks well formed and is not. The client would replay it, find it
        // disagrees with the root hash, and score this node as malicious --
        // for telling the truth about a state it no longer has in full.
        //
        // The verifier is already here, and this runs once per cell, on bytes
        // in hand, behind a build that just did thousands of disk reads.
        if let Err(e) = verify_chunk(header.state_root, from, &proof) {
            debug!(
                target: "rustock::snap",
                "cell {index} of {:?} does not verify against its own root: {e:?}",
                header.state_root
            );
            self.mark_unservable(header.state_root);
            return refuse(Refusal::StateNotStored);
        }

        let entries: Vec<SnapEntry> = proof
            .entries
            .iter()
            .map(|e| SnapEntry {
                message: e.message.clone().into(),
                long_values: e.long_values.iter().map(|v| v.clone().into()).collect(),
            })
            .collect();
        let witness: Vec<_> = proof.witness.iter().map(|w| w.clone().into()).collect();
        let payload = ChunkPayload::Proved { entries, witness };

        // Kept for the next client to ask. A failure here costs only the
        // recomputation, so it is logged and otherwise ignored.
        if let Err(e) = self.store.put_snap_chunk(
            header.state_root,
            grid,
            FORMAT_PROVED,
            index,
            &payload.encode_bytes(),
        ) {
            debug!(target: "rustock::snap", "could not cache cell {index}: {e}");
        }

        trace!(
            target: "rustock::snap",
            "serving cell {index} ({from}..{to}): {} nodes, {} witness nodes, {} bytes",
            proof.entries.len(), proof.witness.len(), proof.wire_len()
        );

        Some(SnapChunkResponse {
            id: request.id,
            payload,
            block_number: request.block_number,
            from,
            to: to.min(total),
            complete: to >= total,
            refusal: Refusal::None,
        })
    }

    /// Serve a cell in rskj's format, to an rskj client.
    ///
    /// Cached separately from the proved form: the same range, two encodings,
    /// and a cache that confused them would serve one client the other's
    /// bytes. The top bit of the index distinguishes them, which cells
    /// themselves never reach.
    fn serve_legacy(
        &self,
        request: &SnapChunkRequest,
        state_root: &B256,
        index: u64,
        from: u64,
        to: u64,
        total: u64,
    ) -> Option<SnapChunkResponse> {
        let answer = |blob: Vec<u8>| {
            Some(SnapChunkResponse {
                id: request.id,
                payload: ChunkPayload::Legacy(blob.into()),
                block_number: request.block_number,
                from,
                to: to.min(total),
                complete: to >= total,
                refusal: Refusal::None,
            })
        };

        let grid = super::RSKJ_CHUNK_GRID;
        if let Ok(Some(cached)) = self.store.snap_chunk(*state_root, grid, FORMAT_LEGACY, index) {
            trace!(target: "rustock::snap", "serving rskj-format cell {index} from cache");
            return answer(cached);
        }

        let root: [u8; 32] = (*state_root).into();
        let Some(chunk) = legacy_chunk(&root, from, to, self.trie.as_ref()) else {
            debug!(
                target: "rustock::snap",
                "cannot build an rskj-format cell at {from}: a long value is missing"
            );
            return Some(SnapChunkResponse {
                id: request.id,
                payload: ChunkPayload::Legacy(Vec::new().into()),
                block_number: request.block_number,
                from,
                to: from,
                complete: false,
                refusal: Refusal::StateNotStored,
            });
        };

        // Same check as the proved path, by the same argument; `rebuild` is
        // what an rskj client will run on these bytes.
        if let Err(e) = rebuild_legacy(&chunk, *state_root) {
            debug!(
                target: "rustock::snap",
                "rskj-format cell {index} of {state_root:?} does not rebuild: {e:?}"
            );
            self.mark_unservable(*state_root);
            return Some(SnapChunkResponse {
                id: request.id,
                payload: ChunkPayload::Legacy(Vec::new().into()),
                block_number: request.block_number,
                from,
                to: from,
                complete: false,
                refusal: Refusal::StateNotStored,
            });
        }

        let blob = encode_legacy_blob(&chunk);
        if let Err(e) =
            self.store.put_snap_chunk(*state_root, grid, FORMAT_LEGACY, index, &blob)
        {
            debug!(target: "rustock::snap", "could not cache rskj-format cell {index}: {e}");
        }
        trace!(
            target: "rustock::snap",
            "serving rskj-format cell {index} ({from}..{to}): {} nodes, {} bytes",
            chunk.nodes.len(), blob.len()
        );
        answer(blob)
    }

    /// The 400 blocks before the one asked for.
    pub fn blocks(&self, request: &SnapBlocksRequest) -> Option<SnapBlocksResponse> {
        if !self.config.server_enabled {
            return None;
        }
        if request.block_number == 0 {
            return None;
        }
        let to = request.block_number.saturating_sub(1);
        let from = to.saturating_sub(self.config.block_chunk_size.saturating_sub(1)).max(1);

        let (blocks, difficulties) = self.blocks_back_from(to, from);
        if blocks.is_empty() {
            return None;
        }
        trace!(
            target: "rustock::snap",
            "serving {} blocks below #{}", blocks.len(), request.block_number
        );
        Some(SnapBlocksResponse { id: request.id, blocks, difficulties })
    }
}

/// The states a snapshot server would answer for: the checkpoint it offers and
/// the one before it.
///
/// A free function because the collector needs the same list and runs before
/// the server exists. Empty when snapshot serving is off -- there is then no
/// checkpoint to protect and no reason for the collector to retain the delta.
///
/// Oldest last, so callers that want just the current one take the first.
pub fn servable_roots(store: &BlockStore, config: &SnapConfig) -> Vec<B256> {
    if !config.server_enabled {
        return Vec::new();
    }
    let Ok(Some(head_hash)) = store.head() else { return Vec::new() };
    let Ok(Some(head)) = store.header(head_hash) else { return Vec::new() };

    let current = config.checkpoint_for(head.number);
    let previous = current.saturating_sub(config.checkpoint_rounding.max(1));

    let mut roots = Vec::new();
    for number in [current, previous] {
        if number == 0 {
            continue;
        }
        let Ok(Some(hash)) = store.canonical_hash(number) else { continue };
        let Ok(Some(header)) = store.header(hash) else { continue };
        if !roots.contains(&header.state_root) {
            roots.push(header.state_root);
        }
    }
    roots
}
