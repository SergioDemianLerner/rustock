//! Old headers, stored in flat files instead of the LSM tree.
//!
//! # The problem this solves
//!
//! Headers live in `CF_HEADERS`, keyed by block hash. Hashes are uniformly
//! distributed, so consecutive blocks share neither a data block nor usually a
//! file. Serving the 192 headers a peer asks for is therefore 192 unrelated
//! point lookups, and each one pulls a whole RocksDB data block off the disk to
//! return ~1.1 KB of header. Measured on this database: **6.5x read
//! amplification**, ~7.1 KB read per header delivered.
//!
//! The amplification is not a tuning problem. It follows from the key: a hash
//! index cannot answer "the headers from N downwards" as a range, because the
//! keys are not ordered by anything the question mentions.
//!
//! # The shape
//!
//! Canonical headers below a threshold are append-only and never change, so
//! they can be stored the way the question asks for them: **by block number, in
//! order, contiguously**. A run of headers becomes one index read and one
//! sequential data read.
//!
//! Two file series, following geth's freezer:
//!
//! - `headers-NNNN.cdat` — the RLP bodies, concatenated, capped at
//!   [`MAX_DATA_FILE_BYTES`] so no single file grows without bound.
//! - `headers.cidx` — a fixed-width index, [`INDEX_ENTRY_BYTES`] per block:
//!   a 2-byte file number and the 4-byte `start` and `end` offsets of that
//!   block's data.
//!
//! Entry `N` is at byte `N * 10`, so finding block `N` is arithmetic rather
//! than a search, and the run `N..N+k` is one read of `k * 10` bytes.
//!
//! # Why each entry carries its own start
//!
//! geth stores only the end offset and takes block `N`'s start from block
//! `N-1`'s end. That is two bytes cheaper and it forces the data to be written
//! in ascending order, which this node cannot promise: a snapshot sync walks
//! the header chain **downwards**, from the tip to genesis, verifying
//! `parent_hash` as it goes. Buffering that walk to re-emit it ascending would
//! mean holding every header from the tip to block 0 -- 8.8 million of them,
//! about 9.7 GB -- before the first append became legal.
//!
//! Carrying both offsets makes the format indifferent to arrival order. A
//! descending walk writes its run backwards through the file and a run is
//! still one contiguous read, because what matters is that a single walk's
//! blocks are adjacent, not that they ascend.
//!
//! An entry of all zeroes means "not here". Block 0 cannot collide with that
//! because its `end` is past its `start`, and the index file is sparse: a hole
//! reads as zeroes, so a number never written simply reports absent.
//!
//! # What is not here
//!
//! **Nothing is deleted from RocksDB.** geth deletes after fsync, and can,
//! because it keeps a hash-to-number index to redirect lookups by hash. This
//! database has no such index -- `CF_HEIGHT_INDEX` maps number to hashes, the
//! wrong direction -- so deleting `CF_HEADERS` would break every lookup by
//! hash. The duplication costs about 10 GB against a measured read win; closing
//! that gap needs the reverse index first, and is deliberately separate work.
//!
//! **Only canonical headers.** A header that is not canonical at its height has
//! no place in a file addressed by height. Those stay in RocksDB, which is also
//! where a reorg goes looking for them.

use anyhow::{bail, Context, Result};
use rustock_core::Header;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// Bytes per index entry: a 2-byte file number, then 4-byte `start` and `end`.
///
/// The offsets bound a data file at 4 GiB, which [`MAX_DATA_FILE_BYTES`] stays
/// well under, and the file number allows 65,536 files -- far more than a
/// chain of this size will ever need.
pub const INDEX_ENTRY_BYTES: u64 = 10;

/// Bytes to accumulate before touching the data file.
///
/// A header is about 1.1 KB, so writing each one through on its own would mean
/// a syscall and an `fsync` per block: 8.8 million of each for a chain of this
/// size, which is slower than the random reads the freezer exists to avoid.
/// Buffering to 8 MB makes it roughly 1,200 flushes for the whole chain, each
/// one a sequential write of about 7,300 headers.
///
/// The buffer is also what makes the crash rule cheap to honour: data is
/// flushed and synced first, and only then are that batch's index entries
/// written, so a crash can orphan data but can never publish an index entry
/// pointing at bytes that are not there.
pub const WRITE_BUFFER_BYTES: usize = 8 * 1024 * 1024;

