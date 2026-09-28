//! Downloading a state trie from several peers at once.
//!
//! # The shape of the problem
//!
//! The trie has a single linear address space: every node sits at an offset
//! in the in-order traversal, and offsets come from sizes each node's hash
//! commits to. So "download the state" is "cover `0..total`", and any two
//! ranges can be fetched independently, from unrelated peers, in any order.
//!
//! That is the whole of the parallelism, and it needs no coordination between
//! peers: a chunk proves itself against the state root, so a peer serving the
//! middle of the trie neither knows nor affects what another is serving.
//!
//! # Not knowing `total` at the start
//!
//! The snap status message offers a trie size, but it is a peer's claim and
//! the download cannot rest on it. It is used only to fan out: the space is
//! split into as many slices as there are requests allowed in flight, and
//! workers start on each.
//!
//! The first chunk that verifies -- from any offset, from any peer -- carries
//! the root node, which commits to the size of the whole trie. At that point
//! the real total replaces the hint and the slices are trimmed or extended to
//! fit. A peer that overstates the size costs a few wasted requests at the
//! end; one that understates it cannot truncate the download, because the
//! last slice grows to the size the root proves.
//!
//! # What a bad peer can do
//!
//! Serve nothing, serve slowly, or serve something that fails verification.
//! All three end the same way: the range goes back in the queue and someone
//! else is asked. Nothing a peer sends is written to the store before it is
//! checked against the root, so a failed chunk leaves no trace.

use super::SnapConfig;

// Verification uses the node's read concurrency: the walk is dominated by
// resolving trie nodes out of the store, which is the same question that
// setting answers. See `rustock_storage::set_read_threads`.
use alloy_primitives::{keccak256, B256};
use rustock_networking::protocol::snap::{ChunkPayload, SnapEntry};
use rustock_trie::snapshot::total_size;
use rustock_trie::snapshot_proof::{verify_chunk, ChunkProof, Entry, VerifyError};
use rustock_trie::{TrieNode, TrieStore};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tracing::{debug, info, trace};

/// A contiguous stretch of the traversal that one worker walks front to back.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Slice {
    /// First offset this slice is responsible for.
    start: u64,
    /// Next offset to ask for. Everything before this, within the slice, is
    /// downloaded and stored.
    cursor: u64,
    /// One past the last offset this slice is responsible for.
    end: u64,
    /// Whether a request for `cursor` is outstanding.
    in_flight: bool,
    /// Ask for exactly `cursor` rather than the cell containing it.
    ///
    /// Set when a cell answer did not move the cursor -- a server that caps
    /// its responses below the grid size will do that -- because asking for
    /// the same cell again would produce the same answer forever. Costs a
    /// cache miss at that server, which is strictly better than a stall.
    exact: bool,
}

impl Slice {
    fn wants_work(&self) -> bool {
        !self.in_flight && self.cursor < self.end
    }
}

/// A range to ask some peer for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChunkRequest {
    /// Offset to start at.
    pub from: u64,
    /// Bytes of nodes wanted. A server may send fewer.
    pub budget: u64,
}

/// What a verified chunk added to the download.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Progress {
    /// Nodes written to the store.
    pub nodes: usize,
    /// Offset space newly covered.
    pub advanced: u64,
    /// Whether that was the last of it.
    pub complete: bool,
}

/// How much more than the requested budget a chunk may be before it is thrown
/// away unread.
///
/// A server is free to round up, and a straddling node at the end can push a
/// chunk over. What this stops is a peer answering a 100KB request with the
/// 16MB the transport allows, repeatedly, to spend the client's CPU on
/// verification. Rejecting on size costs nothing; verifying does not.
const OVERSIZE_FACTOR: u64 = 4;

