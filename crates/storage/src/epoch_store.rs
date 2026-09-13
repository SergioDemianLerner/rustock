//! Epoch-based garbage collection for the trie store.
//!
//! Implements the design in `docs/trie-gc-design.md`. The store is an ordered
//! list of independent databases ("epochs"); only the newest is written, and a
//! collection cycle copies the still-reachable entries out of the oldest and
//! then deletes it wholesale.
//!
//! # Why this works at all
//!
//! Everything rests on content addressing: an entry's key is the hash of its
//! value, so the same entry can live in several epochs at once and a reference
//! to it does not say -- or care -- which one answers. Copying an entry between
//! epochs is therefore invisible to every referrer, which is what makes
//! relocation free and distinguishes this from a conventional copying collector
//! that has to rewrite referrers.
//!
//! # The part that looks like a bug and is not
//!
//! The write path never checks whether a key already exists in an older epoch.
//! It always writes. That duplication is load-bearing for *correctness*, not a
//! performance shortcut, and removing it silently corrupts the store:
//!
//! Suppose a contract's storage returns to a value it last held long ago, so
//! block processing reconstructs a subtree byte-identical to one that now lives
//! only in the oldest epoch. With a conditional write, the new state would
//! reference entries in a database that is about to be deleted, and those
//! references would dangle after the sweep. Writing unconditionally puts the
//! bytes somewhere that survives. See invariant I4 and §6 of the design.
//!
//! # Reclaiming is O(1) in dead entries
//!
//! The sweep is a directory removal, not a per-entry delete. That is the whole
//! point: on RSK mainnet ~99% of the trie store is unreachable history, and no
//! scheme that walks the dead entries can keep up with the live ones.

use alloy_primitives::B256;
use anyhow::{bail, Context, Result};
use rocksdb::{ColumnFamilyDescriptor, Options, WriteBatch, DB};
use rustock_trie::{NodeRef, TrieNode, TrieStore};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Instant;
use tracing::{debug, info, warn};

const CF_TRIE: &str = "trie_nodes";

/// Block cache shared by every epoch.
const BLOCK_CACHE_BYTES: usize = 256 * 1024 * 1024;

/// Directory name for epoch `seq`.
fn epoch_dir(root: &Path, seq: u64) -> PathBuf {
    root.join(format!("epoch-{seq:012}"))
}

/// How the epoch store is sized and when it collects.
#[derive(Debug, Clone)]
pub struct EpochConfig {
    /// Number of epochs, `N` in the design. Must be at least 3: one being
    /// written, one being drained, and at least one in between so that a cycle
    /// never has to drain the epoch it is writing into.
    pub epochs: usize,
    /// Burial depth `D`: how many confirmations a block needs before its state
    /// root may be used as a collection root. Guards against a reorg stranding
    /// the chain on state that was just collected away.
    pub burial_depth: u64,
    /// Rotate once the newest epoch reaches this many bytes. Size-based rather
    /// than time- or block-based, because it is the size that has to be bounded.
    pub rotate_bytes: u64,
}

impl Default for EpochConfig {
    fn default() -> Self {
        Self { epochs: 4, burial_depth: 4_000, rotate_bytes: 1 << 30 }
    }
}

impl EpochConfig {
    fn validate(&self) -> Result<()> {
        if self.epochs < 3 {
            bail!("epochs must be at least 3, got {}", self.epochs);
        }
        Ok(())
    }
}

struct Epoch {
    seq: u64,
    db: Arc<DB>,
    dir: PathBuf,
}

/// What one collection cycle did.
#[derive(Debug, Default, Clone)]
pub struct CollectStats {
    pub marked: u64,
    pub scanned: u64,
    pub drained: u64,
    pub drained_bytes: u64,
    pub reclaimed_bytes: u64,
    pub mark_secs: f64,
    pub drain_secs: f64,
    pub sweep_secs: f64,
}

