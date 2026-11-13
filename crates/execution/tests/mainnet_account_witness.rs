//! The account modification witness, checked against real mainnet blocks.
//!
//! A consensus rule is checked against the chain here, not argued about. This
//! one cannot be checked the usual way: it *changes* state roots by design, so
//! replaying mainnet and matching the header's root would fail at the first
//! block and prove nothing. The two halves of the check are therefore split.
//!
//! **Inertness** — that the rule writes nothing until it activates — is the
//! safety property, and it is checked by the ordinary tooling: with the rule
//! unscheduled, `verify_range` over real mainnet history must still match
//! every header's state root exactly. Nothing in this file is needed for that.
//!
//! **Correctness once active** is what this file checks, and the check is
//! independent of the mechanism that produces the witness. The set of
//! accounts to examine comes from each block's own transaction list — the
//! `from` and `to` of every transaction — not from the executor's journal, so
//! the thing being tested is not also the thing supplying the answers. For
//! each such account the test reads its record out of the parent trie and the
//! post-block trie and asserts:
//!
//! - a changed `(nonce, balance)` forces `lastModifiedBlock == block.number`;
//! - `lastModifiedBlock` never decreases, and never exceeds the block;
//! - an account the block left alone keeps the witness it had.
//!
//! The converse of the first is deliberately *not* asserted, and that
//! asymmetry is the point of the whole RSKIP: an account can be modified
//! twice within one block and end where it started, and a balance comparison
//! cannot see it. Those cases are counted and reported rather than treated as
//! failures — they are the evidence the field does something balance
//! diffing cannot.
//!
//! ```sh
//! RUSTOCK_BLOCKS_DB=/var/lib/rustock \
//! RUSTOCK_TRIE_DB=/mnt/import/rustock-trie \
//! RUSTOCK_WITNESS_FROM=9000000 RUSTOCK_WITNESS_BLOCKS=300 \
//!   cargo test --release -p rustock-execution \
//!     --test mainnet_account_witness -- --ignored --nocapture
//! ```

use alloy_primitives::Address;
use rustock_execution::hardfork::{ConsensusRule, RskHardforkConfig};
use rustock_execution::processor::BlockProcessor;
use rustock_storage::BlockStore;
use rustock_trie::{account_key, AccountState, OverlayTrieStore, TrieKeySlice, TrieNode, TrieStore};
use std::collections::BTreeSet;
use std::sync::Arc;

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok()
}

/// The witness as the trie holds it, or `None` if the account is absent.
fn witness_of(
    root: &TrieNode,
    store: &dyn TrieStore,
    addr: &Address,
) -> Option<AccountState> {
    root.get(&TrieKeySlice::from_key(&account_key(addr)), store)
        .and_then(|raw| AccountState::decode(&raw).ok())
}