/// A chunk that arrived in rskj's format, already rebuilt and checked.
///
/// Kept apart from [`ChunkProof`] because the two prove themselves
/// differently: a proved chunk is replayed against the trie's own traversal,
/// while this one is reconstructed and checked at the root. Both end with
/// nodes in consensus form, which is all the store cares about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RebuiltChunk {
    pub nodes: Vec<(B256, Vec<u8>)>,
    pub long_values: Vec<Vec<u8>>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ChunkError {
    #[error("the chunk failed verification: {0}")]
    Invalid(#[from] VerifyError),
    #[error("no request for offset {0} was outstanding")]
    Unsolicited(u64),
    #[error("the peer's rskj-format chunk did not rebuild: {0}")]
    LegacyRebuild(String),
    #[error("the answer is {got} bytes against a {asked} byte request")]
    Oversized { asked: u64, got: u64 },
}

/// Downloading one state trie.
///
/// Holds no peers and does no I/O: it decides what to ask for and judges what
/// comes back. Which peer to ask is the caller's business, which keeps peer
/// selection, scoring and timeouts out of the part that has to be right.
pub struct StateDownload {
    root_hash: B256,
    store: Arc<dyn TrieStore>,
    budget: u64,
    /// The grid cells sit on. Requests are aligned to it so that every client
    /// asks for the same ranges and a server can answer from cache.
    grid: u64,
    /// The most requests to have outstanding at once.
    max_in_flight: usize,
    slices: Vec<Slice>,
    /// The size the root proves, once a chunk has carried it.
    total: Option<u64>,
    /// The size a peer claimed, used until then.
    hinted_total: u64,
    stored_nodes: u64,
    stored_bytes: u64,
}

impl StateDownload {
    pub fn new(
        root_hash: B256,
        hinted_total: u64,
        config: &SnapConfig,
        store: Arc<dyn TrieStore>,
    ) -> Self {
        Self::on_grid(root_hash, hinted_total, config, config.chunk_grid, store)
    }

    /// The same, but aligned to a grid the server advertised rather than our
    /// own. Asking on the server's grid is what makes its cache useful; a
    /// server that states no grid (rskj does not) leaves us on ours.
    pub fn on_grid(
        root_hash: B256,
        hinted_total: u64,
        config: &SnapConfig,
        grid: u64,
        store: Arc<dyn TrieStore>,
    ) -> Self {
        let workers = config.max_in_flight.max(1);
        let hinted_total = hinted_total.max(1);

        let mut download = Self {
            root_hash,
            store,
            budget: config.chunk_bytes,
            grid: if grid == 0 { config.chunk_grid.max(1) } else { grid },
            max_in_flight: workers,
            slices: Vec::new(),
            total: None,
            hinted_total,
            stored_nodes: 0,
            stored_bytes: 0,
        };
        download.slices = download.split(0, hinted_total, workers);
        download
    }

    /// Cuts `from..to` into `count` slices of roughly equal size.
    fn split(&self, from: u64, to: u64, count: usize) -> Vec<Slice> {
        let span = to.saturating_sub(from);
        let count = (count as u64).min(span.max(1)) as usize;
        let step = span / count as u64;

        (0..count)
            .map(|i| {
                let start = from + step * i as u64;
                let end = if i + 1 == count { to } else { from + step * (i as u64 + 1) };
                Slice { start, cursor: start, end, in_flight: false, exact: false }
            })
            .collect()
    }

    /// The next range to ask a peer for, if any is free.
    ///
    /// Returns `None` when every slice is either finished or already out with
    /// someone -- meaning the caller should wait for an answer rather than
    /// open another request.
    pub fn next_request(&mut self) -> Option<ChunkRequest> {
        // The cap is on requests, not on slices. Those were the same number
        // until the trie turned out bigger than advertised and more slices
        // were cut; keeping the two separate means the limit stays the limit.
        if self.slices.iter().filter(|s| s.in_flight).count() >= self.max_in_flight {
            return None;
        }
        let grid = self.grid;
        let slice = self.slices.iter_mut().find(|s| s.wants_work())?;
        slice.in_flight = true;
        // Asked for on the grid, whatever the cursor. A cell is the unit a
        // server can cache and share; asking for `cursor` exactly would give
        // every client a different range and every server a cache miss.
        let from =
            if slice.exact { slice.cursor } else { (slice.cursor / grid) * grid };
        Some(ChunkRequest { from, budget: self.budget })
    }

    /// Give up on an outstanding request so someone else can be asked.
    ///
    /// The range is untouched: nothing was written, so there is nothing to
    /// undo. Called on a timeout, a disconnect, or a chunk that failed to
    /// verify.
    pub fn release(&mut self, from: u64) {
        let grid = self.grid;
        if let Some(i) = self.slice_for(from) {
            self.slices[i].in_flight = false;
        }
        let _ = grid;
    }

    /// The slice an answer for `from` belongs to.
    ///
    /// A cell begins at or before the cursor that asked for it, because the
    /// request was aligned down to the grid. An exact match wins over an
    /// aligned one, so two slices inside the same cell cannot be confused.
    fn slice_for(&self, from: u64) -> Option<usize> {
        let grid = self.grid;
        self.slices
            .iter()
            .position(|s| s.in_flight && s.cursor == from)
            .or_else(|| {
                self.slices
                    .iter()
                    .position(|s| s.in_flight && (s.cursor / grid) * grid == from)
            })
    }

    /// Check a chunk and, if it holds up, keep it.
    ///
    /// The order matters: verify first, store second. A chunk that fails
    /// leaves the store exactly as it was.
    pub fn accept(&mut self, from: u64, proof: &ChunkProof) -> Result<Progress, ChunkError> {
        if self.slice_for(from).is_none() {
            return Err(ChunkError::Unsolicited(from));
        }

        // Judged on size before anything is read: an answer wildly larger than
        // the question is refused without paying to verify it.
        //
        // Only the node messages and the witness are counted, because they are
        // the only part a peer chooses. A long value's size is fixed by the
        // node that commits to it, and that node's hash chains to the state
        // root -- a peer cannot inflate one, it can only send the real bytes
        // or fail verification. Counting them here would refuse a genuine
        // account whose code is larger than the chunk, making it permanently
        // undownloadable: a worse failure than the one this guards against.
        //
        // A chunk of one node is exempt for the same reason: a server always
        // sends at least one, whatever the budget.
        let allowed = self.budget.saturating_mul(OVERSIZE_FACTOR).max(1 << 18);
        let got: u64 = proof.entries.iter().map(|e| e.message.len() as u64).sum::<u64>()
            + proof.witness.iter().map(|w| w.len() as u64).sum::<u64>();
        if got > allowed && proof.entries.len() > 1 {
            self.release(from);
            return Err(ChunkError::Oversized { asked: self.budget, got });
        }

        let verified = match verify_chunk(self.root_hash, from, proof) {
            Ok(v) => v,
            Err(e) => {
                self.release(from);
                return Err(ChunkError::Invalid(e));
            }
        };

        // The root proves the size of the whole trie. Once that is in hand
        // the peer's claim is no longer needed anywhere.
        if self.total.is_none() {
            self.adopt_total(verified.total);
        }

        for node in &verified.nodes {
            self.store.put(keccak256(&node.message).as_slice(), &node.message);
            self.stored_bytes += node.message.len() as u64;
            for value in &node.long_values {
                self.store.put(keccak256(value).as_slice(), value);
                self.stored_bytes += value.len() as u64;
            }
        }
        self.stored_nodes += verified.nodes.len() as u64;

        let reached = verified.nodes.last().expect("verified chunks are non-empty").end();
        let mut advanced = 0;
        if let Some(i) = self.slice_for(from) {
            let slice = &mut self.slices[i];
            advanced = reached.saturating_sub(slice.cursor).min(slice.end - slice.cursor);
            // A cell can begin before the cursor, so it never moves backwards:
            // the nodes before the cursor were already stored, and storing them
            // again is harmless but must not un-advance the slice.
            //
            // An answer that moved nothing means this server serves less than
            // a whole cell, so the next request names the cursor exactly
            // rather than asking the same cell again and again.
            slice.exact = reached <= slice.cursor;
            slice.cursor = reached.max(slice.cursor);
            slice.in_flight = false;
        }

        trace!(
            target: "rustock::snap",
            "accepted {} nodes from offset {from}, reaching {reached}",
            verified.nodes.len()
        );

        Ok(Progress { nodes: verified.nodes.len(), advanced, complete: self.is_complete() })
    }

    /// Keep a chunk that arrived in rskj's format.
    ///
    /// It has already proved itself: the only way to read one is to rebuild
    /// it, and the rebuild is checked against the state root. So there is
    /// nothing left to verify here, only nodes to store and a cursor to move.
    ///
    /// The offsets it covers are not stated anywhere -- rskj's format carries
    /// no per-node offsets and the rebuild derives structure, not position --
    /// so the slice advances by the cell it asked for rather than by where
    /// the nodes turned out to sit.
    pub fn accept_rebuilt(&mut self, from: u64, chunk: &RebuiltChunk) -> Progress {
        for (hash, message) in &chunk.nodes {
            self.store.put(hash.as_slice(), message);
            self.stored_bytes += message.len() as u64;
        }
        for value in &chunk.long_values {
            self.store.put(keccak256(value).as_slice(), value);
            self.stored_bytes += value.len() as u64;
        }
        self.stored_nodes += chunk.nodes.len() as u64;

        let reached = from.saturating_add(self.grid);
        let mut advanced = 0;
        if let Some(i) = self.slice_for(from) {
            let slice = &mut self.slices[i];
            advanced = reached.saturating_sub(slice.cursor).min(slice.end - slice.cursor);
            slice.exact = reached <= slice.cursor;
            slice.cursor = reached.max(slice.cursor);
            slice.in_flight = false;
        }

        // The size of the trie comes from the root, which a rebuilt chunk
        // always reconstructs; without a parsed root here it stays whatever
        // the peer advertised until a proved chunk settles it.
        if self.total.is_none() {
            self.adopt_total(self.hinted_total);
        }

        Progress { nodes: chunk.nodes.len(), advanced, complete: self.is_complete() }
    }

    /// Replace the hinted size with the one the root proves, and reshape the
    /// slices around it.
    fn adopt_total(&mut self, total: u64) {
        self.total = Some(total);
        if total == self.hinted_total {
            return;
        }
        debug!(
            target: "rustock::snap",
            "trie size is {total}, peer hinted {}; adjusting slices", self.hinted_total
        );

        // Smaller than advertised: anything wholly past the end is finished by
        // definition, and the slice straddling the end stops there.
        for slice in self.slices.iter_mut() {
            slice.end = slice.end.min(total);
            slice.start = slice.start.min(slice.end);
            slice.cursor = slice.cursor.clamp(slice.start, slice.end);
        }

        // Bigger than advertised: the extra space gets its own slices rather
        // than being tacked onto the last one. A peer that understates the
        // size would otherwise have left the tail of the trie to a single
        // worker -- a cheap way to make a parallel download serial.
        if total > self.hinted_total {
            let workers = self.max_in_flight.max(1);
            let extra = self.split(self.hinted_total, total, workers);
            self.slices.extend(extra);
        }
    }

    /// Every offset accounted for.
    pub fn is_complete(&self) -> bool {
        self.total.is_some() && self.slices.iter().all(|s| s.cursor >= s.end)
    }

    /// Offset space covered so far, and the total once it is known.
    pub fn progress(&self) -> (u64, Option<u64>) {
        // A slice's cursor can run past its end: the last chunk of a slice
        // usually straddles the boundary, and the whole node comes with it.
        // Those bytes belong to the next slice, which will fetch them again,
        // so counting them here would put progress over 100%.
        let covered: u64 = self
            .slices
            .iter()
            .map(|s| s.cursor.min(s.end).saturating_sub(s.start))
            .sum();
        (covered, self.total)
    }

    pub fn stored_nodes(&self) -> u64 {
        self.stored_nodes
    }

    pub fn stored_bytes(&self) -> u64 {
        self.stored_bytes
    }

    pub fn root_hash(&self) -> B256 {
        self.root_hash
    }

    /// The ranges the workers divide the trie into.
    #[cfg(test)]
    pub(crate) fn slice_bounds(&self) -> Vec<(u64, u64)> {
        self.slices.iter().map(|s| (s.start, s.end)).collect()
    }

    /// Walk the assembled trie in the local store and confirm it is all
    /// there.
    ///
    /// Each chunk was already proved against the root, so this is not a second
    /// opinion on the peers -- it is a check on this node: that every verified
    /// node reached the disk and can be read back. Cheap next to the download,
    /// and it fails here rather than during the first block that touches a
    /// missing node.
    pub fn verify_stored(&self) -> Result<u64, String> {
        let message = self
            .store
            .get(self.root_hash.as_slice())
            .ok_or_else(|| format!("state root {:?} is not in the store", self.root_hash))?;
        let root = TrieNode::try_from_message(&message, self.store.as_ref())
            .ok_or_else(|| "the stored state root does not parse".to_string())?;

        let total = total_size(&root, self.store.as_ref());
        if let Some(expected) = self.total {
            if total != expected {
                return Err(format!("stored trie spans {total} bytes, expected {expected}"));
            }
        }

        // Walking it chunk by chunk visits every node and resolves every
        // hash, which is exactly the question being asked.
        //
        // Byte ranges are independent of each other -- `chunk_until` says so
        // next door: a cell "depends on nothing but the trie and the two
        // numbers", which is what lets the snapshot *server* answer arbitrary
        // cells for different peers at once. So the walk splits: worker `k`
        // takes `[k*total/N, (k+1)*total/N)`.
        //
        // Splitting by bytes rather than by key space keeps the workers
        // balanced, because bytes traversed is what the work is proportional
        // to. A node straddling a boundary is emitted whole into both
        // neighbouring cells, so nothing falls between two workers.
        const REPORT_EVERY: Duration = Duration::from_secs(5);
        let workers = rustock_storage::read_threads().max(1);
        let started = Instant::now();

        info!(
            target: "rustock::snap",
            "verifying the stored state: {} MB to walk across {workers} thread(s)",
            total / (1 << 20)
        );

        let covered = AtomicU64::new(0);
        let nodes = AtomicU64::new(0);
        let finished = AtomicBool::new(false);
        let failure: Mutex<Option<String>> = Mutex::new(None);

        std::thread::scope(|scope| {
            // One thread does nothing but say how it is going. Ten minutes of
            // silence is indistinguishable from a hang (#187), and with the
            // walk spread over several threads no single one of them knows
            // the whole picture.
            let reporter = scope.spawn(|| {
                // Checked often, reported rarely: the workers are joined
                // before `finished` is set, so a long sleep here would hold
                // the whole walk open waiting for this thread to notice.
                while !finished.load(Ordering::Relaxed) {
                    let mut waited = Duration::ZERO;
                    while waited < REPORT_EVERY && !finished.load(Ordering::Relaxed) {
                        std::thread::sleep(Duration::from_millis(100));
                        waited += Duration::from_millis(100);
                    }
                    if finished.load(Ordering::Relaxed) {
                        break;
                    }
                    let done_bytes = covered.load(Ordering::Relaxed);
                    let fraction = done_bytes as f64 / total as f64;
                    let elapsed = started.elapsed().as_secs_f64();
                    let eta = if fraction > 0.0 {
                        elapsed * (1.0 - fraction) / fraction
                    } else {
                        0.0
                    };
                    info!(
                        target: "rustock::snap",
                        "verifying the stored state: {:.1}% ({}/{} MB), {} nodes, {:.0}s elapsed, ~{:.0}s left",
                        fraction * 100.0,
                        done_bytes / (1 << 20),
                        total / (1 << 20),
                        nodes.load(Ordering::Relaxed),
                        elapsed,
                        eta
                    );
                }
            });

            let mut handles = Vec::with_capacity(workers);
            for k in 0..workers {
                let root = &root;
                let covered = &covered;
                let nodes = &nodes;
                let failure = &failure;
                let store = self.store.as_ref();
                handles.push(scope.spawn(move || {
                    let lo = total * k as u64 / workers as u64;
                    let hi = total * (k as u64 + 1) / workers as u64;
                    let mut offset = lo;
                    while offset < hi {
                        let chunk =
                            rustock_trie::snapshot::chunk_from(root, offset, 1 << 20, store);
                        let Some(last) = chunk.last() else {
                            // A hole must still stop the sync. Skipping one
                            // quietly would be far worse than a slow verify:
                            // the whole point of this phase is failing here
                            // rather than during the first block that touches
                            // a missing node.
                            let mut slot = failure.lock().unwrap();
                            if slot.is_none() {
                                *slot = Some(format!(
                                    "the stored trie has a hole at offset {offset}"
                                ));
                            }
                            return;
                        };
                        nodes.fetch_add(chunk.len() as u64, Ordering::Relaxed);
                        let end = last.end();
                        covered.fetch_add(end.saturating_sub(offset), Ordering::Relaxed);
                        offset = end;
                    }
                }));
            }

            // Join the workers here, not at the end of the scope: the
            // reporter runs until `finished`, and the scope will not return
            // until the reporter has, so setting it afterwards would deadlock.
            for h in handles {
                let _ = h.join();
            }
            finished.store(true, Ordering::Relaxed);
            let _ = reporter.join();
        });

        if let Some(why) = failure.into_inner().unwrap() {
            return Err(why);
        }

        let nodes = nodes.into_inner();
        info!(
            target: "rustock::snap",
            "stored state verified: {} nodes, {} MB, {:.0}s across {workers} thread(s)",
            nodes,
            total / (1 << 20),
            started.elapsed().as_secs_f64()
        );
        Ok(nodes)
    }
}

/// What a chunk turned out to be, whichever dialect it arrived in.
#[derive(Debug)]
pub enum Arrived {
    /// rustock's own format: nodes to replay against the traversal.
    Proved(ChunkProof),
    /// rskj's format: already rebuilt and checked against the root, because
    /// there is no way to check it that does not also rebuild it.
    Rebuilt(RebuiltChunk),
}

/// Turns a wire payload into something the download can use.
///
/// An rskj chunk is rebuilt here rather than later, because unlike a proved
/// chunk it cannot be examined without being reconstructed: its nodes are
/// missing the child hashes that would let them be checked one at a time.
pub fn chunk_from_payload(
    payload: &ChunkPayload,
    root_hash: B256,
) -> Result<Arrived, ChunkError> {
    match payload {
        ChunkPayload::Proved { entries, witness } => Ok(Arrived::Proved(ChunkProof {
            entries: entries.iter().map(entry_from_wire).collect(),
            witness: witness.iter().map(|w| w.to_vec()).collect(),
        })),
        ChunkPayload::Legacy(blob) => {
            let chunk = rustock_trie::snapshot_legacy::decode_blob(blob)
                .ok_or_else(|| ChunkError::LegacyRebuild("malformed blob".into()))?;
            let rebuilt = rustock_trie::snapshot_legacy::rebuild(&chunk, root_hash)
                .map_err(|e| ChunkError::LegacyRebuild(e.to_string()))?;
            Ok(Arrived::Rebuilt(RebuiltChunk {
                nodes: rebuilt.nodes,
                long_values: rebuilt.long_values,
            }))
        }
    }
}

/// Kept for callers that only handle rustock's own format.
pub fn proof_from_payload(payload: &ChunkPayload) -> Result<ChunkProof, ChunkError> {
    match payload {
        ChunkPayload::Legacy(_) => {
            Err(ChunkError::LegacyRebuild("a proved chunk was expected".into()))
        }
        ChunkPayload::Proved { entries, witness } => Ok(ChunkProof {
            entries: entries.iter().map(entry_from_wire).collect(),
            witness: witness.iter().map(|w| w.to_vec()).collect(),
        }),
    }
}

fn entry_from_wire(entry: &SnapEntry) -> Entry {
    Entry {
        message: entry.message.to_vec(),
        long_values: entry.long_values.iter().map(|v| v.to_vec()).collect(),
    }
}
