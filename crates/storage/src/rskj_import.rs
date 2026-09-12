//! Import an rskj database into rustock's store.
//!
//! # Why this is a conversion and not a copy
//!
//! The two nodes store the same chain in structurally different ways:
//!
//! | | rskj | rustock |
//! |---|---|---|
//! | container | one RocksDB **per datasource directory** (`blocks/`, `unitrie/`, `receipts/`) | one RocksDB with column families |
//! | blocks | `hash -> RLP(header, txs, uncles)` in a single value | split across `headers` and `block_bodies` |
//! | index | **MapDB file** holding Java-serialised `BlockInfo` | `block_numbers` + `total_difficulty` CFs |
//! | state | `unitrie/`: `nodeHash -> node message` | `trie_nodes` CF, byte-identical values |
//!
//! So the databases cannot be swapped: RocksDB cannot adopt another database's
//! directory as a column family, and the canonical-chain and cumulative
//! difficulty data is not even in a key-value store — it lives in a MapDB file
//! encoded with Java object serialisation.
//!
//! This importer therefore ignores the MapDB index entirely and *derives* the
//! canonical chain by walking `parentHash` back from the tip, and the total
//! difficulty by accumulating header difficulty forward from genesis. That is
//! more work than reading the index, but it needs no Java-format parser and the
//! result is self-consistent rather than trusting a foreign index.
//!
//! The state trie is the one part that copies verbatim: rustock's node encoding
//! must match rskj's exactly or state roots would not agree, so `unitrie`
//! entries move into `trie_nodes` unchanged.

use crate::{encode_body, encode_header, BlockStore};
use alloy_primitives::{B256, U256};
use alloy_rlp::Decodable;
use anyhow::{Context, Result};
use rocksdb::{DB, IteratorMode, Options, WriteBatch, WriteOptions};
use rustock_core::Block;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

/// Set by the SIGTERM/SIGINT handler; polled by every import loop.
static SHUTDOWN: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(_sig: libc::c_int) {
    // Only async-signal-safe work here: set a flag and return. The loops do the
    // flushing, because calling into RocksDB from a signal handler is not safe.
    SHUTDOWN.store(true, Ordering::SeqCst);
}

/// Installs handlers so SIGTERM and SIGINT stop the import at the next batch
/// boundary and flush, instead of discarding whatever is still in memtables.
///
/// This matters because import writes bypass the WAL: anything not yet flushed
/// exists only in RAM, so an abrupt exit silently loses a hash-random subset of
/// entries. A phase can then report success while being incomplete -- which is
/// exactly what happened before this existed, leaving a block store whose chain
/// walk died 59 blocks in.
///
/// SIGKILL cannot be caught, which is why it must never be used on an import.
pub fn install_signal_handlers() {
    unsafe {
        libc::signal(libc::SIGTERM, on_signal as libc::sighandler_t);
        libc::signal(libc::SIGINT, on_signal as libc::sighandler_t);
    }
}

/// True once a termination signal has been received.
pub fn shutdown_requested() -> bool {
    SHUTDOWN.load(Ordering::SeqCst)
}
use tracing::{debug, info, warn};

/// Entries per write batch. Large enough to amortise the write path, small
/// enough that a failure does not discard much work.
const BATCH_SIZE: usize = 4096;

/// How often to report progress.
const REPORT_EVERY: u64 = 250_000;

/// Whether to check for an existing entry before writing it.
///
/// Both datasources are content-addressed -- trie nodes by their hash, blocks
/// by theirs -- so rewriting a key that is already present writes byte-identical
/// data. Skipping is therefore never required for correctness; it is purely a
/// trade of one point lookup per entry against the cost of re-writing entries
/// we already hold.
///
/// Which side wins depends entirely on how much the two databases overlap:
///
/// - importing into an empty or mostly-empty datadir, nearly every lookup
///   misses, so the check is pure overhead (`Unconditional` is faster);
/// - topping up a datadir that already holds most of the chain, nearly every
///   lookup hits, and skipping avoids re-writing the whole database
///   (`SkipExisting` is faster).
///
/// Measured on this dataset at 20% overlap, the checked path ran at ~59-75k
/// nodes/s with the lookup on the critical path for every one of ~700M nodes.
/// Whether writes go through the write-ahead log.
///
/// The WAL exists so a database can recover writes that had not yet been
/// flushed when a process died. A bulk import does not need that: if it dies, it
/// is re-run from the source, which is still sitting on disk. Paying for the WAL
/// means writing every one of ~700M trie nodes twice.
///
/// Keep it enabled only if the destination must stay crash-consistent *during*
/// the import -- which is not the case here, because a half-imported database is
/// discarded and re-imported rather than repaired.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WalMode {
    /// Skip the write-ahead log. Recommended for import.
    Disabled,
    /// Write through the WAL, as the node does in normal operation.
    Enabled,
}

