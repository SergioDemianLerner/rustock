pub mod message;
pub mod table;

#[cfg(test)]
mod tests;

use tokio::net::UdpSocket;
use message::{DiscoveryPacket, DiscoveryPayload, PongMessage, DiscoveryEndpoint, DiscoveryNode};
use table::NodeTable;
use crate::peers::PeerStore;
use alloy_primitives::B512;
use k256::ecdsa::SigningKey;
use anyhow::Result;
use std::collections::{HashMap, HashSet};
use std::panic::AssertUnwindSafe;
use futures::FutureExt;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{Mutex, RwLock};
use tracing::{info, debug, trace, warn, error};

/// Normal interval between discovery rounds (ping + FindNode sweep).
const DISCOVERY_INTERVAL: Duration = Duration::from_secs(15);
/// Faster interval used while we're below `LOW_PEER_FLOOR` active peers, to
/// re-seed the node table quickly instead of waiting a full round.
const DISCOVERY_INTERVAL_LOW: Duration = Duration::from_secs(5);
/// Active-peer count below which discovery switches to aggressive re-bootstrap:
/// it re-pings every bootstrap address (not just unknown ones) and polls on the
/// shorter interval. This self-heals the isolation that previously needed a
/// manual restart. Not present in rskj (whose Netty/SyncPool stack recovers
/// differently); a rustock improvement.
const LOW_PEER_FLOOR: usize = 4;
/// Backoff after a UDP socket receive error, so a persistently failing socket
/// can't hot-spin the recv loop and flood the logs.
const RECV_ERROR_BACKOFF: Duration = Duration::from_millis(500);

/// How long an endpoint proof stays valid once a peer has answered our Ping
/// with a matching Pong. Mirrors geth's 12-hour bond lifetime.
const ENDPOINT_PROOF_TTL: Duration = Duration::from_secs(12 * 60 * 60);

/// How long we keep waiting for a Pong that matches a Ping we sent. Anything
/// older is discarded, so the pending map cannot grow without bound.
const PING_TIMEOUT: Duration = Duration::from_secs(30);

/// Hard caps on the endpoint-proof bookkeeping. Both maps are keyed by remote
/// address, which an attacker can vary freely by spoofing the source, so they
/// need an absolute ceiling as well as time-based expiry.
const MAX_PENDING_PINGS: usize = 2048;
const MAX_VERIFIED_ENDPOINTS: usize = 4096;

/// Service for node discovery using UDP based on RSK protocol.
///
/// rskj requires a full Ping/Pong bonding handshake before responding
/// to FindNode requests. The sequence is:
///   1. We send Ping → peer
///   2. Peer replies with Pong, and also sends us a Ping
///   3. We reply to their Ping with Pong
///   4. Peer adds us to their `establishedConnections`
///   5. Now peer will respond to our FindNode with Neighbors
///
/// We track peers that have sent us a Ping (to whom we replied with Pong)
/// as "bonded", and only send FindNode to those peers.
pub struct DiscoveryService {
    socket: UdpSocket,
    key: SigningKey,
    table: Arc<RwLock<NodeTable>>,
    /// Peers that have completed the bonding handshake (received their Ping,
    /// sent our Pong). Stored as socket addresses since we might not know
    /// the node ID at discovery time.
    bonded: Mutex<HashSet<std::net::SocketAddr>>,
    /// Pings we have sent and not yet seen answered: address -> (message_id,
    /// sent_at). The message_id is a v4 UUID, so an off-path attacker cannot
    /// forge a matching Pong without receiving our packet at the address it was
    /// sent to. This is what makes the endpoint proof meaningful.
    pending_pings: Mutex<HashMap<std::net::SocketAddr, (String, Instant)>>,
    /// Addresses that have completed an endpoint proof, with the time it was
    /// completed. Only these are answered when a FindNode arrives.
    verified_endpoints: Mutex<HashMap<std::net::SocketAddr, Instant>>,
    network_id: u32,
    local_node: DiscoveryNode,
    /// Bootstrap peer addresses (rskj `peer.discovery.ip.list`). Node IDs are
    /// unknown upfront; we ping these and learn IDs from their signed pongs.
    bootstrap_addrs: Vec<std::net::SocketAddr>,
    /// Active peer connections, used to detect when we're isolated and should
    /// re-bootstrap aggressively.
    peer_store: Arc<PeerStore>,
}

