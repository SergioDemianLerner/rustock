//! Snapshot sync: downloading a state instead of replaying the chain into it.
//!
//! A node joining the network has two ways to obtain the state at a block. It
//! can execute every transaction from genesis, which is correct by
//! construction and takes days. Or it can download the state trie directly
//! and check it against a state root it trusts for other reasons -- which is
//! this.
//!
//! # What the client ends up trusting
//!
//! Only proof-of-work, which is what a full sync trusts too.
//!
//! 1. Peers offer a checkpoint block. The client verifies its header chain
//!    back to a block it already has, each header carrying merged-mining PoW.
//!    A peer that invents a checkpoint has to have mined it.
//! 2. That header's `state_root` becomes the anchor. Every chunk is checked
//!    against it on arrival by [`rustock_trie::snapshot_proof`], which proves
//!    both that the nodes are in that trie and that none between them is
//!    missing.
//! 3. The blocks behind the checkpoint are downloaded too, because contracts
//!    can read recent block data, so the state alone is not enough to execute
//!    what comes next.
//!
//! Nothing here asks the client to believe a peer about the contents of the
//! state. A dishonest peer can refuse to serve, or serve slowly, and that is
//! the whole of what it can do -- both handled by asking someone else.
//!
//! # Layout
//!
//! - [`server`] answers other nodes' requests from this node's own trie.
//! - [`client`] covers the trie's offset space, verifying and storing what
//!   comes back.
//! - [`session`] sequences a whole sync: status, header verification, state,
//!   then the blocks around the checkpoint.

pub mod client;
pub mod driver;
pub mod forward;
pub mod headers;
pub mod indexer;
pub mod rate;
pub mod server;
pub mod session;

#[cfg(test)]
mod forward_tests;
#[cfg(test)]
mod header_tests;
#[cfg(test)]
mod indexer_tests;
#[cfg(test)]
mod rate_tests;
#[cfg(test)]
mod session_tests;
#[cfg(test)]
mod tests;

/// Everything tunable about snapshot sync.
///
/// Defaults match rskj where a matching value exists, so that a deployment
/// switching between the two implementations sees the same behaviour.
#[derive(Debug, Clone)]
pub struct SnapConfig {
    /// Serve snapshots to other nodes.
    pub server_enabled: bool,
    /// Use snapshot sync to catch up, when far enough behind.
    pub client_enabled: bool,

    /// How far behind the tip the checkpoint sits (rskj `limit`).
    ///
    /// Far enough back that the state is settled and unlikely to be reorged
    /// out from under the download.
    pub checkpoint_distance: u64,
    /// Highest block this server will speak for, when told to behave as
    /// though the chain ends there (`--simulate-height`).
    ///
    /// The checkpoint is derived from the head, so without this a server
    /// simulating a short chain would still offer a state far above it — and
    /// a client could snap-sync straight past the end it was told about,
    /// which is not the test anyone asked for.
    pub serve_ceiling: Option<u64>,
    /// Bounds a peer's claimed cumulative difficulty before the header walk
    /// is committed to. `None` on a chain with no checkpoint, where the walk
    /// simply runs as before.
    pub checkpoint: Option<rustock_core::checkpoint::DifficultyCheckpoint>,
    /// Which checkpoint-based defences run. Has no effect without a
    /// `checkpoint` to run them against.
    ///
    /// See `rustock_core::checkpoint::CheckpointDefence` for the matrix: the
    /// hash check and the work bound are separate because one declares which
    /// chain is canonical and the other does not.
    pub checkpoint_defence: rustock_core::checkpoint::CheckpointDefence,
    /// The retarget divisor for the sampled window. That window sits entirely
    /// above the checkpoint, which is itself far above RSKIP156, so it is the
    /// post-RSKIP156 value -- but it is configuration rather than a constant
    /// because a wrong value here is a wrong verdict about a peer.
    pub difficulty_divisor: u64,
    /// The difficulty floor, which clamps the falling side of that bound.
    pub min_difficulty: alloy_primitives::U256,
    /// Checkpoints are rounded down to a multiple of this, so that every
    /// server picks the same one and their chunks are interchangeable
    /// (rskj `BLOCK_NUMBER_CHECKPOINT`).
    pub checkpoint_rounding: u64,

    /// The grid every chunk sits on, in offset space.
    ///
    /// Cell `i` is the run of nodes covering `[i*G, (i+1)*G)`, which depends
    /// on nothing but the trie and those two numbers. That is what lets a
    /// server cache a cell and serve it to every client that asks, and it is
    /// what rskj does too -- its client steps `from` by a fixed
    /// `chunkSize * 1024` rather than resuming wherever the last node ended.
    ///
    /// 100 KB of offset space is about 95 KB on the wire, measured against
    /// mainnet state at #9272510.
    pub chunk_grid: u64,

    /// Bytes of trie nodes to ask for per chunk.
    ///
    /// The PoC report settles on 25-50KB. This defaults higher, because the
    /// witness that proves a chunk costs O(depth) regardless of the chunk's
    /// size, so a bigger chunk spreads it further. Measured against mainnet
    /// state at #9272510:
    ///
    /// ```text
    ///  25 KB chunks -> witness is 13.4% of the payload
    ///  50 KB        ->             7.4%
    /// 100 KB        ->             3.4%
    /// 250 KB        ->             1.3%
    /// ```
    ///
    /// 100KB is where the curve flattens: past it the saving is under two
    /// points and the cost is a slow peer holding a larger piece of the
    /// download hostage for longer.
    pub chunk_bytes: u64,
    /// The most this node will serve in one chunk, whatever is asked.
    pub max_chunk_bytes: u64,
    /// Chunk requests in flight at once, across all peers.
    pub max_in_flight: usize,
    /// Chunk requests one peer may have queued here before being ignored
    /// (rskj `maxSenderRequests`).
    ///
    /// A limit on *concurrency*, not on rate: it stops a peer occupying every
    /// worker, but on its own it does not stop one pulling state as fast as
    /// the wire allows. Measured, a peer with three slots can take cached
    /// cells at about 300 MB/s.
    pub max_requests_per_peer: usize,

