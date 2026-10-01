use crate::protocol::{P2pMessage, HelloMessage, P2P_VERSION, EthStatus, RskStatus, RskMessage, RskSubMessage, Capability};
use crate::protocol::rsk::RSK_RANGE_VERSION;

/// What a peer said it speaks, reduced to the two questions this node asks.
///
/// Absence is not refusal. An rskj peer advertises neither `rsk/63` nor a
/// block range, and must be treated as serving everything -- the same rule
/// geth applies to a peer that never announced a range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PeerCapabilities {
    /// Highest `rsk` version both ends speak.
    pub rsk_version: u64,
    /// The peer offered `snap/1`.
    pub snap: bool,
}

impl Default for PeerCapabilities {
    fn default() -> Self {
        Self { rsk_version: 62, snap: false }
    }
}

impl PeerCapabilities {
    /// The highest `rsk` version in common, and whether `snap` was offered.
    ///
    /// Same rule rskj applies: intersect with what this node supports, then
    /// take the highest. A peer offering `rsk/64` gets `rsk/63` from us, not
    /// an error -- which is exactly why offering `rsk/63` to rskj is safe.
    pub fn negotiate(peer: &[Capability]) -> Self {
        let rsk_version = peer
            .iter()
            .filter(|c| c.name == "rsk")
            .map(|c| c.version)
            .filter(|v| *v <= RSK_RANGE_VERSION)
            .max()
            .unwrap_or(62);
        Self {
            rsk_version,
            snap: peer.iter().any(|c| c.name == "snap"),
        }
    }

    /// Whether this peer can be sent the served-range extension.
    pub fn understands_block_range(&self) -> bool {
        self.rsk_version >= RSK_RANGE_VERSION
    }
}
use crate::node::NodeConfig;
use crate::codec::P2pCodec;
use anyhow::{Result, Context};
use tokio::net::TcpStream;
use tokio_util::codec::Framed;
use futures::{StreamExt, SinkExt};
use alloy_primitives::B512;
use tracing::trace;

use crate::rlpx::{RLPxHandshake, RLPxCodec};
use tokio_util::codec::{Decoder, Encoder};
use bytes::BytesMut;

pub enum HandshakeCodec {
    Plain(P2pCodec),
    RLPx(Box<RLPxCodec>),
}

impl Decoder for HandshakeCodec {
    type Item = P2pMessage;
    type Error = anyhow::Error;
    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Self::Item>> {
        match self {
            Self::Plain(c) => c.decode(src),
            Self::RLPx(c) => c.decode(src),
        }
    }
}

impl Encoder<P2pMessage> for HandshakeCodec {
    type Error = anyhow::Error;
    fn encode(&mut self, item: P2pMessage, dst: &mut BytesMut) -> Result<()> {
        match self {
            Self::Plain(c) => c.encode(item, dst),
            Self::RLPx(c) => c.encode(item, dst),
        }
    }
}

pub struct Handshake {
    stream: TcpStream,
    config: NodeConfig,
    remote_id: Option<alloy_primitives::B512>,
}

impl Handshake {
    pub fn new(stream: TcpStream, config: NodeConfig, remote_id: Option<alloy_primitives::B512>) -> Self {
        Self {
            stream,
            config,
            remote_id,
        }
    }

    /// Performs the full P2P and RSK blockchain handshake.
    pub async fn run(
        self,
    ) -> Result<(
        alloy_primitives::B512,
        RskStatus,
        PeerCapabilities,
        Framed<TcpStream, HandshakeCodec>,
    )> {
        let stream = self.stream;
        let config = self.config;
        let remote_id = self.remote_id;

        if let Some(remote_pk) = remote_id {
            trace!(target: "rustock::net", "Attempting RLPx handshake with {:?}", remote_pk);
            let rlpx = RLPxHandshake::new(stream, config.clone(), remote_pk);
            let (peer_id, frame_codec, stream) = rlpx.run_initiator().await.context("RLPx handshake failed")?;
            
            let codec = HandshakeCodec::RLPx(Box::new(RLPxCodec::new(frame_codec)));
            let mut framed = Framed::new(stream, codec);
            
            let (rsk_status, caps) = Self::p2p_handshake(&config, &mut framed).await?;
            Ok((peer_id, rsk_status, caps, framed))
        } else {
            trace!(target: "rustock::net", "Awaiting inbound RLPx handshake");
            let rlpx = RLPxHandshake::new(stream, config.clone(), B512::ZERO);
            let (peer_id, frame_codec, stream) = rlpx.run_responder().await.context("Inbound RLPx handshake failed")?;

            let codec = HandshakeCodec::RLPx(Box::new(RLPxCodec::new(frame_codec)));
            let mut framed = Framed::new(stream, codec);

            let (_, rsk_status, caps) = Self::p2p_handshake_inbound(&config, &mut framed).await?;
            Ok((peer_id, rsk_status, caps, framed))
        }
    }

