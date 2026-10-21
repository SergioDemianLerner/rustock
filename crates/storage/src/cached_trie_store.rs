//! Write-buffered, LRU-cached trie store wrapping RocksDB.
//!
//! Improves on rskj's `DataSourceWithCache` by using RocksDB `WriteBatch`
//! as the write buffer instead of a `HashMap`:
//! - Single contiguous memory buffer (fewer allocations)
//! - Atomic flush via `db.write(batch)` (no partial writes on crash)
//! - One LRU read cache instead of rskj's dual committed/uncommitted caches
//!
//! # The lock is not held across the disk read
//!
//! It used to be, and that made this store **slower than no cache at all**
//! once blocks were executed on several threads. Parallel sublist execution
//! is bound by random reads of trie nodes; a `get` that missed the cache
//! held the one `Mutex` for the whole RocksDB lookup, so the other sublists
//! queued behind it and the concurrency the executor had just arranged was
//! given straight back.
//!
//! Measured on the 1 GB regtest fixture, 30 blocks on 4 threads
//! (`docs/parallel-execution-benchmark.md`):
//!
//! | store | wall | CPU |
//! |---|---|---|
//! | no cache at all | 73.0 s | 71% |
//! | this store, lock held across the read | 123.8 s | 45% |
//! | this store, as it is now | **54.5 s** | 76% |
//!
//! Releasing the lock means two threads that miss the same key both fetch
//! it. That is a little wasted work and never a wrong answer: a node is
//! immutable and keyed by its own hash.
//!
//! Sharding the cache into several independently locked pieces was measured
//! too and bought nothing (57.2 s against 54.5 s, overlapping spreads).
//! Once the disk read is outside the lock, what remains inside is a hash
//! lookup -- too short for four threads to collide on. The contention was
//! never that the lock was single; it was that it was held across I/O.

use lru::LruCache;
use rocksdb::{DB, WriteBatch};
use rustock_trie::TrieStore;
use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex};
use tracing::{debug, warn};

const CF_TRIE: &str = "trie_nodes";
const DEFAULT_LRU_CAPACITY: usize = 50_000;
const DEFAULT_AUTO_FLUSH_THRESHOLD: usize = 10_000;

/// How a [`CachedTrieStore`] behaves.
#[derive(Debug, Clone)]
pub struct Config {
    /// Nodes held in the read cache.
    pub lru_capacity: usize,
    /// Buffered writes that trigger a flush.
    pub auto_flush_threshold: usize,
    /// Flush without the write-ahead log.
    ///
    /// Sound for trie nodes specifically, and not in general: a node is
    /// **derivable** -- lose it and re-executing the block recomputes it --
    /// so it needs no durability of its own. What it does need is ordering:
    /// the executed-head pointer may only advance once the nodes behind it
    /// are on disk. The caller owns that; this flag only removes the WAL.
    ///
    /// Off by default because on the benchmark machine it bought about 1%:
    /// that workload is bound by reads, and its writes are ~9 MB/s against
    /// an SSD. Somewhere with a slow fsync the trade may look different,
    /// which is why the option exists at all.
    pub disable_wal: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            lru_capacity: DEFAULT_LRU_CAPACITY,
            auto_flush_threshold: DEFAULT_AUTO_FLUSH_THRESHOLD,
            disable_wal: false,
        }
    }
}

struct Inner {
    batch: WriteBatch,
    pending: HashMap<Vec<u8>, Vec<u8>>,
    cache: LruCache<Vec<u8>, Vec<u8>>,
    pending_count: usize,
    total_flushed: u64,
}

/// A trie store that buffers writes in a RocksDB `WriteBatch` and caches
/// reads in an LRU cache. Writes are only durable after `flush()` is called.
pub struct CachedTrieStore {
    db: Arc<DB>,
    inner: Mutex<Inner>,
    auto_flush_threshold: usize,
    disable_wal: bool,
}

impl CachedTrieStore {
    pub fn new(db: Arc<DB>, lru_capacity: usize, auto_flush_threshold: usize) -> Self {
        Self::with_config(
            db,
            Config { lru_capacity, auto_flush_threshold, ..Config::default() },
        )
    }

    pub fn with_defaults(db: Arc<DB>) -> Self {
        Self::with_config(db, Config::default())
    }

