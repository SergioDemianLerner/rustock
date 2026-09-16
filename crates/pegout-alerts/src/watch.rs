//! The analysis. Pure functions over decoded inputs, so every rule is testable
//! without a node, a database or a mail server.

use crate::alert::Alert;
use crate::config::PegoutAlerts;
use alloy_primitives::B256;
use rustock_core::Receipt;

/// `release_requested(bytes32 indexed rskTxHash, bytes32 indexed btcTxHash, uint256 amount)`
/// — emitted when the Bridge has built a peg-out Bitcoin transaction. This is
/// the moment a peg-out becomes real, so it is what the per-peg-out threshold
/// watches.
pub fn release_requested_topic() -> B256 {
    use sha3_topic::keccak;
    B256::from(keccak(b"release_requested(bytes32,bytes32,uint256)"))
}

mod sha3_topic {
    /// Keccak-256 (not the NIST SHA-3 padding), matching the EVM's topic hash.
    pub fn keccak(input: &[u8]) -> [u8; 32] {
        alloy_primitives::keccak256(input).0
    }
}

/// A peg-out as announced by `release_requested`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PegoutRequested {
    pub rsk_tx_hash: [u8; 32],
    pub btc_tx_hash: [u8; 32],
    pub amount_sats: u64,
}

/// Pull every peg-out announcement out of a block's receipts.
///
/// Reads logs rather than hooking execution: receipts are already persisted for
/// every block, so the watcher is an observer of the database and cannot slow
/// block processing down or change what it computes.
pub fn pegouts_in_receipts(receipts: &[Receipt], bridge: &alloy_primitives::Address) -> Vec<PegoutRequested> {
    let topic = release_requested_topic();
    let mut out = Vec::new();
    for r in receipts {
        for log in &r.logs {
            if &log.address != bridge {
                continue;
            }
            if log.topics.first() != Some(&topic) || log.topics.len() < 3 {
                continue;
            }
            // topics: [sig, rskTxHash, btcTxHash]; data: uint256 amount.
            if log.data.len() < 32 {
                continue;
            }
            let amount = alloy_primitives::U256::from_be_slice(&log.data[log.data.len() - 32..]);
            out.push(PegoutRequested {
                rsk_tx_hash: log.topics[1].0,
                btc_tx_hash: log.topics[2].0,
                // A peg-out cannot exceed the 21M-BTC supply, so a saturating
                // conversion cannot hide a real value; it only avoids a panic
                // on absurd input.
                amount_sats: amount.try_into().unwrap_or(u64::MAX),
            });
        }
    }
    out
}

/// One peg-out Bitcoin transaction awaiting signature, already parsed.
#[derive(Debug, Clone)]
pub struct WaitingTx {
    pub txid: String,
    pub outputs: Vec<(Vec<u8>, u64)>, // (scriptPubKey, value in satoshis)
}

/// Parse the raw transactions held in `pegoutsWaitingForSignatures`.
///
/// An unparseable entry is skipped rather than failing the sweep: the watcher
/// must not go blind because one entry is malformed.
pub fn parse_waiting_txs(raw: impl IntoIterator<Item = Vec<u8>>) -> Vec<WaitingTx> {
    use bitcoin::consensus::Decodable;
    let mut out = Vec::new();
    for bytes in raw {
        let mut slice = bytes.as_slice();
        let Ok(tx) = bitcoin::Transaction::consensus_decode(&mut slice) else {
            tracing::warn!(target: "rustock::pegout_alerts", "skipping unparseable peg-out transaction");
            continue;
        };
        out.push(WaitingTx {
            txid: tx.compute_txid().to_string(),
            outputs: tx
                .output
                .iter()
                .map(|o| (o.script_pubkey.to_bytes(), o.value.to_sat()))
                .collect(),
        });
    }
    out
}

/// Apply the per-peg-out threshold.
pub fn check_pegouts(block: u64, pegouts: &[PegoutRequested], cfg: &PegoutAlerts) -> Vec<Alert> {
    let threshold = cfg.pegout_alert_sats();
    pegouts
        .iter()
        .filter(|p| p.amount_sats > threshold)
        .map(|p| Alert::LargePegout {
            block,
            rsk_tx_hash: p.rsk_tx_hash,
            btc_tx_hash: p.btc_tx_hash,
            amount_sats: p.amount_sats,
            threshold_sats: threshold,
        })
        .collect()
}

/// Apply the per-output threshold to the transactions awaiting signature.
///
/// A peg-out transaction pays end users and may also return change to the
/// federation; change is not a transfer to anyone and should not alert. There
/// is no way to tell them apart from the raw transaction alone, so change
/// scripts are configured explicitly. **When none are configured every output
/// is treated as a user output** — that over-alerts on a large change output,
/// which is the safe direction: it can never hide a real transfer.
pub fn check_outputs(block: u64, txs: &[WaitingTx], cfg: &PegoutAlerts, change_scripts: &[Vec<u8>]) -> Vec<Alert> {
    let threshold = cfg.output_alert_sats();
    let mut out = Vec::new();
    for tx in txs {
        for (i, (script, value)) in tx.outputs.iter().enumerate() {
            if change_scripts.iter().any(|c| c == script) {
                continue; // federation change
            }
            if *value > threshold {
                out.push(Alert::LargeOutput {
                    block,
                    btc_txid: tx.txid.clone(),
                    output_index: i,
                    script_hex: hex::encode(script),
                    amount_sats: *value,
                    threshold_sats: threshold,
                });
            }
        }
    }
    out
}