/// Roll to a new data file past this size.
///
/// geth uses 2 GB. The only thing the size decides is how many files a
/// directory listing has to show and how much a single corrupt file costs;
/// reads are addressed through the index either way.
pub const MAX_DATA_FILE_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// How far behind the head a block must be before it is frozen.
///
/// One week at Rootstock's measured 30.3-second block time (19,964 blocks over
/// the 100,000 blocks to 2026-09-28), rounded.
///
/// There is no finality gadget here, so this cannot key off finality the way
/// geth and reth do. It is instead anchored against what this node already
/// treats as settled:
///
/// | constant | value | meaning |
/// |---|---|---|
/// | `MAX_REORG_DEPTH` | 1,000 | deepest reorg the canonical index will follow |
/// | `MIN_KEEP_DEPTH` | 8,000 | the floor block pruning refuses to cross |
///
/// 20,000 is 20x the first and 2.5x the second, so a frozen block is one the
/// rest of the node has already stopped expecting to change.
pub const FREEZE_DEPTH: u64 = 20_000;

/// Where one block's data is: which file, and the byte range within it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct IndexEntry {
    file: u16,
    start: u32,
    end: u32,
}

impl IndexEntry {
    fn decode(bytes: &[u8]) -> Self {
        Self {
            file: u16::from_be_bytes([bytes[0], bytes[1]]),
            start: u32::from_be_bytes([bytes[2], bytes[3], bytes[4], bytes[5]]),
            end: u32::from_be_bytes([bytes[6], bytes[7], bytes[8], bytes[9]]),
        }
    }

    fn encode(&self) -> [u8; INDEX_ENTRY_BYTES as usize] {
        let f = self.file.to_be_bytes();
        let s = self.start.to_be_bytes();
        let e = self.end.to_be_bytes();
        [f[0], f[1], s[0], s[1], s[2], s[3], e[0], e[1], e[2], e[3]]
    }

    /// A hole in the sparse index, or a number never written.
    fn is_absent(&self) -> bool {
        self.end == 0 && self.start == 0
    }
}

/// The append-only side, behind one lock.
struct Writable {
    index: File,
    data: File,
    /// Which data file `data` is.
    file_number: u16,
    /// How many bytes are in it.
    data_len: u32,
    /// One past the highest block number written, so the index length is
    /// known without stat'ing the file.
    end_number: u64,
    /// Header bytes not yet handed to the data file.
    pending_data: Vec<u8>,
    /// Index entries for the blocks sitting in `pending_data`, which must not
    /// be published until that data is durable.
    pending_index: Vec<(u64, IndexEntry)>,
}

/// Flat-file storage for canonical headers below the freeze threshold.
pub struct Freezer {
    dir: PathBuf,
    write: Mutex<Writable>,
}

impl Freezer {
    /// Opens, or creates, a freezer in `dir`.
    ///
    /// A freezer that was interrupted mid-append is repaired here: the index is
    /// the authority, and any data past what the last index entry accounts for
    /// is a partial write from a process that died, so it is truncated away.
    /// Doing it the other way round -- trusting the data -- would leave a
    /// header the index cannot address, which is worse than losing an append
    /// that can simply be redone.
    pub fn open(dir: impl AsRef<Path>) -> Result<Self> {
        let dir = dir.as_ref().to_path_buf();
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("creating freezer directory {}", dir.display()))?;

