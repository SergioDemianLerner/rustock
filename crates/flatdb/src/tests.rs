//! The Java `FlatDbTest` suite, ported, plus the cases Rust makes
//! reachable that Java did not.
//!
//! The porting notes warn that the Java suite ran under a hand-written
//! JUnit shim rather than the project's Gradle build, so it is treated
//! here as a specification of intent: every expected value is re-derived
//! rather than copied.

use super::*;
use std::collections::BTreeMap;

/// A megabyte per segment rather than 512: the boundary paths then run in
/// milliseconds, and the size is part of the format the checkpoint records,
/// so these exercise exactly the production code.
const TEST_SEGMENT: u64 = 1 << 20;

fn options(expected_keys: u64) -> Options {
    Options {
        expected_keys,
        read_only: false,
        sync_writes: false,
        verify_long_records: true,
        segment_size: TEST_SEGMENT,
    }
}

fn open(dir: &Path) -> FlatDb {
    FlatDb::open(dir, options(64)).unwrap()
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

fn key_of(data: &[u8]) -> Vec<u8> {
    Keccak256::digest(data).to_vec()
}

/// A batch of `count` distinct values of `length` bytes each.
fn batch(length: usize, count: u64) -> BTreeMap<Vec<u8>, Vec<u8>> {
    (0..count)
        .map(|i| {
            let data = value(length, 1_000_000 + i);
            (key_of(&data), data)
        })
        .collect()
}

fn write_batch(db: &FlatDb, rows: &BTreeMap<Vec<u8>, Vec<u8>>) {
    let borrowed: Vec<(&[u8], &[u8])> =
        rows.iter().map(|(k, v)| (k.as_slice(), v.as_slice())).collect();
    db.update_batch(borrowed).unwrap();
}

fn present(db: &FlatDb, rows: &BTreeMap<Vec<u8>, Vec<u8>>) -> usize {
    rows.iter().filter(|(k, v)| db.get(k).as_ref() == Some(*v)).count()
}

#[test]
fn stores_values_on_both_sides_of_the_key_storage_threshold() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(dir.path());
    for length in [1usize, 50, STORE_KEY_THRESHOLD - 1, STORE_KEY_THRESHOLD, STORE_KEY_THRESHOLD + 1, 5000] {
        let data = value(length, length as u64);
        db.put(&key_of(&data), &data).unwrap();
        assert_eq!(db.get(&key_of(&data)), Some(data), "length {length}");
    }
}

#[test]
fn missing_key_returns_none() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(dir.path());
    let stored = value(50, 1);
    db.put(&key_of(&stored), &stored).unwrap();
    assert_eq!(db.get(&key_of(&value(50, 2))), None);
}

#[test]
fn values_survive_close_and_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let rows = batch(80, 100);
    {
        let db = open(dir.path());
        write_batch(&db, &rows);
        db.flush().unwrap();
    }
    let reopened = open(dir.path());
    assert_eq!(present(&reopened, &rows), rows.len());
}

#[test]
fn index_grows_beyond_the_declared_capacity() {
    let dir = tempfile::tempdir().unwrap();
    // Declare room for eight and insert a thousand.
    let db = FlatDb::open(dir.path(), options(8)).unwrap();
    let rows = batch(60, 1000);
    write_batch(&db, &rows);
    assert_eq!(present(&db, &rows), rows.len(), "nothing was lost in the rehashes");
    db.flush().unwrap();
    drop(db);

    let reopened = open(dir.path());
    assert_eq!(present(&reopened, &rows), rows.len(), "and it survives a reopen");
}

#[test]
fn repeated_identical_put_is_a_no_op() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(dir.path());
    let data = value(50, 7);
    db.put(&key_of(&data), &data).unwrap();
    let entries = db.entries();
    for _ in 0..10 {
        db.put(&key_of(&data), &data).unwrap();
    }
    assert_eq!(db.entries(), entries, "content addressing: no new entry");
    assert_eq!(db.get(&key_of(&data)), Some(data));
}

