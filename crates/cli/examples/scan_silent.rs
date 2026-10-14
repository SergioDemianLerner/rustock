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
//! scan_silent <store-dir> [kind] [--from N] [--to N] [--coverage]
//!
//!   kind        pegin_refunded, pegin_rejected_no_refund, pegin_accepted,
//!               btc_tx_marked_processed, whitelist_entry_consumed,
//!               refund_build_failed (default: every kind)
//!   --coverage  print which ranges were replayed, and stop
//! ```
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
    let mut it = argv.into_iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--coverage" => coverage_only = true,
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