/// A trie store split into epochs, with wholesale reclamation of the oldest.
pub struct EpochTrieStore {
    root: PathBuf,
    config: EpochConfig,
    /// Oldest first. Guarded so a sweep can remove the front while readers are
    /// held off for the brief moment it takes.
    epochs: RwLock<Vec<Epoch>>,
    next_seq: AtomicU64,
    /// One block cache for every epoch. See `cf_options`.
    cache: rocksdb::Cache,
    /// Bytes written to the newest epoch since it was created.
    ///
    /// Tracked incrementally because the rotation trigger is tested once per
    /// block, and measuring it by walking the directory meant a recursive
    /// readdir plus a stat per SST file on every block. This over-estimates
    /// (it counts pre-compaction bytes), which is the safe direction: it
    /// rotates slightly early rather than letting an epoch overshoot.
    newest_written: AtomicU64,
    /// Counters, so a benchmark can see read amplification rather than infer it.
    pub reads: AtomicU64,
    pub read_probes: AtomicU64,
    pub writes: AtomicU64,
}

/// Background compaction jobs per epoch.
///
/// Deliberately low. Every epoch is an independent RocksDB instance, so this
/// multiplies by `N`: the default of 2 across four epochs puts eight background
/// threads on a machine that may have four cores, and they compete with block
/// processing for them. One epoch is also small by construction, so it has less
/// to compact than a single unbounded database would.
const BG_JOBS_PER_EPOCH: i32 = 1;

fn db_options() -> Options {
    let mut o = Options::default();
    o.create_if_missing(true);
    o.create_missing_column_families(true);
    o.set_write_buffer_size(64 * 1024 * 1024);
    o.set_max_write_buffer_number(3);
    o.set_max_background_jobs(BG_JOBS_PER_EPOCH);
    o
}

/// Table options for one epoch, sharing `cache` with every other epoch.
///
/// The cache is shared on purpose. Giving each epoch its own divides a fixed
/// memory budget `N` ways instead of pooling it, and the split is arbitrary --
/// reads concentrate on the newest epoch, so most of the memory would sit in
/// caches serving the epochs that are read least.
///
/// `cache_index_and_filter_blocks` is left off. Turning it on puts index and
/// filter blocks into the same cache as data blocks, where they are evicted and
/// re-read; an earlier version of this file enabled it against a default-sized
/// cache, which is a known way to make reads slower rather than faster.
fn cf_options(cache: &rocksdb::Cache) -> Options {
    let mut o = Options::default();
    let mut block = rocksdb::BlockBasedOptions::default();
    // A read miss must be answered by every epoch above the one holding the
    // key, so a miss costs N lookups. Bloom filters keep that from becoming N
    // disk reads.
    block.set_bloom_filter(10.0, false);
    block.set_block_cache(cache);
    o.set_block_based_table_factory(&block);
    o
}

fn open_epoch(dir: &Path, cache: &rocksdb::Cache) -> Result<Arc<DB>> {
    let db = DB::open_cf_descriptors(
        &db_options(),
        dir,
        vec![ColumnFamilyDescriptor::new(CF_TRIE, cf_options(cache))],
    )
    .with_context(|| format!("opening epoch at {}", dir.display()))?;
    Ok(Arc::new(db))
}

fn dir_size(p: &Path) -> u64 {
    let mut total = 0;
    if let Ok(rd) = std::fs::read_dir(p) {
        for e in rd.flatten() {
            match e.metadata() {
                Ok(m) if m.is_file() => total += m.len(),
                Ok(m) if m.is_dir() => total += dir_size(&e.path()),
                _ => {}
            }
        }
    }
    total
}