/// The first of the three that matter: a batch that was never flushed must
/// vanish in its entirety, not in part.
#[test]
fn unflushed_batch_is_lost_whole_on_crash() {
    let dir = tempfile::tempdir().unwrap();
    let baseline = value(50, 1);
    let rows = batch(300, 200);
    {
        let db = open(dir.path());
        db.put(&key_of(&baseline), &baseline).unwrap();
        db.flush().unwrap();
        write_batch(&db, &rows);
        db.simulate_crash();
    }

    let reopened = open(dir.path());
    assert_eq!(reopened.get(&key_of(&baseline)), Some(baseline));
    assert_eq!(present(&reopened, &rows), 0, "the batch must be absent in its entirety");
    assert_eq!(reopened.keys().len(), 1);
}

/// The subtle one. The heap and the index still hold the batch on disk;
/// only the checkpoint is rewound, exactly as if the crash had landed
/// between writing the index and writing the checkpoint. The batch must be
/// invisible, *and* records later written over the same heap offsets must
/// come out right — which is the self-healing property the whole design
/// rests on.
#[test]
fn slots_left_by_an_interrupted_flush_are_ignored_and_reclaimed() {
    let dir = tempfile::tempdir().unwrap();
    let baseline = value(50, 1);
    let rows = batch(300, 200);
    let checkpoint_before;
    // Room for every entry, so the index never grows. Rewinding a
    // checkpoint across a grow is not a state a crash can produce -- the
    // new table is adopted only by the checkpoint that names it -- and
    // staging it here would test a scenario rather than the mechanism.
    let roomy = || FlatDb::open(dir.path(), options(4096)).unwrap();
    {
        let db = roomy();
        db.put(&key_of(&baseline), &baseline).unwrap();
        db.flush().unwrap();
        checkpoint_before = std::fs::read(dir.path().join(checkpoint::FILE_NAME)).unwrap();

        write_batch(&db, &rows);
        db.flush().unwrap();
        db.simulate_crash();
    }
    std::fs::write(dir.path().join(checkpoint::FILE_NAME), &checkpoint_before).unwrap();

    let reopened = roomy();
    assert_eq!(reopened.get(&key_of(&baseline)), Some(baseline.clone()));
    assert_eq!(present(&reopened, &rows), 0);
    assert_eq!(reopened.keys().len(), 1);

    // New records occupy the offsets the stale slots point at.
    let fresh = batch(900, 200);
    write_batch(&reopened, &fresh);
    reopened.flush().unwrap();
    assert_eq!(present(&reopened, &fresh), fresh.len());
    assert_eq!(present(&reopened, &rows), 0, "the stale batch stayed invisible");
    assert_eq!(reopened.keys().len(), 201);
}

#[test]
fn a_torn_checkpoint_falls_back_to_the_previous_one() {
    let dir = tempfile::tempdir().unwrap();
    let baseline = value(50, 1);
    {
        let db = open(dir.path());
        db.put(&key_of(&baseline), &baseline).unwrap();
        db.flush().unwrap();
        write_batch(&db, &batch(500, 50));
        db.flush().unwrap();
    }

    let path = dir.path().join(checkpoint::FILE_NAME);
    let mut file = std::fs::read(&path).unwrap();
    // The live slot is the one whose sequence is odd or even to match; read
    // it back rather than assuming which.
    let live = Checkpoint::read(dir.path()).unwrap();
    let live_slot = (live.sequence & 1) as usize;
    for byte in file[live_slot * checkpoint::SLOT_SIZE as usize + 20..][..64].iter_mut() {
        *byte ^= 0x5A;
    }
    std::fs::write(&path, &file).unwrap();

    let reopened = open(dir.path());
    assert_eq!(
        reopened.get(&key_of(&baseline)),
        Some(baseline),
        "the previous checkpoint still describes the baseline"
    );
    let fresh = value(50, 4242);
    reopened.put(&key_of(&fresh), &fresh).unwrap();
    reopened.flush().unwrap();
    assert_eq!(reopened.get(&key_of(&fresh)), Some(fresh));
}

