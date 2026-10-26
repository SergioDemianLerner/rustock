//! Import of an rskj `ExportBlocks` file into an empty node, through the same
//! validation and execution follow mode uses:
//!
//! 1. the header against its parent with the node's [`HeaderVerifier`], as
//!    [`SyncManager`](crate::SyncManager) does for every header it accepts;
//! 2. [`BlockProcessor::validate_block`], the body rules follow mode runs
//!    before executing;
//! 3. [`BlockProcessor::process_block`]: transactions root, ommers hash,
//!    execution, then gas used, paid fees, state root, receipts root and logs
//!    bloom against the header;
//! 4. header, total difficulty and canonical index stored with
//!    [`BlockStore::store_headers_batch`], the body with
//!    [`BlockStore::put_body`], then the receipts and indexes with
//!    [`BlockProcessor::commit_receipts`], which is the second half of
//!    `process_and_commit`;
//! 5. every `flush_every` blocks the trie flushed and the executed head
//!    recorded, through the same [`ExecutionCommitter`] follow mode uses.
//!
//! The order is rskj's (`BlockChainImpl.internalTryToConnect`): validate,
//! execute, check the results, then store, so that nothing is written for a
//! block that fails.
//!
//! Unlike sync, a block that does not extend the best block, or that fails
//! any check, ends the import: the file is one chain, and the first block
//! that does not connect is the answer the caller wants.

use alloy_primitives::{B256, U256};
use alloy_rlp::Decodable;
use rustock_core::validation::{HeaderVerifier, ValidationError};
use rustock_core::{Block, Header, Transaction};
use rustock_execution::{BlockProcessor, ProcessError};
use rustock_storage::BlockStore;
use rustock_trie::{TrieNode, TrieStore};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::execution_commit::ExecutionCommitter;

/// Blocks between flushes when the caller does not say: rskj's
/// `blockchain.flushNumberOfBlocks` default.
pub const DEFAULT_IMPORT_FLUSH_EVERY: u64 = 10;

/// Why a block was not imported.
#[derive(Debug, thiserror::Error)]
pub enum ImportError {
    #[error(
        "not the next block: parent {parent}, number {number}, \
         but the best block is #{best_number} {best_hash}"
    )]
    NotNextBlock {
        number: u64,
        parent: B256,
        best_number: u64,
        best_hash: B256,
    },
    #[error("header validation failed: {0}")]
    Header(ValidationError),
    #[error("block validation failed: {0}")]
    Block(ValidationError),
    #[error("{0}")]
    Process(#[from] ProcessError),
    #[error("recording the executed head failed")]
    ExecHead,
    #[error("storage error: {0}")]
    Storage(#[from] anyhow::Error),
}

impl ImportError {
    /// Short name of the check that failed, for the exit message.
    pub fn check(&self) -> &'static str {
        match self {
            ImportError::NotNextBlock { .. } => "parent",
            ImportError::Header(_) => "header",
            ImportError::Block(_) => "blockRules",
            ImportError::Process(e) => match e {
                ProcessError::GasUsedMismatch { .. } => "gasUsed",
                ProcessError::PaidFeesMismatch { .. } => "paidFees",
                ProcessError::StateRootMismatch { .. } => "stateRoot",
                ProcessError::ReceiptsRootMismatch { .. } => "receiptsRoot",
                ProcessError::TransactionsRootMismatch { .. } => "transactionsRoot",
                ProcessError::OmmersHashMismatch { .. } => "ommersHash",
                ProcessError::LogsBloomMismatch => "logsBloom",
                _ => "execution",
            },
            ImportError::ExecHead | ImportError::Storage(_) => "storage",
        }
    }
}

/// A block imported as the new best block.
#[derive(Debug, Clone)]
pub struct ImportedBlock {
    pub number: u64,
    pub hash: B256,
    pub tx_count: usize,
    pub gas_used: u64,
    /// Whether this block ended with a flush.
    pub flushed: bool,
    /// Where the time went, when the importer measures it
    /// ([`BlockImporter::measure`]).
    pub timings: Option<ImportTimings>,
}

