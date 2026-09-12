//! Writing and opening a standalone copy of one Unitrie state.
//!
//! A full scan of RSK mainnet's live state takes ~48 minutes, almost all of it
//! spent pulling 12M nodes out of a 129 GB store where they make up 0.87% of
//! the entries. The nodes themselves are under a gigabyte. Copying them out
//! once, into a database that holds nothing else, turns every later scan into a
//! dense sequential read of a small file instead of a sparse random read of a
//! large one -- and makes the state immutable, so repeated runs measure the
//! same thing even while the node keeps syncing.
//!
//! The copy is content-addressed exactly like the source: key = node hash,
//! value = the node's serialised message, plus one entry per long value keyed
//! by its own hash. That is the whole schema, so `RocksDbTrieStore` opens a
//! snapshot directly with no translation.
//!
//! What a snapshot does *not* carry is the state root, and without it the
//! contents are unreachable -- every key is a hash, so there is no way to tell
//! which one is the root by looking. That is what the metadata file is for.

use alloy_primitives::B256;
use anyhow::{bail, Context, Result};
use rocksdb::{ColumnFamilyDescriptor, Options, WriteBatch, DB};
use std::path::{Path, PathBuf};
use tracing::info;

const CF_TRIE: &str = "trie_nodes";

/// Name of the metadata file inside a snapshot directory.
///
/// Kept as a plain file rather than a row in the database so that the root is
/// readable without opening RocksDB -- including by a human, and by tooling
/// that wants to identify a directory before committing to it. RocksDB ignores
/// files it does not recognise in its own directory.
pub const META_FILE: &str = "trie_snapshot.json";

/// Bytes buffered before a write batch is committed.
const BATCH_BYTES: usize = 32 * 1024 * 1024;

/// What a snapshot directory records about itself.
#[derive(Debug, Clone)]
pub struct SnapshotMeta {
    /// The state root. Without this the snapshot is unreadable.
    pub root: B256,
    /// Block the root came from, when known.
    pub block: Option<u64>,
    /// Where it was copied from.
    pub source: String,
    /// Distinct nodes written.
    pub nodes: u64,
    /// Long values written as their own entries.
    pub long_values: u64,
    /// Total bytes of node messages and values written.
    pub bytes: u64,
    /// RFC3339-ish UTC timestamp.
    pub created: String,
}

impl SnapshotMeta {
    fn to_json(&self) -> String {
        format!(
            "{{\n  \"root\": \"{:?}\",\n  \"block\": {},\n  \"source\": \"{}\",\n  \
             \"nodes\": {},\n  \"long_values\": {},\n  \"bytes\": {},\n  \
             \"created\": \"{}\",\n  \"schema\": \"rustock-trie-snapshot-1\"\n}}\n",
            self.root,
            self.block.map(|b| b.to_string()).unwrap_or_else(|| "null".into()),
            self.source.replace('\\', "\\\\").replace('"', "\\\""),
            self.nodes,
            self.long_values,
            self.bytes,
            self.created,
        )
    }

    fn parse(text: &str) -> Result<Self> {
        // Deliberately a hand-rolled reader for a file this module writes
        // itself: pulling in a JSON dependency for eight fixed fields is not
        // worth it, and the failure mode we care about (a directory that is not
        // a snapshot) is caught by the missing-field check either way.
        let field = |name: &str| -> Option<String> {
            let pat = format!("\"{name}\":");
            let start = text.find(&pat)? + pat.len();
            let rest = text[start..].trim_start();
            let val = if let Some(stripped) = rest.strip_prefix('"') {
                stripped.split('"').next()?.to_string()
            } else {
                rest.split(|c: char| c == ',' || c == '\n' || c == '}')
                    .next()?
                    .trim()
                    .to_string()
            };
            Some(val)
        };
        let root_s = field("root").context("snapshot metadata has no \"root\"")?;
        let root_s = root_s.trim_start_matches("0x");
        let raw = hex_decode(root_s).context("snapshot \"root\" is not hex")?;
        if raw.len() != 32 {
            bail!("snapshot \"root\" is {} bytes, expected 32", raw.len());
        }
        Ok(SnapshotMeta {
            root: B256::from_slice(&raw),
            block: field("block").and_then(|b| b.parse().ok()),
            source: field("source").unwrap_or_default(),
            nodes: field("nodes").and_then(|b| b.parse().ok()).unwrap_or(0),
            long_values: field("long_values").and_then(|b| b.parse().ok()).unwrap_or(0),
            bytes: field("bytes").and_then(|b| b.parse().ok()).unwrap_or(0),
            created: field("created").unwrap_or_default(),
        })
    }
}

