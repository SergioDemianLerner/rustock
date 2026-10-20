//! What one epoch's storage has to be able to do.
//!
//! The epoch store's own logic — rotation, burial, the live-set sweep, the
//! coherence invariants — has nothing to do with RocksDB. It became
//! RocksDB-shaped only because `epoch_store.rs` calls `cf_handle`,
//! `get_cf`, `put_cf`, `raw_iterator_cf` and `WriteBatch` directly, so
//! there was no seam at which anything else could be substituted.
//!
//! This is that seam, and it is deliberately narrow: everything the epoch
//! store actually asks of an epoch, and nothing a general key/value store
//! might also offer. A backend that cannot delete, cannot iterate in key
//! order and cannot read a snapshot is still a perfectly good epoch,
//! because the epoch model reclaims space by **discarding a whole
//! database** rather than by collecting inside one.
//!
//! See `docs/flatdb.md` for why that matters: it is the property that lets
//! an append-only, content-addressed store serve as an epoch at all.

use anyhow::{Context, Result};
use rocksdb::{ColumnFamilyDescriptor, Options, WriteBatch, DB};
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub const CF_TRIE: &str = "trie_nodes";

/// Storage for one epoch's trie nodes.
///
/// Implementations are shared across threads and must make reads safe
/// alongside writes; the epoch store reads from every epoch while writing
/// to the newest.
pub trait EpochBackend: Send + Sync {
    fn get(&self, key: &[u8]) -> Option<Vec<u8>>;

    fn put(&self, key: &[u8], value: &[u8]) -> Result<()>;

    /// Insert many entries at once. Used by the sweep, which copies live
    /// entries out of the oldest epoch in bulk; a backend that can batch
    /// should.
    fn put_many(&self, entries: &[(Vec<u8>, Vec<u8>)]) -> Result<()>;

    /// Remove a key.
    ///
    /// Not part of the epoch model — nothing in the production path calls
    /// it, and FlatDB cannot implement it at all. It exists so tests can
    /// stage a missing node, which is how the sweep's behaviour against a
    /// damaged epoch is exercised. A backend that cannot delete should say
    /// so with an error rather than succeed silently.
    fn delete(&self, key: &[u8]) -> Result<()>;

    fn flush(&self) -> Result<()>;

    /// True when this epoch holds no entries at all. Drives the decision
    /// to seed a freshly created store rather than rotate into it.
    fn is_empty(&self) -> bool;

    /// Visit every entry, in whatever order the backend finds cheapest.
    ///
    /// The sweep scans the oldest epoch end to end rather than looking up
    /// each live key, because the live set is a small fraction of what is
    /// stored but is scattered across it. Order is not relied upon.
    fn for_each(&self, visit: &mut dyn FnMut(&[u8], &[u8])) -> Result<()>;

    /// Bytes this epoch occupies, for the rotation policy.
    ///
    /// Not necessarily the size of its directory. A backend that
    /// preallocates — FlatDB reserves whole heap segments up front and
    /// sizes its index for a key count it has not reached — would report a
    /// figure that has nothing to do with how much state it holds, and
    /// `rotate_bytes` would fire on an empty epoch. Each backend reports
    /// the quantity the policy means.
    fn size_bytes(&self) -> u64;
}

/// Opens epochs. Holds whatever the backend shares between them — for
/// RocksDB, one block cache across every epoch.
pub trait EpochBackendFactory: Send + Sync {
    fn open(&self, dir: &Path) -> Result<Arc<dyn EpochBackend>>;

    /// Open without taking a lock and without writing anything, not even a
    /// log replay. For benchmarks and diagnostics that must not be able to
    /// damage a live store.
    fn open_read_only(&self, dir: &Path) -> Result<Arc<dyn EpochBackend>>;

    /// Appears in logs and in the error a node gets when the configured
    /// backend does not match what is on disk.
    fn name(&self) -> &'static str;
}

