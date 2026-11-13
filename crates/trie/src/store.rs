/// Trie persistence layer.
///
/// The store maps byte keys (typically Keccak-256 hashes) to byte values.
/// Both trie nodes and long values (>32 bytes) share the same key space,
/// matching rskj's TrieStoreImpl behavior.
use std::collections::HashMap;
use std::sync::Mutex;

/// The embedding bound before RSKIP-TBD raises it.
pub const LEGACY_MAX_EMBEDDED_NODE_SIZE: usize = 44;

/// The embedding bound from RSKIP-TBD onward: 44 plus the largest growth the
/// witness fields can inflict on an account record -- one byte for a
/// `stateFlags` that must now be emitted even when zero, five for a
/// `lastModifiedBlock` once block numbers exceed 16,777,215, and two for an
/// `intraBlockCount` that reaches three digits.
pub const WITNESS_MAX_EMBEDDED_NODE_SIZE: usize = 52;

pub trait TrieStore: Send + Sync {
    fn get(&self, key: &[u8]) -> Option<Vec<u8>>;
    fn put(&self, key: &[u8], value: &[u8]);

    /// The largest node that may be embedded in its parent rather than
    /// stored under its own hash.
    ///
    /// This lives on the store because the store is the one thing already
    /// threaded through every call that decides embedding -- `to_message`,
    /// `message_length`, `collect_for_save` -- so putting it here raises the
    /// bound without touching a single call site.
    ///
    /// It is a per-height value, not a constant, because RSKIP-TBD raises it
    /// from 44 to 52 so that the two witness fields added to the account
    /// record do not evict account leaves from embedding. Measured on
    /// mainnet, 92% of embeddable account nodes sit in the 40-44 byte band
    /// and would otherwise be pushed out, at roughly 68 bytes and one extra
    /// trie node each.
    ///
    /// The raise needs no migration. A child's embedded status is recorded
    /// in its parent's flags byte and is authoritative on read, so a node
    /// written under either bound stays readable; the size test runs once
    /// per child, when that child is written, and an unmodified child is
    /// carried through by reference rather than re-measured. The new bound
    /// therefore reaches a node only when that node is next rewritten --
    /// the same laziness the witness fields have, so both halves of the
    /// rule land on an account at the same moment.
    fn max_embedded_node_size(&self) -> usize {
        LEGACY_MAX_EMBEDDED_NODE_SIZE
    }

    /// Write many entries at once.
    ///
    /// Saving a trie produces every node of a dirty subtree in one go --
    /// a block rewrites some 6,900 of them -- and handing them over one
    /// at a time forces a store to pay its per-write cost 6,900 times.
    /// For an append-only store that cost includes reserving space in a
    /// shared frontier, which is the one thing that has to be serialised;
    /// a batch reserves once.
    ///
    /// The default writes them one at a time, so a store with nothing to
    /// add stays correct.
    fn put_many(&self, entries: &[(Vec<u8>, Vec<u8>)]) {
        for (key, value) in entries {
            self.put(key, value);
        }
    }
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
    /// In-memory writes, used when no store was supplied to hold them.
    overlay: Mutex<HashMap<Vec<u8>, Vec<u8>>>,
    /// Somewhere other than memory to put them, for runs long enough that
    /// memory is the wrong place.
    spill: Option<std::sync::Arc<dyn TrieStore>>,
    written: std::sync::atomic::AtomicUsize,
}

