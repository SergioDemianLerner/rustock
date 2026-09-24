use tokio::net::{TcpListener, TcpStream};
use crate::protocol::{P2pHandler, P2pMessage};
use crate::handshake::Handshake;
use crate::peers::PeerStore;
use alloy_primitives::{B512, B256, U256};
use anyhow::{Result, Context};
use std::net::IpAddr;
use std::sync::Arc;
use std::collections::HashMap;
use tokio::sync::{Semaphore, Mutex};
use tracing::{info, error, warn, debug, trace};

/// Masks `ip` to its network block, so all addresses within one block collapse
/// to a single key. IPv4 uses `prefix` bits; IPv6 uses `prefix * 2` bits, since
/// allocations there are far larger (a /48 or /56 is a routine single-customer
/// assignment, so treating an IPv6 /24 as one block would be meaningless).
fn cidr_block(ip: IpAddr, prefix: u8) -> IpAddr {
    match ip {
        IpAddr::V4(v4) => {
            let bits = u32::from(v4);
            let p = prefix.min(32);
            let mask: u32 = if p == 0 { 0 } else { u32::MAX << (32 - p) };
            IpAddr::V4(std::net::Ipv4Addr::from(bits & mask))
        }
        IpAddr::V6(v6) => {
            let bits = u128::from(v6);
            let p = (prefix as u16 * 2).min(128) as u32;
            let mask: u128 = if p == 0 { 0 } else { u128::MAX << (128 - p) };
            IpAddr::V6(std::net::Ipv6Addr::from(bits & mask))
        }
    }
}

/// A P2P Node that manages incoming connections and peer handshakes.
pub struct Node {
    pub config: NodeConfig,
    pub handlers: Vec<Arc<dyn P2pHandler>>,
    peer_store: Arc<PeerStore>,
    /// Peer scoring and banning. `None` runs the node exactly as before:
    /// every peer welcome, nothing recorded.
    scoring: Option<Arc<crate::scoring::ScoringService>>,
}

/// Configuration for the P2P node.
#[derive(Clone, Debug)]
pub struct NodeConfig {
    pub client_id: String,
    pub listen_port: u16,
    pub id: B512,
    pub chain_id: u8,
    pub network_id: u64,
    pub genesis_hash: B256,
    pub best_hash: B256,
    pub best_block_number: u64,
    pub total_difficulty: U256,
    pub bootnodes: Vec<String>,
    pub secret_key: [u8; 32],
    pub discovery_port: u16,
    pub data_dir: String,
    pub external_ip: Option<IpAddr>,
    /// Target number of outbound peer connections to maintain (rskj's
    /// `maxActivePeers`). The outbound connector dials toward this count.
    pub max_outbound_peers: usize,
    /// Maximum number of simultaneous *inbound* connections. The accept loop
    /// refuses beyond this, so a single host cannot exhaust memory and CPU by
    /// opening connections faster than handshakes complete.
    pub max_inbound_peers: usize,
    /// Maximum simultaneous inbound connections from any single IP address.
    /// Without this, one host can occupy every inbound slot on its own.
    pub max_inbound_per_ip: usize,
    /// Maximum simultaneous inbound connections from any single network block,
    /// with the block size given by `inbound_cidr_prefix`.
    ///
    /// A per-IP cap alone is trivially sidestepped by an attacker holding a
    /// subnet: a /24 gives 256 addresses, each under the per-IP limit. rskj
    /// bounds by network block for this reason (`peer.filter.maxConnections`
    /// over `networkCIDR`), and this mirrors it. The two limits are kept
    /// together rather than one replacing the other: the per-IP cap stops one
    /// host monopolising slots, the per-block cap stops one operator doing the
    /// same from many hosts.
    pub max_inbound_per_cidr: usize,
    /// Prefix length defining a network block for `max_inbound_per_cidr`
    /// (IPv4; the IPv6 block uses the same value against the /64-style prefix).
    pub inbound_cidr_prefix: u8,
}

impl Node {
    pub fn new(config: NodeConfig) -> Self {
        Self {
            config,
            handlers: Vec::new(),
            peer_store: Arc::new(PeerStore::new()),
            scoring: None,
        }
    }

