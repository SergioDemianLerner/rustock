//! FlatDB — a store for data whose key *is* the hash of its value.
//!
//! A Rust port of the `flatdb-simplified` branch of rskj
//! (`co.rsk.flatdb`, commit `8b4f94a4c`), written against
//! `/home/paseo/flatdb/FlatDB-Rust-Port.md`.
//!
//! This is **not** a general key/value store and must not be used as one.
//! An in-RAM hash [`Index`](index::Index) of heap offsets sits over an
//! append-only, memory-mapped [`Heap`](heap::Heap) of records. Because keys
//! are content addresses the index stores **no key bytes at all**: a probe
//! resolves a collision by fetching the candidate and recomputing
//! `H(value)`. That costs sixteen bytes of RAM per entry and makes every
//! successful short-record read a verification of the bytes against their
//! own address.
//!
//! Everything structural follows from that plus three facts about the
//! workload: no deletions, no range queries, no mutation. A design choice
//! here that would also suit a general KV store is probably wrong.
//!
//! # Durability
//!
//! Writes are visible at `put` and durable at `flush`, which is three
//! ordered steps: force the heap, force the index, write the
//! [`Checkpoint`]. The checkpoint is the only thing that makes a batch
//! real. A crash before step 3 leaves heap bytes and index slots that the
//! next startup ignores, because the restored checkpoint puts the frontier
//! below them — such slots read as a miss and the next insertion reclaims
//! them. There is no log, no undo and no recovery scan.
//!
//! # Concurrency
//!
//! Readers take no exclusive lock and allocate only for the record they
//! match. Writers serialise against each other on one mutex and publish
//! each record with a release store to its slot, which readers acquire.
//! That pairing is what the Java version's deleted `experimentalConcurrentRW`
//! was missing, and it is why slots are eight bytes here rather than five.
//!
//! # What this deliberately does not do
//!
//! No deletions, no null values, no range queries, no read snapshots, no
//! isolation between batches. Space is reclaimed by discarding a whole
//! database. Every one of those omissions is what pays for the structure
//! above.

pub mod checkpoint;
pub mod heap;
pub mod index;
pub mod lock;

pub use checkpoint::Checkpoint;
pub use heap::Heap;
pub use index::{Index, Keys, STORE_KEY_THRESHOLD};

use sha3::{Digest, Keccak256};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

/// Keys are 32-byte Keccak digests of their values.
pub struct KeccakKeys;

impl Keys for KeccakKeys {
    fn compute_key(&self, data: &[u8]) -> Vec<u8> {
        Keccak256::digest(data).to_vec()
    }

    /// The last four bytes, as rskj does it. The key is already a uniform
    /// digest, so any four bytes are as good as a hash function — adding a
    /// second one would only cost time.
    fn hash(&self, key: &[u8]) -> u32 {
        let n = key.len();
        u32::from_be_bytes([key[n - 4], key[n - 3], key[n - 2], key[n - 1]])
    }

    fn length(&self) -> usize {
        32
    }
}

/// How to open a [`FlatDb`].
#[derive(Debug, Clone)]
pub struct Options {
    /// Sizes the index up front. A hint, not a limit: exceeding it costs a
    /// rehash rather than a failure.
    pub expected_keys: u64,
    pub read_only: bool,
    /// Flush after every put and batch, trading throughput for durability.
    pub sync_writes: bool,
    /// Bytes per heap segment. Part of the on-disk format: a database
    /// records the size it was written with and refuses to open under a
    /// different one, because every offset would resolve to the wrong
    /// bytes. Must be a power of two.
    pub segment_size: u64,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            expected_keys: 16_000_000,
            read_only: false,
            sync_writes: false,
            segment_size: heap::DEFAULT_SEGMENT_SIZE,
        }
    }
}

pub struct FlatDb {
    dir: PathBuf,
    read_only: bool,
    sync_writes: bool,
    heap: Heap,
    index: Index,
    keys: KeccakKeys,
    _lock: Option<lock::DbLock>,
    /// Serialises writers against each other and against a flush. Readers
    /// never take it.
    writer: Mutex<WriterState>,
    /// Heap size at the last completed batch: what a flush makes durable.
    committed_heap_size: AtomicU64,
    gets: AtomicU64,
    misses: AtomicU64,
    puts: AtomicU64,
    /// Set by `simulate_crash`, so the drop does not helpfully flush away
    /// the very thing a crash test is trying to observe.
    crashed: std::sync::atomic::AtomicBool,
}

struct WriterState {
    checkpoint: Checkpoint,
}

impl FlatDb {
    pub fn open(dir: impl AsRef<Path>, options: Options) -> io::Result<Self> {
        let dir = dir.as_ref().to_path_buf();
        std::fs::create_dir_all(&dir)?;
        let lock = if options.read_only {
            None
        } else {
            Some(lock::DbLock::acquire(&dir.join("lock.dat"))?)
        };

        let checkpoint = Checkpoint::read(&dir)?;
        if checkpoint.segment_size != 0 && checkpoint.segment_size != options.segment_size {
            return Err(io::Error::other(format!(
                "{} was written with {}-byte heap segments and was asked for {}; \
                 every offset would resolve to the wrong bytes",
                dir.display(),
                checkpoint.segment_size,
                options.segment_size
            )));
        }
        let heap = Heap::open(
            &dir,
            options.read_only,
            checkpoint.heap_committed_size,
            options.segment_size,
        )?;
        let index = Index::open(
            &dir,
            options.read_only,
            checkpoint.index_capacity,
            checkpoint.index_size,
            options.expected_keys,
        )?;
        // Index files other than the live one are orphans from a grow that
        // no checkpoint adopted.
        index.remove_stale_files()?;

        tracing::info!(
            target: "rustock::flatdb",
            "opened {} ({:?}, {} entries, {} slots)",
            dir.display(), checkpoint, index.size(), index.capacity()
        );

        Ok(Self {
            dir,
            read_only: options.read_only,
            sync_writes: options.sync_writes,
            heap,
            index,
            keys: KeccakKeys,
            _lock: lock,
            writer: Mutex::new(WriterState { checkpoint }),
            committed_heap_size: AtomicU64::new(checkpoint.heap_committed_size),
            gets: AtomicU64::new(0),
            misses: AtomicU64::new(0),
            puts: AtomicU64::new(0),
            crashed: std::sync::atomic::AtomicBool::new(false),
        })
    }

