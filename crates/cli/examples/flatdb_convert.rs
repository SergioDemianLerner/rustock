//! Copy a RocksDB trie store into a FlatDB one, node for node.
//!
//! The benchmark compares two stores, so they have to hold the same state:
//! same nodes, same root, same everything but the layout. Converting rather
//! than rebuilding guarantees that — a rebuild would have to reproduce the
//! fixture's generation exactly, and any divergence would show up as a
//! performance difference that was really a difference in content.
//!
//! ```text
//! flatdb_convert <rocksdb-dir> <flatdb-dir> [--expected-keys N]
//! ```
//!
//! Every key in a trie store is the Keccak digest of its value, which is
//! what makes the destination legal. The conversion checks that rather
//! than assuming it: a key that is not its value's hash would be
//! unreadable afterwards, because FlatDB recomputes short keys on the way
//! out instead of storing them.

use anyhow::{Context, Result};
use rustock_flatdb::Options;
use rustock_storage::FlatTrieStore;
use rustock_trie::TrieStore;
use std::time::Instant;

const CF_TRIE: &str = "trie_nodes";

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() < 2 {
        anyhow::bail!("usage: flatdb_convert <rocksdb-dir> <flatdb-dir> [--expected-keys N]");
    }
    let source = &args[0];
    let target = &args[1];
    let mut expected_keys = 24_000_000u64;
    let mut i = 2;
    while i + 1 < args.len() {
        if args[i] == "--expected-keys" {
            expected_keys = parse_count(&args[i + 1])?;
        }
        i += 2;
    }

    if std::path::Path::new(target).exists() {
        anyhow::bail!("{target} already exists; remove it or choose another path");
    }

    println!("converting {source} -> {target} (index sized for {expected_keys} keys)");
    let started = Instant::now();

    let mut opts = rocksdb::Options::default();
    opts.create_if_missing(false);
    let db = rocksdb::DB::open_cf_for_read_only(&opts, source, [CF_TRIE], false)
        .with_context(|| format!("opening {source}"))?;
    let cf = db.cf_handle(CF_TRIE).context("the trie column family is missing")?;

    let flat = FlatTrieStore::open(
        target,
        Options { expected_keys, read_only: false, sync_writes: false, ..Options::default() },
    )?;

    let mut copied = 0u64;
    let mut bytes = 0u64;
    let mut not_content_addressed = 0u64;
    for item in db.iterator_cf(cf, rocksdb::IteratorMode::Start) {
        let (key, value) = item?;
        // The destination derives short keys from the value, so a key that
        // is not the hash of its value would be written and then never
        // found again. Count rather than abort: the number is the answer
        // to "is this store really content-addressed".
        if key.as_ref() != alloy_primitives::keccak256(value.as_ref()).as_slice() {
            not_content_addressed += 1;
            continue;
        }
        flat.put(&key, &value);
        copied += 1;
        bytes += value.len() as u64;
        if copied.is_multiple_of(1_000_000) {
            println!(
                "  {copied} nodes, {:.1} GB, {:.0}s",
                bytes as f64 / 1e9,
                started.elapsed().as_secs_f64()
            );
        }
    }
    flat.flush();

    if flat.write_failed() {
        anyhow::bail!("a write failed during conversion; {target} is incomplete");
    }

    println!();
    println!("copied      {copied} nodes, {:.2} GB of values", bytes as f64 / 1e9);
    if not_content_addressed > 0 {
        println!(
            "SKIPPED     {not_content_addressed} entries whose key was not keccak(value); \
             the source is not purely content-addressed and the copy is incomplete"
        );
    }
    println!("entries     {}", flat.entries());
    println!("took        {:.1}s", started.elapsed().as_secs_f64());
    for line in flat.stats() {
        println!("  {line}");
    }

    // The fixture's own metadata travels with it, so the benchmark can be
    // pointed at either directory with no other change.
    for name in ["ROOT", "MANIFEST", "README"] {
        let from = std::path::Path::new(source).join(name);
        if from.exists() {
            std::fs::copy(&from, std::path::Path::new(target).join(name))?;
        }
    }
    Ok(())
}

fn parse_count(s: &str) -> Result<u64> {
    let lower = s.to_ascii_lowercase();
    let (digits, scale) = match lower.strip_suffix('m') {
        Some(d) => (d, 1_000_000),
        None => match lower.strip_suffix('k') {
            Some(d) => (d, 1_000),
            None => (lower.as_str(), 1),
        },
    };
    Ok(digits.trim().parse::<u64>()? * scale)
}
