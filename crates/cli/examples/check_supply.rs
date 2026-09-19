//! Replay canonical blocks and report any that do not conserve the native
//! supply, changing nothing on disk.
//!
//! The Bridge holds the whole 21 M bitcoin, so a peg-in moves value out of it
//! rather than minting any: the sum of all balances must never grow. This
//! replays a range of blocks through the same `execute_block` the node uses and
//! reports what the supply check finds.
//!
//! **Read-only, at the database layer.** Each epoch is opened with
//! `DB::open_for_read_only`, which neither replays the write-ahead log nor
//! writes anything -- not even RocksDB's own recovery. Discarding writes at the
//! `TrieStore` layer is not sufficient on its own: an earlier version of this
//! tool opened the epoch store read-write, and RocksDB flushed a recovered WAL
//! into a new SST at open time, before a single block had been replayed.
//!
//! Nothing touches receipts, bodies or the canonical index, and no alert
//! observer is installed, so no mail is sent whatever it finds.
//!
//! Usage: check_supply <data-dir> <count> [end-block] [--trie-dir DIR]
//!   count       how many blocks back from the end to replay
//!   end-block   last block to replay (default: the executed head)
//!   --archive-trie  a plain (non-epoch) trie store, such as the archival
//!               unitrie, which is the only place state for older blocks lives
//!   --trie-dir  epoch-backend trie directory, when the node keeps state
//!               outside the main database (as the production node does)

use rustock_core::Block;
use rustock_execution::{BlockProcessor, RskHardforkConfig};
use rustock_storage::{BlockStore, CachedTrieStore};
use rustock_trie::{TrieNode, TrieStore};
use std::sync::Arc;
use std::time::Instant;

/// Reads through to the real store; silently drops every write.
///
/// `execute_block` saves the post-state trie as it goes. For a diagnostic run
/// that would write nodes for state the node already has, so the writes go
/// nowhere and the replay stays a pure reader.
struct ReadOnly(Arc<dyn TrieStore>);

impl TrieStore for ReadOnly {
    fn get(&self, key: &[u8]) -> Option<Vec<u8>> {
        self.0.get(key)
    }
    fn put(&self, _key: &[u8], _value: &[u8]) {}
    fn flush(&self) {}
}

/// The epoch trie backend, opened strictly for reading.
///
/// Mirrors `EpochTrieStore`'s lookup exactly -- newest epoch first, first hit
/// wins -- but every database is opened with `open_for_read_only(.., false)`,
/// which skips WAL replay and writes nothing at all.
/// Column family the epoch backend stores trie nodes in.
const CF_TRIE: &str = "trie_nodes";

struct ReadOnlyEpochs {
    /// Oldest first, matching the on-disk ordering; probed in reverse.
    epochs: Vec<rocksdb::DB>,
}

impl ReadOnlyEpochs {
    fn open(dir: &str) -> anyhow::Result<Self> {
        let mut dirs: Vec<std::path::PathBuf> = std::fs::read_dir(dir)?
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_dir() && p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("epoch-")))
            .collect();
        dirs.sort();
        anyhow::ensure!(!dirs.is_empty(), "no epoch-* directories under {dir}");

        let mut opts = rocksdb::Options::default();
        opts.create_if_missing(false);
        let mut epochs = Vec::new();
        for d in &dirs {
            // false => do not fail if a WAL exists; it is simply not replayed.
            // The column family must be named explicitly: falling back to a
            // default-CF open would succeed and then silently read nothing.
            let db = rocksdb::DB::open_cf_for_read_only(&opts, d, [CF_TRIE], false)
                .map_err(|e| anyhow::anyhow!("opening {} read-only: {e}", d.display()))?;
            anyhow::ensure!(
                db.cf_handle(CF_TRIE).is_some(),
                "{} has no {CF_TRIE} column family", d.display()
            );
            epochs.push(db);
        }
        tracing::info!("supply check: opened {} epoch(s) read-only from {dir}", epochs.len());
        Ok(Self { epochs })
    }
}

