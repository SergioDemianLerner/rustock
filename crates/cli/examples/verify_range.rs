//! Verify a range of blocks against rskj's own state, reporting every
//! divergence rather than stopping at the first.
//!
//! A syncing node is the wrong instrument for this. It executes forward from
//! its own post-state, so the first mismatch both halts it and poisons every
//! block after; finding the second costs a fix, a rebuild and a restart.
//!
//! This takes each block's pre-state from **rskj's** database — the parent
//! header's state root, resolved in rskj's unitrie — so blocks are verified
//! independently. A wrong block says nothing about the next one, and a single
//! pass over a fork boundary reports everything that disagrees.
//!
//! Nothing is written to either database. Blocks are read from a rustock store
//! opened read-only, state from rskj's unitrie opened read-only, and the trie
//! the replay computes goes to a scratch store that can be deleted afterwards.
//!
//! ```text
//! verify_range <data-dir> <rskj-unitrie-dir> <from> <to> [options]
//!
//!   --network-id N     30 mainnet, 31 testnet (default), else regtest
//!   --scratch DIR      where the replay's own trie writes go (default: a temp dir)
//!   --max-failures N   stop after this many (default 10; 0 means never)
//!   --cache-mb N       block cache for the rskj unitrie (default 256)
//!   --explain          on a state-root divergence, print the leaves that
//!                      differ from rskj's own post-state, then stop
//! ```
//!
//! The stop exists because one defect usually breaks every block it touches.
//! Ten instances of a rule that is wrong at every block say exactly what one
//! says, so running the remaining fifty thousand buys nothing and costs an
//! hour. The same goes for a range the snapshot cannot serve: if the first
//! blocks have no pre-state, none of them will.

#[path = "support/mod.rs"]
mod support;

use anyhow::{Context, Result};
use rocksdb::{DB, Options};
use rustock_core::Block;
use rustock_execution::BlockProcessor;
use rustock_storage::BlockStore;
use rustock_trie::{OverlayTrieStore, TrieNode, TrieStore};
use std::sync::Arc;

const USAGE: &str = "usage: verify_range <data-dir> <rskj-unitrie-dir> <from> <to> \
[--network-id N] [--scratch DIR] [--max-failures N] [--cache-mb N]";

/// Splits `--key value` and `--key=value` options out of the command line,
/// leaving the positional arguments.
///
/// Hand-rolled because these diagnostics take no dependency on an argument
/// parser, and an unknown option must be an error rather than be silently
/// read as a positional -- a mistyped `--max-failures` that became a block
/// number would be worse than no option at all.
fn split_options(argv: Vec<String>) -> Result<(Vec<String>, Vec<(String, String)>)> {
    const WITH_VALUE: [&str; 4] = ["--network-id", "--scratch", "--max-failures", "--cache-mb"];
    const FLAGS: [&str; 1] = ["--explain"];
    let mut positional = Vec::new();
    let mut options = Vec::new();
    let mut it = argv.into_iter();
    while let Some(a) = it.next() {
        if !a.starts_with("--") {
            positional.push(a);
            continue;
        }
        let (key, inline) = match a.split_once('=') {
            Some((k, v)) => (k.to_string(), Some(v.to_string())),
            None => (a.clone(), None),
        };
        if FLAGS.contains(&key.as_str()) {
            options.push((key, "true".to_string()));
            continue;
        }
        anyhow::ensure!(WITH_VALUE.contains(&key.as_str()), "unknown option {key}\n{USAGE}");
        let value = match inline {
            Some(v) => v,
            None => it.next().with_context(|| format!("{key} needs a value"))?,
        };
        options.push((key, value));
    }
    Ok((positional, options))
}

fn option<T: std::str::FromStr>(
    options: &[(String, String)],
    key: &str,
    default: T,
) -> Result<T>
where
    T::Err: std::fmt::Display,
{
    match options.iter().rev().find(|(k, _)| k == key) {
        Some((_, v)) => v
            .parse()
            .map_err(|e| anyhow::anyhow!("{key}: {e}")),
        None => Ok(default),
    }
}

/// rskj's unitrie, read-only. Content-addressed exactly as rustock's is.
struct RskjUnitrie {
    db: DB,
}

impl TrieStore for RskjUnitrie {
    fn get(&self, key: &[u8]) -> Option<Vec<u8>> {
        self.db.get(key).ok().flatten()
    }
    fn put(&self, _key: &[u8], _value: &[u8]) {
        panic!("the rskj source must never be written to");
    }
}

/// What went wrong with one block, if anything.
#[derive(Default)]
struct Verdict {
    state: bool,
    receipts: bool,
    gas: bool,
}

impl Verdict {
    fn ok(&self) -> bool {
        !self.state && !self.receipts && !self.gas
    }
    fn label(&self) -> String {
        let mut parts = Vec::new();
        if self.state {
            parts.push("state root");
        }
        if self.receipts {
            parts.push("receipts root");
        }
        if self.gas {
            parts.push("gas used");
        }
        parts.join(", ")
    }
}

