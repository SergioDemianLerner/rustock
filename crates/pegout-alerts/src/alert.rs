//! What the watcher can find, and how it reads to a human.

use crate::config::sats_to_btc_string;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Alert {
    /// A single peg-out request above the configured threshold.
    LargePegout {
        block: u64,
        rsk_tx_hash: [u8; 32],
        btc_tx_hash: [u8; 32],
        amount_sats: u64,
        threshold_sats: u64,
    },
    /// An output of a peg-out BTC transaction, above the per-output threshold
    /// and not recognised as federation change.
    LargeOutput {
        block: u64,
        btc_txid: String,
        output_index: usize,
        script_hex: String,
        amount_sats: u64,
        threshold_sats: u64,
    },
    /// Total value of peg-outs requested but not yet confirmed.
    InTransitAboveThreshold {
        block: u64,
        total_sats: u64,
        threshold_sats: u64,
        pegout_count: usize,
    },
}

impl Alert {
    pub fn subject(&self) -> String {
        match self {
            Alert::LargePegout { amount_sats, block, .. } => format!(
                "Large peg-out: {} BTC at block #{block}",
                sats_to_btc_string(*amount_sats)
            ),
            Alert::LargeOutput { amount_sats, block, output_index, .. } => format!(
                "Large peg-out output: {} BTC (output {output_index}) at block #{block}",
                sats_to_btc_string(*amount_sats)
            ),
            Alert::InTransitAboveThreshold { total_sats, block, .. } => format!(
                "Peg-outs in transit: {} BTC at block #{block}",
                sats_to_btc_string(*total_sats)
            ),
        }
    }

    pub fn body(&self) -> String {
        match self {
            Alert::LargePegout { block, rsk_tx_hash, btc_tx_hash, amount_sats, threshold_sats } => {
                format!(
"A peg-out was requested above the configured threshold.

  RSK block      #{block}
  amount         {} BTC
  threshold      {} BTC
  RSK tx hash    0x{}
  BTC tx hash    0x{}

This is the release_requested event: the Bridge has built the Bitcoin
transaction, which now waits for confirmations before it is handed to the
signers.",
                    sats_to_btc_string(*amount_sats),
                    sats_to_btc_string(*threshold_sats),
                    hex::encode(rsk_tx_hash),
                    hex::encode(btc_tx_hash))
            }
            Alert::LargeOutput { block, btc_txid, output_index, script_hex, amount_sats, threshold_sats } => {
                format!(
"An output of a peg-out Bitcoin transaction awaiting signature is above the
configured per-output threshold.

  RSK block      #{block}
  BTC txid       {btc_txid}
  output index   {output_index}
  amount         {} BTC
  threshold      {} BTC
  scriptPubKey   {script_hex}

This transaction is in pegoutsWaitingForSignatures — the set handed to the
signers. Outputs matching a configured federation change script are excluded
from this check; if this output IS federation change, add its scriptPubKey to
`pegout_alerts.federation_change_scripts` to stop alerting on it.",
                    sats_to_btc_string(*amount_sats),
                    sats_to_btc_string(*threshold_sats))
            }
            Alert::InTransitAboveThreshold { block, total_sats, threshold_sats, pegout_count } => {
                format!(
"The total value of peg-outs in transit is above the configured threshold.

  RSK block      #{block}
  in transit     {} BTC across {pegout_count} peg-out transaction(s)
  threshold      {} BTC

\"In transit\" is the set of peg-outs whose Bitcoin transactions have been built
but have not yet reached the required confirmations, so they have not yet been
handed to the signers.",
                    sats_to_btc_string(*total_sats),
                    sats_to_btc_string(*threshold_sats))
            }
        }
    }

    /// Alerts about the same thing at the same block are the same alert; the
    /// watcher uses this to avoid mailing once per poll while a condition
    /// persists.
    pub fn dedup_key(&self) -> String {
        match self {
            Alert::LargePegout { rsk_tx_hash, btc_tx_hash, .. } =>
                format!("pegout:{}:{}", hex::encode(rsk_tx_hash), hex::encode(btc_tx_hash)),
            Alert::LargeOutput { btc_txid, output_index, .. } =>
                format!("output:{btc_txid}:{output_index}"),
            // In transit is a running total, so it is keyed by the value: it
            // re-alerts when the number changes, not on every poll.
            Alert::InTransitAboveThreshold { total_sats, .. } =>
                format!("intransit:{total_sats}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subjects_and_bodies_report_btc_not_satoshis() {
        let a = Alert::LargePegout {
            block: 100, rsk_tx_hash: [0xAB; 32], btc_tx_hash: [0xCD; 32],
            amount_sats: 15_000_000_000, threshold_sats: 10_000_000_000,
        };
        assert!(a.subject().contains("150.00000000 BTC"), "{}", a.subject());
        assert!(a.body().contains("150.00000000"));
        assert!(a.body().contains("100.00000000"));
        assert!(a.body().contains(&hex::encode([0xABu8; 32])));
    }

    #[test]
    fn a_persisting_condition_dedups_but_a_changed_total_does_not() {
        let a = Alert::InTransitAboveThreshold { block: 1, total_sats: 5, threshold_sats: 1, pegout_count: 2 };
        let same_total_later_block = Alert::InTransitAboveThreshold { block: 9, total_sats: 5, threshold_sats: 1, pegout_count: 2 };
        let different_total = Alert::InTransitAboveThreshold { block: 9, total_sats: 6, threshold_sats: 1, pegout_count: 3 };
        assert_eq!(a.dedup_key(), same_total_later_block.dedup_key());
        assert_ne!(a.dedup_key(), different_total.dedup_key());
    }

    #[test]
    fn the_output_alert_says_how_to_silence_a_change_output() {
        let a = Alert::LargeOutput {
            block: 7, btc_txid: "ab".into(), output_index: 1,
            script_hex: "a914deadbeef87".into(),
            amount_sats: 200 * 100_000_000, threshold_sats: 100 * 100_000_000,
        };
        assert!(a.body().contains("federation_change_scripts"));
    }
}
