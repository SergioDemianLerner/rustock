use crate::dto::LogDto;
use crate::helpers::{parse_b256, parse_block_number, to_hex_b256, to_hex_u64};
use crate::server::RpcState;
use crate::types::*;
use alloy_primitives::{Address, B256};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

const MAX_BLOCK_RANGE: u64 = 10_000;

/// In-memory filter storage for eth_newFilter / eth_getFilterChanges.
#[derive(Clone)]
pub struct FilterStore {
    filters: Arc<RwLock<HashMap<u64, Filter>>>,
    next_id: Arc<AtomicU64>,
}

impl Default for FilterStore {
    fn default() -> Self {
        Self::new()
    }
}

impl FilterStore {
    pub fn new() -> Self {
        Self {
            filters: Arc::new(RwLock::new(HashMap::new())),
            next_id: Arc::new(AtomicU64::new(1)),
        }
    }

    fn insert(&self, filter: Filter) -> u64 {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.filters.write().unwrap().insert(id, filter);
        id
    }

    fn get(&self, id: u64) -> Option<Filter> {
        self.filters.read().unwrap().get(&id).cloned()
    }

    fn update_last_polled(&self, id: u64, block: u64) {
        if let Some(f) = self.filters.write().unwrap().get_mut(&id) {
            f.last_polled_block = block;
        }
    }

    fn remove(&self, id: u64) -> bool {
        self.filters.write().unwrap().remove(&id).is_some()
    }
}

#[derive(Clone, Debug)]
enum FilterKind {
    Log(LogFilter),
    Block,
    #[allow(dead_code)]
    PendingTransaction,
}

#[derive(Clone, Debug)]
struct Filter {
    kind: FilterKind,
    last_polled_block: u64,
}

#[derive(Clone, Debug)]
struct LogFilter {
    from_block: Option<u64>,
    to_block: Option<u64>,
    addresses: Vec<Address>,
    topics: Vec<Option<Vec<B256>>>,
}

pub fn eth_get_logs(id: Value, params: &Value, state: &RpcState) -> JsonRpcResponse {
    let Some(filter_obj) = params.get(0) else {
        return JsonRpcResponse::error(id, INVALID_PARAMS, "Missing filter object");
    };

    let head_num = head_number(&state.store).unwrap_or(0);
    let filter = parse_log_filter(filter_obj, head_num);

    let from = filter.from_block.unwrap_or(head_num);
    let to = filter.to_block.unwrap_or(head_num);

    if to > from + MAX_BLOCK_RANGE {
        return JsonRpcResponse::error(id, INVALID_PARAMS, "Block range exceeds maximum (10000)");
    }

    let logs = collect_logs(from, to, &filter, state);
    JsonRpcResponse::success(id, serde_json::to_value(&logs).unwrap())
}

pub fn eth_new_filter(id: Value, params: &Value, state: &RpcState) -> JsonRpcResponse {
    let Some(filter_obj) = params.get(0) else {
        return JsonRpcResponse::error(id, INVALID_PARAMS, "Missing filter object");
    };

    let head_num = head_number(&state.store).unwrap_or(0);
    let log_filter = parse_log_filter(filter_obj, head_num);

    let filter = Filter {
        kind: FilterKind::Log(log_filter),
        last_polled_block: head_num,
    };

    let filter_id = state.filter_store.insert(filter);
    JsonRpcResponse::success(id, json!(to_hex_u64(filter_id)))
}

pub fn eth_new_block_filter(id: Value, state: &RpcState) -> JsonRpcResponse {
    let head_num = head_number(&state.store).unwrap_or(0);
    let filter = Filter {
        kind: FilterKind::Block,
        last_polled_block: head_num,
    };
    let filter_id = state.filter_store.insert(filter);
    JsonRpcResponse::success(id, json!(to_hex_u64(filter_id)))
}

pub fn eth_new_pending_transaction_filter(id: Value) -> JsonRpcResponse {
    // No mempool support — return a dummy filter that always returns empty
    JsonRpcResponse::success(id, json!("0x0"))
}

