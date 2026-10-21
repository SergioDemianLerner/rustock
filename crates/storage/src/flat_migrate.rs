//! Moving an epoch store from RocksDB to FlatDB, in place.
//!
//! `flatdb_convert` copies one trie directory, which is what a benchmark
//! needs. A node has an epoch store: several databases, a rotation order,
//! a burial depth, and a head that has to stay coherent with all of it.
//! Without a path from one to the other, choosing FlatDB means resyncing,
//! and nobody will.
//!
//! Three properties, because a migration that loses state is worse than
//! no migration:
//!
//! - **Resumable.** Each epoch is converted into a sibling directory and
//!   marked done. An interrupted run resumes at the epoch it was on, never
//!   starts over, and never half-writes the live store.
//! - **Verified by the root, not by a count.** A node count can match
//!   while the trie is unwalkable. This resolves the executed head's state
//!   root through the converted store and descends it.
//! - **Nothing is replaced until that passes.** The originals are moved
//!   aside, and removed only if the caller asked and the walk succeeded.

use anyhow::{bail, Context, Result};
use alloy_primitives::B256;
use rustock_flatdb::Options;
use rustock_trie::{NodeRef, TrieNode, TrieStore};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

const CF_TRIE: &str = "trie_nodes";
/// Written into a converted epoch once it is complete. Its absence is what
/// makes a half-converted epoch redone rather than trusted.
const DONE_MARKER: &str = "CONVERTED";

/// How thoroughly to check the converted store before swapping it in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verify {
    /// Resolve the state root and descend the whole trie.
    Full,
    /// Resolve the state root only. Fast, and proves much less.
    RootOnly,
}

#[derive(Debug, Default)]
pub struct MigrationReport {
    pub epochs: usize,
    pub converted_nodes: u64,
    pub skipped_epochs: usize,
    /// Entries whose key was not the digest of their value, which FlatDB
    /// cannot address and which were therefore not carried over.
    pub not_content_addressed: u64,
    pub resolved_nodes: u64,
}

/// Convert every epoch under `root`, verify, and swap the converted ones
/// into place.
///
/// `state_root` is what the node had executed to. Without it nothing is
/// verified and nothing is swapped: the converted epochs are left beside
/// the originals for a later run.
/// Sizes the FlatDB stores the migration creates.
///
/// `segment_size` is not a tuning knob here: it is recorded in each
/// store's checkpoint and the node refuses to open one written with a
/// different value, so the migration has to create epochs the node will
/// accept. Defaulting it silently would produce a store that converts
/// cleanly and then cannot be started.
#[derive(Debug, Clone, Copy)]
pub struct Sizing {
    pub expected_keys: u64,
    pub segment_size: u64,
}

impl Default for Sizing {
    fn default() -> Self {
        Self {
            expected_keys: 24_000_000,
            segment_size: rustock_flatdb::heap::DEFAULT_SEGMENT_SIZE,
        }
    }
}

pub fn migrate_epoch_store(
    root: &Path,
    sizing: Sizing,
    state_root: Option<B256>,
    verify: Verify,
    keep_original: bool,
) -> Result<MigrationReport> {
    let epochs = epoch_dirs(root)?;
    if epochs.is_empty() {
        bail!("{} holds no epoch-N directories", root.display());
    }
    let mut report = MigrationReport { epochs: epochs.len(), ..Default::default() };

    for (seq, source) in &epochs {
        let target = converted_path(root, *seq);
        if target.join(DONE_MARKER).exists() {
            report.skipped_epochs += 1;
            continue;
        }
        // A directory without the marker is the wreckage of an interrupted
        // run. Redo it rather than trust it.
        if target.exists() {
            std::fs::remove_dir_all(&target)?;
        }
        let one = convert_one(source, &target, *seq, sizing)?;
        report.converted_nodes += one.0;
        report.not_content_addressed += one.1;
    }

    let Some(state_root) = state_root else {
        return Ok(report);
    };

    let converted = open_converted(root, &epochs, sizing)?;
    report.resolved_nodes = walk(&converted, state_root, verify)?;
    drop(converted);

    swap_in(root, &epochs, keep_original)?;
    Ok(report)
}

