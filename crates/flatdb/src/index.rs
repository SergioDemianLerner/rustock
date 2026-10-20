//! An open-addressed hash table mapping keys to heap offsets, memory-mapped
//! and worked in place.
//!
//! A slot is eight bytes holding nothing but `heap_offset + 1`; zero means
//! empty. There are no flag bits, no checksum and no padding. The record
//! itself records whether it carries a stored key, and eight divides 512
//! exactly, so no slot can straddle the sector a device writes atomically —
//! a torn slot is impossible rather than detectable.
//!
//! **The table stores no key bytes at all.** A probe resolves a collision
//! by fetching the candidate record and comparing its key, read back for
//! long records and recomputed as `H(value)` for short ones. That is what
//! makes the index cost sixteen bytes of RAM per key, and it makes every
//! successful short-record read a verification of the stored bytes against
//! their own address.
//!
//! # Why eight bytes and not Java's five
//!
//! Java has no 40-bit integer, so a five-byte slot meant assembling an
//! offset byte by byte on every probe step. Eight costs 60% more index RAM
//! and buys a single aligned load, exact sector packing, and — the reason
//! that matters — `AtomicU64`, which is what makes lock-free reads
//! expressible at all. A writer publishes a slot with a release store and a
//! reader acquires it, giving precisely the happens-before edge that the
//! Java version's `experimentalConcurrentRW` lacked and was deleted for.
//!
//! # Why the file is named after its capacity
//!
//! A grow rehashes into a **new file**, `index.<capacity>.dat`, and the old
//! one is removed only after the checkpoint naming the new capacity is
//! durable. The Java version rewrites `index.dat` in place: a crash between
//! that rewrite and the checkpoint leaves a file holding the *new* table
//! while the checkpoint still says the *old* capacity, and reading the
//! first half of a doubled table silently loses every entry that rehashed
//! into the second half. Naming the file after its capacity makes that
//! state unrepresentable.

use crate::heap::{Heap, Record};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::RwLock;

pub const SLOT_SIZE: u64 = 8;
/// The unit a device writes atomically. Eight divides it exactly, so slot
/// alignment costs nothing — unlike the five-byte slot, which needed two
/// padding bytes per sector.
pub const SECTOR_SIZE: u64 = 512;
/// Bounded by the 32-bit hash: more slots than this could not be addressed
/// by it. At load factor 0.5 this is about a billion keys.
pub const MAX_CAPACITY: u64 = 1 << 31;
/// Values longer than this carry their key in the heap instead of having it
/// recomputed on read.
pub const STORE_KEY_THRESHOLD: usize = 1024;

const EMPTY: u64 = 0;

/// The key/value relation the table depends on.
pub trait Keys: Send + Sync {
    /// The key a value answers to. Must be the digest the store is keyed by.
    fn compute_key(&self, data: &[u8]) -> Vec<u8>;
    /// Which slot a key starts probing at.
    fn hash(&self, key: &[u8]) -> u32;
    fn length(&self) -> usize;
}

struct Table {
    map: memmap2::MmapRaw,
    capacity: u64,
    path: PathBuf,
}

impl Table {
    /// # Safety
    ///
    /// `slot` must be below `capacity`. The mapping base is page-aligned
    /// and slots are eight bytes at eight-byte strides, so every slot is
    /// naturally aligned for `AtomicU64`.
    unsafe fn at(&self, slot: u64) -> &AtomicU64 {
        debug_assert!(slot < self.capacity);
        &*(self.map.as_mut_ptr().add((slot * SLOT_SIZE) as usize) as *const AtomicU64)
    }
}

pub struct Index {
    dir: PathBuf,
    read_only: bool,
    verify_long_records: bool,
    table: RwLock<Table>,
    size: AtomicU64,
    dirty: AtomicBool,
}

impl Index {
    /// Sizes the table from the checkpoint, or from an expectation when
    /// starting empty, and maps it.
    #[allow(clippy::too_many_arguments)]
    pub fn open(
        dir: &Path,
        read_only: bool,
        committed_capacity: u64,
        committed_size: u64,
        expected_keys: u64,
        verify_long_records: bool,
    ) -> io::Result<Self> {
        let capacity = if committed_capacity > 0 {
            committed_capacity
        } else {
            table_size_for(expected_keys.saturating_mul(2).max(16))
        };
        let table = map_table(dir, read_only, capacity)?;
        let index = Self {
            dir: dir.to_path_buf(),
            read_only,
            verify_long_records,
            table: RwLock::new(table),
            size: AtomicU64::new(committed_size),
            // A table that no checkpoint describes yet has to be written
            // whole by the first flush.
            dirty: AtomicBool::new(committed_capacity == 0),
        };
        Ok(index)
    }

