//! Replay canonical blocks and report any that do not conserve the native
//! supply, changing nothing on disk.
//!
//! The Bridge holds the whole 21 M bitcoin, so a peg-in moves value out of it
//! rather than minting any: the sum of all balances must never grow. This
//! replays a range of blocks through the same `execute_block` the node uses and
//! reports what the supply check finds.
//!
//! **Read-only.** The trie store is wrapped so writes are discarded, and
//! nothing touches receipts, bodies or the canonical index. It is safe to run
//! against a stopped node's database; it does not need the node's log and it
//! installs no alert observer, so no mail is sent.
//!
//! Usage: check_supply <data-dir> <count> [end-block] [--trie-dir DIR]
//!   count       how many blocks back from the end to replay
//!   end-block   last block to replay (default: the executed head)
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
/// over history that would write nodes for state the node already has, so the
/// writes go nowhere and the replay stays a pure reader.
struct ReadOnly(Arc<dyn TrieStore>);

impl TrieStore for ReadOnly {
    fn get(&self, key: &[u8]) -> Option<Vec<u8>> {
        self.0.get(key)
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
    let mut i = 0;
    while i < argv.len() {
        if argv[i] == "--trie-dir" {
            trie_dir = argv.get(i + 1).cloned();
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
    let backing: Arc<dyn TrieStore> = match &trie_dir {
        Some(dir) => {
            tracing::info!("supply check: epoch trie backend at {dir}");
            Arc::new(rustock_storage::epoch_store::EpochTrieStore::open(
                dir,
                rustock_storage::epoch_store::EpochConfig::default(),
            )?)
        }
        None => Arc::new(CachedTrieStore::with_defaults(store.db().clone())),
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