pub fn epoch_dirs(root: &Path) -> Result<Vec<(u64, PathBuf)>> {
    let mut found: Vec<(u64, PathBuf)> = std::fs::read_dir(root)
        .with_context(|| format!("reading {}", root.display()))?
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            name.strip_prefix("epoch-")
                .and_then(|s| s.parse::<u64>().ok())
                .map(|seq| (seq, e.path()))
        })
        .collect();
    found.sort_by_key(|(seq, _)| *seq);
    Ok(found)
}

fn converted_path(root: &Path, seq: u64) -> PathBuf {
    root.join(format!("epoch-{seq}.flat"))
}

/// Returns (copied, skipped).
fn convert_one(
    source: &Path,
    target: &Path,
    seq: u64,
    sizing: Sizing,
) -> Result<(u64, u64)> {
    let mut opts = rocksdb::Options::default();
    opts.create_if_missing(false);
    let db = rocksdb::DB::open_cf_for_read_only(&opts, source, [CF_TRIE], false)
        .with_context(|| format!("opening {}", source.display()))?;
    let cf = db
        .cf_handle(CF_TRIE)
        .with_context(|| format!("{} has no trie column family", source.display()))?;

    let flat = crate::FlatTrieStore::open(
        target,
        Options {
            expected_keys: sizing.expected_keys,
            segment_size: sizing.segment_size,
            ..Options::default()
        },
    )?;

    let mut copied = 0u64;
    let mut skipped = 0u64;
    for item in db.iterator_cf(cf, rocksdb::IteratorMode::Start) {
        let (key, value) = item?;
        // FlatDB derives a short record's key from its value, so an entry
        // keyed by anything else would be written and never found again.
        if key.as_ref() != alloy_primitives::keccak256(value.as_ref()).as_slice() {
            skipped += 1;
            continue;
        }
        flat.put(&key, &value);
        copied += 1;
    }
    flat.flush();
    if flat.write_failed() {
        bail!("a write failed converting epoch-{seq}; {} is incomplete", target.display());
    }
    std::fs::write(target.join(DONE_MARKER), format!("{copied}\n"))?;
    Ok((copied, skipped))
}

/// A read-only view of the converted epochs, newest first, exactly as the
/// epoch store reads them.
fn open_converted(
    root: &Path,
    epochs: &[(u64, PathBuf)],
    sizing: Sizing,
) -> Result<Arc<dyn TrieStore>> {
    let mut stores = Vec::new();
    for (seq, _) in epochs {
        stores.push(crate::FlatTrieStore::open(
            converted_path(root, *seq),
            Options {
                expected_keys: sizing.expected_keys,
                segment_size: sizing.segment_size,
                read_only: true,
                ..Options::default()
            },
        )?);
    }
    Ok(Arc::new(Layered { stores }))
}

struct Layered {
    stores: Vec<crate::FlatTrieStore>,
}

impl TrieStore for Layered {
    fn get(&self, key: &[u8]) -> Option<Vec<u8>> {
        self.stores.iter().rev().find_map(|s| s.get(key))
    }
    fn put(&self, _key: &[u8], _value: &[u8]) {
        panic!("the migration's verification view is read-only");
    }
}

