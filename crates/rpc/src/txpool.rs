//! `txpool_content`, `txpool_inspect` and `txpool_status`.
//!
//! Every shape here comes from rskj's `TxPoolModuleImpl`, which differs from
//! go-ethereum's txpool namespace in four ways that a consumer written against
//! geth would get wrong. They are listed in `docs/rskj-vs-geth.md`; the ones
//! that bite hardest are that **sender keys carry no `0x` prefix** and that
//! **each nonce maps to an array of transactions, not a transaction**.

use crate::server::RpcState;
use crate::types::*;
use alloy_primitives::{Address, U256};
use rustock_core::Transaction;
use serde_json::{json, Map, Value};

pub fn txpool_content(id: Value, state: &RpcState) -> JsonRpcResponse {
    let Some(pool) = &state.tx_pool else {
        return crate::server::execution_not_available(id, "txpool_content");
    };
    let content = pool.pool_content();
    JsonRpcResponse::success(
        id,
        json!({
            "pending": serialize_group(&content.pending, full_serializer),
            "queued": serialize_group(&content.queued, full_serializer),
        }),
    )
}

pub fn txpool_inspect(id: Value, state: &RpcState) -> JsonRpcResponse {
    let Some(pool) = &state.tx_pool else {
        return crate::server::execution_not_available(id, "txpool_inspect");
    };
    let content = pool.pool_content();
    JsonRpcResponse::success(
        id,
        json!({
            "pending": serialize_group(&content.pending, summary_serializer),
            "queued": serialize_group(&content.queued, summary_serializer),
        }),
    )
}

/// Counts of pending and queued transactions.
///
/// **As JSON numbers**, not hex strings. rskj emits
/// `jsonNodeFactory.numberNode(size)`; go-ethereum emits `hexutil.Uint`, i.e.
/// `"0x2"`. This used to answer geth's way, so a caller doing arithmetic on
/// the result got a string. The two namespaces cannot both be right and this
/// node follows rskj.
pub fn txpool_status(id: Value, state: &RpcState) -> JsonRpcResponse {
    let Some(pool) = &state.tx_pool else {
        return crate::server::execution_not_available(id, "txpool_status");
    };
    let (pending, queued) = pool.pool_status();
    JsonRpcResponse::success(id, json!({ "pending": pending, "queued": queued }))
}

/// `{ sender: { nonce: [tx, ...] } }`.
///
/// Two shapes here are rskj's and not the Ethereum convention:
///
/// * **The sender key has no `0x` prefix.** rskj keys the map with
///   `RskAddress.toString()`, which is `ByteUtil.toHexString(bytes)` -- bare
///   hex. go-ethereum uses `common.Address.Hex()`, which is `0x` + EIP-55
///   mixed case. A consumer written for geth looks up `"0xab…"` in a map whose
///   keys are `"ab…"` and finds nothing, for every sender.
/// * **The value at a nonce is an array.** rskj builds an `ArrayNode` even
///   when it holds one transaction; geth puts the transaction object there
///   directly. This pool holds at most one transaction per (sender, nonce), so
///   the array here always has exactly one element -- but emitting the bare
///   object instead would break every consumer that indexes `[0]`.
///
/// The nonce key is decimal, which *is* what both do.
fn serialize_group<F>(group: &[(Address, Vec<(u64, Transaction)>)], serializer: F) -> Value
where
    F: Fn(&Address, u64, &Transaction) -> Value,
{
    let mut senders = Map::new();
    for (sender, txs) in group {
        let mut nonces = Map::new();
        for (nonce, tx) in txs {
            nonces.insert(
                nonce.to_string(),
                Value::Array(vec![serializer(sender, *nonce, tx)]),
            );
        }
        senders.insert(hex::encode(sender.as_slice()), Value::Object(nonces));
    }
    Value::Object(senders)
}

/// rskj's `fullSerializer`. Eleven fields; no `v`, `r`, `s`, `type` or
/// `chainId`, which geth includes.
///
/// `blockHash` is the **zero hash, not null** -- an unmined transaction is in
/// no block, and rskj says so with 32 zero bytes while `blockNumber` and
/// `transactionIndex` beside it are null.
fn full_serializer(sender: &Address, nonce: u64, tx: &Transaction) -> Value {
    json!({
        "blockHash": "0x0000000000000000000000000000000000000000000000000000000000000000",
        "blockNumber": Value::Null,
        "transactionIndex": Value::Null,
        "from": format!("{:#x}", sender),
        "gas": format!("0x{:x}", tx.gas_limit),
        "gasPrice": to_json_hex(&java_bigint_bytes(&tx.gas_price)),
        "hash": format!("{:#x}", tx.tx_hash()),
        "input": format!("0x{}", hex::encode(&tx.input)),
        "nonce": format!("0x{:x}", nonce),
        "to": to_json_hex(&tx.to),
        "value": to_json_hex(&java_bigint_bytes(&tx.value)),
    })
}

/// rskj's `summarySerializer`:
/// `String.format("%s: %s wei + %d x %s gas", to, value, gasLimit, gasPrice)`.
///
/// Note against geth's `"%s: %v wei + %v gas × %v wei"`: a plain ASCII `x`
/// rather than `×`, the gas limit and gas price in the opposite order, and the
/// word `gas` at the end instead of after the limit. The address is bare hex
/// here too, and the three numbers are decimal.
///
/// A contract creation has no recipient, so the string begins with `": "` --
/// `RskAddress.nullAddress().toString()` is the empty string.
fn summary_serializer(_sender: &Address, _nonce: u64, tx: &Transaction) -> Value {
    Value::String(format!(
        "{}: {} wei + {} x {} gas",
        hex::encode(&tx.to),
        tx.value,
        tx.gas_limit,
        tx.gas_price,
    ))
}

/// rskj's `HexUtils.toJsonHex(byte[])`: `0x` + every byte as given, with **no
/// leading-zero stripping**, and `0x00` for an empty array.
///
/// This is not a JSON-RPC quantity, and the difference shows: `gas` and
/// `nonce` in the same object go through `toQuantityJsonHex` and are
/// canonical, while `value` and `gasPrice` come through here and can carry a
/// leading zero byte. A contract creation's `to` is the empty address, so it
/// renders as `0x00` rather than null.
fn to_json_hex(bytes: &[u8]) -> String {
    if bytes.is_empty() {
        return "0x00".to_string();
    }
    format!("0x{}", hex::encode(bytes))
}

/// `BigInteger.toByteArray()` for a non-negative value: big-endian, shortest
/// form, **plus a leading `0x00` when the top bit of the first byte is set**
/// because Java's encoding is two's-complement and would otherwise read as
/// negative. Zero is `[0x00]`, not empty.
///
/// This is why a `value` of `0xff` appears as `0x00ff` in `txpool_content`
/// while the same amount is `0xff` everywhere else in the API.
fn java_bigint_bytes(v: &U256) -> Vec<u8> {
    let be: [u8; 32] = v.to_be_bytes();
    let first = be.iter().position(|b| *b != 0).unwrap_or(32);
    if first == 32 {
        return vec![0];
    }
    let mut out = Vec::with_capacity(33 - first);
    if be[first] & 0x80 != 0 {
        out.push(0);
    }
    out.extend_from_slice(&be[first..]);
    out
}

