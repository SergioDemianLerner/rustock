//! Build a synthetic world state of a chosen size, so that execution
//! benchmarks start from a trie like a real one.
//!
//! A benchmark that starts from an empty trie measures the wrong thing: the
//! first blocks run against a handful of nodes that are all in cache, the
//! rate starts absurdly high and decays as the trie grows, and the average
//! over the run is an artifact of the ramp. What is wanted instead is a
//! steady state -- the same rate at block 100 and at block 3,000 -- which
//! means starting from a trie whose working set is already larger than
//! anything the benchmark will add.
//!
//! Mainnet's **live** state, per `docs/trie-scan-baseline.md`, is about
//! 600,000 accounts and 4.6 million storage cells: roughly 10.8 million trie
//! nodes and 0.7–0.9 GB. (The 131 GB archival trie is history, not live
//! state.) This builds a trie of comparable size out of accounts alone,
//! which is the cheapest way to get there and leaves the shape of the
//! traversal about right: random 31-byte keys, so the tree is balanced and
//! reads scatter.
//!
//! The accounts have no private keys and are not meant to. They exist to
//! make the trie big and the reads cold; the benchmark's own senders are
//! generated separately and are the only ones that need to sign anything.
//!
//! ```text
//! synthetic_state build <dir> --accounts N [--seed S] [--batch N]
//! synthetic_state measure <dir>
//! ```
//!
//! Build in batches: the trie is saved and reloaded every `--batch`
//! insertions, so the in-memory part stays the path being written rather
//! than the whole tree. Without that, five million accounts do not fit in
//! this machine's RAM.

use anyhow::{Context, Result};
use alloy_primitives::U256;
use rustock_execution::mining::workload::filler_address;
use rustock_trie::{AccountState, TrieStore};
use std::time::Instant;

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let command = args.next().unwrap_or_default();
    let dir = args.next().context("usage: synthetic_state <build|measure> <dir> [options]")?;

    match command.as_str() {
        "build" => {
            let mut accounts = 1_000_000u64;
            let mut seed = 1u64;
            let mut batch = 20_000u64;
            while let Some(flag) = args.next() {
                let value = args.next().with_context(|| format!("{flag} needs a value"))?;
                match flag.as_str() {
                    "--accounts" => accounts = parse_count(&value)?,
                    "--seed" => seed = value.parse()?,
                    "--batch" => batch = parse_count(&value)?,
                    other => anyhow::bail!("unknown option {other}"),
                }
            }
            build(&dir, accounts, seed, batch)
        }
        "measure" => {
            let bytes = directory_bytes(&dir)?;
            println!("{dir}: {}", human(bytes));
            Ok(())
        }
        other => anyhow::bail!("unknown command {other:?}; expected build or measure"),
    }
}

/// Accept `5m`, `500k` and plain digits: the counts here span four orders of
/// magnitude and a wrong zero is an hour of waiting.
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

fn build(dir: &str, accounts: u64, seed: u64, _batch: u64) -> Result<()> {
    if std::path::Path::new(dir).exists() {
        anyhow::bail!(
            "{dir} already exists. Building into an existing trie would mix two states; \
             remove it or choose another path."
        );
    }
    std::fs::create_dir_all(dir)?;

    println!("building {accounts} accounts into {dir} (seed {seed})");
    let started = Instant::now();

    // Keys first, in one flat buffer. Thirty-one bytes each, so five
    // million is about 155 MB -- a Vec<Vec<u8>> of the same would be three
    // times that in allocator overhead alone.
    print!("  generating keys... ");
    use std::io::Write;
    std::io::stdout().flush().ok();
    let mut flat = vec![0u8; accounts as usize * KEY_LEN];
    for i in 0..accounts {
        let address = filler_address(seed, i);
        let key = rustock_trie::account_key(&address);
        debug_assert_eq!(key.len(), KEY_LEN);
        flat[i as usize * KEY_LEN..(i as usize + 1) * KEY_LEN].copy_from_slice(&key);
    }
    println!("{:.0}s", started.elapsed().as_secs_f64());

    // Sorted, because that is what makes each subtree a contiguous range
    // and lets the build close one and forget it.
    print!("  sorting... ");
    std::io::stdout().flush().ok();
    let sorting = Instant::now();
    let mut keys: Vec<&[u8]> = flat.chunks_exact(KEY_LEN).collect();
    keys.sort_unstable();
    keys.dedup();
    println!("{:.0}s ({} distinct)", sorting.elapsed().as_secs_f64(), keys.len());

    print!("  building... ");
    std::io::stdout().flush().ok();
    let building = Instant::now();
    let store = BulkStore::open(dir)?;
    let root = rustock_trie::bulk::build_sorted(&keys, &account_value, &store);
    store.flush();
    drop(store);
    println!("{:.0}s", building.elapsed().as_secs_f64());

    let bytes = directory_bytes(dir)?;
    println!();
    println!("root        {root}");
    println!("accounts    {}", keys.len());
    println!("on disk     {}", human(bytes));
    println!("per account {:.1} bytes", bytes as f64 / keys.len() as f64);
    println!("took        {:.1}s", started.elapsed().as_secs_f64());
    std::fs::write(format!("{dir}/ROOT"), format!("{root}\n"))?;
    println!();
    println!("the root is also in {dir}/ROOT");
    Ok(())
}