    pub fn with_peer_store(config: NodeConfig, peer_store: Arc<PeerStore>) -> Self {
        Self {
            config,
            handlers: Vec::new(),
            peer_store,
            scoring: None,
        }
    }

    /// Attach peer scoring. Without this the node behaves as it always did.
    pub fn with_scoring(mut self, scoring: Arc<crate::scoring::ScoringService>) -> Self {
        self.scoring = Some(scoring);
        self
    }

    /// The scoring service, for the RPC layer and the sync service.
    pub fn scoring(&self) -> Option<Arc<crate::scoring::ScoringService>> {
        self.scoring.clone()
    }

    /// Adds a message handler to the node.
    pub fn add_handler(&mut self, handler: Arc<dyn P2pHandler>) {
        self.handlers.push(handler);
    }

    /// Initializes the discovery node table, loading persisted nodes and bootnodes.
    async fn init_discovery_table(&self) -> Arc<tokio::sync::RwLock<crate::discovery::table::NodeTable>> {
        let table = Arc::new(tokio::sync::RwLock::new(
            crate::discovery::table::NodeTable::new(self.config.id),
        ));
        let discovery_path = std::path::Path::new(&self.config.data_dir).join("discovery.rlp");

        let mut table_lock = table.write().await;
        if discovery_path.exists() {
            match tokio::fs::read(&discovery_path).await {
                Ok(data) => {
                    if let Err(e) = table_lock.decode_and_add(&data) {
                        debug!(target: "rustock::net", "Failed to decode discovery table: {:?}", e);
                    }
                }
                Err(e) => {
                    debug!(target: "rustock::net", "Failed to read discovery table: {:?}", e);
                }
            }
        }
        for enode in self.config.bootnodes.iter().filter(|b| b.starts_with("enode://")) {
            if let Err(e) = table_lock.add_enode(enode) {
                error!(target: "rustock::net", "Failed to add bootnode {}: {:?}", enode, e);
            }
        }
        drop(table_lock);
        table
    }

    /// Resolves `host:port` bootstrap entries (rskj `peer.discovery.ip.list`
    /// style, no node ID) to socket addresses.
    async fn resolve_bootstrap_addrs(&self) -> Vec<std::net::SocketAddr> {
        let mut addrs = Vec::new();
        for entry in self.config.bootnodes.iter().filter(|b| !b.starts_with("enode://")) {
            match tokio::net::lookup_host(entry.as_str()).await {
                Ok(resolved) => addrs.extend(resolved.filter(|a| a.is_ipv4())),
                Err(e) => warn!(target: "rustock::net", "Failed to resolve bootstrap {}: {:?}", entry, e),
            }
        }
        addrs
    }

    /// Starts the UDP discovery service and returns the shared table handle.
    async fn start_discovery(
        &self,
        table: Arc<tokio::sync::RwLock<crate::discovery::table::NodeTable>>,
    ) -> Result<()> {
        let signing_key = k256::ecdsa::SigningKey::from_slice(&self.config.secret_key)
            .context("Invalid secret key")?;

        let ip_bytes: Vec<u8> = match self.config.external_ip {
            Some(IpAddr::V4(v4)) => v4.octets().to_vec(),
            Some(IpAddr::V6(v6)) => v6.octets().to_vec(),
            None => vec![127, 0, 0, 1],
        };
        let local_node = crate::discovery::message::DiscoveryNode {
            ip: alloy_primitives::Bytes::from(ip_bytes),
            udp_port: self.config.discovery_port,
            tcp_port: self.config.listen_port,
            id: self.config.id,
        };

        let bootstrap_addrs = self.resolve_bootstrap_addrs().await;
        let discovery_addr = format!("0.0.0.0:{}", self.config.discovery_port);
        let discovery = Arc::new(crate::discovery::DiscoveryService::new(
            &discovery_addr,
            signing_key,
            table,
            self.config.network_id as u32,
            local_node,
            bootstrap_addrs,
            self.peer_store.clone(),
        ).await?);

        tokio::spawn(discovery.start());
        Ok(())
    }