/// Walk the trie from `root`, proving every node it names is present.
///
/// A node count matching the source proves nothing: the same count can
/// describe a trie that cannot be walked. Resolving the root and
/// descending is the check that means something.
fn walk(store: &Arc<dyn TrieStore>, root: B256, verify: Verify) -> Result<u64> {
    let Some(bytes) = store.get(root.as_slice()) else {
        bail!(
            "the state root {root:?} is not in the converted store; the migration is \
             incomplete and nothing has been swapped"
        );
    };
    if verify == Verify::RootOnly {
        return Ok(1);
    }

    let mut seen: HashSet<B256> = HashSet::new();
    let mut stack = vec![TrieNode::from_message(&bytes, store.as_ref())];
    seen.insert(root);
    let mut resolved = 1u64;

    while let Some(node) = stack.pop() {
        for child in [&node.left, &node.right] {
            let NodeRef::Hash(hash) = child else { continue };
            if !seen.insert(*hash) {
                continue;
            }
            let Some(bytes) = store.get(hash.as_slice()) else {
                bail!(
                    "node {hash:?} is referenced by the trie but absent from the \
                     converted store. If the source was collected before b0eff4b it \
                     may be missing long values (issue #293); that is a precondition \
                     this migration cannot repair. Nothing has been swapped."
                );
            };
            resolved += 1;
            stack.push(TrieNode::from_message(&bytes, store.as_ref()));
        }
    }
    Ok(resolved)
}

