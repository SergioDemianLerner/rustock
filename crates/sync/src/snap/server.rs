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
    ChunkPayload, SnapBlocksRequest, SnapBlocksResponse, SnapChunkRequest, SnapChunkResponse,
    SnapEntry, SnapStatusRequest, SnapStatusResponse,
};
use rustock_core::Block;
use rustock_storage::BlockStore;
use rustock_trie::snapshot::total_size;
use rustock_trie::snapshot_proof::prove_chunk;
use rustock_trie::{TrieNode, TrieStore};
use alloy_primitives::B512;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tracing::{debug, trace, warn};

/// A snap status answer, kept so the hundredth client to ask costs nothing.
struct CachedStatus {
    checkpoint: B256,
    response: SnapStatusResponse,
}

pub struct SnapServer {
    store: Arc<BlockStore>,
    trie: Arc<dyn TrieStore>,
    config: SnapConfig,
    status_cache: Mutex<Option<CachedStatus>>,
    /// Requests currently being served, per peer.
    serving: Mutex<HashMap<B512, usize>>,
}

impl SnapServer {
    pub fn new(store: Arc<BlockStore>, trie: Arc<dyn TrieStore>, config: SnapConfig) -> Self {
        Self {
            store,
            trie,
            config,
            status_cache: Mutex::new(None),
            serving: Mutex::new(HashMap::new()),
        }
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
        let number = self.config.checkpoint_for(head.number);

        let hash = self.store.canonical_hash(number).ok()??;
        let block = self.store.block(hash).ok()??;

        // The state has to be here in full. Holding the root is not proof of
        // that, but it is the cheap half of the question; the rest surfaces
        // per chunk, where a missing node simply ends the traversal early.
        self.trie.get(block.header.state_root.as_slice())?;
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

        let response = SnapStatusResponse { id: request.id, blocks, difficulties, trie_size };
        if let Ok(mut cache) = self.status_cache.lock() {
            *cache = Some(CachedStatus { checkpoint: hash, response: response.clone() });
        }
        Some(response)
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

        let empty = |reason: &str| {
            debug!(
                target: "rustock::snap",
                "declining chunk request from {} at offset {}: {reason}",
                request.block_number, request.from
            );
            Some(SnapChunkResponse {
                id: request.id,
                payload: ChunkPayload::Proved { entries: Vec::new(), witness: Vec::new() },
                block_number: request.block_number,
                from: request.from,
                to: request.from,
                complete: false,
            })
        };

        let Ok(Some(hash)) = self.store.canonical_hash(request.block_number) else {
            return empty("no such block");
        };
        let Ok(Some(header)) = self.store.header(hash) else {
            return empty("no such block");
        };

        // A client that named a root is telling us which state it means. If
        // ours differs, the two are on different chains and there is nothing
        // useful to send -- serving our state under their question would look
        // to them like a peer contradicting itself much later.
        if let Some(wanted) = request.state_root {
            if wanted != header.state_root {
                return empty("state root does not match our chain");
            }
        }

        let Some(root_message) = self.trie.get(header.state_root.as_slice()) else {
            return empty("state not stored");
        };
        let Some(root) = TrieNode::try_from_message(&root_message, self.trie.as_ref()) else {
            return empty("state root unreadable");
        };

        let total = total_size(&root, self.trie.as_ref());
        if request.from >= total {
            return empty("offset past the end of the trie");
        }

        // Zero means "your choice", which is what rskj always sends.
        let budget = if request.chunk_size == 0 {
            self.config.chunk_bytes
        } else {
            request.chunk_size.min(self.config.max_chunk_bytes)
        };

        let proof = prove_chunk(&root, request.from, budget, self.trie.as_ref());
        if proof.entries.is_empty() {
            return empty("no nodes at that offset");
        }

        let entries: Vec<SnapEntry> = proof
            .entries
            .iter()
            .map(|e| SnapEntry {
                message: e.message.clone().into(),
                long_values: e.long_values.iter().map(|v| v.clone().into()).collect(),
            })
            .collect();
        let witness = proof.witness.iter().map(|w| w.clone().into()).collect();

        // `to` is a courtesy for the peer's logs. The client recomputes it
        // from the nodes, and must: see `snapshot_proof`.
        let to = request.from
            + proof.entries.iter().map(|e| e.message.len() as u64).sum::<u64>();

        trace!(
            target: "rustock::snap",
            "serving {} nodes from offset {} ({} witness nodes, {} bytes)",
            proof.entries.len(), request.from, proof.witness.len(), proof.wire_len()
        );

        Some(SnapChunkResponse {
            id: request.id,
            payload: ChunkPayload::Proved { entries, witness },
            block_number: request.block_number,
            from: request.from,
            to,
            complete: to >= total,
        })
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
