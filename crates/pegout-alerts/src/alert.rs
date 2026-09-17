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
    /// A block whose balance changes do not conserve the native supply.
    ///
    /// Not a peg-out finding, but it travels the same delivery path: it is the
    /// most serious thing the node can notice, and the operator wants it by
    /// mail for the same reason.
    SupplyNotConserved {
        block: u64,
        /// True when rBTC was created (block rejected), false when destroyed.
        created: bool,
        /// Imbalance in wei.
        amount_wei: String,
        /// The accounts that moved most, as `address before -> after`.
        top_accounts: Vec<String>,
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
            Alert::SupplyNotConserved { block, created: true, amount_wei, .. } => format!(
                "REJECTED BLOCK #{block}: {amount_wei} wei of rBTC created from nothing"
            ),
            Alert::SupplyNotConserved { block, created: false, amount_wei, .. } => format!(
                "Block #{block} destroyed {amount_wei} wei of rBTC"
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
            Alert::SupplyNotConserved { block, created, amount_wei, top_accounts } => {
                let what = if *created {
                    "MORE rBTC exists after this block than before it. The peg is backed 1:1 by\n                     bitcoin and the Bridge holds the entire supply, so no legitimate block can\n                     do this. The block was REJECTED and not applied."
                } else {
                    "Less rBTC exists after this block than before it. This is not necessarily\n                     wrong -- REMASC burns a share of fees, and some EVM cases destroy balance --\n                     so the block was accepted and this is a record, not a failure."
                };
                format!(
"Native supply changed in block #{block}.

{what}

  imbalance   {amount_wei} wei

Accounts that moved most:
{}
",
                    top_accounts.iter().map(|a| format!("  {a}")).collect::<Vec<_>>().join("\n")
                )
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
            // Raised only for a block that requested a new peg-out (see
            // Watcher::process_block), so the block number is the event: one
            // alert per new peg-out, never a re-alert as the total drifts.
            Alert::InTransitAboveThreshold { block, .. } =>
                format!("intransit:{block}"),
            // One per block: a block is either conserved or it is not.
            Alert::SupplyNotConserved { block, .. } => format!("supply:{block}"),
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
    fn in_transit_is_keyed_by_the_block_that_requested_a_new_pegout() {
        // The alert is only ever raised for a block that requested a new
        // peg-out, so one key per block is one alert per new peg-out. Keying
        // by the total instead would re-alert every time the running total
        // drifted -- including downwards, as peg-outs reach confirmation.
        let a = Alert::InTransitAboveThreshold { block: 9, total_sats: 5, threshold_sats: 1, pegout_count: 2 };
        let same_block_bigger_total = Alert::InTransitAboveThreshold { block: 9, total_sats: 6, threshold_sats: 1, pegout_count: 3 };
        let later_block = Alert::InTransitAboveThreshold { block: 10, total_sats: 5, threshold_sats: 1, pegout_count: 2 };
        assert_eq!(a.dedup_key(), same_block_bigger_total.dedup_key());
        assert_ne!(a.dedup_key(), later_block.dedup_key());
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
