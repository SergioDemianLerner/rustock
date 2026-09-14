//! Deleting block history the node no longer needs.
//!
//! Distinct from the trie collector in `epoch_store`, which reclaims *state*.
//! This reclaims *chain data*: headers, bodies, receipts (and therefore the
//! events in them), the transaction index, and the canonical mapping, for
//! blocks far enough behind the head that nothing can ask for them.
//!
//! # How deep is far enough
//!
//! Two independent requirements stack:
//!
//! - **Reorg tolerance.** The node must be able to rebuild from a
//!   reorganisation up to `D` blocks deep, which needs the blocks themselves.
//! - **Block-info precompiles.** Rootstock exposes precompiles that read
//!   attributes of recent blocks, reaching up to 4,000 blocks back. Executing a
//!   block at height `h` may therefore read data from `h - 4000`.
//!
//! They compose rather than overlap: after a 4,000-block reorg the node
//! re-executes from `head - 4000`, and executing *that* block may reach back a
//! further 4,000. So the shallowest safe retention is **8,000 blocks**, and
//! [`MIN_KEEP_DEPTH`] is a floor the configuration cannot go below.
//!
//! # Gaps are a supported state
//!
//! After pruning, the database has no blocks below a floor. Everything that
//! walks the chain must tolerate that: `ensure_canonical_lineage` already stops
//! when a parent header is absent, the sync connection-point search converges to
//! a height at or above the floor because lower ones are not "ours", and the RPC
//! answers `null` for a pruned height exactly as it does for an unknown one.
//!
//! Genesis is always retained. It costs one block and keeps every "start of the
//! chain" lookup working, so the gap is a hole in the middle of the chain rather
//! than a missing beginning.
//!
//! The floor is recorded in the database -- number, hash and total difficulty --
//! so a node can say what it holds without a scan, and so the value needed to
//! keep total difficulty accumulating is never the thing that was deleted.

use alloy_primitives::{B256, U256};
use anyhow::{bail, Context, Result};
use rocksdb::WriteBatch;
use std::time::Instant;
use tracing::{debug, info};

use crate::BlockStore;

/// Blocks that must always be retained below the head.
///
/// 4,000 for reorg tolerance plus 4,000 for the block-info precompiles, which a
/// re-executed block may itself reach back through. Configuration is clamped to
/// this; it is not a default.
pub const MIN_KEEP_DEPTH: u64 = 8_000;

/// Key under which the floor record lives.
const KEY_PRUNE_FLOOR: &[u8] = b"prune_floor";

#[derive(Debug, Clone)]
pub struct PruneConfig {
    /// Blocks to keep below the head. Clamped up to [`MIN_KEEP_DEPTH`].
    pub keep_depth: u64,
    /// Most blocks to remove in one sweep, so a single call cannot stall the
    /// node for an unbounded time.
    pub max_batch: u64,
}

impl Default for PruneConfig {
    fn default() -> Self {
        Self { keep_depth: 100_000, max_batch: 50_000 }
    }
}

impl PruneConfig {
    fn effective_keep_depth(&self) -> u64 {
        self.keep_depth.max(MIN_KEEP_DEPTH)
    }
}

/// The oldest block the database still holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PruneFloor {
    pub number: u64,
    pub hash: B256,
    /// Cumulative difficulty at the floor. Retained because everything below it
    /// is gone, so this is the only remaining anchor for the chain's weight.
    pub total_difficulty: U256,
}

#[derive(Debug, Default, Clone)]
pub struct PruneStats {
    pub from: u64,
    pub to: u64,
    pub blocks: u64,
    pub bodies: u64,
    pub receipts: u64,
    pub transactions: u64,
    pub seconds: f64,
}

impl BlockStore {
    /// The oldest retained block, if the database has ever been pruned.
    pub fn prune_floor(&self) -> Result<Option<PruneFloor>> {
        let Some(raw) = self.db().get(KEY_PRUNE_FLOOR).context("reading prune floor")? else {
            return Ok(None);
        };
        if raw.len() != 8 + 32 + 32 {
            bail!("prune floor record is {} bytes, expected 72", raw.len());
        }
        let mut num = [0u8; 8];
        num.copy_from_slice(&raw[..8]);
        Ok(Some(PruneFloor {
            number: u64::from_be_bytes(num),
            hash: B256::from_slice(&raw[8..40]),
            total_difficulty: U256::from_be_slice(&raw[40..72]),
        }))
    }

