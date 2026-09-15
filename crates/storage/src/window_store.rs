//! Sliding-window trie store: the write path of a range-segmented build.
//!
//! A build worker replaying a block range keeps the nodes it has recently
//! touched in memory, and seals them to disk in block-ranged segments as it
//! moves on. Every node **read or written** goes into the newest in-memory
//! table, which is what makes a sealed segment self-contained for its range:
//! replaying those blocks later reads only what that segment holds.
//!
//! Lookup order is newest-first -- in-memory tables, then sealed segments
//! newest-first, then a fallback store (the archival trie). Measured on
//! mainnet (`docs/trie-segments-design.md` §6.10) a 512 MB window leaves ~3.3%
//! of reads to fall through, and §6.11 is why those should land on a dense
//! segment rather than the 131 GB archive.

use crate::RocksDbTrieStore;
use anyhow::{Context, Result};
use rustock_trie::TrieStore;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

pub struct WindowConfig {
    /// Bytes accumulated in the newest table before it is closed and a new one
    /// started. This is the sealed segment size.
    pub seal_bytes: u64,
    /// In-memory tables, newest included. The resident window is
    /// `tables * seal_bytes`.
    pub tables: usize,
    /// Where sealed segments are written.
    pub dir: PathBuf,
}

/// An in-memory table, stored as one arena plus an index into it.
///
/// The obvious `HashMap<Vec<u8>, Vec<u8>>` costs two heap allocations and a
/// slot per node, which measured at **7x** the bytes it holds: four workers
/// used 4.95 GB of RAM for 815 MB of trie nodes and drove a 7.7 GB machine to
/// 94% swap. Values live in one contiguous `Vec<u8>` and the index holds
/// 32-byte keys against `(offset, len)`, which brings the overhead to about
/// 1.1x and lets the window be sized by the miss-rate measurement (§6.10)
/// rather than by what fits.
///
/// Keys stay full width. An 8-byte prefix would be smaller, but a collision
/// returns the wrong trie node, which is a consensus fault rather than a
/// performance one.
struct Table {
    /// First block whose execution wrote into this table.
    start_block: u64,
    bytes: u64,
    arena: Vec<u8>,
    index: HashMap<[u8; 32], (u32, u32)>,
    /// Keys that are not 32 bytes. Trie keys are hashes, so this stays empty
    /// outside tests; it exists so a non-hash key cannot be silently dropped.
    overflow: HashMap<Vec<u8>, Vec<u8>>,
}

impl Table {
    fn new(start_block: u64, reserve: usize) -> Self {
        let mut arena = Vec::new();
        // Reserve up front: doubling a 128 MB arena wastes more than the
        // saving this whole change is about.
        arena.reserve(reserve);
        Self { start_block, bytes: 0, arena, index: HashMap::new(), overflow: HashMap::new() }
    }

    fn get(&self, key: &[u8]) -> Option<Vec<u8>> {
        if key.len() == 32 {
            let k: [u8; 32] = key.try_into().expect("checked");
            self.index.get(&k).map(|(off, len)| {
                self.arena[*off as usize..*off as usize + *len as usize].to_vec()
            })
        } else {
            self.overflow.get(key).cloned()
        }
    }

    /// Trie nodes are content-addressed, so a key already present has the same
    /// value: re-inserting would only waste arena.
    fn insert(&mut self, key: &[u8], value: &[u8]) {
        if key.len() == 32 {
            let k: [u8; 32] = key.try_into().expect("checked");
            if self.index.contains_key(&k) {
                return;
            }
            let off = self.arena.len() as u32;
            self.arena.extend_from_slice(value);
            self.index.insert(k, (off, value.len() as u32));
        } else {
            if self.overflow.contains_key(key) {
                return;
            }
            self.overflow.insert(key.to_vec(), value.to_vec());
        }
        self.bytes += (key.len() + value.len()) as u64;
    }

    fn is_empty(&self) -> bool {
        self.index.is_empty() && self.overflow.is_empty()
    }

    fn drain_into(&self, store: &RocksDbTrieStore) {
        for (k, (off, len)) in &self.index {
            store.put(k, &self.arena[*off as usize..*off as usize + *len as usize]);
        }
        for (k, v) in &self.overflow {
            store.put(k, v);
        }
    }
}

/// Where a segment boundary fell, recorded so the sealed store can later be
/// split into range-scoped segments, or routed by height without one.
pub struct Boundary {
    pub seq: u64,
    pub start_block: u64,
    pub bytes: u64,
}