        let index_path = dir.join("headers.cidx");
        let mut index = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&index_path)
            .with_context(|| format!("opening {}", index_path.display()))?;

        let mut index_len = index.metadata()?.len();

        // A torn index write leaves a partial entry; drop it.
        let ragged = index_len % INDEX_ENTRY_BYTES;
        if ragged != 0 {
            index_len -= ragged;
            index.set_len(index_len)?;
            index.sync_all()?;
        }
        let end_number = index_len / INDEX_ENTRY_BYTES;

        // Find the newest data file on disk; the index does not record which
        // one is being appended to, only where each block landed.
        let mut file_number = 0u16;
        for n in 0..=u16::MAX {
            if dir.join(format!("headers-{n:04}.cdat")).exists() {
                file_number = n;
            } else if n > 0 {
                break;
            }
        }

        let data_path = dir.join(format!("headers-{file_number:04}.cdat"));
        let data = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&data_path)
            .with_context(|| format!("opening {}", data_path.display()))?;

        // The index is the authority. Anything in the data file past the
        // furthest point any entry accounts for is a partial write from a
        // process that died, and is discarded: an unreferenced tail costs
        // nothing, while an index entry pointing past the data is unreadable.
        let covered = furthest_end(&mut index, end_number, file_number)?;
        if data.metadata()?.len() > covered as u64 {
            data.set_len(covered as u64)?;
            data.sync_all()?;
        }

        Ok(Self {
            dir,
            write: Mutex::new(Writable {
                index,
                data,
                file_number,
                data_len: covered,
                end_number,
                pending_data: Vec::with_capacity(WRITE_BUFFER_BYTES),
                pending_index: Vec::new(),
            }),
        })
    }

    /// One past the highest block number ever written.
    ///
    /// Not a count: the index is sparse, so a number below this may still be
    /// absent. Use [`Freezer::contains`] to ask about one block.
    pub fn end_number(&self) -> u64 {
        self.write.lock().unwrap().end_number
    }

    /// Whether this block's header is in the freezer.
    pub fn contains(&self, number: u64) -> bool {
        self.entry(number).map(|e| !e.is_absent()).unwrap_or(false)
    }

    fn entry(&self, number: u64) -> Option<IndexEntry> {
        let mut index = File::open(self.dir.join("headers.cidx")).ok()?;
        read_entry_at(&mut index, number).ok().filter(|e| !e.is_absent())
    }

    /// Stores the header for `number`, wherever the write head happens to be.
    ///
    /// Order does not matter to the format -- each entry carries its own byte
    /// range -- but it matters to performance: blocks written consecutively
    /// land consecutively, and only then is a run one read. Callers should
    /// feed a single walk without interleaving another.
    ///
    /// The write is buffered. Nothing reaches the disk until
    /// [`WRITE_BUFFER_BYTES`] have accumulated or [`Freezer::sync`] is called,
    /// and nothing is *readable* until then either -- a buffered block reports
    /// absent, because saying otherwise would promise a read that would fail.
    ///
    /// Writing a number twice overwrites its index entry and orphans the old
    /// data, which is wasteful but not wrong.
    pub fn put(&self, number: u64, header: &Header) -> Result<()> {
        let encoded = crate::encode_header(header);
        let mut w = self.write.lock().unwrap();

        // Roll before writing, never across: a header must be contiguous
        // within one file, or one read cannot answer for it. The pending
        // buffer belongs to the file it was written against, so it goes out
        // before the roll.
        let after = w.data_len as u64 + w.pending_data.len() as u64 + encoded.len() as u64;
        if after > MAX_DATA_FILE_BYTES {
            self.flush_locked(&mut w)?;
            let next = w
                .file_number
                .checked_add(1)
                .context("freezer is out of data files")?;
            let path = self.dir.join(format!("headers-{next:04}.cdat"));
            w.data = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(true)
                .open(&path)
                .with_context(|| format!("opening {}", path.display()))?;
            w.file_number = next;
            w.data_len = 0;
        }

        let start = w.data_len + w.pending_data.len() as u32;
        let end = start + encoded.len() as u32;
        w.pending_data.extend_from_slice(&encoded);
        let entry = IndexEntry { file: w.file_number, start, end };
        w.pending_index.push((number, entry));
        w.end_number = w.end_number.max(number + 1);

        if w.pending_data.len() >= WRITE_BUFFER_BYTES {
            self.flush_locked(&mut w)?;
        }
        Ok(())
    }

    /// Writes the buffered batch: data first and synced, then its index
    /// entries.
    ///
    /// The ordering is the whole crash story. Data with no index entry is
    /// unreferenced and `open` discards it; an index entry with no data behind
    /// it would be an unreadable header that nothing detects until a peer asks
    /// for it.
    fn flush_locked(&self, w: &mut Writable) -> Result<()> {
        if w.pending_data.is_empty() {
            return Ok(());
        }
        let at = w.data_len as u64;
        w.data.seek(SeekFrom::Start(at))?;
        let buf = std::mem::take(&mut w.pending_data);
        w.data.write_all(&buf)?;
        w.data.sync_all()?;
        w.data_len += buf.len() as u32;
        w.pending_data = buf;
        w.pending_data.clear();

        let entries = std::mem::take(&mut w.pending_index);
        for (number, entry) in &entries {
            w.index.seek(SeekFrom::Start(number * INDEX_ENTRY_BYTES))?;
            w.index.write_all(&entry.encode())?;
        }
        Ok(())
    }

    /// Writes out whatever is buffered and makes all of it durable.
    ///
    /// Call this at a point where losing the batch would be a nuisance -- the
    /// end of a sync phase, or shutdown. Losing it is never a correctness
    /// problem: unflushed blocks were never published, so the freezer simply
    /// does not have them, and whatever fills it will write them again.
    pub fn sync(&self) -> Result<()> {
        let mut w = self.write.lock().unwrap();
        self.flush_locked(&mut w)?;
        w.index.sync_all()?;
        Ok(())
    }

    /// The header for one block, if it is frozen.
    pub fn header(&self, number: u64) -> Result<Option<Header>> {
        Ok(self.run(number, 1)?.pop().flatten())
    }

    /// Headers for `count` blocks starting at `from`, ascending, positionally.
    ///
    /// This is the method the whole file format exists for. One index read
    /// covers the run; then, for each data file the run touches -- normally
    /// exactly one -- a single read spans from the lowest offset any of those
    /// blocks occupies to the highest, and each header is sliced out of it.
    ///
    /// That holds whichever direction the run was written in. A descending
    /// walk puts block `N+1` before block `N` in the file, but they are still
    /// adjacent, so the span is still one read.
    pub fn run(&self, from: u64, count: usize) -> Result<Vec<Option<Header>>> {
        let mut out: Vec<Option<Header>> = vec![None; count];
        if count == 0 {
            return Ok(out);
        }

        let end_number = self.end_number();
        if from >= end_number {
            return Ok(out);
        }
        let available = ((end_number - from) as usize).min(count);

        let mut index = File::open(self.dir.join("headers.cidx"))?;
        let mut raw = vec![0u8; available * INDEX_ENTRY_BYTES as usize];
        index.seek(SeekFrom::Start(from * INDEX_ENTRY_BYTES))?;
        // A sparse tail may be shorter than asked for; a short read leaves
        // zeroes, which decode as absent, which is the truth.
        let got = read_up_to(&mut index, &mut raw)?;
        raw.truncate(got - got % INDEX_ENTRY_BYTES as usize);

        let entries: Vec<IndexEntry> = raw
            .chunks_exact(INDEX_ENTRY_BYTES as usize)
            .map(IndexEntry::decode)
            .collect();

        // Bucket the present entries by data file, keeping their slot.
        let mut by_file: std::collections::BTreeMap<u16, Vec<usize>> =
            std::collections::BTreeMap::new();
        for (i, e) in entries.iter().enumerate() {
            if !e.is_absent() {
                by_file.entry(e.file).or_default().push(i);
            }
        }

        for (file, slots) in by_file {
            let lo = slots.iter().map(|i| entries[*i].start).min().unwrap();
            let hi = slots.iter().map(|i| entries[*i].end).max().unwrap();

            let path = self.dir.join(format!("headers-{file:04}.cdat"));
            let mut data = File::open(&path)
                .with_context(|| format!("opening {}", path.display()))?;
            let mut buf = vec![0u8; (hi - lo) as usize];
            data.seek(SeekFrom::Start(lo as u64))?;
            data.read_exact(&mut buf)?;

            for i in slots {
                let e = entries[i];
                out[i] = decode_header(&buf[(e.start - lo) as usize..(e.end - lo) as usize]);
            }
        }

        Ok(out)
    }

    /// Drops frozen blocks from `number` upwards.
    ///
    /// Exists for the case the freeze threshold turns out to be wrong. At
    /// [`FREEZE_DEPTH`] it should never run, and a caller reaching for it
    /// should treat that as news.
    ///
    /// Only the index is cut. Data files are left alone: with arbitrary write
    /// order there is no single offset above which everything belongs to the
    /// dropped blocks, and unreferenced bytes cost only space.
    pub fn truncate_head(&self, number: u64) -> Result<()> {
        let mut w = self.write.lock().unwrap();
        if number >= w.end_number {
            return Ok(());
        }
        w.index.set_len(number * INDEX_ENTRY_BYTES)?;
        w.index.sync_all()?;
        w.end_number = number;
        Ok(())
    }

    /// Bytes held across all data files.
    pub fn data_bytes(&self) -> u64 {
        let w = self.write.lock().unwrap();
        let mut total = w.data_len as u64;
        for f in 0..w.file_number {
            if let Ok(m) = std::fs::metadata(self.dir.join(format!("headers-{f:04}.cdat"))) {
                total += m.len();
            }
        }
        total
    }
}

