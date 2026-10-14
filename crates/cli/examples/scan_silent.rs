//! Read the silent-decision store: what the Bridge decided without logging it.
//!
//! The Bridge's event log answers questions about the recent chain quickly and
//! older ones not at all -- `rejected_pegin` arrives at iris300 (#3,614,800),
//! and mainnet's only two peg-in refunds happened 760,000 blocks earlier. An
//! event-based search reports zero refunds, truthfully and uselessly.
//!
//! `verify_range --record-silent` fills that in by recording decisions as it
//! replays. This reads what it recorded.
//!
//! ```text
//! scan_silent <store-dir> [kind] [--from N] [--to N] [--coverage] [--account]
//!
//!   kind        pegin_refunded, pegin_rejected_no_refund, pegin_accepted,
//!               btc_tx_marked_processed, whitelist_entry_consumed,
//!               refund_build_failed (default: every kind)
//!   --coverage  print which ranges were replayed, and stop
//!   --account   total the bitcoin that reached the federation and was never
//!               returned, by reason
//! ```
//!
//! `--account` answers "how much value went into the peg and never came back
//! out". Two details decide whether that total means what it looks like.
//!
//! **Deduplication by bitcoin transaction.** A rejected peg-in is marked
//! processed only under RSKIP459 && !RSKIP551, a narrow window. Outside it
//! nothing stops the same bitcoin transaction being handed to
//! `registerBtcTransaction` again, and each attempt is rejected afresh and
//! recorded afresh. The bitcoin moved once, so the total is over DISTINCT
//! `btcTxHash`, with the record count printed beside it so any gap shows.
//!
//! **The funds are not in the Bridge UTXO set.** Every no-refund path returns
//! before `registerNewUtxos`, so the Bridge never learns about these outputs.
//! No RBTC was minted against them -- they are bitcoin held in excess of the
//! pegged supply -- but the Bridge cannot spend them, and a migration carries
//! over only UTXOs the Bridge knows about. Value rejected while an earlier
//! federation was active is stranded at that federation's address.
//!
//! A query over a range answers with the records AND with whether the range
//! was ever replayed. "No refunds found" and "nobody has looked" are
//! different answers, and a report that conflates them is worse than no
//! report, because it is believed.

use anyhow::{Context, Result};
use rustock_execution::silent::SilentKind;
use rustock_storage::silent_store::SilentStore;

const USAGE: &str = "usage: scan_silent <store-dir> [kind] [--from N] [--to N] [--coverage]";

fn main() -> Result<()> {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let mut positional = Vec::new();
    let mut from = 0u64;
    let mut to = u64::MAX;
    let mut coverage_only = false;
    let mut account = false;
    let mut it = argv.into_iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--coverage" => coverage_only = true,
            "--account" => account = true,
            "--from" => from = it.next().context("--from needs a value")?.parse()?,
            "--to" => to = it.next().context("--to needs a value")?.parse()?,
            _ if a.starts_with("--") => anyhow::bail!("unknown option {a}\n{USAGE}"),
            _ => positional.push(a),
        }
    }
    anyhow::ensure!(!positional.is_empty(), "{USAGE}");
    let store = SilentStore::open_read_only(&positional[0])?;

    let spans = store.coverage()?;
    let gaps = store.gaps()?;
    if coverage_only {
        println!("coverage ({} span(s)):", spans.len());
        for s in &spans {
            println!("  {s}");
        }
        println!("\ngaps ({}):", gaps.len());
        for g in &gaps {
            println!("  {g}");
        }
        return Ok(());
    }

    if account {
        return accounting(&store, from, to);
    }

    let kinds: Vec<String> = match positional.get(1) {
        Some(k) => vec![k.clone()],
        None => SilentKind::all()
            .iter()
            .map(|k| k.name().to_string())
            .collect(),
    };

    // Said before the results, not after: the reader needs to know how much
    // weight the count below can carry before they read it.
    let covered = store.covers(from, to)?;
    let range = if to == u64::MAX {
        format!("#{from}..")
    } else {
        format!("#{from}..=#{to}")
    };
    if covered {
        println!("{range}: fully replayed\n");
    } else {
        println!(
            "{range}: NOT fully replayed -- the records below are everything found in the \
             parts that were, and say nothing about the rest.\n"
        );
    }

    let mut total = 0;
    for kind in &kinds {
        let rows = store.scan(kind, from, to)?;
        if rows.is_empty() {
            continue;
        }
        println!("{kind}: {} record(s)", rows.len());
        for (block, tx_index, json) in &rows {
            println!("  #{block} tx[{tx_index}]  {json}");
        }
        println!();
        total += rows.len();
    }
    if total == 0 {
        println!("(no records)");
    }
    Ok(())
}

/// Reads one field out of a stored record. They are small and flat, and one
/// place writes them; a typed parse would buy nothing here.
fn field<'a>(json: &'a serde_json::Value, key: &str) -> Option<&'a serde_json::Value> {
    json.get(key)
}

fn satoshis(v: u128) -> String {
    format!("{}.{:08} BTC", v / 100_000_000, v % 100_000_000)
}

