//! What the canonical `number -> hash` index costs.
//!
//! A snap-synced node has every header on disk but an index covering only the
//! window around its checkpoint, so the question is what filling the rest
//! would take. This measures both halves of that on a real database: how much
//! space the index occupies, and how fast the walk that would build it runs.
//!
//! Opened read-only, so it is safe beside a running node.
//!
//! Usage: canonical_index_cost <data-dir> [sample-blocks]

use rustock_storage::BlockStore;
use std::time::Instant;

fn main() -> anyhow::Result<()> {
    let a: Vec<String> = std::env::args().collect();
    if a.len() < 2 {
        eprintln!("usage: canonical_index_cost <data-dir> [sample-blocks]");
        std::process::exit(2);
    }
    let dir = &a[1];
    let sample: u64 = a.get(2).and_then(|s| s.parse().ok()).unwrap_or(200_000);

    let store = BlockStore::open_read_only(dir)?;
    let head = store.head()?.ok_or_else(|| anyhow::anyhow!("no head"))?;
    let header = store.header(head)?.ok_or_else(|| anyhow::anyhow!("no head header"))?;
    let height = header.number;

    println!("head        #{height}");

    // What the index occupies, as RocksDB accounts for it.
    let db = store.db();
    let cf = store.cf_numbers()?;
    for property in [
        "rocksdb.estimate-live-data-size",
        "rocksdb.total-sst-files-size",
        "rocksdb.estimate-num-keys",
    ] {
        match db.property_int_value_cf(cf, property) {
            Ok(Some(value)) => println!("{property:<34} {value}"),
            _ => println!("{property:<34} (unavailable)"),
        }
    }

    // The walk that would build it: header by header, down the parent chain.
    // Exactly what a background indexer does, minus the writes.
    let mut hash = head;
    let mut walked = 0u64;
    let started = Instant::now();
    while walked < sample {
        let Some(h) = store.header(hash)? else { break };
        if h.number == 0 {
            break;
        }
        hash = h.parent_hash;
        walked += 1;
    }
    let elapsed = started.elapsed();

    println!();
    println!(
        "walked      {walked} headers in {:?} ({:.0} headers/s)",
        elapsed,
        walked as f64 / elapsed.as_secs_f64()
    );

    let rate = walked as f64 / elapsed.as_secs_f64();
    if rate > 0.0 {
        println!(
            "whole chain ~{:.0} s to walk {height} headers at this rate",
            height as f64 / rate
        );
    }

    // Per-entry cost, from what the index actually holds.
    if let Ok(Some(size)) = db.property_int_value_cf(cf, "rocksdb.estimate-live-data-size") {
        if height > 0 {
            println!(
                "index       {:.1} MB for {height} heights ({:.1} bytes per entry)",
                size as f64 / 1e6,
                size as f64 / height as f64
            );
        }
    }
    Ok(())
}
