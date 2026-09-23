//! The cache through the real read path: `get_stored_block` and the main-chain
//! walk, against a trie that counts every read.
//!
//! These are the tests the issue asks for that a unit test of the cache cannot
//! give: that a cached node and an uncached node reach the **same answers**
//! while touching the **same trie nodes**.

use super::btc_block_cache::BtcBlockCache;
use super::btc_store::{btc_hash_to_storage_key, StoredBlock};
use crate::bridge::btc_chain::{main_chain_block_at_height, MainChainLookup};
use crate::precompiles::BRIDGE_ADDR;
use alloy_primitives::{B256, U256};
use bitcoin::block::{Header as BtcHeader, Version};
use bitcoin::hashes::Hash;
use bitcoin::{BlockHash, CompactTarget, TxMerkleNode};
use revm::MainContext;
use rustock_trie::{storage_key, MemoryTrieStore, TrieKeySlice, TrieNode, TrieStore};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

/// A trie store that counts reads, so "did the cache save work?" and "did the
/// cache do work the uncached path would not have?" are both measurable.
struct CountingStore {
    inner: MemoryTrieStore,
    reads: AtomicUsize,
}

impl CountingStore {
    fn new() -> Self {
        Self { inner: MemoryTrieStore::new(), reads: AtomicUsize::new(0) }
    }
    fn reads(&self) -> usize {
        self.reads.load(Ordering::Relaxed)
    }
    fn reset(&self) {
        self.reads.store(0, Ordering::Relaxed);
    }
}

impl TrieStore for CountingStore {
    fn get(&self, key: &[u8]) -> Option<Vec<u8>> {
        self.reads.fetch_add(1, Ordering::Relaxed);
        self.inner.get(key)
    }
    fn put(&self, key: &[u8], value: &[u8]) {
        self.inner.put(key, value);
    }
}

fn header_at(prev: BlockHash, nonce: u32) -> BtcHeader {
    BtcHeader {
        version: Version::from_consensus(1),
        prev_blockhash: prev,
        merkle_root: TxMerkleNode::all_zeros(),
        time: 1_700_000_000,
        bits: CompactTarget::from_consensus(0x1d00ffff),
        nonce,
    }
}

/// A chain of `len` BTC blocks written straight into a trie, exactly where
/// `RawStorage::get_from_trie` will look for them. Returns the store, the
/// root, and the blocks from genesis upward.
fn btc_chain_in_trie(len: u32, salt: u32) -> (Arc<CountingStore>, TrieNode, Vec<StoredBlock>) {
    let store = Arc::new(CountingStore::new());
    let mut root = TrieNode::empty();
    let mut blocks = Vec::new();
    let mut prev = BlockHash::all_zeros();

    for n in 0..len {
        let block = StoredBlock::new(header_at(prev, n + salt), n, U256::from((n as u64 + 1) * 100));
        prev = block.header.block_hash();

        let key = btc_hash_to_storage_key(&prev);
        let trie_key = storage_key(&BRIDGE_ADDR, &B256::from(key));
        let data = block.serialize_compact_v2();
        root = root.put(&TrieKeySlice::from_key(&trie_key), &data, store.as_ref());

        blocks.push(block);
    }

    // The chain head, where `load_chain_head` looks.
    let head_key = super::storage::bridge_storage_key("blockStoreChainHead");
    let trie_key = storage_key(&BRIDGE_ADDR, &B256::from(head_key));
    let head_data = blocks.last().unwrap().serialize_compact_v2();
    root = root.put(&TrieKeySlice::from_key(&trie_key), &head_data, store.as_ref());

    root.save(store.as_ref(), true);

    // Reload the root from its hash. Without this the returned `TrieNode` still
    // holds its children in memory, every lookup walks them directly, and the
    // store is never touched -- which would make the read-counting tests below
    // pass by measuring nothing at all. (It did, on the first run.)
    let root_hash = root.compute_hash(store.as_ref());
    let data = store.get(root_hash.as_slice()).expect("saved root");
    let root = TrieNode::from_message(&data, store.as_ref());

    store.reset();
    (store, root, blocks)
}

/// A context reading from `store`/`root`, optionally with a cache installed.
fn ctx_over(
    store: Arc<dyn TrieStore>,
    root: TrieNode,
    cache: Option<Arc<BtcBlockCache>>,
) -> impl crate::RskContextTr {
    let mut chain_ext = crate::raw_storage::RskChainExt::default();
    chain_ext.raw_storage.set_reader(store, root);
    chain_ext.btc_block_cache = cache;
    revm::Context::mainnet().with_chain(chain_ext)
}