/// Which storage engine holds the nodes.
///
/// Orthogonal to [`TrieBackend`](crate::epoch_store::TrieBackend), which
/// chooses the retention topology: single database, external directory, or
/// epochs with collection. The same epoch policy runs over either engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrieEngine {
    RocksDb,
    FlatDb,
}

impl TrieEngine {
    pub fn parse(name: &str) -> Result<Self> {
        match name {
            "rocksdb" | "rocks" | "default" => Ok(TrieEngine::RocksDb),
            "flatdb" | "flat" => Ok(TrieEngine::FlatDb),
            other => anyhow::bail!(
                "unknown trie engine {other:?}; expected \"rocksdb\" or \"flatdb\""
            ),
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            TrieEngine::RocksDb => "rocksdb",
            TrieEngine::FlatDb => "flatdb",
        }
    }

    pub fn factory(&self, expected_keys: u64) -> Arc<dyn EpochBackendFactory> {
        match self {
            TrieEngine::RocksDb => Arc::new(RocksFactory::new(BLOCK_CACHE_BYTES)),
            TrieEngine::FlatDb => Arc::new(FlatFactory::new(expected_keys)),
        }
    }

    /// What engine, if any, already wrote this directory.
    ///
    /// Each leaves an unmistakable file: FlatDB a `checkpoint.dat`, RocksDB
    /// a `CURRENT`. A directory that is empty or holds neither returns
    /// `None`, which is a store that has not been created yet.
    ///
    /// This exists so that a misconfigured node fails at startup rather
    /// than opening an empty store beside a full one and resyncing from
    /// genesis with the old data still on disk.
    pub fn detect(dir: &Path) -> Option<Self> {
        if dir.join(rustock_flatdb::checkpoint::FILE_NAME).exists() {
            return Some(TrieEngine::FlatDb);
        }
        if dir.join("CURRENT").exists() {
            return Some(TrieEngine::RocksDb);
        }
        None
    }

    /// Refuse to open a directory written by a different engine.
    ///
    /// For an epoch store the probe is the oldest epoch directory, since
    /// the root holds only the per-epoch subdirectories.
    pub fn check_matches(&self, dir: &Path) -> Result<()> {
        let probe = first_epoch_dir(dir).unwrap_or_else(|| dir.to_path_buf());
        match Self::detect(&probe) {
            Some(found) if found != *self => anyhow::bail!(
                "{} was written by the {} trie engine and the node is configured for {}; \
                 opening it would create an empty store beside the existing data",
                probe.display(),
                found.name(),
                self.name()
            ),
            _ => Ok(()),
        }
    }
}

/// The lowest-numbered `epoch-N` directory, if the path is an epoch root.
fn first_epoch_dir(root: &Path) -> Option<PathBuf> {
    let mut found: Option<(u64, PathBuf)> = None;
    for entry in std::fs::read_dir(root).ok()?.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if let Some(seq) = name.strip_prefix("epoch-").and_then(|s| s.parse::<u64>().ok()) {
            if found.as_ref().is_none_or(|(best, _)| seq < *best) {
                found = Some((seq, entry.path()));
            }
        }
    }
    found.map(|(_, path)| path)
}

// ---------------------------------------------------------------- RocksDB

/// Block cache shared by every epoch.
///
/// One cache, not one per epoch: the newest epoch takes nearly all the
/// reads and should be free to use nearly all the cache. Per-epoch caches
/// would reserve memory for epochs that are read once a sweep.
pub const BLOCK_CACHE_BYTES: usize = 256 * 1024 * 1024;

pub struct RocksFactory {
    cache: rocksdb::Cache,
}

impl RocksFactory {
    pub fn new(cache_bytes: usize) -> Self {
        Self { cache: rocksdb::Cache::new_lru_cache(cache_bytes) }
    }

    fn db_options() -> Options {
        let mut opts = Options::default();
        opts.create_if_missing(true);
        opts.create_missing_column_families(true);
        opts
    }