/// Wall time of each phase of [`BlockImporter::import`], from a monotonic
/// clock. The phases run in this order and `total` covers the whole call.
///
/// Each phase is named after the phase of rskj's
/// `BlockChainImpl.internalTryToConnect` that does the same work, so the two
/// clients' lines can be read side by side.
#[derive(Debug, Default, Clone)]
pub struct ImportTimings {
    pub total: Duration,
    /// Checking that the block extends the best block (rskj `preambleMs`).
    pub preamble: Duration,
    /// Header against its parent, body rules, transactions root and ommers
    /// hash (rskj `initialValidationMs`).
    pub initial_validation: Duration,
    /// `execute_block` (rskj `executeMs`).
    pub execute: Duration,
    /// Results checked against the header (rskj `postExecuteValidationMs`).
    pub post_execute_validation: Duration,
    /// Header, total difficulty, canonical index and body stored (rskj
    /// `switchChainMs`, which saves the block and makes it best).
    pub switch_chain: Duration,
    /// Receipts and transaction and Bridge-event indexes stored (rskj
    /// `saveReceiptsMs`).
    pub save_receipts: Duration,
    /// Every `flush_every` blocks: the trie flushed and the executed head
    /// recorded, zero on other blocks (rskj `onBestBlockMs`, whose listener
    /// runs the flush every `flushNumberOfBlocks`).
    pub on_best_block: Duration,
}

impl ImportTimings {
    /// One line in the shape of rskj's `block connect breakdown best:` debug
    /// line, milliseconds with three decimals.
    pub fn breakdown_line(&self, block: &ImportedBlock) -> String {
        let ms = |d: Duration| format!("{:.3}", d.as_secs_f64() * 1000.0);
        format!(
            "block connect breakdown best: block={} hash={} totalMs={} preambleMs={} \
             initialValidationMs={} executeMs={} postExecuteValidationMs={} switchChainMs={} \
             saveReceiptsMs={} onBestBlockMs={} txCount={} gasUsed={}",
            block.number,
            alloy_primitives::hex::encode(block.hash),
            ms(self.total),
            ms(self.preamble),
            ms(self.initial_validation),
            ms(self.execute),
            ms(self.post_execute_validation),
            ms(self.switch_chain),
            ms(self.save_receipts),
            ms(self.on_best_block),
            block.tx_count,
            block.gas_used,
        )
    }
}

/// Times consecutive phases; reads no clock when disabled.
struct Laps {
    start: Option<Instant>,
    last: Option<Instant>,
}

impl Laps {
    fn new(enabled: bool) -> Self {
        let now = enabled.then(Instant::now);
        Self {
            start: now,
            last: now,
        }
    }

    /// Time since the previous lap, zero when disabled.
    fn lap(&mut self) -> Duration {
        match self.last {
            Some(last) => {
                let now = Instant::now();
                self.last = Some(now);
                now - last
            }
            None => Duration::ZERO,
        }
    }

    fn total(&self) -> Duration {
        self.start.map(|s| s.elapsed()).unwrap_or_default()
    }
}

/// Imports a chain block by block on top of the store's best block.
pub struct BlockImporter {
    store: Arc<BlockStore>,
    verifier: HeaderVerifier,
    processor: BlockProcessor,
    trie_store: Arc<dyn TrieStore>,
    state_root: TrieNode,
    best: Header,
    best_hash: B256,
    best_td: U256,
    committer: ExecutionCommitter,
    last_executed: Option<(B256, B256)>,
    measure: bool,
}

impl BlockImporter {
    /// An importer continuing from the store's head, whose state is
    /// `state_root`.
    pub fn new(
        store: Arc<BlockStore>,
        verifier: HeaderVerifier,
        processor: BlockProcessor,
        trie_store: Arc<dyn TrieStore>,
        state_root: TrieNode,
        flush_every: u64,
    ) -> anyhow::Result<Self> {
        let best_hash = store
            .head()?
            .ok_or_else(|| anyhow::anyhow!("the store has no head block"))?;
        let best = store
            .header(best_hash)?
            .ok_or_else(|| anyhow::anyhow!("head header {best_hash} missing"))?;
        let best_td = store.total_difficulty(best_hash)?.unwrap_or_default();
        Ok(Self {
            store,
            verifier,
            processor,
            trie_store,
            state_root,
            best,
            best_hash,
            best_td,
            committer: ExecutionCommitter::new(flush_every),
            last_executed: None,
            measure: false,
        })
    }