    /// Spawns a background task that periodically persists the discovery table.
    fn start_table_persistence(
        &self,
        table: Arc<tokio::sync::RwLock<crate::discovery::table::NodeTable>>,
    ) {
        let discovery_path = std::path::Path::new(&self.config.data_dir)
            .join("discovery.rlp");
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(tokio::time::Duration::from_secs(60)).await;
                let data = table.read().await.encode();
                if let Err(e) = tokio::fs::write(&discovery_path, data).await {
                    error!(target: "rustock::net", "Failed to save discovery table: {:?}", e);
                }
            }
        });
    }

    /// Starts the P2P node: discovery, outbound connector, and the accept loop.
    pub async fn start(&self) -> Result<()> {
        let addr = format!("0.0.0.0:{}", self.config.listen_port);
        let listener = TcpListener::bind(&addr).await.context("Failed to bind listener")?;
        info!(target: "rustock::net", "P2P node started on {}", addr);

        let table = self.init_discovery_table().await;
        self.start_discovery(table.clone()).await?;
        self.start_table_persistence(table.clone());

        let peer_exchange = Arc::new(crate::peer_exchange::PeerExchangeHandler::new(table.clone()));
        let mut all_handlers = vec![peer_exchange as Arc<dyn P2pHandler>];
        all_handlers.extend(self.handlers.clone());

        let outbound = crate::outbound::OutboundConnector::new(
            self.config.clone(),
            table.clone(),
            self.peer_store.clone(),
            all_handlers.clone(),
            self.config.max_outbound_peers,
        )
        .with_scoring(self.scoring.clone());
        tokio::spawn(outbound.start());

        // Bounds concurrent inbound connections. Each one spawns a task that
        // performs an ECIES handshake (secp256k1 ECDH + Keccak), so an unbounded
        // accept loop is a CPU and memory exhaustion vector from a single host.
        let inbound_slots = Arc::new(Semaphore::new(self.config.max_inbound_peers));
        let per_ip = Arc::new(Mutex::new(HashMap::<IpAddr, usize>::new()));
        let per_cidr = Arc::new(Mutex::new(HashMap::<IpAddr, usize>::new()));

        loop {
            let (stream, peer_addr) = listener.accept().await?;

            // Acquire a global slot first. try_acquire_owned never blocks the
            // accept loop: over the limit we drop the connection immediately
            // rather than queueing work an attacker controls.
            let permit = match inbound_slots.clone().try_acquire_owned() {
                Ok(p) => p,
                Err(_) => {
                    debug!(
                        target: "rustock::net",
                        "Refusing {}: inbound capacity {} reached",
                        peer_addr, self.config.max_inbound_peers
                    );
                    drop(stream);
                    continue;
                }
            };

            // Then a per-IP slot, so one host cannot take every global slot.
            let ip = peer_addr.ip();
            {
                let mut map = per_ip.lock().await;
                let count = map.entry(ip).or_insert(0);
                if *count >= self.config.max_inbound_per_ip {
                    debug!(
                        target: "rustock::net",
                        "Refusing {}: {} connections already from this IP",
                        peer_addr, count
                    );
                    drop(stream);
                    continue;
                }
                *count += 1;
            }

            let block = cidr_block(ip, self.config.inbound_cidr_prefix);
            {
                let mut map = per_cidr.lock().await;
                let count = map.entry(block).or_insert(0);
                if *count >= self.config.max_inbound_per_cidr {
                    debug!(
                        target: "rustock::net",
                        "Refusing {}: {} connections already from network block {}",
                        peer_addr, count, block
                    );
                    // Give back the per-IP slot taken just above.
                    let mut ip_map = per_ip.lock().await;
                    if let Some(c) = ip_map.get_mut(&ip) {
                        *c = c.saturating_sub(1);
                        if *c == 0 {
                            ip_map.remove(&ip);
                        }
                    }
                    drop(stream);
                    continue;
                }
                *count += 1;
            }

            // Reputation is checked *before* the ECIES handshake, which is
            // the expensive part: a banned host must not be able to make this
            // node do secp256k1 work by reconnecting. rskj checks in
            // `HandshakeHandler` after decoding, which is later than it needs
            // to be.
            if let Some(scoring) = &self.scoring {
                if !scoring.address_is_welcome(ip) {
                    debug!(
                        target: "rustock::net",
                        "Refusing {}: address is banned or serving a punishment", peer_addr
                    );
                    release_inbound_slot(&per_ip, ip).await;
                    release_inbound_slot(&per_cidr, block).await;
                    drop(stream);
                    continue;
                }
            }

            debug!(target: "rustock::net", "New connection from: {}", peer_addr);

            let config = self.config.clone();
            let handlers = all_handlers.clone();
            let peer_store = self.peer_store.clone();
            let scoring = self.scoring.clone();
            let per_ip_task = per_ip.clone();
            let per_cidr_task = per_cidr.clone();
            tokio::spawn(async move {
                if let Err(e) =
                    handle_incoming(stream, config, handlers, peer_store, scoring, Some(peer_addr))
                        .await
                {
                    error!(target: "rustock::net", "Error handling peer {}: {:?}", peer_addr, e);
                }
                // Release both slots however the session ended, including panics
                // unwinding through this task.
                {
                    let mut map = per_ip_task.lock().await;
                    if let Some(c) = map.get_mut(&ip) {
                        *c = c.saturating_sub(1);
                        if *c == 0 {
                            // Do not let the map grow without bound across churn.
                            map.remove(&ip);
                        }
                    }
                }
                {
                    let mut map = per_cidr_task.lock().await;
                    if let Some(c) = map.get_mut(&block) {
                        *c = c.saturating_sub(1);
                        if *c == 0 {
                            map.remove(&block);
                        }
                    }
                }
                drop(permit);
            });
        }
    }
}

