use crate::helpers::{
    parse_b256, parse_block_number, to_hex_u256, to_hex_u64, BlockResultDto,
};
use crate::server::TxSubmitter;
use crate::types::*;
use rustock_core::config::ChainConfig;
use rustock_storage::BlockStore;
use serde_json::{json, Value};
use std::sync::Arc;

pub fn eth_protocol_version(id: Value) -> JsonRpcResponse {
    JsonRpcResponse::success(id, json!("0x3e"))
}

pub fn eth_chain_id(id: Value, config: &ChainConfig) -> JsonRpcResponse {
    JsonRpcResponse::success(id, json!(to_hex_u64(config.chain_id as u64)))
}

pub fn eth_block_number(id: Value, store: &BlockStore) -> JsonRpcResponse {
    match head_number(store) {
        Some(n) => JsonRpcResponse::success(id, json!(to_hex_u64(n))),
        None => JsonRpcResponse::error(id, INTERNAL_ERROR, "No head block"),
    }
}

pub async fn eth_syncing(
    id: Value,
    store: &BlockStore,
    peer_store: &rustock_networking::peers::PeerStore,
) -> JsonRpcResponse {
    // `currentBlock` is the executed head, not the best downloaded header.
    //
    // Those two diverge precisely when something is wrong. Header download and
    // block execution are separate pipelines, so if execution stalls the
    // downloaded head keeps climbing with the network while the state the node
    // can actually serve stays put. Reporting the downloaded head then makes a
    // wedged node indistinguishable from a healthy one: it answers `false`
    // -- fully synced -- while `eth_getBalance` serves state hundreds of blocks
    // stale. A node stuck this way went unnoticed for ~1,300 retries because
    // this method said it was fine.
    //
    // Callers reading `currentBlock` want the block whose state they will be
    // served, which is the executed one.
    let downloaded = head_number(store).unwrap_or(0);
    let current = executed_number(store).unwrap_or(downloaded);
    let peer_best = peer_store
        .best_peer()
        .await
        .map(|(_, meta)| meta.best_number)
        .unwrap_or(0);

    // The target is the highest block we know to exist. A block we have already
    // downloaded but not executed counts: with no peers, or with peers at our
    // own downloaded height, that is the only thing revealing the backlog.
    let highest = peer_best.max(downloaded);

    if highest > current {
        JsonRpcResponse::success(
            id,
            json!({
                "startingBlock": to_hex_u64(0),
                "currentBlock": to_hex_u64(current),
                "highestBlock": to_hex_u64(highest),
            }),
        )
    } else {
        JsonRpcResponse::success(id, json!(false))
    }
}

pub fn eth_gas_price(id: Value, store: &BlockStore) -> JsonRpcResponse {
    let price = head_header(store)
        .map(|h| to_hex_u256(&h.minimum_gas_price))
        .unwrap_or_else(|| "0x0".to_string());
    JsonRpcResponse::success(id, json!(price))
}

pub fn eth_mining(id: Value) -> JsonRpcResponse {
    JsonRpcResponse::success(id, json!(false))
}

pub fn eth_hashrate(id: Value) -> JsonRpcResponse {
    JsonRpcResponse::success(id, json!("0x0"))
}

pub fn eth_accounts(id: Value) -> JsonRpcResponse {
    JsonRpcResponse::success(id, json!([]))
}

pub fn eth_coinbase(id: Value) -> JsonRpcResponse {
    JsonRpcResponse::success(id, json!("0x0000000000000000000000000000000000000000"))
}

pub fn eth_get_block_by_hash(
    id: Value,
    params: &Value,
    store: &BlockStore,
) -> JsonRpcResponse {
    let Some(hash) = params.get(0).and_then(|v| v.as_str()).and_then(parse_b256) else {
        return JsonRpcResponse::error(id, INVALID_PARAMS, "Missing or invalid block hash");
    };
    let full_txs = params.get(1).and_then(|v| v.as_bool()).unwrap_or(false);

    match build_block_dto_with_full_txs(store, hash, full_txs) {
        Some(dto) => JsonRpcResponse::success(id, serde_json::to_value(dto).unwrap()),
        None => JsonRpcResponse::success(id, Value::Null),
    }
}

pub fn eth_get_block_by_number(
    id: Value,
    params: &Value,
    store: &BlockStore,
) -> JsonRpcResponse {
    let Some(bn_str) = params.get(0).and_then(|v| v.as_str()) else {
        return JsonRpcResponse::error(id, INVALID_PARAMS, "Missing block number parameter");
    };
    let head_num = head_number(store).unwrap_or(0);
    let Some(number) = parse_block_number(bn_str, head_num) else {
        return JsonRpcResponse::error(id, INVALID_PARAMS, "Invalid block number");
    };
    let full_txs = params.get(1).and_then(|v| v.as_bool()).unwrap_or(false);

    let hash = match store.canonical_hash(number) {
        Ok(Some(h)) => h,
        _ => return JsonRpcResponse::success(id, Value::Null),
    };

    match build_block_dto_with_full_txs(store, hash, full_txs) {
        Some(dto) => JsonRpcResponse::success(id, serde_json::to_value(dto).unwrap()),
        None => JsonRpcResponse::success(id, Value::Null),
    }
}

