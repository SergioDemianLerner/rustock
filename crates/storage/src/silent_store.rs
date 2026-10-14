//! A derived record of Bridge decisions that emit no event.
//!
//! Its own database, deliberately: these rows are reproducible from blocks
//! plus a state source, are never read during execution, and should be
//! deletable without consequence. Keeping them beside consensus data would
//! invite both assumptions.
//!
//! Keys mirror the Bridge event index -- `kind ‖ block ‖ tx_index ‖ seq` --
//! so scanning one kind is a prefix walk, as it is for a topic.
//!
//! Two things are stored besides the records, and they are what make the
//! database answerable rather than merely full:
//!
//! - **Coverage**: which block ranges were actually replayed. Without it,
//!   "no refunds in #1..#2,392,699" cannot be told apart from "never looked",
//!   and the second masquerading as the first is how a wrong answer gets
//!   believed.
//! - **Provenance**: which build produced each range. A replay is only as
//!   good as the code that ran it, and this session has twice found data that
//!   looked authoritative and came from a binary that was wrong at the time.

use anyhow::{Context, Result};
use rocksdb::{ColumnFamilyDescriptor, Options, WriteBatch, DB};

const CF_RECORDS: &str = "records";
const CF_META: &str = "meta";

/// One recorded decision, as handed over by the execution crate.
pub struct SilentRow {
    pub kind: String,
    pub block: u64,
    pub tx_index: u32,
    pub json: String,
}

/// A replayed range and the build that replayed it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Coverage {
    pub from: u64,
    pub to: u64,
    pub build: String,
    pub recorded_at: u64,
    /// Blocks whose computed state root matched the header. A range with
    /// mismatches is recorded rather than refused -- but a reader must be
    /// able to see it, because records from a diverging replay describe this
    /// implementation's decisions, not the chain's.
    pub verified: u64,
    pub diverged: u64,
}

pub struct SilentStore {
    db: DB,
}

fn kind_prefix(kind: &str) -> [u8; 32] {
    alloy_primitives::keccak256(kind.as_bytes()).0
}

fn record_key(kind: &str, block: u64, tx_index: u32, seq: u32) -> Vec<u8> {
    let mut k = Vec::with_capacity(48);
    k.extend_from_slice(&kind_prefix(kind));
    k.extend_from_slice(&block.to_be_bytes());
    k.extend_from_slice(&tx_index.to_be_bytes());
    k.extend_from_slice(&seq.to_be_bytes());
    k
}

impl SilentStore {
    pub fn open(path: &str) -> Result<Self> {
        let mut opts = Options::default();
        opts.create_if_missing(true);
        opts.create_missing_column_families(true);
        let cfs = vec![
            ColumnFamilyDescriptor::new(CF_RECORDS, Options::default()),
            ColumnFamilyDescriptor::new(CF_META, Options::default()),
        ];
        let db = DB::open_cf_descriptors(&opts, path, cfs)
            .with_context(|| format!("opening the silent-decision store at {path}"))?;
        Ok(Self { db })
    }

    pub fn open_read_only(path: &str) -> Result<Self> {
        let mut opts = Options::default();
        opts.create_if_missing(false);
        let db = DB::open_cf_for_read_only(&opts, path, [CF_RECORDS, CF_META], false)
            .with_context(|| format!("opening {path} read-only"))?;
        Ok(Self { db })
    }

    pub fn put_records(&self, rows: &[SilentRow]) -> Result<()> {
        let cf = self.db.cf_handle(CF_RECORDS).context("records cf")?;
        let mut batch = WriteBatch::default();
        // `seq` separates records that share a kind, block and transaction --
        // one registration can refund one peg-in and mark another processed.
        let mut seq_at: std::collections::HashMap<(String, u64, u32), u32> = Default::default();
        for r in rows {
            let e = seq_at
                .entry((r.kind.clone(), r.block, r.tx_index))
                .or_insert(0);
            batch.put_cf(
                cf,
                record_key(&r.kind, r.block, r.tx_index, *e),
                r.json.as_bytes(),
            );
            *e += 1;
        }
        self.db.write(batch).context("writing silent records")
    }