fn read_entry_at(index: &mut File, n: u64) -> Result<IndexEntry> {
    let mut raw = [0u8; INDEX_ENTRY_BYTES as usize];
    index.seek(SeekFrom::Start(n * INDEX_ENTRY_BYTES))?;
    // Past the end, or in a hole, reads as zeroes: absent.
    let got = read_up_to(index, &mut raw)?;
    if got < raw.len() {
        return Ok(IndexEntry { file: 0, start: 0, end: 0 });
    }
    Ok(IndexEntry::decode(&raw))
}

/// Reads as much as is there, returning how much that was.
fn read_up_to(f: &mut File, buf: &mut [u8]) -> Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        match f.read(&mut buf[filled..])? {
            0 => break,
            n => filled += n,
        }
    }
    Ok(filled)
}

/// The highest `end` offset any entry claims in `file`.
///
/// Scanning the whole index at open is linear in the number of frozen blocks;
/// at 9.2 million entries that is 92 MB read once at start-up. It is done
/// because with arbitrary write order the last entry is not necessarily the
/// furthest one, and guessing wrong here truncates live data.
fn furthest_end(index: &mut File, end_number: u64, file: u16) -> Result<u32> {
    let mut furthest = 0u32;
    let mut raw = vec![0u8; 1 << 16];
    index.seek(SeekFrom::Start(0))?;
    let mut seen = 0u64;
    while seen < end_number {
        let got = read_up_to(index, &mut raw)?;
        if got < INDEX_ENTRY_BYTES as usize {
            break;
        }
        for chunk in raw[..got - got % INDEX_ENTRY_BYTES as usize]
            .chunks_exact(INDEX_ENTRY_BYTES as usize)
        {
            let e = IndexEntry::decode(chunk);
            if !e.is_absent() && e.file == file {
                furthest = furthest.max(e.end);
            }
            seen += 1;
        }
    }
    Ok(furthest)
}

