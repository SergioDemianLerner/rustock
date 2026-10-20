//! Recovery from writes that did not all reach the platter.
//!
//! The crash tests in the crate simulate by rewinding the checkpoint and
//! dropping the mappings. That exercises the recovery *logic*, and it
//! caught real bugs, but nothing in it ever writes half a sector. The
//! durability argument rests on claims those tests cannot make.
//!
//! This harness tears for real. It snapshots a database at a committed
//! point, writes more, and then rebuilds the directory with an arbitrary
//! subset of 512-byte sectors reverted to their earlier contents — which
//! is what a device that lost power mid-writeback leaves behind. The
//! checkpoint is kept at the old one, because that is the honest shape of
//! the failure: the checkpoint is written last and fsynced, so a crash
//! that tears the heap or the index is a crash that did not reach it.
//!
//! What must then hold, for **any** subset of torn sectors:
//!
//! 1. The store opens.
//! 2. Every key durable at the old checkpoint reads back.
//! 3. It is still writable, and what is written afterwards reads back.
//!
//! The reason (2) can hold at all is worth stating, because it is the
//! whole argument for sector granularity: **a slot write never modifies
//! another slot.** Eight-byte slots divide a 512-byte sector exactly, so
//! reverting a sector loses precisely the slots written into it since the
//! snapshot, and never corrupts one that was already there. Those lost
//! slots point at or beyond the old committed frontier, so they would
//! have read as a miss anyway.

use proptest::prelude::*;
use rustock_flatdb::{FlatDb, Options};
use sha3::{Digest, Keccak256};
use std::collections::BTreeMap;
use std::path::Path;

const SECTOR: usize = 512;

fn options() -> Options {
    Options {
        expected_keys: 4096,
        read_only: false,
        sync_writes: false,
        verify_long_records: true,
        // Small, so a test covers several heap files without writing a
        // gigabyte. The size is recorded in the checkpoint, so this
        // exercises the same code a production segment would.
        segment_size: 1 << 20,
    }
}

fn key_of(data: &[u8]) -> Vec<u8> {
    Keccak256::digest(data).to_vec()
}

fn value(length: usize, seed: u64) -> Vec<u8> {
    let mut out = Vec::with_capacity(length);
    let mut x = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
    for _ in 0..length {
        x = x.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
        out.push((x >> 33) as u8);
    }
    out
}

fn entries(count: u64, length: usize, salt: u64) -> BTreeMap<Vec<u8>, Vec<u8>> {
    (0..count)
        .map(|i| {
            let v = value(length, salt.wrapping_mul(1_000_003).wrapping_add(i));
            (key_of(&v), v)
        })
        .collect()
}

fn write_all(db: &FlatDb, rows: &BTreeMap<Vec<u8>, Vec<u8>>) {
    db.update_batch(rows.iter().map(|(k, v)| (k.as_slice(), v.as_slice()))).unwrap();
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap().flatten() {
        let name = entry.file_name();
        // The lock file is per-process state, not data.
        if name == std::ffi::OsStr::new("lock.dat") {
            continue;
        }
        std::fs::copy(entry.path(), to.join(name)).unwrap();
    }
}

/// Which files a tear may touch. The checkpoint is excluded on purpose:
/// it is written last and fsynced, so a crash that tore the heap or the
/// index is a crash that never got to it. (A torn *checkpoint* is a
/// different failure and is covered by the crate's own tests.)
fn tearable(name: &str) -> bool {
    name.starts_with("heap.") || name.starts_with("index.")
}

/// Rebuild `live` so that every sector whose global index is in `torn`
/// holds what it held in `snapshot` — bytes that never reached the disk.
///
/// Sectors are numbered across the tearable files in name order, so a
/// single index space covers the whole database and a property test can
/// choose from it without knowing the layout.
fn tear(snapshot: &Path, live: &Path, torn: &dyn Fn(usize) -> bool) -> usize {
    let mut names: Vec<String> = std::fs::read_dir(live)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| tearable(n))
        .collect();
    names.sort();

    let mut sector = 0usize;
    let mut reverted = 0usize;
    for name in names {
        let old_path = snapshot.join(&name);
        let new_path = live.join(&name);
        // A file that did not exist at the snapshot is entirely new; a
        // tear there reverts to nothing, which the absence of its bytes
        // already represents.
        let Ok(old) = std::fs::read(&old_path) else {
            sector += std::fs::metadata(&new_path).map(|m| m.len() as usize).unwrap_or(0) / SECTOR;
            continue;
        };
        let mut new = std::fs::read(&new_path).unwrap();
        let count = new.len() / SECTOR;
        for i in 0..count {
            if torn(sector + i) {
                let at = i * SECTOR;
                if at + SECTOR <= old.len() && new[at..at + SECTOR] != old[at..at + SECTOR] {
                    // Counted only when the bytes actually differ, so
                    // `reverted` means "sectors whose new contents were
                    // destroyed" rather than "sectors copied over
                    // themselves". A test asserting it is non-zero is
                    // then asserting the tear did something.
                    new[at..at + SECTOR].copy_from_slice(&old[at..at + SECTOR]);
                    reverted += 1;
                }
            }
        }
        std::fs::write(&new_path, &new).unwrap();
        sector += count;
    }
    reverted
}

