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
//! - [`client`] drives a download: status, then chunks, then the blocks
//!   around the checkpoint.

pub mod client;
pub mod server;

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

    /// Bytes of trie nodes to ask for per chunk.
    ///
    /// The PoC report settles on 25-50KB: large enough that per-message
    /// overhead disappears, small enough that a slow peer holding one request
    /// does not stall the pipeline.
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

            chunk_bytes: 50_000,
            max_chunk_bytes: 1 << 20,
            max_in_flight: 8,
            max_requests_per_peer: 3,

            blocks_required: 6_000,
            block_chunk_size: 400,
        }
    }
}

impl SnapConfig {
    /// The checkpoint a node at `best` would offer: rounded down so that
    /// independent servers converge on the same block.
    pub fn checkpoint_for(&self, best: u64) -> u64 {
        let rounding = self.checkpoint_rounding.max(1);
        let rounded = best - (best % rounding);
        rounded.saturating_sub(self.checkpoint_distance)
    }
}