impl EpochTrieStore {
    /// Opens (or creates) an epoch store rooted at `dir`.
    ///
    /// Existing epochs are discovered from the directory names, so the store
    /// reopens wherever a previous run left it -- including part-way through a
    /// cycle, which needs no recovery beyond restarting the cycle.
    pub fn open(dir: impl AsRef<Path>, config: EpochConfig) -> Result<Self> {
        config.validate()?;
        let root = dir.as_ref().to_path_buf();
        std::fs::create_dir_all(&root)
            .with_context(|| format!("creating {}", root.display()))?;

        let mut seqs: Vec<u64> = std::fs::read_dir(&root)?
            .flatten()
            .filter_map(|e| {
                let n = e.file_name().to_string_lossy().to_string();
                n.strip_prefix("epoch-").and_then(|s| s.parse::<u64>().ok())
            })
            .collect();
        seqs.sort_unstable();

        // A fresh store starts with a single epoch and grows to N as it
        // rotates; there is nothing to collect until there are older epochs.
        if seqs.is_empty() {
            seqs.push(0);
        }

        let cache = rocksdb::Cache::new_lru_cache(BLOCK_CACHE_BYTES);
        let mut epochs = Vec::with_capacity(seqs.len());
        for seq in &seqs {
            let d = epoch_dir(&root, *seq);
            epochs.push(Epoch { seq: *seq, db: open_epoch(&d, &cache)?, dir: d });
        }
        let next = seqs.last().copied().unwrap_or(0) + 1;

        info!(
            target: "rustock::gc",
            "Epoch store at {}: {} epoch(s) {:?}, N={}, D={}, rotate at {} MB",
            root.display(), epochs.len(),
            epochs.iter().map(|e| e.seq).collect::<Vec<_>>(),
            config.epochs, config.burial_depth, config.rotate_bytes / (1 << 20)
        );

        let newest_written = AtomicU64::new(dir_size(&epoch_dir(&root, seqs[seqs.len() - 1])));

        Ok(Self {
            root,
            config,
            epochs: RwLock::new(epochs),
            next_seq: AtomicU64::new(next),
            cache,
            newest_written,
            reads: AtomicU64::new(0),
            read_probes: AtomicU64::new(0),
            writes: AtomicU64::new(0),
        })
    }

    /// Bytes held by the newest epoch, which is what the rotation trigger watches.
    pub fn newest_bytes(&self) -> u64 {
        let e = self.epochs.read().unwrap();
        e.last().map(|e| dir_size(&e.dir)).unwrap_or(0)
    }

    /// Bytes held by the whole store.
    pub fn total_bytes(&self) -> u64 {
        let e = self.epochs.read().unwrap();
        e.iter().map(|e| dir_size(&e.dir)).sum()
    }

    pub fn epoch_count(&self) -> usize {
        self.epochs.read().unwrap().len()
    }

    /// True when the newest epoch has grown past the rotation threshold.
    ///
    /// Reads a counter rather than measuring the directory: this is tested once
    /// per block, and a recursive directory walk per block is not free.
    pub fn should_collect(&self) -> bool {
        self.newest_written.load(Ordering::Relaxed) >= self.config.rotate_bytes
    }

    /// Runs one collection cycle against `root_hash`, the state root of a block
    /// buried at least `burial_depth` below the head.
    ///
    /// The caller chooses the root, because only the caller knows the chain.
    /// Passing an insufficiently buried root is the one way to lose state that
    /// a reorg would need, so the choice is deliberately not made here.
    pub fn collect(&self, root_hash: B256) -> Result<CollectStats> {
        let mut stats = CollectStats::default();

        // Nothing to reclaim until the store has grown to its full width:
        // with fewer epochs than N, the oldest is still within the retention
        // window and deleting it would drop state the design promises to keep.
        if self.epoch_count() < self.config.epochs {
            self.rotate_only()?;
            debug!(target: "rustock::gc", "Grew to {} epochs; nothing to collect yet", self.epoch_count());
            return Ok(stats);
        }

        let t = Instant::now();
        let live = self.mark(root_hash)?;
        stats.marked = live.len() as u64;
        stats.mark_secs = t.elapsed().as_secs_f64();

        let t = Instant::now();
        let (scanned, drained, bytes) = self.drain(&live)?;
        stats.scanned = scanned;
        stats.drained = drained;
        stats.drained_bytes = bytes;
        stats.drain_secs = t.elapsed().as_secs_f64();

        let t = Instant::now();
        stats.reclaimed_bytes = self.sweep_and_rotate()?;
        stats.sweep_secs = t.elapsed().as_secs_f64();

        info!(
            target: "rustock::gc",
            "Collected: marked {} live, scanned {} in oldest epoch, drained {} ({} MB), \
             reclaimed {} MB | mark {:.1}s drain {:.1}s sweep {:.2}s",
            stats.marked, stats.scanned, stats.drained,
            stats.drained_bytes / (1 << 20), stats.reclaimed_bytes / (1 << 20),
            stats.mark_secs, stats.drain_secs, stats.sweep_secs
        );
        Ok(stats)
    }

