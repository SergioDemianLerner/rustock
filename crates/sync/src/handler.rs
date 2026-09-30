use crate::events::SyncEvent;
use crate::manager::SyncManager;
use alloy_primitives::B512;
use crate::snap::server::SnapServer;
use rustock_networking::protocol::{
    BlockHashResponse, BlockHeadersResponse, BlockIdentifier, BodyResponse,
    P2pHandler, P2pMessage, RskMessage, RskSubMessage, SkeletonResponse,
};
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{trace, warn};

/// Skeleton step size (must match rskj's chunkSize = 192).
const SKELETON_STEP: u64 = 192;

/// Maximum skeleton entries per response (matches rskj's maxSkeletonChunks = 20).
const MAX_SKELETON_ENTRIES: usize = 20;

/// Maximum headers to serve in a single response.
const MAX_HEADERS_SERVE: u32 = 192;

/// What a snap answer is expected to weigh, for rate limiting.
///
/// Only state is metered. A status or a block range is small, infrequent, and
/// already bounded by the concurrency limit; metering them would complicate
/// the accounting for nothing.
#[derive(Debug, Clone, Copy)]
enum Cost {
    Small,
    Chunk,
}

/// Dispatches inbound messages to the state machine channel and serves
/// data to peers (headers, block hashes, skeletons).
pub struct SyncHandler {
    manager: Arc<SyncManager>,
    event_tx: mpsc::UnboundedSender<SyncEvent>,
    /// Present only when this node serves snapshots. Absent is the default
    /// and costs nothing: snap requests are then simply not answered, which
    /// is what a node without the feature looks like from outside.
    snap: Option<Arc<SnapServer>>,
    /// Attached after the handler is shared, which is when the node knows
    /// whether it was asked to serve snapshots.
    snap_late: std::sync::RwLock<Option<Arc<SnapServer>>>,
}

impl SyncHandler {
    pub fn new(manager: Arc<SyncManager>, event_tx: mpsc::UnboundedSender<SyncEvent>) -> Self {
        Self { manager, event_tx, snap: None, snap_late: std::sync::RwLock::new(None) }
    }

    /// Serve snapshots of this node's state to peers that ask.
    pub fn with_snap_server(mut self, snap: Arc<SnapServer>) -> Self {
        self.snap = Some(snap);
        self
    }

    /// Same, for a handler already shared behind an `Arc`.
    ///
    /// Installed once at startup, before any peer is connected, so the
    /// lock is uncontended and the read on the message path stays cheap.
    pub fn attach_snap_server(&self, snap: Arc<SnapServer>) {
        if let Ok(mut slot) = self.snap_late.write() {
            *slot = Some(snap);
        }
    }

    fn snap_server(&self) -> Option<Arc<SnapServer>> {
        if let Some(snap) = &self.snap {
            return Some(snap.clone());
        }
        self.snap_late.read().ok()?.clone()
    }

    /// Answers a snap request off the network thread.
    ///
    /// Serving state is disk-bound -- on a cold store a single chunk is most
    /// of a second -- and this is called from inside the peer's async task.
    /// Doing the work here would block not just that peer but every other task
    /// sharing the runtime worker: a node that serves snapshots would stop
    /// following the chain while it did. So the work goes to a blocking thread
    /// and the answer is sent when it is ready, which is also what rskj does
    /// (`scheduleJob`).
    ///
    /// Returns `None` always: the reply does not travel back through the
    /// handler's return value.
    fn serve_snap<F>(&self, peer: B512, cost: Cost, reply: F) -> Option<P2pMessage>
    where
        F: FnOnce(&SnapServer) -> Option<RskSubMessage> + Send + 'static,
    {
        let server = self.snap_server()?;

        // Charged before the slot is taken, so a peer over its rate does not
        // sit on one while it waits.
        let wait = match cost {
            Cost::Small => std::time::Duration::ZERO,
            Cost::Chunk => match server.allowance(peer, server.chunk_cost()) {
                crate::snap::rate::Allowance::Now => std::time::Duration::ZERO,
                crate::snap::rate::Allowance::After(d) => d,
                crate::snap::rate::Allowance::Refuse => {
                    trace!(
                        target: "rustock::snap",
                        "refusing a chunk to {:?}: over its rate", &peer.0[..4]
                    );
                    return None;
                }
            },
        };

        if !server.admit(peer) {
            trace!(
                target: "rustock::snap",
                "refusing a snap request from {:?}: already serving its limit", &peer.0[..4]
            );
            return None;
        }

        let peer_store = self.manager.peer_store.clone();
        tokio::spawn(async move {
            // Backpressure, not an error: a peer over its rate is slowed
            // rather than told no, which needs nothing of the protocol and
            // throttles the peer through the slots it is holding.
            if !wait.is_zero() {
                tokio::time::sleep(wait).await;
            }
            let serving = server.clone();
            let built = tokio::task::spawn_blocking(move || {
                let answer = reply(&serving);
                serving.release(peer);
                answer
            })
            .await;

            match built {
                Ok(Some(sub)) => {
                    peer_store
                        .send_to_peer(&peer, P2pMessage::RskMessage(RskMessage::new(sub)))
                        .await;
                }
                Ok(None) => {}
                Err(e) => {
                    // The slot is released inside the blocking closure, which
                    // did not run to completion; release it here too.
                    server.release(peer);
                    warn!(target: "rustock::snap", "serving a snap request failed: {e}");
                }
            }
        });
        None
    }