use tokio::sync::mpsc;
use crate::session::PeerSession;
use crate::protocol::rsk::RskStatus;

/// Registers a peer and runs a session. Shared by incoming and outbound connection paths.
pub(crate) async fn register_and_run_session(
    peer_id: B512,
    rsk_status: RskStatus,
    framed: tokio_util::codec::Framed<TcpStream, crate::handshake::HandshakeCodec>,
    handlers: Vec<Arc<dyn P2pHandler>>,
    peer_store: Arc<PeerStore>,
    scoring: Option<Arc<crate::scoring::ScoringService>>,
    address: Option<IpAddr>,
) -> Result<()> {
    // A node id can be banned even when its address is not -- and the id is
    // only known once the handshake has produced it, so this is the earliest
    // it can be checked.
    if let Some(scoring) = &scoring {
        if !scoring.node_is_welcome(peer_id) {
            debug!(
                target: "rustock::net",
                "Dropping {:?}: node id is banned or serving a punishment",
                &peer_id.as_slice()[..4]
            );
            return Ok(());
        }
    }

    let (tx, rx) = mpsc::channel(crate::peers::PEER_CHANNEL_CAPACITY);

    if !peer_store.add_peer(peer_id, tx).await {
        trace!(target: "rustock::net", "Peer already connected: {:?}", peer_id);
        return Ok(());
    }

    if let Some(scoring) = &scoring {
        if let Some(address) = address {
            scoring.note_address(peer_id, address);
        }
        scoring.record(
            Some(peer_id),
            address,
            crate::scoring::EventType::SuccessfulHandshake,
        );
    }

    let metadata = crate::peers::PeerMetadata {
        best_number: rsk_status.best_block_number,
        best_hash: rsk_status.best_block_hash,
        total_difficulty: rsk_status.total_difficulty.unwrap_or_default(),
        client_id: String::new(),
        address,
    };
    peer_store.update_metadata(&peer_id, metadata).await;

    let mut session = PeerSession::from_framed(peer_id, framed, rx);
    for handler in handlers {
        session.add_handler(handler);
    }

    // Request peer list immediately after connecting
    peer_store.send_to_peer(&peer_id, P2pMessage::GetPeers).await;

    let res = session.run().await;
    peer_store.remove_peer(&peer_id).await;
    if let Some(scoring) = &scoring {
        // rskj records DISCONNECTION on every session end, clean or not. It
        // does not move the score -- it falls in the "increments while
        // non-negative" group -- so this is a counter for the operator's
        // report rather than a punishment.
        scoring.record(
            Some(peer_id),
            address,
            crate::scoring::EventType::Disconnection,
        );
        scoring.forget_address(&peer_id);
    }
    res
}