    fn cf_options(&self) -> Options {
        let mut block = rocksdb::BlockBasedOptions::default();
        // Ten bits a key, filters outside the data cache.
        block.set_bloom_filter(10.0, false);
        block.set_block_cache(&self.cache);
        let mut opts = Options::default();
        opts.set_block_based_table_factory(&block);
        opts
    }
}

impl Default for RocksFactory {
    fn default() -> Self {
        Self::new(BLOCK_CACHE_BYTES)
    }
}

impl EpochBackendFactory for RocksFactory {
    fn open(&self, dir: &Path) -> Result<Arc<dyn EpochBackend>> {
        let db = DB::open_cf_descriptors(
            &Self::db_options(),
            dir,
            vec![ColumnFamilyDescriptor::new(CF_TRIE, self.cf_options())],
        )
        .with_context(|| format!("opening epoch at {}", dir.display()))?;
        Ok(Arc::new(RocksEpoch { db: Arc::new(db), dir: dir.to_path_buf(), read_only: false }))
    }

    fn open_read_only(&self, dir: &Path) -> Result<Arc<dyn EpochBackend>> {
        let db = DB::open_cf_descriptors_read_only(
            &Self::db_options(),
            dir,
            vec![ColumnFamilyDescriptor::new(CF_TRIE, self.cf_options())],
            false,
        )
        .with_context(|| format!("opening epoch read-only at {}", dir.display()))?;
        Ok(Arc::new(RocksEpoch { db: Arc::new(db), dir: dir.to_path_buf(), read_only: true }))
    }

    fn name(&self) -> &'static str {
        "rocksdb"
    }
}

pub struct RocksEpoch {
    db: Arc<DB>,
    dir: PathBuf,
    read_only: bool,
}

impl RocksEpoch {
    /// The underlying handle, for the paths that still need it.
    pub fn db(&self) -> &Arc<DB> {
        &self.db
    }

    fn cf(&self) -> Result<&rocksdb::ColumnFamily> {
        self.db
            .cf_handle(CF_TRIE)
            .with_context(|| format!("epoch at {} has no trie column family", self.dir.display()))
    }
}

impl EpochBackend for RocksEpoch {
    fn get(&self, key: &[u8]) -> Option<Vec<u8>> {
        let cf = self.db.cf_handle(CF_TRIE)?;
        self.db.get_cf(cf, key).ok().flatten()
    }

    fn put(&self, key: &[u8], value: &[u8]) -> Result<()> {
        let cf = self.cf()?;
        self.db.put_cf(cf, key, value).context("epoch write")
    }

    fn put_many(&self, entries: &[(Vec<u8>, Vec<u8>)]) -> Result<()> {
        if entries.is_empty() {
            return Ok(());
        }
        let cf = self.cf()?;
        let mut batch = WriteBatch::default();
        for (key, value) in entries {
            batch.put_cf(cf, key, value);
        }
        self.db.write(batch).context("epoch batch write")
    }

    fn delete(&self, key: &[u8]) -> Result<()> {
        let cf = self.cf()?;
        self.db.delete_cf(cf, key).context("epoch delete")
    }

    fn flush(&self) -> Result<()> {
        if self.read_only {
            return Ok(());
        }
        if let Some(cf) = self.db.cf_handle(CF_TRIE) {
            self.db.flush_cf(cf).ok();
        }
        self.db.flush().ok();
        Ok(())
    }

    fn is_empty(&self) -> bool {
        match self.db.cf_handle(CF_TRIE) {
            Some(cf) => {
                let mut it = self.db.raw_iterator_cf(cf);
                it.seek_to_first();
                !it.valid()
            }
            None => true,
        }
    }

