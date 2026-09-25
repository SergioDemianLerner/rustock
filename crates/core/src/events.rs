//! Chain events, published by whoever executes blocks and consumed by
//! whoever needs to be told.
//!
//! # Why this is in `core`
//!
//! The sync service publishes and the RPC layer's `eth_subscribe` consumes.
//! Neither crate depends on the other — both sit on top of `core`,
//! `storage`, `execution`, `networking` and `trie` — so the event type has to
//! live where both can see it. The *rendering* into JSON-RPC notifications
//! stays in `rustock-rpc`, which is the only place that should know what a
//! subscriber's wire format looks like.
//!
//! rskj has no counterpart: its equivalent is the `EthereumListener`
//! fan-out, which rustock deliberately does not have (see
//! `docs/rskj-to-rustock-map.md`). This is a single typed channel rather than
//! an interface with a dozen methods and a dozen implementors.

use crate::types::header::Header;
use crate::types::receipt::Receipt;
use crate::types::transaction::Transaction;
use alloy_primitives::B256;
use std::sync::Arc;

/// How many events a subscriber may fall behind before it is disconnected.
///
/// A subscriber that stops reading must not be able to grow the node's
/// memory. `crates/networking/src/peers.rs` learned this with an unbounded
/// peer channel that let one stalled peer drive memory arbitrarily high.
pub const SUBSCRIBER_LAG: usize = 512;

/// A block joined or left the canonical chain.
#[derive(Debug, Clone)]
pub struct BlockEvent {
    pub hash: B256,
    pub header: Header,
    pub transactions: Vec<Transaction>,
    pub receipts: Vec<Receipt>,
    /// True when this block **left** the chain, so a subscriber must retract
    /// whatever it did with the logs.
    pub removed: bool,
}

/// What the node publishes.
///
/// The block variant is behind an `Arc` because `tokio::sync::broadcast`
/// clones the value for every receiver and a block's receipts are not small.
#[derive(Debug, Clone)]
pub enum ChainEvent {
    Block(Arc<BlockEvent>),
    PendingTransaction(B256),
}

pub type EventSender = tokio::sync::broadcast::Sender<ChainEvent>;
pub type EventReceiver = tokio::sync::broadcast::Receiver<ChainEvent>;

/// Create the channel, with `SUBSCRIBER_LAG` as the per-receiver backlog.
pub fn channel() -> EventSender {
    tokio::sync::broadcast::channel(SUBSCRIBER_LAG).0
}
