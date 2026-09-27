//! The `evm_*` namespace: driving a development chain from a test suite.
//!
//! This is how contract test suites use a node. They snapshot before each
//! test, run it, and revert; they mine on demand rather than waiting for a
//! block; they move the clock to exercise time-dependent logic. A suite
//! written against rskj expects these seven methods and fails on the first
//! call if they are absent.
//!
//! Ported from rskj's `EvmModuleImpl` and `SnapshotManager`.
//!
//! # A snapshot is a block height, not a state copy
//!
//! rskj's `SnapshotManager.takeSnapshot` records `blockchain.getBestBlock()
//! .getNumber()` and returns the size of the list — that is the whole of it.
//! Reverting sets the chain's head back to that height and discards what came
//! after.
//!
//! So this costs nothing to take, and reverting is a head move rather than a
//! state restoration. It also means a snapshot is only as good as the state
//! still being on disk at that height: on a development chain nothing has been
//! collected yet, so that always holds.
//!
//! Snapshot ids are 1-based indices into a list that is **truncated** on
//! revert, which is rskj's behaviour: reverting to 2 discards snapshots 3 and
//! above, so ids are not stable across a revert. Reproduced rather than
//! improved, because a test suite written against rskj relies on it.
//!
//! # Why this is gated
//!
//! Every method here rewrites or extends the chain on demand. rskj ships the
//! namespace enabled by default; this node does not, and requires an explicit
//! flag. A node that can be told to discard its own chain over RPC has no
//! business being reachable on a network, whatever the default elsewhere.

use crate::server::RpcState;
use crate::types::*;
use serde_json::{json, Value};
use std::sync::Mutex;

/// Heights that were snapshotted, oldest first. The id of a snapshot is its
/// 1-based position.
#[derive(Default)]
pub struct SnapshotManager {
    heights: Mutex<Vec<u64>>,
}

impl SnapshotManager {
    pub fn new() -> Self {
        Self::default()
    }

    fn take(&self, height: u64) -> usize {
        let mut heights = self.heights.lock().unwrap_or_else(|e| e.into_inner());
        heights.push(height);
        heights.len()
    }

    /// The height to revert to, truncating the list as rskj does. `None` when
    /// the id names no snapshot.
    fn resolve(&self, id: usize) -> Option<u64> {
        let mut heights = self.heights.lock().unwrap_or_else(|e| e.into_inner());
        if id == 0 || id > heights.len() {
            return None;
        }
        let height = heights[id - 1];
        heights.truncate(id);
        Some(height)
    }

    fn clear(&self) {
        self.heights.lock().unwrap_or_else(|e| e.into_inner()).clear();
    }
}

fn parse_quantity(v: Option<&Value>) -> Option<i64> {
    let s = v?.as_str()?;
    let t = s.trim();
    match t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
        Some(hex) => i64::from_str_radix(hex, 16).ok(),
        None => t.parse::<i64>().ok(),
    }
}

/// `evm_snapshot()` → the new snapshot's id.
pub fn evm_snapshot(id: Value, state: &RpcState) -> JsonRpcResponse {
    let Some(manager) = &state.snapshots else { return unavailable(id) };
    let Some(height) = head_number(state) else {
        return JsonRpcResponse::error(id, INVALID_PARAMS, "no best block to snapshot");
    };
    let snapshot_id = manager.take(height);
    tracing::info!(target: "rustock::rpc", "evm_snapshot: #{height} is snapshot {snapshot_id}");
    JsonRpcResponse::success(id, json!(format!("0x{snapshot_id:x}")))
}

/// `evm_revert(snapshotId)` → whether the revert happened.
///
/// Returns `false` rather than erroring for an unknown id, which is rskj's
/// behaviour, and `true` without doing anything when the chain is already at
/// or below the snapshotted height.
pub fn evm_revert(id: Value, params: &Value, state: &RpcState) -> JsonRpcResponse {
    let Some(manager) = &state.snapshots else { return unavailable(id) };
    let Some(raw) = parse_quantity(params.get(0)) else {
        return JsonRpcResponse::error(id, INVALID_PARAMS, "invalid snapshot id");
    };
    if raw < 0 {
        return JsonRpcResponse::success(id, json!(false));
    }
    let Some(target) = manager.resolve(raw as usize) else {
        return JsonRpcResponse::success(id, json!(false));
    };

    match rewind_to_height(state, target) {
        Ok(true) => {
            tracing::info!(target: "rustock::rpc", "evm_revert: chain rolled back to #{target}");
            JsonRpcResponse::success(id, json!(true))
        }
        Ok(false) => JsonRpcResponse::success(id, json!(true)),
        Err(e) => JsonRpcResponse::error(id, INVALID_PARAMS, e),
    }
}