pub struct WindowStore {
    config: WindowConfig,
    /// Oldest first, newest last. Writes go to the last.
    tables: RwLock<Vec<Table>>,
    /// One database per worker, not one per segment.
    ///
    /// Segments are cut every `seal_bytes`, so a whole-chain range produces
    /// hundreds of them; keeping a RocksDB open per segment would exhaust file
    /// handles, and a miss would have to probe every one. It also has to stay
    /// correct for the range starting at genesis, where the archival fallback
    /// holds nothing and every read not in memory *must* be found here.
    ///
    /// Density -- the property that makes a segment worth having -- is a
    /// property of the content, not the file: this holds only what one worker
    /// touched. The boundaries are recorded so it can be split afterwards.
    sealed: RwLock<Option<RocksDbTrieStore>>,
    boundaries: RwLock<Vec<Boundary>>,
    fallback: Option<Arc<dyn TrieStore>>,
    block: AtomicU64,
    next_seq: AtomicU64,
    pub reads: AtomicU64,
    pub hits_memory: AtomicU64,
    pub hits_sealed: AtomicU64,
    pub hits_fallback: AtomicU64,
    pub misses: AtomicU64,
    pub writes: AtomicU64,
    pub sealed_bytes: AtomicU64,
}

impl WindowStore {
    pub fn new(config: WindowConfig, fallback: Option<Arc<dyn TrieStore>>) -> Result<Self> {
        std::fs::create_dir_all(&config.dir)
            .with_context(|| format!("creating {}", config.dir.display()))?;
        let config_reserve = config.seal_bytes as usize;
        Ok(Self {
            tables: RwLock::new(vec![Table::new(0, config_reserve)]),
            sealed: RwLock::new(None),
            boundaries: RwLock::new(Vec::new()),
            config,
            fallback,
            block: AtomicU64::new(0),
            next_seq: AtomicU64::new(0),
            reads: AtomicU64::new(0),
            hits_memory: AtomicU64::new(0),
            hits_sealed: AtomicU64::new(0),
            hits_fallback: AtomicU64::new(0),
            misses: AtomicU64::new(0),
            writes: AtomicU64::new(0),
            sealed_bytes: AtomicU64::new(0),
        })
    }

    /// Tell the store which block is executing. Segment boundaries are recorded
    /// in block numbers, so a later reader can route by height.
    pub fn set_block(&self, n: u64) {
        self.block.store(n, Ordering::Relaxed);
        if self.tables.read().unwrap().last().map_or(false, |t| t.bytes == 0) {
            if let Some(t) = self.tables.write().unwrap().last_mut() {
                if t.bytes == 0 {
                    t.start_block = n;
                }
            }
        }
    }

    pub fn segment_count(&self) -> usize {
        self.boundaries.read().unwrap().len()
    }

    /// Open (or create) the worker's sealed database.
    fn sealed_store(&self) -> Result<()> {
        let mut guard = self.sealed.write().unwrap();
        if guard.is_none() {
            let path = self.config.dir.join("sealed");
            *guard = Some(RocksDbTrieStore::open(&path)
                .with_context(|| format!("opening sealed store {}", path.display()))?);
        }
        Ok(())
    }

    pub fn resident_bytes(&self) -> u64 {
        self.tables.read().unwrap().iter().map(|t| t.bytes).sum()
    }

    /// Insert into the newest table, rotating and sealing when it is full.
    fn insert(&self, key: &[u8], value: &[u8]) {
        let mut tables = self.tables.write().unwrap();
        let newest = tables.last_mut().expect("at least one table");
        newest.insert(key, value);
        if newest.bytes < self.config.seal_bytes {
            return;
        }
        // Rotate: a fresh table becomes newest; if that takes us over the
        // resident limit, the oldest is written out and dropped.
        let start = self.block.load(Ordering::Relaxed);
        let reserve = self.config.seal_bytes as usize;
        tables.push(Table::new(start, reserve));
        if tables.len() > self.config.tables {
            let old = tables.remove(0);
            drop(tables);
            if let Err(e) = self.seal(old) {
                tracing::error!(target: "rustock::window", "sealing segment failed: {e:?}");
            }
        }
    }

    fn seal(&self, table: Table) -> Result<()> {
        self.sealed_store()?;
        let seq = self.next_seq.fetch_add(1, Ordering::Relaxed);
        {
            let guard = self.sealed.read().unwrap();
            let store = guard.as_ref().expect("sealed store opened above");
            table.drain_into(store);
            store.flush();
        }
        self.sealed_bytes.fetch_add(table.bytes, Ordering::Relaxed);
        self.boundaries.write().unwrap().push(Boundary {
            seq,
            start_block: table.start_block,
            bytes: table.bytes,
        });
        self.write_manifest();
        Ok(())
    }

