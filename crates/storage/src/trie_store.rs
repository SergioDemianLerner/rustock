/// RocksDB-backed implementation of the TrieStore trait.
///
/// Persists trie nodes and long values to a dedicated column family,
/// matching rskj's TrieStoreImpl behavior where both node data and
/// externalized values share the same key space (keyed by keccak256 hash).
use rocksdb::{DB, Options, ColumnFamilyDescriptor};
use rustock_trie::TrieStore;
use std::path::Path;
use std::sync::Arc;
use tracing::warn;

const CF_TRIE: &str = "trie_nodes";

/// Persistent trie store backed by RocksDB.
/// Threads used to service one `get_many` batch.
///
/// This is a queue-depth knob, not a CPU one: the threads spend their time
/// blocked on the device, so the useful value tracks how many concurrent
/// requests the storage can serve rather than how many cores exist. 16 keeps a
/// network-attached SSD busy without oversubscribing RocksDB's block cache
/// locks.
const READ_THREADS: usize = 16;

pub struct RocksDbTrieStore {
    db: Arc<DB>,
}

impl RocksDbTrieStore {
    /// Open or create a RocksDB-backed trie store at the given path.
    pub fn open<P: AsRef<Path>>(path: P) -> anyhow::Result<Self> {
        let mut opts = Options::default();
        opts.create_if_missing(true);
        opts.create_missing_column_families(true);

        let cfs = vec![
            ColumnFamilyDescriptor::new(CF_TRIE, Options::default()),
        ];

        let db = DB::open_cf_descriptors(&opts, path, cfs)
            .map_err(|e| anyhow::anyhow!("Failed to open trie RocksDB: {e}"))?;
        Ok(Self { db: Arc::new(db) })
    }

    /// Open a trie store read-only, without taking the directory lock.
    ///
    /// RocksDB allows one writer per directory and that writer holds an
    /// exclusive lock, so several readers of one archival trie -- parallel
    /// block replay across disjoint ranges, say -- cannot each `open` it. A
    /// read-only handle takes no lock, so any number can coexist, alongside a
    /// live writer if there is one. It sees the manifest as it stands and not
    /// the writer's memtables.
    pub fn open_read_only<P: AsRef<Path>>(path: P) -> anyhow::Result<Self> {
        let mut opts = Options::default();
        opts.create_if_missing(false);
        // Bound this handle's memory, because these exist to be opened many at
        // once -- parallel replay over disjoint ranges -- and each one
        // otherwise pins an index and filter block per SST in an unbounded
        // table cache. Against a store with a couple of thousand files that is
        // hundreds of megabytes per handle, and on a small machine it evicts
        // the page cache the readers depend on: measured, eight readers of one
        // warm range fell from 46 blocks/s to 12 and began hitting disk again.
        //
        // Capping `max_open_files` fixes the memory but costs far more than it
        // saves -- at 128 against 2,002 SSTs the file churn took a warm reader
        // from 27 blocks/s to 5.9. Put the index and filter blocks in a bounded
        // block cache instead, which evicts rather than thrashing handles.
        let mut block = rocksdb::BlockBasedOptions::default();
        block.set_block_cache(&rocksdb::Cache::new_lru_cache(64 * 1024 * 1024));
        block.set_cache_index_and_filter_blocks(true);
        block.set_pin_l0_filter_and_index_blocks_in_cache(true);
        opts.set_block_based_table_factory(&block);
        // `false`: tolerate a live writer's WAL files being present.
        let db = DB::open_cf_for_read_only(&opts, path, vec![CF_TRIE], false)
            .map_err(|e| anyhow::anyhow!("Failed to open trie RocksDB read-only: {e}"))?;
        Ok(Self { db: Arc::new(db) })
    }

    /// Open using an existing RocksDB instance that has a `trie_nodes` CF.
    pub fn from_db(db: Arc<DB>) -> Self {
        Self { db }
    }
}

impl TrieStore for RocksDbTrieStore {
    fn get(&self, key: &[u8]) -> Option<Vec<u8>> {
        let cf = self.db.cf_handle(CF_TRIE)?;
        match self.db.get_cf(cf, key) {
            Ok(val) => val,
            Err(e) => {
                warn!(target: "rustock::trie_store", "RocksDB get error: {e}");
                None
            }
        }
    }