/// `evm_reset()` — back to genesis, and forget every snapshot.
pub fn evm_reset(id: Value, state: &RpcState) -> JsonRpcResponse {
    let Some(manager) = &state.snapshots else { return unavailable(id) };
    manager.clear();
    // A clock still carrying the last test's offset would leak into the next.
    rustock_execution::mining::dev::reset_time();
    match rewind_to_height(state, 0) {
        Ok(_) => {
            tracing::info!(target: "rustock::rpc", "evm_reset: chain reset to genesis");
            JsonRpcResponse::success(id, json!(true))
        }
        Err(e) => JsonRpcResponse::error(id, INVALID_PARAMS, e),
    }
}

/// `evm_increaseTime(seconds)` → the new total offset.
///
/// Moves the timestamp future blocks will carry. It does not touch the system
/// clock, and it does not restamp blocks already mined.
pub fn evm_increase_time(id: Value, params: &Value, state: &RpcState) -> JsonRpcResponse {
    if state.snapshots.is_none() {
        return unavailable(id);
    }
    let Some(seconds) = parse_quantity(params.get(0)) else {
        return JsonRpcResponse::error(id, INVALID_PARAMS, "invalid number of seconds");
    };
    let total = rustock_execution::mining::dev::increase_time(seconds);
    tracing::info!(target: "rustock::rpc", "evm_increaseTime({seconds}): offset now {total}s");
    JsonRpcResponse::success(id, json!(format!("0x{:x}", total.max(0))))
}

/// `evm_mine()` — mine one block now.
pub fn evm_mine(id: Value, state: &RpcState) -> JsonRpcResponse {
    if state.snapshots.is_none() {
        return unavailable(id);
    }
    let Some(miner) = &state.miner else {
        return JsonRpcResponse::error(
            id,
            INVALID_PARAMS,
            "node was not started with mining enabled; evm_mine needs --mine",
        );
    };
    match miner.mine_one_now() {
        Ok(number) => {
            tracing::info!(target: "rustock::rpc", "evm_mine: mined #{number}");
            JsonRpcResponse::success(id, json!(true))
        }
        Err(e) => JsonRpcResponse::error(id, INVALID_PARAMS, e),
    }
}

/// `evm_startMining()` / `evm_stopMining()`.
///
/// rskj starts and stops a background miner thread. This node mines only when
/// asked, so both are accepted and answer truthfully about what they did
/// rather than pretending to control a loop that does not exist -- a test
/// suite calls these for symmetry and does not inspect the result.
pub fn evm_set_mining(id: Value, state: &RpcState, _start: bool) -> JsonRpcResponse {
    if state.snapshots.is_none() {
        return unavailable(id);
    }
    JsonRpcResponse::success(id, json!(true))
}

fn unavailable(id: Value) -> JsonRpcResponse {
    JsonRpcResponse::error(
        id,
        METHOD_NOT_FOUND,
        "the evm_ namespace is a development-chain facility and is off; \
         start the node with --dev-rpc to enable it",
    )
}

fn head_number(state: &RpcState) -> Option<u64> {
    let hash = state.store.head().ok()??;
    state.store.header(hash).ok()?.map(|h| h.number)
}

/// Move the chain head, and the executed head, back to `target`.
///
/// Returns `Ok(false)` when the chain is already at or below it, which is not
/// a failure — rskj answers `true` for that case too.
fn rewind_to_height(state: &RpcState, target: u64) -> Result<bool, String> {
    let store = &state.store;
    let current = head_number(state).ok_or_else(|| "no best block".to_string())?;
    if current <= target {
        return Ok(false);
    }

    let target_hash = store
        .canonical_hash(target)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("no canonical block at #{target}"))?;
    let header = store
        .header(target_hash)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("no header for #{target}"))?;

    // The executed head must move with the chain head, and it must point at a
    // state that is actually on disk. `state_root` in a header before RSKIP126
    // is the legacy root, so the executed head's own record is the one to
    // trust when it is at or below the target.
    let state_root = match store.exec_head().map_err(|e| e.to_string())? {
        Some((exec_hash, root)) if exec_hash == target_hash => root,
        _ => header.state_root,
    };

    // Drop the canonical index above the target first: a reader that sees the
    // new head must not still be able to resolve a height beyond it.
    for number in (target + 1)..=current {
        store.delete_canonical_hash(number).map_err(|e| e.to_string())?;
    }
    store.set_head(target_hash).map_err(|e| e.to_string())?;
    store
        .set_exec_head(target_hash, state_root)
        .map_err(|e| e.to_string())?;

    Ok(true)
}
