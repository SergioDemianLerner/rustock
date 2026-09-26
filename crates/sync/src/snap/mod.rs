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
pub mod server;
pub mod session;

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
    pub max_requests_per_peer: usize,

    /// Blocks before the checkpoint whose bodies are needed, because
    /// contracts can read them (rskj `BLOCKS_REQUIRED`).
    pub blocks_required: u64,
    /// Blocks per `SnapBlocks` response (rskj `BLOCK_CHUNK_SIZE`).
    pub block_chunk_size: u64,
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
            checkpoint_rounding: 5_000,

            chunk_grid: 100_000,
            chunk_bytes: 100_000,
            max_chunk_bytes: 1 << 20,
            max_in_flight: 8,
            max_requests_per_peer: 3,

            blocks_required: 6_000,
            block_chunk_size: 400,
        }
    }
}

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
        offset % self.chunk_grid.max(1) == 0
    }

    /// The checkpoint a node at `best` would offer: rounded down so that
    /// independent servers converge on the same block.
    pub fn checkpoint_for(&self, best: u64) -> u64 {
        let rounding = self.checkpoint_rounding.max(1);
        let rounded = best - (best % rounding);
        rounded.saturating_sub(self.checkpoint_distance)
    }
}
