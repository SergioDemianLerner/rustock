//! Did the Bridge's peg-in refund path ever execute on mainnet?
//!
//! Both outcomes of a rejected peg-in log `rejected_pegin`, so that event
//! alone does not mean a refund. rskj's `handleNonRefundablePegin` logs
//! `unrefundable_pegin` as well and returns the funds to nobody;
//! `handleRefundablePegin` instead calls `generateRejectionRelease`, which --
//! when the pegout builds -- emits `release_requested`.
//!
//! So a refund is a `rejected_pegin` with no `unrefundable_pegin` for the same
//! Bitcoin transaction. This pairs them and reports the exceptions.
//!
//! Read-only; safe against a running node.

use alloy_primitives::B256;
use rustock_storage::BlockStore;
use std::collections::{BTreeMap, HashMap, HashSet};

const REJECTED: &str = "0x708ce1ead20561c5894a93be3fee64b326b2ad6c198f8253e4bb56f1626053d6";
const UNREFUNDABLE: &str = "0x35be155c87e408cbbcb753dc12f95fc5a242a29460a3d7189e807e63d7c185a7";
const RELEASE_REQUESTED: &str = "0x7a7c29481528ac8c2b2e93aee658fddd4dc15304fa723a5c2b88514557bcc790";

/// Seconds since the epoch as an ISO-ish date, without pulling in a crate.
fn chrono_like(ts: u64) -> String {
    let days = ts / 86_400;
    let (mut y, mut d) = (1970i64, days as i64);
    loop {
        let leap = (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
        let len = if leap { 366 } else { 365 };
        if d < len { break }
        d -= len; y += 1;
    }
    let leap = (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
    let ml = [31, if leap {29} else {28}, 31,30,31,30,31,31,30,31,30,31];
    let mut m = 0;
    while d >= ml[m] { d -= ml[m]; m += 1; }
    format!("{y:04}-{:02}-{:02} {:02}:{:02}Z", m + 1, d + 1, (ts % 86_400) / 3600, (ts % 3600) / 60)
}

fn reason_name(r: u64) -> &'static str {
    match r {
        1 => "PEGIN_CAP_SURPASSED",
        2 => "LEGACY_PEGIN_MULTISIG_SENDER",
        3 => "LEGACY_PEGIN_UNDETERMINED_SENDER",
        5 => "INVALID_AMOUNT",
        _ => "(unknown)",
    }
}

fn main() -> anyhow::Result<()> {
    let dir = std::env::args().nth(1).unwrap_or("/var/lib/rustock".into());
    let s = BlockStore::open_read_only(&dir)?;

    let rejected = s.scan_bridge_events(REJECTED.parse()?, 0, u64::MAX)?;
    let unref = s.scan_bridge_events(UNREFUNDABLE.parse()?, 0, u64::MAX)?;
    let released = s.scan_bridge_events(RELEASE_REQUESTED.parse()?, 0, u64::MAX)?;
    println!("rejected_pegin     {}", rejected.len());
    println!("unrefundable_pegin {}", unref.len());
    println!("release_requested  {}", released.len());

    // `btcTxHash` is topic[1] of both peg-in events, so the two sides pair on
    // it. Transaction hash alone would not: one transaction may register
    // several peg-ins.
    let unref_btc: HashSet<B256> = unref.iter().filter_map(|e| e.4.get(1).copied()).collect();
    let released_tx: HashSet<B256> = released.iter().map(|e| e.3).collect();

    let mut by_reason: BTreeMap<u64, u64> = BTreeMap::new();
    let mut refunds = Vec::new();
    for (block, _txi, _li, tx, topics, data) in &rejected {
        let reason = data.iter().fold(0u64, |a, b| (a << 8) | *b as u64);
        *by_reason.entry(reason).or_default() += 1;
        let btc = match topics.get(1) {
            Some(b) => *b,
            None => continue,
        };
        if !unref_btc.contains(&btc) {
            refunds.push((*block, *tx, btc, reason, released_tx.contains(tx)));
        }
    }

    println!("\nrejected_pegin by reason:");
    let total: u64 = by_reason.values().sum();
    for (r, c) in &by_reason {
        println!("  {:<34} {:>8}  ({:.1}%)", reason_name(*r), c, 100.0 * *c as f64 / total as f64);
    }

    println!("\nrejections with NO unrefundable_pegin (i.e. refunded): {}", refunds.len());
    let mut by_reason_refund: BTreeMap<u64, u64> = BTreeMap::new();
    for (_, _, _, r, _) in &refunds {
        *by_reason_refund.entry(*r).or_default() += 1;
    }
    for (r, c) in &by_reason_refund {
        println!("  {:<34} {:>8}", reason_name(*r), c);
    }
    for (block, tx, btc, reason, had_release) in refunds.iter().take(25) {
        println!(
            "  #{block}  tx {tx:?}  btcTx {btc:?}  {}  release_requested_in_same_tx={had_release}",
            reason_name(*reason)
        );
    }

    // Independent of the peg-in events, and reaching further back: a refund
    // is built inside `registerBtcTransaction`, so its `release_requested`
    // is emitted by that call. An ordinary pegout's comes from
    // `updateCollections` or `releaseBtc`. `release_requested` is RSKIP146
    // (papyrus200, #2,392,700), which is 1.2M blocks earlier than
    // `rejected_pegin`, so this covers an era the pairing above cannot.
    let mut by_selector: BTreeMap<String, u64> = BTreeMap::new();
    let mut from_register = Vec::new();
    for (block, tx_index, _li, tx, _topics, _data) in &released {
        let Some(hash) = s.canonical_hash(*block)? else { continue };
        let Some((txs, _)) = s.body(hash)? else { continue };
        let Some(t) = txs.get(*tx_index as usize) else { continue };
        let sel = if t.input.len() >= 4 {
            alloy_primitives::hex::encode(&t.input[..4])
        } else {
            "(no selector)".to_string()
        };
        *by_selector.entry(sel.clone()).or_default() += 1;
        if sel == "43dc0656" {
            from_register.push((*block, *tx));
        }
    }
    println!("\nrelease_requested by calling selector:");
    for (sel, c) in &by_selector {
        let name = match sel.as_str() {
            "43dc0656" => " registerBtcTransaction  <-- REFUND",
            "0c5a9990" => " updateCollections",
            "e5400e7b" => " receiveHeaders",
            _ => "",
        };
        println!("  0x{sel}  {c:>6}{name}");
    }
    println!("release_requested emitted by registerBtcTransaction: {}", from_register.len());
    for (block, tx_index, _li, tx, topics, data) in &released {
        if !from_register.iter().any(|(b, t)| b == block && t == tx) { continue; }
        let _ = tx_index;
        let hdr = s.canonical_hash(*block)?.and_then(|h| s.header(h).ok().flatten());
        let ts = hdr.as_ref().map(|h| h.timestamp).unwrap_or(0);
        let amount = alloy_primitives::U256::from_be_slice(data);
        println!("  #{block}  {}  rskTx {tx:?}", chrono_like(ts));
        println!("      pegout btcTx (event topic[2]) {:?}", topics.get(2));
        println!("      amount {} sat = {:.8} BTC",
                 amount, amount.to::<u128>() as f64 / 1e8);
    }

    // The range the draft RSKIP names in its verification section.
    let r74 = s.scan_bridge_events(REJECTED.parse()?, 7_300_000, 7_400_000)?;
    let mut r74_by: BTreeMap<u64, u64> = BTreeMap::new();
    for e in &r74 {
        *r74_by.entry(e.5.iter().fold(0u64, |a, b| (a << 8) | *b as u64)).or_default() += 1;
    }
    println!("\n#7,300,000..#7,400,000: {} rejected_pegin", r74.len());
    for (r, c) in &r74_by {
        println!("  {:<34} {:>8}", reason_name(*r), c);
    }

    // The converse, as a check on the pairing itself.
    let rejected_btc: HashMap<B256, u64> =
        rejected.iter().filter_map(|e| e.4.get(1).map(|b| (*b, e.0))).collect();
    let orphan_unref = unref
        .iter()
        .filter(|e| e.4.get(1).is_some_and(|b| !rejected_btc.contains_key(b)))
        .count();
    println!("\nunrefundable_pegin with no matching rejected_pegin: {orphan_unref}");
    Ok(())
}
