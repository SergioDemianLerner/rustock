//! The `sco_*` namespace: peer scoring and banning.
//!
//! Source of truth: rskj's `Web3Impl.sco_*` over `PeerScoringManager`.
//! `docs/peer-scoring.md` records the behaviour and where it differs.
//!
//! A scoring system nobody can inspect is a scoring system nobody trusts, so
//! these six methods are part of the feature rather than an extra: an
//! operator needs to see why a peer was dropped, and to override it.

use crate::server::RpcState;
use crate::types::*;
use rustock_networking::scoring::EventType;
use serde_json::{json, Map, Value};

fn unavailable(id: Value) -> JsonRpcResponse {
    JsonRpcResponse::error(
        id,
        METHOD_NOT_FOUND,
        "Peer scoring is not enabled on this node",
    )
}

/// `sco_banAddress(address)` — an address or a CIDR block.
///
/// rskj returns nothing (`void`) and this returns `true`, because a JSON-RPC
/// result of `null` is indistinguishable from "the method did nothing".
/// Callers checking for an error get the same answer either way.
pub fn sco_ban_address(id: Value, params: &Value, state: &RpcState) -> JsonRpcResponse {
    let Some(scoring) = &state.scoring else { return unavailable(id) };
    let Some(address) = params.get(0).and_then(|v| v.as_str()) else {
        return JsonRpcResponse::error(id, INVALID_PARAMS, "Missing address");
    };
    match scoring.ban(address) {
        Ok(()) => JsonRpcResponse::success(id, Value::Bool(true)),
        Err(why) => JsonRpcResponse::error(
            id,
            INVALID_PARAMS,
            format!("invalid banned address {address}: {why}"),
        ),
    }
}

/// `sco_unbanAddress(address)`.
pub fn sco_unban_address(id: Value, params: &Value, state: &RpcState) -> JsonRpcResponse {
    let Some(scoring) = &state.scoring else { return unavailable(id) };
    let Some(address) = params.get(0).and_then(|v| v.as_str()) else {
        return JsonRpcResponse::error(id, INVALID_PARAMS, "Missing address");
    };
    match scoring.unban(address) {
        Ok(()) => JsonRpcResponse::success(id, Value::Bool(true)),
        Err(why) => JsonRpcResponse::error(
            id,
            INVALID_PARAMS,
            format!("invalid banned address {address}: {why}"),
        ),
    }
}

/// `sco_bannedAddresses()` — single addresses and blocks, in one list.
pub fn sco_banned_addresses(id: Value, state: &RpcState) -> JsonRpcResponse {
    let Some(scoring) = &state.scoring else { return unavailable(id) };
    let list = scoring.with(|m| m.banned_addresses());
    JsonRpcResponse::success(id, Value::Array(list.into_iter().map(Value::String).collect()))
}

/// `sco_peerList()` — every scored entry, by node id and by address.
pub fn sco_peer_list(id: Value, state: &RpcState) -> JsonRpcResponse {
    let Some(scoring) = &state.scoring else { return unavailable(id) };
    let entries = scoring.with(|m| m.information());
    JsonRpcResponse::success(id, Value::Array(entries.iter().map(render).collect()))
}

/// `sco_clearPeerScoring(id)` — by address if it parses as one, else by node
/// id — and returns the peer list, as rskj does.
pub fn sco_clear_peer_scoring(id: Value, params: &Value, state: &RpcState) -> JsonRpcResponse {
    let Some(scoring) = &state.scoring else { return unavailable(id) };
    let Some(target) = params.get(0).and_then(|v| v.as_str()) else {
        return JsonRpcResponse::error(id, INVALID_PARAMS, "Missing peer id");
    };

    // rskj tries `InetAddress.getByName` first and falls back to a node id.
    // Note its `getByName` resolves DNS names, so `sco_clearPeerScoring
    // ("example.com")` would clear whatever that resolves to; here only a
    // literal address parses, and anything else is taken as a node id.
    if let Ok(address) = target.parse::<std::net::IpAddr>() {
        scoring.with(|m| m.clear_address(address));
    } else if let Some(node) = parse_node_id(target) {
        scoring.with(|m| m.clear_node(node));
    } else {
        return JsonRpcResponse::error(
            id,
            INVALID_PARAMS,
            "Not an IP address or a 64-byte node id",
        );
    }

    let entries = scoring.with(|m| m.information());
    JsonRpcResponse::success(id, Value::Array(entries.iter().map(render).collect()))
}