fn hex_decode(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect()
}

/// Reads the metadata beside a snapshot database.
pub fn read_meta(dir: &str) -> Result<SnapshotMeta> {
    let path = Path::new(dir).join(META_FILE);
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("{} is not a trie snapshot: cannot read {}", dir, path.display()))?;
    SnapshotMeta::parse(&text)
}

/// True if the directory looks like a snapshot rather than a node datadir.
pub fn is_snapshot(dir: &str) -> bool {
    Path::new(dir).join(META_FILE).is_file()
}

/// Accumulates nodes into a new database as a scan discovers them.
pub struct SnapshotWriter {
    db: DB,
    dir: PathBuf,
    batch: WriteBatch,
    pending_bytes: usize,
    pub nodes: u64,
    pub long_values: u64,
    pub bytes: u64,
}

impl SnapshotWriter {
    /// Creates the target database.
    ///
    /// Refuses to write into an existing directory unless `overwrite` is set:
    /// a snapshot written on top of unrelated data would look valid -- every
    /// key is a hash, so nothing would collide or complain -- while carrying
    /// whatever was there before. Failing is the only way to notice.
    pub fn create(dir: &str, overwrite: bool) -> Result<Self> {
        let path = PathBuf::from(dir);
        if path.exists() {
            if !overwrite {
                bail!(
                    "{} already exists; pass the overwrite flag to replace it",
                    path.display()
                );
            }
            info!(target: "rustock::snapshot", "Removing existing {}", path.display());
            std::fs::remove_dir_all(&path)
                .with_context(|| format!("removing {}", path.display()))?;
        }

        let mut opts = Options::default();
        opts.create_if_missing(true);
        opts.create_missing_column_families(true);
        // The writes are a single bulk load of content-addressed keys in
        // discovery (i.e. random) order, then never touched again. Large
        // memtables keep that from turning into many small sorted runs.
        opts.set_write_buffer_size(256 * 1024 * 1024);
        opts.set_max_write_buffer_number(4);
        opts.increase_parallelism(
            std::thread::available_parallelism().map(|n| n.get() as i32).unwrap_or(4),
        );

        let db = DB::open_cf_descriptors(
            &opts,
            &path,
            vec![ColumnFamilyDescriptor::new(CF_TRIE, Options::default())],
        )
        .with_context(|| format!("creating snapshot database at {}", path.display()))?;

        Ok(Self {
            db,
            dir: path,
            batch: WriteBatch::default(),
            pending_bytes: 0,
            nodes: 0,
            long_values: 0,
            bytes: 0,
        })
    }

    /// Records one node's stored bytes under its hash.
    ///
    /// Only nodes that exist as their own entry in the source belong here.
    /// An embedded node is serialised inside its parent and has no entry of its
    /// own; writing one would add a key the source does not have.
    pub fn put_node(&mut self, hash: B256, message: &[u8]) -> Result<()> {
        self.write(hash, message)?;
        self.nodes += 1;
        Ok(())
    }

    /// Records a value too long to live inside its node (over 32 bytes), which
    /// the trie stores separately keyed by the value's own hash.
    pub fn put_long_value(&mut self, hash: B256, value: &[u8]) -> Result<()> {
        self.write(hash, value)?;
        self.long_values += 1;
        Ok(())
    }

