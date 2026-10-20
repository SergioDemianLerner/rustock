//! Move an epoch store from RocksDB to FlatDB, in place, node stopped.
//!
//! ```text
//! flatdb_migrate <epoch-root> --blocks <block-db> [--expected-keys N]
//!                [--segment-mb N] [--verify full|root] [--keep-original]
//! ```
//!
//! The work lives in `rustock_storage::flat_migrate`, which is where its
//! tests are. This is argument parsing and a progress report.
//!
//! Without `--blocks` nothing is verified and nothing is swapped: the
//! converted epochs are left beside the originals, which is a legitimate
//! way to do the slow half in advance.

use anyhow::{bail, Context, Result};
use rustock_storage::flat_migrate::{migrate_epoch_store, Sizing, Verify};
use rustock_storage::BlockStore;
use std::path::PathBuf;
use std::time::Instant;

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        bail!(
            "usage: flatdb_migrate <epoch-root> --blocks <block-db> \
             [--expected-keys N] [--segment-mb N] [--verify full|root] [--keep-original]"
        );
    }
    let root = PathBuf::from(&args[0]);
    let mut blocks: Option<String> = None;
    let mut sizing = Sizing::default();
    let mut verify = Verify::Full;
    let mut keep_original = false;

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--blocks" => {
                blocks = Some(args.get(i + 1).context("--blocks needs a path")?.clone());
                i += 2;
            }
            "--expected-keys" => {
                sizing.expected_keys =
                    parse_count(args.get(i + 1).context("--expected-keys needs a value")?)?;
                i += 2;
            }
            "--segment-mb" => {
                let mb: u64 = args
                    .get(i + 1)
                    .context("--segment-mb needs a value")?
                    .parse()?;
                sizing.segment_size = mb * (1 << 20);
                i += 2;
            }
            "--verify" => {
                verify = match args.get(i + 1).map(String::as_str) {
                    Some("root") => Verify::RootOnly,
                    Some("full") | None => Verify::Full,
                    Some(other) => bail!("--verify takes full or root, not {other:?}"),
                };
                i += 2;
            }
            "--keep-original" => {
                keep_original = true;
                i += 1;
            }
            other => bail!("unknown option {other}"),
        }
    }

    // The node refuses to open a store whose segment size differs from
    // its own, so a migration that guessed would convert cleanly and then
    // not start.
    println!(
        "migrating {} (index for {} keys, {} MB segments)",
        root.display(),
        sizing.expected_keys,
        sizing.segment_size / (1 << 20)
    );

    let state_root = match &blocks {
        Some(path) => {
            let store = BlockStore::open_read_only(path)
                .with_context(|| format!("opening the block database at {path}"))?;
            match store.exec_head()? {
                Some((_, state_root)) => Some(state_root),
                None => bail!(
                    "the block database has no executed head, so there is no state \
                     root to verify the migration against"
                ),
            }
        }
        None => None,
    };
    if let Some(root) = state_root {
        println!("verifying against the executed state root {root:?}");
    } else {
        println!("no --blocks given: converting only, nothing will be verified or swapped");
    }

    let started = Instant::now();
    let report = migrate_epoch_store(&root, sizing, state_root, verify, keep_original)?;

    println!();
    println!("epochs            {}", report.epochs);
    println!("  converted       {}", report.epochs - report.skipped_epochs);
    println!("  already done    {}", report.skipped_epochs);
    println!("nodes copied      {}", report.converted_nodes);
    if report.not_content_addressed > 0 {
        println!(
            "SKIPPED           {} entries whose key was not keccak(value); the source \
             was not purely content-addressed and the copy is incomplete",
            report.not_content_addressed
        );
    }
    if state_root.is_some() {
        println!("nodes resolved    {}", report.resolved_nodes);
    }
    println!("took              {:.1}s", started.elapsed().as_secs_f64());

    if state_root.is_some() {
        println!();
        println!("Start the node with --trie-engine flatdb.");
        if keep_original {
            println!("The originals are at epoch-N.rocksdb; remove them once satisfied.");
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