    /// Respond to a BodyRequest by looking up the block and returning its body.
    fn serve_body_request(
        &self,
        request_id: u64,
        hash: alloy_primitives::B256,
    ) -> Option<P2pMessage> {
        let store = &self.manager.store;
        let (transactions, uncles) = store.body(hash).ok()??;

        trace!(
            target: "rustock::sync",
            "Serving block body for {:?} ({} txs, {} uncles)",
            hash, transactions.len(), uncles.len()
        );

        let resp = BodyResponse {
            id: request_id,
            transactions,
            uncles,
        };
        Some(P2pMessage::RskMessage(RskMessage::new(
            RskSubMessage::BodyResponse(resp),
        )))
    }

    /// Respond to a BlockHeadersRequest by walking backwards from the given
    /// hash, collecting up to `count` headers (same logic as rskj).
    fn serve_headers_request(
        &self,
        request_id: u64,
        hash: alloy_primitives::B256,
        count: u32,
    ) -> Option<P2pMessage> {
        let count = count.min(MAX_HEADERS_SERVE);
        let store = &self.manager.store;

        let first = store.header(hash).ok()??;
        let headers = canonical_run(store, &first, count)
            .unwrap_or_else(|| walk_by_parent(store, first, count));

        trace!(
            target: "rustock::sync",
            "Serving {} headers (starting from {:?})",
            headers.len(), hash
        );

        let resp = BlockHeadersResponse {
            id: request_id,
            headers,
        };
        Some(P2pMessage::RskMessage(RskMessage::new(
            RskSubMessage::BlockHeadersResponse(resp),
        )))
    }

    /// Respond to a BlockHashRequest by looking up the canonical hash at the
    /// requested height.
    fn serve_block_hash_request(
        &self,
        request_id: u64,
        height: u64,
    ) -> Option<P2pMessage> {
        if height == 0 {
            return None;
        }
        let hash = self.manager.store.canonical_hash(height).ok()??;
        trace!(
            target: "rustock::sync",
            "Serving block hash for height #{}: {:?}",
            height, hash
        );
        let resp = BlockHashResponse {
            id: request_id,
            hash,
        };
        Some(P2pMessage::RskMessage(RskMessage::new(
            RskSubMessage::BlockHashResponse(resp),
        )))
    }

    /// Respond to a SkeletonRequest by constructing evenly-spaced block
    /// identifiers from the requested start (matching rskj's algorithm).
    fn serve_skeleton_request(
        &self,
        request_id: u64,
        start_number: u64,
    ) -> Option<P2pMessage> {
        let store = &self.manager.store;

        // Verify we have the starting block
        store.canonical_hash(start_number).ok()??;

        let best_number = store
            .head()
            .ok()?
            .and_then(|h| store.header(h).ok()?)
            .map(|h| h.number)?;

        let skeleton_start = (start_number / SKELETON_STEP) * SKELETON_STEP;
        let max_skeleton_number = best_number.min(
            skeleton_start + SKELETON_STEP * MAX_SKELETON_ENTRIES as u64,
        );

        let mut identifiers = Vec::new();
        let mut n = skeleton_start;
        while n < max_skeleton_number {
            if let Ok(Some(hash)) = store.canonical_hash(n) {
                identifiers.push(BlockIdentifier { hash, number: n });
            }
            n += SKELETON_STEP;
        }

        // Always include the best block (or the last skeleton point if equal)
        let last_number = best_number.min(n);
        if let Ok(Some(hash)) = store.canonical_hash(last_number) {
            if identifiers.last().is_none_or(|last| last.number != last_number) {
                identifiers.push(BlockIdentifier {
                    hash,
                    number: last_number,
                });
            }
        }

        if identifiers.is_empty() {
            return None;
        }

        trace!(
            target: "rustock::sync",
            "Serving skeleton ({} entries, #{} -> #{})",
            identifiers.len(),
            identifiers.first().map(|b| b.number).unwrap_or(0),
            identifiers.last().map(|b| b.number).unwrap_or(0)
        );

        let resp = SkeletonResponse {
            id: request_id,
            block_identifiers: identifiers,
        };
        Some(P2pMessage::RskMessage(RskMessage::new(
            RskSubMessage::SkeletonResponse(resp),
        )))
    }
}