    /// Every record of one kind, in block order, optionally bounded.
    pub fn scan(&self, kind: &str, from: u64, to: u64) -> Result<Vec<(u64, u32, String)>> {
        let cf = self.db.cf_handle(CF_RECORDS).context("records cf")?;
        let prefix = kind_prefix(kind);
        let mut start = Vec::with_capacity(40);
        start.extend_from_slice(&prefix);
        start.extend_from_slice(&from.to_be_bytes());
        let mut out = Vec::new();
        for item in self.db.iterator_cf(
            cf,
            rocksdb::IteratorMode::From(&start, rocksdb::Direction::Forward),
        ) {
            let (k, v) = item?;
            if k.len() < 44 || k[..32] != prefix {
                break;
            }
            let block = u64::from_be_bytes(k[32..40].try_into().unwrap());
            if block > to {
                break;
            }
            let tx_index = u32::from_be_bytes(k[40..44].try_into().unwrap());
            out.push((block, tx_index, String::from_utf8_lossy(&v).into_owned()));
        }
        Ok(out)
    }

    pub fn add_coverage(&self, c: &Coverage) -> Result<()> {
        let cf = self.db.cf_handle(CF_META).context("meta cf")?;
        let mut key = b"coverage:".to_vec();
        key.extend_from_slice(&c.from.to_be_bytes());
        let value = format!(
            "{{\"from\":{},\"to\":{},\"build\":{:?},\"recordedAt\":{},\"verified\":{},\"diverged\":{}}}",
            c.from, c.to, c.build, c.recorded_at, c.verified, c.diverged
        );
        self.db
            .put_cf(cf, key, value.as_bytes())
            .context("writing coverage")
    }

    /// Notes a block that could not be recorded reliably, and why. A hole in
    /// coverage is a fact about the replay, not an absence of one -- without
    /// this, a reader can see that a block is uncovered but not whether it
    /// diverged, was missing, or was simply never reached.
    pub fn add_gap(&self, block: u64, why: &str) -> Result<()> {
        let cf = self.db.cf_handle(CF_META).context("meta cf")?;
        let mut key = b"gap:".to_vec();
        key.extend_from_slice(&block.to_be_bytes());
        self.db
            .put_cf(
                cf,
                key,
                format!("{{\"block\":{block},\"why\":{why:?}}}").as_bytes(),
            )
            .context("writing a coverage gap")
    }

    pub fn gaps(&self) -> Result<Vec<String>> {
        let cf = self.db.cf_handle(CF_META).context("meta cf")?;
        let mut out = Vec::new();
        for item in self.db.iterator_cf(cf, rocksdb::IteratorMode::Start) {
            let (k, v) = item?;
            if k.starts_with(b"gap:") {
                out.push(String::from_utf8_lossy(&v).into_owned());
            }
        }
        Ok(out)
    }

    pub fn coverage(&self) -> Result<Vec<String>> {
        let cf = self.db.cf_handle(CF_META).context("meta cf")?;
        let mut out = Vec::new();
        for item in self.db.iterator_cf(cf, rocksdb::IteratorMode::Start) {
            let (k, v) = item?;
            if k.starts_with(b"coverage:") {
                out.push(String::from_utf8_lossy(&v).into_owned());
            }
        }
        Ok(out)
    }

    /// Whether every block in `from..=to` has been replayed. Answering a
    /// "has X ever happened" question over a range this returns false for is
    /// unsound, and the caller should say so rather than report zero.
    pub fn covers(&self, from: u64, to: u64) -> Result<bool> {
        let mut spans: Vec<(u64, u64)> = Vec::new();
        for c in self.coverage()? {
            let g = |k: &str| -> Option<u64> {
                let i = c.find(&format!("\"{k}\":"))? + k.len() + 3;
                c[i..]
                    .split(|ch: char| !ch.is_ascii_digit())
                    .next()?
                    .parse()
                    .ok()
            };
            if let (Some(a), Some(b)) = (g("from"), g("to")) {
                spans.push((a, b));
            }
        }
        spans.sort_unstable();
        let mut reached = from;
        for (a, b) in spans {
            if a > reached {
                break;
            }
            if b >= reached {
                reached = b + 1;
            }
            if reached > to {
                return Ok(true);
            }
        }
        Ok(reached > to)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, SilentStore) {
        let d = tempfile::tempdir().unwrap();
        let s = SilentStore::open(d.path().to_str().unwrap()).unwrap();
        (d, s)
    }