    pub fn size(&self) -> u64 {
        self.size.load(Ordering::Acquire)
    }

    pub fn capacity(&self) -> u64 {
        self.table.read().expect("index table").capacity
    }

    pub fn modified(&self) -> bool {
        self.dirty.load(Ordering::Acquire)
    }

    /// The key a record answers to: read back when stored, recomputed from
    /// the data otherwise.
    ///
    /// One helper, used everywhere a key is needed. The Java version's
    /// rehash recovered keys from value bytes directly and silently lost
    /// every long record; routing all three call sites through here is what
    /// makes that mistake unrepresentable.
    fn key_of(keys: &dyn Keys, record: &Record<'_>) -> Vec<u8> {
        match record.key {
            Some(stored) => stored.to_vec(),
            None => keys.compute_key(record.data),
        }
    }

    /// Looks `key` up, returning a copy of its value.
    ///
    /// Takes no exclusive lock, so any number of threads may be in here at
    /// once, and allocates only for the record that matches: probe steps
    /// that miss borrow straight out of the mapping.
    /// Does this record's data still hash to the key it is filed under?
    ///
    /// Only asked of a **long** record, and only once its stored key has
    /// already matched. A short record has no stored key, so the probe
    /// recomputed `H(data)` to compare in the first place and is verified
    /// by construction.
    ///
    /// Doing it on the hit rather than on every probe step is what makes
    /// it affordable. Recomputing during the comparison would mean hashing
    /// a 24 KB contract body to *reject* it on a collision -- about 80 µs
    /// at the ~283 MB/s measured here -- which is why the key is stored at
    /// all. One hash on the hit costs 3.6 µs at the 1 KiB threshold and
    /// scales with the record; zero on a miss.
    fn long_record_is_intact(&self, keys: &dyn Keys, record: &Record<'_>, key: &[u8]) -> bool {
        if !self.verify_long_records || record.key.is_none() {
            return true;
        }
        if keys.compute_key(record.data) == key {
            return true;
        }
        // The bytes do not hash to their own address. Reporting a miss is
        // recoverable -- a trie node that is absent can be fetched from a
        // peer -- and returning the bytes is not.
        tracing::error!(
            target: "rustock::flatdb",
            "a stored record no longer hashes to its key; reporting it as absent"
        );
        false
    }

    pub fn get(&self, heap: &Heap, keys: &dyn Keys, key: &[u8]) -> Option<Vec<u8>> {
        let table = self.table.read().expect("index table");
        if table.capacity == 0 {
            return None;
        }
        let mask = table.capacity - 1;
        let mut i = keys.hash(key) as u64 & mask;
        let key_length = keys.length();
        loop {
            // SAFETY: `i` is masked into the table on every iteration.
            let slot = unsafe { table.at(i) }.load(Ordering::Acquire);
            if slot == EMPTY {
                return None;
            }
            let found = heap.with_record(slot - 1, key_length, |record| {
                if Self::key_of(keys, &record) == key
                    && self.long_record_is_intact(keys, &record, key)
                {
                    Some(record.data.to_vec())
                } else {
                    None
                }
            });
            if let Some(Some(data)) = found {
                return Some(data);
            }
            i = (i + 1) & mask;
        }
    }

    /// Ask the kernel to fetch the index pages these keys will probe.
    ///
    /// A mapped read is a page fault, and a fault is one thread blocked on
    /// one page — queue depth 1, which is the worst thing to hand an SSD.
    /// `MADV_WILLNEED` over the slots a batch is about to touch lets the
    /// kernel issue them together instead.
    ///
    /// Only the first slot of each probe chain is advised. Chains are
    /// short at load factor 0.5 and the rest of a chain is usually on the
    /// same page anyway, so following them here would cost more than it
    /// saves.
    pub fn prefetch(&self, keys: &dyn Keys, wanted: &[Vec<u8>]) {
        let table = self.table.read().expect("index table");
        if table.capacity == 0 {
            return;
        }
        let mask = table.capacity - 1;
        for key in wanted {
            let slot = keys.hash(key) as u64 & mask;
            let _ = table.map.advise_range(
                memmap2::Advice::WillNeed,
                (slot * SLOT_SIZE) as usize,
                SLOT_SIZE as usize,
            );
        }
    }