    fn set_prune_floor(&self, floor: &PruneFloor) -> Result<()> {
        let mut buf = Vec::with_capacity(72);
        buf.extend_from_slice(&floor.number.to_be_bytes());
        buf.extend_from_slice(floor.hash.as_slice());
        buf.extend_from_slice(&floor.total_difficulty.to_be_bytes::<32>());
        self.db().put(KEY_PRUNE_FLOOR, buf).context("writing prune floor")
    }

    /// Removes block data below `head_number - keep_depth`.
    ///
    /// Returns what was removed. Deleting nothing is success, not an error: a
    /// chain shorter than the retention depth simply has nothing to prune.
    pub fn prune_blocks(&self, config: &PruneConfig, head_number: u64) -> Result<PruneStats> {
        let keep = config.effective_keep_depth();
        let started = Instant::now();
        let mut stats = PruneStats::default();

        let Some(target) = head_number.checked_sub(keep) else {
            debug!(
                target: "rustock::prune",
                "Head #{head_number} is within the {keep}-block retention depth; nothing to prune"
            );
            return Ok(stats);
        };

        let floor = self.prune_floor()?;
        // Resume *at* the floor, not past it. The floor is the lowest pruned-to
        // block, so it is the next one eligible to go; starting at `floor + 1`
        // leaves one block behind on every resumed sweep, stranding data below
        // the floor that nothing will ever report or reclaim.
        //
        // Genesis is never pruned, so the first prunable block is 1. Keeping it
        // costs one block and removes a class of edge cases: every "start of the
        // chain" lookup keeps working, and the chain retains an anchor whose
        // hash is fixed by the network rather than by what this node happens
        // still to hold.
        let from = floor.as_ref().map(|f| f.number).unwrap_or(1).max(1);
        if from > target {
            debug!(target: "rustock::prune", "Already pruned to #{}", from.saturating_sub(1));
            return Ok(stats);
        }
        let to = target.min(from + config.max_batch.max(1) - 1);

        // Establish the new floor *before* deleting, so an interrupted sweep
        // leaves a floor that understates what is held rather than one that
        // claims blocks already gone. Over-reporting what exists is the
        // dangerous direction: it invites a reader to ask for something deleted.
        let new_floor_number = to + 1;
        let new_floor = self.floor_record(new_floor_number)?;
        if let Some(f) = &new_floor {
            self.set_prune_floor(f)?;
        }

        let cf_headers = self.cf_headers()?;
        let cf_bodies = self.cf_bodies()?;
        let cf_numbers = self.cf_numbers()?;
        let cf_td = self.cf_td()?;
        let cf_receipts = self.cf_receipts()?;
        let cf_tx_index = self.cf_tx_index()?;

        let mut batch = WriteBatch::default();
        for number in from..=to {
            let Some(hash) = self.canonical_hash(number)? else {
                continue;
            };

            // The transaction index is keyed by transaction hash, so the body
            // has to be read to know which entries belong to this block.
            if let Ok(Some((txs, _))) = self.body(hash) {
                for tx in &txs {
                    let mut buf = Vec::new();
                    alloy_rlp::Encodable::encode(tx, &mut buf);
                    let tx_hash = B256::from_slice(&<sha3::Keccak256 as sha3::Digest>::digest(&buf));
                    batch.delete_cf(cf_tx_index, tx_hash.as_slice());
                    stats.transactions += 1;
                }
                batch.delete_cf(cf_bodies, hash.as_slice());
                stats.bodies += 1;
            }
            if matches!(self.receipts(hash), Ok(Some(_))) {
                batch.delete_cf(cf_receipts, hash.as_slice());
                stats.receipts += 1;
            }
            batch.delete_cf(cf_headers, hash.as_slice());
            batch.delete_cf(cf_td, hash.as_slice());
            batch.delete_cf(cf_numbers, number.to_be_bytes());
            stats.blocks += 1;
        }

        self.db().write(batch).context("committing prune batch")?;

        stats.from = from;
        stats.to = to;
        stats.seconds = started.elapsed().as_secs_f64();
        info!(
            target: "rustock::prune",
            "Pruned #{}..#{}: {} blocks, {} bodies, {} receipts, {} tx index entries in {:.1}s \
             (floor now #{})",
            stats.from, stats.to, stats.blocks, stats.bodies, stats.receipts,
            stats.transactions, stats.seconds,
            new_floor.as_ref().map(|f| f.number).unwrap_or(new_floor_number)
        );
        Ok(stats)
    }

    /// The lowest block above genesis that the database holds contiguously: the
    /// prune floor if it has been pruned, genesis otherwise.
    ///
    /// Genesis itself is always present, so this is not "the oldest block" so
    /// much as "where uninterrupted history begins". Callers that need a chain
    /// anchor can keep using block 0.
    pub fn oldest_block(&self) -> Result<Option<(u64, B256)>> {
        if let Some(f) = self.prune_floor()? {
            if self.canonical_hash(f.number)? == Some(f.hash) {
                return Ok(Some((f.number, f.hash)));
            }
        }
        Ok(self.canonical_hash(0)?.map(|h| (0, h)))
    }

