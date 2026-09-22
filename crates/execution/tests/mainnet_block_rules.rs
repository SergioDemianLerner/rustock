//! Differential check of the block-validity rules against real mainnet blocks.
//!
//! These rules *reject* blocks, so the risk they carry is the opposite of the
//! usual one: a rule that is too strict rejects a block the network accepted,
//! which halts the node. Unit tests cannot find that — only real data can.
//!
//! Ignored by default; point it at a synced block store to run:
//!
//! ```sh
//! RUSTOCK_BLOCKS_DB=/var/lib/rustock \
//!   cargo test --release -p rustock-execution --test mainnet_block_rules -- --ignored --nocapture
//! ```
use rustock_execution::hardfork::RskHardforkConfig;
use rustock_execution::processor::BlockProcessor;
use rustock_storage::BlockStore;
use std::sync::Arc;

fn db_path() -> Option<String> {
    std::env::var("RUSTOCK_BLOCKS_DB").ok()
}

#[test]
#[ignore = "requires a synced block store (RUSTOCK_BLOCKS_DB)"]
fn block_rules_accept_every_sampled_mainnet_block() {
    let Some(path) = db_path() else {
        eprintln!("RUSTOCK_BLOCKS_DB not set; skipping");
        return;
    };
    let store = Arc::new(BlockStore::open_read_only(&path).expect("open block store"));
    let head = store
        .head()
        .expect("head")
        .and_then(|h| store.header(h).ok().flatten())
        .expect("head header");
    println!("head #{}", head.number);

    let processor = BlockProcessor::new(RskHardforkConfig::mainnet(), store.clone());

    // Sample across every era, plus a dense run either side of each activation
    // the new rules key on.
    // Genesis is excluded on purpose: rskj's `BlockValidatorImpl.isValid`
    // rejects it outright because it is loaded from the genesis file rather
    // than validated, and it carries no REMASC transaction.
    let mut heights: Vec<u64> = (0..=head.number).step_by(50_000).filter(|n| *n > 0).collect();
    for anchor in [1_591_000u64, 3_614_800, 5_468_000, 6_223_700, 7_338_024, 8_052_200] {
        for d in 0..40u64 {
            if anchor >= 20 && anchor - 20 + d <= head.number {
                heights.push(anchor - 20 + d);
            }
        }
    }
    heights.sort_unstable();
    heights.dedup();

    let mut checked = 0usize;
    let mut skipped = 0usize;
    let mut failures: Vec<(u64, String)> = Vec::new();

    for number in heights {
        let Some(hash) = store.canonical_hash(number).ok().flatten() else {
            skipped += 1;
            continue;
        };
        let Some(block) = store.block(hash).ok().flatten() else {
            skipped += 1;
            continue;
        };
        let parent = store.header(block.header.parent_hash).ok().flatten();
        match processor.validate_block_rules(&block, parent.as_ref()) {
            Ok(()) => checked += 1,
            Err(e) => failures.push((number, format!("{e}"))),
        }
    }

    println!("checked {checked}, skipped {skipped}, failures {}", failures.len());
    for (number, err) in failures.iter().take(25) {
        println!("  #{number}: {err}");
    }
    assert!(
        failures.is_empty(),
        "{} mainnet blocks were rejected by the new rules",
        failures.len()
    );
}

/// The uncle rules only mean something on blocks that actually carry uncles,
/// which the uniform sample above mostly misses. Walk a dense range and assert
/// every uncle-bearing block still passes.
#[test]
#[ignore = "requires a synced block store (RUSTOCK_BLOCKS_DB)"]
fn uncle_rules_accept_real_uncle_bearing_blocks() {
    let Some(path) = db_path() else {
        eprintln!("RUSTOCK_BLOCKS_DB not set; skipping");
        return;
    };
    let store = Arc::new(BlockStore::open_read_only(&path).expect("open block store"));
    let head = store
        .head()
        .expect("head")
        .and_then(|h| store.header(h).ok().flatten())
        .expect("head header");
    let processor = BlockProcessor::new(RskHardforkConfig::mainnet(), store.clone());

    let start = head.number.saturating_sub(20_000);
    let mut with_uncles = 0usize;
    let mut failures: Vec<(u64, String)> = Vec::new();

    for number in start..head.number {
        let Some(hash) = store.canonical_hash(number).ok().flatten() else { continue };
        let Some(block) = store.block(hash).ok().flatten() else { continue };
        if block.ommers.is_empty() {
            continue;
        }
        with_uncles += 1;
        let parent = store.header(block.header.parent_hash).ok().flatten();
        if let Err(e) = processor.validate_block_rules(&block, parent.as_ref()) {
            failures.push((number, format!("{e}")));
        }
    }

    println!("uncle-bearing blocks checked: {with_uncles}, failures {}", failures.len());
    for (number, err) in failures.iter().take(25) {
        println!("  #{number}: {err}");
    }
    assert!(with_uncles > 0, "sample contained no uncle-bearing blocks");
    assert!(failures.is_empty(), "{} uncle-bearing blocks rejected", failures.len());
}

/// The RSKIP110 fork-detection rule is the one most likely to be subtly wrong:
/// it reconstructs 12 bytes from 449 ancestors, including a commit-to-parents
/// vector built from the least-significant byte of seven BTC block hashes —
/// a byte order that is easy to get backwards. Run it over a dense recent
/// range, where every block must match.
#[test]
#[ignore = "requires a synced block store (RUSTOCK_BLOCKS_DB)"]
fn fork_detection_data_matches_on_real_blocks() {
    let Some(path) = db_path() else {
        eprintln!("RUSTOCK_BLOCKS_DB not set; skipping");
        return;
    };
    let store = Arc::new(BlockStore::open_read_only(&path).expect("open block store"));
    let head = store
        .head()
        .expect("head")
        .and_then(|h| store.header(h).ok().flatten())
        .expect("head header");
    let processor = BlockProcessor::new(RskHardforkConfig::mainnet(), store.clone())
        .with_fork_detection_validation(true);

    let start = head.number.saturating_sub(300);
    let mut checked = 0usize;
    let mut failures: Vec<(u64, String)> = Vec::new();

    for number in start..head.number {
        let Some(hash) = store.canonical_hash(number).ok().flatten() else { continue };
        let Some(block) = store.block(hash).ok().flatten() else { continue };
        let parent = store.header(block.header.parent_hash).ok().flatten();
        match processor.validate_block_rules(&block, parent.as_ref()) {
            Ok(()) => checked += 1,
            Err(e) => failures.push((number, format!("{e}"))),
        }
    }

    println!("fork-detection blocks checked: {checked}, failures {}", failures.len());
    for (number, err) in failures.iter().take(10) {
        println!("  #{number}: {err}");
    }
    assert!(checked > 100, "expected a dense run of blocks, got {checked}");
    assert!(failures.is_empty(), "{} blocks failed fork detection", failures.len());
}