fn decode_header(bytes: &[u8]) -> Option<Header> {
    use alloy_rlp::Decodable;
    Header::decode(&mut &bytes[..]).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{Address, Bytes, B256, U256};

    pub fn test_header(number: u64) -> Header {
        header(number)
    }

    fn header(number: u64) -> Header {
        Header {
            parent_hash: B256::repeat_byte(number as u8),
            ommers_hash: B256::repeat_byte(2),
            beneficiary: Address::repeat_byte(3),
            state_root: B256::repeat_byte(4),
            transactions_root: B256::repeat_byte(5),
            receipts_root: B256::repeat_byte(6),
            difficulty: U256::from(1_000_000u64 + number),
            number,
            gas_limit: U256::from(6_800_000u64),
            gas_used: 21_000,
            timestamp: 1_700_000_000 + number,
            extra_data: Bytes::from_static(b"rustock"),
            paid_fees: U256::from(1234u64),
            minimum_gas_price: U256::from(59_240_000u64),
            uncle_count: 0,
            logs_bloom: Default::default(),
            extension_data: None,
            umm_root: None,
            bitcoin_merged_mining_header: None,
            bitcoin_merged_mining_merkle_proof: None,
            bitcoin_merged_mining_coinbase_transaction: None,
            cached_hash: None,
            cached_hash_for_merged_mining: None,
        }
    }

    fn freezer() -> (Freezer, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        (Freezer::open(dir.path()).unwrap(), dir)
    }

    #[test]
    fn a_header_comes_back_as_it_went_in() {
        let (f, _d) = freezer();
        for n in 0..50 {
            f.put(n, &header(n)).unwrap();
        }
        f.sync().unwrap();
        assert_eq!(f.end_number(), 50);
        for n in 0..50 {
            let got = f.header(n).unwrap().expect("frozen");
            assert_eq!(got.number, n);
            assert_eq!(got.difficulty, U256::from(1_000_000u64 + n));
            assert_eq!(got.parent_hash, B256::repeat_byte(n as u8));
        }
    }

    /// The point of the format: a run is answered positionally, in order.
    #[test]
    fn a_run_is_answered_in_one_piece() {
        let (f, _d) = freezer();
        for n in 0..200 {
            f.put(n, &header(n)).unwrap();
        }
        f.sync().unwrap();
        let run = f.run(64, 100).unwrap();
        assert_eq!(run.len(), 100);
        for (i, h) in run.iter().enumerate() {
            assert_eq!(h.as_ref().unwrap().number, 64 + i as u64);
        }
    }

    /// Asking past the end is a short answer, not an error.
    #[test]
    fn a_run_off_the_end_is_padded_with_none() {
        let (f, _d) = freezer();
        for n in 0..10 {
            f.put(n, &header(n)).unwrap();
        }
        f.sync().unwrap();
        let run = f.run(5, 10).unwrap();
        assert_eq!(run.iter().filter(|h| h.is_some()).count(), 5);
        assert!(run[5..].iter().all(|h| h.is_none()));
        assert!(f.header(10).unwrap().is_none());
        assert!(f.run(99, 3).unwrap().iter().all(|h| h.is_none()));
    }

    /// The format is indifferent to arrival order. A snapshot sync walks the
    /// header chain downwards, so this is the ordinary case, not an odd one.
    #[test]
    fn a_descending_walk_reads_back_as_a_run() {
        let (f, _d) = freezer();
        for n in (0..200).rev() {
            f.put(n, &header(n)).unwrap();
        }
        f.sync().unwrap();
        assert_eq!(f.end_number(), 200);

        let run = f.run(64, 100).unwrap();
        for (i, h) in run.iter().enumerate() {
            assert_eq!(
                h.as_ref().expect("written descending, still present").number,
                64 + i as u64
            );
        }
    }

    /// A number nobody wrote is absent, not a wrong answer: the index is
    /// sparse and a hole reads as zeroes.
    #[test]
    fn a_hole_reports_absent() {
        let (f, _d) = freezer();
        f.put(0, &header(0)).unwrap();
        f.put(9, &header(9)).unwrap();
        f.sync().unwrap();
        assert!(f.contains(0));
        assert!(f.contains(9));
        for n in 1..9 {
            assert!(!f.contains(n), "#{n} was never written");
        }
        let run = f.run(0, 10).unwrap();
        assert!(run[0].is_some());
        assert!(run[9].is_some());
        assert!(run[1..9].iter().all(|h| h.is_none()));
    }

    #[test]
    fn it_reopens_where_it_left_off() {
        let dir = tempfile::tempdir().unwrap();
        {
            let f = Freezer::open(dir.path()).unwrap();
            for n in 0..30 {
                f.put(n, &header(n)).unwrap();
            }
            f.sync().unwrap();
            f.sync().unwrap();
        }
        let f = Freezer::open(dir.path()).unwrap();
        assert_eq!(f.end_number(), 30);
        assert_eq!(f.header(29).unwrap().unwrap().number, 29);
        f.put(30, &header(30)).unwrap();
        f.sync().unwrap();
        assert_eq!(f.header(30).unwrap().unwrap().number, 30);
    }

    /// A process that died mid-append leaves data the index does not cover.
    /// The index is the authority; the orphaned tail goes.
    #[test]
    fn a_torn_append_is_repaired_from_the_index() {
        let dir = tempfile::tempdir().unwrap();
        {
            let f = Freezer::open(dir.path()).unwrap();
            for n in 0..20 {
                f.put(n, &header(n)).unwrap();
            }
            f.sync().unwrap();
            f.sync().unwrap();
        }
        // Simulate a partial data write with no index entry behind it.
        let data_path = dir.path().join("headers-0000.cdat");
        let before = std::fs::metadata(&data_path).unwrap().len();
        {
            let mut d = OpenOptions::new().append(true).open(&data_path).unwrap();
            d.write_all(&[0xAA; 512]).unwrap();
        }

        let f = Freezer::open(dir.path()).unwrap();
        assert_eq!(f.end_number(), 20, "the index decides how many blocks there are");
        assert_eq!(
            std::fs::metadata(&data_path).unwrap().len(),
            before,
            "data past the last index entry must be discarded"
        );
        assert_eq!(f.header(19).unwrap().unwrap().number, 19);
    }

    #[test]
    fn truncate_head_drops_from_a_point_upwards() {
        let (f, _d) = freezer();
        for n in 0..40 {
            f.put(n, &header(n)).unwrap();
        }
        f.sync().unwrap();
        f.truncate_head(25).unwrap();
        assert_eq!(f.end_number(), 25);
        assert!(f.header(25).unwrap().is_none());
        assert!(!f.contains(25));
        assert_eq!(f.header(24).unwrap().unwrap().number, 24);

        // And it can be written to again from there.
        f.put(25, &header(25)).unwrap();
        f.sync().unwrap();
        assert_eq!(f.header(25).unwrap().unwrap().number, 25);
    }

    #[test]
    fn one_week_of_blocks_is_the_freeze_depth() {
        // 30.29 s mean block time, measured over blocks 9,177,825..9,277,825.
        let week = 7.0 * 86_400.0 / 30.29;
        assert!(
            (FREEZE_DEPTH as f64 - week).abs() < 1_000.0,
            "FREEZE_DEPTH should be about a week of blocks, got {FREEZE_DEPTH}"
        );
    }
}