    fn write(&mut self, hash: B256, bytes: &[u8]) -> Result<()> {
        let cf = self
            .db
            .cf_handle(CF_TRIE)
            .context("snapshot is missing the trie_nodes column family")?;
        self.batch.put_cf(cf, hash.as_slice(), bytes);
        self.pending_bytes += bytes.len() + 32;
        self.bytes += bytes.len() as u64;
        if self.pending_bytes >= BATCH_BYTES {
            self.commit()?;
        }
        Ok(())
    }

    fn commit(&mut self) -> Result<()> {
        if self.pending_bytes == 0 {
            return Ok(());
        }
        let batch = std::mem::take(&mut self.batch);
        self.db.write(batch).context("writing snapshot batch")?;
        self.pending_bytes = 0;
        Ok(())
    }

    /// Commits what is buffered, compacts, and writes the metadata file.
    ///
    /// The metadata is written **last**, on purpose: it is what makes a
    /// directory a readable snapshot, so a run that dies partway leaves a
    /// directory that is visibly incomplete rather than one that opens and
    /// silently serves a truncated trie.
    pub fn finish(mut self, root: B256, block: Option<u64>, source: &str) -> Result<SnapshotMeta> {
        self.commit()?;
        self.db.flush().context("flushing snapshot")?;
        info!(target: "rustock::snapshot", "Compacting snapshot");
        self.db.compact_range(None::<&[u8]>, None::<&[u8]>);

        let meta = SnapshotMeta {
            root,
            block,
            source: source.to_string(),
            nodes: self.nodes,
            long_values: self.long_values,
            bytes: self.bytes,
            created: now_utc(),
        };
        let path = self.dir.join(META_FILE);
        std::fs::write(&path, meta.to_json())
            .with_context(|| format!("writing {}", path.display()))?;
        info!(
            target: "rustock::snapshot",
            "Snapshot written to {}: {} nodes, {} long values, root {:?}",
            self.dir.display(), meta.nodes, meta.long_values, root
        );
        Ok(meta)
    }
}

fn now_utc() -> String {
    // Seconds since the epoch, rendered as a UTC calendar date. Avoids pulling
    // in a date library for one timestamp in one metadata file.
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0) as i64;
    let days = secs.div_euclid(86_400);
    let tod = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        tod / 3600,
        (tod % 3600) / 60,
        tod % 60
    )
}

/// Howard Hinnant's days-from-civil, inverted. Exact for the whole range.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meta_round_trips() {
        let m = SnapshotMeta {
            root: B256::repeat_byte(0xAB),
            block: Some(9_233_965),
            source: "/var/lib/rustock".into(),
            nodes: 12_247_350,
            long_values: 745_920,
            bytes: 916_010_000,
            created: "2026-09-12T19:00:00Z".into(),
        };
        let back = SnapshotMeta::parse(&m.to_json()).unwrap();
        assert_eq!(back.root, m.root);
        assert_eq!(back.block, m.block);
        assert_eq!(back.source, m.source);
        assert_eq!(back.nodes, m.nodes);
        assert_eq!(back.long_values, m.long_values);
    }

    #[test]
    fn meta_without_block_round_trips() {
        let m = SnapshotMeta {
            root: B256::repeat_byte(0x01),
            block: None,
            source: "x".into(),
            nodes: 1,
            long_values: 0,
            bytes: 2,
            created: "2026-01-01T00:00:00Z".into(),
        };
        let back = SnapshotMeta::parse(&m.to_json()).unwrap();
        assert_eq!(back.block, None);
        assert_eq!(back.root, m.root);
    }

    #[test]
    fn rejects_a_directory_that_is_not_a_snapshot() {
        assert!(SnapshotMeta::parse("{\"nodes\": 3}").is_err());
    }

    #[test]
    fn create_refuses_to_clobber_without_the_flag() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("snap");
        std::fs::create_dir_all(&p).unwrap();
        let res = SnapshotWriter::create(p.to_str().unwrap(), false);
        assert!(res.is_err(), "an existing directory must not be written into");
        let msg = res.err().map(|e| e.to_string()).unwrap_or_default();
        assert!(msg.contains("already exists"), "{msg}");
    }

    #[test]
    fn civil_from_days_matches_known_dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(19_000), (2022, 1, 8));
    }
}