    /// Measure each import ([`ImportedBlock::timings`]). Off by default, and
    /// then no clock is read.
    pub fn measure(mut self, measure: bool) -> Self {
        self.measure = measure;
        self
    }

    /// Number and hash of the best block.
    pub fn best(&self) -> (u64, B256) {
        (self.best.number, self.best_hash)
    }

    /// Validate, execute and store `block` as the new best block.
    pub fn import(&mut self, block: Block) -> Result<ImportedBlock, ImportError> {
        let mut laps = Laps::new(self.measure);
        let mut t = ImportTimings::default();
        let mut process = rustock_execution::ProcessTimings::default();
        let header = &block.header;
        let hash = header.hash();
        if header.parent_hash != self.best_hash || header.number != self.best.number + 1 {
            return Err(ImportError::NotNextBlock {
                number: header.number,
                parent: header.parent_hash,
                best_number: self.best.number,
                best_hash: self.best_hash,
            });
        }
        t.preamble = laps.lap();

        self.verifier
            .verify(header, Some(&self.best))
            .map_err(ImportError::Header)?;
        self.processor
            .validate_block(&block)
            .map_err(ImportError::Block)?;
        t.initial_validation = laps.lap();
        let trie_store = self.trie_store.clone();
        let processed = if self.measure {
            self.processor
                .process_block_timed(&block, &self.state_root, trie_store, &mut process)
        } else {
            self.processor
                .process_block(&block, &self.state_root, trie_store)
        };
        let result = match processed {
            Ok(result) => result,
            Err(e) => {
                // Keep the trie nodes the failed block wrote: its computed
                // state can then be diffed against the reference client's.
                self.trie_store.flush();
                return Err(e.into());
            }
        };
        laps.lap();
        t.initial_validation += process.pre_validation;
        t.execute = process.execute;
        t.post_execute_validation = process.post_validation;

        let td = self.best_td + header.difficulty;
        self.store
            .store_headers_batch(&[(header, td)], Some(self.best_hash), self.best_td)?;
        self.store
            .put_body(hash, &block.transactions, &block.ommers)?;
        t.switch_chain = laps.lap();
        self.processor.commit_receipts(&block, &result)?;
        t.save_receipts = laps.lap();
        self.state_root = result.new_state_root;
        self.last_executed = Some((hash, result.state_root_hash));

        let store = &self.store;
        let root = result.state_root_hash;
        let flushed = match self
            .committer
            .after_block(self.trie_store.as_ref(), false, || {
                crate::service::record_execution(store, hash, root)
            }) {
            None => false,
            Some(true) => true,
            Some(false) => return Err(ImportError::ExecHead),
        };
        t.on_best_block = laps.lap();
        t.total = laps.total();

        let imported = ImportedBlock {
            number: header.number,
            hash,
            tx_count: block.transactions.len(),
            gas_used: header.gas_used,
            flushed,
            timings: self.measure.then_some(t),
        };
        self.best_hash = hash;
        self.best_td = td;
        self.best = block.header;
        Ok(imported)
    }

    /// Flush whatever the last blocks left in memory and record the head.
    pub fn finish(mut self) -> Result<(), ImportError> {
        let Some((hash, root)) = self.last_executed else {
            return Ok(());
        };
        let store = &self.store;
        match self.committer.finish(self.trie_store.as_ref(), || {
            crate::service::record_execution(store, hash, root)
        }) {
            Some(false) => Err(ImportError::ExecHead),
            _ => Ok(()),
        }
    }
}

/// One line of rskj's `ExportBlocks` format:
/// `number,hashHex,totalDifficultyHex,blockRlpHex`.
#[derive(Debug, Clone)]
pub struct ExportedBlock {
    pub number: u64,
    pub hash: B256,
    pub block: Block,
}

