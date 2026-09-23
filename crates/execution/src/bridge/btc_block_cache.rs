//! A bounded cache for Bitcoin `StoredBlock`s read from Bridge storage.
//!
//! # The problem
//!
//! Answering "what is the main-chain BTC block at height H?" costs one trie
//! read per block of depth whenever the RSKIP199 height→hash index cannot
//! answer it -- which is every height below iris300, and any height the index
//! misses. `getBtcTransactionConfirmations` and both peg-in paths land there.
//!
//! # Why this is not rskj's cache
//!
//! rskj's `RepositoryBtcBlockStoreWithCache` **pre-loads**: every
//! `setChainHead` walks back up to 5,000 blocks and reads each one from the
//! repository, whether or not consensus asked for it
//! (`RepositoryBtcBlockStoreWithCache.populateCache`, l.245).
//!
//! rustock cannot do that, for two reasons that are not about performance:
//!
//! 1. **Self-contained trie snapshots.** A segbuild chunk holds the nodes
//!    rustock *read*, not the nodes its block range needs. Speculatively
//!    reading 5,000 entries per chain-head update enlarges the read set by
//!    entries consensus never required, and every consumer of those chunks
//!    inherits the enlargement.
//!
//! 2. **A missing node and an absent value are the same `None`.** A trie walk
//!    that dies on a missing node returns exactly what a genuine absence
//!    returns. A speculative read against a snapshot that does not hold those
//!    nodes therefore reads "absent", and a cache that believed it would hand
//!    that answer to a later *consensus-required* lookup. A performance
//!    optimisation would have become a consensus fault.
//!
//! # What makes this one safe
//!
//! Three properties, and the third is the one that matters:
//!
//! * **Immutable keys.** A `StoredBlock` is stored under its own header hash.
//!   The height and chain work of a given header are fixed by its ancestry, so
//!   the value at a key never changes. A cached entry cannot go stale.
//!
//! * **Populated only from consensus-required reads.** Nothing walks ahead.
//!   The cache fills with blocks the node read because a Bridge method needed
//!   them, so the read set is exactly what it would have been without a cache.
//!
//! * **No negative caching, ever.** An absent block is *not* recorded. A read
//!   that finds nothing -- whether because the block genuinely is not there or
//!   because the trie node is missing from an incomplete snapshot -- leaves the
//!   cache unchanged, so the next lookup asks the trie again. The failure mode
//!   in point 2 above is therefore unreachable: the cache can answer or say
//!   nothing, and it never says "no".
//!
//! # What is deliberately not cached
//!
//! The chain head and the RSKIP199 height→hash index. Both are mutable -- a
//! reorg rewrites them -- so neither has the immutable-key property above, and
//! a cache over them would need invalidation this one does not have.

use crate::bridge::btc_store::StoredBlock;
use bitcoin::BlockHash;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

/// Entries held before the least recently used is evicted.
///
/// rskj's equivalent bound is `DEFAULT_MAX_SIZE_BLOCK_CACHE = 10,000`
/// (`MaxSizeHashMap`, access-ordered). The same figure is used here so the
/// memory profile is comparable, but it is reached by demand rather than by a
/// pre-load, so in practice occupancy is far lower.
pub const DEFAULT_CAPACITY: usize = 10_000;

/// A `StoredBlock` serialises to 96 bytes (V2) or 84 (legacy); in memory it is
/// the 80-byte header plus height and chain work. With the `HashMap` entry and
/// the recency list, budget ~200 bytes per entry -- about 2 MB at the default
/// capacity.
pub const APPROX_BYTES_PER_ENTRY: usize = 200;

/// Counters, for the periodic report and for tests that assert behaviour
/// rather than timing.
#[derive(Debug, Default)]
pub struct CacheStats {
    pub hits: AtomicU64,
    pub misses: AtomicU64,
    pub inserts: AtomicU64,
    pub evictions: AtomicU64,
}