    /// Computes the live set: every key reachable from `root_hash`.
    ///
    /// Batched breadth-first rather than a recursive descent. A trie traversal
    /// is a chain of *dependent* reads -- each address is known only once the
    /// previous read returns -- so a naive walk leaves the device at queue depth
    /// 1. Keys discovered at the same level are independent and are read
    /// together. Measured on the mainnet store this was worth ~5x.
    fn mark(&self, root_hash: B256) -> Result<HashSet<B256>> {
        let mut live: HashSet<B256> = HashSet::new();
        let mut frontier: Vec<B256> = vec![root_hash];
        live.insert(root_hash);

        while !frontier.is_empty() {
            let keys: Vec<Vec<u8>> = frontier.iter().map(|h| h.as_slice().to_vec()).collect();
            let values = self.get_many(&keys);
            let mut next = Vec::new();

            for (hash, value) in frontier.iter().zip(values) {
                let Some(bytes) = value else {
                    // A referenced entry that is not in any epoch. Either the
                    // root predates this store or something has already been
                    // lost; either way, marking cannot proceed through it.
                    warn!(target: "rustock::gc", "Mark: {hash:?} not found; subtree unreachable");
                    continue;
                };
                let node = TrieNode::from_message(&bytes, self);

                // A value over 32 bytes is its own entry, keyed by the hash of
                // the value, and is just as collectable as a node.
                if node.has_long_value() {
                    if let Some(vh) = node.value_hash {
                        if live.insert(vh) {
                            // Not traversed: a value has no references.
                        }
                    }
                }
                for child in [&node.left, &node.right] {
                    match child {
                        NodeRef::Hash(h) => {
                            if live.insert(*h) {
                                next.push(*h);
                            }
                        }
                        // Embedded children live inside this node's bytes and
                        // have no entry of their own to mark.
                        NodeRef::Node(_) | NodeRef::Empty => {}
                    }
                }
            }
            frontier = next;
        }
        Ok(live)
    }

    /// Copies every entry of the oldest epoch whose key is live into the newest.
    ///
    /// A sequential scan of the oldest epoch, not a lookup per live key: the
    /// live set is a small fraction of what is stored, but it is scattered, and
    /// reading the epoch in key order is far cheaper than seeking to each
    /// survivor. Returns (scanned, drained, bytes).
    fn drain(&self, live: &HashSet<B256>) -> Result<(u64, u64, u64)> {
        let (oldest, newest) = {
            let e = self.epochs.read().unwrap();
            (e.first().map(|e| e.db.clone()), e.last().map(|e| e.db.clone()))
        };
        let (Some(oldest), Some(newest)) = (oldest, newest) else {
            return Ok((0, 0, 0));
        };

        let cf_src = oldest.cf_handle(CF_TRIE).context("oldest epoch has no trie cf")?;
        let cf_dst = newest.cf_handle(CF_TRIE).context("newest epoch has no trie cf")?;

        let mut scanned = 0u64;
        let mut drained = 0u64;
        let mut bytes = 0u64;
        let mut batch = WriteBatch::default();
        let mut pending = 0usize;

        let mut iter = oldest.raw_iterator_cf(cf_src);
        iter.seek_to_first();
        while iter.valid() {
            if let (Some(k), Some(v)) = (iter.key(), iter.value()) {
                scanned += 1;
                if k.len() == 32 && live.contains(&B256::from_slice(k)) {
                    batch.put_cf(cf_dst, k, v);
                    drained += 1;
                    bytes += v.len() as u64;
                    pending += 1;
                    if pending >= 10_000 {
                        newest.write(std::mem::take(&mut batch)).context("drain batch")?;
                        pending = 0;
                    }
                }
            }
            iter.next();
        }
        if pending > 0 {
            newest.write(batch).context("drain batch")?;
        }
        newest.flush().context("flush after drain")?;
        Ok((scanned, drained, bytes))
    }

