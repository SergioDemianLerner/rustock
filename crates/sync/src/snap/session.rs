//! One snapshot sync, start to finish.
//!
//! The session is a state machine with no I/O and no clock: it is asked what
//! to do next, told what arrived, and says when it is finished. Everything
//! about peers -- choosing them, timing them out, scoring them -- belongs to
//! the caller. That split is deliberate: the part that decides whether to
//! trust a state should be readable in one sitting and testable without a
//! network.
//!
//! # The order of things, and why
//!
//! 1. **Ask for a status.** A peer names a checkpoint block and the 400
//!    blocks before it. None of it is believed yet.
//! 2. **Verify the headers back to something known.** Each header is checked
//!    under the consensus rules, including merged-mining proof of work, and
//!    linked to its child by hash. The walk ends at a block this node already
//!    has -- for a fresh node, genesis. Until this finishes, the checkpoint
//!    is a stranger's claim; after it, forging one costs what it costs to mine
//!    the chain.
//! 3. **Download the state** under that header's `state_root`, every chunk
//!    proved on arrival.
//! 4. **Download the bodies** of the blocks behind the checkpoint, because
//!    contracts can read recent block data and the state alone would not be
//!    enough to execute what comes next.
//!
//! Step 2 before step 3 is the whole trust argument. A client that downloads
//! first and verifies the header chain afterwards has spent an hour on
//! whatever a stranger sent it; one that verifies first spends the hour only
//! on a chain with real work behind it.

use super::client::{ChunkError, StateDownload};
use super::SnapConfig;
use alloy_primitives::{B256, U256};
use rustock_core::validation::HeaderVerifier;
use rustock_core::{Block, Header};
use rustock_networking::protocol::snap::ChunkPayload;
use rustock_storage::BlockStore;
use rustock_trie::TrieStore;
use std::sync::Arc;
use tracing::{debug, info, warn};

/// Headers to ask for in one request while walking back from the checkpoint.
const HEADER_WALK_CHUNK: u32 = 192;

/// How many peers may answer the header walk with nothing before the session
/// concludes the chain does not connect to ours.
const EMPTY_HEADER_REPLIES_ALLOWED: u32 = 5;

/// Chunk answers in a row that add nothing before the session gives up.
///
/// A peer declining to serve is ordinary -- it may be pruned, or on another
/// chain -- and the range simply goes to someone else. But if *every* answer
/// declines, re-asking is a hot loop that never ends, so the session stops
/// and lets the caller try a different set of peers.
const FRUITLESS_CHUNKS_ALLOWED: u32 = 64;

/// What the session wants done. The caller turns these into messages.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Ask a peer which state it can serve.
    RequestStatus,
    /// Ask for headers below `from`, walking toward genesis.
    RequestHeaders { from: B256, count: u32 },
    /// Ask for a range of the state.
    RequestChunk { block_number: u64, state_root: B256, from: u64, budget: u64 },
    /// Ask for the 400 blocks below `block_number`.
    RequestBlocks { block_number: u64 },
}