/// Decode one header of an exported block.
///
/// `ExportBlocks` writes `Block.getEncoded()`, whose headers are in rskj's
/// full encoding. For a version-0 header that is the hashed encoding too, so
/// the hash is taken from the bytes, as the wire decoders do. A versioned
/// (RSKIP-351) header is hashed in its compressed encoding, where the bloom
/// slot holds the extension commitment and the version fields are left out,
/// so its hash is recomputed from the decoded fields instead.
///
fn decode_exported_header(buf: &mut &[u8]) -> alloy_rlp::Result<Header> {
    let mut header = Header::decode_with_hash(buf)?;
    if header.version_fields.is_some() {
        header.cached_hash = None;
        header.cached_hash_for_merged_mining = None;
        header.cached_hash = Some(header.hash());
    }
    Ok(header)
}

/// Decode a block in rskj's encoding, `RLP([header, transactions, uncles])`,
/// with each header's hash computed as rskj computes it.
pub fn decode_block(mut rlp: &[u8]) -> anyhow::Result<Block> {
    let buf = &mut rlp;
    let list = alloy_rlp::Header::decode(buf)?;
    anyhow::ensure!(list.list, "block is not an RLP list");
    let mut body = &buf[..list.payload_length];
    let header = decode_exported_header(&mut body)?;
    let transactions = Vec::<Transaction>::decode(&mut body)?;
    let uncles = alloy_rlp::Header::decode(&mut body)?;
    anyhow::ensure!(uncles.list, "uncles are not an RLP list");
    let (mut uncles_rlp, rest) = body.split_at(uncles.payload_length);
    let mut ommers = Vec::new();
    while !uncles_rlp.is_empty() {
        ommers.push(decode_exported_header(&mut uncles_rlp)?);
    }
    anyhow::ensure!(rest.is_empty(), "unexpected fields after the uncles");
    Ok(Block {
        header,
        transactions,
        ommers,
    })
}

/// Parse one `ExportBlocks` line. `Ok(None)` for a line rskj's
/// `ConnectBlocks` skips (fewer than four fields). The decoded block must
/// have the number and hash the line states.
pub fn parse_export_line(line: &str) -> anyhow::Result<Option<ExportedBlock>> {
    let parts: Vec<&str> = line.trim().split(',').collect();
    if parts.len() < 4 {
        return Ok(None);
    }
    let number: u64 = parts[0]
        .parse()
        .map_err(|e| anyhow::anyhow!("block number '{}': {e}", parts[0]))?;
    let hash: B256 = parts[1]
        .parse()
        .map_err(|e| anyhow::anyhow!("block hash '{}': {e}", parts[1]))?;
    let rlp = alloy_primitives::hex::decode(parts[3])?;
    let block = decode_block(&rlp)?;
    anyhow::ensure!(
        block.header.number == number,
        "the line says block {number}, the block is #{}",
        block.header.number
    );
    anyhow::ensure!(
        block.header.hash() == hash,
        "block #{number}: the line's hash is {hash}, rustock computes {}",
        block.header.hash()
    );
    Ok(Some(ExportedBlock {
        number,
        hash,
        block,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_lines_are_skipped_like_rskj_connect_blocks() {
        assert!(parse_export_line("").unwrap().is_none());
        assert!(parse_export_line("1,abc,01").unwrap().is_none());
    }

    #[test]
    fn the_breakdown_line_reads_like_rskjs() {
        let block = ImportedBlock {
            number: 7,
            hash: B256::repeat_byte(0xab),
            tx_count: 21,
            gas_used: 24_000_000,
            flushed: false,
            timings: None,
        };
        let t = ImportTimings {
            total: Duration::from_micros(12_345),
            execute: Duration::from_micros(10_000),
            ..Default::default()
        };
        let line = t.breakdown_line(&block);
        assert!(line.starts_with(&format!(
            "block connect breakdown best: block=7 hash={} totalMs=12.345 preambleMs=0.000 ",
            "ab".repeat(32)
        )));
        assert!(line.contains(" executeMs=10.000 "));
        assert!(line.ends_with(" onBestBlockMs=0.000 txCount=21 gasUsed=24000000"));
    }

    #[test]
    fn malformed_lines_are_errors() {
        let hash = "00".repeat(32);
        assert!(parse_export_line(&format!("x,{hash},01,c0")).is_err());
        assert!(parse_export_line(&format!("1,{hash},01,zz")).is_err());
        // An empty list is not a block.
        assert!(parse_export_line(&format!("1,{hash},01,c0")).is_err());
    }
}