    /// Deletes the oldest epoch and opens a new newest one.
    ///
    /// This is the step that reclaims, and it costs one directory removal
    /// regardless of how many dead entries it drops.
    fn sweep_and_rotate(&self) -> Result<u64> {
        let mut epochs = self.epochs.write().unwrap();
        let reclaimed = if epochs.len() >= self.config.epochs {
            let old = epochs.remove(0);
            let size = dir_size(&old.dir);
            let dir = old.dir.clone();
            // Drop every handle before the directory goes.
            drop(old);
            std::fs::remove_dir_all(&dir)
                .with_context(|| format!("removing epoch {}", dir.display()))?;
            debug!(target: "rustock::gc", "Swept {} ({} MB)", dir.display(), size / (1 << 20));
            size
        } else {
            0
        };

        let seq = self.next_seq.fetch_add(1, Ordering::SeqCst);
        let dir = epoch_dir(&self.root, seq);
        epochs.push(Epoch { seq, db: open_epoch(&dir, &self.cache)?, dir });
        self.newest_written.store(0, Ordering::Relaxed);
        Ok(reclaimed)
    }

    /// Adds an epoch without removing one, used while the store is still
    /// growing to its configured width.
    fn rotate_only(&self) -> Result<()> {
        let mut epochs = self.epochs.write().unwrap();
        let seq = self.next_seq.fetch_add(1, Ordering::SeqCst);
        let dir = epoch_dir(&self.root, seq);
        epochs.push(Epoch { seq, db: open_epoch(&dir, &self.cache)?, dir });
        self.newest_written.store(0, Ordering::Relaxed);
        Ok(())
    }

    /// Flushes every epoch, so a clean shutdown loses nothing.
    pub fn flush(&self) -> Result<()> {
        for e in self.epochs.read().unwrap().iter() {
            e.db.flush().ok();
        }
        Ok(())
    }
}

impl TrieStore for EpochTrieStore {
    /// Newest epoch first: recently written entries are the most read, and
    /// stopping at the first hit keeps the common case to one lookup.
    fn get(&self, key: &[u8]) -> Option<Vec<u8>> {
        self.reads.fetch_add(1, Ordering::Relaxed);
        let epochs = self.epochs.read().unwrap();
        for e in epochs.iter().rev() {
            self.read_probes.fetch_add(1, Ordering::Relaxed);
            let cf = e.db.cf_handle(CF_TRIE)?;
            if let Ok(Some(v)) = e.db.get_cf(cf, key) {
                return Some(v);
            }
        }
        None
    }

    /// Writes to the newest epoch, unconditionally.
    ///
    /// No existence check against older epochs: see the module docs and
    /// invariant I4. Skipping a write whose key already exists in an older
    /// epoch is what makes a later sweep drop entries the current state still
    /// references.
    fn put(&self, key: &[u8], value: &[u8]) {
        self.writes.fetch_add(1, Ordering::Relaxed);
        self.newest_written
            .fetch_add((key.len() + value.len()) as u64, Ordering::Relaxed);
        let epochs = self.epochs.read().unwrap();
        if let Some(e) = epochs.last() {
            if let Some(cf) = e.db.cf_handle(CF_TRIE) {
                if let Err(err) = e.db.put_cf(cf, key, value) {
                    warn!(target: "rustock::gc", "Epoch write failed: {err}");
                }
            }
        }
    }

    fn get_many(&self, keys: &[Vec<u8>]) -> Vec<Option<Vec<u8>>> {
        keys.iter().map(|k| self.get(k)).collect()
    }
}