/// Move the originals aside and the converted epochs into place. Only
/// reached once the walk has succeeded.
fn swap_in(root: &Path, epochs: &[(u64, PathBuf)], keep_original: bool) -> Result<()> {
    for (seq, source) in epochs {
        let aside = root.join(format!("epoch-{seq}.rocksdb"));
        std::fs::rename(source, &aside)
            .with_context(|| format!("moving {} aside", source.display()))?;
        std::fs::rename(converted_path(root, *seq), source)
            .with_context(|| format!("moving the converted epoch-{seq} into place"))?;
        // The marker belongs to the migration, not to the store.
        let _ = std::fs::remove_file(source.join(DONE_MARKER));
        if !keep_original {
            std::fs::remove_dir_all(&aside)
                .with_context(|| format!("removing {}", aside.display()))?;
        }
    }
    let _ = Instant::now();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::epoch_store::{EpochConfig, EpochTrieStore};
    use rustock_trie::TrieKeySlice;

    /// Small segments and a small index: segments are reserved whole, so
    /// a production 512 MiB one does not belong in a temporary directory.
    fn sizing() -> Sizing {
        Sizing { expected_keys: 4096, segment_size: 1 << 20 }
    }

    fn config() -> EpochConfig {
        EpochConfig { epochs: 3, burial_depth: 10, rotate_bytes: 32 * 1024 }
    }

    fn trie_key(i: u16) -> Vec<u8> {
        use sha3::{Digest, Keccak256};
        let mut h = Keccak256::new();
        h.update(b"migrate");
        h.update(i.to_be_bytes());
        h.finalize()[..31].to_vec()
    }

    /// Build a RocksDB-backed epoch store holding a real trie, and return
    /// its root. Writes enough to force a rotation, so the migration has
    /// more than one epoch to move.
    fn build_source(root: &Path, count: u16) -> B256 {
        let store = EpochTrieStore::open(root, config(), None).unwrap();
        let backing: Arc<dyn TrieStore> = Arc::new(store);
        let mut trie = TrieNode::empty();
        for i in 0..count {
            trie = trie.put(
                &TrieKeySlice::from_key(&trie_key(i)),
                &[(i % 251) as u8; 60],
                backing.as_ref(),
            );
        }
        trie.save(backing.as_ref(), true);
        backing.flush();
        trie.compute_hash(backing.as_ref())
    }

    fn assert_reads_back(root: &Path, state_root: B256, count: u16) {
        let store = EpochTrieStore::open_with(
            root,
            config(),
            None,
            Arc::new(
                crate::epoch_backend::FlatFactory::new(sizing().expected_keys)
                    .with_segment_size(sizing().segment_size),
            ),
        )
        .unwrap();
        let backing: Arc<dyn TrieStore> = Arc::new(store);
        let node = TrieNode::from_message(
            &backing.get(state_root.as_slice()).expect("the root is present"),
            backing.as_ref(),
        );
        for i in 0..count {
            assert_eq!(
                node.get(&TrieKeySlice::from_key(&trie_key(i)), backing.as_ref()).as_deref(),
                Some(&[(i % 251) as u8; 60][..]),
                "key {i} after migration"
            );
        }
    }

    #[test]
    fn a_migrated_epoch_store_serves_the_same_trie() {
        let dir = tempfile::tempdir().unwrap();
        let state_root = build_source(dir.path(), 400);

        let report = migrate_epoch_store(
            dir.path(),
            sizing(),
            Some(state_root),
            Verify::Full,
            false,
        )
        .unwrap();

        assert!(report.epochs >= 1);
        assert!(report.converted_nodes > 0);
        assert_eq!(report.not_content_addressed, 0, "a trie store is content-addressed");
        assert!(report.resolved_nodes > 1, "the walk descended past the root");
        assert_reads_back(dir.path(), state_root, 400);
    }

    /// The originals go away only when asked. A migration that deleted
    /// them before anyone had looked would have no way back.
    #[test]
    fn keeping_the_originals_leaves_them_beside_the_converted_store() {
        let dir = tempfile::tempdir().unwrap();
        let state_root = build_source(dir.path(), 200);
        migrate_epoch_store(dir.path(), sizing(), Some(state_root), Verify::Full, true).unwrap();

        let kept: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".rocksdb"))
            .collect();
        assert!(!kept.is_empty(), "the originals were removed despite keep_original");
        assert_reads_back(dir.path(), state_root, 200);
    }

    /// An interrupted run must resume, not start over, and not leave a
    /// half-converted epoch in place of a good one.
    #[test]
    fn an_interrupted_migration_resumes() {
        let dir = tempfile::tempdir().unwrap();
        let state_root = build_source(dir.path(), 400);

        // First pass: convert, but stop before verifying or swapping.
        let first = migrate_epoch_store(dir.path(), sizing(), None, Verify::Full, false).unwrap();
        assert!(first.converted_nodes > 0);
        assert_eq!(first.skipped_epochs, 0);

        // Damage one converted epoch the way an interruption would: the
        // directory is there, the marker is not.
        let epochs = epoch_dirs(dir.path()).unwrap();
        let victim = converted_path(dir.path(), epochs[0].0);
        std::fs::remove_file(victim.join(DONE_MARKER)).unwrap();

        // Second pass: redoes the damaged one, skips the rest, finishes.
        let second =
            migrate_epoch_store(dir.path(), sizing(), Some(state_root), Verify::Full, false).unwrap();
        assert_eq!(
            second.skipped_epochs,
            epochs.len() - 1,
            "only the epoch missing its marker should be redone"
        );
        assert_reads_back(dir.path(), state_root, 400);
    }

    /// A state root the converted store cannot resolve must stop the
    /// migration with the originals untouched.
    #[test]
    fn an_unresolvable_root_refuses_to_swap() {
        let dir = tempfile::tempdir().unwrap();
        build_source(dir.path(), 200);
        let before: Vec<_> = epoch_dirs(dir.path()).unwrap();

        let error = migrate_epoch_store(
            dir.path(),
            sizing(),
            Some(B256::repeat_byte(0xAB)),
            Verify::Full,
            false,
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("nothing has been swapped"), "{error}");

        for (_, path) in &before {
            assert!(path.exists(), "an original epoch was moved despite the failure");
        }
    }

    /// Root-only verification resolves the root and stops, which is much
    /// cheaper and proves much less -- so it must still refuse a root it
    /// cannot find.
    #[test]
    fn root_only_verification_still_catches_a_missing_root() {
        let dir = tempfile::tempdir().unwrap();
        build_source(dir.path(), 100);
        assert!(migrate_epoch_store(
            dir.path(),
            sizing(),
            Some(B256::repeat_byte(0x11)),
            Verify::RootOnly,
            false,
        )
        .is_err());
    }
}