#[cfg(test)]
mod buffering_tests {
    use super::tests::*;
    use super::*;

    /// A buffered block is not readable, and says so. Reporting it present
    /// would promise a read that would fail.
    #[test]
    fn a_buffered_block_reports_absent_until_synced() {
        let dir = tempfile::tempdir().unwrap();
        let f = Freezer::open(dir.path()).unwrap();
        f.put(0, &test_header(0)).unwrap();

        assert_eq!(f.end_number(), 1, "the writer knows it accepted the block");
        assert!(!f.contains(0), "but nothing is readable before a flush");
        assert!(f.header(0).unwrap().is_none());

        f.sync().unwrap();
        assert!(f.contains(0));
        assert_eq!(f.header(0).unwrap().unwrap().number, 0);
    }

    /// Crossing the buffer threshold flushes without being asked, so a long
    /// walk does not grow memory without bound.
    #[test]
    fn it_flushes_itself_once_the_buffer_fills() {
        let dir = tempfile::tempdir().unwrap();
        let f = Freezer::open(dir.path()).unwrap();
        let data = dir.path().join("headers-0000.cdat");

        let mut n = 0u64;
        while std::fs::metadata(&data).map(|m| m.len()).unwrap_or(0) == 0 {
            f.put(n, &test_header(n)).unwrap();
            n += 1;
            assert!(n < 100_000, "should have flushed long before this");
        }

        let written = std::fs::metadata(&data).unwrap().len();
        assert!(
            written >= WRITE_BUFFER_BYTES as u64,
            "a flush should carry a full buffer, got {written}"
        );
        assert!(f.contains(0), "flushed blocks are readable without a sync");
    }
}