    #[test]
    fn records_come_back_by_kind_and_range() {
        let (_d, s) = store();
        s.put_records(&[
            SilentRow {
                kind: "pegin_refunded".into(),
                block: 100,
                tx_index: 1,
                json: "{\"a\":1}".into(),
            },
            SilentRow {
                kind: "pegin_refunded".into(),
                block: 300,
                tx_index: 0,
                json: "{\"a\":2}".into(),
            },
            SilentRow {
                kind: "pegin_accepted".into(),
                block: 200,
                tx_index: 0,
                json: "{\"a\":3}".into(),
            },
        ])
        .unwrap();

        let r = s.scan("pegin_refunded", 0, u64::MAX).unwrap();
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].0, 100);
        assert_eq!(r[1].0, 300);
        assert_eq!(s.scan("pegin_refunded", 150, 400).unwrap().len(), 1);
        assert_eq!(s.scan("pegin_accepted", 0, u64::MAX).unwrap().len(), 1);
        assert!(s.scan("never_used", 0, u64::MAX).unwrap().is_empty());
    }

    /// Several records of one kind in one transaction must not overwrite each
    /// other: a registration can refund one peg-in and accept another.
    #[test]
    fn records_sharing_a_slot_are_all_kept() {
        let (_d, s) = store();
        s.put_records(&[
            SilentRow {
                kind: "pegin_refunded".into(),
                block: 5,
                tx_index: 2,
                json: "{\"n\":1}".into(),
            },
            SilentRow {
                kind: "pegin_refunded".into(),
                block: 5,
                tx_index: 2,
                json: "{\"n\":2}".into(),
            },
        ])
        .unwrap();
        assert_eq!(s.scan("pegin_refunded", 0, u64::MAX).unwrap().len(), 2);
    }

    /// The point of coverage: silence must be distinguishable from absence.
    #[test]
    fn an_unscanned_range_is_not_reported_as_covered() {
        let (_d, s) = store();
        assert!(!s.covers(1, 100).unwrap(), "nothing scanned yet");

        s.add_coverage(&Coverage {
            from: 1,
            to: 100,
            build: "abc123".into(),
            recorded_at: 0,
            verified: 100,
            diverged: 0,
        })
        .unwrap();
        assert!(s.covers(1, 100).unwrap());
        assert!(s.covers(10, 50).unwrap());
        assert!(!s.covers(1, 101).unwrap(), "one block past the end");
        assert!(!s.covers(101, 200).unwrap());
    }

    #[test]
    fn adjacent_ranges_join_and_a_hole_does_not() {
        let (_d, s) = store();
        for (a, b) in [(1u64, 100u64), (101, 200), (300, 400)] {
            s.add_coverage(&Coverage {
                from: a,
                to: b,
                build: "x".into(),
                recorded_at: 0,
                verified: b - a + 1,
                diverged: 0,
            })
            .unwrap();
        }
        assert!(s.covers(1, 200).unwrap(), "adjacent ranges join");
        assert!(
            !s.covers(1, 400).unwrap(),
            "the gap 201..299 is not covered"
        );
        assert!(s.covers(300, 400).unwrap());
    }

    /// A hole must say why it is there.
    #[test]
    fn a_gap_is_recorded_with_its_reason() {
        let (_d, s) = store();
        s.add_coverage(&Coverage {
            from: 1,
            to: 99,
            build: "x".into(),
            recorded_at: 0,
            verified: 99,
            diverged: 0,
        })
        .unwrap();
        s.add_gap(100, "state root diverged").unwrap();
        assert!(
            !s.covers(1, 100).unwrap(),
            "the diverged block is not covered"
        );
        let g = s.gaps().unwrap();
        assert_eq!(g.len(), 1);
        assert!(
            g[0].contains("\"block\":100") && g[0].contains("diverged"),
            "{}",
            g[0]
        );
    }

    #[test]
    fn coverage_records_the_build_and_whether_it_diverged() {
        let (_d, s) = store();
        s.add_coverage(&Coverage {
            from: 1,
            to: 10,
            build: "deadbeef".into(),
            recorded_at: 1760000000,
            verified: 9,
            diverged: 1,
        })
        .unwrap();
        let c = s.coverage().unwrap();
        assert_eq!(c.len(), 1);
        assert!(c[0].contains("\"build\":\"deadbeef\""));
        assert!(
            c[0].contains("\"diverged\":1"),
            "a diverging replay must be visible: {}",
            c[0]
        );
    }
}
