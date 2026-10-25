use crate::helpers::to_hex_u64;
use crate::types::JsonRpcResponse;
use rustock_core::config::ChainConfig;
use rustock_networking::peers::PeerStore;
use serde_json::{json, Value};

/// The chain id, in decimal, as a string.
///
/// Not the network id, despite the name. rskj's `Web3Impl.net_version` is
///
/// ```java
/// byte netVersion = config.getNetworkConstants().getChainId();
/// return Byte.toString(netVersion);
/// ```
///
/// so it answers 30 on mainnet, 31 on testnet and 33 on regtest -- the same
/// number `eth_chainId` reports, in a different base. This node answered
/// `network_id` instead (775, 8100, 7771), which disagrees with rskj on every
/// chain, and disagrees with its own `eth_chainId` on all of them.
///
/// The name is the trap, and the configured `network_id` is still the right
/// value for what it is actually for: the p2p handshake, where it keeps
/// chains from peering with each other. It is simply not what this method
/// returns.
///
/// Found by the rskj-k6-tests battery, which asserts the two agree and passes
/// against rskj.
pub fn net_version(id: Value, config: &ChainConfig) -> JsonRpcResponse {
    JsonRpcResponse::success(id, json!(config.chain_id.to_string()))
}

pub async fn net_peer_count(id: Value, peer_store: &PeerStore) -> JsonRpcResponse {
    let count = peer_store.count().await;
    JsonRpcResponse::success(id, json!(to_hex_u64(count as u64)))
}

pub fn net_listening(id: Value) -> JsonRpcResponse {
    JsonRpcResponse::success(id, json!(true))
}

pub async fn net_peer_list(id: Value, peer_store: &PeerStore) -> JsonRpcResponse {
    let peers = peer_store.peers().await;
    let list: Vec<String> = peers.iter().map(|p| format!("{:#x}", p)).collect();
    JsonRpcResponse::success(id, json!(list))
}