impl WalMode {
    fn write_options(self) -> WriteOptions {
        let mut wo = WriteOptions::default();
        wo.disable_wal(self == WalMode::Disabled);
        wo
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteMode {
    /// Write every entry, no lookup. Correct because keys are content-addressed.
    Unconditional,
    /// Look up first and skip entries already held.
    SkipExisting,
}

#[derive(Debug, Default, Clone)]
pub struct ImportStats {
    pub scanned: u64,
    pub written: u64,
    pub skipped: u64,
    pub bytes: u64,
    pub elapsed_secs: f64,
    /// Highest block seen in the source, and its hash. Reported here because
    /// the caller cannot get it from `BlockStore::head` -- that still holds the
    /// previously synced head until the metadata rebuild moves it.
    pub tip_number: u64,
    pub tip_hash: B256,
}

impl ImportStats {
    pub fn rate(&self) -> f64 {
        if self.elapsed_secs > 0.0 { self.written as f64 / self.elapsed_secs } else { 0.0 }
    }
    pub fn mb_per_sec(&self) -> f64 {
        if self.elapsed_secs > 0.0 { self.bytes as f64 / 1e6 / self.elapsed_secs } else { 0.0 }
    }
}

/// Fraction of the keyspace traversed, from the key currently being read.
///
/// Both datasources are keyed by 32-byte Keccak hashes, which are uniformly
/// distributed, and RocksDB iterates in sorted key order. So the leading bytes
/// of the current key *are* the progress: a key beginning 0x80.. is halfway.
///
/// This is preferred over `rocksdb.estimate-num-keys`, which proved badly wrong
/// here -- it sums table properties over *open* SST files, and `open_source`
/// caps the table cache at 256 handles against a database of 2180 SSTs, so it
/// extrapolated from a small sample and under-reported the unitrie by ~8x.
/// Keyspace position needs no estimate and cannot drift.
fn keyspace_fraction(key: &[u8]) -> f64 {
    let mut buf = [0u8; 8];
    let n = key.len().min(8);
    buf[..n].copy_from_slice(&key[..n]);
    u64::from_be_bytes(buf) as f64 / u64::MAX as f64
}

/// Formats a duration as a human ETA.
fn fmt_eta(secs: f64) -> String {
    if !secs.is_finite() || secs <= 0.0 {
        return "--".to_string();
    }
    let s = secs as u64;
    if s >= 3600 {
        format!("{}h {}m", s / 3600, (s % 3600) / 60)
    } else if s >= 60 {
        format!("{}m {}s", s / 60, s % 60)
    } else {
        format!("{}s", s)
    }
}

/// Opens an rskj datasource read-only, so the source is never mutated and the
/// import can be re-run against the same extracted files.
fn open_source(path: &Path) -> Result<DB> {
    let mut opts = Options::default();
    opts.create_if_missing(false);
    // rskj's unitrie holds ~2200 SST files and blocks ~540. RocksDB defaults to
    // keeping every table open, which exhausts the process file-descriptor
    // limit before the database finishes opening ("Too many open files").
    // Bounding the table cache makes RocksDB close and reopen tables as needed,
    // which is correct regardless of what ulimit the process inherited -- this
    // is a sequential full scan, so the cache hit rate hardly matters.
    opts.set_max_open_files(256);
    DB::open_for_read_only(&opts, path, false)
        .with_context(|| format!("opening rskj datasource at {}", path.display()))
}


/// Points the node at the imported chain.
///
/// Sets both heads. `update_head` alone is not enough: the node resolves its
/// starting state from `exec_head`, the last block it has *executed*, and
/// without that it rebuilds from genesis and re-syncs the whole chain -- which
/// is exactly what a freshly imported database looked like before this existed.
///
/// The tip header's `state_root` is the Unitrie root for any block after
/// RSKIP126, which every usable snapshot is. Setting it also gives a free
/// end-to-end check of the trie import: on startup the node loads that root,
/// recomputes its hash and refuses it if they disagree, so a trie missing nodes
/// is caught immediately rather than part-way through execution.
fn set_heads(dest: &BlockStore, tip_hash: B256, td: U256) -> Result<()> {
    let tip = dest
        .header(tip_hash)?
        .context("tip header missing when setting heads")?;
    dest.update_head(&tip, td)?;
    dest.set_exec_head(tip_hash, tip.state_root)?;
    info!(
        target: "rustock::import",
        "heads set: block #{} {:?}, exec state root {:?}",
        tip.number, tip_hash, tip.state_root
    );
    Ok(())
}

/// Splits the 256-way first-byte keyspace into `n` contiguous ranges.
///
/// Trie keys are Keccak hashes, so they are uniformly distributed and equal
/// slices of the keyspace hold roughly equal numbers of nodes. Returns
/// `(start, end)` bounds where `end` is exclusive and `None` means open-ended.
fn keyspace_ranges(n: usize) -> Vec<(Vec<u8>, Option<Vec<u8>>)> {
    let n = n.max(1).min(256);
    (0..n)
        .map(|i| {
            let start = (i * 256 / n) as u8;
            let start_key = vec![start];
            let end_key = if i + 1 == n {
                None
            } else {
                Some(vec![((i + 1) * 256 / n) as u8])
            };
            (start_key, end_key)
        })
        .collect()
}

/// Copies `unitrie` into `trie_nodes` using several threads.
///
/// The sequential copy is CPU-bound, not disk-bound: it sustained 48.6 MB/s
/// against a volume measured at 358 MB/s sequential, so roughly 14% of the
/// available bandwidth. The work is decompressing source SST blocks and pushing
/// them through the write path, which is per-core.
///
/// Splitting is straightforward because the keys are hashes: each thread takes a
/// contiguous slice of the keyspace and its own iterator, and they share nothing
/// but the destination handle. `DB::write` is thread-safe, and since the entries
/// are content-addressed there is no ordering requirement between threads.
///
/// Reads and writes also land on different volumes here, so they do not contend.
pub fn import_unitrie_parallel(
    src_dir: &Path,
    dest_db: &Arc<DB>,
    mode: WriteMode,
    wal: WalMode,
    threads: usize,
    csv: Option<&mut dyn std::io::Write>,
) -> Result<ImportStats> {
    use std::sync::atomic::AtomicU64;

    let src = Arc::new(open_source(&src_dir.join("unitrie"))?);
    let ranges = keyspace_ranges(threads);
    info!(
        target: "rustock::import",
        "unitrie: {} threads over the keyspace, mode {:?}, WAL {:?}",
        ranges.len(), mode, wal
    );

    let scanned = AtomicU64::new(0);
    let written = AtomicU64::new(0);
    let skipped = AtomicU64::new(0);
    let bytes = AtomicU64::new(0);
    let start = Instant::now();

    std::thread::scope(|scope| -> Result<()> {
        let mut handles = Vec::new();
        for (idx, (lo, hi)) in ranges.iter().enumerate() {
            let src = Arc::clone(&src);
            let dest = Arc::clone(dest_db);
            let (lo, hi) = (lo.clone(), hi.clone());
            let (scanned, written, skipped, bytes) = (&scanned, &written, &skipped, &bytes);

            handles.push(scope.spawn(move || -> Result<()> {
                let cf = dest
                    .cf_handle("trie_nodes")
                    .context("destination is missing the trie_nodes column family")?;
                let wo = wal.write_options();

                let mut ro = rocksdb::ReadOptions::default();
                if let Some(ref upper) = hi {
                    ro.set_iterate_upper_bound(upper.clone());
                }
                let iter = src.iterator_opt(
                    IteratorMode::From(&lo, rocksdb::Direction::Forward),
                    ro,
                );

                let mut batch = WriteBatch::default();
                let mut in_batch = 0usize;
                let mut local = 0u64;

                for item in iter {
                    let (k, v) = item.context("iterating rskj unitrie")?;
                    local += 1;
                    bytes.fetch_add((k.len() + v.len()) as u64, Ordering::Relaxed);

                    let skip = mode == WriteMode::SkipExisting
                        && dest.get_cf(cf, &k)?.is_some();
                    if skip {
                        skipped.fetch_add(1, Ordering::Relaxed);
                    } else {
                        batch.put_cf(cf, &k, &v);
                        in_batch += 1;
                        written.fetch_add(1, Ordering::Relaxed);
                    }

                    if in_batch >= BATCH_SIZE {
                        dest.write_opt(std::mem::take(&mut batch), &wo)?;
                        in_batch = 0;
                    }
                    if local % 50_000 == 0 {
                        scanned.fetch_add(50_000, Ordering::Relaxed);
                        local = 0;
                        if shutdown_requested() {
                            break;
                        }
                    }
                }
                if in_batch > 0 {
                    dest.write_opt(batch, &wo)?;
                }
                scanned.fetch_add(local, Ordering::Relaxed);
                debug!(target: "rustock::import", "unitrie thread {idx} finished");
                Ok(())
            }));
        }

        // Report aggregate progress while the workers run.
        let mut last = Instant::now();
        loop {
            if handles.iter().all(|h| h.is_finished()) {
                break;
            }
            if last.elapsed().as_secs() >= 10 {
                let sc = scanned.load(Ordering::Relaxed);
                let by = bytes.load(Ordering::Relaxed);
                let secs = start.elapsed().as_secs_f64().max(0.001);
                info!(
                    target: "rustock::import",
                    "unitrie: {} scanned | {} written, {} present | {:.0} nodes/s, {:.1} MB/s",
                    sc, written.load(Ordering::Relaxed), skipped.load(Ordering::Relaxed),
                    sc as f64 / secs, by as f64 / 1e6 / secs
                );
                last = Instant::now();
            }
            std::thread::sleep(std::time::Duration::from_millis(250));
        }
        for h in handles {
            h.join().map_err(|_| anyhow::anyhow!("unitrie worker panicked"))??;
        }
        Ok(())
    })?;

    // Durable before reporting complete: writes bypass the WAL.
    dest_db.flush()?;

    let mut st = ImportStats {
        scanned: scanned.load(Ordering::Relaxed),
        written: written.load(Ordering::Relaxed),
        skipped: skipped.load(Ordering::Relaxed),
        bytes: bytes.load(Ordering::Relaxed),
        ..Default::default()
    };
    st.elapsed_secs = start.elapsed().as_secs_f64();
    if let Some(w) = csv {
        let _ = writeln!(
            w, "unitrie_parallel_{},{},{},{},{},{:.1}",
            ranges.len(), st.scanned, st.written, st.skipped, st.bytes, st.elapsed_secs
        );
    }
    // Say "stopped early" rather than "done" when interrupted: a phase that
    // reports completion while being partial is what made an earlier killed
    // import look finished when it was missing data.
    info!(
        target: "rustock::import",
        "unitrie {} ({} threads): {} nodes ({} new, {} present) in {:.0}s = {:.0} nodes/s",
        if shutdown_requested() { "STOPPED EARLY" } else { "done" },
        ranges.len(), st.scanned, st.written, st.skipped, st.elapsed_secs, st.rate()
    );
    Ok(st)
}

/// Copies `unitrie` into the `trie_nodes` column family.
///
/// Values are moved verbatim: a trie node's serialised form is identical in
/// both implementations, which is what makes the state roots agree.
pub fn import_unitrie(
    src_dir: &Path,
    dest_db: &Arc<DB>,
    mode: WriteMode,
    wal: WalMode,
    csv: Option<&mut dyn std::io::Write>,
) -> Result<ImportStats> {
    let wo = wal.write_options();
    let src = open_source(&src_dir.join("unitrie"))?;
    let cf = dest_db
        .cf_handle("trie_nodes")
        .context("destination is missing the trie_nodes column family")?;

    info!(target: "rustock::import", "unitrie: progress measured by keyspace position (keys are uniform hashes)");

    let mut st = ImportStats::default();
    let start = Instant::now();
    let mut batch = WriteBatch::default();
    let mut in_batch = 0usize;

    for item in src.iterator(IteratorMode::Start) {
        let (k, v) = item.context("iterating rskj unitrie")?;
        st.scanned += 1;
        st.bytes += (k.len() + v.len()) as u64;

        // Content-addressed: an entry already present is identical by
        // construction, so writing over it is safe. Only look it up first when
        // asked to -- that lookup is otherwise on the critical path for every
        // node in the database.
        let skip = mode == WriteMode::SkipExisting && dest_db.get_cf(cf, &k)?.is_some();
        if skip {
            st.skipped += 1;
        } else {
            batch.put_cf(cf, &k, &v);
            in_batch += 1;
            st.written += 1;
        }

        if in_batch >= BATCH_SIZE {
            dest_db.write_opt(std::mem::take(&mut batch), &wo)?;
            in_batch = 0;
        }
        if shutdown_requested() {
            info!(target: "rustock::import", "unitrie: signal received, flushing at {} nodes", st.scanned);
            break;
        }

        if st.scanned % REPORT_EVERY == 0 {
            st.elapsed_secs = start.elapsed().as_secs_f64();
            let rate = st.scanned as f64 / st.elapsed_secs;
            let frac = keyspace_fraction(&k);
            // Projected from position: at f of the keyspace having scanned n,
            // the database holds about n/f keys.
            let projected = if frac > 0.001 { (st.scanned as f64 / frac) as u64 } else { 0 };
            let eta = if frac > 0.001 && rate > 0.0 {
                fmt_eta((projected.saturating_sub(st.scanned)) as f64 / rate)
            } else {
                "--".to_string()
            };
            info!(
                target: "rustock::import",
                "unitrie: {:.1}% | {} scanned of ~{} | {} written, {} present | {:.0} nodes/s, {:.1} MB/s | ETA {}",
                frac * 100.0, st.scanned, projected, st.written, st.skipped,
                rate, st.mb_per_sec(), eta
            );
        }
    }
    if in_batch > 0 {
        dest_db.write_opt(batch, &wo)?;
    }
    // Force everything to SST files before reporting the phase complete.
    //
    // Writes bypass the WAL for speed, which means anything still in a memtable
    // exists only in RAM. Without this flush a phase that "finished" can lose a
    // hash-random subset of its entries if the process dies later -- observed
    // exactly that: the block phase reported 12.5M blocks written, then a kill
    // during the next phase left the chain walk unable to get past 59 blocks.
    dest_db.flush()?;
    st.elapsed_secs = start.elapsed().as_secs_f64();
    if let Some(w) = csv {
        let _ = writeln!(w, "unitrie,{},{},{},{},{:.1}", st.scanned, st.written, st.skipped, st.bytes, st.elapsed_secs);
    }
    info!(
        target: "rustock::import",
        "unitrie {}: {} nodes ({} new, {} present) in {:.0}s = {:.0} nodes/s",
        if shutdown_requested() { "STOPPED EARLY" } else { "done" },
        st.scanned, st.written, st.skipped, st.elapsed_secs, st.rate()
    );
    Ok(st)
}

/// Splits rskj's `blocks` datasource into rustock's `headers` and
/// `block_bodies` column families.
///
/// Blocks already held are skipped, so an import over a partially synced node
/// fast-forwards past what it already has instead of redoing it.
pub fn import_blocks(
    src_dir: &Path,
    dest: &BlockStore,
    mode: WriteMode,
    wal: WalMode,
    csv: Option<&mut dyn std::io::Write>,
) -> Result<ImportStats> {
    let src = open_source(&src_dir.join("blocks"))?;
    info!(target: "rustock::import", "blocks: progress measured by keyspace position");
    let wo = wal.write_options();
    let cf_h = dest.cf_headers()?;
    let cf_b = dest.cf_bodies()?;
    let mut batch = WriteBatch::default();
    let mut in_batch = 0usize;
    let mut st = ImportStats::default();
    let start = Instant::now();
    let mut max_number = 0u64;
    let mut tip_hash = B256::ZERO;

    for item in src.iterator(IteratorMode::Start) {
        let (k, v) = item.context("iterating rskj blocks")?;
        st.scanned += 1;
        st.bytes += (k.len() + v.len()) as u64;

        // rskj keys this datasource by block hash, but it also stores other
        // bookkeeping entries; anything that is not a 32-byte key or does not
        // decode as a block is skipped rather than aborting the import.
        if k.len() != 32 {
            continue;
        }
        let mut slice: &[u8] = &v;
        let block = match Block::decode(&mut slice) {
            Ok(b) => b,
            Err(e) => {
                warn!(target: "rustock::import", "skipping undecodable entry: {e}");
                continue;
            }
        };
        let hash = B256::from_slice(&k);

        if block.header.number > max_number {
            max_number = block.header.number;
            tip_hash = hash;
        }

        // Same reasoning as the trie: the key is the block hash, so a block we
        // already hold is identical.
        let skip = mode == WriteMode::SkipExisting && dest.has_block(hash)?;
        if skip {
            st.skipped += 1;
        } else {
            // Batched and WAL-free, same as the trie phase. Encoding comes from
            // the store so there is one definition of the on-disk format.
            batch.put_cf(cf_h, hash.as_slice(), encode_header(&block.header));
            batch.put_cf(cf_b, hash.as_slice(), encode_body(&block.transactions, &block.ommers));
            in_batch += 1;
            st.written += 1;
            if in_batch >= BATCH_SIZE {
                dest.db().write_opt(std::mem::take(&mut batch), &wo)?;
                in_batch = 0;
            }
        }

        if shutdown_requested() {
            info!(target: "rustock::import", "unitrie: signal received, flushing at {} nodes", st.scanned);
            break;
        }

        if shutdown_requested() {
            info!(target: "rustock::import", "blocks: signal received, flushing at {} entries", st.scanned);
            break;
        }

        if st.scanned % REPORT_EVERY == 0 {
            st.elapsed_secs = start.elapsed().as_secs_f64();
            let rate = st.scanned as f64 / st.elapsed_secs;
            let frac = keyspace_fraction(&k);
            let projected = if frac > 0.001 { (st.scanned as f64 / frac) as u64 } else { 0 };
            let eta = if frac > 0.001 && rate > 0.0 {
                fmt_eta((projected.saturating_sub(st.scanned)) as f64 / rate)
            } else {
                "--".to_string()
            };
            info!(
                target: "rustock::import",
                "blocks: {:.1}% | {} scanned of ~{} | {} written, {} present | tip #{} | {:.0} blk/s, {:.1} MB/s | ETA {}",
                frac * 100.0, st.scanned, projected, st.written, st.skipped,
                max_number, rate, st.mb_per_sec(), eta
            );
        }
    }
    if in_batch > 0 {
        dest.db().write_opt(batch, &wo)?;
    }
    // See the note in import_unitrie: make the phase durable before reporting
    // it done, or a later kill silently discards part of it.
    dest.db().flush()?;
    st.tip_number = max_number;
    st.tip_hash = tip_hash;
    st.elapsed_secs = start.elapsed().as_secs_f64();
    if let Some(w) = csv {
        let _ = writeln!(w, "blocks,{},{},{},{},{:.1}", st.scanned, st.written, st.skipped, st.bytes, st.elapsed_secs);
    }
    info!(
        target: "rustock::import",
        "blocks {}: {} scanned ({} new, {} present), tip #{} {:?}, {:.0}s",
        if shutdown_requested() { "STOPPED EARLY" } else { "done" },
        st.scanned, st.written, st.skipped, max_number, tip_hash, st.elapsed_secs
    );
    Ok(st)
}

/// Rebuilds chain metadata using an in-memory index of the headers.
///
/// The naive walk is slow for a structural reason: following `parentHash` from
/// the tip is a chain of dependent random lookups, where each address is only
/// known once the previous read returns. Nothing can be batched or prefetched,
/// and it measured ~1,065 blocks/s -- about five hours for RSK mainnet, with
/// 88% of that in the walk rather than the writes.
///
/// The dependency is unavoidable, but the *disk* access is not. Every header is
/// already stored; only the order of traversal is unknown. So:
///
///   1. scan the headers column family sequentially, which has no dependent
///      lookups, building `hash -> (number, parent, difficulty)` in memory;
///   2. walk the chain through that map, which is pointer-chasing in RAM;
///   3. write the canonical mapping and accumulated difficulty in batches.
///
/// Note the block header carries `difficulty`, its own, but no cumulative
/// total -- total difficulty exists only as a running sum, which is why step 3
/// has to accumulate rather than copy. That sum is cheap: the forward pass
/// already ran at ~7,661 blocks/s, so it was never the problem.
///
/// Memory: roughly 150 bytes per header, so about 2 GB for RSK mainnet's 12.5M
/// headers. The function reports its own footprint as it builds.
pub fn rebuild_chain_metadata_in_memory(
    dest: &BlockStore,
    wal: WalMode,
    csv: Option<&mut dyn std::io::Write>,
) -> Result<u64> {
    use std::collections::HashMap;

    let overall = Instant::now();
    let cf_headers = dest.cf_headers()?;

    // --- 1. Sequential scan into memory ---------------------------------
    let scan_start = Instant::now();
    let mut index: HashMap<B256, (u64, B256, U256)> = HashMap::with_capacity(13_000_000);
    let mut scanned = 0u64;
    let mut tip = (0u64, B256::ZERO);
    let mut last_report = Instant::now();

    for item in dest.db().iterator_cf(cf_headers, IteratorMode::Start) {
        let (k, v) = item.context("scanning headers")?;
        if k.len() != 32 {
            continue;
        }
        let mut slice: &[u8] = &v;
        let h = match rustock_core::Header::decode(&mut slice) {
            Ok(h) => h,
            Err(_) => continue,
        };
        let hash = B256::from_slice(&k);
        if h.number >= tip.0 {
            tip = (h.number, hash);
        }
        index.insert(hash, (h.number, h.parent_hash, h.difficulty));
        scanned += 1;

        if shutdown_requested() {
            anyhow::bail!("interrupted while indexing headers");
        }
        if last_report.elapsed().as_secs() >= 10 {
            let rate = scanned as f64 / scan_start.elapsed().as_secs_f64().max(0.001);
            info!(
                target: "rustock::import",
                "header index: {} headers | {:.0}/s | ~{} MB | tip #{}",
                scanned, rate, scanned * 150 / 1_000_000, tip.0
            );
            last_report = Instant::now();
        }
    }
    let scan_secs = scan_start.elapsed().as_secs_f64();
    info!(
        target: "rustock::import",
        "header index built: {} headers in {:.1}s ({:.0}/s), tip #{}",
        scanned, scan_secs, scanned as f64 / scan_secs.max(0.001), tip.0
    );

    // --- 2. Walk the chain in RAM ---------------------------------------
    let walk_start = Instant::now();
    let tip_number = tip.0;
    let mut lineage: Vec<(u64, B256, U256)> = Vec::with_capacity(tip_number as usize + 1);
    let mut cursor = tip.1;
    loop {
        let (number, parent, difficulty) = match index.get(&cursor) {
            Some(v) => *v,
            None => {
                warn!(target: "rustock::import", "chain walk stopped: missing header {cursor:?}");
                break;
            }
        };
        lineage.push((number, cursor, difficulty));
        if number == 0 {
            break;
        }
        cursor = parent;
    }
    lineage.reverse();
    let walk_secs = walk_start.elapsed().as_secs_f64();
    info!(
        target: "rustock::import",
        "chain walked in memory: {} blocks in {:.2}s ({:.0} blk/s)",
        lineage.len(), walk_secs, lineage.len() as f64 / walk_secs.max(0.001)
    );
    drop(index);

    // --- 3. Batched forward pass ----------------------------------------
    let fwd_start = Instant::now();
    let wo = wal.write_options();
    let cf_numbers = dest.cf_numbers()?;
    let cf_td = dest.cf_td()?;
    let mut batch = WriteBatch::default();
    let mut in_batch = 0usize;
    let mut td = U256::ZERO;
    let mut done = 0u64;
    let mut last_report = Instant::now();

    for (number, hash, difficulty) in &lineage {
        td += *difficulty;
        batch.put_cf(cf_numbers, number.to_be_bytes(), hash.as_slice());
        batch.put_cf(cf_td, hash.as_slice(), crate::encode_td(td));
        in_batch += 1;
        done += 1;
        if in_batch >= BATCH_SIZE {
            dest.db().write_opt(std::mem::take(&mut batch), &wo)?;
            in_batch = 0;
        }
        if shutdown_requested() {
            dest.db().write_opt(std::mem::take(&mut batch), &wo)?;
            dest.db().flush()?;
            anyhow::bail!("interrupted during forward pass at #{number}");
        }
        if last_report.elapsed().as_secs() >= 10 {
            let rate = done as f64 / fwd_start.elapsed().as_secs_f64().max(0.001);
            info!(
                target: "rustock::import",
                "metadata forward: {:.1}% | #{} | {:.0} blk/s | ETA {}",
                done as f64 / lineage.len() as f64 * 100.0, number, rate,
                fmt_eta((lineage.len() as u64 - done) as f64 / rate.max(1.0))
            );
            last_report = Instant::now();
        }
    }
    if in_batch > 0 {
        dest.db().write_opt(batch, &wo)?;
    }
    dest.db().flush()?;
    let fwd_secs = fwd_start.elapsed().as_secs_f64();

    if let Some((_, tip_hash, _)) = lineage.last() {
        set_heads(dest, *tip_hash, td)?;
    }

    let total = overall.elapsed().as_secs_f64();
    info!(
        target: "rustock::import",
        "metadata done (in-memory): {} blocks in {:.1}s | scan {:.1}s, walk {:.2}s, forward {:.1}s ({:.0} blk/s) | head #{} td={}",
        lineage.len(), total, scan_secs, walk_secs, fwd_secs,
        lineage.len() as f64 / fwd_secs.max(0.001), tip_number, td
    );
    if let Some(w) = csv {
        let _ = writeln!(w, "metadata_in_memory,{},{},0,0,{:.1}", lineage.len(), lineage.len(), total);
    }
    Ok(tip_number)
}

/// Loads chain metadata from a dump of rskj's MapDB block index.
///
/// This is the fast path, and the reason it exists is that the alternative is
/// slow for a structural reason rather than an implementation one. Deriving the
/// canonical chain means walking `parentHash` back from the tip: 9.2M lookups
/// where each address depends on the previous result, so nothing can be batched
/// or prefetched. Measured at ~1,065 blocks/s, which is about five hours for
/// RSK mainnet -- and 88% of that is the walk, not the writes.
///
/// rskj already has the answer. Its index stores, per height, the canonical
/// block's hash and its cumulative difficulty. The obstacle was only that it
/// lives in a MapDB file holding Java-serialised objects, which a separate Java
/// tool (tools/dump-index) converts to a flat file:
///
/// ```text
/// <number> <hash-hex> <cumulative-difficulty-decimal>
/// ```
///
/// Reading that is a sequential scan with no dependent lookups at all.
///
/// Feed it sorted by block number: writes then land in key order, which lets
/// RocksDB compact by trivial move instead of rewriting.
pub fn load_metadata_from_dump(
    dest: &BlockStore,
    dump_path: &Path,
    wal: WalMode,
    csv: Option<&mut dyn std::io::Write>,
) -> Result<u64> {
    use std::io::{BufRead, BufReader};

    let f = std::fs::File::open(dump_path)
        .with_context(|| format!("opening index dump {}", dump_path.display()))?;
    let reader = BufReader::with_capacity(1 << 20, f);
    let wo = wal.write_options();
    let cf_numbers = dest.cf_numbers()?;
    let cf_td = dest.cf_td()?;

    let start = Instant::now();
    let mut batch = WriteBatch::default();
    let mut in_batch = 0usize;
    let mut count = 0u64;
    let mut malformed = 0u64;
    let mut tip_number = 0u64;
    let mut tip_hash = B256::ZERO;
    let mut tip_td = U256::ZERO;
    let mut last_report = Instant::now();

    for line in reader.lines() {
        let line = line?;
        let mut it = line.split_ascii_whitespace();
        let (num, hash_hex, td_dec) = match (it.next(), it.next(), it.next()) {
            (Some(a), Some(b), Some(c)) => (a, b, c),
            _ => { malformed += 1; continue; }
        };
        let number: u64 = match num.parse() { Ok(n) => n, Err(_) => { malformed += 1; continue; } };
        let hash_bytes = match hex::decode(hash_hex) {
            Ok(h) if h.len() == 32 => h,
            _ => { malformed += 1; continue; }
        };
        let td = match U256::from_str_radix(td_dec, 10) {
            Ok(t) => t,
            Err(_) => { malformed += 1; continue; }
        };
        let hash = B256::from_slice(&hash_bytes);

        batch.put_cf(cf_numbers, number.to_be_bytes(), hash.as_slice());
        batch.put_cf(cf_td, hash.as_slice(), crate::encode_td(td));
        in_batch += 1;
        count += 1;

        if number >= tip_number {
            tip_number = number;
            tip_hash = hash;
            tip_td = td;
        }

        if in_batch >= BATCH_SIZE {
            dest.db().write_opt(std::mem::take(&mut batch), &wo)?;
            in_batch = 0;
        }
        if shutdown_requested() {
            info!(target: "rustock::import", "index dump: signal received at {count} records");
            break;
        }
        if last_report.elapsed().as_secs() >= 10 {
            let rate = count as f64 / start.elapsed().as_secs_f64().max(0.001);
            info!(
                target: "rustock::import",
                "index dump: {} records | {:.0} rec/s | at #{}",
                count, rate, number
            );
            last_report = Instant::now();
        }
    }
    if in_batch > 0 {
        dest.db().write_opt(batch, &wo)?;
    }
    dest.db().flush()?;

    // Set both heads from the highest record seen.
    if tip_hash != B256::ZERO {
        set_heads(dest, tip_hash, tip_td)?;
    }

    let secs = start.elapsed().as_secs_f64();
    if malformed > 0 {
        warn!(target: "rustock::import", "index dump: {malformed} malformed lines skipped");
    }
    info!(
        target: "rustock::import",
        "index dump done: {} records in {:.1}s ({:.0} rec/s), head #{} td={}",
        count, secs, count as f64 / secs.max(0.001), tip_number, tip_td
    );
    if let Some(w) = csv {
        let _ = writeln!(w, "index_dump,{},{},0,0,{:.1}", count, count, secs);
    }
    Ok(tip_number)
}

/// Rebuilds chain metadata for a block range only, for benchmarking.
///
/// The full pass takes hours, which is far too slow a loop for trying
/// optimisations. A range gives the same per-block work at a chosen cost.
///
/// It can skip the two things that force the full pass to start at the extremes:
///
/// - the backward walk normally starts at the tip because canonical-ness is
///   defined as the tip's ancestry; here it starts at `canonical_hash(to)`,
///   which an earlier full pass already established;
/// - total difficulty normally accumulates from genesis; here it is seeded from
///   `total_difficulty` at `from - 1`.
///
/// Both are reads of existing metadata, so this is only meaningful once a full
/// pass has run. It is non-destructive: the values recomputed for the range are
/// identical to the ones already stored, since the lineage comes from immutable
/// parentHash links and the difficulty sum is deterministic.
pub fn rebuild_chain_metadata_range(
    dest: &BlockStore,
    from: u64,
    to: u64,
    csv: Option<&mut dyn std::io::Write>,
) -> Result<()> {
    if from > to {
        anyhow::bail!("--metadata-from ({from}) is above --metadata-to ({to})");
    }
    let start = Instant::now();

    let end_hash = dest
        .canonical_hash(to)?
        .with_context(|| format!("no canonical hash at #{to}; run a full metadata pass first"))?;

    // Seed the difficulty from the block before the range.
    let mut td = if from == 0 {
        U256::ZERO
    } else {
        let prev = dest
            .canonical_hash(from - 1)?
            .with_context(|| format!("no canonical hash at #{}", from - 1))?;
        dest.total_difficulty(prev)?
            .with_context(|| format!("no total difficulty at #{}", from - 1))?
    };

    info!(
        target: "rustock::import",
        "metadata range #{from}..#{to} ({} blocks), seeded td={td}",
        to - from + 1
    );

    // Backward walk over the range only.
    let walk_start = Instant::now();
    let mut lineage: Vec<(u64, B256)> = Vec::with_capacity((to - from + 1) as usize);
    let mut cursor = end_hash;
    loop {
        let h = dest.header(cursor)?.context("missing header during range walk")?;
        if h.number < from {
            break;
        }
        lineage.push((h.number, cursor));
        if h.number == 0 || shutdown_requested() {
            break;
        }
        cursor = h.parent_hash;
    }
    lineage.reverse();
    let walk_secs = walk_start.elapsed().as_secs_f64();

    // Forward pass.
    let fwd_start = Instant::now();
    for (number, hash) in &lineage {
        let h = dest.header(*hash)?.context("header vanished mid-walk")?;
        td += h.difficulty;
        dest.put_canonical_hash(*number, *hash)?;
        dest.put_total_difficulty(*hash, td)?;
        if shutdown_requested() {
            break;
        }
    }
    let fwd_secs = fwd_start.elapsed().as_secs_f64();
    let total_secs = start.elapsed().as_secs_f64();
    let n = lineage.len() as f64;

    info!(
        target: "rustock::import",
        "metadata range done: {} blocks in {:.1}s ({:.0} blk/s) | walk {:.1}s ({:.0} blk/s), forward {:.1}s ({:.0} blk/s) | final td={}",
        lineage.len(), total_secs, n / total_secs.max(0.001),
        walk_secs, n / walk_secs.max(0.001),
        fwd_secs, n / fwd_secs.max(0.001),
        td
    );
    if let Some(w) = csv {
        let _ = writeln!(
            w, "metadata_range_{}_{},{},{},0,0,{:.1}",
            from, to, lineage.len(), lineage.len(), total_secs
        );
    }
    Ok(())
}

/// Rebuilds `block_numbers` and `total_difficulty` from the imported headers.
///
/// rskj keeps both in its MapDB index, which is not readable from here, so they
/// are derived: walk `parentHash` back from the tip to find the canonical
/// chain, then accumulate difficulty forward from genesis.
pub fn rebuild_chain_metadata(dest: &BlockStore, tip: B256, csv: Option<&mut dyn std::io::Write>) -> Result<u64> {
    let start = Instant::now();
    let tip_header = dest
        .header(tip)?
        .context("tip header missing after block import")?;
    let tip_number = tip_header.number;
    info!(target: "rustock::import", "walking canonical chain back from #{tip_number}");

    // Walk back to genesis collecting the canonical lineage.
    let mut lineage: Vec<(u64, B256)> = Vec::with_capacity(tip_number as usize + 1);
    let mut cursor = tip;
    let mut last_report = Instant::now();
    loop {
        let h = match dest.header(cursor)? {
            Some(h) => h,
            None => {
                warn!(target: "rustock::import", "chain walk stopped: missing header {cursor:?}");
                break;
            }
        };
        // Progress is exact here, unlike the hash-keyed phases: the walk steps
        // down one block number at a time from a known tip.
        if last_report.elapsed().as_secs() >= 10 {
            let done = tip_number.saturating_sub(h.number);
            let pct = done as f64 / tip_number.max(1) as f64 * 100.0;
            let rate = done as f64 / start.elapsed().as_secs_f64().max(0.001);
            info!(
                target: "rustock::import",
                "chain walk back: {:.1}% | at #{} of #{} | {:.0} blk/s | ETA {}",
                pct, h.number, tip_number, rate,
                fmt_eta(h.number as f64 / rate.max(1.0))
            );
            last_report = Instant::now();
        }
        if shutdown_requested() {
            info!(target: "rustock::import", "chain walk: signal received at #{}", h.number);
            return Err(anyhow::anyhow!("interrupted during chain walk; rerun with --import-metadata-only"));
        }
        lineage.push((h.number, cursor));
        if h.number == 0 {
            break;
        }
        cursor = h.parent_hash;
    }
    lineage.reverse();
    info!(target: "rustock::import", "lineage: {} blocks, genesis .. #{tip_number}", lineage.len());

    // Forward pass: canonical mapping plus cumulative difficulty.
    let mut td = U256::ZERO;
    let fwd_start = Instant::now();
    let total = lineage.len() as u64;
    let mut done = 0u64;
    let mut last_report = Instant::now();
    for (number, hash) in &lineage {
        let h = dest.header(*hash)?.context("header vanished mid-walk")?;
        td += h.difficulty;
        dest.put_canonical_hash(*number, *hash)?;
        dest.put_total_difficulty(*hash, td)?;
        done += 1;
        if shutdown_requested() {
            dest.db().flush()?;
            return Err(anyhow::anyhow!(
                "interrupted during metadata rebuild at #{number}; rerun with --import-metadata-only"
            ));
        }
        if last_report.elapsed().as_secs() >= 10 {
            let rate = done as f64 / fwd_start.elapsed().as_secs_f64().max(0.001);
            info!(
                target: "rustock::import",
                "metadata forward: {:.1}% | #{} of #{} | {:.0} blk/s | ETA {}",
                done as f64 / total.max(1) as f64 * 100.0,
                number, tip_number, rate,
                fmt_eta((total - done) as f64 / rate.max(1.0))
            );
            last_report = Instant::now();
        }
    }
    set_heads(dest, tip, td)?;
    let elapsed = start.elapsed().as_secs_f64();
    if let Some(w) = csv {
        let _ = writeln!(w, "metadata,{},{},0,0,{:.1}", lineage.len(), lineage.len(), elapsed);
    }
    info!(
        target: "rustock::import",
        "metadata done: {} blocks, head #{} td={}, {:.0}s",
        lineage.len(), tip_number, td, elapsed
    );
    Ok(tip_number)
}