impl OverlayTrieStore {
    /// Keeps writes in memory. Right for a block or two; a long replay should
    /// use [`Self::with_spill`] instead.
    pub fn new(base: std::sync::Arc<dyn TrieStore>) -> Self {
        Self {
            base,
            overlay: Mutex::new(HashMap::new()),
            spill: None,
            written: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    /// Sends writes to `spill` rather than memory.
    ///
    /// Replaying fifty thousand blocks writes far more trie than a diagnostic
    /// should hold in RAM, and the writes are worthless once the run ends --
    /// they exist only so the replay can read back what it just computed. A
    /// scratch store on disk gives that for a fixed memory cost, and is
    /// deleted afterwards.
    pub fn with_spill(
        base: std::sync::Arc<dyn TrieStore>,
        spill: std::sync::Arc<dyn TrieStore>,
    ) -> Self {
        Self {
            base,
            overlay: Mutex::new(HashMap::new()),
            spill: Some(spill),
            written: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    /// Drops the in-memory writes, keeping the base.
    ///
    /// For a replay that resolves each block's parent from the base rather
    /// than carrying it forward: nothing a block wrote is needed once the
    /// block is done, so holding it is a leak that grows without bound over
    /// millions of blocks. Clearing between blocks makes such a replay
    /// constant-memory and, because nothing is spilled, **purely reads** on
    /// the disk — no scratch database, no compaction competing with the
    /// reads.
    ///
    /// Wrong for a sequential replay, where the next block's parent state is
    /// exactly what the last one wrote. A no-op when writes go to a spill.
    pub fn clear_overlay(&self) {
        if self.spill.is_none() {
            self.overlay.lock().unwrap().clear();
        }
    }

    /// Number of nodes written since the overlay was created.
    pub fn written(&self) -> usize {
        match self.spill {
            Some(_) => self.written.load(std::sync::atomic::Ordering::Relaxed),
            None => self.overlay.lock().unwrap().len(),
        }
    }
}

impl TrieStore for OverlayTrieStore {
    fn get(&self, key: &[u8]) -> Option<Vec<u8>> {
        match &self.spill {
            Some(s) => s.get(key).or_else(|| self.base.get(key)),
            None => {
                if let Some(v) = self.overlay.lock().unwrap().get(key) {
                    return Some(v.clone());
                }
                self.base.get(key)
            }
        }
    }

    fn put(&self, key: &[u8], value: &[u8]) {
        match &self.spill {
            Some(s) => {
                s.put(key, value);
                self.written.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            None => {
                self.overlay.lock().unwrap().insert(key.to_vec(), value.to_vec());
            }
        }
    }

    /// Nothing reaches the base either way. A spill store is a real database
    /// and is flushed, so a long run's writes are readable after a restart.
    fn flush(&self) {
        if let Some(s) = &self.spill {
            s.flush();
        }
    }

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

#[cfg(test)]
mod overlay_spill_tests {
    use super::*;
    use std::sync::Arc;

    struct ReadOnlyBase(MemoryTrieStore);

    impl TrieStore for ReadOnlyBase {
        fn get(&self, key: &[u8]) -> Option<Vec<u8>> {
            self.0.get(key)
        }
        fn put(&self, _key: &[u8], _value: &[u8]) {
            panic!("write reached the read-only base store");
        }
    }

    #[test]
    fn a_spilled_write_goes_to_the_spill_store_and_not_the_base() {
        let base = MemoryTrieStore::new();
        base.put(b"old", b"from-base");
        let spill = Arc::new(MemoryTrieStore::new());
        let overlay =
            OverlayTrieStore::with_spill(Arc::new(ReadOnlyBase(base)), spill.clone());

        overlay.put(b"new", b"written");
        assert_eq!(spill.get(b"new"), Some(b"written".to_vec()));
        assert_eq!(overlay.get(b"new"), Some(b"written".to_vec()));
        // Reads still fall through for anything the run did not write.
        assert_eq!(overlay.get(b"old"), Some(b"from-base".to_vec()));
        assert_eq!(overlay.written(), 1);
    }

    #[test]
    fn a_spilled_write_shadows_the_base() {
        let base = MemoryTrieStore::new();
        base.put(b"k", b"disk");
        let overlay = OverlayTrieStore::with_spill(
            Arc::new(ReadOnlyBase(base)),
            Arc::new(MemoryTrieStore::new()),
        );
        overlay.put(b"k", b"recomputed");
        assert_eq!(overlay.get(b"k"), Some(b"recomputed".to_vec()));
    }

    /// The point of spilling is that memory does not grow with the run.
    #[test]
    fn spilling_leaves_the_in_memory_map_empty() {
        let overlay = OverlayTrieStore::with_spill(
            Arc::new(ReadOnlyBase(MemoryTrieStore::new())),
            Arc::new(MemoryTrieStore::new()),
        );
        for i in 0..1000u32 {
            overlay.put(&i.to_be_bytes(), b"value");
        }
        assert!(overlay.overlay.lock().unwrap().is_empty(), "nothing held in memory");
        assert_eq!(overlay.written(), 1000);
    }
}

#[cfg(test)]
mod overlay_clear_tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn clearing_drops_writes_but_keeps_the_base() {
        let base = Arc::new(MemoryTrieStore::new());
        base.put(b"frombase", b"B");
        let o = OverlayTrieStore::new(base);

        o.put(b"written", b"W");
        assert_eq!(o.get(b"written").as_deref(), Some(&b"W"[..]));

        o.clear_overlay();
        assert_eq!(o.get(b"written"), None, "the write is gone");
        assert_eq!(o.get(b"frombase").as_deref(), Some(&b"B"[..]), "the base is not");
    }

    /// A spilled overlay is a sequential replay's carried state. Clearing it
    /// would silently discard the next block's parent.
    #[test]
    fn clearing_is_a_no_op_when_writes_go_to_a_spill() {
        let base = Arc::new(MemoryTrieStore::new());
        let spill = Arc::new(MemoryTrieStore::new());
        let o = OverlayTrieStore::with_spill(base, spill);

        o.put(b"carried", b"C");
        o.clear_overlay();
        assert_eq!(
            o.get(b"carried").as_deref(),
            Some(&b"C"[..]),
            "a spilled write must survive clear_overlay"
        );
    }
}

/// A store that delegates everything but reports a different embedding
/// bound.
///
/// The bound is a per-height consensus value once RSKIP-TBD activates, and
/// the store is the only thing already threaded through every call that
/// decides embedding -- `to_message`, `message_length`, `collect_for_save`.
/// Wrapping for the duration of one block therefore sets the bound for all
/// of them without a global, which also keeps tests at different heights
/// from interfering when they run in parallel.
///
/// Every call that participates in computing a state root for a block must
/// use the same wrapper: applying changes, hashing, and saving alike. Mixing
/// a wrapped and an unwrapped store within one block would produce a root
/// neither bound agrees with.
pub struct EmbeddingBound<'a> {
    inner: &'a dyn TrieStore,
    bound: usize,
}

impl<'a> EmbeddingBound<'a> {
    pub fn new(inner: &'a dyn TrieStore, bound: usize) -> Self {
        Self { inner, bound }
    }
}

impl TrieStore for EmbeddingBound<'_> {
    fn get(&self, key: &[u8]) -> Option<Vec<u8>> {
        self.inner.get(key)
    }
    fn put(&self, key: &[u8], value: &[u8]) {
        self.inner.put(key, value)
    }
    fn max_embedded_node_size(&self) -> usize {
        self.bound
    }
    fn put_many(&self, entries: &[(Vec<u8>, Vec<u8>)]) {
        self.inner.put_many(entries)
    }
    fn flush(&self) {
        self.inner.flush()
    }
    fn get_many(&self, keys: &[Vec<u8>]) -> Vec<Option<Vec<u8>>> {
        self.inner.get_many(keys)
    }
    fn get_many_sorted(&self, keys: &[Vec<u8>]) -> Vec<Option<Vec<u8>>> {
        self.inner.get_many_sorted(keys)
    }
    fn collected_below(&self) -> Option<u64> {
        self.inner.collected_below()
    }
}

#[cfg(test)]
mod embedding_threshold_tests {
    use super::*;
    use crate::{TrieKeySlice, TrieNode};

    /// A store that delegates to a `MemoryTrieStore` but reports whichever
    /// embedding bound the test asks for, standing in for the activation
    /// height the execution layer supplies in production.
    struct Bounded {
        inner: MemoryTrieStore,
        bound: usize,
    }

    impl TrieStore for Bounded {
        fn get(&self, key: &[u8]) -> Option<Vec<u8>> {
            self.inner.get(key)
        }
        fn put(&self, key: &[u8], value: &[u8]) {
            self.inner.put(key, value)
        }
        fn max_embedded_node_size(&self) -> usize {
            self.bound
        }
    }

    fn key(bits: &[u8]) -> TrieKeySlice {
        TrieKeySlice::from_key(bits)
    }

    /// Two 31-byte keys differing in the very first bit, so the root
    /// branches at once and each leaf carries a 247-bit shared path -- the
    /// shape a real account leaf has, and the reason account nodes sit near
    /// the bound at all.
    const KEY_A: [u8; 31] = [0x00; 31];
    fn key_b() -> [u8; 31] {
        let mut k = [0x00u8; 31];
        k[0] = 0x80;
        k
    }

    /// 1 (flags) + 32 (247-bit shared path) + 14 = 47: above the old bound,
    /// below the new one.
    fn banded_value() -> Vec<u8> {
        vec![0xAB; 14]
    }

    fn left_is_embedded(node: &TrieNode, store: &dyn TrieStore) -> bool {
        node.to_message(store)[0] & 0b0000_0010 != 0
    }

    /// RSKIP-TBD test case 8, and the property the whole lazy activation
    /// rests on.
    ///
    /// A child stored separately under the 44-byte bound must stay stored
    /// separately after the raise to 52, for as long as nothing rewrites the
    /// child itself -- even though its size now falls under the new bound.
    /// If it silently re-embedded when a *sibling* changed, the state root
    /// would depend on which nodes an implementation happened to have in
    /// memory, and two nodes could not agree.
    #[test]
    fn an_unmodified_child_keeps_its_embedded_status_across_the_raise() {
        let mid = banded_value();

        let old = Bounded { inner: MemoryTrieStore::new(), bound: LEGACY_MAX_EMBEDDED_NODE_SIZE };
        let mut root = TrieNode::empty();
        root = root.put(&key(&KEY_A), &mid, &old);
        root = root.put(&key(&key_b()), &[0x01][..], &old);

        // The fixture must actually be in the band, or the test is vacuous.
        assert!(
            !left_is_embedded(&root, &old),
            "fixture leaf should be too big to embed at {LEGACY_MAX_EMBEDDED_NODE_SIZE}"
        );
        root.save(&old, true);
        let root_hash = root.compute_hash(&old);

        // Reload under the raised bound and rewrite only the *sibling*.
        let new = Bounded { inner: old.inner, bound: WITNESS_MAX_EMBEDDED_NODE_SIZE };
        let reloaded = TrieNode::from_message(&new.get(root_hash.as_slice()).unwrap(), &new);
        let after_sibling = reloaded.put(&key(&key_b()), &[0x02][..], &new);

        assert_eq!(
            after_sibling.get(&key(&KEY_A), &new).as_deref(),
            Some(&mid[..]),
            "the untouched child must still be reachable"
        );
        assert!(
            !left_is_embedded(&after_sibling, &new),
            "an unmodified child must not be re-embedded by the raise alone"
        );

        // Rewriting the child itself is what lets the new bound apply.
        let after_child = after_sibling.put(&key(&KEY_A), &mid, &new);
        assert!(
            left_is_embedded(&after_child, &new),
            "rewriting the child is what applies the new bound to it"
        );
    }

    /// The bound is read from the store rather than compiled in, so two
    /// stores disagreeing about it produce different node hashes for the
    /// same content. That is the hardfork, and it should be visible.
    #[test]
    fn the_same_content_hashes_differently_under_the_two_bounds() {
        let build = |bound: usize| {
            let s = Bounded { inner: MemoryTrieStore::new(), bound };
            let mut r = TrieNode::empty();
            r = r.put(&key(&KEY_A), &banded_value(), &s);
            r = r.put(&key(&key_b()), &[0x01][..], &s);
            r.compute_hash(&s)
        };
        assert_ne!(
            build(LEGACY_MAX_EMBEDDED_NODE_SIZE),
            build(WITNESS_MAX_EMBEDDED_NODE_SIZE),
            "raising the bound must change the root for a node in the band"
        );
    }
}
