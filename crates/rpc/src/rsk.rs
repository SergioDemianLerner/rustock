use crate::helpers::{parse_b256, parse_block_number, to_hex_bytes};
use crate::types::*;
use alloy_rlp::Encodable;
use rustock_storage::BlockStore;
use serde_json::{json, Value};

pub fn rsk_protocol_version(id: Value) -> JsonRpcResponse {
    JsonRpcResponse::success(id, json!("0x1"))
}

pub fn rsk_get_raw_block_header_by_hash(
    id: Value,
    params: &Value,
    store: &BlockStore,
) -> JsonRpcResponse {
    let hash_str = match params.get(0).and_then(|v| v.as_str()) {
        Some(s) => s,
        None => return JsonRpcResponse::error(id, INVALID_PARAMS, "Missing block hash"),
    };
    let hash = match parse_b256(hash_str) {
        Some(h) => h,
        None => return JsonRpcResponse::error(id, INVALID_PARAMS, "Invalid block hash"),
    };

    match store.header(hash) {
        Ok(Some(header)) => {
            let mut buf = Vec::new();
            header.encode(&mut buf);
            JsonRpcResponse::success(id, json!(to_hex_bytes(&buf)))
        }
        _ => JsonRpcResponse::success(id, Value::Null),
    }
}

pub fn rsk_get_raw_block_header_by_number(
    id: Value,
    params: &Value,
    store: &BlockStore,
) -> JsonRpcResponse {
    let bn_str = match params.get(0).and_then(|v| v.as_str()) {
        Some(s) => s,
        None => return JsonRpcResponse::error(id, INVALID_PARAMS, "Missing block number"),
    };

    let head_num = store
        .head()
        .ok()
        .flatten()
        .and_then(|h| store.header(h).ok().flatten())
        .map(|h| h.number)
        .unwrap_or(0);

    let number = match parse_block_number(bn_str, head_num) {
        Some(n) => n,
        None => return JsonRpcResponse::error(id, INVALID_PARAMS, "Invalid block number"),
    };

    let hash = match store.canonical_hash(number) {
        Ok(Some(h)) => h,
        _ => return JsonRpcResponse::success(id, Value::Null),
    };

    match store.header(hash) {
        Ok(Some(header)) => {
            let mut buf = Vec::new();
            header.encode(&mut buf);
            JsonRpcResponse::success(id, json!(to_hex_bytes(&buf)))
        }
        _ => JsonRpcResponse::success(id, Value::Null),
    }
}

/// `rsk_getStorageBytesAt` — the variable-length value at a storage key.
///
/// `eth_getStorageAt` can only return one 32-byte word. rskj stores some
/// precompile state as a single variable-length value at one unitrie key
/// (`Repository.addStorageBytes` / `getStorageBytes`), and the Bridge's state is
/// largely of that shape: the BTC chain head, the federation, the release
/// request queue, the UTXO set. None of it is reachable through
/// `eth_getStorageAt`, which hands back the first word and silently drops the
/// rest.
///
/// # The three answers, which are not two
///
/// rskj distinguishes an empty value from an absent one, and so must this
/// (`Web3Impl.rsk_getStorageBytesAt` with `HexUtils.toUnformattedJsonHex`):
///
/// | case | answer |
/// |---|---|
/// | present, non-empty | `0x` + the bytes |
/// | present but **empty** | `0x` |
/// | **absent** | `0x0` |
///
/// `0x` and `0x0` look alike and are not: the first says the key holds a
/// zero-length value, the second that the key is not there. Collapsing them
/// would make "the queue is empty" indistinguishable from "there is no queue",
/// which for Bridge state are different facts.
///
/// Note that rustock's own writer treats an empty write as a delete
/// (`RawStorage::put`), so the middle row is not reachable through this node's
/// execution path -- it is honoured here because the format is rskj's, not
/// because rustock can produce it.
pub fn rsk_get_storage_bytes_at(
    id: Value,
    params: &Value,
    state: &crate::server::RpcState,
) -> JsonRpcResponse {
    use alloy_primitives::{Address, B256};
    use rustock_trie::{storage_key, TrieKeySlice};

    let Some(addr) = params
        .get(0)
        .and_then(|v| v.as_str())
        .and_then(|s| s.parse::<Address>().ok())
    else {
        return JsonRpcResponse::error(id, INVALID_PARAMS, "Missing or invalid address");
    };

    // rskj takes a DataWord via `strHexOrStrNumberToByteArray`, which accepts a
    // short hex string as well as a full 32-byte one and left-pads it.
    let Some(slot) = params.get(1).and_then(|v| v.as_str()).and_then(parse_storage_key) else {
        return JsonRpcResponse::error(id, INVALID_PARAMS, "Missing or invalid storage key");
    };

    let block_param = params.get(2).and_then(|v| v.as_str()).unwrap_or("latest");
    let Some((root, _header)) = crate::state::resolve_state_root(block_param, state) else {
        return JsonRpcResponse::error(id, INTERNAL_ERROR, "Cannot resolve block");
    };

    let Some(trie_store) = state.trie_store.clone() else {
        // No state to read: absent, which is the honest answer rather than an
        // empty value.
        return JsonRpcResponse::success(id, json!("0x0"));
    };

    let key = storage_key(&addr, &slot);
    match root.get(&TrieKeySlice::from_key(&key), &*trie_store) {
        Some(data) => JsonRpcResponse::success(id, json!(to_hex_bytes(&data))),
        None => JsonRpcResponse::success(id, json!("0x0")),
    }
}

