//! The watcher task.
//!
//! Polls the node's own database. It never touches execution, never holds a
//! lock the node needs, and its only inputs are blocks the node has already
//! committed — so no failure here can affect consensus or block rate.

use crate::config::PegoutAlerts;
use crate::sink::AlertSink;
use crate::watch;
use alloy_primitives::B256;
use rustock_storage::BlockStore;
use rustock_trie::{TrieNode, TrieStore};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

/// Peg-out events worth recording, whether or not they breach a threshold.
/// Requirement: *every* peg-out is logged.
fn pegout_event_names() -> Vec<(B256, &'static str)> {
    let t = |s: &str| B256::from(alloy_primitives::keccak256(s.as_bytes()).0);
    vec![
        (t("release_request_received(address,bytes,uint256)"), "release_request_received"),
        (t("release_request_received(address,bytes20,uint256)"), "release_request_received"),
        (t("release_requested(bytes32,bytes32,uint256)"), "release_requested"),
        (t("release_btc(bytes32,bytes)"), "release_btc"),
        (t("pegout_confirmed(bytes32,uint256)"), "pegout_confirmed"),
        (t("pegout_transaction_created(bytes32,bytes)"), "pegout_transaction_created"),
    ]
}

/// Modification time of `path`, or `None` if it cannot be read right now.
fn file_mtime(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

pub struct Watcher {
    store: Arc<BlockStore>,
    trie: Arc<dyn TrieStore>,
    cfg: PegoutAlerts,
    sinks: Vec<Box<dyn AlertSink>>,
    change_scripts: Vec<Vec<u8>>,
    /// Alerts already delivered, so a condition that persists across polls is
    /// reported once rather than every few seconds.
    seen: HashSet<String>,
    next_block: u64,
    /// Configuration file to re-read when it changes on disk. `None` means the
    /// watcher keeps whatever it was built with.
    config_path: Option<PathBuf>,
    /// Modification time of that file as last seen — whether or not it parsed,
    /// so a broken edit is reported once rather than on every poll.
    config_seen_mtime: Option<SystemTime>,
}

impl Watcher {
    pub fn new(
        store: Arc<BlockStore>,
        trie: Arc<dyn TrieStore>,
        cfg: PegoutAlerts,
        sinks: Vec<Box<dyn AlertSink>>,
        change_scripts: Vec<Vec<u8>>,
    ) -> anyhow::Result<Self> {
        cfg.validate()?;
        // Default to the current head: switching alerting on should not replay
        // and re-alert the whole chain.
        let head = store
            .exec_head()?
            .and_then(|(h, _)| store.header(h).ok().flatten())
            .map(|h| h.number)
            .unwrap_or(0);
        let next_block = cfg.start_block.unwrap_or(head.saturating_add(1));
        Ok(Self {
            store,
            trie,
            cfg,
            sinks,
            change_scripts,
            seen: HashSet::new(),
            next_block,
            config_path: None,
            config_seen_mtime: None,
        })
    }

    /// Re-read `path` whenever its modification time changes, so thresholds can
    /// be retuned against a running node.
    ///
    /// Only the fields that can take effect immediately are adopted; a change
    /// to how mail is delivered, or to whether the watcher runs at all, is
    /// reported as needing a restart rather than half-applied.
    pub fn reloading_from(mut self, path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        self.config_seen_mtime = file_mtime(&path);
        self.config_path = Some(path);
        self
    }

    fn reload_config_if_changed(&mut self) {
        let Some(path) = self.config_path.clone() else { return };
        // Unreadable right now (being rewritten, say): keep running on what we
        // have and look again next poll.
        let Some(mtime) = file_mtime(&path) else { return };
        if Some(mtime) == self.config_seen_mtime {
            return;
        }
        self.config_seen_mtime = Some(mtime);

        let new = match crate::config::Config::load(&path) {
            Ok(c) => c.pegout_alerts,
            Err(e) => {
                tracing::error!(
                    target: "rustock::pegout_alerts",
                    "{} changed but does not load; keeping the running configuration: {e:#}",
                    path.display()
                );
                return;
            }
        };
        let scripts = match new.change_scripts() {
            Ok(s) => s,
            Err(e) => {
                tracing::error!(
                    target: "rustock::pegout_alerts",
                    "{} changed but is unusable; keeping the running configuration: {e:#}",
                    path.display()
                );
                return;
            }
        };

        let (live, restart) = self.cfg.describe_changes(&new);
        if live.is_empty() && restart.is_empty() {
            tracing::info!(
                target: "rustock::pegout_alerts",
                "{} changed but no setting differs", path.display()
            );
            return;
        }
        for change in &restart {
            tracing::warn!(
                target: "rustock::pegout_alerts",
                "{} changed {change}, which needs a node restart to take effect",
                path.display()
            );
        }
        if !live.is_empty() {
            self.cfg.adopt_live(&new);
            self.change_scripts = scripts;
            tracing::info!(
                target: "rustock::pegout_alerts",
                "reloaded {}: {}", path.display(), live.join(", ")
            );
        }
    }

    /// Run until cancelled. Every error is logged and swallowed: a watcher that
    /// exits on a transient read failure is worse than one that retries.
    pub async fn run(mut self) {
        tracing::info!(
            target: "rustock::pegout_alerts",
            "peg-out watcher started at #{}, thresholds: pegout {} BTC, output {} BTC, in transit {} BTC, {} confirmations",
            self.next_block, self.cfg.pegout_alert_btc, self.cfg.output_alert_btc,
            self.cfg.in_transit_alert_btc, self.cfg.confirmations
        );
        loop {
            // Read inside the loop so poll_interval_secs is itself reloadable.
            tokio::time::sleep(Duration::from_secs(self.cfg.poll_interval_secs)).await;
            self.reload_config_if_changed();
            if let Err(e) = self.sweep() {
                tracing::warn!(target: "rustock::pegout_alerts", "sweep failed, will retry: {e:#}");
            }
        }
    }

    fn head_number(&self) -> anyhow::Result<u64> {
        Ok(self
            .store
            .exec_head()?
            .and_then(|(h, _)| self.store.header(h).ok().flatten())
            .map(|h| h.number)
            .unwrap_or(0))
    }

    /// Process every block executed since the last sweep.
    pub fn sweep(&mut self) -> anyhow::Result<()> {
        let head = self.head_number()?;
        while self.next_block <= head {
            let n = self.next_block;
            if let Err(e) = self.process_block(n) {
                // Do not advance past a block we failed to read: retry it.
                tracing::warn!(target: "rustock::pegout_alerts", "block #{n}: {e:#}");
                return Ok(());
            }
            self.next_block = n + 1;
        }
        Ok(())
    }

    fn process_block(&mut self, number: u64) -> anyhow::Result<()> {
        let Some(hash) = self.store.canonical_hash(number)? else {
            anyhow::bail!("no canonical hash");
        };
        let Some(header) = self.store.header(hash)? else {
            anyhow::bail!("no header");
        };
        let receipts = self.store.receipts(hash)?.unwrap_or_default();
        let bridge = rustock_execution::precompiles::BRIDGE_ADDR;

        // 1. Log every peg-out event.
        let names = pegout_event_names();
        for r in &receipts {
            for log in &r.logs {
                if log.address != bridge {
                    continue;
                }
                if let Some((_, name)) = log
                    .topics
                    .first()
                    .and_then(|t| names.iter().find(|(topic, _)| topic == t))
                {
                    tracing::info!(
                        target: "rustock::pegout_alerts",
                        block = number, event = name, topics = log.topics.len(),
                        data = %hex::encode(&log.data),
                        "peg-out event"
                    );
                }
            }
        }

        let mut alerts = Vec::new();

        // 2. Per-peg-out threshold, from release_requested.
        let pegouts = watch::pegouts_in_receipts(&receipts, &bridge);
        for p in &pegouts {
            tracing::info!(
                target: "rustock::pegout_alerts",
                block = number,
                amount_sats = p.amount_sats,
                rsk_tx = %hex::encode(p.rsk_tx_hash),
                btc_tx = %hex::encode(p.btc_tx_hash),
                "peg-out requested"
            );
        }
        alerts.extend(watch::check_pegouts(number, &pegouts, &self.cfg));

        // 3 and 4 need Bridge state, and are only worth reading when something
        // peg-out related happened or periodically — reading the trie on every
        // block would be wasted work on a chain where most blocks have none.
        let bridge_touched = !pegouts.is_empty()
            || receipts.iter().any(|r| r.logs.iter().any(|l| l.address == bridge));
        if bridge_touched {
            match self.read_bridge_state(header.state_root) {
                Ok((waiting_txs, in_transit)) => {
                    alerts.extend(watch::check_outputs(
                        number, &waiting_txs, &self.cfg, &self.change_scripts,
                    ));
                    // In transit is a running total, so it would otherwise
                    // re-alert on every change -- including when the total
                    // FALLS as peg-outs reach their confirmations. Evaluate it
                    // only when this block requested a new peg-out: that is the
                    // moment the number goes up and is worth knowing about.
                    let (total, alert) = watch::check_in_transit(number, &in_transit, &self.cfg);
                    tracing::debug!(
                        target: "rustock::pegout_alerts",
                        block = number, in_transit_sats = total,
                        waiting_txs = waiting_txs.len(), "bridge peg-out state"
                    );
                    if !pegouts.is_empty() {
                        alerts.extend(alert);
                    }
                }
                Err(e) => tracing::warn!(
                    target: "rustock::pegout_alerts",
                    "block #{number}: reading Bridge state: {e:#}"
                ),
            }
        }

        for alert in alerts {
            let key = alert.dedup_key();
            if !self.seen.insert(key) {
                continue;
            }
            for sink in &self.sinks {
                if let Err(e) = sink.deliver(&alert) {
                    tracing::error!(
                        target: "rustock::pegout_alerts",
                        "sink {} failed to deliver {:?}: {e:#}", sink.name(), alert.subject()
                    );
                }
            }
        }
        Ok(())
    }

    /// Read the two peg-out queues straight out of the unitrie at `state_root`.
    ///
    /// Returns the transactions awaiting signature (what the signers are handed)
    /// and, for the in-transit total, the creation height and non-change output
    /// value of each peg-out awaiting confirmation.
    fn read_bridge_state(
        &self,
        state_root: B256,
    ) -> anyhow::Result<(Vec<watch::WaitingTx>, Vec<(u64, u64)>)> {
        use rustock_execution::bridge::peg::{
            deserialize_pegouts_waiting_for_confirmations, deserialize_rsk_txs_waiting_for_signatures,
        };
        use rustock_execution::bridge::storage::{
            PEGOUTS_WAITING_FOR_CONFIRMATIONS_KEY, PEGOUTS_WAITING_FOR_CONFIRMATIONS_WITH_TXHASH_KEY,
            PEGOUTS_WAITING_FOR_SIGNATURES_KEY,
        };

        let data = self
            .trie
            .get(state_root.as_slice())
            .ok_or_else(|| anyhow::anyhow!("state root {state_root:?} not in the trie store"))?;
        let root = TrieNode::from_message(&data, self.trie.as_ref());

        let wfs_raw = self.read_named(&root, PEGOUTS_WAITING_FOR_SIGNATURES_KEY);
        let waiting = deserialize_rsk_txs_waiting_for_signatures(&wfs_raw);
        let waiting_txs = watch::parse_waiting_txs(waiting.into_values());

        let mut wfc = deserialize_pegouts_waiting_for_confirmations(
            &self.read_named(&root, PEGOUTS_WAITING_FOR_CONFIRMATIONS_KEY),
            false,
        );
        wfc.extend(deserialize_pegouts_waiting_for_confirmations(
            &self.read_named(&root, PEGOUTS_WAITING_FOR_CONFIRMATIONS_WITH_TXHASH_KEY),
            true,
        ));

        // Value in transit is what leaves the federation, so configured change
        // outputs are excluded — the same rule the per-output check uses.
        let in_transit = watch::parse_waiting_txs(wfc.iter().map(|e| e.btc_tx_raw.clone()))
            .into_iter()
            .zip(wfc.iter())
            .map(|(tx, entry)| {
                let leaving: u64 = tx
                    .outputs
                    .iter()
                    .filter(|(script, _)| !self.change_scripts.iter().any(|c| c == script))
                    .map(|(_, v)| *v)
                    .sum();
                (entry.rsk_block_height, leaving)
            })
            .collect();

        Ok((waiting_txs, in_transit))
    }

    fn read_named(&self, root: &TrieNode, name: &str) -> Vec<u8> {
        use rustock_execution::bridge::storage::bridge_storage_key;
        let slot = B256::from(bridge_storage_key(name));
        let key = rustock_trie::storage_key(&rustock_execution::precompiles::BRIDGE_ADDR, &slot);
        root.get(&rustock_trie::TrieKeySlice::from_key(&key), self.trie.as_ref())
            .unwrap_or_default()
    }
}