fn main() -> Result<()> {
    // Honour RUST_LOG so a divergence can be chased with the execution
    // crate's own debug output, which is usually the fastest next step.
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn"));
    tracing_subscriber::fmt().with_env_filter(filter).with_target(false).init();

    let (positional, options) = split_options(std::env::args().skip(1).collect())?;
    anyhow::ensure!(positional.len() == 4, "{USAGE}");
    let data_dir = positional[0].clone();
    let unitrie_dir = positional[1].clone();
    let from: u64 = positional[2].parse().context("<from>")?;
    let to: u64 = positional[3].parse().context("<to>")?;
    let network_id: u64 = option(&options, "--network-id", support::TESTNET)?;
    let scratch: Option<String> =
        options.iter().rev().find(|(k, _)| k == "--scratch").map(|(_, v)| v.clone());
    // 0 disables the stop, for a run that is deliberately exhaustive.
    let max_failures: u64 = option(&options, "--max-failures", 10)?;
    // Small by default: this runs beside whatever else is on the machine, and
    // a cache sized for the database rather than the box gets it killed.
    let cache_mb: usize = option(&options, "--cache-mb", 256)?;
    let explain = options.iter().any(|(k, _)| k == "--explain");
    anyhow::ensure!(from <= to, "empty range: {from} > {to}");

    let store = Arc::new(
        BlockStore::open_read_only(&data_dir)
            .with_context(|| format!("opening block store at {data_dir} read-only"))?,
    );

    let mut opts = Options::default();
    let mut bbt = rocksdb::BlockBasedOptions::default();
    bbt.set_block_cache(&rocksdb::Cache::new_lru_cache(cache_mb * 1024 * 1024));
    bbt.set_cache_index_and_filter_blocks(true);
    opts.set_block_based_table_factory(&bbt);
    let rskj: Arc<dyn TrieStore> = Arc::new(RskjUnitrie {
        db: DB::open_for_read_only(&opts, &unitrie_dir, false)
            .with_context(|| format!("opening rskj unitrie at {unitrie_dir} read-only"))?,
    });

    // Somewhere for the replay's own writes that is neither database. Kept on
    // disk rather than in memory because a 50,000-block range writes far more
    // trie than belongs in RAM.
    let tmp;
    let scratch_path = match &scratch {
        Some(p) => std::path::PathBuf::from(p),
        None => {
            tmp = tempfile::tempdir().context("creating a scratch directory")?;
            tmp.path().to_path_buf()
        }
    };
    let spill: Arc<dyn TrieStore> = Arc::new(
        rustock_storage::RocksDbTrieStore::open(&scratch_path)
            .with_context(|| format!("opening scratch trie at {}", scratch_path.display()))?,
    );
    let trie: Arc<dyn TrieStore> = Arc::new(OverlayTrieStore::with_spill(rskj, spill));

    let hardfork_cfg = support::hardfork_for(network_id);
    eprintln!(
        "verifying #{from}..=#{to} ({} blocks) on network-id {network_id} (chain_id {})",
        to - from + 1,
        hardfork_cfg.chain_id
    );
    eprintln!("  blocks  {data_dir} (read-only)");
    eprintln!("  state   {unitrie_dir} (read-only)");
    eprintln!("  scratch {}", scratch_path.display());
    let processor = BlockProcessor::new(hardfork_cfg, store.clone());

    if max_failures > 0 {
        eprintln!("  stopping after {max_failures} divergent or unverifiable block(s)");
    }

    let started = std::time::Instant::now();
    let mut checked = 0u64;
    let mut diverged = 0u64;
    let mut unverifiable = 0u64;
    let mut first_bad: Option<u64> = None;
    let mut stopped_at: Option<u64> = None;

    for n in from..=to {
        // Checked at the top of each block so every exit from the body below
        // -- divergence or unverifiable -- is counted before it is consulted.
        if max_failures > 0 && diverged + unverifiable >= max_failures {
            stopped_at = Some(n - 1);
            eprintln!(
                "\nstopping at #{}: {max_failures} failures reached. \
                 Re-run with --max-failures 0 to see the rest.",
                n - 1
            );
            break;
        }

        let Some(hash) = store.canonical_hash(n)? else {
            eprintln!("#{n}: no canonical hash — the metadata rebuild did not reach here");
            unverifiable += 1;
            continue;
        };
        let Some(header) = store.header(hash)? else {
            eprintln!("#{n}: header missing");
            unverifiable += 1;
            continue;
        };
        let Some((transactions, ommers)) = store.body(hash)? else {
            eprintln!("#{n}: body missing");
            unverifiable += 1;
            continue;
        };

        // The pre-state is rskj's, not ours: that is what makes each block an
        // independent check instead of a continuation of the last one.
        let Some(parent) = store.header(header.parent_hash)? else {
            eprintln!("#{n}: parent header missing");
            unverifiable += 1;
            continue;
        };
        let Some(root_data) = trie.get(parent.state_root.as_slice()) else {
            // States the fact, not a cause: an absent root means the source
            // was pruned below here, or that these blocks and this unitrie
            // are from different chains. Naming only one of those sends the
            // reader after the wrong thing.
            eprintln!(
                "#{n}: parent state root {:?} is not in the source unitrie \
                 (pruned below this height, or not the same chain as these blocks)",
                parent.state_root
            );
            unverifiable += 1;
            continue;
        };
        let pre = TrieNode::from_message(&root_data, trie.as_ref());

        let block = Block { header, transactions, ommers };
        let processed = match processor.execute_block(&block, &pre, trie.clone()) {
            Ok(p) => p,
            Err(e) => {
                println!("#{n}  EXECUTION ERROR  {e}");
                diverged += 1;
                first_bad.get_or_insert(n);
                continue;
            }
        };

        let verdict = Verdict {
            state: processed.state_root_hash != block.header.state_root,
            receipts: processed.receipts_root != block.header.receipts_root,
            gas: processed.gas_used != block.header.gas_used,
        };
        checked += 1;
        if !verdict.ok() {
            diverged += 1;
            first_bad.get_or_insert(n);
            println!("#{n}  DIVERGES  ({})", verdict.label());
            if verdict.state {
                println!("    state    computed {:?}", processed.state_root_hash);
                println!("             header   {:?}", block.header.state_root);
            }
            if verdict.receipts {
                println!("    receipts computed {:?}", processed.receipts_root);
                println!("             header   {:?}", block.header.receipts_root);
            }
            if verdict.gas {
                println!(
                    "    gas      computed {} header {} ({} txs)",
                    processed.gas_used,
                    block.header.gas_used,
                    block.transactions.len()
                );
            }

            // Both post-states are reachable through the same store -- ours
            // from the scratch spill, rskj's from its unitrie -- so the two
            // can be walked against each other directly. That names the
            // leaves that differ instead of leaving a pair of root hashes.
            if explain && verdict.state {
                match trie.get(block.header.state_root.as_slice()) {
                    None => println!(
                        "    (rskj's own post-state for this block is not in its unitrie)"
                    ),
                    Some(bytes) => {
                        let theirs_root = TrieNode::from_message(&bytes, trie.as_ref());
                        let (mine, theirs) = support::diff_roots(
                            &processed.new_state_root,
                            &theirs_root,
                            trie.as_ref(),
                        );
                        let a = support::index(mine);
                        let b = support::index(theirs);
                        let mut keys: Vec<&Vec<u8>> = a.keys().chain(b.keys()).collect();
                        keys.sort();
                        keys.dedup();
                        let differing: Vec<&Vec<u8>> = keys
                            .into_iter()
                            .filter(|k| a.get(*k).map(|e| e.0) != b.get(*k).map(|e| e.0))
                            .collect();
                        // The pre-state too: it separates "wrote the wrong
                        // value" from "wrote nothing and kept the old one",
                        // which are different bugs and look identical without
                        // it.
                        let (pre_leaves, _) =
                            support::diff_roots(&pre, &processed.new_state_root, trie.as_ref());
                        let p = support::index(pre_leaves);

                        println!("\n    {} leaf/leaves differ from rskj:", differing.len());
                        for k in differing {
                            println!("      {}", support::describe_key(k));
                            println!("        key     0x{}", alloy_primitives::hex::encode(k));
                            println!(
                                "        before  {}",
                                support::describe_value(k, p.get(k).and_then(|e| e.1.as_ref()))
                            );
                            println!(
                                "        ours    {}",
                                support::describe_value(k, a.get(k).and_then(|e| e.1.as_ref()))
                            );
                            println!(
                                "        rskj    {}",
                                support::describe_value(k, b.get(k).and_then(|e| e.1.as_ref()))
                            );
                        }
                        stopped_at = Some(n);
                        println!("\n    (--explain stops at the first explained block)");
                        break;
                    }
                }
            }
        }

        if n % 1000 == 0 || n == to {
            let secs = started.elapsed().as_secs_f64();
            eprintln!(
                "  #{n}  {checked} verified, {diverged} diverged, {unverifiable} unverifiable \
                 | {:.0} blk/s",
                (n - from + 1) as f64 / secs.max(0.001)
            );
        }
    }

    let reached = stopped_at.unwrap_or(to);
    println!("\n=== #{from}..=#{reached} ===");
    if let Some(at) = stopped_at {
        // Said plainly, so the counts are never mistaken for a full pass over
        // the range that was asked for.
        println!("  STOPPED EARLY at #{at}; #{}..=#{to} was not examined", at + 1);
    }
    println!("  verified      {checked}");
    println!("  diverged      {diverged}");
    println!("  unverifiable  {unverifiable}");
    if let Some(n) = first_bad {
        println!("  first failure #{n}");
        println!("\n  next: re-run with --explain to see which leaves differ from rskj");
    }
    println!("  took {:.0}s", started.elapsed().as_secs_f64());

    // A non-zero exit makes this usable from a script without parsing output.
    // An early stop counts as failure even if every examined block passed,
    // because the range was not actually cleared.
    if diverged > 0 || stopped_at.is_some() {
        std::process::exit(1);
    }
    Ok(())
}
