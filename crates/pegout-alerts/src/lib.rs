//! Peg-out monitoring and alerting.
//!
//! An observer, not a participant. It reads blocks, receipts and Bridge state
//! that the node has already committed, so it cannot change what the node
//! computes and cannot slow block processing: there is no hook in the execution
//! or sync path at all. If this module stalls, crashes or blocks on an
//! unreachable mail server, the node does not notice.
//!
//! What it watches, all thresholds configurable:
//!
//! - every peg-out request, logged, and alerted above a per-peg-out threshold;
//! - every output of a peg-out Bitcoin transaction awaiting signature -- the
//!   transactions handed to the signers -- alerted above a per-output
//!   threshold, excluding configured federation change scripts;
//! - the total value in transit: peg-outs built but not yet confirmed.

pub mod alert;
pub mod config;
pub mod service;
pub mod sink;
pub mod watch;

pub use alert::Alert;
pub use config::{Config, PegoutAlerts};
pub use service::Watcher;
pub use sink::{AlertSink, LogSink};
#[cfg(feature = "smtp")]
pub use sink::SmtpSink;