    async fn p2p_handshake<S>(config: &NodeConfig, framed: &mut S) -> Result<(RskStatus, PeerCapabilities)> 
    where S: StreamExt<Item = Result<P2pMessage, anyhow::Error>> + SinkExt<P2pMessage, Error = anyhow::Error> + Unpin
    {
        Self::send_hello(config, framed).await?;
        let (_peer_id, caps) = Self::receive_hello(framed).await?;
        
        Self::send_status(config, framed).await?;
        let status = Self::receive_status(config, framed).await?;

        Ok((status, caps))
    }

    async fn p2p_handshake_inbound<S>(config: &NodeConfig, framed: &mut S) -> Result<(alloy_primitives::B512, RskStatus, PeerCapabilities)> 
    where S: StreamExt<Item = Result<P2pMessage, anyhow::Error>> + SinkExt<P2pMessage, Error = anyhow::Error> + Unpin
    {
        let (peer_id, caps) = Self::receive_hello(framed).await?;
        Self::send_hello(config, framed).await?;
        
        let status = Self::receive_status(config, framed).await?;
        Self::send_status(config, framed).await?;

        Ok((peer_id, status, caps))
    }

    /// What this node offers in the handshake.
    ///
    /// `rsk/62` is what rskj speaks. `rsk/63` adds the served block range: the
    /// fifth status element and the `BlockRangeUpdate` message. Advertising
    /// both is safe because rskj intersects a peer's capabilities with its own
    /// before picking the highest, so an unknown version is dropped rather
    /// than chosen -- checked in `ConfigCapabilitiesImpl.getSupportedCapabilities`.
    ///
    /// `snap/1` is rskj's own constant, offered when this node has anything to
    /// do with snapshots. It is not invented here.
    fn capabilities(config: &NodeConfig) -> Vec<Capability> {
        let mut caps = vec![
            Capability { name: "rsk".to_string(), version: 62 },
            Capability { name: "rsk".to_string(), version: RSK_RANGE_VERSION },
        ];
        if config.snap_capability {
            caps.push(Capability { name: "snap".to_string(), version: 1 });
        }
        caps
    }

    async fn send_hello<S>(config: &NodeConfig, framed: &mut S) -> Result<()> 
    where S: SinkExt<P2pMessage, Error = anyhow::Error> + Unpin
    {
        let hello = HelloMessage {
            protocol_version: P2P_VERSION,
            client_id: config.client_id.clone(),
            capabilities: Self::capabilities(config),
            listen_port: config.listen_port,
            id: config.id,
        };
        framed.send(P2pMessage::Hello(hello)).await.context("Failed to send Hello")
    }

    async fn receive_hello<S>(framed: &mut S) -> Result<(alloy_primitives::B512, PeerCapabilities)>
    where S: StreamExt<Item = Result<P2pMessage, anyhow::Error>> + Unpin
    {
        let msg = framed.next().await
            .context("Connection closed waiting for Hello")??;
        
        if let P2pMessage::Hello(peer_hello) = msg {
            trace!(target: "rustock::net", "P2P Handshake successful with peer: {}", peer_hello.client_id);
            let caps = PeerCapabilities::negotiate(&peer_hello.capabilities);
            Ok((peer_hello.id, caps))
        } else {
            Err(anyhow::anyhow!("Expected Hello, got {:?}", msg))
        }
    }

    async fn send_status<S>(config: &NodeConfig, framed: &mut S) -> Result<()> 
    where S: SinkExt<P2pMessage, Error = anyhow::Error> + Unpin
    {
        let status = EthStatus {
            protocol_version: 0x3e, // RSK protocol version V62
            network_id: config.network_id,
            total_difficulty: config.total_difficulty,
            best_hash: config.best_hash,
            genesis_hash: config.genesis_hash,
        };
        framed.send(P2pMessage::EthStatus(status)).await?;
        
        let rsk_status = RskStatus {
            best_block_number: config.best_block_number,
            best_block_hash: config.best_hash,
            best_block_parent_hash: config.best_block_parent_hash,
            total_difficulty: Some(config.total_difficulty),
            // Only when the parent is known: the list is positional, so a
            // fifth element without the third and fourth is not a message
            // anyone can read.
            earliest_block: config
                .best_block_parent_hash
                .map(|_| config.earliest_block),
        };
        framed.send(P2pMessage::RskMessage(RskMessage::new(RskSubMessage::Status(rsk_status)))).await?;
        Ok(())
    }