    fn for_each(&self, visit: &mut dyn FnMut(&[u8], &[u8])) -> Result<()> {
        let cf = self.cf()?;
        let mut iter = self.db.raw_iterator_cf(cf);
        iter.seek_to_first();
        while iter.valid() {
            if let (Some(k), Some(v)) = (iter.key(), iter.value()) {
                visit(k, v);
            }
            iter.next();
        }
        iter.status().context("scanning an epoch")
    }

    fn size_bytes(&self) -> u64 {
        dir_size(&self.dir)
    }
}

/// Bytes under `path`, following directories one level at a time.
pub fn dir_size(path: &Path) -> u64 {
    let mut total = 0;
    if let Ok(entries) = std::fs::read_dir(path) {
        for entry in entries.flatten() {
            match entry.metadata() {
                Ok(m) if m.is_file() => total += m.len(),
                Ok(m) if m.is_dir() => total += dir_size(&entry.path()),
                _ => {}
            }
        }
    }
    total
}

// ---------------------------------------------------------------- FlatDB

/// Opens epochs backed by [`rustock_flatdb`].
///
/// Nothing is shared between epochs: FlatDB caches nothing of its own, so
/// there is no counterpart to the RocksDB block cache. The mapped heap
/// *is* the cache, managed by the kernel, which is also why a FlatDB epoch
/// store has one fewer memory budget to get wrong.
pub struct FlatFactory {
    /// Sizes each epoch's index at open. A hint: exceeding it costs a
    /// rehash rather than a failure, but the rehash is stop-the-world, so
    /// it should be set from the expected peak and not the present.
    pub expected_keys: u64,
    /// Bytes per heap segment. Part of the on-disk format: a store records
    /// what it was written with and refuses to open under anything else,
    /// so everything that creates an epoch has to agree on this.
    pub segment_size: u64,
}

impl FlatFactory {
    pub fn new(expected_keys: u64) -> Self {
        Self { expected_keys, segment_size: rustock_flatdb::heap::DEFAULT_SEGMENT_SIZE }
    }

    /// A smaller segment, for tests. Segments are reserved whole, so the
    /// production 512 MiB is more than a temporary directory wants.
    pub fn with_segment_size(mut self, bytes: u64) -> Self {
        self.segment_size = bytes;
        self
    }

    fn options(&self, read_only: bool) -> rustock_flatdb::Options {
        rustock_flatdb::Options {
            expected_keys: self.expected_keys,
            read_only,
            // The epoch store decides when an epoch becomes durable; an
            // epoch that fsynced on every put would pay three syncs per
            // node written.
            sync_writes: false,
            segment_size: self.segment_size,
            ..rustock_flatdb::Options::default()
        }
    }
}

impl EpochBackendFactory for FlatFactory {
    fn open(&self, dir: &Path) -> Result<Arc<dyn EpochBackend>> {
        let db = rustock_flatdb::FlatDb::open(dir, self.options(false))
            .with_context(|| format!("opening flatdb epoch at {}", dir.display()))?;
        Ok(Arc::new(FlatEpoch { db }))
    }

    fn open_read_only(&self, dir: &Path) -> Result<Arc<dyn EpochBackend>> {
        let db = rustock_flatdb::FlatDb::open(dir, self.options(true))
            .with_context(|| format!("opening flatdb epoch read-only at {}", dir.display()))?;
        Ok(Arc::new(FlatEpoch { db }))
    }

    fn name(&self) -> &'static str {
        "flatdb"
    }
}

pub struct FlatEpoch {
    db: rustock_flatdb::FlatDb,
}

impl EpochBackend for FlatEpoch {
    fn get(&self, key: &[u8]) -> Option<Vec<u8>> {
        self.db.get(key)
    }

    fn put(&self, key: &[u8], value: &[u8]) -> Result<()> {
        self.db.put(key, value).context("flatdb epoch write")
    }

    fn put_many(&self, entries: &[(Vec<u8>, Vec<u8>)]) -> Result<()> {
        self.db
            .update_batch(entries.iter().map(|(k, v)| (k.as_slice(), v.as_slice())))
            .context("flatdb epoch batch write")
    }