    /// Rewritten on every seal, so a killed worker still leaves a usable record
    /// of where its segments begin.
    fn write_manifest(&self) {
        let b = self.boundaries.read().unwrap();
        let mut out = String::from("seq,start_block,bytes\n");
        for e in b.iter() {
            out.push_str(&format!("{},{},{}\n", e.seq, e.start_block, e.bytes));
        }
        let path = self.config.dir.join("segments.csv");
        let tmp = self.config.dir.join("segments.csv.tmp");
        if std::fs::write(&tmp, out).is_ok() {
            let _ = std::fs::rename(&tmp, &path);
        }
    }

    /// Seal the resident window and record where execution had reached, so a
    /// worker that exits can pick up from here.
    ///
    /// Sealing is not optional at a checkpoint. The nodes in memory are the
    /// only copy for a range built from genesis -- the archival fallback holds
    /// nothing below #1,591,000 -- so a checkpoint that recorded a block
    /// without flushing them would produce a resume that cannot read its own
    /// recent past.
    pub fn checkpoint(&self, block: u64, state_root: &[u8]) -> Result<()> {
        self.finish()?;
        if let Some(store) = self.sealed.read().unwrap().as_ref() {
            store.flush();
        }
        let body = format!("last_block={block}\nstate_root={}\n", hex_lower(state_root));
        let path = self.config.dir.join("checkpoint");
        let tmp = self.config.dir.join("checkpoint.tmp");
        std::fs::write(&tmp, body).context("writing checkpoint")?;
        std::fs::rename(&tmp, &path).context("installing checkpoint")?;
        Ok(())
    }

    /// `(last_block, state_root)` from a previous run, if any.
    pub fn read_checkpoint(dir: &Path) -> Option<(u64, Vec<u8>)> {
        let text = std::fs::read_to_string(dir.join("checkpoint")).ok()?;
        let mut block = None;
        let mut root = None;
        for line in text.lines() {
            match line.split_once('=') {
                Some(("last_block", v)) => block = v.trim().parse::<u64>().ok(),
                Some(("state_root", v)) => root = hex_decode(v.trim()),
                _ => {}
            }
        }
        Some((block?, root?))
    }

    /// Write everything still resident, so a finished range leaves no nodes
    /// only in memory.
    pub fn finish(&self) -> Result<()> {
        let mut tables = self.tables.write().unwrap();
        let drained: Vec<Table> = tables.drain(..).collect();
        let reserve = self.config.seal_bytes as usize;
        tables.push(Table::new(self.block.load(Ordering::Relaxed), reserve));
        drop(tables);
        for t in drained {
            if !t.is_empty() {
                self.seal(t)?;
            }
        }
        Ok(())
    }
}

fn hex_lower(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn hex_decode(s: &str) -> Option<Vec<u8>> {
    let s = s.strip_prefix("0x").unwrap_or(s);
    if s.len() % 2 != 0 {
        return None;
    }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok()).collect()
}

impl TrieStore for WindowStore {
    fn get(&self, key: &[u8]) -> Option<Vec<u8>> {
        self.reads.fetch_add(1, Ordering::Relaxed);
        {
            let tables = self.tables.read().unwrap();
            // Newest first: a node rewritten recently is the one wanted, and
            // the newest copy is always at least as fresh.
            for (i, t) in tables.iter().enumerate().rev() {
                if let Some(v) = t.get(key) {
                    self.hits_memory.fetch_add(1, Ordering::Relaxed);
                    if i + 1 != tables.len() {
                        drop(tables);
                        // Copy forward, or this segment will not be
                        // self-contained for its own block range.
                        self.insert(key, &v);
                    }
                    return Some(v);
                }
            }
        }
        {
            let sealed = self.sealed.read().unwrap();
            if let Some(store) = sealed.as_ref() {
                if let Some(v) = store.get(key) {
                    self.hits_sealed.fetch_add(1, Ordering::Relaxed);
                    drop(sealed);
                    self.insert(key, &v);
                    return Some(v);
                }
            }
        }
        if let Some(fb) = &self.fallback {
            if let Some(v) = fb.get(key) {
                self.hits_fallback.fetch_add(1, Ordering::Relaxed);
                self.insert(key, &v);
                return Some(v);
            }
        }
        self.misses.fetch_add(1, Ordering::Relaxed);
        None
    }

    fn put(&self, key: &[u8], value: &[u8]) {
        self.writes.fetch_add(1, Ordering::Relaxed);
        self.insert(key, value);
    }
}