    fn put(&self, key: &[u8], value: &[u8]) {
        let cf = match self.db.cf_handle(CF_TRIE) {
            Some(cf) => cf,
            None => {
                warn!(target: "rustock::trie_store", "trie_nodes CF not found");
                return;
            }
        };
        if let Err(e) = self.db.put_cf(cf, key, value) {
            warn!(target: "rustock::trie_store", "RocksDB put error: {e}");
        }
    }

    /// Issues independent reads concurrently, so the device sees more than one
    /// outstanding request.
    ///
    /// Two measurements on this database shaped the implementation, and both
    /// pointed away from the obvious choices:
    ///
    /// 1. **Point lookups beat iteration.** The live trie is 0.87% of stored
    ///    nodes, so consecutive keys -- even sorted -- are ~115 entries apart
    ///    and share neither a data block nor usually a file. A `get` consults
    ///    each file's bloom filter and skips the ones that cannot hold the key;
    ///    a cursor must position in all 772 of them. Sorted-cursor reads
    ///    measured 479 nodes/s against 2,658 for plain `get`.
    ///
    /// 2. **`multi_get_cf` does not overlap I/O.** It reuses one pinned
    ///    superversion, but the lookups still run one after another inside
    ///    RocksDB, which measured 517 nodes/s -- no better than serial.
    ///
    /// So the batch is split across a small pool of threads, each doing
    /// ordinary point lookups. `DB` is `Sync`, so the threads share it without
    /// locking, and the queue depth the device sees equals the pool size.
    /// Results are written back positionally, so the caller still gets one
    /// entry per key in the order it asked.
    fn get_many(&self, keys: &[Vec<u8>]) -> Vec<Option<Vec<u8>>> {
        if keys.len() < READ_THREADS * 2 {
            return keys.iter().map(|k| self.get(k)).collect();
        }
        let mut out: Vec<Option<Vec<u8>>> = vec![None; keys.len()];
        let chunk = keys.len().div_ceil(READ_THREADS);
        std::thread::scope(|scope| {
            for (key_chunk, out_chunk) in keys.chunks(chunk).zip(out.chunks_mut(chunk)) {
                scope.spawn(|| {
                    for (key, slot) in key_chunk.iter().zip(out_chunk.iter_mut()) {
                        *slot = self.get(key);
                    }
                });
            }
        });
        out
    }

