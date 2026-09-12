/// Trie persistence layer.
///
/// The store maps byte keys (typically Keccak-256 hashes) to byte values.
/// Both trie nodes and long values (>32 bytes) share the same key space,
/// matching rskj's TrieStoreImpl behavior.
use std::collections::HashMap;
use std::sync::Mutex;

pub trait TrieStore: Send + Sync {
    fn get(&self, key: &[u8]) -> Option<Vec<u8>>;
    fn put(&self, key: &[u8], value: &[u8]);
    /// Durably flush any buffered writes to the underlying storage.
    /// Default implementation is a no-op (suitable for stores that write synchronously).
    fn flush(&self) {}

    /// Reads many independent keys, concurrently where the store can.
    ///
    /// The point is queue depth. A trie traversal issues *dependent* reads --
    /// each address known only once the previous read returns -- so the device
    /// sees one outstanding request at a time, which is the worst case for any
    /// SSD and especially for a network-attached one. Keys discovered at the
    /// same level of a traversal are independent of one another and can be in
    /// flight together.
    ///
    /// The returned vector has one entry per key, positionally, with `None`
    /// where the key is absent.
    ///
    /// The default implementation reads them one at a time, so a store with no
    /// concurrency gains nothing but stays correct.
    fn get_many(&self, keys: &[Vec<u8>]) -> Vec<Option<Vec<u8>>> {
        keys.iter().map(|k| self.get(k)).collect()
    }

    /// Reads many keys that are already in ascending order, in one forward pass.
    ///
    /// Exists because `get` cannot express what a bulk reader knows: that the
    /// keys are sorted, and that the backing store may therefore serve them by
    /// moving a cursor forward instead of performing an independent lookup for
    /// each. For an LSM store that is the difference between one descent per
    /// key -- binary search through the index, bloom filter, block fetch -- and
    /// a walk that stays inside the blocks it has already decompressed.
    ///
    /// `keys` **must** be sorted ascending; the default implementation does not
    /// depend on it, but stores that use a cursor will return wrong results if
    /// it is violated.
    ///
    /// The returned vector has one entry per key, positionally, with `None`
    /// where the key is absent.
    ///
    /// The default implementation simply calls `get` for each key, so a store
    /// with no cursor gains nothing but stays correct.
    fn get_many_sorted(&self, keys: &[Vec<u8>]) -> Vec<Option<Vec<u8>>> {
        keys.iter().map(|k| self.get(k)).collect()
    }
}

/// In-memory store for testing.
pub struct MemoryTrieStore {
    data: Mutex<HashMap<Vec<u8>, Vec<u8>>>,
}

impl MemoryTrieStore {
    pub fn new() -> Self {
        Self { data: Mutex::new(HashMap::new()) }
    }

    pub fn len(&self) -> usize {
        self.data.lock().unwrap().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl Default for MemoryTrieStore {
    fn default() -> Self {
        Self::new()
    }
}

impl TrieStore for MemoryTrieStore {
    fn get(&self, key: &[u8]) -> Option<Vec<u8>> {
        self.data.lock().unwrap().get(key).cloned()
    }

    fn put(&self, key: &[u8], value: &[u8]) {
        self.data.lock().unwrap().insert(key.to_vec(), value.to_vec());
    }
}
