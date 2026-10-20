//! Property tests over the index, which the porting notes ask for and the
//! Java suite had no way to express.
//!
//! The invariant under test is the one the whole store rests on: **every
//! key that was put is retrievable, and no key ever shadows another**. Open
//! addressing with no stored keys makes that a real question — a probe
//! resolves collisions by fetching the candidate record and comparing, so
//! a mistake in the probe sequence or in the reclaim logic shows up as a
//! key that silently disappears behind another.

use proptest::prelude::*;
use rustock_flatdb::{FlatDb, Options};
use sha3::{Digest, Keccak256};
use std::collections::BTreeMap;

fn key_of(data: &[u8]) -> Vec<u8> {
    Keccak256::digest(data).to_vec()
}

/// Deliberately small: the index starts at 32 slots so a few dozen values
/// force several rehashes, which is where the interesting failures live.
fn options() -> Options {
    Options {
        expected_keys: 16,
        read_only: false,
        sync_writes: false,
        verify_long_records: true,
        segment_size: 1 << 20,
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]

    /// Insert an arbitrary set of values, then demand every one back.
    #[test]
    fn every_value_put_is_retrievable(
        values in prop::collection::vec(
            prop::collection::vec(any::<u8>(), 0..1400),
            1..160,
        ),
    ) {
        let dir = tempfile::tempdir().unwrap();
        let db = FlatDb::open(dir.path(), options()).unwrap();

        // Content addressing collapses duplicates, so the expectation is a
        // map keyed the same way the store is.
        let expected: BTreeMap<Vec<u8>, Vec<u8>> =
            values.iter().map(|v| (key_of(v), v.clone())).collect();

        for value in &values {
            db.put(&key_of(value), value).unwrap();
        }
        for (key, value) in &expected {
            prop_assert_eq!(db.get(key), Some(value.clone()));
        }
        prop_assert_eq!(db.entries(), expected.len() as u64);
        prop_assert_eq!(db.keys().len(), expected.len());
    }

    /// The same, across a flush and a reopen: what is durable must be
    /// exactly what was there.
    #[test]
    fn everything_survives_a_flush_and_reopen(
        values in prop::collection::vec(
            prop::collection::vec(any::<u8>(), 0..1400),
            1..120,
        ),
    ) {
        let dir = tempfile::tempdir().unwrap();
        let expected: BTreeMap<Vec<u8>, Vec<u8>> =
            values.iter().map(|v| (key_of(v), v.clone())).collect();
        {
            let db = FlatDb::open(dir.path(), options()).unwrap();
            for value in &values {
                db.put(&key_of(value), value).unwrap();
            }
            db.flush().unwrap();
        }
        let db = FlatDb::open(dir.path(), options()).unwrap();
        for (key, value) in &expected {
            prop_assert_eq!(db.get(key), Some(value.clone()), "after reopen");
        }
        prop_assert_eq!(db.keys().len(), expected.len());
    }

    /// Interleaving reads with writes must not change any answer, and keys
    /// that were never put must stay absent however full the table gets.
    #[test]
    fn reads_interleaved_with_writes_stay_consistent(
        values in prop::collection::vec(
            prop::collection::vec(any::<u8>(), 1..200),
            1..120,
        ),
        absent in prop::collection::vec(any::<u8>(), 1..200),
    ) {
        let dir = tempfile::tempdir().unwrap();
        let db = FlatDb::open(dir.path(), options()).unwrap();
        let absent_key = key_of(&absent);
        let mut written: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();

        for value in &values {
            db.put(&key_of(value), value).unwrap();
            written.insert(key_of(value), value.clone());
            // Everything written so far is still there, every step.
            for (key, expected) in &written {
                prop_assert_eq!(db.get(key), Some(expected.clone()));
            }
            if !written.contains_key(&absent_key) {
                prop_assert_eq!(db.get(&absent_key), None);
            }
        }
    }

    /// A crash loses the uncommitted tail and nothing else. Whatever was
    /// flushed must come back whole.
    #[test]
    fn a_crash_loses_exactly_the_unflushed_tail(
        committed in prop::collection::vec(
            prop::collection::vec(any::<u8>(), 1..300),
            1..60,
        ),
        dropped in prop::collection::vec(
            prop::collection::vec(any::<u8>(), 1..300),
            1..60,
        ),
    ) {
        let dir = tempfile::tempdir().unwrap();
        let durable: BTreeMap<Vec<u8>, Vec<u8>> =
            committed.iter().map(|v| (key_of(v), v.clone())).collect();
        {
            let db = FlatDb::open(dir.path(), options()).unwrap();
            for value in &committed {
                db.put(&key_of(value), value).unwrap();
            }
            db.flush().unwrap();
            for value in &dropped {
                db.put(&key_of(value), value).unwrap();
            }
            db.simulate_crash();
        }

        let db = FlatDb::open(dir.path(), options()).unwrap();
        for (key, value) in &durable {
            prop_assert_eq!(db.get(key), Some(value.clone()), "committed value lost");
        }
        for value in &dropped {
            // Unless the same bytes were also in the committed set, in
            // which case content addressing means it was already there.
            if !durable.contains_key(&key_of(value)) {
                prop_assert_eq!(db.get(&key_of(value)), None, "uncommitted value survived");
            }
        }
    }
}