    /// Serves sorted keys with one cursor moving forward through the column
    /// family, instead of an independent lookup per key.
    ///
    /// A point lookup costs a fresh descent every time: consult the table
    /// index, test the bloom filter, fetch and decompress the data block. Keys
    /// here are trie node hashes, so successive keys in an unsorted stream land
    /// in unrelated blocks and none of that work is reusable.
    ///
    /// Given the keys in ascending order, a single iterator seeks forward
    /// through them. Keys that fall in the same data block are answered from
    /// the block already decompressed, and the iterator never rewinds. This is
    /// what `docs/trie-gc-design.md` §10.2 means by an ordered walk the storage
    /// engine can serve -- sorting the keys alone is not enough, the reads have
    /// to go through one cursor to benefit.
    fn get_many_sorted(&self, keys: &[Vec<u8>]) -> Vec<Option<Vec<u8>>> {
        let mut out = Vec::with_capacity(keys.len());
        let cf = match self.db.cf_handle(CF_TRIE) {
            Some(cf) => cf,
            None => {
                warn!(target: "rustock::trie_store", "trie_nodes CF not found");
                return vec![None; keys.len()];
            }
        };

        let mut iter = self.db.raw_iterator_cf(cf);
        for key in keys {
            // seek() moves forward from the current position when the target is
            // ahead of it, which is always true for ascending keys.
            iter.seek(key);
            match iter.key() {
                Some(k) if k == key.as_slice() => out.push(iter.value().map(|v| v.to_vec())),
                _ => out.push(None),
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustock_trie::{TrieNode, TrieKeySlice, TrieStore, AccountState, account_key};
    use alloy_primitives::{Address, U256};

    #[test]
    fn test_basic_put_get() {
        let dir = tempfile::tempdir().unwrap();
        let store = RocksDbTrieStore::open(dir.path()).unwrap();

        store.put(b"key1", b"value1");
        store.put(b"key2", b"value2");

        assert_eq!(store.get(b"key1"), Some(b"value1".to_vec()));
        assert_eq!(store.get(b"key2"), Some(b"value2".to_vec()));
        assert_eq!(store.get(b"key3"), None);
    }

    #[test]
    fn test_overwrite() {
        let dir = tempfile::tempdir().unwrap();
        let store = RocksDbTrieStore::open(dir.path()).unwrap();

        store.put(b"key", b"old");
        assert_eq!(store.get(b"key"), Some(b"old".to_vec()));

        store.put(b"key", b"new");
        assert_eq!(store.get(b"key"), Some(b"new".to_vec()));
    }

    #[test]
    fn test_persistence_across_reopen() {
        let dir = tempfile::tempdir().unwrap();

        {
            let store = RocksDbTrieStore::open(dir.path()).unwrap();
            store.put(b"persistent_key", b"persistent_value");
        }

        {
            let store = RocksDbTrieStore::open(dir.path()).unwrap();
            assert_eq!(
                store.get(b"persistent_key"),
                Some(b"persistent_value".to_vec()),
                "value should survive close and reopen"
            );
        }
    }

    #[test]
    fn test_trie_operations_with_rocksdb_store() {
        let dir = tempfile::tempdir().unwrap();
        let store = RocksDbTrieStore::open(dir.path()).unwrap();

        let root = TrieNode::empty();
        let addr = Address::repeat_byte(0xAA);
        let key_bytes = account_key(&addr);
        let key = TrieKeySlice::from_key(&key_bytes);

        let acct = AccountState::new(U256::from(42), U256::from(1_000_000));
        let root = root.put(&key, &acct.encode(), &store);

        let retrieved = root.get(&key, &store).unwrap();
        let decoded = AccountState::decode(&retrieved).unwrap();
        assert_eq!(decoded.nonce, U256::from(42));
        assert_eq!(decoded.balance, U256::from(1_000_000));
    }

    #[test]
    fn test_trie_save_and_reload() {
        let dir = tempfile::tempdir().unwrap();
        let store = RocksDbTrieStore::open(dir.path()).unwrap();

        let addr1 = Address::repeat_byte(0x11);
        let addr2 = Address::repeat_byte(0x22);
        let acct1 = AccountState::new(U256::from(1), U256::from(100));
        let acct2 = AccountState::new(U256::from(2), U256::from(200));

        let root = TrieNode::empty();
        let key1 = TrieKeySlice::from_key(&account_key(&addr1));
        let key2 = TrieKeySlice::from_key(&account_key(&addr2));
        let root = root.put(&key1, &acct1.encode(), &store);
        let root = root.put(&key2, &acct2.encode(), &store);

        let mut root = root;
        root.save(&store, true);
        let root_hash = root.compute_hash(&store);

        // Reload from store using the root hash
        let data = store.get(root_hash.as_slice())
            .expect("root node should be persisted after save");
        let reloaded = TrieNode::from_message(&data, &store);

        let v1 = reloaded.get(&key1, &store).unwrap();
        let d1 = AccountState::decode(&v1).unwrap();
        assert_eq!(d1.nonce, U256::from(1));
        assert_eq!(d1.balance, U256::from(100));

        let v2 = reloaded.get(&key2, &store).unwrap();
        let d2 = AccountState::decode(&v2).unwrap();
        assert_eq!(d2.nonce, U256::from(2));
        assert_eq!(d2.balance, U256::from(200));

        assert_eq!(
            reloaded.compute_hash(&store),
            root_hash,
            "reloaded trie should have the same root hash"
        );
    }

    #[test]
    fn test_empty_value_key() {
        let dir = tempfile::tempdir().unwrap();
        let store = RocksDbTrieStore::open(dir.path()).unwrap();

        store.put(b"", b"empty_key_value");
        assert_eq!(store.get(b""), Some(b"empty_key_value".to_vec()));

        store.put(b"key", b"");
        assert_eq!(store.get(b"key"), Some(b"".to_vec()));
    }
}
