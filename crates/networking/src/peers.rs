use tracing::warn;
use std::collections::HashMap;
use tokio::sync::{Mutex, mpsc};
use alloy_primitives::{B512, U256, B256};
use crate::protocol::P2pMessage;

/// Metadata about a connected peer.
#[derive(Debug, Clone, Default)]
pub struct PeerMetadata {
    pub best_number: u64,
    pub best_hash: B256,
    pub total_difficulty: U256,
    pub client_id: String,
}

struct PeerState {
    sender: mpsc::Sender<P2pMessage>,
    metadata: PeerMetadata,
}

/// Depth of each peer's outbound queue.
///
/// This was previously an unbounded channel, which let a peer that completed the
/// handshake and then simply stopped reading drive our memory arbitrarily high:
/// transaction and block broadcast fan out to every connected peer, so the queue
/// for one stalled peer grows for as long as the node keeps producing traffic.
/// A bounded queue converts that into backpressure we can act on -- we drop the
/// message, and the peer falls behind rather than the node falling over.
pub const PEER_CHANNEL_CAPACITY: usize = 1024;

/// Thread-safe store for tracking active peer connections and their outbound senders.
#[derive(Default)]
pub struct PeerStore {
    connected_peers: Mutex<HashMap<B512, PeerState>>,
}

impl PeerStore {
    pub fn new() -> Self {
        Self { connected_peers: Mutex::new(HashMap::new()) }
    }

    /// Attempts to add a peer to the store. Returns true if it was newly added.
    pub async fn add_peer(&self, id: B512, sender: mpsc::Sender<P2pMessage>) -> bool {
        let mut peers = self.connected_peers.lock().await;
        use std::collections::hash_map::Entry;
        match peers.entry(id) {
            Entry::Occupied(_) => false,
            Entry::Vacant(e) => {
                e.insert(PeerState { sender, metadata: PeerMetadata::default() });
                true
            }
        }
    }

    /// Updates the metadata for a peer.
    pub async fn update_metadata(&self, id: &B512, metadata: PeerMetadata) {
        let mut peers = self.connected_peers.lock().await;
        if let Some(state) = peers.get_mut(id) {
            state.metadata = metadata;
        }
    }

    /// Returns the metadata for a specific peer.
    pub async fn metadata(&self, id: &B512) -> Option<PeerMetadata> {
        let peers = self.connected_peers.lock().await;
        peers.get(id).map(|s| s.metadata.clone())
    }

    /// Finds the best peer to sync from based on total difficulty.
    pub async fn best_peer(&self) -> Option<(B512, PeerMetadata)> {
        let peers = self.connected_peers.lock().await;
        peers.iter()
            .max_by_key(|(_, s)| s.metadata.total_difficulty)
            .map(|(id, s)| (*id, s.metadata.clone()))
    }

    /// Removes a peer from the store.
    pub async fn remove_peer(&self, id: &B512) {
        self.connected_peers.lock().await.remove(id);
    }

    /// Checks if a peer is already connected.
    pub async fn is_connected(&self, id: &B512) -> bool {
        self.connected_peers.lock().await.contains_key(id)
    }

    /// Sends a message to a specific peer.
    pub async fn send_to_peer(&self, id: &B512, msg: P2pMessage) -> bool {
        let peers = self.connected_peers.lock().await;
        if let Some(state) = peers.get(id) {
            // try_send never awaits: a peer that has stopped reading must not be
            // able to stall the caller or grow its queue without limit. A full
            // queue means the peer is not keeping up, so the message is dropped.
            match state.sender.try_send(msg) {
                Ok(()) => true,
                Err(mpsc::error::TrySendError::Full(_)) => {
                    warn!(target: "rustock::net", "Outbound queue full for peer {:?}; dropping message", id);
                    false
                }
                Err(mpsc::error::TrySendError::Closed(_)) => false,
            }
        } else {
            false
        }
    }

    /// Returns a list of all connected peer IDs.
    pub async fn peers(&self) -> Vec<B512> {
        self.connected_peers.lock().await.keys().cloned().collect()
    }
    
    /// Returns the number of active peer connections.
    pub async fn count(&self) -> usize {
        self.connected_peers.lock().await.len()
    }