/// Total value of peg-outs built but not yet confirmed, and the alert if it is
/// above the threshold.
///
/// "In transit" is `pegoutsWaitingForConfirmations`: the Bridge has built the
/// Bitcoin transaction but it has not yet reached the required confirmations,
/// so it has not been handed to the signers. Entries older than that are no
/// longer in transit and the Bridge has already moved them on.
pub fn check_in_transit(
    block: u64,
    entries: &[(u64, u64)], // (creation block height, total output value in sats)
    cfg: &PegoutAlerts,
) -> (u64, Option<Alert>) {
    let still_in_transit: Vec<_> = entries
        .iter()
        .filter(|(created, _)| block.saturating_sub(*created) < cfg.confirmations)
        .collect();
    let total: u64 = still_in_transit.iter().map(|(_, v)| *v).sum();
    let threshold = cfg.in_transit_alert_sats();
    let alert = (total > threshold).then(|| Alert::InTransitAboveThreshold {
        block,
        total_sats: total,
        threshold_sats: threshold,
        pegout_count: still_in_transit.len(),
    });
    (total, alert)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::SATOSHIS_PER_BTC;

    fn cfg() -> PegoutAlerts { PegoutAlerts::default() }
    fn btc(n: u64) -> u64 { n * SATOSHIS_PER_BTC }

    #[test]
    fn a_pegout_at_the_threshold_does_not_alert_but_above_it_does() {
        let c = cfg();
        let at = PegoutRequested { rsk_tx_hash: [1; 32], btc_tx_hash: [2; 32], amount_sats: btc(100) };
        let above = PegoutRequested { rsk_tx_hash: [3; 32], btc_tx_hash: [4; 32], amount_sats: btc(100) + 1 };
        assert!(check_pegouts(1, std::slice::from_ref(&at), &c).is_empty(), "100 BTC is not ABOVE 100 BTC");
        assert_eq!(check_pegouts(1, &[above], &c).len(), 1);
    }

    #[test]
    fn every_output_is_checked_when_no_change_script_is_configured() {
        let tx = WaitingTx {
            txid: "t".into(),
            outputs: vec![(vec![0xAA], btc(150)), (vec![0xBB], btc(1))],
        };
        let alerts = check_outputs(5, &[tx], &cfg(), &[]);
        assert_eq!(alerts.len(), 1, "only the 150 BTC output is above 100");
    }

    #[test]
    fn a_configured_change_script_is_excluded() {
        let change = vec![0xAAu8];
        let tx = WaitingTx {
            txid: "t".into(),
            outputs: vec![(change.clone(), btc(500)), (vec![0xBB], btc(150))],
        };
        let alerts = check_outputs(5, &[tx], &cfg(), &[change]);
        assert_eq!(alerts.len(), 1, "the 500 BTC change must be excluded, the 150 BTC user output must not");
        match &alerts[0] {
            Alert::LargeOutput { output_index, amount_sats, .. } => {
                assert_eq!(*output_index, 1);
                assert_eq!(*amount_sats, btc(150));
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn in_transit_counts_only_peg_outs_inside_the_confirmation_window() {
        let c = cfg(); // 4,000 confirmations
        let now = 10_000;
        let entries = vec![
            (9_999, btc(150)),  // 1 block old: in transit
            (6_001, btc(100)),  // 3,999 blocks old: in transit
            (6_000, btc(500)),  // exactly 4,000 old: confirmed, NOT in transit
            (1_000, btc(900)),  // long confirmed
        ];
        let (total, alert) = check_in_transit(now, &entries, &c);
        assert_eq!(total, btc(250), "only the two inside the window count");
        let alert = alert.expect("250 BTC is above the 200 BTC threshold");
        match alert {
            Alert::InTransitAboveThreshold { pegout_count, total_sats, .. } => {
                assert_eq!(pegout_count, 2);
                assert_eq!(total_sats, btc(250));
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn in_transit_below_the_threshold_is_silent() {
        let (total, alert) = check_in_transit(100, &[(99, btc(199))], &cfg());
        assert_eq!(total, btc(199));
        assert!(alert.is_none());
    }

    #[test]
    fn receipts_without_bridge_logs_yield_nothing() {
        let bridge = alloy_primitives::Address::repeat_byte(0x11);
        assert!(pegouts_in_receipts(&[], &bridge).is_empty());
    }

    #[test]
    fn an_unparseable_waiting_transaction_is_skipped_not_fatal() {
        let txs = parse_waiting_txs(vec![vec![0x00, 0x01, 0x02]]);
        assert!(txs.is_empty(), "malformed entry skipped, sweep continues");
    }
}
