//! A [`TrieStore`] backed by [`FlatDb`].
//!
//! The adapter is thin because the fit is exact. A trie store writes
//! `keccak(value) -> value` and nothing else — `TrieNode::save` stores a
//! node under the hash of its message, and a long value under the hash of
//! the value — which is precisely the invariant FlatDB is built on. The
//! index can therefore hold no key bytes at all, and every short-record
//! read verifies the bytes against their own address on the way out.
//!
//! What this gives up, relative to RocksDB, is everything a trie store
//! does not use: deletion, ordered iteration, range scans, compaction.
//! Space comes back by discarding a whole database, which is how the epoch
//! store already prunes.

use rustock_flatdb::{FlatDb, Options};
use rustock_trie::TrieStore;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tracing::{error, warn};

pub struct FlatTrieStore {
    db: Arc<FlatDb>,
    /// Set if any write failed.
    ///
    /// `TrieStore::put` returns nothing, so a failed write cannot reach the
    /// caller — and a replay that cannot store the nodes it computes goes
    /// on to read them back as absent, producing wrong roots rather than an
    /// error. Callers drawing conclusions from a replay must be able to ask
    /// whether this happened.
    write_failed: Arc<AtomicBool>,
}

impl FlatTrieStore {
    pub fn open(dir: impl AsRef<Path>, options: Options) -> anyhow::Result<Self> {
        let db = FlatDb::open(dir.as_ref(), options)
            .map_err(|e| anyhow::anyhow!("opening flatdb at {}: {e}", dir.as_ref().display()))?;
        Ok(Self { db: Arc::new(db), write_failed: Arc::new(AtomicBool::new(false)) })
    }

    pub fn write_failed(&self) -> bool {
        self.write_failed.load(Ordering::Relaxed)
    }

    pub fn stats(&self) -> Vec<String> {
        self.db.stats()
    }

    pub fn entries(&self) -> u64 {
        self.db.entries()
    }
}

impl TrieStore for FlatTrieStore {
    fn get(&self, key: &[u8]) -> Option<Vec<u8>> {
        self.db.get(key)
    }

    fn put(&self, key: &[u8], value: &[u8]) {
        if let Err(e) = self.db.put(key, value) {
            // Logged once. The condition is usually a full disk, which
            // fails every subsequent write too, and tens of thousands of
            // identical lines bury the one thing the reader needs to see.
            if !self.write_failed.swap(true, Ordering::Relaxed) {
                error!(
                    target: "rustock::flat_trie",
                    "flatdb put failed, so this store is now incomplete \
                     and anything computed from it is unsound: {e}"
                );
            }
        }
    }

    /// One reservation and one batch, which is what `TrieNode::save`
    /// now hands over: a block's whole dirty subtree at once.
    fn put_many(&self, entries: &[(Vec<u8>, Vec<u8>)]) {
        let rows: Vec<(&[u8], &[u8])> =
            entries.iter().map(|(k, v)| (k.as_slice(), v.as_slice())).collect();
        if let Err(e) = self.db.update_batch(rows) {
            if !self.write_failed.swap(true, Ordering::Relaxed) {
                error!(
                    target: "rustock::flat_trie",
                    "flatdb batch put failed, so this store is now incomplete \
                     and anything computed from it is unsound: {e}"
                );
            }
        }
    }

    fn flush(&self) {
        if let Err(e) = self.db.flush() {
            if !self.write_failed.swap(true, Ordering::Relaxed) {
                warn!(target: "rustock::flat_trie", "flatdb flush failed: {e}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustock_trie::{TrieKeySlice, TrieNode};

    fn options() -> Options {
        Options { expected_keys: 1024, segment_size: 1 << 20, ..Options::default() }
    }

    /// The point of the adapter: a trie built through it reads back, and
    /// reaches the same root as one built in memory.
    #[test]
    fn a_trie_round_trips_through_flatdb() {
        let dir = tempfile::tempdir().unwrap();
        let store = FlatTrieStore::open(dir.path(), options()).unwrap();

        let keys: Vec<Vec<u8>> = (0u16..400)
            .map(|i| {
                use sha3::{Digest, Keccak256};
                let mut h = Keccak256::new();
                h.update(b"flat trie");
                h.update(i.to_be_bytes());
                h.finalize()[..31].to_vec()
            })
            .collect();

        let mut root = TrieNode::empty();
        for (i, key) in keys.iter().enumerate() {
            root = root.put(&TrieKeySlice::from_key(key), &[i as u8; 40], &store);
        }
        root.save(&store, true);
        let hash = root.compute_hash(&store);
        store.flush();

        let memory = rustock_trie::MemoryTrieStore::new();
        let mut expected = TrieNode::empty();
        for (i, key) in keys.iter().enumerate() {
            expected = expected.put(&TrieKeySlice::from_key(key), &[i as u8; 40], &memory);
        }
        assert_eq!(hash, expected.compute_hash(&memory), "same trie, same root");

        // And it reads back through a fresh handle on the same directory.
        drop(store);
        let reopened = FlatTrieStore::open(dir.path(), options()).unwrap();
        let loaded = TrieNode::from_message(
            &reopened.get(hash.as_slice()).expect("the root is stored"),
            &reopened,
        );
        for (i, key) in keys.iter().enumerate() {
            assert_eq!(
                loaded.get(&TrieKeySlice::from_key(key), &reopened).as_deref(),
                Some(&[i as u8; 40][..]),
                "key {i}"
            );
        }
    }

    /// Long values live outside their node and are stored under their own
    /// hash, which is the second of the two content-addressed writes a
    /// trie store makes.
    #[test]
    fn long_values_round_trip_too() {
        let dir = tempfile::tempdir().unwrap();
        let store = FlatTrieStore::open(dir.path(), options()).unwrap();
        let keys: Vec<Vec<u8>> = (0u16..40)
            .map(|i| {
                use sha3::{Digest, Keccak256};
                let mut h = Keccak256::new();
                h.update(b"long");
                h.update(i.to_be_bytes());
                h.finalize()[..31].to_vec()
            })
            .collect();

        let mut root = TrieNode::empty();
        for (i, key) in keys.iter().enumerate() {
            root = root.put(&TrieKeySlice::from_key(key), &vec![i as u8; 2000], &store);
        }
        root.save(&store, true);
        store.flush();

        for (i, key) in keys.iter().enumerate() {
            assert_eq!(
                root.get(&TrieKeySlice::from_key(key), &store).as_deref(),
                Some(&vec![i as u8; 2000][..]),
                "key {i}"
            );
        }
    }
}
