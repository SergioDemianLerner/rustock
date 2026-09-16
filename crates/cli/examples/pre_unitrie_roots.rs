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
//! Roots go to a RocksDB keyed by block number, written in batches as the
//! replay proceeds, with a schema version and a resume cursor. Three reasons,
//! all learned the hard way from the first version, which buffered a CSV in
//! memory and wrote it once at the end:
//!
//!   - A crash lost everything. Worse, the *interesting* failure -- the
//!     #1,591,000 anchor not matching -- propagates an error out of `main`,
//!     so exactly the run whose roots you need to bisect is the run that
//!     discards them.
//!   - 1.59M rows is ~135 MB of `String` on a machine with 7.7 GB.
//!   - A baseline that later runs check against needs a schema version, or an
//!     intentional trie-encoding change reads as 1.59M failures instead of one
//!     "baseline is stale".
//!
//! Usage: pre_unitrie_roots <block-dir> <trie-dir> <roots-db> <count> [orchid-interval]

use rustock_core::Block;
use rustock_core::config::ChainConfig;
use rustock_execution::{BlockProcessor, RskHardforkConfig};
use rustock_storage::{BlockStore, RocksDbTrieStore};
use rustock_trie::{account_key, orchid_state_root, AccountState, TrieKeySlice, TrieNode, TrieStore};
use std::sync::Arc;
use std::time::Instant;
use rocksdb::{ColumnFamilyDescriptor, Options, WriteBatch, DB};

/// Bump when anything that changes a computed root changes: trie encoding, key
/// mapping, account serialisation. A baseline from an older schema is stale,
/// not wrong, and must say so rather than reporting every block as a failure.
const SCHEMA: u32 = 1;
const CF_ROOTS: &str = "roots";
const CF_META: &str = "meta";
const BATCH: u64 = 10_000;

fn open_roots_db(path: &str) -> anyhow::Result<DB> {
    let mut opts = Options::default();
    opts.create_if_missing(true);
    opts.create_missing_column_families(true);
    Ok(DB::open_cf_descriptors(
        &opts,
        path,
        vec![
            ColumnFamilyDescriptor::new(CF_ROOTS, Options::default()),
            ColumnFamilyDescriptor::new(CF_META, Options::default()),
        ],
    )?)
}

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let block_dir = args.next().expect("usage: pre_unitrie_roots <block-dir> <trie-dir> <count> [orchid-interval] [csv]");
    let trie_dir = args.next().expect("trie-dir");
    let roots_db = args.next().expect("roots-db");
    let count: u64 = args.next().expect("count").parse()?;
    let orchid_interval: u64 = args.next().map(|s| s.parse()).transpose()?.unwrap_or(0);

    let store = Arc::new(BlockStore::open_read_only(&block_dir)?);
    let trie: Arc<dyn TrieStore> = Arc::new(RocksDbTrieStore::open(&trie_dir)?);
    let db = open_roots_db(&roots_db)?;
    let cf_roots = db.cf_handle(CF_ROOTS).unwrap();
    let cf_meta = db.cf_handle(CF_META).unwrap();

    // A baseline written under a different schema cannot be compared with one
    // written now. Refuse rather than silently mixing them.
    match db.get_cf(cf_meta, b"schema")? {
        Some(v) if v.as_slice() != SCHEMA.to_be_bytes() => anyhow::bail!(
            "roots db at {roots_db} was written by schema {}, this build is {SCHEMA}; \
             delete it to rebuild the baseline",
            u32::from_be_bytes(v[..4].try_into().unwrap_or([0; 4]))
        ),
        Some(_) => {}
        None => db.put_cf(cf_meta, b"schema", SCHEMA.to_be_bytes())?,
    }

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

    // Resume where a previous run stopped. The trie is persistent, so the root
    // recorded for the last completed block is still loadable: a crashed or
    // killed run costs only the blocks since its last batch.
    let mut start = 1u64;
    if let Some(v) = db.get_cf(cf_meta, b"last_block")? {
        let last = u64::from_be_bytes(v[..8].try_into()?);
        let recorded = db.get_cf(cf_roots, last.to_be_bytes())?
            .ok_or_else(|| anyhow::anyhow!("roots db says #{last} but holds no root for it"))?;
        let data = trie.get(&recorded)
            .ok_or_else(|| anyhow::anyhow!(
                "root for #{last} is not in {trie_dir}; the trie and the roots db \
                 are from different runs"))?;
        root = TrieNode::from_message(&data, trie.as_ref());
        start = last + 1;
        eprintln!("resuming at #{start} from recorded root");
    }

    let processor = BlockProcessor::new(RskHardforkConfig::mainnet(), store.clone());
    let t0 = Instant::now();
    let mut orchid_checks = 0u64;
    let mut orchid_fail = 0u64;
    let mut orchid_total_ms = 0f64;

    let mut batch = WriteBatch::default();
    for n in start..=count {
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
        let _ = (&txs, &ok, ms);
        batch.put_cf(cf_roots, n.to_be_bytes(), unitrie_root.as_slice());
        if n % BATCH == 0 {
            batch.put_cf(cf_meta, b"last_block", n.to_be_bytes());
            db.write(std::mem::take(&mut batch))?;
        }

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
    if !batch.is_empty() {
        batch.put_cf(cf_meta, b"last_block", (count).to_be_bytes());
        db.write(batch)?;
    }
    eprintln!("roots -> {roots_db}");
    Ok(())
}