    pub fn with_config(db: Arc<DB>, config: Config) -> Self {
        let capacity = NonZeroUsize::new(config.lru_capacity.max(1)).expect("at least one");
        Self {
            db,
            inner: Mutex::new(Inner {
                batch: WriteBatch::default(),
                pending: HashMap::new(),
                cache: LruCache::new(capacity),
                pending_count: 0,
                total_flushed: 0,
            }),
            auto_flush_threshold: config.auto_flush_threshold.max(1),
            disable_wal: config.disable_wal,
        }
    }

    /// Rewrite every SST in the trie column family under the options the
    /// database was opened with. Used to rebuild a fixture after changing a
    /// table option that only applies to newly written files.
    pub fn compact_all(&self) {
        if let Some(cf) = self.db.cf_handle(CF_TRIE) {
            self.db.compact_range_cf(cf, None::<&[u8]>, None::<&[u8]>);
        }
    }

    fn do_flush(db: &DB, inner: &mut Inner, disable_wal: bool) {
        if inner.pending_count == 0 {
            return;
        }

        let batch = std::mem::take(&mut inner.batch);
        let count = inner.pending_count;

        let result = if disable_wal {
            let mut options = rocksdb::WriteOptions::default();
            options.disable_wal(true);
            db.write_opt(batch, &options)
        } else {
            db.write(batch)
        };
        if let Err(e) = result {
            warn!(target: "rustock::cached_trie", "WriteBatch flush failed: {e}");
            return;
        }

        inner.pending.clear();
        inner.pending_count = 0;
        inner.total_flushed += count as u64;

        debug!(
            target: "rustock::cached_trie",
            "Flushed {count} trie entries (total: {})",
            inner.total_flushed
        );
    }
}

impl TrieStore for CachedTrieStore {
    fn get(&self, key: &[u8]) -> Option<Vec<u8>> {
        {
            let mut inner = self.inner.lock().unwrap();
            if let Some(val) = inner.pending.get(key) {
                return Some(val.clone());
            }
            if let Some(val) = inner.cache.get(key) {
                return Some(val.clone());
            }
        }

        // Lock released: see the module note. Concurrent misses on the same
        // key duplicate the read, which is waste and not an error.
        let cf = self.db.cf_handle(CF_TRIE)?;
        let found = match self.db.get_cf(cf, key) {
            Ok(found) => found,
            Err(e) => {
                warn!(target: "rustock::cached_trie", "RocksDB get error: {e}");
                return None;
            }
        };

        let mut inner = self.inner.lock().unwrap();
        match found {
            Some(val) => {
                inner.cache.put(key.to_vec(), val.clone());
                Some(val)
            }
            // Absent from disk, but another thread may have written it while
            // the lock was down. Checking again costs one lookup on the path
            // that was going to return `None` anyway, and skipping it would
            // report a node as missing that the store has been given.
            None => inner.pending.get(key).cloned(),
        }
    }

    fn put(&self, key: &[u8], value: &[u8]) {
        let mut inner = self.inner.lock().unwrap();

        let cf = match self.db.cf_handle(CF_TRIE) {
            Some(cf) => cf,
            None => {
                warn!(target: "rustock::cached_trie", "trie_nodes CF not found");
                return;
            }
        };

        inner.batch.put_cf(&cf, key, value);
        inner.pending.insert(key.to_vec(), value.to_vec());
        inner.cache.put(key.to_vec(), value.to_vec());
        inner.pending_count += 1;

        if inner.pending_count >= self.auto_flush_threshold {
            debug!(
                target: "rustock::cached_trie",
                "Auto-flushing: pending count {} >= threshold {}",
                inner.pending_count, self.auto_flush_threshold
            );
            Self::do_flush(&self.db, &mut inner, self.disable_wal);
        }
    }

    fn flush(&self) {
        let mut inner = self.inner.lock().unwrap();
        Self::do_flush(&self.db, &mut inner, self.disable_wal);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rocksdb::Options;

    fn open_test_db(dir: &std::path::Path) -> Arc<DB> {
        let mut opts = Options::default();
        opts.create_if_missing(true);
        opts.create_missing_column_families(true);
        let cfs = vec![rocksdb::ColumnFamilyDescriptor::new(
            CF_TRIE,
            Options::default(),
        )];
        Arc::new(DB::open_cf_descriptors(&opts, dir, cfs).unwrap())
    }

    #[test]
    fn put_get_before_flush() {
        let dir = tempfile::tempdir().unwrap();
        let db = open_test_db(dir.path());
        let store = CachedTrieStore::with_defaults(db);

        store.put(b"key1", b"val1");
        assert_eq!(store.get(b"key1"), Some(b"val1".to_vec()));
    }

    #[test]
    fn get_miss_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let db = open_test_db(dir.path());
        let store = CachedTrieStore::with_defaults(db);

        assert_eq!(store.get(b"nonexistent"), None);
    }