impl DiscoveryService {
    pub async fn new(
        listen_addr: &str,
        key: SigningKey,
        table: Arc<RwLock<NodeTable>>,
        network_id: u32,
        local_node: DiscoveryNode,
        bootstrap_addrs: Vec<std::net::SocketAddr>,
        peer_store: Arc<PeerStore>,
    ) -> Result<Self> {
        let socket = UdpSocket::bind(listen_addr).await?;
        Ok(Self {
            socket,
            key,
            table,
            bonded: Mutex::new(HashSet::new()),
            pending_pings: Mutex::new(HashMap::new()),
            verified_endpoints: Mutex::new(HashMap::new()),
            network_id,
            local_node,
            bootstrap_addrs,
            peer_store,
        })
    }

    /// Starts the UDP service loop for processing discovery packets.
    pub async fn start(self: Arc<Self>) {
        let mut buf = [0u8; 4096];
        
        info!(target: "rustock::discovery", "Discovery service started");
        
        let receive_self = self.clone();
        tokio::spawn(async move {
            loop {
                match receive_self.socket.recv_from(&mut buf).await {
                    Ok((n, addr)) => {
                        // handle_packet runs inside this loop, so a panic would
                        // unwind out of it and kill the receive task outright —
                        // peer discovery would then stay dead until the process
                        // restarts. The input is unauthenticated UDP from
                        // anyone, so contain a panic to the packet that caused
                        // it rather than trusting every parser downstream.
                        let handled = AssertUnwindSafe(
                            receive_self.handle_packet(&buf[..n], addr),
                        )
                        .catch_unwind()
                        .await;
                        match handled {
                            Ok(Ok(())) => {}
                            Ok(Err(e)) => {
                                warn!(target: "rustock::discovery", "Error handling packet from {}: {:?}", addr, e);
                            }
                            Err(_) => {
                                error!(
                                    target: "rustock::discovery",
                                    "Panic while handling discovery packet from {} — packet dropped, receive loop continues",
                                    addr
                                );
                            }
                        }
                    }
                    Err(e) => {
                        // A persistent socket error must not hot-spin the loop
                        // (rskj's Netty stack handles this for us); back off so
                        // we don't peg a core and flood the logs.
                        error!(target: "rustock::discovery", "UDP socket error: {:?}", e);
                        tokio::time::sleep(RECV_ERROR_BACKOFF).await;
                    }
                }
            }
        });

        // Background discovery loop
        loop {
            let nodes = self.table.read().await.all_nodes();
            let peer_count = self.peer_store.count().await;
            let low_peers = peer_count < LOW_PEER_FLOOR;

            // Ping bootstrap addresses we don't know yet (no node ID in the
            // table for their address); their pongs/pings get them added. When
            // peers are low, re-ping *every* bootstrap to aggressively re-seed
            // discovery and recover from isolation.
            let known_addrs: HashSet<std::net::SocketAddr> = nodes
                .iter()
                .filter_map(|n| {
                    crate::utils::bytes_to_ip(&n.ip)
                        .map(|ip| std::net::SocketAddr::new(ip, n.udp_port))
                })
                .collect();
            for addr in &self.bootstrap_addrs {
                if low_peers || !known_addrs.contains(addr) {
                    let _ = self.send_ping(*addr).await;
                }
            }

            let bonded = self.bonded.lock().await;

            trace!(
                target: "rustock::discovery",
                "Discovery loop: {} nodes in table, {} bonded, {} active peers{}",
                nodes.len(),
                bonded.len(),
                peer_count,
                if low_peers { " (low — re-bootstrapping)" } else { "" }
            );

            for node in &nodes {
                if let Some(ip) = crate::utils::bytes_to_ip(&node.ip) {
                    let socket_addr = std::net::SocketAddr::new(ip, node.udp_port);
                    let _ = self.send_ping(socket_addr).await;
                    if bonded.contains(&socket_addr) {
                        let _ = self.send_find_node(self.local_node.id, socket_addr).await;
                    }
                }
            }
            drop(bonded);

            // Keep the node table fresh. The bonding and FindNode flow needs
            // multiple rounds: first we Ping, then they Ping us back, then we
            // send FindNode on the next cycle. Poll faster while peers are low.
            let interval = if low_peers { DISCOVERY_INTERVAL_LOW } else { DISCOVERY_INTERVAL };
            tokio::time::sleep(interval).await;
        }
    }