impl TrieStore for ReadOnlyEpochs {
    fn get(&self, key: &[u8]) -> Option<Vec<u8>> {
        for db in self.epochs.iter().rev() {
            let cf = db.cf_handle(CF_TRIE).expect("checked at open");
            if let Ok(Some(v)) = db.get_cf(cf, key) {
                return Some(v);
            }
        }
        None
    }
    fn put(&self, _key: &[u8], _value: &[u8]) {}
    fn flush(&self) {}
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            std::env::var("RUST_LOG").unwrap_or_else(|_| "info,rustock::supply=info".into()),
        )
        .with_target(false)
        .init();

    // Positional arguments, with --trie-dir and its value removed first so an
    // optional end-block cannot swallow the flag.
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let mut positional: Vec<String> = Vec::new();
    let mut trie_dir: Option<String> = None;
    let mut archive_trie: Option<String> = None;
    let mut i = 0;
    while i < argv.len() {
        if argv[i] == "--trie-dir" {
            trie_dir = argv.get(i + 1).cloned();
            i += 2;
        } else if argv[i] == "--archive-trie" {
            archive_trie = argv.get(i + 1).cloned();
            i += 2;
        } else {
            positional.push(argv[i].clone());
            i += 1;
        }
    }
    let data_dir = positional.first().cloned()
        .expect("usage: check_supply <data-dir> <count> [end-block] [--trie-dir DIR]");
    let count: u64 = positional.get(1)
        .expect("usage: check_supply <data-dir> <count> [end-block] [--trie-dir DIR]")
        .parse()?;
    let end_arg: Option<u64> = match positional.get(2) {
        Some(s) => Some(s.parse()?),
        None => None,
    };

    let store = Arc::new(BlockStore::open(&data_dir)?);
    let (exec_hash, _) = store.exec_head()?.expect("no exec_head");
    let exec_number = store.header(exec_hash)?.expect("exec head header").number;
    let end: u64 = end_arg.unwrap_or(exec_number);
    let start = end.saturating_sub(count - 1).max(1);

    // Seed from the state of the block BEFORE the range, so the first replayed
    // block is executed against its true parent state.
    let seed_hash = store.canonical_hash(start - 1)?.expect("parent of range");
    let seed_header = store.header(seed_hash)?.expect("parent header");

    // The node may keep state in the epoch backend rather than the main
    // database; open whichever actually holds the seed root.
    let backing: Arc<dyn TrieStore> = match (&archive_trie, &trie_dir) {
        (Some(dir), _) => {
            tracing::info!("supply check: archival trie at {dir} (read-only)");
            Arc::new(rustock_storage::RocksDbTrieStore::open_read_only(dir)?)
        }
        (None, d) => match d {
        Some(dir) => {
            tracing::info!("supply check: epoch trie backend at {dir}");
            Arc::new(ReadOnlyEpochs::open(dir)?)
        }
        None => Arc::new(CachedTrieStore::with_defaults(store.db().clone())),
        },
    };
    let trie_store: Arc<dyn TrieStore> = Arc::new(ReadOnly(backing.clone()));
    let root_data = backing.get(seed_header.state_root.as_slice()).unwrap_or_else(|| {
        panic!(
            "state root {} for block #{} is not in the trie store -- pass --trie-dir if the \
             node uses the epoch backend, and check the range is within the retained window",
            seed_header.state_root, start - 1
        )
    });
    let mut root = TrieNode::from_message(&root_data, trie_store.as_ref());

    tracing::info!("supply check: blocks #{start}..#{end} ({} blocks)", end - start + 1);
    tracing::info!("supply check: read-only -- trie writes discarded, no receipts, no alerts");
    tracing::info!("supply check: seeded from #{} root {}", start - 1, seed_header.state_root);

    let processor = BlockProcessor::new(RskHardforkConfig::mainnet(), store.clone());
    let started = Instant::now();
    let (mut ok, mut created, mut destroyed, mut other) = (0u64, 0u64, 0u64, 0u64);

    for n in start..=end {
        let hash = store.canonical_hash(n)?.expect("canonical hash");
        let header = store.header(hash)?.expect("header");
        let (transactions, ommers) = store.body(hash)?.expect("body");
        let expected_root = header.state_root;
        let block = Block { header, transactions, ommers };

        match processor.execute_block(&block, &root, trie_store.clone()) {
            Ok(processed) => {
                if processed.state_root_hash.as_slice() != expected_root.as_slice() {
                    tracing::error!(
                        "#{n}: state root mismatch (computed {} header {}), cannot continue \
                         -- the replay has diverged and later blocks would be meaningless",
                        processed.state_root_hash, expected_root
                    );
                    other += 1;
                    break;
                }
                root = processed.new_state_root;
                ok += 1;
            }
            Err(rustock_execution::processor::ProcessError::SupplyCreated { number, created: amt }) => {
                tracing::error!("#{number}: SUPPLY CREATED, {amt} wei -- block would be rejected");
                created += 1;
                break;
            }
            Err(e) => {
                tracing::error!("#{n}: execution error: {e}");
                other += 1;
                break;
            }
        }
        if n % 500 == 0 || n == end {
            let done = n - start + 1;
            tracing::info!(
                "  {:.1}% (#{n}) | {done} replayed | {:.0} blocks/s",
                done as f64 * 100.0 / (end - start + 1) as f64,
                done as f64 / started.elapsed().as_secs_f64().max(0.001)
            );
        }
    }

    // Burns are counted from the log, not here: they do not stop a block.
    let _ = &mut destroyed;
    tracing::info!(
        "supply check: DONE in {:.1}s | {ok} blocks conserved the supply, \
         {created} would be REJECTED for creating it, {other} stopped for other reasons",
        started.elapsed().as_secs_f64()
    );
    if created == 0 && other == 0 {
        tracing::info!("supply check: no supply was created in this range");
    }
    Ok(())
}