    #[test]
    fn flush_persists_to_rocksdb() {
        let dir = tempfile::tempdir().unwrap();
        let db = open_test_db(dir.path());

        {
            let store = CachedTrieStore::with_defaults(db.clone());
            store.put(b"persist_key", b"persist_val");
            store.flush();
        }

        let cf = db.cf_handle(CF_TRIE).unwrap();
        let val = db.get_cf(cf, b"persist_key").unwrap();
        assert_eq!(val, Some(b"persist_val".to_vec()));
    }

    #[test]
    fn unflushed_data_not_in_rocksdb() {
        let dir = tempfile::tempdir().unwrap();
        let db = open_test_db(dir.path());

        {
            let store = CachedTrieStore::with_defaults(db.clone());
            store.put(b"temp_key", b"temp_val");
            // no flush
        }

        let cf = db.cf_handle(CF_TRIE).unwrap();
        let val = db.get_cf(cf, b"temp_key").unwrap();
        assert!(val.is_none(), "unflushed data should not be in RocksDB");
    }

    #[test]
    fn auto_flush_on_threshold() {
        let dir = tempfile::tempdir().unwrap();
        let db = open_test_db(dir.path());
        let store = CachedTrieStore::new(db.clone(), 1000, 5);

        for i in 0..5u8 {
            store.put(&[i], &[i + 100]);
        }

        let cf = db.cf_handle(CF_TRIE).unwrap();
        let val = db.get_cf(cf, [0u8]).unwrap();
        assert_eq!(val, Some(vec![100]), "auto-flush should have written to DB");
    }

    #[test]
    fn lru_cache_serves_reads() {
        let dir = tempfile::tempdir().unwrap();
        let db = open_test_db(dir.path());

        let cf = db.cf_handle(CF_TRIE).unwrap();
        db.put_cf(cf, b"cached_key", b"cached_val").unwrap();

        let store = CachedTrieStore::new(db, 10, 10_000);

        // First read populates LRU
        assert_eq!(store.get(b"cached_key"), Some(b"cached_val".to_vec()));
        // Second read should come from LRU (can't easily verify, but ensures no crash)
        assert_eq!(store.get(b"cached_key"), Some(b"cached_val".to_vec()));
    }

    #[test]
    fn multiple_puts_same_key_uses_latest() {
        let dir = tempfile::tempdir().unwrap();
        let db = open_test_db(dir.path());
        let store = CachedTrieStore::with_defaults(db);

        store.put(b"key", b"first");
        store.put(b"key", b"second");
        store.put(b"key", b"third");

        assert_eq!(store.get(b"key"), Some(b"third".to_vec()));
    }

    #[test]
    fn flush_then_read_from_lru() {
        let dir = tempfile::tempdir().unwrap();
        let db = open_test_db(dir.path());
        let store = CachedTrieStore::with_defaults(db);

        store.put(b"k", b"v");
        store.flush();

        // After flush, pending is cleared but LRU still has the entry
        assert_eq!(store.get(b"k"), Some(b"v".to_vec()));
    }

    #[test]
    fn trie_save_and_reload_through_cache() {
        use rustock_trie::{TrieNode, TrieKeySlice, AccountState, account_key};
        use alloy_primitives::{Address, U256};

        let dir = tempfile::tempdir().unwrap();
        let db = open_test_db(dir.path());
        let store = CachedTrieStore::with_defaults(db);

        let addr = Address::repeat_byte(0xBB);
        let acct = AccountState::new(U256::from(7), U256::from(42_000));

        let root = TrieNode::empty();
        let key_bytes = account_key(&addr);
        let key = TrieKeySlice::from_key(&key_bytes);
        let root = root.put(&key, &acct.encode(), &store);

        let mut root = root;
        let hash = root.compute_hash(&store);
        root.save(&store, true);
        store.flush();

        // Reload from store using root hash
        let data = store.get(hash.as_slice()).expect("root should be persisted");
        let reloaded = TrieNode::from_message(&data, &store);

        let val = reloaded.get(&key, &store).unwrap();
        let decoded = AccountState::decode(&val).unwrap();
        assert_eq!(decoded.nonce, U256::from(7));
        assert_eq!(decoded.balance, U256::from(42_000));
        assert_eq!(reloaded.compute_hash(&store), hash);
    }