/// Decides which headers belong in the freezer, and hands them over.
///
/// The freezer itself takes whatever it is given at whatever number; this is
/// the part that knows *which* blocks are old enough to be given to it, and it
/// is where both sync paths meet.
///
/// # One method, not two
///
/// The plan called for separate ascending and descending adapters, because the
/// original index inferred a block's start from its predecessor's end and so
/// demanded ascending writes. Carrying both offsets per entry removed that
/// constraint, and with it the difference between the two callers: an ordinary
/// sync walking up from the connection point and a snapshot sync walking down
/// from the tip now offer blocks the same way. What is left is one rule, in
/// one place.
///
/// The callers still differ in a way worth knowing: each should feed a single
/// walk without interleaving another, because blocks written consecutively
/// land consecutively, and only then is a run one read.
pub struct FreezerWriter {
    freezer: std::sync::Arc<Freezer>,
    /// Blocks strictly below this may be frozen.
    horizon: u64,
}

impl FreezerWriter {
    pub fn new(freezer: std::sync::Arc<Freezer>) -> Self {
        Self { freezer, horizon: 0 }
    }

    /// Moves the horizon to match a new chain head.
    ///
    /// Only ever forwards. A head that goes backwards is a reorg, and letting
    /// the horizon follow it down would not un-freeze anything anyway -- the
    /// blocks are already in the files. Holding it steady keeps the boundary
    /// monotonic, which is one less thing for a caller to reason about.
    pub fn set_head(&mut self, head: u64) {
        self.horizon = self.horizon.max(head.saturating_sub(FREEZE_DEPTH));
    }