pub fn eth_get_filter_changes(id: Value, params: &Value, state: &RpcState) -> JsonRpcResponse {
    let Some(filter_id) = params.get(0).and_then(|v| v.as_str()).and_then(parse_hex_u64) else {
        return JsonRpcResponse::error(id, INVALID_PARAMS, "Missing or invalid filter id");
    };

    if filter_id == 0 {
        return JsonRpcResponse::success(id, json!([]));
    }

    let Some(filter) = state.filter_store.get(filter_id) else {
        return JsonRpcResponse::error(id, INVALID_PARAMS, "Filter not found");
    };

    let head_num = head_number(&state.store).unwrap_or(0);

    match &filter.kind {
        FilterKind::Log(log_filter) => {
            let from = filter.last_polled_block + 1;
            let to = head_num;
            if from > to {
                return JsonRpcResponse::success(id, json!([]));
            }
            let logs = collect_logs(from, to, log_filter, state);
            state.filter_store.update_last_polled(filter_id, head_num);
            JsonRpcResponse::success(id, serde_json::to_value(&logs).unwrap())
        }
        FilterKind::Block => {
            let from = filter.last_polled_block + 1;
            let to = head_num;
            let mut hashes = Vec::new();
            for num in from..=to {
                if let Some(hash) = state.store.canonical_hash(num).ok().flatten() {
                    hashes.push(to_hex_b256(&hash));
                }
            }
            state.filter_store.update_last_polled(filter_id, head_num);
            JsonRpcResponse::success(id, json!(hashes))
        }
        FilterKind::PendingTransaction => {
            JsonRpcResponse::success(id, json!([]))
        }
    }
}

pub fn eth_get_filter_logs(id: Value, params: &Value, state: &RpcState) -> JsonRpcResponse {
    let Some(filter_id) = params.get(0).and_then(|v| v.as_str()).and_then(parse_hex_u64) else {
        return JsonRpcResponse::error(id, INVALID_PARAMS, "Missing or invalid filter id");
    };

    let Some(filter) = state.filter_store.get(filter_id) else {
        return JsonRpcResponse::error(id, INVALID_PARAMS, "Filter not found");
    };

    match &filter.kind {
        FilterKind::Log(log_filter) => {
            let head_num = head_number(&state.store).unwrap_or(0);
            let from = log_filter.from_block.unwrap_or(head_num);
            let to = log_filter.to_block.unwrap_or(head_num);
            let logs = collect_logs(from, to, log_filter, state);
            JsonRpcResponse::success(id, serde_json::to_value(&logs).unwrap())
        }
        _ => JsonRpcResponse::success(id, json!([])),
    }
}

pub fn eth_uninstall_filter(id: Value, params: &Value, state: &RpcState) -> JsonRpcResponse {
    let Some(filter_id) = params.get(0).and_then(|v| v.as_str()).and_then(parse_hex_u64) else {
        return JsonRpcResponse::error(id, INVALID_PARAMS, "Missing or invalid filter id");
    };
    let removed = state.filter_store.remove(filter_id);
    JsonRpcResponse::success(id, json!(removed))
}

// --- internal helpers ---

fn collect_logs(from: u64, to: u64, filter: &LogFilter, state: &RpcState) -> Vec<LogDto> {
    let mut result = Vec::new();
    // Only worth reading a header to test its bloom when the query actually
    // constrains something. An unfiltered `eth_getLogs` wants every log, so
    // the test could never skip a block and would be a read per block for
    // nothing.
    let use_bloom = filter_constrains_anything(filter);

    for num in from..=to {
        let Some(block_hash) = state.store.canonical_hash(num).ok().flatten() else {
            continue;
        };

        // The cheap rejection: one header read and three bit tests per
        // filtered term, against a receipt list that may hold hundreds of
        // logs. The header's `logs_bloom` is the OR of its receipts' blooms
        // and is validated against the recomputed value when the block is
        // executed (`ProcessError::LogsBloomMismatch`), so it can be trusted
        // to be complete.
        if use_bloom {
            if let Some(header) = state.store.header(block_hash).ok().flatten() {
                if !bloom_may_match(&header.logs_bloom, filter) {
                    continue;
                }
            }
        }

        let Some(receipts) = state.store.receipts(block_hash).ok().flatten() else {
            continue;
        };
        let transactions = state.store.body(block_hash).ok().flatten()
            .map(|(txs, _)| txs)
            .unwrap_or_default();

        let mut global_log_index = 0u32;
        for (tx_idx, receipt) in receipts.iter().enumerate() {
            let tx_hash = transactions.get(tx_idx)
                .map(compute_tx_hash)
                .unwrap_or(B256::ZERO);

            for log in &receipt.logs {
                if matches_filter(log, filter) {
                    result.push(LogDto::from_log(
                        log,
                        block_hash,
                        num,
                        tx_hash,
                        tx_idx as u32,
                        global_log_index,
                    ));
                }
                global_log_index += 1;
            }
        }
    }

    result
}

