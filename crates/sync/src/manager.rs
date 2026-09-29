use rustock_core::validation::HeaderVerifier;
use rustock_core::types::header::Header;
use rustock_storage::BlockStore;
use rustock_networking::protocol::{
    BlockHeadersQuery, BlockHeadersRequest, P2pMessage, RskMessage, RskSubMessage,
};
use alloy_primitives::{B256, U256};
use anyhow::Result;
// HashMap no longer needed — sequential TD propagation replaces hash-based parent lookup
use std::sync::Arc;
use tracing::{debug, trace, warn};

/// Maximum skeleton chunks to process per round (rskj default: 20).
pub(crate) const MAX_SKELETON_CHUNKS: usize = 20;

/// Validates and stores headers.
pub struct SyncManager {
    pub store: Arc<BlockStore>,
    verifier: Arc<HeaderVerifier>,
    pub peer_store: Arc<rustock_networking::peers::PeerStore>,
}

impl SyncManager {
    pub fn new(
        store: Arc<BlockStore>,
        verifier: Arc<HeaderVerifier>,
        peer_store: Arc<rustock_networking::peers::PeerStore>,
    ) -> Self {
        Self {
            store,
            verifier,
            peer_store,
        }
    }

    /// Handles a batch of headers received from a peer.
    /// RSK peers return headers in descending order (from requested hash toward genesis).
    /// We reverse them, validate, and store in a single atomic RocksDB WriteBatch.
    pub fn handle_headers_response(&self, mut headers: Vec<Header>) -> Result<()> {
        if headers.is_empty() {
            trace!(target: "rustock::sync", "Received empty headers response");
            return Ok(());
        }

        // RSK returns headers in descending order; reverse for ascending processing
        if headers.len() > 1 && headers[0].number > headers[headers.len() - 1].number {
            headers.reverse();
        }

        let first_num = headers.first().map(|h| h.number).unwrap_or(0);
        let last_num = headers.last().map(|h| h.number).unwrap_or(0);
        trace!(
            target: "rustock::sync",
            "Processing {} headers (#{} -> #{})",
            headers.len(),
            first_num,
            last_num
        );

        // Read current head TD once (instead of per-header)
        let current_head_hash = self.store.head()?;
        let current_td = match current_head_hash {
            Some(h) => self.store.total_difficulty(h)?.unwrap_or_default(),
            None => U256::ZERO,
        };

        // Propagate total difficulty positionally through the chunk, but never
        // *identify the parent* positionally. Headers in a chunk are normally a
        // chain, so the previous entry is usually the parent -- but "usually" is
        // not a rule the consensus check may rely on. A chunk can carry a fork
        // header at a height it already covered, or arrive out of order, and
        // then the previous entry is a different block at the right height, or
        // the right block at the wrong height.
        //
        // Verifying a header against anything other than the block its own
        // `parent_hash` names produces a wrong expected difficulty and rejects
        // valid chain data. That was observed on mainnet: #9,236,893 was checked
        // against #9,236,891, whose timestamp is 34s earlier rather than 7s, so
        // the adjustment sign flipped and the expected difficulty came out 0.75%
        // low. A rejected header writes no canonical entry, and the resulting
        // hole in the index later wedged execution entirely.
        let mut prev_in_chunk: Option<(&Header, U256)> = None;
        let mut validated: Vec<(&Header, U256)> = Vec::with_capacity(headers.len());
        let mut skipped = 0u64;
        // Headers refused because nothing was available to verify them against.
        let mut no_parent = 0u64;
        // Deduplicate by hash, not by block number. Two different blocks may
        // legitimately share a height -- that is what a fork is -- and dropping
        // the second means dropping whichever of them arrived last, which may be
        // the canonical one.
        let mut seen: std::collections::HashSet<B256> = std::collections::HashSet::new();

        for header in &headers {
            let hash = header.hash();
            if !seen.insert(hash) {
                continue;
            }

            let already_stored = self.store.header(hash)?.is_some();

            // The parent is the block named by `parent_hash`, and nothing else.
            // Take it from the chunk when the previous entry *is* that block,
            // otherwise from the store.
            #[allow(unused_assignments)]
            let mut parent_from_store: Option<Header> = None;
            let parent_ref: Option<&Header>;
            let parent_td: U256;

            match prev_in_chunk {
                Some((prev_hdr, prev_td)) if prev_hdr.hash() == header.parent_hash => {
                    parent_ref = Some(prev_hdr);
                    parent_td = prev_td;
                }
                _ => {
                    parent_from_store = self.store.header(header.parent_hash)?;
                    if parent_from_store.is_some() {
                        parent_ref = parent_from_store.as_ref();
                        parent_td = self
                            .store
                            .total_difficulty(header.parent_hash)?
                            .unwrap_or_default();
                    } else {
                        // The invariant: a header is stored only once it has
                        // been verified, and verification needs the parent.
                        //
                        // Nothing is lost by refusing. Header data is the most
                        // replaceable thing in the system, and skeleton chunks
                        // are drained in order
                        // (`PeerChunkTracker::drain_ready`), so an honest
                        // peer's headers arrive with their parent already
                        // stored. A follow-mode announcement that runs ahead of
                        // its parent is simply re-requested.
                        no_parent += 1;
                        debug!(
                            target: "rustock::sync",
                            "Header #{} ({:?}) refused: parent {:?} not held, so \
                             nothing verifies it",
                            header.number, hash, header.parent_hash
                        );
                        continue;
                    }
                }
            }

            let new_td = parent_td + header.difficulty;

            // Every header reaching this point has a parent, so verification
            // always runs -- including the static rules, which is where proof
            // of work lives. Already-stored headers skip it because they
            // passed it when first stored, which is now true without
            // exception.
            if !already_stored {
                if let Some(p) = parent_ref {
                    if let Err(e) = self.verifier.verify(header, Some(p)) {
                        warn!(
                            target: "rustock::sync",
                            "Header #{} ({:?}) failed verification: {:?}",
                            header.number, hash, e
                        );
                        skipped += 1;
                        // Still propagate TD to the next header in the chunk
                        prev_in_chunk = Some((header, new_td));
                        continue;
                    }
                }
            }

            prev_in_chunk = Some((header, new_td));
            validated.push((header, new_td));
        }

        let stored = validated.len() as u64;

        // Commit all validated headers in a single atomic batch
        let _new_head = self.store.store_headers_batch(&validated, current_head_hash, current_td)?;

        if skipped > 0 || no_parent > 0 {
            // Both are refusals, and both leave a hole at that height. A hole
            // in the canonical index halts execution when it is reached, so
            // neither is routine.
            //
            // `no_parent` in particular is what an honest peer does not
            // produce during skeleton sync: chunks are drained in order, so a
            // parent is always already stored. It is expected only in follow
            // mode, where an announcement can run ahead of its parent and the
            // caller ignores the error.
            warn!(
                target: "rustock::sync",
                "Stored {} headers (#{} -> #{}), refused {} invalid and {} with \
                 no parent to verify against",
                stored, first_num, last_num, skipped, no_parent
            );
            return Err(anyhow::anyhow!(
                "{skipped} invalid header(s) and {no_parent} with no verifiable parent"
            ));
        }

        trace!(
            target: "rustock::sync",
            "Stored {} headers (#{} -> #{})",
            stored, first_num, last_num
        );
        Ok(())
    }

    /// Helper to create a headers request message.
    pub fn create_headers_request(&self, start_hash: B256, count: u32) -> P2pMessage {
        let req = BlockHeadersRequest {
            id: rand::random::<u64>() & 0x7FFFFFFFFFFFFFFF,
            query: BlockHeadersQuery {
                hash: start_hash,
                count,
            },
        };
        P2pMessage::RskMessage(RskMessage::new(RskSubMessage::BlockHeadersRequest(req)))
    }
}