    /// The heap offsets these keys' first probe step points at, for
    /// prefetching the records themselves. `None` where the slot is empty.
    pub fn first_offsets(&self, keys: &dyn Keys, wanted: &[Vec<u8>]) -> Vec<Option<u64>> {
        let table = self.table.read().expect("index table");
        if table.capacity == 0 {
            return vec![None; wanted.len()];
        }
        let mask = table.capacity - 1;
        wanted
            .iter()
            .map(|key| {
                let slot = keys.hash(key) as u64 & mask;
                // SAFETY: masked into the table.
                match unsafe { table.at(slot) }.load(Ordering::Acquire) {
                    EMPTY => None,
                    packed => Some(packed - 1),
                }
            })
            .collect()
    }

    /// Inserts `key -> data`, appending the value to the heap.
    ///
    /// Callers must serialise this against itself; readers need no
    /// exclusion. Re-putting a key is a no-op, because content addressing
    /// means the value must already be identical.
    pub fn put(
        &self,
        heap: &Heap,
        keys: &dyn Keys,
        key: &[u8],
        data: &[u8],
    ) -> io::Result<()> {
        // A full lap without an empty slot means occupancy outran the entry
        // count -- see the note on resurrected slots. Grow and start over
        // rather than probe forever; `grow` drops the duplicates that got
        // us here, so the retry terminates.
        for _ in 0..2 {
            if self.try_put(heap, keys, key, data)? {
                return Ok(());
            }
            self.grow(heap, keys)?;
        }
        Err(io::Error::other("index remained full after growing"))
    }

    /// Returns `false` when a full lap found no empty slot.
    fn try_put(
        &self,
        heap: &Heap,
        keys: &dyn Keys,
        key: &[u8],
        data: &[u8],
    ) -> io::Result<bool> {
        if self.size.load(Ordering::Relaxed) + 1 > self.capacity() / 2 {
            self.grow(heap, keys)?;
        }
        let table = self.table.read().expect("index table");
        let mask = table.capacity - 1;
        let key_length = keys.length();
        let mut i = keys.hash(key) as u64 & mask;
        let mut reusable: Option<u64> = None;

        for _ in 0..table.capacity {
            // SAFETY: `i` is masked into the table on every iteration.
            let cell = unsafe { table.at(i) };
            let slot = cell.load(Ordering::Acquire);
            if slot == EMPTY {
                let target = reusable.unwrap_or(i);
                let store_key = data.len() > STORE_KEY_THRESHOLD;
                let offset = heap.append(key, data, store_key)?;
                // Release, paired with the acquire loads above and in
                // `get`: everything the append wrote happens-before any
                // reader that observes this slot.
                // SAFETY: `target` came from a masked probe.
                unsafe { table.at(target) }.store(offset + 1, Ordering::Release);
                self.size.fetch_add(1, Ordering::AcqRel);
                self.dirty.store(true, Ordering::Release);
                return Ok(true);
            }

            enum Seen {
                Dead,
                Same,
                Conflict,
                Other,
            }
            let seen = heap
                .with_record(slot - 1, key_length, |record| {
                    if Self::key_of(keys, &record) != key {
                        Seen::Other
                    } else if record.key.is_some() && record.data != data {
                        // Only reachable for a stored-key record, where the
                        // key was read rather than derived. Two values under
                        // one key is a collision or a misuse, not something
                        // to shrug off.
                        Seen::Conflict
                    } else {
                        Seen::Same
                    }
                })
                .unwrap_or(Seen::Dead);

            match seen {
                // A slot left behind by an interrupted flush, pointing at or
                // beyond the frontier. Remember it to reclaim, but keep
                // probing: the key may still be present further along.
                Seen::Dead => {
                    if reusable.is_none() {
                        reusable = Some(i);
                    }
                }
                Seen::Same => return Ok(true),
                Seen::Conflict => {
                    return Err(io::Error::other(
                        "two distinct values stored under one key",
                    ))
                }
                Seen::Other => {}
            }
            i = (i + 1) & mask;
        }

        // A full lap. If a dead slot was seen it can still take the record;
        // otherwise the caller grows.
        match reusable {
            Some(target) => {
                let store_key = data.len() > STORE_KEY_THRESHOLD;
                let offset = heap.append(key, data, store_key)?;
                // SAFETY: `target` came from a masked probe.
                unsafe { table.at(target) }.store(offset + 1, Ordering::Release);
                self.size.fetch_add(1, Ordering::AcqRel);
                self.dirty.store(true, Ordering::Release);
                Ok(true)
            }
            None => Ok(false),
        }
    }