impl CacheStats {
    pub fn snapshot(&self) -> CacheStatsSnapshot {
        CacheStatsSnapshot {
            hits: self.hits.load(Ordering::Relaxed),
            misses: self.misses.load(Ordering::Relaxed),
            inserts: self.inserts.load(Ordering::Relaxed),
            evictions: self.evictions.load(Ordering::Relaxed),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CacheStatsSnapshot {
    pub hits: u64,
    pub misses: u64,
    pub inserts: u64,
    pub evictions: u64,
}

impl CacheStatsSnapshot {
    pub fn lookups(&self) -> u64 {
        self.hits + self.misses
    }

    /// Hit rate as a percentage, or `None` when nothing has been looked up.
    pub fn hit_rate(&self) -> Option<f64> {
        let total = self.lookups();
        (total > 0).then(|| self.hits as f64 * 100.0 / total as f64)
    }
}

/// An access-ordered LRU over `BlockHash -> StoredBlock`.
///
/// Hand-rolled rather than pulled from a crate because the eviction policy is
/// something the tests assert directly, and a policy that lives in this file
/// can be read alongside the argument for why it is safe.
struct Lru {
    map: HashMap<BlockHash, (StoredBlock, u64)>,
    /// Monotonic access counter; the entry with the lowest value is evicted.
    clock: u64,
    capacity: usize,
}

impl Lru {
    fn new(capacity: usize) -> Self {
        Self { map: HashMap::new(), clock: 0, capacity: capacity.max(1) }
    }

    fn get(&mut self, hash: &BlockHash) -> Option<StoredBlock> {
        self.clock += 1;
        let clock = self.clock;
        let entry = self.map.get_mut(hash)?;
        entry.1 = clock;
        Some(entry.0.clone())
    }

    /// Insert, evicting the least recently used entry if full. Returns true if
    /// an eviction happened.
    fn put(&mut self, hash: BlockHash, block: StoredBlock) -> bool {
        self.clock += 1;
        if self.map.contains_key(&hash) {
            let clock = self.clock;
            let e = self.map.get_mut(&hash).expect("checked");
            e.0 = block;
            e.1 = clock;
            return false;
        }

        let mut evicted = false;
        if self.map.len() >= self.capacity {
            if let Some(victim) = self
                .map
                .iter()
                .min_by_key(|(_, (_, used))| *used)
                .map(|(h, _)| *h)
            {
                self.map.remove(&victim);
                evicted = true;
            }
        }
        self.map.insert(hash, (block, self.clock));
        evicted
    }
}

/// A bounded, demand-populated cache of BTC stored blocks.
///
/// Shared across blocks by the node, so it must be `Send + Sync`; contention is
/// negligible because execution is single-threaded per block.
pub struct BtcBlockCache {
    lru: Mutex<Lru>,
    pub stats: CacheStats,
}

impl BtcBlockCache {
    pub fn new(capacity: usize) -> Self {
        Self { lru: Mutex::new(Lru::new(capacity)), stats: CacheStats::default() }
    }

    pub fn with_default_capacity() -> Self {
        Self::new(DEFAULT_CAPACITY)
    }

    /// Look a block up. A miss is just a miss -- the caller reads the trie.
    pub fn get(&self, hash: &BlockHash) -> Option<StoredBlock> {
        let found = self.lru.lock().expect("btc block cache").get(hash);
        match &found {
            Some(_) => self.stats.hits.fetch_add(1, Ordering::Relaxed),
            None => self.stats.misses.fetch_add(1, Ordering::Relaxed),
        };
        found
    }

    /// Record a block that was actually read from committed state.
    ///
    /// There is no companion `insert_absent`, and there must never be one: see
    /// the module docs. A caller that read nothing calls nothing.
    pub fn insert(&self, hash: BlockHash, block: StoredBlock) {
        let evicted = self.lru.lock().expect("btc block cache").put(hash, block);
        self.stats.inserts.fetch_add(1, Ordering::Relaxed);
        if evicted {
            self.stats.evictions.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub fn len(&self) -> usize {
        self.lru.lock().expect("btc block cache").map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn capacity(&self) -> usize {
        self.lru.lock().expect("btc block cache").capacity
    }

    /// Approximate resident bytes, for the periodic report.
    pub fn approx_bytes(&self) -> usize {
        self.len() * APPROX_BYTES_PER_ENTRY
    }

    /// Drop every entry. Only for tests and for an operator command; normal
    /// operation never needs it, because entries cannot go stale.
    pub fn clear(&self) {
        self.lru.lock().expect("btc block cache").map.clear();
    }
}

impl std::fmt::Debug for BtcBlockCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = self.stats.snapshot();
        write!(
            f,
            "BtcBlockCache {{ {}/{} entries, {} hits, {} misses, {} evictions }}",
            self.len(),
            self.capacity(),
            s.hits,
            s.misses,
            s.evictions
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::U256;
    use bitcoin::block::{Header as BtcHeader, Version};
    use bitcoin::hashes::Hash;
    use bitcoin::{CompactTarget, TxMerkleNode};

    fn block(nonce: u32, height: u32) -> StoredBlock {
        let header = BtcHeader {
            version: Version::from_consensus(1),
            prev_blockhash: BlockHash::all_zeros(),
            merkle_root: TxMerkleNode::all_zeros(),
            time: 1_700_000_000,
            bits: CompactTarget::from_consensus(0x1d00ffff),
            nonce,
        };
        StoredBlock::new(header, height, U256::from(height as u64 * 1000))
    }

    // -- correctness -----------------------------------------------------

    #[test]
    fn a_stored_block_comes_back_unchanged() {
        let cache = BtcBlockCache::new(16);
        let b = block(1, 700_000);
        let h = b.header.block_hash();

        assert!(cache.get(&h).is_none(), "empty cache answered");
        cache.insert(h, b.clone());

        let got = cache.get(&h).expect("miss after insert");
        assert_eq!(got.header.block_hash(), h);
        assert_eq!(got.height, b.height);
        assert_eq!(got.chain_work, b.chain_work);
    }

    #[test]
    fn a_block_that_was_never_inserted_is_a_miss() {
        let cache = BtcBlockCache::new(16);
        cache.insert(block(1, 1).header.block_hash(), block(1, 1));
        assert!(cache.get(&block(2, 2).header.block_hash()).is_none());
    }

    /// The property the whole design rests on. There is no API to record an
    /// absence, so a lookup that found nothing cannot poison the cache — which
    /// is what would turn a missing trie node into a wrong consensus answer.
    #[test]
    fn a_miss_records_nothing() {
        let cache = BtcBlockCache::new(16);
        let h = block(9, 9).header.block_hash();

        for _ in 0..10 {
            assert!(cache.get(&h).is_none());
        }
        assert_eq!(cache.len(), 0, "a miss added an entry");
        assert_eq!(cache.stats.snapshot().inserts, 0);
        assert_eq!(cache.stats.snapshot().misses, 10);
    }

    #[test]
    fn stats_count_hits_and_misses() {
        let cache = BtcBlockCache::new(16);
        let b = block(1, 1);
        let h = b.header.block_hash();

        cache.get(&h);            // miss
        cache.insert(h, b);
        cache.get(&h);            // hit
        cache.get(&h);            // hit

        let s = cache.stats.snapshot();
        assert_eq!((s.hits, s.misses, s.inserts), (2, 1, 1));
        assert_eq!(s.hit_rate().map(|r| r.round()), Some(67.0));
    }

    #[test]
    fn re_inserting_a_key_replaces_rather_than_grows() {
        let cache = BtcBlockCache::new(16);
        let b = block(1, 1);
        let h = b.header.block_hash();
        cache.insert(h, b.clone());
        cache.insert(h, b);
        assert_eq!(cache.len(), 1);
        assert_eq!(cache.stats.snapshot().evictions, 0);
    }

    // -- eviction and memory --------------------------------------------

    #[test]
    fn the_cache_never_exceeds_its_capacity() {
        let cache = BtcBlockCache::new(10);
        for n in 0..1_000u32 {
            cache.insert(block(n, n).header.block_hash(), block(n, n));
            assert!(cache.len() <= 10, "capacity exceeded at insert {n}: {}", cache.len());
        }
        assert_eq!(cache.len(), 10);
        assert_eq!(cache.stats.snapshot().evictions, 990);
    }

    /// The policy is asserted directly, not inferred from the size. An LRU
    /// that evicts *something* is not an LRU.
    #[test]
    fn the_least_recently_used_entry_is_the_one_evicted() {
        let cache = BtcBlockCache::new(3);
        let (a, b, c, d) = (block(1, 1), block(2, 2), block(3, 3), block(4, 4));
        let (ha, hb, hc, hd) = (
            a.header.block_hash(),
            b.header.block_hash(),
            c.header.block_hash(),
            d.header.block_hash(),
        );

        cache.insert(ha, a);
        cache.insert(hb, b);
        cache.insert(hc, c);

        // Touch a and c, leaving b as least recently used.
        assert!(cache.get(&ha).is_some());
        assert!(cache.get(&hc).is_some());

        cache.insert(hd, d);

        assert!(cache.get(&hb).is_none(), "the wrong entry was evicted");
        assert!(cache.get(&ha).is_some(), "a recently used entry was evicted");
        assert!(cache.get(&hc).is_some(), "a recently used entry was evicted");
        assert!(cache.get(&hd).is_some(), "the new entry is missing");
    }

    /// A deep sequential walk — the access pattern this cache exists for —
    /// must not be able to grow memory without bound.
    #[test]
    fn a_deep_sequential_walk_stays_within_the_bound() {
        let cache = BtcBlockCache::new(100);
        for n in 0..50_000u32 {
            let b = block(n, n);
            cache.insert(b.header.block_hash(), b);
        }
        assert_eq!(cache.len(), 100);
        assert!(cache.approx_bytes() <= 100 * APPROX_BYTES_PER_ENTRY);
    }

    #[test]
    fn eviction_changes_cost_but_never_an_answer() {
        let cache = BtcBlockCache::new(4);
        let blocks: Vec<StoredBlock> = (0..40u32).map(|n| block(n, n)).collect();
        for b in &blocks {
            cache.insert(b.header.block_hash(), b.clone());
        }
        // Whatever survived must still be correct; what did not is simply a
        // miss, and the caller reads the trie.
        for b in &blocks {
            if let Some(got) = cache.get(&b.header.block_hash()) {
                assert_eq!(got.height, b.height);
                assert_eq!(got.chain_work, b.chain_work);
            }
        }
    }

    #[test]
    fn a_capacity_of_zero_is_clamped_rather_than_dividing_by_zero() {
        let cache = BtcBlockCache::new(0);
        let b = block(1, 1);
        cache.insert(b.header.block_hash(), b.clone());
        assert!(cache.len() <= 1);
    }

    #[test]
    fn the_default_capacity_matches_rskj_and_its_footprint_is_stated() {
        assert_eq!(DEFAULT_CAPACITY, 10_000, "rskj DEFAULT_MAX_SIZE_BLOCK_CACHE");
        let cache = BtcBlockCache::with_default_capacity();
        assert_eq!(cache.capacity(), 10_000);
        // ~2 MB at full occupancy; the issue asks for this to be a number.
        assert!(DEFAULT_CAPACITY * APPROX_BYTES_PER_ENTRY < 4 * 1024 * 1024);
    }

    #[test]
    fn clearing_empties_the_cache() {
        let cache = BtcBlockCache::new(8);
        let b = block(1, 1);
        cache.insert(b.header.block_hash(), b);
        cache.clear();
        assert!(cache.is_empty());
    }

    #[test]
    fn the_cache_is_shareable_across_threads() {
        use std::sync::Arc;
        let cache = Arc::new(BtcBlockCache::new(1_000));
        let handles: Vec<_> = (0..4u32)
            .map(|t| {
                let c = cache.clone();
                std::thread::spawn(move || {
                    for n in 0..500u32 {
                        let b = block(t * 1000 + n, n);
                        let h = b.header.block_hash();
                        c.insert(h, b);
                        c.get(&h);
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        assert!(cache.len() <= 1_000);
    }
}
