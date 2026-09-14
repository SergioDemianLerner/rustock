//! Drops the `trie_nodes` column family from a node database.
//!
//! Only meaningful after the trie has been moved out with `detach_trie` and the
//! node has been verified running against the detached store. In RocksDB each
//! SST belongs to exactly one column family, so dropping one frees its files
//! directly rather than waiting for compaction to rewrite them.
//!
//! **This is irreversible.** The node must be stopped, and there must be another
//! copy of the trie -- a detached store, a snapshot, or the source the database
//! was imported from.
//!
//! After dropping, the node must NOT be started with `--trie-backend single`:
//! the column family would be recreated empty and the node would come up with no
//! state at all rather than failing loudly. Use `external` or `epoch`.
//!
//! ```text
//! drop_trie_cf /var/lib/rustock --i-have-a-backup
//! ```

use anyhow::{bail, Context, Result};

fn dir_size(p: &std::path::Path) -> u64 {
    let mut t = 0;
    if let Ok(rd) = std::fs::read_dir(p) {
        for e in rd.flatten() {
            match e.metadata() {
                Ok(m) if m.is_file() => t += m.len(),
                Ok(m) if m.is_dir() => t += dir_size(&e.path()),
                _ => {}
            }
        }
    }
    t
}

fn main() -> Result<()> {
    let mut a = std::env::args().skip(1);
    let dir = a.next().context("usage: drop_trie_cf <datadir> --i-have-a-backup")?;
    let confirmed = a.next().map(|s| s == "--i-have-a-backup").unwrap_or(false);
    if !confirmed {
        bail!(
            "refusing to drop trie_nodes without --i-have-a-backup.\n\
             There must be another copy of the trie first: a detached store from \
             detach_trie, a snapshot, or the database this one was imported from."
        );
    }

    let before = dir_size(std::path::Path::new(&dir));
    println!("database before: {:.1} GB", before as f64 / 1e9);

    let mut o = rocksdb::Options::default();
    o.create_if_missing(false);
    // Do not recreate what we are about to remove.
    o.create_missing_column_families(false);
    o.set_max_open_files(512);
    let cfs = ["headers", "block_numbers", "total_difficulty", "block_bodies",
               "receipts", "tx_index", "trie_nodes"];
    let mut db = rocksdb::DB::open_cf(&o, &dir, cfs).with_context(|| format!("opening {dir}"))?;

    println!("dropping trie_nodes");
    db.drop_cf("trie_nodes").context("dropping trie_nodes")?;
    // Nudge RocksDB to release the files now rather than at the next compaction.
    db.flush().ok();
    drop(db);

    let after = dir_size(std::path::Path::new(&dir));
    println!("database after:  {:.1} GB", after as f64 / 1e9);
    println!("reclaimed:       {:.1} GB", (before.saturating_sub(after)) as f64 / 1e9);
    println!();
    println!("The node must now run with --trie-backend external or epoch.");
    println!("--trie-backend single would recreate the column family empty.");
    Ok(())
}