    async fn receive_status<S>(config: &NodeConfig, framed: &mut S) -> Result<RskStatus> 
    where S: StreamExt<Item = Result<P2pMessage, anyhow::Error>> + Unpin
    {
        // Wait for EthStatus
        let eth_msg = framed.next().await
            .context("Connection closed waiting for EthStatus")??;
        
        if let P2pMessage::EthStatus(s) = eth_msg {
            if s.genesis_hash != config.genesis_hash {
                return Err(anyhow::anyhow!("Genesis hash mismatch: expected {:?}, got {:?}", config.genesis_hash, s.genesis_hash));
            }
            trace!(target: "rustock::net", "Peer EthStatus: best_hash={:?}", s.best_hash);
        } else {
            return Err(anyhow::anyhow!("Expected EthStatus, got {:?}", eth_msg));
        }

        // Wait for RskStatus
        let rsk_msg = framed.next().await
            .context("Connection closed waiting for RskStatus")??;
        
        if let P2pMessage::RskMessage(m) = rsk_msg {
            if let RskSubMessage::Status(s) = m.sub_message {
                trace!(target: "rustock::net", "RSK Handshake successful: peer at block {}", s.best_block_number);
                Ok(s)
            } else {
                Err(anyhow::anyhow!("Expected RskStatus, got {:?}", m.sub_message))
            }
        } else {
            Err(anyhow::anyhow!("Expected RskMessage, got {:?}", rsk_msg))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{B512, B256, U256};
    use k256::SecretKey;
    use k256::elliptic_curve::sec1::ToEncodedPoint;
    use tokio::net::TcpListener;

    fn secret_to_public(sk_bytes: &[u8; 32]) -> B512 {
        let sk = SecretKey::from_slice(sk_bytes).unwrap();
        let pk_encoded = sk.public_key().to_encoded_point(false);
        let mut pk_64 = [0u8; 64];
        pk_64.copy_from_slice(&pk_encoded.as_bytes()[1..]);
        B512::from_slice(&pk_64)
    }

    fn mock_config_with_key(genesis: B256, sk: [u8; 32]) -> NodeConfig {
        let id = secret_to_public(&sk);
        NodeConfig {
            client_id: "test".to_string(),
            listen_port: 0,
            id,
            chain_id: 33,
            network_id: 33,
            genesis_hash: genesis,
            best_hash: genesis,
            best_block_number: 0,
            total_difficulty: U256::ZERO,
            bootnodes: vec![],
            closed_network: false,
        read_only: false,
            secret_key: sk,
            discovery_port: 0,
            data_dir: ".".to_string(),
            external_ip: None,
            max_outbound_peers: 10,
            max_inbound_peers: 32,
            max_inbound_per_ip: 4,
            max_inbound_per_cidr: 16,
            inbound_cidr_prefix: 24,
        best_block_parent_hash: None,
        earliest_block: 0,
        snap_capability: false,
        }
    }

    pub(super) fn mock_config(genesis: B256) -> NodeConfig {
        mock_config_with_key(genesis, [0x11; 32])
    }

    #[tokio::test]
    async fn test_handshake_genesis_mismatch() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        
        let genesis1 = B256::repeat_byte(0x11);
        let genesis2 = B256::repeat_byte(0x22);
        
        let client_task = tokio::spawn(async move {
            let stream = TcpStream::connect(addr).await.unwrap();
            let config = mock_config(genesis1);
            let handshake = Handshake::new(stream, config.clone(), None);
            let mut framed = tokio_util::codec::Framed::new(handshake.stream, HandshakeCodec::Plain(P2pCodec));
            Handshake::send_hello(&config, &mut framed).await.unwrap();
            let _ = Handshake::receive_hello(&mut framed).await.unwrap();
            Handshake::send_status(&config, &mut framed).await.unwrap();
            Handshake::receive_status(&config, &mut framed).await
        });

        let server_task = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let config = mock_config(genesis2);
            let handshake = Handshake::new(stream, config.clone(), None);
            let mut framed = tokio_util::codec::Framed::new(handshake.stream, HandshakeCodec::Plain(P2pCodec));
            let _ = Handshake::receive_hello(&mut framed).await.unwrap();
            Handshake::send_hello(&config, &mut framed).await.unwrap();
            Handshake::receive_status(&config, &mut framed).await
        });

        let (res1, res2) = tokio::join!(client_task, server_task);
        let res1: Result<crate::protocol::RskStatus, anyhow::Error> = res1.unwrap();
        let res2: Result<crate::protocol::RskStatus, anyhow::Error> = res2.unwrap();
        
        assert!(res1.is_err());
        assert!(res2.is_err());
        
        let err1 = res1.unwrap_err().to_string();
        let err2 = res2.unwrap_err().to_string();
        