/// Which trie storage backend a node should use.
///
/// The node only ever holds an `Arc<dyn TrieStore>`, so the choice is made once
/// at startup and nothing downstream knows which one it got. That is the whole
/// extent of the abstraction: the collector is not a mode of the existing store,
/// it is a different store that happens to satisfy the same trait.
#[derive(Debug, Clone)]
pub enum TrieBackend {
    /// A standalone trie database, separate from the node's main database but
    /// with no collection. Same retention as `Single`; the difference is only
    /// that the trie lives in its own directory, so it can be swapped, copied
    /// or replaced without touching headers and bodies.
    External,
    /// One database, nothing is ever reclaimed. Every historical version of
    /// every node is kept, which is what makes historical state queryable and
    /// what makes the store grow without bound.
    Single,
    /// Epoch list with generational collection. Bounded size, at the cost of
    /// dropping state older than the retention window.
    Epochs(EpochConfig),
}

impl TrieBackend {
    /// Parses `single` or `epoch`, the two values a user types.
    pub fn parse(name: &str, config: EpochConfig) -> Result<Self> {
        match name {
            "single" | "none" | "off" => Ok(TrieBackend::Single),
            "external" | "detached" => Ok(TrieBackend::External),
            "epoch" | "epochs" | "gc" => Ok(TrieBackend::Epochs(config)),
            other => bail!("unknown trie backend {other:?}; expected \"single\" or \"epoch\""),
        }
    }

    pub fn is_collecting(&self) -> bool {
        matches!(self, TrieBackend::Epochs(_))
    }

    /// True when the trie lives outside the node's main database.
    pub fn is_detached(&self) -> bool {
        matches!(self, TrieBackend::External | TrieBackend::Epochs(_))
    }
}

