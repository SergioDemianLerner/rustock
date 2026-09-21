//! The `mnr_*` namespace: what mining software talks to.
//!
//! The JSON shapes are rskj's, field for field, because the consumers are
//! existing pool daemons rather than anything written against this node. Two
//! of them are surprising enough to be worth naming: `feesPaidToMiner` is a
//! *decimal* string (rskj sends `String.valueOf(Coin)`, not a quantity), and
//! `blockImportedResult` is the hex encoding of an ASCII status word such as
//! `IMPORTED_BEST` -- `HexUtils.toJsonHex(stringToByteArray(...))` in
//! `SubmittedBlockInfo`. Sending either the "sensible" way breaks the clients.

use rustock_execution::mining::{MinerWork, SubmitError, SubmittedBlockInfo};
use serde_json::{json, Value};
use std::sync::Arc;

use crate::helpers::{to_hex_b256, to_hex_u64};
use crate::types::*;

/// rskj's application-defined error code for a rejected block submission
/// (`JsonRpcApplicationDefinedErrorCodes.SUBMIT_BLOCK`).
pub const SUBMIT_BLOCK_ERROR: i64 = -33000;

/// The node's miner, as the RPC layer needs it. A trait so the RPC crate does
/// not have to assemble a real miner -- with a block processor, a trie store
/// and a chain behind it -- to be tested.
pub trait MiningService: Send + Sync {
    fn get_work(&self) -> Result<MinerWork, SubmitError>;
    fn submit_bitcoin_block(&self, raw_block: &[u8]) -> Result<SubmittedBlockInfo, SubmitError>;
    fn submit_bitcoin_block_transactions(
        &self,
        raw_header: &[u8],
        raw_coinbase: &[u8],
        tx_hashes: &str,
    ) -> Result<SubmittedBlockInfo, SubmitError>;
    fn submit_bitcoin_block_partial_merkle(
        &self,
        raw_header: &[u8],
        raw_coinbase: &[u8],
        merkle_hashes: &str,
        block_tx_count: u32,
    ) -> Result<SubmittedBlockInfo, SubmitError>;
}

impl MiningService for rustock_execution::MinerServer {
    fn get_work(&self) -> Result<MinerWork, SubmitError> {
        rustock_execution::MinerServer::get_work(self)
    }

    fn submit_bitcoin_block(&self, raw_block: &[u8]) -> Result<SubmittedBlockInfo, SubmitError> {
        rustock_execution::MinerServer::submit_bitcoin_block(self, raw_block)
    }

    fn submit_bitcoin_block_transactions(
        &self,
        raw_header: &[u8],
        raw_coinbase: &[u8],
        tx_hashes: &str,
    ) -> Result<SubmittedBlockInfo, SubmitError> {
        rustock_execution::MinerServer::submit_bitcoin_block_transactions(
            self, raw_header, raw_coinbase, tx_hashes,
        )
    }

    fn submit_bitcoin_block_partial_merkle(
        &self,
        raw_header: &[u8],
        raw_coinbase: &[u8],
        merkle_hashes: &str,
        block_tx_count: u32,
    ) -> Result<SubmittedBlockInfo, SubmitError> {
        rustock_execution::MinerServer::submit_bitcoin_block_partial_merkle(
            self, raw_header, raw_coinbase, merkle_hashes, block_tx_count,
        )
    }
}

pub fn mnr_get_work(id: Value, miner: &Arc<dyn MiningService>) -> JsonRpcResponse {
    match miner.get_work() {
        Ok(work) => JsonRpcResponse::success(id, json!(work_to_json(&work))),
        Err(e) => JsonRpcResponse::error(id, INTERNAL_ERROR, e.to_string()),
    }
}

fn work_to_json(work: &MinerWork) -> Value {
    json!({
        "blockHashForMergedMining": to_hex_b256(&work.block_hash_for_merged_mining),
        // Zero-padded to the full 32 bytes: this is a target to compare a hash
        // against, not a quantity, and rskj pads it the same way.
        "target": format!("0x{:064x}", work.target),
        "feesPaidToMiner": work.fees_paid_to_miner.to_string(),
        "notify": work.notify,
        "parentBlockHash": to_hex_b256(&work.parent_block_hash),
    })
}