/// A storage key, accepting the short forms rskj does.
///
/// `DataWord.valueOf(HexUtils.strHexOrStrNumberToByteArray(s))` takes `0x1` as
/// readily as a full 32-byte word and left-pads to 32 bytes. A caller who
/// writes the Bridge's key as a short hex string should not get a parse error
/// where rskj would answer.
fn parse_storage_key(s: &str) -> Option<alloy_primitives::B256> {
    let hex_part = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")).unwrap_or(s);
    if hex_part.is_empty() || hex_part.len() > 64 || !hex_part.bytes().all(|b| b.is_ascii_hexdigit())
    {
        return None;
    }
    let padded = format!("{:0>64}", hex_part);
    let bytes = hex::decode(padded).ok()?;
    Some(alloy_primitives::B256::from_slice(&bytes))
}

/// `rsk_getRawTransactionReceiptByHash` — the receipt's raw RLP.
///
/// `eth_getTransactionReceipt` returns a decoded receipt you have to *trust*.
/// This returns the bytes that actually went into the receipts trie, so a
/// caller who already trusts the block's `receiptsRoot` can *check* it —
/// together with [`rsk_get_transaction_receipt_nodes_by_hash`], which supplies
/// the path.
///
/// Main chain only, matching rskj's `receiptStore.getInMainChain`: a receipt
/// from a block that lost a reorg proves nothing against the canonical root.
/// `null` when the transaction is unknown.
pub fn rsk_get_raw_transaction_receipt_by_hash(
    id: Value,
    params: &Value,
    store: &BlockStore,
) -> JsonRpcResponse {
    let Some(tx_hash) = params.get(0).and_then(|v| v.as_str()).and_then(parse_b256) else {
        return JsonRpcResponse::error(id, INVALID_PARAMS, "Missing or invalid transaction hash");
    };

    let Ok(Some((block_hash, index))) = store.tx_location(tx_hash) else {
        return JsonRpcResponse::success(id, Value::Null);
    };

    // `tx_location` indexes the canonical chain, which is rskj's
    // `getInMainChain`; re-checking the canonical pointer keeps that true if
    // the index is ever widened to cover forks.
    let on_main_chain = store
        .header(block_hash)
        .ok()
        .flatten()
        .and_then(|h| store.canonical_hash(h.number).ok().flatten())
        .is_some_and(|canonical| canonical == block_hash);
    if !on_main_chain {
        return JsonRpcResponse::success(id, Value::Null);
    }

    let Ok(Some(receipts)) = store.receipts(block_hash) else {
        return JsonRpcResponse::success(id, Value::Null);
    };
    let Some(receipt) = receipts.get(index as usize) else {
        return JsonRpcResponse::success(id, Value::Null);
    };

    JsonRpcResponse::success(id, json!(to_hex_bytes(&receipt.rlp_encode())))
}

/// `rsk_getTransactionReceiptNodesByHash` — the trie nodes proving a receipt.
///
/// Given these plus the receipt's RLP, a light client or a bridge on another
/// chain can verify the receipt against a `receiptsRoot` it already trusts,
/// without the block, without the state, and without trusting the node that
/// served it.
///
/// # Ordering: leaf first, root last
///
/// Not the obvious order, and worth stating because a consumer that assumes
/// root-first silently fails to verify. rskj's `Trie.findNodes` appends each
/// level *after* recursing into its child:
///
/// ```text
///   List<Trie> subnodes = node.findNodes(...);
///   subnodes.add(this);          // <- this node goes AFTER its child's list
/// ```
///
/// so the deepest node comes back first and the root last. `nodes_on_path`
/// mirrors that.
///
/// The trie is rebuilt from the block's receipts on demand, exactly as rskj
/// does — the nodes are not stored.
pub fn rsk_get_transaction_receipt_nodes_by_hash(
    id: Value,
    params: &Value,
    store: &BlockStore,
) -> JsonRpcResponse {
    use rustock_core::types::receipt::{build_receipts_trie, receipt_trie_key};
    use rustock_trie::{MemoryTrieStore, TrieKeySlice, TrieStore};

    let Some(block_hash) = params.get(0).and_then(|v| v.as_str()).and_then(parse_b256) else {
        return JsonRpcResponse::error(id, INVALID_PARAMS, "Missing or invalid block hash");
    };
    let Some(tx_hash) = params.get(1).and_then(|v| v.as_str()).and_then(parse_b256) else {
        return JsonRpcResponse::error(id, INVALID_PARAMS, "Missing or invalid transaction hash");
    };

    let Ok(Some((transactions, _ommers))) = store.body(block_hash) else {
        return JsonRpcResponse::success(id, Value::Null);
    };

    // The transaction has to be in *this* block. rskj returns null when the
    // index search never matches, and a proof against another block's root
    // would be meaningless.
    let Some(index) = transactions.iter().position(|tx| tx.tx_hash() == tx_hash) else {
        return JsonRpcResponse::success(id, Value::Null);
    };

    let Ok(Some(receipts)) = store.receipts(block_hash) else {
        return JsonRpcResponse::success(id, Value::Null);
    };
    if receipts.len() != transactions.len() {
        // A partial receipt set would build a different trie and produce a
        // proof that cannot verify. Refuse rather than answer wrongly.
        return JsonRpcResponse::success(id, Value::Null);
    }

    let mem = MemoryTrieStore::new();
    let root = build_receipts_trie(&receipts, &mem);
    let key = TrieKeySlice::from_key(&receipt_trie_key(index as u32));

    let Some(nodes) = root.nodes_on_path(&key, &mem) else {
        return JsonRpcResponse::success(id, Value::Null);
    };

    let encoded: Vec<Value> = nodes
        .iter()
        .map(|n| json!(to_hex_bytes(&n.to_message(&mem as &dyn TrieStore))))
        .collect();
    JsonRpcResponse::success(id, Value::Array(encoded))
}
