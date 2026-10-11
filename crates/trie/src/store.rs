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

    /// The highest block whose state this store may have collected, if it
    /// collects at all.
    ///
    /// Answers the question a missing node raises and cannot answer alone: was
    /// it taken deliberately, or is the store damaged? A node missing at or
    /// below this height was collected and is not recoverable here; one above
    /// it should still be present, so its absence is damage.
    ///
    /// `None` means this store never collects -- every store but the epoch one
    /// -- so any missing node is damage. Defaulting to `None` is the safe
    /// direction: it calls damage damage, rather than excusing it as
    /// collection that never happened.
    fn collected_below(&self) -> Option<u64> {
        None
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

    /// Every key held, in no particular order.
    pub fn keys(&self) -> Vec<Vec<u8>> {
        self.data.lock().unwrap().keys().cloned().collect()
    }

    /// Drops one node, so a test can produce the corruption this store exists
    /// to detect: state that was accepted but is not all there.
    pub fn remove(&self, key: &[u8]) -> bool {
        self.data.lock().unwrap().remove(key).is_some()
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

/// A read-only store with an in-memory layer of writes on top.
///
/// Replaying a block computes a new trie, and computing a trie means writing
/// its nodes. An offline diagnostic therefore cannot simply open a node's
/// database read-only and execute against it: the first `put` fails, or worse,
/// succeeds and mutates the database being examined.
///
/// The overlay resolves that. Reads fall through to the base store; writes go
/// nowhere but memory and are discarded when it is dropped. The replay sees a
/// consistent trie, the node's database is untouched, and the diagnostic can
/// run beside a live node — which is exactly when one is needed.
///
/// Writes shadow the base, so a node recomputed to a different value during
/// the replay reads back as the recomputed one.
pub struct OverlayTrieStore {
    base: std::sync::Arc<dyn TrieStore>,
    overlay: Mutex<HashMap<Vec<u8>, Vec<u8>>>,
}

impl OverlayTrieStore {
    pub fn new(base: std::sync::Arc<dyn TrieStore>) -> Self {
        Self { base, overlay: Mutex::new(HashMap::new()) }
    }

    /// Number of nodes written since the overlay was created.
    pub fn written(&self) -> usize {
        self.overlay.lock().unwrap().len()
    }
}

impl TrieStore for OverlayTrieStore {
    fn get(&self, key: &[u8]) -> Option<Vec<u8>> {
        if let Some(v) = self.overlay.lock().unwrap().get(key) {
            return Some(v.clone());
        }
        self.base.get(key)
    }

    fn put(&self, key: &[u8], value: &[u8]) {
        self.overlay.lock().unwrap().insert(key.to_vec(), value.to_vec());
    }

    /// Deliberately a no-op: the whole point is that nothing reaches the base.
    fn flush(&self) {}

    /// Inherited from the base, so a missing node is still judged against the
    /// height the real store collected below rather than excused.
    fn collected_below(&self) -> Option<u64> {
        self.base.collected_below()
    }
}

#[cfg(test)]
mod overlay_tests {
    use super::*;
    use std::sync::Arc;

    /// A base that panics on write, standing in for a store opened read-only.
    struct ReadOnlyBase(MemoryTrieStore);

    impl TrieStore for ReadOnlyBase {
        fn get(&self, key: &[u8]) -> Option<Vec<u8>> {
            self.0.get(key)
        }
        fn put(&self, _key: &[u8], _value: &[u8]) {
            panic!("write reached the read-only base store");
        }
        fn collected_below(&self) -> Option<u64> {
            Some(4000)
        }
    }

    fn base_with(entries: &[(&[u8], &[u8])]) -> Arc<ReadOnlyBase> {
        let m = MemoryTrieStore::new();
        for (k, v) in entries {
            m.put(k, v);
        }
        Arc::new(ReadOnlyBase(m))
    }

    #[test]
    fn writes_never_reach_the_base() {
        // The base panics on write, so this passing is the assertion: a
        // replay against a read-only database must not touch it.
        let overlay = OverlayTrieStore::new(base_with(&[]));
        overlay.put(b"node", b"value");
        overlay.flush();
        assert_eq!(overlay.get(b"node"), Some(b"value".to_vec()));
        assert_eq!(overlay.written(), 1);
    }

    #[test]
    fn reads_fall_through_to_the_base() {
        let overlay = OverlayTrieStore::new(base_with(&[(b"old", b"from-base")]));
        assert_eq!(overlay.get(b"old"), Some(b"from-base".to_vec()));
        assert_eq!(overlay.get(b"absent"), None);
        assert_eq!(overlay.written(), 0);
    }

    #[test]
    fn a_write_shadows_the_base() {
        // A replay that recomputes a node must read back its own version, not
        // the one on disk, or the trie it builds is a mix of two states.
        let overlay = OverlayTrieStore::new(base_with(&[(b"k", b"disk")]));
        overlay.put(b"k", b"recomputed");
        assert_eq!(overlay.get(b"k"), Some(b"recomputed".to_vec()));
    }

    #[test]
    fn the_collection_horizon_comes_from_the_base() {
        // Otherwise a node missing because the real store collected it would
        // be reported as damage by anything reading through the overlay.
        let overlay = OverlayTrieStore::new(base_with(&[]));
        assert_eq!(overlay.collected_below(), Some(4000));
    }

    #[test]
    fn bulk_reads_see_the_overlay_too() {
        // get_many/get_many_sorted default to get, which is what makes this
        // hold; the test pins it so a future override cannot bypass the
        // overlay and read a stale node from disk.
        let overlay = OverlayTrieStore::new(base_with(&[(b"a", b"base-a")]));
        overlay.put(b"b", b"over-b");
        let keys = vec![b"a".to_vec(), b"b".to_vec(), b"c".to_vec()];
        assert_eq!(
            overlay.get_many(&keys),
            vec![Some(b"base-a".to_vec()), Some(b"over-b".to_vec()), None]
        );
        assert_eq!(overlay.get_many_sorted(&keys), overlay.get_many(&keys));
    }
}