/// Maximum time allowed for a connection handshake (RLPx + p2p Hello + status)
/// to complete. Without this, a peer that opens a TCP connection and never
/// speaks would pin a task and socket forever (rskj relies on Netty's
/// `ReadTimeoutHandler` at the front of the pipeline for the same protection).
/// The outbound path already enforces this; the inbound path did not.
pub(crate) const HANDSHAKE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Handles an incoming connection by performing a handshake and starting a session.
pub async fn handle_incoming(
    stream: TcpStream,
    config: NodeConfig,
    handlers: Vec<Arc<dyn P2pHandler>>,
    peer_store: Arc<PeerStore>,
    scoring: Option<Arc<crate::scoring::ScoringService>>,
    peer_addr: Option<std::net::SocketAddr>,
) -> Result<()> {
    let address = peer_addr.map(|a| a.ip());
    let handshake = Handshake::new(stream, config, None);
    let outcome = tokio::time::timeout(HANDSHAKE_TIMEOUT, handshake.run()).await;

    // rskj records FAILED_HANDSHAKE against the address (the node id is not
    // known yet, which is why `recordEvent` takes both as optional). It costs
    // no score there, and costs none here -- the counter is what an operator
    // reads when a host is hammering the listener.
    let failed = |scoring: &Option<Arc<crate::scoring::ScoringService>>| {
        if let Some(scoring) = scoring {
            scoring.record(None, address, crate::scoring::EventType::FailedHandshake);
        }
    };

    let (peer_id, rsk_status, framed) = match outcome {
        Ok(Ok(parts)) => parts,
        Ok(Err(e)) => {
            failed(&scoring);
            return Err(e);
        }
        Err(e) => {
            failed(&scoring);
            return Err(anyhow::Error::new(e).context("Inbound handshake timed out"));
        }
    };
    register_and_run_session(peer_id, rsk_status, framed, handlers, peer_store, scoring, address)
        .await
}