    /// Every departure from the original shape has to round-trip the same
    /// way: write a lot of keys, read them all back.
    #[test]
    fn every_configuration_round_trips() {
        for config in configurations() {
            let dir = tempfile::tempdir().unwrap();
            let db = open_test_db(dir.path());
            let store = CachedTrieStore::with_config(db, config.clone());

            let keys: Vec<Vec<u8>> = (0u16..600).map(key_of).collect();
            for (i, key) in keys.iter().enumerate() {
                store.put(key, format!("value-{i}").as_bytes());
            }
            for (i, key) in keys.iter().enumerate() {
                assert_eq!(
                    store.get(key).as_deref(),
                    Some(format!("value-{i}").as_bytes()),
                    "before flush, {config:?}"
                );
            }
            store.flush();
            for (i, key) in keys.iter().enumerate() {
                assert_eq!(
                    store.get(key).as_deref(),
                    Some(format!("value-{i}").as_bytes()),
                    "after flush, {config:?}"
                );
            }
        }
    }

    /// Unlocked reads drop the lock across the RocksDB lookup, so a write
    /// can land in `pending` while a reader is out on disk. The reader must
    /// still find it: returning `None` for a node the store holds reads as
    /// a missing node, which produces a wrong root rather than an error.
    #[test]
    fn a_read_that_reached_the_disk_still_sees_an_unflushed_write() {
        let dir = tempfile::tempdir().unwrap();
        let db = open_test_db(dir.path());
        let store = CachedTrieStore::with_config(db, Config::default());

        store.put(b"written_never_flushed", b"v");
        assert_eq!(store.get(b"written_never_flushed"), Some(b"v".to_vec()));
        assert_eq!(store.get(b"never_written"), None);
    }

    /// Writing without the WAL is still writing: after a flush the bytes
    /// are readable through a different handle on the same database.
    #[test]
    fn a_wal_less_flush_still_persists() {
        let dir = tempfile::tempdir().unwrap();
        let db = open_test_db(dir.path());
        {
            let store = CachedTrieStore::with_config(
                db.clone(),
                Config { disable_wal: true, ..Config::default() },
            );
            store.put(b"nowal", b"value");
            store.flush();
        }
        let cf = db.cf_handle(CF_TRIE).unwrap();
        assert_eq!(db.get_cf(cf, b"nowal").unwrap(), Some(b"value".to_vec()));
    }

    /// The reason the lock was opened up in the first place: several threads
    /// reading at once must each get their own key's value, not another's.
    #[test]
    fn concurrent_readers_each_get_their_own_value() {
        let dir = tempfile::tempdir().unwrap();
        let db = open_test_db(dir.path());
        let store = Arc::new(CachedTrieStore::with_config(
            db,
            Config {
                lru_capacity: 64, // small, so reads really do reach RocksDB
                ..Config::default()
            },
        ));
        let keys: Vec<Vec<u8>> = (0u16..400).map(key_of).collect();
        for (i, key) in keys.iter().enumerate() {
            store.put(key, format!("v{i}").as_bytes());
        }
        store.flush();

        std::thread::scope(|scope| {
            for _ in 0..4 {
                let store = store.clone();
                let keys = &keys;
                scope.spawn(move || {
                    for _ in 0..5 {
                        for (i, key) in keys.iter().enumerate() {
                            assert_eq!(
                                store.get(key).as_deref(),
                                Some(format!("v{i}").as_bytes())
                            );
                        }
                    }
                });
            }
        });
    }

    fn configurations() -> Vec<Config> {
        vec![
            Config::default(),
            Config { disable_wal: true, ..Config::default() },
            // A cache too small to hold the keys, so reads really reach
            // RocksDB and take the unlocked path.
            Config { lru_capacity: 32, auto_flush_threshold: 16, disable_wal: false },
            Config { lru_capacity: 32, auto_flush_threshold: 16, disable_wal: true },
        ]
    }

    /// A hash-shaped key, because `shard_of` reads the first byte and
    /// sequential keys would all land in one shard.
    fn key_of(i: u16) -> Vec<u8> {
        use sha3::{Digest, Keccak256};
        let mut h = Keccak256::new();
        h.update(b"cache test");
        h.update(i.to_be_bytes());
        h.finalize()[..31].to_vec()
    }
}