/// An account key: domain prefix, ten secure bytes, twenty address bytes.
const KEY_LEN: usize = 31;

/// A filler account's state, as a function of its key.
///
/// A function of the key rather than of an index so the build needs no
/// permutation bookkeeping after the sort. The values differ from each
/// other, which matters: identical leaves would deduplicate into one node
/// and the trie would be smaller than the account count claims.
fn account_value(key: &[u8]) -> Vec<u8> {
    let tail = u64::from_be_bytes(key[23..31].try_into().expect("31-byte key"));
    AccountState::new(U256::from(tail % 7), U256::from(1_000_000_000u64 + tail % 1_000_000_007))
        .encode()
}

/// A trie store for building a fixture, and for nothing else.
///
/// Durability is deliberately off: no write-ahead log, no fsync, writes
/// batched by the thousand. A crash half way through leaves a directory to
/// delete and rebuild, which is the right trade for a file whose only
/// purpose is to be read by a benchmark -- and the wrong trade for anything
/// else, which is why this lives in an example rather than in the storage
/// crate where a node could reach it.
struct BulkStore {
    db: rocksdb::DB,
    pending: std::sync::Mutex<(rocksdb::WriteBatch, usize)>,
}

/// Nodes buffered before a batch goes to RocksDB. Writing node by node was
/// half of why the first attempt managed three hundred accounts a second.
const WRITE_BATCH_NODES: usize = 20_000;

impl BulkStore {
    fn open(dir: &str) -> Result<Self> {
        let mut opts = rocksdb::Options::default();
        opts.create_if_missing(true);
        opts.create_missing_column_families(true);
        opts.set_use_fsync(false);
        // Big memtables and plenty of them: this writes once, sequentially,
        // and never reads back what it just wrote.
        opts.set_write_buffer_size(64 * 1024 * 1024);
        opts.set_max_write_buffer_number(2);
        opts.increase_parallelism(4);
        let cfs = vec![rocksdb::ColumnFamilyDescriptor::new(
            CF_TRIE,
            rocksdb::Options::default(),
        )];
        Ok(Self {
            db: rocksdb::DB::open_cf_descriptors(&opts, dir, cfs)?,
            pending: std::sync::Mutex::new((rocksdb::WriteBatch::default(), 0)),
        })
    }

    fn commit(&self, batch: rocksdb::WriteBatch) {
        let mut options = rocksdb::WriteOptions::default();
        options.disable_wal(true);
        if let Err(e) = self.db.write_opt(batch, &options) {
            panic!("writing the fixture failed, so it is incomplete: {e}");
        }
    }
}

const CF_TRIE: &str = "trie_nodes";

impl TrieStore for BulkStore {
    fn get(&self, key: &[u8]) -> Option<Vec<u8>> {
        // Only reached for a node already written, which during a bottom-up
        // build means one still in the pending batch. Flushing first is
        // simpler than keeping a shadow map and is rare enough not to show.
        self.flush();
        let cf = self.db.cf_handle(CF_TRIE)?;
        self.db.get_cf(cf, key).ok().flatten()
    }

    fn put(&self, key: &[u8], value: &[u8]) {
        let mut guard = self.pending.lock().expect("fixture store");
        let Some(cf) = self.db.cf_handle(CF_TRIE) else {
            panic!("the trie column family is missing");
        };
        guard.0.put_cf(cf, key, value);
        guard.1 += 1;
        if guard.1 >= WRITE_BATCH_NODES {
            let batch = std::mem::take(&mut guard.0);
            guard.1 = 0;
            drop(guard);
            self.commit(batch);
        }
    }

    fn flush(&self) {
        let mut guard = self.pending.lock().expect("fixture store");
        if guard.1 == 0 {
            return;
        }
        let batch = std::mem::take(&mut guard.0);
        guard.1 = 0;
        drop(guard);
        self.commit(batch);
    }
}

fn directory_bytes(dir: &str) -> Result<u64> {
    let mut total = 0u64;
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let meta = entry.metadata()?;
        if meta.is_file() {
            total += meta.len();
        } else if meta.is_dir() {
            total += directory_bytes(entry.path().to_str().unwrap_or_default())?;
        }
    }
    Ok(total)
}

fn human(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}