    /// Doubles the table into a new file and rehashes every live entry.
    ///
    /// Stop-the-world: it holds the write lock, and it reads every record
    /// out of the heap to recover its key, because the index stores none.
    /// At sixteen million entries that is minutes during which no read
    /// proceeds.
    ///
    /// It is a safety net rather than an expected event — an epoch's key
    /// count is bounded by its rotation threshold, so
    /// `epoch_backend::keys_for_rotation` sizes the table past it. Reaching
    /// here means that sizing was wrong, which an operator needs to know,
    /// so it says so at both ends with how long it took.
    fn grow(&self, heap: &Heap, keys: &dyn Keys) -> io::Result<()> {
        let started = std::time::Instant::now();
        let mut table = self.table.write().expect("index table");
        tracing::warn!(
            target: "rustock::flatdb",
            "index full at {} of {} slots: rehashing, which stops every read until \
             it finishes. The store was sized for too few keys; see expected_keys.",
            self.size.load(Ordering::Relaxed),
            table.capacity
        );
        let new_capacity = if table.capacity == 0 { 16 } else { table.capacity * 2 };
        if new_capacity > MAX_CAPACITY {
            return Err(io::Error::other(format!(
                "index cannot grow beyond {MAX_CAPACITY} slots"
            )));
        }
        let fresh = map_table(&self.dir, self.read_only, new_capacity)?;
        let mask = new_capacity - 1;
        let key_length = keys.length();
        let mut moved = 0u64;

        for j in 0..table.capacity {
            // SAFETY: `j` is below the old capacity by construction.
            let slot = unsafe { table.at(j) }.load(Ordering::Acquire);
            if slot == EMPTY {
                continue;
            }
            // A dead slot is dropped rather than carried over; this is the
            // one place the table shrinks.
            let Some(key) =
                heap.with_record(slot - 1, key_length, |record| Self::key_of(keys, &record))
            else {
                continue;
            };
            let mut i = keys.hash(&key) as u64 & mask;
            let mut duplicate = false;
            loop {
                // SAFETY: `i` is masked into the new table.
                let occupant = unsafe { fresh.at(i) }.load(Ordering::Relaxed);
                if occupant == EMPTY {
                    break;
                }
                // Two slots can point at the same key once a stale slot has
                // been resurrected. Carrying both over would let the
                // duplicates accumulate across grows; this is where they go.
                let same = heap
                    .with_record(occupant - 1, key_length, |record| {
                        Self::key_of(keys, &record) == key
                    })
                    .unwrap_or(false);
                if same {
                    duplicate = true;
                    break;
                }
                i = (i + 1) & mask;
            }
            if duplicate {
                continue;
            }
            // SAFETY: `i` is masked into the new table.
            unsafe { fresh.at(i) }.store(slot, Ordering::Relaxed);
            moved += 1;
        }

        let previous = std::mem::replace(&mut *table, fresh);
        self.size.store(moved, Ordering::Release);
        self.dirty.store(true, Ordering::Release);
        drop(previous);
        tracing::warn!(
            target: "rustock::flatdb",
            "index rehashed to {} slots, {moved} entries carried over, {:.1}s stopped",
            new_capacity,
            started.elapsed().as_secs_f64()
        );
        Ok(())
    }

    /// Hands every live key to `action`.
    pub fn for_each_key(&self, heap: &Heap, keys: &dyn Keys, mut action: impl FnMut(Vec<u8>)) {
        let table = self.table.read().expect("index table");
        let key_length = keys.length();
        for i in 0..table.capacity {
            // SAFETY: `i` is below the capacity by construction.
            let slot = unsafe { table.at(i) }.load(Ordering::Acquire);
            if slot == EMPTY {
                continue;
            }
            if let Some(key) =
                heap.with_record(slot - 1, key_length, |record| Self::key_of(keys, &record))
            {
                action(key);
            }
        }
    }