pub fn eth_get_block_transaction_count_by_hash(id: Value, params: &Value, store: &BlockStore) -> JsonRpcResponse {
    let Some(hash) = params.get(0).and_then(|v| v.as_str()).and_then(parse_b256) else {
        return JsonRpcResponse::error(id, INVALID_PARAMS, "Missing or invalid block hash");
    };
    match store.body(hash) {
        Ok(Some((txs, _))) => JsonRpcResponse::success(id, json!(to_hex_u64(txs.len() as u64))),
        Ok(None) => {
            // No body stored, but block may exist (header-only)
            if store.has_block(hash).unwrap_or(false) {
                JsonRpcResponse::success(id, json!("0x0"))
            } else {
                JsonRpcResponse::success(id, Value::Null)
            }
        }
        Err(_) => JsonRpcResponse::success(id, Value::Null),
    }
}

pub fn eth_get_block_transaction_count_by_number(id: Value, params: &Value, store: &BlockStore) -> JsonRpcResponse {
    let Some(bn_str) = params.get(0).and_then(|v| v.as_str()) else {
        return JsonRpcResponse::error(id, INVALID_PARAMS, "Missing block number");
    };
    let head_num = head_number(store).unwrap_or(0);
    let Some(number) = parse_block_number(bn_str, head_num) else {
        return JsonRpcResponse::error(id, INVALID_PARAMS, "Invalid block number");
    };
    let hash = match store.canonical_hash(number) {
        Ok(Some(h)) => h,
        _ => return JsonRpcResponse::success(id, Value::Null),
    };
    match store.body(hash) {
        Ok(Some((txs, _))) => JsonRpcResponse::success(id, json!(to_hex_u64(txs.len() as u64))),
        _ => JsonRpcResponse::success(id, json!("0x0")),
    }
}

pub fn eth_get_uncle_count_by_block_hash(id: Value, params: &Value, store: &BlockStore) -> JsonRpcResponse {
    let Some(hash) = params.get(0).and_then(|v| v.as_str()).and_then(parse_b256) else {
        return JsonRpcResponse::error(id, INVALID_PARAMS, "Missing or invalid block hash");
    };
    match store.header(hash) {
        Ok(Some(h)) => JsonRpcResponse::success(id, json!(to_hex_u64(h.uncle_count))),
        _ => JsonRpcResponse::success(id, Value::Null),
    }
}

pub fn eth_get_uncle_count_by_block_number(id: Value, params: &Value, store: &BlockStore) -> JsonRpcResponse {
    let Some(bn_str) = params.get(0).and_then(|v| v.as_str()) else {
        return JsonRpcResponse::error(id, INVALID_PARAMS, "Missing block number");
    };
    let head_num = head_number(store).unwrap_or(0);
    let Some(number) = parse_block_number(bn_str, head_num) else {
        return JsonRpcResponse::error(id, INVALID_PARAMS, "Invalid block number");
    };
    let hash = match store.canonical_hash(number) {
        Ok(Some(h)) => h,
        _ => return JsonRpcResponse::success(id, Value::Null),
    };
    match store.header(hash) {
        Ok(Some(h)) => JsonRpcResponse::success(id, json!(to_hex_u64(h.uncle_count))),
        _ => JsonRpcResponse::success(id, Value::Null),
    }
}

// -- internal helpers --------------------------------------------------------

fn head_number(store: &BlockStore) -> Option<u64> {
    store.head().ok()?
        .and_then(|h| store.header(h).ok().flatten())
        .map(|h| h.number)
}

/// Height of the last block the node has *executed*, i.e. the newest state it
/// can serve. Distinct from `head_number`, which is the newest header it has
/// downloaded.
fn executed_number(store: &BlockStore) -> Option<u64> {
    store.exec_head().ok()?
        .and_then(|(hash, _)| store.header(hash).ok().flatten())
        .map(|h| h.number)
}

fn head_header(store: &BlockStore) -> Option<rustock_core::types::header::Header> {
    store.head().ok()?
        .and_then(|h| store.header(h).ok().flatten())
}

fn build_block_dto_with_full_txs(
    store: &BlockStore,
    hash: alloy_primitives::B256,
    full_txs: bool,
) -> Option<BlockResultDto> {
    let header = store.header(hash).ok()??;
    // A block that exists must not disappear because an auxiliary field will
    // not parse. `ok()?` on the total difficulty turned a decode failure into
    // "no such block", so a single bad value hid the header, the transactions
    // and everything else behind a bare `null` -- with no error to explain it.
    // An unknown total difficulty is reported as zero; the block is still
    // returned.
    let td = store.total_difficulty(hash).ok().flatten().unwrap_or_default();
    let body = store.body(hash).ok().flatten();
    Some(BlockResultDto::from_header_with_body(&header, hash, td, body.as_ref(), full_txs))
}

pub async fn eth_send_raw_transaction(
    id: Value,
    params: &Value,
    tx_submitter: &Option<Arc<dyn TxSubmitter>>,
) -> JsonRpcResponse {
    let Some(submitter) = tx_submitter else {
        return JsonRpcResponse::error(id, METHOD_NOT_FOUND, "Transaction relay not available");
    };

    let Some(raw_hex) = params.as_array().and_then(|a| a.first()).and_then(|v| v.as_str()) else {
        return JsonRpcResponse::error(id, INVALID_PARAMS, "Missing raw transaction hex");
    };

    let raw_hex = raw_hex.strip_prefix("0x").unwrap_or(raw_hex);
    let Ok(raw_bytes) = hex::decode(raw_hex) else {
        return JsonRpcResponse::error(id, INVALID_PARAMS, "Invalid hex");
    };

    match submitter
        .submit_transaction(alloy_primitives::Bytes::from(raw_bytes))
        .await
    {
        Ok(tx_hash) => JsonRpcResponse::success(id, json!(format!("0x{:x}", tx_hash))),
        Err(e) => JsonRpcResponse::error(id, INVALID_PARAMS, e),
    }
}