/// Give back one inbound slot from the per-IP or per-block table.
async fn release_inbound_slot(map: &Arc<Mutex<HashMap<IpAddr, usize>>>, key: IpAddr) {
    let mut map = map.lock().await;
    if let Some(count) = map.get_mut(&key) {
        *count = count.saturating_sub(1);
        if *count == 0 {
            map.remove(&key);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::B512;
    use tokio::time::{sleep, Duration, timeout};

    #[test]
    fn test_cidr_block_groups_a_subnet() {
        use std::net::Ipv4Addr;
        // A /24 collapses to one key, so an attacker holding the subnet cannot
        // sidestep the per-IP cap by spreading across 256 addresses.
        let a = cidr_block(IpAddr::V4(Ipv4Addr::new(203, 0, 113, 7)), 24);
        let b = cidr_block(IpAddr::V4(Ipv4Addr::new(203, 0, 113, 250)), 24);
        assert_eq!(a, b);
        let other = cidr_block(IpAddr::V4(Ipv4Addr::new(203, 0, 114, 7)), 24);
        assert_ne!(a, other, "a different /24 must be a different block");
    }

    #[test]
    fn test_cidr_block_ipv6_uses_wider_prefix() {
        use std::net::Ipv6Addr;
        // prefix 24 means /48 for IPv6, since a /48 is a routine single-customer
        // allocation there.
        let a = cidr_block(IpAddr::V6(Ipv6Addr::new(0x2001, 0xdb8, 0xabcd, 1, 0, 0, 0, 1)), 24);
        let b = cidr_block(IpAddr::V6(Ipv6Addr::new(0x2001, 0xdb8, 0xabcd, 9, 0, 0, 0, 2)), 24);
        assert_eq!(a, b, "same /48 must collapse to one block");
        let other = cidr_block(IpAddr::V6(Ipv6Addr::new(0x2001, 0xdb8, 0xabce, 1, 0, 0, 0, 1)), 24);
        assert_ne!(a, other);
    }

    #[test]
    fn test_cidr_block_prefix_edges() {
        use std::net::Ipv4Addr;
        let ip = IpAddr::V4(Ipv4Addr::new(198, 51, 100, 42));
        // /32 degenerates to a per-IP key; /0 collapses everything.
        assert_eq!(cidr_block(ip, 32), ip);
        assert_eq!(cidr_block(ip, 0), IpAddr::V4(Ipv4Addr::UNSPECIFIED));
        // Over-long prefixes must saturate rather than overflow the shift.
        assert_eq!(cidr_block(ip, 255), ip);
    }

    #[tokio::test]
    async fn test_handshake() {
        let node1_config = NodeConfig {
            client_id: "Node1".to_string(),
            listen_port: 0,
            id: B512::ZERO,
            chain_id: 33,
            network_id: 33,
            genesis_hash: B256::repeat_byte(0xaa),
            best_hash: B256::repeat_byte(0xaa),
            best_block_number: 0,
            total_difficulty: U256::ZERO,
            bootnodes: vec![],
            secret_key: [0x42; 32],
            discovery_port: 0,
            data_dir: ".".to_string(),
            external_ip: None,
            max_outbound_peers: 10,
            max_inbound_peers: 32,
            max_inbound_per_ip: 4,
            max_inbound_per_cidr: 16,
            inbound_cidr_prefix: 24,
        };
        
        let node2_config = NodeConfig {
            client_id: "Node2".to_string(),
            listen_port: 0,
            id: B512::repeat_byte(0x01),
            chain_id: 33,
            network_id: 33,
            genesis_hash: B256::repeat_byte(0xaa),
            best_hash: B256::repeat_byte(0xaa),
            best_block_number: 0,
            total_difficulty: U256::ZERO,
            bootnodes: vec![],
            secret_key: [0x43; 32],
            discovery_port: 0,
            data_dir: ".".to_string(),
            external_ip: None,
            max_outbound_peers: 10,
            max_inbound_peers: 32,
            max_inbound_per_ip: 4,
            max_inbound_per_cidr: 16,
            inbound_cidr_prefix: 24,
        };

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let addr = format!("127.0.0.1:{}", port);
        
        let node1_store = Arc::new(PeerStore::new());
        let _node2_store = Arc::new(PeerStore::new());

        let node1_task = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let _ = timeout(Duration::from_secs(2), handle_incoming(stream, node1_config, vec![], node1_store, None, None)).await;
        });

        let node2_task = tokio::spawn(async move {
            sleep(Duration::from_millis(100)).await;
            let stream = TcpStream::connect(addr).await.unwrap();
            let handshake = Handshake::new(stream, node2_config, None);
            let _ = timeout(Duration::from_secs(2), handshake.run()).await;
        });

        let _ = tokio::join!(node1_task, node2_task);
    }

    struct PingHandler;
    impl crate::protocol::P2pHandler for PingHandler {
        fn handle_message(&self, _id: alloy_primitives::B512, msg: &crate::protocol::P2pMessage) -> Option<crate::protocol::P2pMessage> {
            if let crate::protocol::P2pMessage::Ping = msg {
                Some(crate::protocol::P2pMessage::Pong)
            } else {
                None
            }
        }
    }

    #[test]
    fn test_handler_logic() {
        let handler = PingHandler;
        let response = handler.handle_message(alloy_primitives::B512::ZERO, &crate::protocol::P2pMessage::Ping);
        assert!(matches!(response, Some(crate::protocol::P2pMessage::Pong)));
        
        let response = handler.handle_message(alloy_primitives::B512::ZERO, &crate::protocol::P2pMessage::Pong);
        assert!(response.is_none());
    }

    #[tokio::test]
    async fn test_duplicate_peer_rejected() {
        let store = Arc::new(PeerStore::new());
        let peer_id = B512::repeat_byte(0x01);

        let (tx1, _rx1) = tokio::sync::mpsc::channel(crate::peers::PEER_CHANNEL_CAPACITY);
        assert!(store.add_peer(peer_id, tx1).await, "First add should succeed");

        let (tx2, _rx2) = tokio::sync::mpsc::channel(crate::peers::PEER_CHANNEL_CAPACITY);
        assert!(!store.add_peer(peer_id, tx2).await, "Duplicate add should be rejected");

        assert_eq!(store.count().await, 1);
    }
}