    /// State bytes per second one peer may be served.
    ///
    /// Egress is the one resource here that nothing else bounds. Computing
    /// cells is bounded by the cache -- a server serves exactly one state, so
    /// there are only ever ~9,200 distinct cells and the worst a peer can do
    /// is make it compute them all once, which then benefits everyone.
    ///
    /// 8 MB/s is generous for an honest client: the whole 0.9 GB mainnet
    /// state in about two minutes from a single server. It bounds one peer to
    /// roughly a fortieth of what it could take unthrottled.
    pub peer_bytes_per_second: u64,

    /// State bytes per second this server will produce in total.
    ///
    /// Four peers at the per-peer limit reach it. 32 MB/s is 256 Mbit, which
    /// leaves a gigabit host room for the ordinary block and transaction
    /// traffic it exists to carry; a smaller uplink wants a smaller number,
    /// which is why this is configuration rather than a constant.
    pub total_bytes_per_second: u64,

    /// Blocks before the checkpoint whose bodies are needed, because
    /// contracts can read them (rskj `BLOCKS_REQUIRED`).
    pub blocks_required: u64,
    /// Blocks per `SnapBlocks` response (rskj `BLOCK_CHUNK_SIZE`).
    pub block_chunk_size: u64,

    /// Establish the header chain by ascending from ground this node already
    /// holds, instead of descending from the peer's offered checkpoint.
    ///
    /// Off by default: the descending walk in `snap::headers` is what has been
    /// exercised against mainnet, and this changes the shape of the whole
    /// sync. Requires a peer that serves `rsk/63` headers-with-uncles -- the
    /// ascent totals the work exactly and cannot do that without them -- so a
    /// session against an `rsk/62` peer falls back to the descending walk
    /// whatever this says.
    ///
    /// See `docs/header-first-sync.md`.
    pub forward_headers: bool,

    /// Archive the uncle headers a sync receives, so this node can serve
    /// `rsk/63` and re-derive its own cumulative difficulty later.
    ///
    /// On by default. Off is what `--prune-uncles` asks for, and it is
    /// prevention rather than deletion: uncles have no store of their own in
    /// the block database -- they live inside bodies -- so the only separate
    /// copy is the freezer's, and the freezer cannot drop from the bottom.
    /// A node that does not want the history simply never writes it.
    pub archive_uncles: bool,

    /// After a snapshot sync, fill in the canonical `number -> hash` index for
    /// the history below the checkpoint window.
    ///
    /// Nothing about consensus needs it -- every execution-path lookup is
    /// bounded well inside the window the sync already indexes. What needs it
    /// is answering RPC about old heights, and serving history to other
    /// peers: without it the node takes history from the network and gives
    /// none back.
    pub index_history: bool,
}

impl Default for SnapConfig {
    fn default() -> Self {
        Self {
            // Off on both sides, as rskj ships it. Snapshot sync changes how
            // a node comes to trust its state; that is a decision an operator
            // makes, not a default they discover.
            server_enabled: false,
            client_enabled: false,

            checkpoint_distance: 10_000,
            checkpoint: None,
            checkpoint_defence: rustock_core::checkpoint::CheckpointDefence::NONE,
            serve_ceiling: None,
            // Post-RSKIP156, which is what the sampled window is by
            // construction.
            difficulty_divisor: 400,
            min_difficulty: alloy_primitives::U256::from_limbs([
                7_000_000_000_000_000u64, 0, 0, 0,
            ]),
            checkpoint_rounding: 5_000,

            chunk_grid: 100_000,
            chunk_bytes: 100_000,
            max_chunk_bytes: 1 << 20,
            max_in_flight: 8,
            max_requests_per_peer: 3,
            peer_bytes_per_second: 8 * 1024 * 1024,
            total_bytes_per_second: 32 * 1024 * 1024,

            forward_headers: false,
            archive_uncles: true,
            blocks_required: 6_000,
            block_chunk_size: 400,
            index_history: true,
        }
    }
}

/// The grid rskj's snapshot chunks sit on: `getSnapshotChunkSize()` returns a
/// hard-coded 50, times `CHUNK_ITEM_SIZE` of 1024.
///
/// Not configurable on their side, so it is a constant on ours too. An rskj
/// client asks on this grid and cannot be told otherwise, so serving one means
/// serving cells of this size.
pub const RSKJ_CHUNK_GRID: u64 = 50 * 1024;

impl SnapConfig {
    /// The cell an offset belongs to, and where that cell begins.
    pub fn cell_of(&self, offset: u64) -> (u64, u64) {
        let g = self.chunk_grid.max(1);
        let index = offset / g;
        (index, index * g)
    }

    /// Whether an offset is a cell boundary. A request that is not gets a
    /// refusal naming the reason rather than a silent empty answer, because
    /// the client's response should be to realign, not to give up on the peer.
    pub fn on_grid(&self, offset: u64) -> bool {
        offset.is_multiple_of(self.chunk_grid.max(1))
    }

    /// The checkpoint a node at `best` would offer: rounded down so that
    /// independent servers converge on the same block.
    pub fn checkpoint_for(&self, best: u64) -> u64 {
        let rounding = self.checkpoint_rounding.max(1);
        let rounded = best - (best % rounding);
        rounded.saturating_sub(self.checkpoint_distance)
    }
}