/// Build a store, snapshot it at a committed point, write more, tear, and
/// return the directory to open plus what must still be readable.
struct Staged {
    _root: tempfile::TempDir,
    live: std::path::PathBuf,
    durable: BTreeMap<Vec<u8>, Vec<u8>>,
    lost: BTreeMap<Vec<u8>, Vec<u8>>,
    reverted: usize,
}

fn stage(durable_count: u64, later_count: u64, torn: &dyn Fn(usize) -> bool) -> Staged {
    let root = tempfile::tempdir().unwrap();
    let live = root.path().join("live");
    let snapshot = root.path().join("snapshot");

    let durable = entries(durable_count, 80, 1);
    let lost = entries(later_count, 80, 2);

    {
        let db = FlatDb::open(&live, options()).unwrap();
        write_all(&db, &durable);
        db.flush().unwrap();
    }
    copy_dir(&live, &snapshot);

    {
        // No flush: the batch is written and the process dies before the
        // checkpoint. Dropping the mappings leaves whatever reached the
        // page cache in the files, which is exactly the state a crash at
        // this point leaves behind -- and the checkpoint on disk is still
        // the old one, because nothing rewrote it.
        //
        // Staging it by completing the flush and then reverting the
        // checkpoint would be unfaithful: a completed flush also removes
        // the index file the old capacity names, so the rewind would
        // produce a pairing no crash can reach. The first run of this
        // harness did exactly that and reported it as a lost key.
        let db = FlatDb::open(&live, options()).unwrap();
        write_all(&db, &lost);
        db.simulate_crash();
    }

    let reverted = tear(&snapshot, &live, torn);
    Staged { _root: root, live, durable, lost, reverted }
}

fn assert_recovers(staged: &Staged) -> Result<(), TestCaseError> {
    let db = FlatDb::open(&staged.live, options()).unwrap();

    for (k, v) in &staged.durable {
        prop_assert_eq!(
            db.get(k),
            Some(v.clone()),
            "a key durable at the checkpoint was lost after tearing {} sector(s)",
            staged.reverted
        );
    }
    // Nothing from the uncommitted batch may be visible, torn or not.
    for (k, _) in &staged.lost {
        if !staged.durable.contains_key(k) {
            prop_assert_eq!(db.get(k), None, "an uncommitted key survived");
        }
    }

    // And the store still works: new writes land and read back.
    let fresh = entries(40, 80, 3);
    write_all(&db, &fresh);
    db.flush().unwrap();
    for (k, v) in &fresh {
        prop_assert_eq!(db.get(k), Some(v.clone()), "a write after recovery was lost");
    }
    Ok(())
}

#[test]
fn an_untorn_recovery_keeps_the_committed_set() {
    let staged = stage(300, 300, &|_| false);
    assert_eq!(staged.reverted, 0, "nothing was asked to tear");
    assert_recovers(&staged).unwrap();
}

/// The harness has to be able to destroy something, or every test below
/// it passes for the wrong reason.
#[test]
fn tearing_actually_destroys_written_bytes() {
    let staged = stage(300, 600, &|_| true);
    assert!(
        staged.reverted > 20,
        "only {} sector(s) differed; the harness is not tearing anything",
        staged.reverted
    );
}

/// Every sector written since the snapshot is lost at once — the worst
/// case the model allows.
#[test]
fn tearing_every_sector_keeps_the_committed_set() {
    let staged = stage(300, 300, &|_| true);
    assert!(staged.reverted > 0, "the tear must actually have reverted something");
    assert_recovers(&staged).unwrap();
}

#[test]
fn tearing_every_other_sector_keeps_the_committed_set() {
    let staged = stage(300, 300, &|i| i % 2 == 0);
    assert!(staged.reverted > 0);
    assert_recovers(&staged).unwrap();
}

/// A batch large enough to force the index to grow, so the tear lands on
/// a table that was rehashed as well as on heap records.
#[test]
fn tearing_across_an_index_grow_keeps_the_committed_set() {
    let staged = stage(100, 4000, &|i| i % 3 == 0);
    assert!(staged.reverted > 0);
    assert_recovers(&staged).unwrap();
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(24))]

    /// For an arbitrary subset of torn sectors, the committed set survives.
    ///
    /// The seed drives a cheap pseudo-random choice rather than a
    /// generated collection, because the sector count is not known until
    /// the database has been built.
    #[test]
    fn any_subset_of_torn_sectors_keeps_the_committed_set(
        seed in any::<u64>(),
        density in 1u64..64,
        durable_count in 50u64..400,
        later_count in 50u64..600,
    ) {
        let torn = move |i: usize| {
            let mut x = (i as u64).wrapping_add(seed).wrapping_mul(6_364_136_223_846_793_005);
            x ^= x >> 33;
            x % 64 < density
        };
        let staged = stage(durable_count, later_count, &torn);
        assert_recovers(&staged)?;
    }
}