/// Why a session gave up. Every one of these is a reason to stop listening to
/// the peer that caused it, which is the caller's decision to make.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SnapFailure {
    #[error("the peer offered no checkpoint block")]
    NoCheckpoint,
    #[error("the offered blocks do not form a chain")]
    BrokenChain,
    #[error("header #{number} failed validation: {reason}")]
    InvalidHeader { number: u64, reason: String },
    #[error("cumulative difficulty does not increase across the offered blocks")]
    BadDifficulty,
    #[error("the header walk reached genesis without meeting a block we have")]
    NoCommonAncestor,
    #[error("a chunk was refused: {0}")]
    BadChunk(#[from] ChunkError),
    #[error("the downloaded state is not whole: {0}")]
    IncompleteState(String),
    #[error("{0} chunk requests in a row came back with nothing usable")]
    NoProgress(u32),
    #[error("block #{number}: the {what} are not the ones its header commits to")]
    BadBody { number: u64, what: &'static str },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// Waiting to hear what any peer can serve.
    AwaitingStatus,
    /// Walking the header chain back toward a block we already trust.
    VerifyingHeaders,
    /// Downloading the state under the checkpoint's root.
    DownloadingState,
    /// Filling in the block bodies behind the checkpoint.
    DownloadingBlocks,
    /// The state is whole and the blocks are in.
    Done,
    /// Something failed; the caller should start over with another peer.
    Failed,
}

pub struct SnapSession {
    config: SnapConfig,
    store: Arc<BlockStore>,
    trie: Arc<dyn TrieStore>,
    verifier: Arc<HeaderVerifier>,

    phase: Phase,
    failure: Option<SnapFailure>,

    /// The block whose state is being downloaded, once one has been offered.
    checkpoint: Option<Header>,
    /// Its cumulative difficulty, as offered and then vouched for by the
    /// header walk.
    checkpoint_td: U256,
    /// The blocks that came with the status, held until the header walk
    /// vouches for the chain they sit on.
    offered: Vec<(Block, U256)>,
    /// The oldest header verified so far while walking back.
    walk_tip: Option<Header>,
    /// Outstanding header request, so a late duplicate is ignored.
    headers_in_flight: bool,
    /// Consecutive peers that answered the header walk with nothing.
    empty_header_replies: u32,

    download: Option<StateDownload>,
    /// Chunk answers in a row that added nothing.
    fruitless_chunks: u32,
    /// The oldest block number whose body is still wanted.
    blocks_wanted_to: u64,
    /// The next `SnapBlocks` request to make, walking backwards.
    blocks_cursor: u64,
    blocks_in_flight: bool,
    /// Block answers in a row that added nothing.
    fruitless_blocks: u32,
}

impl SnapSession {
    pub fn new(
        config: SnapConfig,
        store: Arc<BlockStore>,
        trie: Arc<dyn TrieStore>,
        verifier: Arc<HeaderVerifier>,
    ) -> Self {
        Self {
            config,
            store,
            trie,
            verifier,
            phase: Phase::AwaitingStatus,
            failure: None,
            checkpoint: None,
            checkpoint_td: U256::ZERO,
            offered: Vec::new(),
            walk_tip: None,
            headers_in_flight: false,
            empty_header_replies: 0,
            download: None,
            fruitless_chunks: 0,
            blocks_wanted_to: 0,
            blocks_cursor: 0,
            blocks_in_flight: false,
            fruitless_blocks: 0,
        }
    }

    pub fn phase(&self) -> Phase {
        self.phase
    }

    pub fn failure(&self) -> Option<&SnapFailure> {
        self.failure.as_ref()
    }

    pub fn checkpoint(&self) -> Option<&Header> {
        self.checkpoint.as_ref()
    }

    /// Bytes of state covered, and the total once it is known.
    pub fn state_progress(&self) -> (u64, Option<u64>) {
        self.download.as_ref().map_or((0, None), |d| d.progress())
    }

    /// The oldest block whose body is worth asking for.
    ///
    /// Never below one: block zero is genesis, which any node that can
    /// validate a chain already has.
    fn blocks_floor(&self) -> u64 {
        self.blocks_wanted_to.max(1)
    }

    fn fail(&mut self, why: SnapFailure) -> Vec<Action> {
        warn!(target: "rustock::snap", "snapshot sync failed: {why}");
        self.failure = Some(why);
        self.phase = Phase::Failed;
        Vec::new()
    }

    /// What to do now. Safe to call as often as the caller likes: it returns
    /// only work that is not already outstanding.
    pub fn poll(&mut self) -> Vec<Action> {
        match self.phase {
            Phase::AwaitingStatus => vec![Action::RequestStatus],

            Phase::VerifyingHeaders => {
                if self.headers_in_flight {
                    return Vec::new();
                }
                let Some(tip) = self.walk_tip.clone() else { return Vec::new() };
                self.headers_in_flight = true;
                vec![Action::RequestHeaders { from: tip.parent_hash, count: HEADER_WALK_CHUNK }]
            }

            Phase::DownloadingState => {
                let Some(checkpoint) = self.checkpoint.clone() else { return Vec::new() };
                let Some(download) = self.download.as_mut() else { return Vec::new() };

                let mut actions = Vec::new();
                while let Some(request) = download.next_request() {
                    actions.push(Action::RequestChunk {
                        block_number: checkpoint.number,
                        state_root: checkpoint.state_root,
                        from: request.from,
                        budget: request.budget,
                    });
                }
                actions
            }

            Phase::DownloadingBlocks => {
                if self.blocks_in_flight || self.blocks_cursor <= self.blocks_floor() {
                    return Vec::new();
                }
                self.blocks_in_flight = true;
                vec![Action::RequestBlocks { block_number: self.blocks_cursor }]
            }

            Phase::Done | Phase::Failed => Vec::new(),
        }
    }

    /// A peer's offer of a state it can serve.
    ///
    /// The blocks are checked for shape only -- that they form a chain, that
    /// difficulty rises along it. Whether this chain is *the* chain is settled
    /// by the header walk that follows, not here.
    pub fn on_status(
        &mut self,
        blocks: &[Block],
        difficulties: &[U256],
        trie_size: u64,
    ) -> Vec<Action> {
        if self.phase != Phase::AwaitingStatus {
            return Vec::new();
        }
        let Some(checkpoint) = blocks.last() else {
            return self.fail(SnapFailure::NoCheckpoint);
        };
        if difficulties.len() != blocks.len() {
            return self.fail(SnapFailure::BadDifficulty);
        }
        if let Err(why) = check_chain_shape(blocks, difficulties) {
            return self.fail(why);
        }

        let header = checkpoint.header.clone();
        info!(
            target: "rustock::snap",
            "peer offers state at #{} ({:?}), {} bytes; verifying its header chain first",
            header.number, header.hash(), trie_size
        );

        // The state download is set up now but does not start until the
        // header chain is verified: see the module header.
        self.download = Some(StateDownload::new(
            header.state_root,
            trie_size,
            &self.config,
            self.trie.clone(),
        ));
        self.blocks_wanted_to = header.number.saturating_sub(self.config.blocks_required);
        self.blocks_cursor = header.number;

        self.checkpoint_td = *difficulties.last().unwrap_or(&U256::ZERO);
        self.offered = blocks.iter().cloned().zip(difficulties.iter().copied()).collect();

        self.walk_tip = Some(header.clone());
        self.checkpoint = Some(header);
        self.phase = Phase::VerifyingHeaders;
        self.poll()
    }

    /// Headers walking back from the checkpoint, newest first, as rskj's
    /// `BlockHeadersResponse` delivers them.
    pub fn on_headers(&mut self, headers: &[Header]) -> Vec<Action> {
        if self.phase != Phase::VerifyingHeaders {
            return Vec::new();
        }
        self.headers_in_flight = false;

        let Some(mut child) = self.walk_tip.clone() else {
            return self.fail(SnapFailure::NoCheckpoint);
        };

        for header in headers {
            // Linked by hash to the header we already verified, so a peer
            // cannot splice a different chain onto the walk.
            if header.hash() != child.parent_hash {
                return self.fail(SnapFailure::BrokenChain);
            }
            // Proof of work and the rest of the static rules. The parent is
            // not in hand yet -- it is the next header in this very walk --
            // so parent-dependent rules are checked from the other side, when
            // this header validates its own child.
            if let Err(e) = self.verifier.verify(header, None) {
                return self.fail(SnapFailure::InvalidHeader {
                    number: header.number,
                    reason: e.to_string(),
                });
            }
            if let Err(e) = self.verifier.verify(&child, Some(header)) {
                return self.fail(SnapFailure::InvalidHeader {
                    number: child.number,
                    reason: e.to_string(),
                });
            }

            // Written by hash, not indexed: a header nobody points at is
            // inert, so storing it now costs nothing if the walk later turns
            // out to be a foreign chain. The canonical index -- the thing
            // that makes a header part of *this* node's chain -- is written
            // only once the walk reaches ground this node already trusts.
            let _ = self.store.put_header_with_hash(header.hash(), header);

            child = header.clone();
            self.walk_tip = Some(header.clone());

            // Met the chain we already have: everything above this is now
            // anchored to work this node had already accepted.
            if self.is_ours(header.number.saturating_sub(1), header.parent_hash) {
                return self.start_state_download();
            }

            // Reaching block zero is only an answer if it is *our* genesis.
            // A chain of well-formed headers back to a genesis this node has
            // never seen is a different network, or an invented one, and the
            // work behind it says nothing about ours.
            if header.number == 0 {
                return if self.is_ours(0, header.hash()) {
                    self.start_state_download()
                } else {
                    self.fail(SnapFailure::NoCommonAncestor)
                };
            }
        }

        if headers.is_empty() {
            // That peer cannot continue the walk; another might. Give up only
            // once several in a row have come back empty.
            self.empty_header_replies += 1;
            if self.empty_header_replies >= EMPTY_HEADER_REPLIES_ALLOWED {
                return self.fail(SnapFailure::NoCommonAncestor);
            }
        } else {
            self.empty_header_replies = 0;
        }
        self.poll()
    }

    /// Is this block one this node had already accepted as canonical?
    ///
    /// Asked of the canonical index rather than of "is this header stored",
    /// because the walk itself stores headers by hash as it goes. A check that
    /// its own writes could satisfy would be no check at all: a peer could
    /// walk the client back to an invented genesis and have the client agree,
    /// on the strength of headers the client had just been handed. The
    /// canonical index is written only once the walk has already succeeded.
    fn is_ours(&self, number: u64, hash: B256) -> bool {
        self.store.canonical_hash(number).ok().flatten() == Some(hash)
    }

    fn start_state_download(&mut self) -> Vec<Action> {
        let checkpoint = self.checkpoint.clone();

        // The walk reached a block this node already had, so everything above
        // it is anchored to work this node had already accepted. Only now do
        // the headers become part of this chain.
        for (block, td) in std::mem::take(&mut self.offered) {
            let hash = block.header.hash();
            let _ = self.store.put_header_with_hash(hash, &block.header);
            let _ = self.store.put_body(hash, &block.transactions, &block.ommers);
            let _ = self.store.put_total_difficulty(hash, td);
        }
        if let Some(header) = &checkpoint {
            let hash = header.hash();
            let _ = self.store.put_total_difficulty(hash, self.checkpoint_td);

            // Index the window this session will work in, and no more.
            //
            // The walk stored every header it verified, by hash, which for a
            // fresh node is the whole chain. Indexing all of it here would
            // build one write batch of nine million entries -- hundreds of
            // megabytes, in the middle of the sync loop. What the session
            // needs indexed is the range whose bodies it is about to fetch,
            // plus a margin; the rest is a local pass over data already on
            // disk, and belongs in the background. See issue #133.
            let window = self.config.blocks_required + self.config.block_chunk_size + 1;
            match self.store.repair_canonical_lineage(hash, window) {
                Ok(report) => debug!(
                    target: "rustock::snap",
                    "indexed {} blocks down to #{:?} for the checkpoint",
                    report.walked, report.lowest
                ),
                Err(e) => warn!(
                    target: "rustock::snap",
                    "could not index the verified chain: {e}"
                ),
            }
            let _ = self.store.set_head(hash);
        }

        info!(
            target: "rustock::snap",
            "header chain verified back to #{}; downloading state at #{}",
            self.walk_tip.as_ref().map_or(0, |h| h.number),
            checkpoint.as_ref().map_or(0, |h| h.number)
        );
        self.phase = Phase::DownloadingState;
        self.poll()
    }

    /// A chunk of state, still unverified.
    ///
    /// `from` is the offset the *client* asked for, carried through the
    /// request's own bookkeeping -- never read off the response.
    pub fn on_chunk(&mut self, from: u64, payload: &ChunkPayload) -> Vec<Action> {
        if self.phase != Phase::DownloadingState {
            return Vec::new();
        }

        let proof = match super::client::proof_from_payload(payload) {
            Ok(proof) => proof,
            Err(e) => {
                // A peer speaking the wrong dialect is not a reason to abandon
                // the sync, only that peer: the range goes back in the queue.
                debug!(target: "rustock::snap", "chunk from offset {from} unusable: {e}");
                return self.fruitless(from);
            }
        };

        if proof.entries.is_empty() {
            // "I cannot serve this." Hand the range back rather than treat an
            // empty answer as the end of the trie -- only the root says where
            // that is.
            return self.fruitless(from);
        }

        let Some(download) = self.download.as_mut() else { return Vec::new() };
        match download.accept(from, &proof) {
            Ok(progress) => {
                self.fruitless_chunks = 0;
                if progress.complete {
                    return self.finish_state_download();
                }
            }
            Err(e) => {
                debug!(target: "rustock::snap", "chunk from offset {from} refused: {e}");
                return self.fruitless(from);
            }
        }
        self.poll()
    }

    /// An answer that added nothing: put the range back, and count it.
    fn fruitless(&mut self, from: u64) -> Vec<Action> {
        if let Some(download) = self.download.as_mut() {
            download.release(from);
        }
        self.fruitless_chunks += 1;
        if self.fruitless_chunks >= FRUITLESS_CHUNKS_ALLOWED {
            return self.fail(SnapFailure::NoProgress(self.fruitless_chunks));
        }
        self.poll()
    }

    /// Give a range back after a timeout or a disconnect.
    pub fn release_chunk(&mut self, from: u64) {
        if let Some(download) = self.download.as_mut() {
            download.release(from);
        }
    }

    /// Ask for the header walk again after a timeout.
    pub fn release_headers(&mut self) {
        self.headers_in_flight = false;
    }

    /// Ask for a block range again after a timeout.
    pub fn release_blocks(&mut self) {
        self.blocks_in_flight = false;
    }

    fn finish_state_download(&mut self) -> Vec<Action> {
        let Some(download) = self.download.as_ref() else { return Vec::new() };
        match download.verify_stored() {
            Ok(nodes) => {
                let (covered, _) = download.progress();
                info!(
                    target: "rustock::snap",
                    "state complete: {nodes} nodes, {covered} bytes, root {:?}",
                    download.root_hash()
                );
                self.phase = Phase::DownloadingBlocks;
                self.poll()
            }
            // Every chunk verified against the root, so this is not about the
            // peers -- it is this node failing to keep what it was given.
            Err(why) => self.fail(SnapFailure::IncompleteState(why)),
        }
    }

    /// Blocks behind the checkpoint, oldest first.
    pub fn on_blocks(&mut self, blocks: &[Block], difficulties: &[U256]) -> Vec<Action> {
        if self.phase != Phase::DownloadingBlocks {
            return Vec::new();
        }
        self.blocks_in_flight = false;

        if blocks.is_empty() || difficulties.len() != blocks.len() {
            // That peer had nothing to add. Another may; but if none does,
            // stop rather than ask the same question forever.
            self.fruitless_blocks += 1;
            if self.fruitless_blocks >= FRUITLESS_CHUNKS_ALLOWED {
                return self.fail(SnapFailure::NoProgress(self.fruitless_blocks));
            }
            return self.poll();
        }
        self.fruitless_blocks = 0;

        if let Err(why) = check_chain_shape(blocks, difficulties) {
            return self.fail(why);
        }

        let mut oldest = self.blocks_cursor;
        for (block, td) in blocks.iter().zip(difficulties.iter()) {
            let hash = block.header.hash();

            // These blocks are never executed -- they are fetched because
            // contracts can read them -- so nothing downstream would ever
            // check them. Bind each body to the header this node verified for
            // itself during the walk: the right chain, and the right contents
            // for that chain.
            if !self.is_ours(block.header.number, hash) {
                return self.fail(SnapFailure::BrokenChain);
            }
            if let Err(why) = check_body_matches_header(block) {
                return self.fail(why);
            }

            let _ = self.store.put_body(hash, &block.transactions, &block.ommers);
            let _ = self.store.put_total_difficulty(hash, *td);
            oldest = oldest.min(block.header.number);
        }

        self.blocks_cursor = oldest;
        if self.blocks_cursor <= self.blocks_floor() {
            info!(
                target: "rustock::snap",
                "snapshot sync complete: state at #{}, bodies back to #{}",
                self.checkpoint.as_ref().map_or(0, |h| h.number),
                self.blocks_cursor
            );
            self.phase = Phase::Done;
            return Vec::new();
        }
        self.poll()
    }
}

/// A body must be the one its header commits to.
///
/// The transaction root's encoding changed at the unitrie fork, and this has
/// no hardfork table to consult, so either encoding is accepted: both are
/// genuine, and matching one of them is what proves the transaction set. The
/// question being answered is "are these the right transactions", not "which
/// era is this".
fn check_body_matches_header(block: &Block) -> Result<(), SnapFailure> {
    use rustock_core::ordered_tx_trie_root;

    let wanted = block.header.transactions_root;
    if ordered_tx_trie_root(&block.transactions, true) != wanted
        && ordered_tx_trie_root(&block.transactions, false) != wanted
    {
        return Err(SnapFailure::BadBody {
            number: block.header.number,
            what: "transactions",
        });
    }
    if rustock_execution::processor::compute_ommers_hash(&block.ommers)
        != block.header.ommers_hash
    {
        return Err(SnapFailure::BadBody { number: block.header.number, what: "uncles" });
    }
    Ok(())
}

/// Blocks must form one chain, oldest first, with cumulative difficulty
/// rising along it.
///
/// This is rskj's `areBlockPairsValid` in miniature. It catches a peer
/// stitching unrelated blocks together cheaply, before any of the expensive
/// checks run -- it is not itself evidence that the chain is the real one.
fn check_chain_shape(blocks: &[Block], difficulties: &[U256]) -> Result<(), SnapFailure> {
    for i in 1..blocks.len() {
        let (parent, child) = (&blocks[i - 1], &blocks[i]);
        if child.header.parent_hash != parent.header.hash() {
            return Err(SnapFailure::BrokenChain);
        }
        if child.header.number != parent.header.number + 1 {
            return Err(SnapFailure::BrokenChain);
        }
        // Cumulative difficulty must grow by exactly this block's own
        // difficulty: the pair has to agree with itself.
        if difficulties[i] <= difficulties[i - 1] {
            return Err(SnapFailure::BadDifficulty);
        }
        if difficulties[i] - difficulties[i - 1] != child.header.difficulty {
            return Err(SnapFailure::BadDifficulty);
        }
    }
    Ok(())
}