#[test]
fn both_checkpoints_destroyed_opens_empty_rather_than_failing() {
    let dir = tempfile::tempdir().unwrap();
    let data = value(50, 1);
    {
        let db = open(dir.path());
        db.put(&key_of(&data), &data).unwrap();
        db.flush().unwrap();
    }
    std::fs::write(
        dir.path().join(checkpoint::FILE_NAME),
        vec![0u8; checkpoint::FILE_SIZE as usize],
    )
    .unwrap();

    let reopened = open(dir.path());
    assert_eq!(reopened.get(&key_of(&data)), None, "nothing is addressable");
    let fresh = value(50, 2);
    reopened.put(&key_of(&fresh), &fresh).unwrap();
    assert_eq!(reopened.get(&key_of(&fresh)), Some(fresh), "and it still works");
}

#[test]
fn sync_writes_survive_a_crash_without_an_explicit_flush() {
    let dir = tempfile::tempdir().unwrap();
    let data = value(50, 1);
    {
        let db = FlatDb::open(
            dir.path(),
            Options { sync_writes: true, ..options(64) },
        )
        .unwrap();
        db.put(&key_of(&data), &data).unwrap();
        db.simulate_crash();
    }
    let reopened = open(dir.path());
    assert_eq!(reopened.get(&key_of(&data)), Some(data));
}

#[test]
fn a_second_open_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let _first = open(dir.path());
    let error = FlatDb::open(dir.path(), options(64))
        .err()
        .expect("the second open must be refused");
    assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
}

#[test]
fn read_only_refuses_writes() {
    let dir = tempfile::tempdir().unwrap();
    let data = value(50, 1);
    {
        let db = open(dir.path());
        db.put(&key_of(&data), &data).unwrap();
        db.flush().unwrap();
    }
    let db = FlatDb::open(dir.path(), Options { read_only: true, ..options(64) }).unwrap();
    assert_eq!(db.get(&key_of(&data)), Some(data.clone()), "reads still work");
    let error = db.put(&key_of(&data), &data).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
}

/// Java's `removalsAreRejectedRatherThanIgnored` covered an API this port
/// does not expose: there is no `delete`, so the compiler enforces what
/// Java could only enforce at runtime. What is left to check is that the
/// store really never shrinks.
#[test]
fn the_store_has_no_way_to_remove_anything() {
    let dir = tempfile::tempdir().unwrap();
    let db = open(dir.path());
    let rows = batch(60, 20);
    write_batch(&db, &rows);
    let before = db.entries();
    write_batch(&db, &rows);
    assert_eq!(db.entries(), before);
    assert_eq!(present(&db, &rows), rows.len());
}

// ---------------------------------------------------------------- beyond Java

/// Readers take no exclusive lock, so this must hold while a writer is
/// active. Run under the thread sanitiser it would also check the
/// release/acquire pairing; here it checks that nothing is lost or torn.
#[test]
fn readers_and_a_writer_can_run_at_once() {
    let dir = tempfile::tempdir().unwrap();
    let db = FlatDb::open(dir.path(), options(4096)).unwrap();
    let settled = batch(70, 500);
    write_batch(&db, &settled);

    let incoming = batch(70, 500);
    std::thread::scope(|scope| {
        scope.spawn(|| {
            for (key, data) in &incoming {
                db.put(key, data).unwrap();
            }
        });
        for _ in 0..4 {
            scope.spawn(|| {
                for _ in 0..20 {
                    for (key, data) in &settled {
                        assert_eq!(
                            db.get(key).as_ref(),
                            Some(data),
                            "a settled value changed under a concurrent writer"
                        );
                    }
                }
            });
        }
    });
    assert_eq!(present(&db, &incoming), incoming.len());
    assert_eq!(present(&db, &settled), settled.len());
}