impl P2pHandler for SyncHandler {
    fn handle_message(&self, id: B512, msg: &P2pMessage) -> Option<P2pMessage> {
        if let P2pMessage::RskMessage(m) = msg {
            match &m.sub_message {
                RskSubMessage::Status(s) => {
                    trace!(
                        target: "rustock::sync",
                        "Received status from peer {:?}: #{} (TD: {:?})",
                        id,
                        s.best_block_number,
                        s.total_difficulty
                    );
                    let claimed = s.total_difficulty.unwrap_or_default();

                    // The cheapest check there is: a peer claiming more work
                    // than the shipped checkpoint allows, for a height at or
                    // below the checkpoint, is refuted by arithmetic. No
                    // requests, no sampling, no round trips.
                    //
                    // Recorded rather than acted on beyond a warning: the
                    // metadata is what peer selection reads, so refusing to
                    // store it is enough to stop this node syncing from the
                    // peer. Disconnecting is the scoring layer's decision.
                    if refuted_by_checkpoint(claimed, s.best_block_number) {
                        warn!(
                            target: "rustock::sync",
                            "Peer {:?} claims total difficulty {} at #{}, which is more than \
                             the checkpoint at #{} permits; not syncing from it",
                            &id.0[..4],
                            claimed,
                            s.best_block_number,
                            rustock_core::checkpoint::MAINNET_CHECKPOINT.number
                        );
                        // Refusing to record the metadata is what stops this
                        // node syncing from the peer: peer selection reads
                        // exactly that. Scoring lives in the service, which
                        // this handler does not reach.
                    } else {

                        let metadata = rustock_networking::peers::PeerMetadata {
                            best_number: s.best_block_number,
                            best_hash: s.best_block_hash,
                            total_difficulty: claimed,
                            client_id: "".to_string(),
                            address: None,
                        };
                        let peer_store = self.manager.peer_store.clone();
                        tokio::spawn(async move {
                            peer_store.update_metadata(&id, metadata).await;
                        });
                    }
                }
                RskSubMessage::BlockHashResponse(r) => {
                    let _ = self.event_tx.send(SyncEvent::BlockHashResponse {
                        peer: id,
                        hash: r.hash,
                    });
                }
                RskSubMessage::SkeletonResponse(r) => {
                    let _ = self.event_tx.send(SyncEvent::SkeletonResponse {
                        peer: id,
                        id: r.id,
                        identifiers: r.block_identifiers.clone(),
                    });
                }
                RskSubMessage::BlockHeadersResponse(r) => {
                    let _ = self.event_tx.send(SyncEvent::HeadersResponse {
                        peer: id,
                        id: r.id,
                        headers: r.headers.clone(),
                    });
                }
                RskSubMessage::NewBlockHashes(blocks) => {
                    let _ = self.event_tx.send(SyncEvent::NewBlockHashes {
                        peer: id,
                        identifiers: blocks.clone(),
                    });
                }

                RskSubMessage::BodyResponse(r) => {
                    trace!(
                        target: "rustock::sync",
                        "Received BodyResponse id={} ({} txs, {} uncles) from {:?}",
                        r.id, r.transactions.len(), r.uncles.len(), &id.0[..4]
                    );
                    let _ = self.event_tx.send(SyncEvent::BodyResponse {
                        peer: id,
                        id: r.id,
                        transactions: r.transactions.clone(),
                        uncles: r.uncles.clone(),
                    });
                }

                // --- Serve data to peers ---
                RskSubMessage::BlockHeadersRequest(r) => {
                    return self.serve_headers_request(r.id, r.query.hash, r.query.count);
                }
                RskSubMessage::BlockHashRequest(r) => {
                    return self.serve_block_hash_request(r.id, r.height);
                }
                RskSubMessage::SkeletonRequest(r) => {
                    return self.serve_skeleton_request(r.id, r.start_number);
                }
                RskSubMessage::BodyRequest(r) => {
                    return self.serve_body_request(r.id, r.hash);
                }

                // --- Snapshot sync ---
                RskSubMessage::SnapStatusRequest(r) => {
                    let r = r.clone();
                    return self.serve_snap(id, Cost::Small, move |s| {
                        s.status(&r).map(|m| RskSubMessage::SnapStatusResponse(Box::new(m)))
                    });
                }
                RskSubMessage::SnapChunkRequest(r) => {
                    let r = r.clone();
                    return self.serve_snap(id, Cost::Chunk, move |s| {
                        s.chunk(&r).map(|m| RskSubMessage::SnapChunkResponse(Box::new(m)))
                    });
                }
                RskSubMessage::SnapBlocksRequest(r) => {
                    let r = r.clone();
                    return self.serve_snap(id, Cost::Small, move |s| {
                        s.blocks(&r).map(|m| RskSubMessage::SnapBlocksResponse(Box::new(m)))
                    });
                }
                RskSubMessage::SnapStatusResponse(r) => {
                    let _ = self.event_tx.send(SyncEvent::SnapStatusResponse {
                        peer: id,
                        id: r.id,
                        blocks: r.blocks.clone(),
                        difficulties: r.difficulties.clone(),
                        trie_size: r.trie_size,
                        chunk_grid: r.chunk_grid,
                    });
                }
                RskSubMessage::SnapChunkResponse(r) => {
                    let _ = self.event_tx.send(SyncEvent::SnapChunkResponse {
                        peer: id,
                        id: r.id,
                        from: r.from,
                        payload: r.payload.clone(),
                        refusal: r.refusal,
                    });
                }
                RskSubMessage::SnapBlocksResponse(r) => {
                    let _ = self.event_tx.send(SyncEvent::SnapBlocksResponse {
                        peer: id,
                        id: r.id,
                        blocks: r.blocks.clone(),
                        difficulties: r.difficulties.clone(),
                    });
                }

                _ => {}
            }
        }
        None
    }
}