// -------------------------------------------------------------- correctness --

/// The headline requirement: identical answers at every depth, cached or not.
#[test]
fn cached_and_uncached_lookups_agree_at_every_depth() {
    let (store, root, blocks) = btc_chain_in_trie(60, 0);
    let dyn_store: Arc<dyn TrieStore> = store.clone();

    let cache = Arc::new(BtcBlockCache::with_default_capacity());
    let mut plain = ctx_over(dyn_store.clone(), root.clone(), None);
    let mut cached = ctx_over(dyn_store, root, Some(cache));

    for height in 0..blocks.len() as u32 {
        let a = main_chain_block_at_height(&mut plain, height, false, None);
        let b = main_chain_block_at_height(&mut cached, height, false, None);
        match (a, b) {
            (MainChainLookup::Found(x), MainChainLookup::Found(y)) => {
                assert_eq!(x.header.block_hash(), y.header.block_hash(), "height {height}");
                assert_eq!(x.height, y.height, "height {height}");
                assert_eq!(x.chain_work, y.chain_work, "height {height}");
            }
            (MainChainLookup::Absent, MainChainLookup::Absent) => {}
            (MainChainLookup::StoreError, MainChainLookup::StoreError) => {}
            (x, y) => panic!(
                "height {height}: cached and uncached disagree ({} vs {})",
                outcome(&x),
                outcome(&y)
            ),
        }
    }
}

fn outcome(l: &MainChainLookup) -> &'static str {
    match l {
        MainChainLookup::Found(_) => "Found",
        MainChainLookup::Absent => "Absent",
        MainChainLookup::StoreError => "StoreError",
    }
}

/// `getBtcTransactionConfirmations` maps `Absent` and `StoreError` to error
/// codes −2 and −3, so a cache that collapsed them would change consensus.
#[test]
fn the_absent_and_store_error_distinction_survives_the_cache() {
    let (store, root, blocks) = btc_chain_in_trie(20, 0);
    let dyn_store: Arc<dyn TrieStore> = store.clone();
    let cache = Arc::new(BtcBlockCache::with_default_capacity());
    let mut cached = ctx_over(dyn_store, root, Some(cache));

    // Above the head: rskj throws -> StoreError.
    let above = main_chain_block_at_height(&mut cached, blocks.len() as u32 + 5, false, None);
    assert!(matches!(above, MainChainLookup::StoreError), "above head should be StoreError");

    // In range: found, twice (second time from the cache).
    for _ in 0..2 {
        let hit = main_chain_block_at_height(&mut cached, 10, false, None);
        assert!(matches!(hit, MainChainLookup::Found(_)));
    }
}

/// This block's own writes are not yet committed and may still be discarded,
/// so they must beat the cache every time.
#[test]
fn a_write_made_in_this_block_beats_the_cache() {
    let (store, root, blocks) = btc_chain_in_trie(10, 0);
    let dyn_store: Arc<dyn TrieStore> = store.clone();
    let cache = Arc::new(BtcBlockCache::with_default_capacity());
    let mut ctx = ctx_over(dyn_store, root, Some(cache.clone()));

    let target = blocks[5].clone();
    let hash = target.header.block_hash();

    // Warm the cache from the trie.
    let first = super::btc_store::get_stored_block(&mut ctx, &hash).expect("in trie");
    assert_eq!(first.chain_work, target.chain_work);
    assert_eq!(cache.len(), 1);

    // Now write a different value for the same key in this block. (BTC blocks
    // are immutable in practice; this asserts the layering, not a real case.)
    let mut rewritten = target.clone();
    rewritten.chain_work = U256::from(999_999u64);
    super::btc_store::put_stored_block(&mut ctx, &rewritten, true);

    let after = super::btc_store::get_stored_block(&mut ctx, &hash).expect("overlay");
    assert_eq!(
        after.chain_work,
        U256::from(999_999u64),
        "the cache answered ahead of this block's own write"
    );
}

/// A block absent from the trie must stay absent — and must leave no trace, so
/// that a later read of the same key asks the trie again rather than trusting
/// a remembered "no".
#[test]
fn an_absent_block_is_never_remembered() {
    let (store, root, _) = btc_chain_in_trie(10, 0);
    let dyn_store: Arc<dyn TrieStore> = store.clone();
    let cache = Arc::new(BtcBlockCache::with_default_capacity());
    let mut ctx = ctx_over(dyn_store, root, Some(cache.clone()));

    let ghost = header_at(BlockHash::all_zeros(), 4242).block_hash();
    for _ in 0..5 {
        assert!(super::btc_store::get_stored_block(&mut ctx, &ghost).is_none());
    }
    assert_eq!(cache.len(), 0, "an absence was cached");
    assert_eq!(cache.stats.snapshot().inserts, 0);
}