/// A grow moves every entry to a new table and a new file. Doing that while
/// readers are in flight is the riskiest moment in the store.
#[test]
fn a_grow_under_concurrent_readers_loses_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let db = FlatDb::open(dir.path(), options(8)).unwrap();
    let settled = batch(70, 200);
    write_batch(&db, &settled);

    let incoming = batch(70, 3000);
    std::thread::scope(|scope| {
        scope.spawn(|| {
            for (key, data) in &incoming {
                db.put(key, data).unwrap();
            }
        });
        for _ in 0..3 {
            scope.spawn(|| {
                for _ in 0..30 {
                    for (key, data) in &settled {
                        assert_eq!(db.get(key).as_ref(), Some(data));
                    }
                }
            });
        }
    });
    assert_eq!(present(&db, &settled), settled.len());
    assert_eq!(present(&db, &incoming), incoming.len());
}

/// The index file is named after its capacity precisely so that a grow
/// interrupted before its checkpoint cannot be half-adopted. Simulate that:
/// grow, do not flush, crash, reopen.
#[test]
fn a_grow_that_never_reached_a_checkpoint_is_not_adopted() {
    let dir = tempfile::tempdir().unwrap();
    let settled = batch(70, 100);
    {
        let db = FlatDb::open(dir.path(), options(8)).unwrap();
        write_batch(&db, &settled);
        db.flush().unwrap();

        // Enough to force several more doublings, then lose the process.
        write_batch(&db, &batch(70, 4000));
        db.simulate_crash();
    }

    let reopened = open(dir.path());
    assert_eq!(
        present(&reopened, &settled),
        settled.len(),
        "the committed entries survived a grow that was rolled back"
    );
}

#[test]
fn stale_index_files_are_cleaned_up() {
    let dir = tempfile::tempdir().unwrap();
    {
        let db = FlatDb::open(dir.path(), options(8)).unwrap();
        write_batch(&db, &batch(70, 2000));
        db.flush().unwrap();
    }
    let index_files: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with("index."))
        .collect();
    assert_eq!(index_files.len(), 1, "exactly one live table: {index_files:?}");
}

/// A value large enough to need more than one heap segment's worth of
/// neighbours, so the segment-boundary path runs end to end through the
/// public API.
#[test]
fn values_spanning_several_heap_segments_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let db = FlatDb::open(dir.path(), options(4096)).unwrap();
    let big = value(100_000, 11);
    let mut keys = Vec::new();
    // Well past one segment, so records land on both sides of a boundary
    // and at least one of them has to skip across it.
    for i in 0..60u64 {
        let mut data = big.clone();
        data[..8].copy_from_slice(&i.to_le_bytes());
        db.put(&key_of(&data), &data).unwrap();
        keys.push(key_of(&data));
    }
    let segments: usize = db
        .stats()
        .iter()
        .find_map(|s| s.strip_prefix("heap segments: ").map(|n| n.parse().unwrap()))
        .unwrap();
    assert!(segments >= 2, "the heap spanned {segments} segment(s)");
    for (i, key) in keys.iter().enumerate() {
        let mut expected = big.clone();
        expected[..8].copy_from_slice(&(i as u64).to_le_bytes());
        assert_eq!(db.get(key).as_ref(), Some(&expected), "entry {i}");
    }
}

/// Every offset is `segment * segment_size + position`, so reopening under
/// a different segment size would resolve every record to the wrong bytes.
/// It must be refused, not attempted.
#[test]
fn reopening_with_a_different_segment_size_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let data = value(50, 1);
    {
        let db = open(dir.path());
        db.put(&key_of(&data), &data).unwrap();
        db.flush().unwrap();
    }
    let error = FlatDb::open(
        dir.path(),
        Options { segment_size: TEST_SEGMENT * 2, ..options(64) },
    )
    .err()
    .expect("a different segment size must be refused");
    assert!(error.to_string().contains("resolve to the wrong bytes"), "{error}");

    // And the original size still opens.
    let db = open(dir.path());
    assert_eq!(db.get(&key_of(&data)), Some(data));
}