    async fn send_ping(&self, to: std::net::SocketAddr) -> Result<()> {
        use uuid::Uuid;
        let message_id = Uuid::new_v4().to_string();
        let payload = DiscoveryPayload::Ping(message::PingMessage {
            from: DiscoveryEndpoint {
                ip: self.local_node.ip.clone(),
                udp_port: self.local_node.udp_port,
                tcp_port: self.local_node.tcp_port,
            },
            to: self.addr_to_endpoint(to),
            message_id: message_id.clone(),
            network_id: self.network_id,
        });
        {
            let mut pending = self.pending_pings.lock().await;
            let now = Instant::now();
            pending.retain(|_, (_, sent)| now.duration_since(*sent) < PING_TIMEOUT);
            if pending.len() < MAX_PENDING_PINGS {
                pending.insert(to, (message_id, now));
            }
        }
        let packet = DiscoveryPacket::create(payload, &self.key)?;
        self.socket.send_to(&packet.encode(), to).await?;
        Ok(())
    }

    /// True if `addr` has answered one of our Pings with a matching Pong recently.
    async fn has_endpoint_proof(&self, addr: &std::net::SocketAddr) -> bool {
        let verified = self.verified_endpoints.lock().await;
        match verified.get(addr) {
            Some(at) => Instant::now().duration_since(*at) < ENDPOINT_PROOF_TTL,
            None => false,
        }
    }

    async fn send_find_node(&self, target: B512, to: std::net::SocketAddr) -> Result<()> {
        use uuid::Uuid;
        let payload = DiscoveryPayload::FindNode(message::FindNodeMessage {
            target,
            message_id: Uuid::new_v4().to_string(),
            network_id: self.network_id,
        });
        let packet = DiscoveryPacket::create(payload, &self.key)?;
        self.socket.send_to(&packet.encode(), to).await?;
        Ok(())
    }