pub fn mnr_submit_bitcoin_block(
    id: Value,
    params: &Value,
    miner: &Arc<dyn MiningService>,
) -> JsonRpcResponse {
    let raw = match hex_param(params, 0) {
        Ok(v) => v,
        Err(e) => return JsonRpcResponse::error(id, INVALID_PARAMS, e),
    };
    respond(id, miner.submit_bitcoin_block(&raw))
}

pub fn mnr_submit_bitcoin_block_transactions(
    id: Value,
    params: &Value,
    miner: &Arc<dyn MiningService>,
) -> JsonRpcResponse {
    // rskj takes (blockHashHex, blockHeaderHex, coinbaseHex, txnHashesHex) and
    // ignores the first: the block hash is recomputed from the header, and a
    // submission that disagreed with itself about it would be caught by the
    // merged-mining check anyway.
    let header = match hex_param(params, 1) {
        Ok(v) => v,
        Err(e) => return JsonRpcResponse::error(id, INVALID_PARAMS, e),
    };
    let coinbase = match hex_param(params, 2) {
        Ok(v) => v,
        Err(e) => return JsonRpcResponse::error(id, INVALID_PARAMS, e),
    };
    let hashes = match str_param(params, 3) {
        Ok(v) => v,
        Err(e) => return JsonRpcResponse::error(id, INVALID_PARAMS, e),
    };
    respond(id, miner.submit_bitcoin_block_transactions(&header, &coinbase, &hashes))
}

pub fn mnr_submit_bitcoin_block_partial_merkle(
    id: Value,
    params: &Value,
    miner: &Arc<dyn MiningService>,
) -> JsonRpcResponse {
    let header = match hex_param(params, 1) {
        Ok(v) => v,
        Err(e) => return JsonRpcResponse::error(id, INVALID_PARAMS, e),
    };
    let coinbase = match hex_param(params, 2) {
        Ok(v) => v,
        Err(e) => return JsonRpcResponse::error(id, INVALID_PARAMS, e),
    };
    let hashes = match str_param(params, 3) {
        Ok(v) => v,
        Err(e) => return JsonRpcResponse::error(id, INVALID_PARAMS, e),
    };
    if hashes.trim().is_empty() {
        return JsonRpcResponse::error(
            id,
            SUBMIT_BLOCK_ERROR,
            "The list of merkle hashes can't be empty",
        );
    }
    let tx_count = match str_param(params, 4) {
        Ok(v) => match u32::from_str_radix(v.strip_prefix("0x").unwrap_or(&v), 16) {
            Ok(n) => n,
            Err(_) => {
                return JsonRpcResponse::error(id, INVALID_PARAMS, "blockTxnCount is not hex")
            }
        },
        Err(e) => return JsonRpcResponse::error(id, INVALID_PARAMS, e),
    };
    respond(
        id,
        miner.submit_bitcoin_block_partial_merkle(&header, &coinbase, &hashes, tx_count),
    )
}

fn respond(id: Value, result: Result<SubmittedBlockInfo, SubmitError>) -> JsonRpcResponse {
    match result {
        Ok(info) => JsonRpcResponse::success(
            id,
            json!({
                "blockImportedResult": format!(
                    "0x{}",
                    hex::encode(info.block_imported_result.as_str().as_bytes())
                ),
                "blockHash": to_hex_b256(&info.block_hash),
                "blockIncludedHeight": to_hex_u64(info.block_included_height),
            }),
        ),
        // rskj turns every rejected submission into one application-defined
        // error, so a miner distinguishes them by message rather than code.
        Err(e) => JsonRpcResponse::error(id, SUBMIT_BLOCK_ERROR, e.to_string()),
    }
}

fn str_param(params: &Value, index: usize) -> Result<String, String> {
    params
        .get(index)
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| format!("missing or non-string parameter {index}"))
}

fn hex_param(params: &Value, index: usize) -> Result<Vec<u8>, String> {
    let raw = str_param(params, index)?;
    let trimmed = raw.strip_prefix("0x").unwrap_or(&raw);
    hex::decode(trimmed).map_err(|_| format!("parameter {index} is not hex"))
}
