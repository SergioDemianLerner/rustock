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
//! verify_range <data-dir> <rskj-unitrie-dir> <from> <to> [network-id] [scratch-dir]
//!
//!   network-id   30 mainnet, 31 testnet (default), anything else regtest
//!   scratch-dir  where the replay's own trie writes go; a temp dir if omitted
//! ```

#[path = "support/mod.rs"]
mod support;

use anyhow::{Context, Result};
use rocksdb::{DB, Options};
use rustock_core::Block;
use rustock_execution::BlockProcessor;
use rustock_storage::BlockStore;
use rustock_trie::{OverlayTrieStore, TrieNode, TrieStore};
use std::sync::Arc;

const USAGE: &str =
    "usage: verify_range <data-dir> <rskj-unitrie-dir> <from> <to> [network-id] [scratch-dir]";

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
    tracing_subscriber::fmt().with_env_filter("warn").with_target(false).init();

    let mut args = std::env::args().skip(1);
    let data_dir = args.next().ok_or_else(|| anyhow::anyhow!(USAGE))?;
    let unitrie_dir = args.next().ok_or_else(|| anyhow::anyhow!(USAGE))?;
    let from: u64 = args.next().ok_or_else(|| anyhow::anyhow!(USAGE))?.parse()?;
    let to: u64 = args.next().ok_or_else(|| anyhow::anyhow!(USAGE))?.parse()?;
    let network_id: u64 = args.next().map(|a| a.parse()).transpose()?.unwrap_or(support::TESTNET);
    let scratch = args.next();
    anyhow::ensure!(from <= to, "empty range: {from} > {to}");

    let store = Arc::new(
        BlockStore::open_read_only(&data_dir)
            .with_context(|| format!("opening block store at {data_dir} read-only"))?,
    );

    let mut opts = Options::default();
    let mut bbt = rocksdb::BlockBasedOptions::default();
    bbt.set_block_cache(&rocksdb::Cache::new_lru_cache(2 * 1024 * 1024 * 1024));
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

    let started = std::time::Instant::now();
    let mut checked = 0u64;
    let mut diverged = 0u64;
    let mut unverifiable = 0u64;
    let mut first_bad: Option<u64> = None;

    for n in from..=to {
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
            eprintln!(
                "#{n}: parent state root {:?} is not in the rskj unitrie — \
                 that database is pruned below this height",
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

    println!("\n=== #{from}..=#{to} ===");
    println!("  verified      {checked}");
    println!("  diverged      {diverged}");
    println!("  unverifiable  {unverifiable}");
    if let Some(n) = first_bad {
        println!("  first failure #{n}");
        println!("\n  next: `block_writes {data_dir} {n} {network_id}` for what that block wrote");
    }
    println!("  took {:.0}s", started.elapsed().as_secs_f64());

    // A non-zero exit makes this usable from a script without parsing output.
    if diverged > 0 {
        std::process::exit(1);
    }
    Ok(())
}