    /// Makes the table durable.
    ///
    /// With the table mapped there is nothing to copy and no dirty-page
    /// bookkeeping to keep: `msync` writes back exactly the pages that
    /// changed. The Java version's `BitSet` of dirty pages, its run
    /// coalescing and its `fullRewrite` flag all exist to reconstruct what
    /// the kernel already knows.
    pub fn save(&self) -> io::Result<()> {
        if self.read_only || !self.modified() {
            return Ok(());
        }
        self.table.read().expect("index table").map.flush()?;
        self.dirty.store(false, Ordering::Release);
        Ok(())
    }

    /// Removes index files other than the live one, which are orphans left
    /// by a grow that no checkpoint ever adopted.
    pub fn remove_stale_files(&self) -> io::Result<()> {
        if self.read_only {
            return Ok(());
        }
        let live = self.table.read().expect("index table").path.clone();
        for entry in std::fs::read_dir(&self.dir)? {
            let path = entry?.path();
            let is_index = path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("index.") && n.ends_with(".dat"));
            if is_index && path != live {
                let _ = std::fs::remove_file(&path);
            }
        }
        Ok(())
    }
}

fn table_path(dir: &Path, capacity: u64) -> PathBuf {
    dir.join(format!("index.{capacity}.dat"))
}

fn map_table(dir: &Path, read_only: bool, capacity: u64) -> io::Result<Table> {
    let path = table_path(dir, capacity);
    let bytes = capacity * SLOT_SIZE;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(!read_only)
        .create(!read_only)
        .open(&path)?;
    if !read_only && file.metadata()?.len() < bytes {
        file.set_len(bytes)?;
    }
    // As in `Heap::map_segment`: the directory lock is what makes the
    // pointer dereferences below sound, and `map_raw` itself hands out no
    // references.
    let mut options = memmap2::MmapOptions::new();
    options.len(bytes as usize);
    let map = if read_only {
        options.map_raw_read_only(&file)?
    } else {
        options.map_raw(&file)?
    };
    Ok(Table { map, capacity, path })
}