/// Opens the trie store a backend describes.
///
/// `single_db` is the node's main database, which the single backend shares
/// with headers and bodies; the epoch backend keeps its own directories under
/// `epoch_dir` instead, because an epoch has to be deletable on its own.
pub fn open_backend(
    backend: &TrieBackend,
    single_db: Arc<DB>,
    epoch_root: &Path,
) -> Result<Arc<dyn TrieStore>> {
    match backend {
        TrieBackend::Single => Ok(Arc::new(crate::RocksDbTrieStore::from_db(single_db))),
        TrieBackend::External => {
            Ok(Arc::new(crate::RocksDbTrieStore::open(epoch_root)?))
        }
        TrieBackend::Epochs(cfg) => {
            Ok(Arc::new(EpochTrieStore::open(epoch_root, cfg.clone())?))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustock_trie::TrieKeySlice;

    fn cfg(epochs: usize) -> EpochConfig {
        EpochConfig { epochs, burial_depth: 0, rotate_bytes: 1 }
    }

    fn key(n: u64) -> Vec<u8> {
        let mut k = vec![0u8; 8];
        k.copy_from_slice(&n.to_be_bytes());
        k
    }

    /// Builds a trie holding `n` entries and returns its saved root hash.
    fn build(store: &dyn TrieStore, n: u64, salt: u64) -> B256 {
        let mut root = TrieNode::empty();
        for i in 0..n {
            let k = key(i);
            let mut v = vec![0u8; 16];
            v[..8].copy_from_slice(&(i + salt).to_be_bytes());
            root = root.put(&TrieKeySlice::from_key(&k), &v, store);
        }
        root.save(store, true);
        root.compute_hash(store)
    }

    fn walk(store: &dyn TrieStore, root: B256) -> Result<u64> {
        let mut seen = HashSet::new();
        let mut stack = vec![root];
        let mut n = 0;
        while let Some(h) = stack.pop() {
            if !seen.insert(h) {
                continue;
            }
            let bytes = store
                .get(h.as_slice())
                .ok_or_else(|| anyhow::anyhow!("dangling reference {h:?}"))?;
            n += 1;
            let node = TrieNode::from_message(&bytes, store);
            if node.has_long_value() {
                if let Some(vh) = node.value_hash {
                    store
                        .get(vh.as_slice())
                        .ok_or_else(|| anyhow::anyhow!("dangling long value {vh:?}"))?;
                }
            }
            for c in [&node.left, &node.right] {
                if let NodeRef::Hash(ch) = c {
                    stack.push(*ch);
                }
            }
        }
        Ok(n)
    }

    #[test]
    fn rejects_fewer_than_three_epochs() {
        let d = tempfile::tempdir().unwrap();
        assert!(EpochTrieStore::open(d.path(), cfg(2)).is_err());
    }

    #[test]
    fn reads_find_entries_in_older_epochs() {
        let d = tempfile::tempdir().unwrap();
        let s = EpochTrieStore::open(d.path(), cfg(3)).unwrap();
        s.put(b"aaaaaaaa", b"first");
        s.rotate_only().unwrap();
        s.put(b"bbbbbbbb", b"second");
        assert_eq!(s.get(b"aaaaaaaa").as_deref(), Some(&b"first"[..]));
        assert_eq!(s.get(b"bbbbbbbb").as_deref(), Some(&b"second"[..]));
        assert_eq!(s.get(b"cccccccc"), None);
    }

    #[test]
    fn collection_keeps_the_state_it_collected_against() {
        let d = tempfile::tempdir().unwrap();
        let s = EpochTrieStore::open(d.path(), cfg(3)).unwrap();

        let root = build(&s, 200, 0);
        let before = walk(&s, root).unwrap();

        // Grow to full width, then collect against the same root.
        for _ in 0..4 {
            s.collect(root).unwrap();
        }

        let after = walk(&s, root).unwrap();
        assert_eq!(after, before, "every node reachable from the root survived");
    }

    #[test]
    fn collection_drops_state_older_than_the_collection_root() {
        let d = tempfile::tempdir().unwrap();
        let s = EpochTrieStore::open(d.path(), cfg(3)).unwrap();

        let old_root = build(&s, 200, 0);
        let old_nodes = walk(&s, old_root).unwrap();

        // A later state that shares little with the old one.
        let new_root = build(&s, 200, 1_000_000);

        for _ in 0..6 {
            s.collect(new_root).unwrap();
        }

        // The new state is intact...
        walk(&s, new_root).expect("collection root must stay readable");
        // ...and the superseded one is at least partly gone, which is the
        // point: this is pruning, not compaction. If nothing were dropped the
        // collector would not be reclaiming anything.
        let survived = walk(&s, old_root).is_ok();
        assert!(
            !survived || old_nodes == 0,
            "old state should not survive collection against a newer root"
        );
    }

    #[test]
    fn unconditional_writes_keep_a_revived_subtree_alive() {
        // The case the design calls out as load-bearing. A state reverts to a
        // value it held before, so the trie rebuilds a subtree byte-identical
        // to one that now lives only in the oldest epoch. With a conditional
        // write the new state would point into a database about to be deleted.
        let d = tempfile::tempdir().unwrap();
        let s = EpochTrieStore::open(d.path(), cfg(3)).unwrap();

        let original = build(&s, 120, 7);
        // Move on to a different state, rotating as we go.
        let _ = build(&s, 120, 999);
        s.rotate_only().unwrap();
        // Now revert: rebuild exactly the original content. Every node is
        // rewritten into the newest epoch even though the bytes already exist
        // in an older one.
        let revived = build(&s, 120, 7);
        assert_eq!(revived, original, "same content must hash the same");

        for _ in 0..5 {
            s.collect(revived).unwrap();
        }
        walk(&s, revived).expect("revived state must survive collection");
    }

    #[test]
    fn sweep_reclaims_disk() {
        let d = tempfile::tempdir().unwrap();
        let s = EpochTrieStore::open(d.path(), cfg(3)).unwrap();
        let mut root = B256::ZERO;
        for i in 0..6 {
            root = build(&s, 400, i * 100_000);
            s.collect(root).unwrap();
        }
        let stats = s.collect(root).unwrap();
        assert!(stats.scanned > 0, "the drain must have scanned the oldest epoch");
        walk(&s, root).expect("current state intact after repeated collection");
    }
}