    /// Broadcasts a message to all connected peers except those in `exclude`.
    /// Returns the number of peers the message was sent to.
    pub async fn broadcast(&self, msg: P2pMessage, exclude: &[B512]) -> usize {
        let peers = self.connected_peers.lock().await;
        peers.iter()
            .filter(|(id, _)| !exclude.contains(id))
            .filter(|(_, state)| state.sender.try_send(msg.clone()).is_ok())
            .count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{B256, B512, U256};
    use tokio::sync::mpsc;

    #[tokio::test]
    async fn test_get_best_peer_by_total_difficulty() {
        let store = PeerStore::new();
        let peer_a = B512::repeat_byte(0x0a);
        let peer_b = B512::repeat_byte(0x0b);
        let (tx_a, _rx_a) = mpsc::channel(PEER_CHANNEL_CAPACITY);
        let (tx_b, _rx_b) = mpsc::channel(PEER_CHANNEL_CAPACITY);

        store.add_peer(peer_a, tx_a).await;
        store.add_peer(peer_b, tx_b).await;

        store.update_metadata(&peer_a, PeerMetadata {
            total_difficulty: U256::from(100),
            best_number: 10,
            ..Default::default()
        }).await;
        store.update_metadata(&peer_b, PeerMetadata {
            total_difficulty: U256::from(200),
            best_number: 20,
            ..Default::default()
        }).await;

        let best = store.best_peer().await.unwrap();
        assert_eq!(best.0, peer_b, "peer B has higher TD");

        store.update_metadata(&peer_a, PeerMetadata {
            total_difficulty: U256::from(300),
            best_number: 10,
            ..Default::default()
        }).await;

        let best = store.best_peer().await.unwrap();
        assert_eq!(best.0, peer_a, "peer A now has higher TD");
    }

    #[tokio::test]
    async fn test_get_best_peer_empty() {
        let store = PeerStore::new();
        assert!(store.best_peer().await.is_none());
    }

    #[tokio::test]
    async fn test_update_and_get_metadata() {
        let store = PeerStore::new();
        let peer_id = B512::repeat_byte(0x01);
        let (tx, _rx) = mpsc::channel(PEER_CHANNEL_CAPACITY);
        store.add_peer(peer_id, tx).await;

        let metadata = PeerMetadata {
            best_number: 42,
            best_hash: B256::repeat_byte(0x11),
            total_difficulty: U256::from(999),
            client_id: "test".to_string(),
        };
        store.update_metadata(&peer_id, metadata.clone()).await;

        let retrieved = store.metadata(&peer_id).await.unwrap();
        assert_eq!(retrieved.best_number, 42);
        assert_eq!(retrieved.best_hash, B256::repeat_byte(0x11));
        assert_eq!(retrieved.total_difficulty, U256::from(999));
        assert_eq!(retrieved.client_id, "test");

        let unknown = B512::repeat_byte(0xff);
        assert!(store.metadata(&unknown).await.is_none());
    }

    #[tokio::test]
    async fn test_peer_store_flow() {
        let store = PeerStore::new();
        let id1 = B512::repeat_byte(0x01);
        let id2 = B512::repeat_byte(0x02);
        let (tx1, mut rx1) = mpsc::channel(PEER_CHANNEL_CAPACITY);
        let (tx2, _rx2) = mpsc::channel(PEER_CHANNEL_CAPACITY);

        // Add
        assert!(store.add_peer(id1, tx1).await);
        assert!(store.add_peer(id2, tx2).await);
        assert!(!store.add_peer(id1, mpsc::channel(PEER_CHANNEL_CAPACITY).0).await); // Duplicate

        assert_eq!(store.count().await, 2);
        assert!(store.is_connected(&id1).await);
        
        // Get peers
        let peers = store.peers().await;
        assert_eq!(peers.len(), 2);
        assert!(peers.contains(&id1));
        assert!(peers.contains(&id2));

        // Send
        assert!(store.send_to_peer(&id1, P2pMessage::Ping).await);
        let msg = rx1.recv().await.unwrap();
        assert!(matches!(msg, P2pMessage::Ping));

        // Remove
        store.remove_peer(&id1).await;
        assert_eq!(store.count().await, 1);
        assert!(!store.is_connected(&id1).await);
        assert!(!store.send_to_peer(&id1, P2pMessage::Ping).await);
    }

    #[tokio::test]
    async fn test_broadcast_sends_to_all_except_excluded() {
        let store = PeerStore::new();
        let id1 = B512::repeat_byte(0x01);
        let id2 = B512::repeat_byte(0x02);
        let id3 = B512::repeat_byte(0x03);
        let (tx1, mut rx1) = mpsc::channel(PEER_CHANNEL_CAPACITY);
        let (tx2, mut rx2) = mpsc::channel(PEER_CHANNEL_CAPACITY);
        let (tx3, mut rx3) = mpsc::channel(PEER_CHANNEL_CAPACITY);

        store.add_peer(id1, tx1).await;
        store.add_peer(id2, tx2).await;
        store.add_peer(id3, tx3).await;

        let sent = store.broadcast(P2pMessage::Ping, &[id2]).await;
        assert_eq!(sent, 2);

        assert!(rx1.try_recv().is_ok());
        assert!(rx2.try_recv().is_err());
        assert!(rx3.try_recv().is_ok());
    }

    #[tokio::test]
    async fn test_broadcast_empty_exclude() {
        let store = PeerStore::new();
        let id1 = B512::repeat_byte(0x01);
        let id2 = B512::repeat_byte(0x02);
        let (tx1, mut rx1) = mpsc::channel(PEER_CHANNEL_CAPACITY);
        let (tx2, mut rx2) = mpsc::channel(PEER_CHANNEL_CAPACITY);

        store.add_peer(id1, tx1).await;
        store.add_peer(id2, tx2).await;

        let sent = store.broadcast(P2pMessage::Ping, &[]).await;
        assert_eq!(sent, 2);

        assert!(rx1.try_recv().is_ok());
        assert!(rx2.try_recv().is_ok());
    }
}