    /// Builds the floor record for a block that is being retained.
    fn floor_record(&self, number: u64) -> Result<Option<PruneFloor>> {
        let Some(hash) = self.canonical_hash(number)? else {
            return Ok(None);
        };
        let td = self.total_difficulty(hash)?.unwrap_or_default();
        Ok(Some(PruneFloor { number, hash, total_difficulty: td }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::BlockStore;
    use rustock_core::types::header::Header;
    use tempfile::tempdir;

    fn header(number: u64, parent: B256) -> Header {
        Header {
            number,
            parent_hash: parent,
            ommers_hash: B256::ZERO,
            beneficiary: alloy_primitives::Address::ZERO,
            state_root: B256::ZERO,
            transactions_root: B256::ZERO,
            receipts_root: B256::ZERO,
            logs_bloom: Default::default(),
            extension_data: None,
            difficulty: U256::from(1_000u64),
            gas_limit: U256::ZERO,
            gas_used: 0,
            timestamp: 1_700_000_000 + number,
            extra_data: alloy_primitives::Bytes::default(),
            paid_fees: U256::ZERO,
            minimum_gas_price: U256::ZERO,
            uncle_count: 0,
            umm_root: None,
            bitcoin_merged_mining_header: None,
            bitcoin_merged_mining_merkle_proof: None,
            bitcoin_merged_mining_coinbase_transaction: None,
            cached_hash: None,
            cached_hash_for_merged_mining: None,
        }
    }

    /// Builds a canonical chain of `n` blocks (0..n-1) with total difficulty.
    fn chain(store: &BlockStore, n: u64) -> Vec<B256> {
        let mut hashes = Vec::with_capacity(n as usize);
        let mut parent = B256::ZERO;
        let mut td = U256::ZERO;
        for i in 0..n {
            let h = header(i, parent);
            let hash = h.hash();
            td += h.difficulty;
            store.put_header(&h).unwrap();
            store.put_canonical_hash(i, hash).unwrap();
            store.put_total_difficulty(hash, td).unwrap();
            store.put_body(hash, &[], &[]).unwrap();
            parent = hash;
            hashes.push(hash);
        }
        hashes
    }

    #[test]
    fn keeps_at_least_the_minimum_depth_whatever_the_configuration_says() {
        // The floor exists because Rootstock's block-info precompiles reach
        // 4,000 blocks back and a reorg may re-execute from 4,000 back, so a
        // re-executed block can read 8,000 back. A configuration asking for less
        // must be clamped, not honoured.
        let dir = tempdir().unwrap();
        let store = BlockStore::open(dir.path()).unwrap();
        chain(&store, 12_000);

        let cfg = PruneConfig { keep_depth: 10, max_batch: 100_000 };
        assert_eq!(cfg.effective_keep_depth(), MIN_KEEP_DEPTH);

        let stats = store.prune_blocks(&cfg, 11_999).unwrap();
        assert_eq!(stats.to, 11_999 - MIN_KEEP_DEPTH, "pruned no closer than the floor");

        // Everything inside the retention window is still readable.
        for n in (11_999 - MIN_KEEP_DEPTH + 1)..=11_999 {
            let hash = store.canonical_hash(n).unwrap().expect("retained block");
            assert!(store.header(hash).unwrap().is_some(), "#{n} must survive");
        }
    }

    #[test]
    fn prunes_nothing_on_a_chain_shorter_than_the_retention_depth() {
        let dir = tempdir().unwrap();
        let store = BlockStore::open(dir.path()).unwrap();
        chain(&store, 500);
        let stats = store.prune_blocks(&PruneConfig::default(), 499).unwrap();
        assert_eq!(stats.blocks, 0);
        assert!(store.prune_floor().unwrap().is_none(), "no floor recorded");
        assert!(store.header(store.canonical_hash(0).unwrap().unwrap()).unwrap().is_some());
    }

    #[test]
    fn removes_headers_bodies_receipts_and_the_transaction_index() {
        let dir = tempdir().unwrap();
        let store = BlockStore::open(dir.path()).unwrap();
        chain(&store, 9_000);

        let doomed = store.canonical_hash(5).unwrap().unwrap();
        let survivor = store.canonical_hash(8_999).unwrap().unwrap();
        store.put_receipts(doomed, &[]).unwrap();

        let stats = store
            .prune_blocks(&PruneConfig { keep_depth: MIN_KEEP_DEPTH, max_batch: 100_000 }, 8_999)
            .unwrap();
        assert!(stats.blocks > 0, "something must have been pruned");

        assert!(store.header(doomed).unwrap().is_none(), "header gone");
        assert!(store.body(doomed).unwrap().is_none(), "body gone");
        assert!(store.receipts(doomed).unwrap().is_none(), "receipts and their events gone");
        assert!(store.canonical_hash(5).unwrap().is_none(), "canonical mapping gone");
        assert!(store.total_difficulty(doomed).unwrap().is_none(), "td gone");

        assert!(store.header(survivor).unwrap().is_some(), "the head is untouched");
    }

    #[test]
    fn genesis_is_never_pruned() {
        // Keeping block 0 costs one block and keeps every "start of the chain"
        // lookup working, so a pruned database has a hole in the middle rather
        // than a missing beginning.
        let dir = tempdir().unwrap();
        let store = BlockStore::open(dir.path()).unwrap();
        let hashes = chain(&store, 12_000);

        store
            .prune_blocks(&PruneConfig { keep_depth: MIN_KEEP_DEPTH, max_batch: 100_000 }, 11_999)
            .unwrap();

        assert_eq!(store.canonical_hash(0).unwrap(), Some(hashes[0]), "genesis kept");
        assert!(store.header(hashes[0]).unwrap().is_some(), "genesis header kept");
        assert!(store.canonical_hash(1).unwrap().is_none(), "block 1 pruned");
        assert!(store.header(hashes[1]).unwrap().is_none());
    }

    #[test]
    fn records_a_floor_that_can_be_read_back() {
        let dir = tempdir().unwrap();
        let store = BlockStore::open(dir.path()).unwrap();
        chain(&store, 9_000);

        store
            .prune_blocks(&PruneConfig { keep_depth: MIN_KEEP_DEPTH, max_batch: 100_000 }, 8_999)
            .unwrap();

        let floor = store.prune_floor().unwrap().expect("floor recorded");
        assert_eq!(floor.number, 8_999 - MIN_KEEP_DEPTH + 1, "floor is the lowest retained block");
        // The floor's own data is retained, including the total difficulty that
        // everything below it used to carry.
        assert_eq!(store.canonical_hash(floor.number).unwrap(), Some(floor.hash));
        assert!(store.header(floor.hash).unwrap().is_some());
        assert!(!floor.total_difficulty.is_zero(), "difficulty anchor kept");
        assert_eq!(store.total_difficulty(floor.hash).unwrap(), Some(floor.total_difficulty));
    }

    #[test]
    fn pruning_is_resumable_and_idempotent() {
        let dir = tempdir().unwrap();
        let store = BlockStore::open(dir.path()).unwrap();
        chain(&store, 12_000);

        // Small batches, as a continuous pruner would use.
        let cfg = PruneConfig { keep_depth: MIN_KEEP_DEPTH, max_batch: 500 };
        let mut total = 0;
        for _ in 0..20 {
            total += store.prune_blocks(&cfg, 11_999).unwrap().blocks;
        }
        // Blocks 1..=target are prunable; genesis is retained, so the count is
        // `target`, not `target + 1`.
        assert_eq!(total, 11_999 - MIN_KEEP_DEPTH, "all prunable blocks removed across sweeps");

        // Running again removes nothing and does not move the floor.
        let before = store.prune_floor().unwrap().unwrap();
        assert_eq!(store.prune_blocks(&cfg, 11_999).unwrap().blocks, 0);
        assert_eq!(store.prune_floor().unwrap().unwrap(), before);
    }

    #[test]
    fn the_chain_above_the_floor_stays_walkable_by_parent_hash() {
        // A pruned database has a gap below the floor. Walking back from the
        // head must still work down to the floor, and must stop there rather
        // than failing.
        let dir = tempdir().unwrap();
        let store = BlockStore::open(dir.path()).unwrap();
        chain(&store, 9_500);
        store
            .prune_blocks(&PruneConfig { keep_depth: MIN_KEEP_DEPTH, max_batch: 100_000 }, 9_499)
            .unwrap();

        let floor = store.prune_floor().unwrap().unwrap();
        let mut hash = store.canonical_hash(9_499).unwrap().unwrap();
        let mut walked = 0u64;
        loop {
            let Some(h) = store.header(hash).unwrap() else { break };
            walked += 1;
            if h.number == 0 {
                break;
            }
            hash = h.parent_hash;
        }
        assert_eq!(
            walked,
            9_499 - floor.number + 1,
            "the walk reaches the floor and stops, rather than running off the end"
        );
    }
}