#[test]
#[ignore = "requires a synced block store (RUSTOCK_BLOCKS_DB) and an archival trie (RUSTOCK_TRIE_DB)"]
fn the_witness_tracks_real_mainnet_account_changes() {
    let (Some(blocks_dir), Some(trie_dir)) = (env("RUSTOCK_BLOCKS_DB"), env("RUSTOCK_TRIE_DB"))
    else {
        eprintln!("RUSTOCK_BLOCKS_DB / RUSTOCK_TRIE_DB not set; skipping");
        return;
    };
    let from: u64 = env("RUSTOCK_WITNESS_FROM")
        .and_then(|s| s.parse().ok())
        .unwrap_or(9_000_000);
    let count: u64 = env("RUSTOCK_WITNESS_BLOCKS")
        .and_then(|s| s.parse().ok())
        .unwrap_or(200);

    let blocks = Arc::new(BlockStore::open_read_only(&blocks_dir).expect("open block store"));
    let source: Arc<dyn TrieStore> = Arc::new(
        rustock_storage::RocksDbTrieStore::open_read_only(&trie_dir).expect("open archival trie"),
    );
    // Every node the replay writes goes to the overlay, so the archival store
    // is never written to and the run leaves nothing behind.
    let store: Arc<dyn TrieStore> = Arc::new(OverlayTrieStore::new(source.clone()));

    // The rule is scheduled on no network, so the replay turns it on by hand
    // at the first block of the range. Everything below `from` therefore has
    // an unset witness, which is exactly the "not modified since activation"
    // state a real activation would produce.
    let mut cfg = RskHardforkConfig::for_network(30);
    cfg = cfg.with_consensus_rule(ConsensusRule::AccountWitness, Some(from));

    let parent = blocks
        .canonical_hash(from - 1)
        .expect("parent hash")
        .expect("parent is canonical");
    let parent_header = blocks.header(parent).expect("parent header").expect("present");
    let root_bytes = source
        .get(parent_header.state_root.as_slice())
        .unwrap_or_else(|| panic!("state for #{} is not in {trie_dir}", from - 1));
    let mut root = TrieNode::from_message(&root_bytes, store.as_ref());

    let processor = BlockProcessor::new(cfg, blocks.clone());

    let mut checked_accounts = 0usize;
    let mut changed = 0usize;
    let mut netted: Vec<(u64, Address)> = Vec::new();
    let mut blocks_run = 0u64;

    for number in from..from + count {
        let Some(hash) = blocks.canonical_hash(number).expect("canonical hash") else {
            break;
        };
        let block = blocks.block(hash).expect("block").expect("present");

        // The account set, taken from the block itself rather than from
        // anything execution reports.
        let mut watched: BTreeSet<Address> = BTreeSet::new();
        for tx in &block.transactions {
            // A creation has an empty `to`; the created address is not
            // predictable from the list alone, so it is simply not watched.
            if tx.to.len() == 20 {
                watched.insert(Address::from_slice(&tx.to));
            }
            // The sender is the strongest case available: its nonce
            // necessarily advances, so it must be witnessed in this block or
            // the rule is broken.
            if let Ok(from_addr) = tx.recover_sender(30) {
                watched.insert(from_addr);
            }
        }

        let before: Vec<(Address, Option<AccountState>)> = watched
            .iter()
            .map(|a| (*a, witness_of(&root, store.as_ref(), a)))
            .collect();

        let processed = processor
            .execute_block(&block, &root, store.clone())
            .unwrap_or_else(|e| panic!("#{number} failed to execute: {e:?}"));
        let after_root = processed.new_state_root;

        for (addr, prev) in before {
            let Some(now) = witness_of(&after_root, store.as_ref(), &addr) else {
                continue; // self-destructed, or never existed
            };
            checked_accounts += 1;

            assert!(
                now.last_modified_block <= number,
                "#{number} {addr}: lastModifiedBlock {} is in the future",
                now.last_modified_block
            );

            match prev {
                Some(prev) => {
                    assert!(
                        now.last_modified_block >= prev.last_modified_block,
                        "#{number} {addr}: lastModifiedBlock went backwards, {} -> {}",
                        prev.last_modified_block,
                        now.last_modified_block
                    );
                    if prev.subject() != now.subject() {
                        changed += 1;
                        assert_eq!(
                            now.last_modified_block, number,
                            "#{number} {addr}: record changed but the witness did not follow"
                        );
                    } else if now.last_modified_block == number {
                        // Modified and back again inside one block. A balance
                        // comparison sees nothing here; the witness does.
                        netted.push((number, addr));
                    } else {
                        assert_eq!(
                            now.last_modified_block, prev.last_modified_block,
                            "#{number} {addr}: untouched account lost its witness"
                        );
                        assert_eq!(
                            now.intra_block_count, prev.intra_block_count,
                            "#{number} {addr}: untouched account lost its count"
                        );
                    }
                }
                None => {
                    // The account came into existence in this block.
                    assert_eq!(
                        now.last_modified_block, number,
                        "#{number} {addr}: a new account must be witnessed at its birth"
                    );
                    changed += 1;
                }
            }
        }

        root = after_root;
        blocks_run += 1;
    }

    assert!(blocks_run > 0, "no blocks were replayed; is the range canonical here?");
    assert!(
        checked_accounts > 0,
        "replayed {blocks_run} blocks but examined no accounts -- the range may be empty"
    );

    eprintln!(
        "replayed #{from}..#{} ({blocks_run} blocks)\n  \
         accounts examined   {checked_accounts}\n  \
         witnessed as changed {changed}\n  \
         changed and restored within one block (invisible to balance diffing) {}",
        from + blocks_run - 1,
        netted.len()
    );
    for (number, addr) in netted.iter().take(20) {
        eprintln!("    #{number}  {addr}");
    }
}