/// The old walk: one dependent point lookup per header.
///
/// Still the fallback, and still correct for any chain. It is what
/// [`canonical_run`] declines to do when the range is not plainly canonical.
pub(crate) fn walk_by_parent(
    store: &rustock_storage::BlockStore,
    first: rustock_core::Header,
    count: u32,
) -> Vec<rustock_core::Header> {
    let mut headers = vec![first.clone()];
    let mut current = first;
    for _ in 1..count {
        match store.header(current.parent_hash) {
            Ok(Some(parent)) => {
                current = parent.clone();
                headers.push(parent);
            }
            _ => break,
        }
    }
    headers
}

/// The same run, fetched in parallel, when the range is canonical.
///
/// # Why this exists
///
/// Walking by parent hash is a chain of *dependent* reads: the next hash is
/// not known until the current header has been read, so the device sees one
/// outstanding request at a time. Measured serving a real snapshot-syncing
/// client: 933 KB/s over **loopback** with the server CPU at 12% — disk-bound
/// at queue depth 1. See #185.
///
/// Heights are not dependent on each other. `CF_NUMBERS` turns the run into a
/// set of hashes with one ordered cursor, and those hashes are then fetched
/// concurrently.
///
/// # Why it verifies rather than trusts
///
/// The request names a **hash**, not a height, and the chain asked for may be
/// a sibling rather than the canonical one. Serving canonical headers for a
/// non-canonical request would answer a question nobody asked, with a chain
/// the peer cannot link to what it already has.
///
/// So this returns `None` — and the caller falls back to the honest walk —
/// unless every one of these holds:
///
/// - the starting hash **is** the canonical block at its own height;
/// - every height in the range resolved to a hash;
/// - every header was present;
/// - and each header's `parent_hash` is the next one down, checked link by
///   link. The canonical index can be mid-repair, and a gap in it must not
///   become a gap in what is served.
pub(crate) fn canonical_run(
    store: &rustock_storage::BlockStore,
    first: &rustock_core::Header,
    count: u32,
) -> Option<Vec<rustock_core::Header>> {
    let top = first.number;
    // Genesis is its own floor; a short run near it is not worth the setup.
    if count < 8 || top < count as u64 {
        return None;
    }
    if store.canonical_hash(top).ok()?? != first.hash() {
        return None;
    }

    // One call, so a frozen run is answered from the flat files without
    // resolving a hash per height.
    let fetched = store.canonical_headers_descending(top, count as usize);

    let mut headers = Vec::with_capacity(count as usize);
    for slot in fetched {
        headers.push(slot?);
    }

    // Link check. Cheap next to the reads, and the only thing standing between
    // a stale index and a chain that does not join up.
    for pair in headers.windows(2) {
        if pair[0].parent_hash != pair[1].hash() || pair[0].number != pair[1].number + 1 {
            return None;
        }
    }
    Some(headers)
}


/// Whether a peer's advertised total difficulty is already impossible.
///
/// Only the free case: a claim for a height at or below the checkpoint, where
/// the work is known exactly and nothing may exceed it. Heights above the
/// checkpoint need sampling (`crate::sampler`), which costs requests.
fn refuted_by_checkpoint(claimed: alloy_primitives::U256, height: u64) -> bool {
    crate::sampler::ChainSampler::refuted_by_checkpoint_alone(
        &rustock_core::checkpoint::MAINNET_CHECKPOINT,
        claimed,
        height,
    )
}