    /// The highest block number that will *not* be frozen.
    pub fn horizon(&self) -> u64 {
        self.horizon
    }

    /// Offers a canonical header. Returns whether it was frozen.
    ///
    /// A block at or above the horizon is declined: it is young enough that a
    /// reorg could still replace it, and a file addressed by height has no way
    /// to express "this height means something else now" short of
    /// [`Freezer::truncate_head`].
    pub fn offer(&self, number: u64, header: &Header) -> Result<bool> {
        if number >= self.horizon {
            return Ok(false);
        }
        self.freezer.put(number, header)?;
        Ok(true)
    }

    /// Makes everything offered so far durable and readable.
    pub fn flush(&self) -> Result<()> {
        self.freezer.sync()
    }

    pub fn freezer(&self) -> &std::sync::Arc<Freezer> {
        &self.freezer
    }
}

#[cfg(test)]
mod writer_tests {
    use super::tests::test_header;
    use super::*;
    use std::sync::Arc;

    fn writer(head: u64) -> (FreezerWriter, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let f = Arc::new(Freezer::open(dir.path()).unwrap());
        let mut w = FreezerWriter::new(f);
        w.set_head(head);
        (w, dir)
    }

    /// The boundary is the part most likely to be wrong, so pin both sides of
    /// it: one block below the horizon freezes, the horizon itself does not.
    #[test]
    fn the_horizon_is_exactly_head_minus_freeze_depth() {
        let head = 1_000_000u64;
        let (w, _d) = writer(head);
        assert_eq!(w.horizon(), head - FREEZE_DEPTH);

        assert!(w.offer(w.horizon() - 1, &test_header(w.horizon() - 1)).unwrap());
        assert!(!w.offer(w.horizon(), &test_header(w.horizon())).unwrap());
        assert!(!w.offer(head, &test_header(head)).unwrap());
    }

    /// A young chain freezes nothing at all rather than underflowing.
    #[test]
    fn a_chain_shorter_than_the_depth_freezes_nothing() {
        let (w, _d) = writer(FREEZE_DEPTH / 2);
        assert_eq!(w.horizon(), 0);
        for n in 0..50 {
            assert!(!w.offer(n, &test_header(n)).unwrap());
        }
        w.flush().unwrap();
        assert_eq!(w.freezer().end_number(), 0);
    }

    /// Both walks reach the same place, which is the point of having one rule.
    #[test]
    fn ascending_and_descending_walks_agree() {
        let head = 100_000u64;
        let (up, _d1) = writer(head);
        let (down, _d2) = writer(head);

        for n in 0..head {
            up.offer(n, &test_header(n)).unwrap();
        }
        for n in (0..head).rev() {
            down.offer(n, &test_header(n)).unwrap();
        }
        up.flush().unwrap();
        down.flush().unwrap();

        let h = head - FREEZE_DEPTH;
        assert_eq!(up.freezer().end_number(), h);
        assert_eq!(down.freezer().end_number(), h);

        let a = up.freezer().run(h - 200, 200).unwrap();
        let b = down.freezer().run(h - 200, 200).unwrap();
        assert_eq!(a.len(), b.len());
        for (x, y) in a.iter().zip(b.iter()) {
            assert_eq!(
                x.as_ref().unwrap().number,
                y.as_ref().unwrap().number,
                "the two walks must produce the same freezer"
            );
        }
        assert!(!up.freezer().contains(h), "the horizon itself stays out");
    }

    /// The horizon does not retreat when the head does.
    #[test]
    fn a_reorg_does_not_move_the_horizon_backwards() {
        let (mut w, _d) = writer(1_000_000);
        let before = w.horizon();
        w.set_head(999_000);
        assert_eq!(w.horizon(), before, "a shorter head must not lower the horizon");
        w.set_head(1_001_000);
        assert_eq!(w.horizon(), 1_001_000 - FREEZE_DEPTH);
    }
}