/// Does this query name anything a bloom could be tested against?
fn filter_constrains_anything(filter: &LogFilter) -> bool {
    !filter.addresses.is_empty()
        || filter.topics.iter().flatten().any(|allowed| !allowed.is_empty())
}

/// Could a block with this logs bloom hold a log matching `filter`?
///
/// **`false` is certain, `true` is a maybe.** A bloom filter has no false
/// negatives, so a `false` here means no log in the block can match and the
/// block is safe to skip without reading its receipts. A `true` proves
/// nothing and every candidate is still confirmed by `matches_filter`.
///
/// The logic mirrors `matches_filter`: addresses are OR-ed, topic positions
/// are AND-ed with the alternatives at each position OR-ed.
fn bloom_may_match(bloom: &alloy_primitives::Bloom, filter: &LogFilter) -> bool {
    use rustock_core::bloom::may_contain;

    if !filter.addresses.is_empty()
        && !filter.addresses.iter().any(|a| may_contain(bloom, a.as_slice()))
    {
        return false;
    }

    for allowed in filter.topics.iter().flatten() {
        // An empty alternatives list at a position means "the log must have
        // **no** topic here" (see `matches_filter`). There is nothing to look
        // up, so the bloom says nothing about it and the block must be read.
        if allowed.is_empty() {
            continue;
        }
        if !allowed.iter().any(|t| may_contain(bloom, t.as_slice())) {
            return false;
        }
    }

    true
}

fn matches_filter(log: &rustock_core::Log, filter: &LogFilter) -> bool {
    if !filter.addresses.is_empty() && !filter.addresses.contains(&log.address) {
        return false;
    }

    for (i, topic_filter) in filter.topics.iter().enumerate() {
        if let Some(allowed) = topic_filter {
            match log.topics.get(i) {
                Some(t) if allowed.contains(t) => {}
                None if allowed.is_empty() => {}
                _ => return false,
            }
        }
    }

    true
}

fn parse_log_filter(obj: &Value, head_num: u64) -> LogFilter {
    let from_block = obj.get("fromBlock")
        .and_then(|v| v.as_str())
        .and_then(|s| parse_block_number(s, head_num));

    let to_block = obj.get("toBlock")
        .and_then(|v| v.as_str())
        .and_then(|s| parse_block_number(s, head_num));

    let addresses = parse_address_filter(obj.get("address"));

    let topics = obj.get("topics")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter().map(|t| {
                if t.is_null() {
                    None
                } else if let Some(s) = t.as_str() {
                    parse_b256(s).map(|h| vec![h])
                } else if let Some(arr) = t.as_array() {
                    let topics: Vec<B256> = arr.iter()
                        .filter_map(|v| v.as_str().and_then(parse_b256))
                        .collect();
                    if topics.is_empty() { None } else { Some(topics) }
                } else {
                    None
                }
            }).collect()
        })
        .unwrap_or_default();

    LogFilter { from_block, to_block, addresses, topics }
}

fn parse_address_filter(v: Option<&Value>) -> Vec<Address> {
    match v {
        None => vec![],
        Some(Value::String(s)) => s.parse::<Address>().ok().into_iter().collect(),
        Some(Value::Array(arr)) => arr.iter()
            .filter_map(|v| v.as_str()?.parse::<Address>().ok())
            .collect(),
        _ => vec![],
    }
}

/// The canonical transaction hash -- see `helpers::tx_hash`. A log's
/// `transactionHash` must match what the transaction is indexed under, or a
/// caller cannot fetch the transaction a log came from.
fn compute_tx_hash(tx: &rustock_core::Transaction) -> B256 {
    tx.tx_hash()
}

fn head_number(store: &rustock_storage::BlockStore) -> Option<u64> {
    store.head().ok()?
        .and_then(|h| store.header(h).ok().flatten())
        .map(|h| h.number)
}

fn parse_hex_u64(s: &str) -> Option<u64> {
    let s = s.strip_prefix("0x").unwrap_or(s);
    u64::from_str_radix(s, 16).ok()
}
