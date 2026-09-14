//! Replay the pre-RSKIP126 era from genesis, recording the Unitrie root each
//! block computes, and optionally validating it against the header.
//!
//! Before block 1,591,000 the header carries an Orchid-format state root, not a
//! Unitrie one, so there is nothing to compare a computed Unitrie root against
//! directly -- and the rskj snapshot holds no Unitrie state for that era either.
//! Two things can still be done:
//!
//!   1. **Record** the computed root per block, as a baseline that later runs
//!      check against. Cheap: 32 bytes per block, compared in nanoseconds.
//!   2. **Validate** that baseline, by converting the Unitrie to the Orchid
//!      format and comparing with the header (rskj 1.x did exactly this while
//!      building its Unitrie from genesis). Expensive: a full trie walk.
//!
//! (2) is what makes (1) worth having -- an unvalidated baseline can bake in a
//! bug forever. This tool measures what (2) costs, so the interval between
//! checks can be chosen rather than guessed.
//!
//! Usage: pre_unitrie_roots <block-dir> <trie-dir> <count> [orchid-interval] [csv]

use rustock_core::Block;
use rustock_core::config::ChainConfig;
use rustock_execution::{BlockProcessor, RskHardforkConfig};
use rustock_storage::{BlockStore, RocksDbTrieStore};
use rustock_trie::{account_key, orchid_state_root, AccountState, TrieKeySlice, TrieNode, TrieStore};
use std::sync::Arc;
use std::time::Instant;

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let block_dir = args.next().expect("usage: pre_unitrie_roots <block-dir> <trie-dir> <count> [orchid-interval] [csv]");
    let trie_dir = args.next().expect("trie-dir");
    let count: u64 = args.next().expect("count").parse()?;
    let orchid_interval: u64 = args.next().map(|s| s.parse()).transpose()?.unwrap_or(0);
    let csv_path = args.next();

    let store = Arc::new(BlockStore::open_read_only(&block_dir)?);
    let trie: Arc<dyn TrieStore> = Arc::new(RocksDbTrieStore::open(&trie_dir)?);

    // Genesis state comes from the chain config's alloc, exactly as the node
    // builds it on a fresh data directory.
    let config = ChainConfig::mainnet();
    let mut root = TrieNode::empty();
    for entry in &config.genesis_alloc() {
        let key = account_key(&entry.address);
        let acct = AccountState::new(entry.nonce, entry.balance);
        root = root.put(&TrieKeySlice::from_key(&key), &acct.encode(), trie.as_ref());
    }
    root.save(trie.as_ref(), true);
    eprintln!("genesis Unitrie root: {:?}", root.compute_hash(trie.as_ref()));

    let processor = BlockProcessor::new(RskHardforkConfig::mainnet(), store.clone());
    let mut csv = String::from("block,txs,unitrie_root,orchid_ok,orchid_ms,elapsed_s\n");
    let t0 = Instant::now();
    let mut orchid_checks = 0u64;
    let mut orchid_fail = 0u64;
    let mut orchid_total_ms = 0f64;

    for n in 1..=count {
        let hash = match store.canonical_hash(n)? { Some(h) => h, None => break };
        let header = match store.header(hash)? { Some(h) => h, None => break };
        let (transactions, ommers) = match store.body(hash)? { Some(b) => b, None => break };
        let txs = transactions.len();
        let block = Block { header, transactions, ommers };

        let processed = processor.execute_block(&block, &root, trie.clone())?;
        root = processed.new_state_root;
        let unitrie_root = processed.state_root_hash;

        // The header's root is Orchid-format before #1,591,000; converting the
        // Unitrie and comparing is the only ground truth available there.
        let (mut ok, mut ms) = (String::new(), 0f64);
        if orchid_interval > 0 && n % orchid_interval == 0 {
            let t = Instant::now();
            let computed = orchid_state_root(&root, trie.as_ref());
            ms = t.elapsed().as_secs_f64() * 1000.0;
            orchid_total_ms += ms;
            orchid_checks += 1;
            let matches = computed == block.header.state_root;
            if !matches {
                orchid_fail += 1;
                eprintln!("#{n} ORCHID MISMATCH computed={computed:?} header={:?}",
                          block.header.state_root);
            }
            ok = matches.to_string();
        }
        csv.push_str(&format!("{n},{txs},{unitrie_root:?},{ok},{ms:.1},{:.1}\n",
                              t0.elapsed().as_secs_f64()));

        if n % 20000 == 0 {
            eprintln!("#{n} | {:.0} blocks/s | {orchid_checks} orchid checks, \
                       {:.0} ms mean, {orchid_fail} failed",
                      n as f64 / t0.elapsed().as_secs_f64(),
                      if orchid_checks > 0 { orchid_total_ms / orchid_checks as f64 } else { 0.0 });
        }
    }

    eprintln!("\n=== {count} blocks in {:.1}s ({:.0} blocks/s) ===",
              t0.elapsed().as_secs_f64(), count as f64 / t0.elapsed().as_secs_f64());
    eprintln!("orchid checks: {orchid_checks} ({orchid_fail} failed), mean {:.0} ms",
              if orchid_checks > 0 { orchid_total_ms / orchid_checks as f64 } else { 0.0 });
    if let Some(p) = csv_path { std::fs::write(&p, csv)?; eprintln!("CSV -> {p}"); }
    Ok(())
}