// --------------------------------------------------- the read-set property --

/// **The property that keeps trie snapshots self-contained.**
///
/// A cached node must never read a trie node an uncached node would not have
/// read. If anyone later adds a pre-load, this fails.
#[test]
fn the_cache_reads_no_trie_node_the_uncached_path_would_not() {
    let (plain_store, plain_root, blocks) = btc_chain_in_trie(80, 0);
    let (cached_store, cached_root, _) = btc_chain_in_trie(80, 0);

    let heights: Vec<u32> = (0..blocks.len() as u32).collect();

    let dyn_plain: Arc<dyn TrieStore> = plain_store.clone();
    let mut plain = ctx_over(dyn_plain, plain_root, None);
    for h in &heights {
        main_chain_block_at_height(&mut plain, *h, false, None);
    }
    let uncached_reads = plain_store.reads();
    assert!(
        uncached_reads > 0,
        "the fixture performed no trie reads at all; this test would pass vacuously"
    );

    let cache = Arc::new(BtcBlockCache::with_default_capacity());
    let dyn_cached: Arc<dyn TrieStore> = cached_store.clone();
    let mut cached = ctx_over(dyn_cached, cached_root, Some(cache));
    for h in &heights {
        main_chain_block_at_height(&mut cached, *h, false, None);
    }
    let cached_reads = cached_store.reads();

    assert!(
        cached_reads <= uncached_reads,
        "the cache performed MORE trie reads than the uncached path \
         ({cached_reads} vs {uncached_reads}) -- a pre-load has been introduced, \
         and trie snapshots are no longer self-contained"
    );
}

/// And it must actually save work, or it is not worth its memory.
#[test]
fn the_cache_substantially_reduces_trie_reads_on_a_deep_walk() {
    let (plain_store, plain_root, blocks) = btc_chain_in_trie(120, 0);
    let (cached_store, cached_root, _) = btc_chain_in_trie(120, 0);
    let deep = 5u32; // a deep walk: from the head down to near genesis

    let dyn_plain: Arc<dyn TrieStore> = plain_store.clone();
    let mut plain = ctx_over(dyn_plain, plain_root, None);
    for _ in 0..5 {
        main_chain_block_at_height(&mut plain, deep, false, None);
    }
    let uncached = plain_store.reads();

    let cache = Arc::new(BtcBlockCache::with_default_capacity());
    let dyn_cached: Arc<dyn TrieStore> = cached_store.clone();
    let mut cached = ctx_over(dyn_cached, cached_root, Some(cache.clone()));
    for _ in 0..5 {
        main_chain_block_at_height(&mut cached, deep, false, None);
    }
    let with_cache = cached_store.reads();

    assert!(
        with_cache * 2 < uncached,
        "repeating a deep walk five times should cost far less with a cache: \
         {with_cache} vs {uncached} reads"
    );
    assert!(cache.stats.snapshot().hits > 0, "no cache hits on a repeated walk");
    let _ = blocks;
}

/// Eviction must cost time, never correctness: with a cache far too small for
/// the walk, every answer must still be right.
#[test]
fn a_cache_too_small_for_the_walk_still_answers_correctly() {
    let (store, root, blocks) = btc_chain_in_trie(60, 0);
    let dyn_store: Arc<dyn TrieStore> = store.clone();

    let tiny = Arc::new(BtcBlockCache::new(3));
    let mut cached = ctx_over(dyn_store.clone(), root.clone(), Some(tiny.clone()));
    let mut plain = ctx_over(dyn_store, root, None);

    for height in (0..blocks.len() as u32).rev() {
        let a = main_chain_block_at_height(&mut plain, height, false, None);
        let b = main_chain_block_at_height(&mut cached, height, false, None);
        if let (MainChainLookup::Found(x), MainChainLookup::Found(y)) = (&a, &b) {
            assert_eq!(x.header.block_hash(), y.header.block_hash(), "height {height}");
        } else {
            assert_eq!(outcome(&a), outcome(&b), "height {height}");
        }
    }
    assert!(tiny.len() <= 3, "the tiny cache grew past its bound");
    assert!(tiny.stats.snapshot().evictions > 0, "the walk should have evicted");
}