    /// FlatDB cannot delete, by construction.
    ///
    /// An append-only heap has nowhere to put a tombstone and no way to
    /// move a record, and the probe chains that make the index work assume
    /// no slot is ever vacated. Reclamation happens by discarding a whole
    /// epoch, which is the only kind this store needs. Failing loudly
    /// beats appearing to succeed.
    fn delete(&self, _key: &[u8]) -> Result<()> {
        anyhow::bail!(
            "flatdb epochs cannot delete individual keys; space is reclaimed \
             by discarding a whole epoch"
        )
    }

    fn flush(&self) -> Result<()> {
        self.db.flush().context("flatdb epoch flush")
    }

    fn is_empty(&self) -> bool {
        self.db.entries() == 0
    }

    fn for_each(&self, visit: &mut dyn FnMut(&[u8], &[u8])) -> Result<()> {
        self.db.for_each_record(visit);
        Ok(())
    }

    fn size_bytes(&self) -> u64 {
        self.db.size_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rocks() -> Box<dyn EpochBackendFactory> {
        Box::new(RocksFactory::new(8 * 1024 * 1024))
    }

    fn flat() -> Box<dyn EpochBackendFactory> {
        Box::new(FlatFactory::new(1024))
    }

    /// FlatDB keys must be the digest of their value; RocksDB does not
    /// care. Using content addresses for both keeps one test body.
    fn entry(seed: u8, length: usize) -> (Vec<u8>, Vec<u8>) {
        use sha3::{Digest, Keccak256};
        let value: Vec<u8> = (0..length).map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed)).collect();
        (Keccak256::digest(&value).to_vec(), value)
    }

    /// Everything the epoch store asks of a backend, asked of each one.
    /// The point of the abstraction is that this body does not know which
    /// it is talking to.
    #[test]
    fn both_backends_satisfy_the_epoch_contract() {
        for factory in [rocks(), flat()] {
            let name = factory.name();
            let dir = tempfile::tempdir().unwrap();
            let epoch = factory.open(dir.path()).unwrap();
            assert!(epoch.is_empty(), "{name}: a fresh epoch holds nothing");

            let (k1, v1) = entry(1, 50);
            let batch: Vec<(Vec<u8>, Vec<u8>)> =
                (2u8..40).map(|i| entry(i, 40 + i as usize)).collect();

            epoch.put(&k1, &v1).unwrap();
            epoch.put_many(&batch).unwrap();
            epoch.put_many(&[]).unwrap();
            epoch.flush().unwrap();

            assert!(!epoch.is_empty(), "{name}");
            assert_eq!(epoch.get(&k1).as_deref(), Some(&v1[..]), "{name}");
            for (k, v) in &batch {
                assert_eq!(epoch.get(k).as_deref(), Some(&v[..]), "{name}");
            }
            assert_eq!(epoch.get(&entry(200, 50).0), None, "{name}: absent key");

            // The sweep's scan must see every entry exactly once, by key.
            let mut seen: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
            epoch.for_each(&mut |k, v| seen.push((k.to_vec(), v.to_vec()))).unwrap();
            let mut expected: Vec<(Vec<u8>, Vec<u8>)> =
                std::iter::once((k1.clone(), v1.clone())).chain(batch.iter().cloned()).collect();
            seen.sort();
            expected.sort();
            seen.dedup();
            assert_eq!(seen, expected, "{name}: scan");

            assert!(epoch.size_bytes() > 0, "{name}: an epoch with data occupies space");
        }
    }

    /// Reopening must show what was flushed, for either backend.
    #[test]
    fn both_backends_survive_a_reopen() {
        for factory in [rocks(), flat()] {
            let name = factory.name();
            let dir = tempfile::tempdir().unwrap();
            let entries: Vec<(Vec<u8>, Vec<u8>)> = (0u8..50).map(|i| entry(i, 60)).collect();
            {
                let epoch = factory.open(dir.path()).unwrap();
                epoch.put_many(&entries).unwrap();
                epoch.flush().unwrap();
            }
            let epoch = factory.open(dir.path()).unwrap();
            for (k, v) in &entries {
                assert_eq!(epoch.get(k).as_deref(), Some(&v[..]), "{name}: after reopen");
            }
            let read_only = factory.open_read_only(dir.path()).unwrap();
            assert_eq!(read_only.get(&entries[0].0).as_deref(), Some(&entries[0].1[..]), "{name}");
        }
    }

    /// A backend that cannot delete must say so. Succeeding silently would
    /// let the epoch store believe it had reclaimed something.
    #[test]
    fn deleting_is_supported_by_rocksdb_and_refused_by_flatdb() {
        let dir = tempfile::tempdir().unwrap();
        let (k, v) = entry(1, 50);

        let epoch = rocks().open(dir.path()).unwrap();
        epoch.put(&k, &v).unwrap();
        epoch.delete(&k).unwrap();
        assert_eq!(epoch.get(&k), None);

        let dir = tempfile::tempdir().unwrap();
        let epoch = flat().open(dir.path()).unwrap();
        epoch.put(&k, &v).unwrap();
        let error = epoch.delete(&k).unwrap_err();
        assert!(error.to_string().contains("cannot delete"), "{error}");
        assert_eq!(epoch.get(&k).as_deref(), Some(&v[..]), "and the entry is still there");
    }

    /// FlatDB reserves heap segments whole and sizes its index for a key
    /// count it has not reached, so the directory is a gigabyte before a
    /// single node is stored. A rotation policy reading the filesystem
    /// would fire on an empty epoch; `size_bytes` must not.
    #[test]
    fn flatdb_reports_what_it_holds_rather_than_what_it_reserved() {
        let dir = tempfile::tempdir().unwrap();
        let epoch = flat().open(dir.path()).unwrap();
        let empty = epoch.size_bytes();
        let on_disk = dir_size(dir.path());

        epoch.put_many(&(0u8..100).map(|i| entry(i, 100)).collect::<Vec<_>>()).unwrap();
        epoch.flush().unwrap();

        assert!(
            epoch.size_bytes() > empty,
            "storing data must move the figure the policy watches"
        );
        assert!(
            on_disk > epoch.size_bytes() * 4,
            "the directory ({on_disk}) overstates what is held ({}); that is the \
             whole reason the policy does not read it",
            epoch.size_bytes()
        );
    }

    /// A record never straddles a heap segment: the frontier skips, leaving
    /// a gap. A scan has to jump those rather than stop at the first one.
    #[test]
    fn a_flatdb_scan_crosses_segment_gaps() {
        let dir = tempfile::tempdir().unwrap();
        let db = rustock_flatdb::FlatDb::open(
            dir.path(),
            rustock_flatdb::Options {
                expected_keys: 4096,
                segment_size: 1 << 20,
                ..rustock_flatdb::Options::default()
            },
        )
        .unwrap();
        // 100 KB each over 1 MiB segments: several boundaries, and no
        // record can sit across one.
        let entries: Vec<(Vec<u8>, Vec<u8>)> = (0u8..40).map(|i| entry(i, 100_000)).collect();
        for (k, v) in &entries {
            db.put(k, v).unwrap();
        }
        let mut seen = 0usize;
        db.for_each_record(&mut |k, v| {
            assert!(entries.iter().any(|(ek, ev)| ek == k && ev == v));
            seen += 1;
        });
        assert_eq!(seen, entries.len(), "every record was reached past the gaps");
    }

    #[test]
    fn engine_names_parse_and_round_trip() {
        for (name, expected) in [
            ("rocksdb", TrieEngine::RocksDb),
            ("rocks", TrieEngine::RocksDb),
            ("default", TrieEngine::RocksDb),
            ("flatdb", TrieEngine::FlatDb),
            ("flat", TrieEngine::FlatDb),
        ] {
            assert_eq!(TrieEngine::parse(name).unwrap(), expected, "{name}");
        }
        assert_eq!(TrieEngine::parse("rocksdb").unwrap().name(), "rocksdb");
        assert_eq!(TrieEngine::parse("flat").unwrap().name(), "flatdb");
        let error = TrieEngine::parse("leveldb").unwrap_err().to_string();
        assert!(error.contains("unknown trie engine"), "{error}");
    }

    /// Opening a RocksDB directory as FlatDB, or the reverse, would create
    /// an empty store beside the real data and resync from scratch with the
    /// old data still on disk. It has to be refused at startup.
    #[test]
    fn each_engine_refuses_a_directory_the_other_wrote() {
        for (factory, engine, other) in [
            (rocks(), TrieEngine::RocksDb, TrieEngine::FlatDb),
            (flat(), TrieEngine::FlatDb, TrieEngine::RocksDb),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let (k, v) = entry(1, 50);
            {
                let epoch = factory.open(dir.path()).unwrap();
                epoch.put(&k, &v).unwrap();
                epoch.flush().unwrap();
            }

            assert_eq!(TrieEngine::detect(dir.path()), Some(engine));
            engine.check_matches(dir.path()).expect("its own directory opens");

            let error = other
                .check_matches(dir.path())
                .unwrap_err()
                .to_string();
            assert!(
                error.contains(engine.name()) && error.contains(other.name()),
                "the refusal must name both engines: {error}"
            );
        }
    }

    /// A directory that no engine has written is not a mismatch -- it is a
    /// store about to be created, and either engine may create it.
    #[test]
    fn an_empty_directory_matches_either_engine() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(TrieEngine::detect(dir.path()), None);
        TrieEngine::RocksDb.check_matches(dir.path()).unwrap();
        TrieEngine::FlatDb.check_matches(dir.path()).unwrap();
    }

    /// An epoch root holds only `epoch-N` subdirectories, so the probe has
    /// to descend into the oldest one rather than looking at the root.
    #[test]
    fn detection_descends_into_an_epoch_root() {
        let root = tempfile::tempdir().unwrap();
        let epoch_dir = root.path().join("epoch-0");
        let (k, v) = entry(1, 50);
        {
            let epoch = flat().open(&epoch_dir).unwrap();
            epoch.put(&k, &v).unwrap();
            epoch.flush().unwrap();
        }
        assert_eq!(TrieEngine::detect(root.path()), None, "nothing at the root itself");
        TrieEngine::FlatDb.check_matches(root.path()).expect("flatdb opens its own epochs");
        let error = TrieEngine::RocksDb.check_matches(root.path()).unwrap_err().to_string();
        assert!(error.contains("flatdb"), "{error}");
    }

    /// The whole epoch store, rotation and sweep included, over FlatDB.
    #[test]
    fn an_epoch_store_runs_on_flatdb() {
        use crate::epoch_store::{EpochConfig, EpochTrieStore};
        use rustock_trie::TrieStore;

        let dir = tempfile::tempdir().unwrap();
        let store = EpochTrieStore::open_with(
            dir.path(),
            EpochConfig { epochs: 3, burial_depth: 10, rotate_bytes: 64 * 1024 },
            None,
            Arc::new(FlatFactory::new(4096)),
        )
        .unwrap();

        let entries: Vec<(Vec<u8>, Vec<u8>)> = (0u8..200).map(|i| entry(i, 120)).collect();
        for (k, v) in &entries {
            store.put(k, v);
        }
        store.flush().unwrap();
        for (k, v) in &entries {
            assert_eq!(store.get(k).as_deref(), Some(&v[..]));
        }
        assert!(store.newest_bytes() > 0, "the rotation trigger sees the data");
        assert!(store.total_bytes() >= store.newest_bytes());
    }
}
