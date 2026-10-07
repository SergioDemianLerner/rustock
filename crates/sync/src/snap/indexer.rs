//! Filling in the canonical index a snapshot sync left incomplete.
//!
//! # What is missing, and what is not
//!
//! A snap-synced node has every header of the chain on disk, verified, keyed
//! by hash. What it lacks is the `number -> hash` index below the window
//! around its checkpoint -- about 6400 blocks -- because building the rest
//! where the sync finishes would mean one write batch of nine million entries
//! in the middle of the sync loop.
//!
//! Nothing about consensus needs it. Every `number -> hash` lookup on the
//! execution path is bounded and falls inside that window: the `BlockHeader`
//! precompile reaches 4000 blocks, REMASC a few generations around its
//! maturity, `BLOCKHASH` 256. A node without this index executes, stays in
//! consensus, and follows the chain.
//!
//! Two things do need it. `eth_getBlockByNumber` and friends cannot answer
//! below the window. And -- the reason this is on by default -- neither can
//! `serve_block_hash_request` or `serve_skeleton_request`, so the node cannot
//! help anyone else sync past its own window. A node that takes history from
//! the network and gives none back is a freeloader by accident.
//!
//! # Why it is shaped like this
//!
//! Measured against mainnet at #9274144: the index is ~415 MB for 9.27M
//! heights, and the walk that builds it runs at 82,700 headers/s when the
//! headers are in page cache and 1,770/s when they are not -- about two
//! minutes warm, an hour and a half cold. A freshly snap-synced node is the
//! cold case; it has just written some 5.5 GB of headers.
//!
//! An hour and a half of background disk has two consequences. It must yield,
//! so the node it runs behind keeps its disk for executing blocks. And it must
//! **resume**, because a node that restarts daily would otherwise never
//! finish: the cursor is stored, so an interrupted pass picks up where it
//! stopped rather than at the top.
//!
//! # And it must stop at the prune floor
//!
//! This fill writes `number -> hash` downward; the pruner deletes exactly
//! those entries below its floor. Left to themselves the two undo each other
//! in a loop: the fill rebuilds the index for heights the pruner has just
//! discarded, the next sweep discards them again, and the node spends its
//! background disk writing data it is about to delete.
//!
//! So the fill stops at the floor. A pruning node does not want an index for
//! history it has thrown away, and the blocks below the floor are gone --
//! there is nothing there to point at. The stop lives inside
//! `index_canonical_batch`, per height: a batch is ten thousand heights, so a
//! check between batches would write straight through the floor and overshoot
//! by up to a batch.

use rustock_storage::BlockStore;
use std::sync::Arc;
use std::time::Duration;
use tracing::{debug, info, warn};

/// Heights per batch. Each is one write, and the pause between them is what
/// keeps this out of the way.
const BATCH: u64 = 10_000;

/// Rest between batches, so a node executing blocks keeps its disk.
const BREATHE: Duration = Duration::from_millis(250);

/// How often to say something, in batches. Quiet enough not to fill a log for
/// ninety minutes, loud enough that an operator can see it moving.
const REPORT_EVERY: u64 = 20;

/// Fill the canonical index below whatever is already indexed, from `head`
/// downward, and return when it is complete or the chain runs out.
///
/// Safe to call on a node that already has a full index: one lookup at the
/// bottom of the chain settles that before any walking begins.
pub async fn fill_canonical_index(store: Arc<BlockStore>, head: alloy_primitives::B256) {
    // Is there anything to do? The gap a snapshot sync leaves is at the
    // bottom, so one lookup at the bottom answers it -- rather than an hour of
    // reads discovering there was nothing missing.
    //
    // On a node with a freezer this is the usual answer, and it is why the
    // pass is now normally skipped outright. `canonical_hash` asks the
    // freezer when `CF_NUMBERS` has no entry, and the freezer holds canonical
    // headers addressed by number -- so every frozen height already has its
    // mapping, in a file, reachable by arithmetic. Building `CF_NUMBERS` for
    // those heights would be writing down what is already known, and it cost
    // hours of the node not following the chain (#195).
    if store.index_cursor().ok().flatten().is_none()
        && store.canonical_hash(1).ok().flatten().is_some()
    {
        let from_freezer = store
            .freezer()
            .map(|f| f.contains(1))
            .unwrap_or(false);
        if from_freezer {
            info!(
                target: "rustock::snap",
                "canonical index: the freezer answers for the frozen heights; nothing to fill"
            );
        } else {
            debug!(
                target: "rustock::snap",
                "canonical index already reaches the bottom of the chain; nothing to fill"
            );
        }
        return;
    }

    // Resume where an interrupted pass stopped, rather than at the top.
    let resume = store.index_cursor().ok().flatten();
    let mut cursor = match resume {
        Some((number, hash)) => {
            info!(
                target: "rustock::snap",
                "resuming canonical index fill at #{number}"
            );
            hash
        }
        None => head,
    };

    let mut written_total = 0u64;
    let mut batches = 0u64;
    let started = std::time::Instant::now();

    loop {
        let store_for_batch = store.clone();
        let at = cursor;
        // The walk is disk-bound and synchronous; a blocking thread keeps it
        // off the async runtime, where it would stall everything sharing the
        // worker.
        let outcome = tokio::task::spawn_blocking(move || {
            store_for_batch.index_canonical_batch(at, BATCH)
        })
        .await;

        let (written, next) = match outcome {
            Ok(Ok(result)) => result,
            Ok(Err(e)) => {
                warn!(target: "rustock::snap", "canonical index fill stopped: {e}");
                return;
            }
            Err(e) => {
                warn!(target: "rustock::snap", "canonical index fill panicked: {e}");
                return;
            }
        };

        written_total += written;
        batches += 1;

        let Some((next_number, next_hash)) = next else {
            let _ = store.clear_index_cursor();
            info!(
                target: "rustock::snap",
                "canonical index complete: {written_total} heights filled in {:?}",
                started.elapsed()
            );
            return;
        };

        // Recorded before the pause, so a kill during the pause costs nothing.
        if let Err(e) = store.set_index_cursor(next_number, next_hash) {
            debug!(target: "rustock::snap", "could not record the index cursor: {e}");
        }
        cursor = next_hash;

        if batches.is_multiple_of(REPORT_EVERY) {
            info!(
                target: "rustock::snap",
                "canonical index: {written_total} heights filled, now at #{next_number}"
            );
        }

        tokio::time::sleep(BREATHE).await;
    }
}