/// `sco_reputationSummary()` — totals across every scored entry.
///
/// rskj builds this in `PeerScoringReporterUtil.buildReputationSummary`, one
/// sum per counter plus the good/bad split.
pub fn sco_reputation_summary(id: Value, state: &RpcState) -> JsonRpcResponse {
    let Some(scoring) = &state.scoring else { return unavailable(id) };
    let entries = scoring.with(|m| m.information());

    let mut totals: Map<String, Value> = Map::new();
    for event in EventType::ALL {
        let name = event.report_name();
        let sum: u64 = entries
            .iter()
            .map(|e| {
                e.counters
                    .iter()
                    .find(|(n, _)| *n == name)
                    .map(|(_, c)| *c as u64)
                    .unwrap_or(0)
            })
            .sum();
        totals.insert(name.to_string(), Value::from(sum));
    }

    let good = entries.iter().filter(|e| e.good_reputation).count();
    totals.insert("goodReputationCount".into(), Value::from(good));
    totals.insert("badReputationCount".into(), Value::from(entries.len() - good));
    totals.insert("peersTotalCount".into(), Value::from(entries.len()));
    totals.insert(
        "punishmentCount".into(),
        Value::from(entries.iter().map(|e| e.punishments as u64).sum::<u64>()),
    );
    JsonRpcResponse::success(id, Value::Object(totals))
}

fn render(entry: &rustock_networking::scoring::ScoringInformation) -> Value {
    let mut out = Map::new();
    out.insert("id".into(), Value::String(entry.id.clone()));
    out.insert("type".into(), Value::String(entry.kind.to_string()));
    for (name, count) in &entry.counters {
        out.insert((*name).to_string(), Value::from(*count));
    }
    // JSON numbers, not hex: rskj's `PeerScoringInformation` is a plain bean
    // of `int`s and a `long`, serialised by Jackson. Same shape as `trace_*`,
    // and the same trap for a client that assumes hex everywhere.
    out.insert("score".into(), Value::from(entry.score));
    out.insert("punishments".into(), Value::from(entry.punishments));
    out.insert("goodReputation".into(), Value::Bool(entry.good_reputation));
    out.insert("punishedUntil".into(), Value::from(entry.punished_until_ms));
    Value::Object(out)
}

/// A node id as `sco_clearPeerScoring` accepts it: 128 hex characters, with
/// or without the `0x` prefix.
fn parse_node_id(text: &str) -> Option<alloy_primitives::B512> {
    let text = text.trim_start_matches("0x");
    if text.len() != 128 {
        return None;
    }
    let bytes = hex::decode(text).ok()?;
    Some(alloy_primitives::B512::from_slice(&bytes))
}

/// Not an rskj method. `sco_*` has no way to ask "would this address be let
/// in", and an operator testing a CIDR ban otherwise has to wait for the peer
/// to reconnect.
pub fn sco_is_welcome(id: Value, params: &Value, state: &RpcState) -> JsonRpcResponse {
    let Some(scoring) = &state.scoring else { return unavailable(id) };
    let Some(target) = params.get(0).and_then(|v| v.as_str()) else {
        return JsonRpcResponse::error(id, INVALID_PARAMS, "Missing address");
    };
    let Ok(address) = target.parse::<std::net::IpAddr>() else {
        return JsonRpcResponse::error(id, INVALID_PARAMS, "Not an IP address");
    };
    let banned = scoring.with(|m| m.is_address_banned(address));
    let welcome = scoring.address_is_welcome(address);
    JsonRpcResponse::success(
        id,
        json!({
            "address": target,
            "banned": banned,
            // False without `banned` means a punishment is running, which is
            // temporary; banned means it is not.
            "welcome": welcome,
        }),
    )
}
