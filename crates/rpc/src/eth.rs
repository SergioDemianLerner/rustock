use crate::helpers::{
    parse_b256, parse_block_number, parse_hex_u32, to_hex_u256, to_hex_u64, BlockResultDto,
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

/// Pending transactions **belonging to this node's own accounts** — always
/// empty here, because this node has no wallet.
///
/// The name misleads, and the misreading is worth spelling out because it is
/// the obvious one. This is not "the mempool". rskj filters the pool down to
/// senders the node's own wallet manages
/// (`EthModuleWalletEnabled.ethPendingTransactions`):
///
/// ```java
/// List<Transaction> pendingTxs = transactionPool.getPendingTransactions();
/// List<String> managedAccounts = Arrays.asList(accounts());
/// return pendingTxs.stream()
///         .filter(tx -> managedAccounts.contains(tx.getSender(...).toJsonString()))
///         .collect(Collectors.toList());
/// ```
///
/// and with the wallet off it returns `Collections.emptyList()` unconditionally
/// (`EthModuleWalletDisabled`). rustock is that case: `eth_accounts` is empty,
/// `personal_*` is unsupported and `eth_sendTransaction` refuses. So `[]` is
/// the faithful answer, not a stub.
///
/// Returning the whole pending pool instead would be worse than useless. Under
/// rskj's contract these are *the caller's own* transactions, so a wallet or
/// frontend would present every transaction in the public mempool as the
/// user's — wrong in a way the caller cannot detect.
///
/// **What you probably want is `txpool_content`** (issue #83), which reports
/// the pool without claiming the transactions belong to anyone in particular.
pub fn eth_pending_transactions(id: Value) -> JsonRpcResponse {
    JsonRpcResponse::success(id, json!([]))
}

/// The address block rewards are paid to.
///
/// With mining off there is no such address and the zero address is the
/// honest answer. With mining on, answering zero would tell mining software
/// the rewards go nowhere, so the miner's configured address is reported --
/// which is what rskj does.
pub fn eth_coinbase(
    id: Value,
    miner: &Option<std::sync::Arc<dyn crate::mnr::MiningService>>,
) -> JsonRpcResponse {
    let addr = miner.as_ref().map(|m| m.coinbase()).unwrap_or([0u8; 20]);
    JsonRpcResponse::success(id, json!(format!("0x{}", hex::encode(addr))))
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

pub fn eth_get_uncle_by_block_hash_and_index(id: Value, params: &Value, store: &BlockStore) -> JsonRpcResponse {
    let Some(hash) = params.get(0).and_then(|v| v.as_str()).and_then(parse_b256) else {
        return JsonRpcResponse::error(id, INVALID_PARAMS, "Missing or invalid block hash");
    };
    let Some(index) = params.get(1).and_then(|v| v.as_str()).and_then(parse_hex_u32) else {
        return JsonRpcResponse::error(id, INVALID_PARAMS, "Missing or invalid index");
    };

    match uncle_dto(store, hash, index) {
        Some(dto) => JsonRpcResponse::success(id, serde_json::to_value(dto).unwrap()),
        None => JsonRpcResponse::success(id, Value::Null),
    }
}

pub fn eth_get_uncle_by_block_number_and_index(id: Value, params: &Value, store: &BlockStore) -> JsonRpcResponse {
    let Some(bn_str) = params.get(0).and_then(|v| v.as_str()) else {
        return JsonRpcResponse::error(id, INVALID_PARAMS, "Missing block number");
    };
    let head_num = head_number(store).unwrap_or(0);
    let Some(number) = parse_block_number(bn_str, head_num) else {
        return JsonRpcResponse::error(id, INVALID_PARAMS, "Invalid block number");
    };
    // The index is validated before the block is looked up, so a malformed
    // index is an error even when the block is unknown -- rskj rejects it in
    // parameter deserialisation, before the method body runs.
    let Some(index) = params.get(1).and_then(|v| v.as_str()).and_then(parse_hex_u32) else {
        return JsonRpcResponse::error(id, INVALID_PARAMS, "Missing or invalid index");
    };

    let hash = match store.canonical_hash(number) {
        Ok(Some(h)) => h,
        _ => return JsonRpcResponse::success(id, Value::Null),
    };

    match uncle_dto(store, hash, index) {
        Some(dto) => JsonRpcResponse::success(id, serde_json::to_value(dto).unwrap()),
        None => JsonRpcResponse::success(id, Value::Null),
    }
}

// -- internal helpers --------------------------------------------------------

/// One uncle of `block_hash`, rendered as a block. `None` means JSON `null`.
///
/// This mirrors rskj's `Web3Impl.getUncleResultDTO`, which has two behaviours
/// that are not the obvious ones:
///
/// 1. **An out-of-range index is `null`, not an error.** So is an unknown
///    parent block. Only a malformed index is an error, and that is rejected
///    earlier, during parameter parsing.
///
/// 2. **The uncle is rendered with its own body when the node happens to have
///    that block.** rskj calls `blockchain.getBlockByHash(uncleHeader.getHash())`
///    first and only falls back to `Block.createBlockFromHeader` -- an empty
///    block synthesised from the header -- when the lookup misses. An uncle
///    was a real block on a competing branch, so if this node downloaded that
///    branch it has the transactions, and rskj returns them.
///
///    go-ethereum does not do this: it always builds `NewBlockWithHeader(uncle)`
///    and so always answers with empty `transactions` and `uncles`. The
///    consequence is that on RSK the same call against two honest nodes can
///    return different `transactions` and `size` for the same uncle, depending
///    on what each node stored. See `docs/rskj-vs-geth.md`.
///
/// `totalDifficulty` gets the same treatment by accident of rskj's storage
/// layer: `getTotalDifficultyForHash` returns ZERO for a hash it does not
/// have, which is what an unstored uncle hits, and `unwrap_or_default()` here
/// gives the same `0x0`.
fn uncle_dto(
    store: &BlockStore,
    block_hash: alloy_primitives::B256,
    index: u32,
) -> Option<BlockResultDto> {
    // The uncle *headers* live in the parent's body. No body means the node
    // has at most a bare header for this block, which cannot name its uncles.
    let (_, ommers) = store.body(block_hash).ok().flatten()?;
    let uncle_header = ommers.get(index as usize)?;
    let uncle_hash = uncle_header.hash();

    let uncle_body = store.body(uncle_hash).ok().flatten();
    let td = store.total_difficulty(uncle_hash).ok().flatten().unwrap_or_default();

    Some(BlockResultDto::from_header_with_body(
        uncle_header,
        uncle_hash,
        td,
        uncle_body.as_ref(),
        false,
    ))
}


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

/// `eth_bridgeState` — two fields, not the whole Bridge.
///
/// The name and the `BridgeState` class both suggest a full dump. They are
/// misleading: `BridgeState` *holds* the UTXO set, the federation, the release
/// request queue and the pegouts waiting for confirmations, but
/// `stateToMap()` — which is what the RPC returns — exposes only two of them:
///
/// ```java
/// public Map<String, Object> stateToMap() {
///     Map<String, Object> result = new HashMap<>();
///     result.put("rskTxsWaitingForSignatures", this.toStringList(rskTxsWaitingForSignatures.keySet()));
///     result.put("btcBlockchainBestChainHeight", this.btcBlockchainBestChainHeight);
///     return result;
/// }
/// ```
///
/// The rest is only in `getEncoded()`, which the RPC never calls.
///
/// Two further details that are easy to get wrong:
///
/// * **No block parameter.** rskj reads `blockchain.getBestBlock()`
///   unconditionally (`EthModule.bridgeState`). A caller cannot ask about a
///   historical block, so neither does this.
/// * **The hashes carry no `0x` prefix.** `Keccak256.toHexString()` is
///   `Hex.toHexString(bytes)`, which is bare hex — unusual for JSON-RPC, where
///   almost everything else is prefixed, and the sort of difference a consumer
///   discovers by failing to parse.
pub fn eth_bridge_state(id: Value, state: &crate::server::RpcState) -> JsonRpcResponse {
    use rustock_execution::bridge::{btc_store, peg, storage as bridge_storage};

    let Some((root, _header)) = crate::state::resolve_state_root("latest", state) else {
        return JsonRpcResponse::error(id, INTERNAL_ERROR, "Cannot resolve the best block");
    };
    let Some(trie_store) = state.trie_store.clone() else {
        return JsonRpcResponse::error(id, INTERNAL_ERROR, "No state available");
    };

    let mut ctx = rustock_execution::bridge_read_context(trie_store, root, None);

    let height = btc_store::load_chain_head(&mut ctx).map(|h| h.height).unwrap_or(0);

    // Keys only: rskj maps `rskTxsWaitingForSignatures.keySet()`, discarding the
    // Bitcoin transactions themselves.
    let key = bridge_storage::bridge_storage_key(
        bridge_storage::PEGOUTS_WAITING_FOR_SIGNATURES_KEY,
    );
    let raw = bridge_storage::bridge_load_raw(&mut ctx, key).unwrap_or_default();
    let waiting = peg::deserialize_rsk_txs_waiting_for_signatures(&raw);
    let hashes: Vec<Value> = waiting.keys().map(|h| json!(hex::encode(h))).collect();

    JsonRpcResponse::success(
        id,
        json!({
            "rskTxsWaitingForSignatures": hashes,
            "btcBlockchainBestChainHeight": height,
        }),
    )
}
