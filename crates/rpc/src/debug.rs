//! The `debug_*` namespace.
//!
//! Shapes come from rskj's `Web3DebugModule` / `DebugModuleImpl`, not from
//! go-ethereum, which has different methods here entirely. Differences worth
//! knowing are in `docs/debug-namespace.md`.

use crate::server::RpcState;
use crate::types::*;
use serde_json::{json, Value};

/// `debug_wireProtocolQueueSize()` → a hex quantity **string**.
///
/// rskj: `HexUtils.toQuantityJsonHex(messageHandler.getMessageQueueSize())` --
/// inbound messages received from peers and not yet handled. The equivalent
/// here is the depth of the `SyncEvent` channel, which is where inbound wire
/// work waits for this node.
///
/// A string rather than a number, because that is what `toQuantityJsonHex`
/// produces; `txpool_status`'s counts in the same server are JSON numbers,
/// because rskj builds those with `numberNode`. The inconsistency is rskj's.
pub fn debug_wire_protocol_queue_size(id: Value, state: &RpcState) -> JsonRpcResponse {
    let depth = state
        .wire_queue_depth
        .as_ref()
        .map(|d| d.load(std::sync::atomic::Ordering::Relaxed))
        .unwrap_or(0);
    JsonRpcResponse::success(id, json!(format!("0x{depth:x}")))
}

/// `debug_accountTransactionQuota(address)` → `{timestamp, availableVirtualGas}`,
/// or `null` for an address the limiter is not tracking.
///
/// rskj returns `txQuotaChecker.getTxQuota(address)`, a straight map lookup
/// that yields `null` when the address has no entry -- which is the normal
/// answer, because an entry is only created when an account's transaction is
/// admitted. Serialised, `TxQuota` exposes exactly two `@JsonProperty` fields,
/// `timestamp` (epoch milliseconds) and `availableVirtualGas` (a double).
///
/// The double is deliberate and must not be rounded: the virtual-gas cost of a
/// transaction is a product of six fractional factors, so a quota is rarely a
/// whole number and the fractional part is what separates one admitted
/// transaction from the next.
pub fn debug_account_transaction_quota(id: Value, params: &Value, state: &RpcState) -> JsonRpcResponse {
    let Some(address) = params
        .get(0)
        .and_then(|v| v.as_str())
        .and_then(|s| s.parse::<alloy_primitives::Address>().ok())
    else {
        return JsonRpcResponse::error(id, INVALID_PARAMS, "Missing or invalid address");
    };

    let Some(pool) = &state.tx_pool else {
        return crate::server::execution_not_available(id, "debug_accountTransactionQuota");
    };

    match pool.quota_report(&address) {
        Some((available_virtual_gas, timestamp)) => JsonRpcResponse::success(
            id,
            json!({ "timestamp": timestamp, "availableVirtualGas": available_virtual_gas }),
        ),
        None => JsonRpcResponse::success(id, Value::Null),
    }
}