/// Totals the bitcoin that reached the federation and was never returned.
fn accounting(store: &SilentStore, from: u64, to: u64) -> Result<()> {
    // Kinds where the bitcoin stayed with the federation and no RBTC was
    // minted against it. `pegin_refunded` is excluded: that value went back,
    // less the bitcoin network fee the refund itself paid.
    const UNRETURNED: [&str; 2] = ["pegin_rejected_no_refund", "refund_build_failed"];

    let range = if to == u64::MAX { format!("#{from}..") } else { format!("#{from}..=#{to}") };
    if store.covers(from, to)? {
        println!("{range}: fully replayed\n");
    } else {
        println!("{range}: NOT fully replayed -- every total below is a LOWER BOUND,");
        println!("covering only the parts that were.\n");
    }

    let mut by_reason: std::collections::BTreeMap<String, (u64, u128)> = Default::default();
    // How many times each bitcoin transaction was registered and rejected.
    // Counted, not just suppressed: a transaction resubmitted hundreds of
    // times is worth seeing, and the count is only visible here because
    // nothing on chain records a rejection that changed no state.
    let mut attempts: std::collections::HashMap<String, (u64, u64, String)> = Default::default();
    let mut seen: std::collections::HashSet<String> = Default::default();
    // For undetermined senders, what the first input actually looked like.
    // Every shape rskj recognises needs a non-empty scriptSig, so a native
    // segwit spend lands here and its bitcoin is kept.
    let mut shapes: std::collections::BTreeMap<String, (u64, u128)> = Default::default();
    let mut records = 0u64;
    let mut repeats = 0u64;

    for kind in UNRETURNED {
        for (_block, _tx, json) in store.scan(kind, from, to)? {
            records += 1;
            let v: serde_json::Value = match serde_json::from_str(&json) {
                Ok(v) => v,
                Err(e) => {
                    eprintln!("skipping an unparseable record: {e}");
                    continue;
                }
            };
            let hash = field(&v, "btcTxHash").and_then(|h| h.as_str()).unwrap_or("").to_string();
            let amount = field(&v, "amountSat").and_then(|a| a.as_u64()).unwrap_or(0);
            let reason =
                field(&v, "reason").and_then(|r| r.as_str()).unwrap_or("(none)").to_string();
            let e = attempts.entry(hash.clone()).or_insert((0, amount, reason.clone()));
            e.0 += 1;
            // The same bitcoin transaction rejected twice moved its coins once.
            if !seen.insert(hash) {
                repeats += 1;
                continue;
            }
            let e = by_reason.entry(reason).or_insert((0, 0));
            e.0 += 1;
            e.1 += amount as u128;
            if let Some(shape) = field(&v, "inputShape").and_then(|s| s.as_str()) {
                let e = shapes.entry(shape.to_string()).or_insert((0, 0));
                e.0 += 1;
                e.1 += amount as u128;
            }
        }
    }

    println!("bitcoin that reached the federation and was never returned:\n");
    println!("  {:<40} {:>8}  {:>22}", "reason", "txs", "value");
    let mut total_txs = 0u64;
    let mut total_sat = 0u128;
    for (reason, (n, v)) in &by_reason {
        println!("  {:<40} {:>8}  {:>22}", reason, n, satoshis(*v));
        total_txs += n;
        total_sat += v;
    }
    println!("  {:-<40} {:->8}  {:->22}", "", "", "");
    println!("  {:<40} {:>8}  {:>22}", "TOTAL", total_txs, satoshis(total_sat));
    println!("\n  from {records} record(s); {repeats} were repeat registrations of a");
    println!("  bitcoin transaction already counted, and are excluded.");

    // The transactions somebody kept resubmitting. Each attempt cost its
    // sender an RSK transaction fee and changed nothing.
    let mut repeated: Vec<(&String, &(u64, u64, String))> =
        attempts.iter().filter(|(_, (n, _, _))| *n > 1).collect();
    repeated.sort_by(|a, b| b.1.0.cmp(&a.1.0).then(b.1.1.cmp(&a.1.1)));
    println!(
        "\n  {} bitcoin transaction(s) were registered more than once.",
        repeated.len()
    );
    if !repeated.is_empty() {
        println!("  the ten most repeated:\n");
        println!("  {:<8} {:>20}  {:<32} btcTxHash", "attempts", "value", "reason");
        for (hash, (n, amount, reason)) in repeated.iter().take(10) {
            println!(
                "  {:<8} {:>20}  {:<32} {}",
                n,
                satoshis(*amount as u128),
                reason,
                hash
            );
        }
    }

    if !shapes.is_empty() {
        println!("\n  undetermined senders, by what the first input actually was:\n");
        println!("  {:<32} {:>8}  {:>22}", "input shape", "txs", "value");
        for (shape, (n, v)) in &shapes {
            println!("  {:<32} {:>8}  {:>22}", shape, n, satoshis(*v));
        }
        println!(
            "\n  every shape rskj recognises requires a non-empty scriptSig, so a native"
        );
        println!("  segwit (bech32) spend cannot be classified and its bitcoin is kept.");
    }

    // Stated rather than left to be assumed: excess bitcoin, but not peg funds.
    for line in [
        "",
        "  None of this is in the Bridge's UTXO set -- every no-refund path returns",
        "  before registerNewUtxos. No RBTC was minted against it, so it is bitcoin",
        "  held in excess of the pegged supply; but the Bridge cannot spend it, and",
        "  a migration carries over only the UTXOs the Bridge knows about. Value",
        "  rejected under an earlier federation is stranded at that federation's",
        "  address, not sitting at the current one.",
    ] {
        println!("{line}");
    }

    // For contrast, what the refund path did return.
    let mut refunded_sat = 0u128;
    let mut refunded_seen: std::collections::HashSet<String> = Default::default();
    for (_b, _t, json) in &store.scan("pegin_refunded", from, to)? {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(json) {
            let h = field(&v, "btcTxHash").and_then(|h| h.as_str()).unwrap_or("").to_string();
            if refunded_seen.insert(h) {
                refunded_sat +=
                    field(&v, "amountSat").and_then(|a| a.as_u64()).unwrap_or(0) as u128;
            }
        }
    }
    println!(
        "\n  for comparison, {} transaction(s) totalling {} WERE refunded",
        refunded_seen.len(),
        satoshis(refunded_sat)
    );
    println!("  (less the bitcoin network fee each refund paid).");
    Ok(())
}