        assert!(err1.contains("Genesis hash mismatch") || err1.contains("Connection closed") || err1.contains("Connection reset"));
        assert!(err2.contains("Genesis hash mismatch") || err2.contains("Connection closed") || err2.contains("Connection reset"));
    }

    /// Full end-to-end test: outbound initiator (RLPx) connects to inbound
    /// responder (RLPx), both go through `Handshake::run()`.
    #[tokio::test]
    async fn test_inbound_rlpx_handshake() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let genesis = B256::repeat_byte(0xAA);
        let sk_client: [u8; 32] = [0x11; 32];
        let sk_server: [u8; 32] = [0x22; 32];
        let pk_server = secret_to_public(&sk_server);
        let pk_client = secret_to_public(&sk_client);

        let client_config = mock_config_with_key(genesis, sk_client);
        let server_config = mock_config_with_key(genesis, sk_server);

        let client_task = tokio::spawn(async move {
            let stream = TcpStream::connect(addr).await.unwrap();
            let handshake = Handshake::new(stream, client_config, Some(pk_server));
            handshake.run().await
        });

        let server_task = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let handshake = Handshake::new(stream, server_config, None);
            handshake.run().await
        });

        let (client_res, server_res) = tokio::join!(client_task, server_task);
        let (client_peer_id, client_rsk_status, _, _) = client_res.unwrap().unwrap();
        let (server_peer_id, server_rsk_status, _, _) = server_res.unwrap().unwrap();

        // Each side should see the other's public key
        assert_eq!(client_peer_id, pk_server);
        assert_eq!(server_peer_id, pk_client);

        // Both sides should have exchanged RSK status
        assert_eq!(client_rsk_status.best_block_number, 0);
        assert_eq!(server_rsk_status.best_block_number, 0);
    }
}

#[cfg(test)]
mod capability_tests {
    use super::*;
    use super::tests::mock_config;
    use alloy_primitives::B256;

    fn cap(name: &str, version: u64) -> Capability {
        Capability { name: name.to_string(), version }
    }

    /// An rskj peer offers `rsk/62` and `snap/1`, and must come out at 62.
    ///
    /// Sending it the extension would cost the connection: rskj throws
    /// `IllegalArgumentException` out of `MessageType.valueOfType` for any
    /// type it does not know, and `P2pHandler.exceptionCaught` answers with
    /// `ctx.close()`.
    #[test]
    fn an_rskj_peer_negotiates_the_old_version() {
        let caps = PeerCapabilities::negotiate(&[cap("rsk", 62), cap("snap", 1)]);
        assert_eq!(caps.rsk_version, 62);
        assert!(caps.snap, "rskj offers snap/1 whenever it speaks rsk");
        assert!(
            !caps.understands_block_range(),
            "an rskj peer must never be sent the extension"
        );
    }

    /// Another rustock node offers both, and the pair settle on the newer.
    #[test]
    fn two_rustock_nodes_negotiate_the_extension() {
        let caps = PeerCapabilities::negotiate(&[cap("rsk", 62), cap("rsk", 63)]);
        assert_eq!(caps.rsk_version, 63);
        assert!(caps.understands_block_range());
    }

    /// A future peer offering a version this node does not have settles on the
    /// highest it does — the same rule rskj applies to us, and the reason
    /// offering `rsk/63` to rskj is safe rather than hopeful.
    #[test]
    fn a_newer_peer_settles_on_what_this_node_speaks() {
        let caps = PeerCapabilities::negotiate(&[cap("rsk", 62), cap("rsk", 63), cap("rsk", 99)]);
        assert_eq!(caps.rsk_version, RSK_RANGE_VERSION);
    }

    /// A peer that offers no rsk capability at all still yields a usable
    /// default rather than panicking; the handshake rejects it elsewhere.
    #[test]
    fn an_empty_capability_list_defaults_to_the_old_version() {
        let caps = PeerCapabilities::negotiate(&[]);
        assert_eq!(caps.rsk_version, 62);
        assert!(!caps.snap);
        assert!(!caps.understands_block_range());
    }

    /// What this node offers: both rsk versions always, snap only when it has
    /// something to do with snapshots.
    #[test]
    fn this_node_offers_both_versions_and_snap_only_when_relevant() {
        let mut config = mock_config(B256::ZERO);
        config.snap_capability = false;
        let caps = Handshake::capabilities(&config);
        assert!(caps.iter().any(|c| c.name == "rsk" && c.version == 62));
        assert!(caps.iter().any(|c| c.name == "rsk" && c.version == 63));
        assert!(!caps.iter().any(|c| c.name == "snap"));

        config.snap_capability = true;
        let caps = Handshake::capabilities(&config);
        assert!(
            caps.iter().any(|c| c.name == "snap" && c.version == 1),
            "snap/1 is rskj's own constant, not one invented here"
        );
    }
}