    fn check_writable(&self) -> io::Result<()> {
        if self.read_only {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "database opened read-only",
            ));
        }
        Ok(())
    }

    pub fn get(&self, key: &[u8]) -> Option<Vec<u8>> {
        self.gets.fetch_add(1, Ordering::Relaxed);
        let found = self.index.get(&self.heap, &self.keys, key);
        if found.is_none() {
            self.misses.fetch_add(1, Ordering::Relaxed);
        }
        found
    }

    pub fn put(&self, key: &[u8], value: &[u8]) -> io::Result<()> {
        self.check_writable()?;
        {
            let _guard = self.writer.lock().expect("flatdb writer");
            self.index.put(&self.heap, &self.keys, key, value)?;
            self.puts.fetch_add(1, Ordering::Relaxed);
            self.committed_heap_size.store(self.heap.size(), Ordering::Release);
        }
        if self.sync_writes {
            self.flush()?;
        }
        Ok(())
    }

    /// Applies every row, then advances the watermark once. Until `flush`
    /// the batch is visible but not durable; a crash in between discards it
    /// whole.
    pub fn update_batch<'a>(
        &self,
        rows: impl IntoIterator<Item = (&'a [u8], &'a [u8])>,
    ) -> io::Result<()> {
        self.check_writable()?;
        {
            let _guard = self.writer.lock().expect("flatdb writer");
            for (key, value) in rows {
                self.index.put(&self.heap, &self.keys, key, value)?;
                self.puts.fetch_add(1, Ordering::Relaxed);
            }
            self.committed_heap_size.store(self.heap.size(), Ordering::Release);
        }
        if self.sync_writes {
            self.flush()?;
        }
        Ok(())
    }

    /// Makes everything committed so far durable.
    ///
    /// Takes the writer mutex, so it excludes other writers but no readers
    /// — which is the property the original design was proudest of and the
    /// one this port keeps.
    pub fn flush(&self) -> io::Result<()> {
        if self.read_only {
            return Ok(());
        }
        let mut guard = self.writer.lock().expect("flatdb writer");
        let heap_size = self.committed_heap_size.load(Ordering::Acquire);
        let capacity = self.index.capacity();
        let size = self.index.size();
        if heap_size == guard.checkpoint.heap_committed_size && !self.index.modified() {
            return Ok(());
        }
        self.heap.force()?;
        self.index.save()?;
        let next = guard.checkpoint.next(heap_size, capacity, size, self.heap.segment_size());
        next.write(&self.dir)?;
        guard.checkpoint = next;
        // Only now is the new capacity the live one, so only now may the
        // table it replaced be removed.
        self.index.remove_stale_files()?;
        Ok(())
    }

    /// Every live key, each once.
    ///
    /// Deduplicated because two slots can point at one record after a
    /// stale slot has been resurrected — see the note in [`index`]. rskj's
    /// `keys()` returns a `Set` and hides the same thing.
    pub fn keys(&self) -> std::collections::HashSet<Vec<u8>> {
        let mut out = std::collections::HashSet::new();
        self.index.for_each_key(&self.heap, &self.keys, |k| {
            out.insert(k);
        });
        out
    }

    pub fn exists(dir: impl AsRef<Path>) -> bool {
        dir.as_ref().join(checkpoint::FILE_NAME).exists()
    }

    pub fn entries(&self) -> u64 {
        self.index.size()
    }

    pub fn stats(&self) -> Vec<String> {
        let capacity = self.index.capacity();
        let size = self.index.size();
        vec![
            format!("entries: {size}"),
            format!("index slots: {capacity}"),
            format!(
                "index load: {}%",
                if capacity == 0 { 0 } else { 100 * size / capacity }
            ),
            format!("index RAM [MB]: {}", capacity * index::SLOT_SIZE / 1_000_000),
            format!("heap [MB]: {}", self.heap.size() / 1_000_000),
            format!("heap segments: {}", self.heap.segment_count()),
            format!(
                "gets: {} (misses: {})",
                self.gets.load(Ordering::Relaxed),
                self.misses.load(Ordering::Relaxed)
            ),
            format!("puts: {}", self.puts.load(Ordering::Relaxed)),
        ]
    }

    /// Drops everything that was not checkpointed, as losing the process
    /// would. Test hook; the real path is `flush` then drop.
    ///
    /// Drop the value afterwards to release the directory lock, exactly as
    /// a dying process would.
    #[doc(hidden)]
    pub fn simulate_crash(&self) {
        self.crashed.store(true, Ordering::Release);
        let committed =
            self.writer.lock().expect("flatdb writer").checkpoint.heap_committed_size;
        self.heap.truncate_to(committed);
    }
}

impl Drop for FlatDb {
    fn drop(&mut self) {
        if !self.read_only && !self.crashed.load(Ordering::Acquire) {
            if let Err(e) = self.flush() {
                tracing::warn!(target: "rustock::flatdb", "flush on close failed: {e}");
            }
        }
    }
}

#[cfg(test)]
mod tests;