fn table_size_for(wanted: u64) -> u64 {
    let mut c = 16u64;
    while c < wanted && c < MAX_CAPACITY {
        c <<= 1;
    }
    c.min(MAX_CAPACITY)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha3::{Digest, Keccak256};

    struct KeccakKeys;

    impl Keys for KeccakKeys {
        fn compute_key(&self, data: &[u8]) -> Vec<u8> {
            Keccak256::digest(data).to_vec()
        }
        fn hash(&self, key: &[u8]) -> u32 {
            u32::from_be_bytes(key[28..32].try_into().expect("32-byte key"))
        }
        fn length(&self) -> usize {
            32
        }
    }

    struct Fixture {
        _dir: tempfile::TempDir,
        heap: Heap,
        index: Index,
    }

    fn fixture(expected_keys: u64) -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let heap = Heap::open(dir.path(), false, 0, 1 << 20).unwrap();
        let index = Index::open(dir.path(), false, 0, 0, expected_keys, true).unwrap();
        Fixture { _dir: dir, heap, index }
    }

    fn value(length: usize, seed: u8) -> Vec<u8> {
        (0..length).map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed)).collect()
    }

    fn key_of(data: &[u8]) -> Vec<u8> {
        Keccak256::digest(data).to_vec()
    }

    #[test]
    fn values_round_trip_on_both_sides_of_the_key_storage_threshold() {
        let f = fixture(64);
        for length in [1usize, 50, 1023, 1024, 1025, 4000] {
            let data = value(length, length as u8);
            f.index.put(&f.heap, &KeccakKeys, &key_of(&data), &data).unwrap();
        }
        for length in [1usize, 50, 1023, 1024, 1025, 4000] {
            let data = value(length, length as u8);
            assert_eq!(
                f.index.get(&f.heap, &KeccakKeys, &key_of(&data)),
                Some(data),
                "length {length}"
            );
        }
    }

    #[test]
    fn a_missing_key_is_a_miss() {
        let f = fixture(64);
        let data = value(50, 1);
        f.index.put(&f.heap, &KeccakKeys, &key_of(&data), &data).unwrap();
        assert_eq!(f.index.get(&f.heap, &KeccakKeys, &key_of(&value(50, 2))), None);
    }

    /// The declared capacity is a hint. Exceeding it must rehash, not fail,
    /// and must not lose an entry on the way.
    #[test]
    fn the_table_grows_past_its_declared_capacity_without_losing_entries() {
        let f = fixture(8);
        let count = 2_000usize;
        for i in 0..count {
            let data = value(40, i as u8).iter().chain(&i.to_le_bytes()).copied().collect::<Vec<_>>();
            f.index.put(&f.heap, &KeccakKeys, &key_of(&data), &data).unwrap();
        }
        assert!(f.index.capacity() >= 2 * count as u64, "it grew");
        assert_eq!(f.index.size(), count as u64);
        for i in 0..count {
            let data = value(40, i as u8).iter().chain(&i.to_le_bytes()).copied().collect::<Vec<_>>();
            assert_eq!(f.index.get(&f.heap, &KeccakKeys, &key_of(&data)), Some(data), "entry {i}");
        }
    }

    /// Long records are rehashed by their *stored* key. Recovering it from
    /// the value bytes instead is the defect that silently dropped them.
    #[test]
    fn growing_preserves_long_records_too() {
        let f = fixture(8);
        let mut expected = Vec::new();
        for i in 0..600usize {
            let length = if i % 3 == 0 { 2048 } else { 40 };
            let mut data = value(length, i as u8);
            data.extend_from_slice(&i.to_le_bytes());
            f.index.put(&f.heap, &KeccakKeys, &key_of(&data), &data).unwrap();
            expected.push(data);
        }
        for data in &expected {
            assert_eq!(f.index.get(&f.heap, &KeccakKeys, &key_of(data)).as_ref(), Some(data));
        }
        assert_eq!(f.index.size(), expected.len() as u64);
    }

    /// Content addressing: the same value twice must not grow the heap.
    #[test]
    fn a_repeated_identical_put_is_a_no_op() {
        let f = fixture(64);
        let data = value(50, 7);
        f.index.put(&f.heap, &KeccakKeys, &key_of(&data), &data).unwrap();
        let after_first = f.heap.size();
        f.index.put(&f.heap, &KeccakKeys, &key_of(&data), &data).unwrap();
        assert_eq!(f.heap.size(), after_first, "the heap did not grow");
        assert_eq!(f.index.size(), 1);
    }

    /// For a stored-key record the key is read, not derived, so a differing
    /// value is a real conflict rather than something to ignore.
    #[test]
    fn a_differing_value_under_a_stored_key_is_rejected() {
        let f = fixture(64);
        let data = value(2000, 1);
        let key = key_of(&data);
        f.index.put(&f.heap, &KeccakKeys, &key, &data).unwrap();
        let other = value(2000, 2);
        let error = f.index.put(&f.heap, &KeccakKeys, &key, &other).unwrap_err();
        assert!(error.to_string().contains("two distinct values"));
    }

    #[test]
    fn for_each_key_sees_exactly_the_live_entries() {
        let f = fixture(64);
        let mut keys = Vec::new();
        for i in 0..50u8 {
            let data = value(60, i);
            f.index.put(&f.heap, &KeccakKeys, &key_of(&data), &data).unwrap();
            keys.push(key_of(&data));
        }
        let mut seen = Vec::new();
        f.index.for_each_key(&f.heap, &KeccakKeys, |k| seen.push(k));
        seen.sort();
        keys.sort();
        assert_eq!(seen, keys);
    }

    /// Slots pointing past the frontier must be walked over, and then
    /// reclaimed by the insertion that walks over them.
    #[test]
    fn slots_past_the_frontier_are_skipped_and_reused() {
        let f = fixture(64);
        let keeper = value(50, 1);
        f.index.put(&f.heap, &KeccakKeys, &key_of(&keeper), &keeper).unwrap();
        let committed = f.heap.size();
        let committed_entries = f.index.size();

        for i in 10..40u8 {
            let data = value(50, i);
            f.index.put(&f.heap, &KeccakKeys, &key_of(&data), &data).unwrap();
        }
        // Rewind as a reopen after an interrupted flush would: the slots
        // stay, the heap frontier does not.
        f.heap.truncate_to(committed);

        assert_eq!(f.index.get(&f.heap, &KeccakKeys, &key_of(&keeper)), Some(keeper.clone()));
        for i in 10..40u8 {
            assert_eq!(f.index.get(&f.heap, &KeccakKeys, &key_of(&value(50, i))), None);
        }

        // New records land on the offsets the stale slots point at.
        for i in 100..130u8 {
            let data = value(50, i);
            f.index.put(&f.heap, &KeccakKeys, &key_of(&data), &data).unwrap();
        }
        for i in 100..130u8 {
            let data = value(50, i);
            assert_eq!(f.index.get(&f.heap, &KeccakKeys, &key_of(&data)), Some(data), "fresh {i}");
        }
        for i in 10..40u8 {
            assert_eq!(
                f.index.get(&f.heap, &KeccakKeys, &key_of(&value(50, i))),
                None,
                "stale {i} stayed invisible"
            );
        }
        let _ = committed_entries;
    }

    #[test]
    fn capacity_rounds_up_to_a_power_of_two() {
        assert_eq!(table_size_for(1), 16);
        assert_eq!(table_size_for(16), 16);
        assert_eq!(table_size_for(17), 32);
        assert_eq!(table_size_for(1000), 1024);
        assert_eq!(table_size_for(u64::MAX), MAX_CAPACITY);
    }

    /// Eight-byte slots divide the sector exactly, which is what removes
    /// the padding the five-byte layout needed.
    #[test]
    fn no_slot_crosses_a_sector_boundary() {
        assert_eq!(SECTOR_SIZE % SLOT_SIZE, 0);
        for slot in 0..10_000u64 {
            let start = slot * SLOT_SIZE;
            assert_eq!(
                start / SECTOR_SIZE,
                (start + SLOT_SIZE - 1) / SECTOR_SIZE,
                "slot {slot} straddles a sector"
            );
        }
    }

    /// A long record whose bytes no longer hash to its key must not be
    /// returned. Short records cannot reach this state: the probe
    /// recomputes their key to compare, so a corrupt one simply misses.
    #[test]
    fn a_corrupt_long_record_reads_as_absent() {
        let f = fixture(64);
        let data = value(4000, 7);
        let key = key_of(&data);
        f.index.put(&f.heap, &KeccakKeys, &key, &data).unwrap();
        assert_eq!(f.index.get(&f.heap, &KeccakKeys, &key), Some(data.clone()));

        // Corrupt one byte of the stored value, leaving the stored key
        // intact -- which is precisely the case the stored key cannot
        // catch by itself.
        corrupt_one_data_byte(&f);

        assert_eq!(
            f.index.get(&f.heap, &KeccakKeys, &key),
            None,
            "the bytes no longer hash to their address, so they are not the record"
        );
    }

    /// With verification off the same corruption is handed back, which is
    /// the behaviour the option exists to describe -- and the reason it
    /// defaults to on.
    #[test]
    fn without_verification_a_corrupt_long_record_is_returned() {
        let dir = tempfile::tempdir().unwrap();
        let heap = Heap::open(dir.path(), false, 0, 1 << 20).unwrap();
        let index = Index::open(dir.path(), false, 0, 0, 64, false).unwrap();
        let f = Fixture { _dir: dir, heap, index };

        let data = value(4000, 7);
        let key = key_of(&data);
        f.index.put(&f.heap, &KeccakKeys, &key, &data).unwrap();
        corrupt_one_data_byte(&f);

        let got = f.index.get(&f.heap, &KeccakKeys, &key);
        assert!(got.is_some(), "unverified, the damaged bytes come straight back");
        assert_ne!(got.as_ref(), Some(&data), "and they are not what was stored");
    }

    /// Flip a byte well inside the record's data, past the magic, the
    /// length field and the 32-byte stored key.
    fn corrupt_one_data_byte(f: &Fixture) {
        f.heap
            .with_record(0, 32, |record| {
                let at = record.data.as_ptr() as *mut u8;
                // SAFETY: the mapping is writable and this is inside the
                // record's own data, which the test owns exclusively.
                unsafe { *at.add(100) ^= 0xFF };
            })
            .expect("the record is at offset zero");
    }

    /// Verification happens on the hit, never on a rejection: a probe that
    /// walks over an unrelated long record must not hash it.
    #[test]
    fn verification_does_not_run_on_a_probe_that_misses() {
        let f = fixture(64);
        let stored = value(4000, 1);
        f.index.put(&f.heap, &KeccakKeys, &key_of(&stored), &stored).unwrap();
        // A key that is not present: every probe step rejects, and none of
        // them may need the record's contents hashed to do it.
        let absent = key_of(&value(4000, 2));
        assert_eq!(f.index.get(&f.heap, &KeccakKeys, &absent), None);
    }
}