    async fn handle_packet(&self, buf: &[u8], addr: std::net::SocketAddr) -> Result<()> {
        let packet = DiscoveryPacket::decode(buf)?;
        
        match &packet.payload {
            DiscoveryPayload::Ping(ping) => {
                trace!(target: "rustock::discovery", "Received Ping from {}", addr);
                // Reply with Pong to complete bonding from the remote's perspective
                self.send_pong(ping.message_id.clone(), addr).await?;
                
                let node = DiscoveryNode {
                    ip: crate::utils::ip_to_bytes(addr.ip()),
                    udp_port: addr.port(),
                    tcp_port: ping.from.tcp_port,
                    id: packet.recover_id()?,
                };
                self.table.write().await.add_node(node);

                // Mark this peer as bonded — we replied with Pong, so the
                // remote will accept our FindNode after processing our Pong.
                let newly_bonded = self.bonded.lock().await.insert(addr);
                if newly_bonded {
                    debug!(
                        target: "rustock::discovery",
                        "Bonded with new peer at {}",
                        addr
                    );
                    // Small delay to let the remote process our Pong before
                    // we send FindNode. rskj adds us to establishedConnections
                    // upon receiving our Pong; without this delay, FindNode
                    // may arrive before Pong is processed.
                    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                    let _ = self.send_find_node(self.local_node.id, addr).await;
                }
            }
            DiscoveryPayload::Pong(pong) => {
                trace!(target: "rustock::discovery", "Received Pong from {}", addr);
                // Endpoint proof: the Pong must echo the message_id of a Ping we
                // sent to this exact address. The id is a v4 UUID (122 bits of
                // entropy), so an off-path attacker spoofing the source address
                // cannot produce a matching Pong -- they never received the Ping.
                let matched = {
                    let mut pending = self.pending_pings.lock().await;
                    match pending.get(&addr) {
                        Some((expected_id, sent))
                            if *expected_id == pong.message_id
                                && Instant::now().duration_since(*sent) < PING_TIMEOUT =>
                        {
                            pending.remove(&addr);
                            true
                        }
                        _ => false,
                    }
                };
                if matched {
                    let mut verified = self.verified_endpoints.lock().await;
                    let now = Instant::now();
                    verified.retain(|_, at| now.duration_since(*at) < ENDPOINT_PROOF_TTL);
                    if verified.len() < MAX_VERIFIED_ENDPOINTS {
                        verified.insert(addr, now);
                    }
                } else {
                    trace!(
                        target: "rustock::discovery",
                        "Unsolicited Pong from {} — no endpoint proof granted",
                        addr
                    );
                }
                // Like rskj's PeerExplorer.handlePong: learn the node from the
                // pong, recovering its ID from the packet signature.
                let tcp_port = if pong.from.tcp_port != 0 { pong.from.tcp_port } else { addr.port() };
                let node = DiscoveryNode {
                    ip: crate::utils::ip_to_bytes(addr.ip()),
                    udp_port: addr.port(),
                    tcp_port,
                    id: packet.recover_id()?,
                };
                self.table.write().await.add_node(node);
            }
            DiscoveryPayload::FindNode(find) => {
                trace!(target: "rustock::discovery", "Received FindNode from {}", addr);
                // Answering without an endpoint proof turns this node into a UDP
                // reflector: a ~100 byte FindNode yields a ~1.3 KiB Neighbors
                // reply, roughly 13x amplification, and the source address of a
                // UDP datagram is trivially spoofed. FindNode is not
                // authenticated either — the packet MDC is a plain Keccak hash
                // that anyone can compute over their own bytes — so the address
                // proof is the only thing standing between us and being used to
                // flood a third party.
                if !self.has_endpoint_proof(&addr).await {
                    debug!(
                        target: "rustock::discovery",
                        "Ignoring FindNode from unverified endpoint {}",
                        addr
                    );
                    // Start a proof of our own so a legitimate peer becomes
                    // answerable shortly. Sent to the claimed address, so a
                    // spoofing attacker gains nothing: the Ping goes to the
                    // victim, is ~the same size as the FindNode, and the reply
                    // it would need to forge is unguessable.
                    let _ = self.send_ping(addr).await;
                    return Ok(());
                }
                let closest = self.table.read().await.closest_nodes(&find.target, 16);
                self.send_neighbors(find.message_id.clone(), closest, addr).await?;
            }
            DiscoveryPayload::Neighbors(neighbors) => {
                trace!(
                    target: "rustock::discovery",
                    "Received {} neighbors from {}",
                    neighbors.nodes.len(),
                    addr
                );
                let mut table = self.table.write().await;
                for node in &neighbors.nodes {
                    table.add_node(node.clone());
                }
            }
        }
        
        Ok(())
    }

    async fn send_pong(&self, message_id: String, to: std::net::SocketAddr) -> Result<()> {
        let payload = DiscoveryPayload::Pong(PongMessage {
            from: DiscoveryEndpoint {
                ip: self.local_node.ip.clone(),
                udp_port: self.local_node.udp_port,
                tcp_port: self.local_node.tcp_port,
            },
            to: self.addr_to_endpoint(to),
            message_id,
            network_id: self.network_id,
        });
        
        let packet = DiscoveryPacket::create(payload, &self.key)?;
        self.socket.send_to(&packet.encode(), to).await?;
        Ok(())
    }

    async fn send_neighbors(&self, message_id: String, nodes: Vec<DiscoveryNode>, to: std::net::SocketAddr) -> Result<()> {
        let payload = DiscoveryPayload::Neighbors(message::NeighborsMessage {
            nodes,
            message_id,
            network_id: self.network_id,
        });
        
        let packet = DiscoveryPacket::create(payload, &self.key)?;
        self.socket.send_to(&packet.encode(), to).await?;
        Ok(())
    }

    fn addr_to_endpoint(&self, addr: std::net::SocketAddr) -> DiscoveryEndpoint {
        DiscoveryEndpoint {
            ip: crate::utils::ip_to_bytes(addr.ip()),
            udp_port: addr.port(),
            tcp_port: 0,
        }
    }
}
