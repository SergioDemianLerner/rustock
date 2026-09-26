//! Walking the header chain down from the checkpoint, in parallel.
//!
//! # Why the obvious way is slow
//!
//! To ask for headers you need a hash, and the only hash you have is the one
//! the last answer gave you. So the natural walk -- ask for 192 headers, take
//! the oldest one's parent, ask again -- is strictly serial. For a node
//! starting from nothing that is about 48,000 round trips, one at a time,
//! before a single byte of state is requested. rskj's client walks this way.
//!
//! # Breaking the dependency
//!
//! A *skeleton* request answers with block identifiers at fixed heights --
//! twenty of them, 192 apart -- and it is addressed by height, not by hash. So
//! skeletons can all be asked for at once, and each identifier they return is
//! the starting hash for a header request that can also be asked for at once.
//!
//! What that buys is not fewer messages. It is that the messages no longer
//! have to wait for each other.
//!
//! # What it must not cost
//!
//! A skeleton identifier is a peer's claim about which block sits at a height.
//! Believing one would hand a peer the chain. It is never believed: it says
//! only *where to ask*, and what makes the answer trustworthy is that the
//! chunks **link**.
//!
//! Chunk boundaries are arithmetic rather than a search, because a request
//! from the hash at height `p` returns `p` down to `p-191`. So the chunk
//! below `p` starts at `p-192`, and:
//!
//! ```text
//! chunk(p).oldest.parent_hash == chunk(p-192).newest.hash()
//! ```
//!
//! A wrong identifier produces a chunk that fails that test, and the range is
//! asked of somebody else. The chain is trusted exactly when an unbroken run
//! of these links reaches from the checkpoint down to a block already in this
//! node's canonical index -- the same anchor the serial walk used, and for the
//! same reason: the canonical index is the one thing the walk cannot write.
//!
//! Every header is checked under the full consensus rules, merged-mining proof
//! of work included, exactly as before. Pipelining changes when the questions
//! are asked, not which answers are accepted.

use alloy_primitives::B256;
use rustock_core::validation::HeaderVerifier;
use rustock_core::Header;
use rustock_networking::protocol::BlockIdentifier;
use rustock_storage::BlockStore;
use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;
use tracing::debug;

/// Headers per request, and so the spacing of skeleton points. rskj serves at
/// most this many and so does rustock.
pub const HEADER_CHUNK: u64 = 192;

/// Identifiers one skeleton answer carries.
const SKELETON_POINTS: u64 = 20;

/// What the walk wants asked next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Want {
    /// Block identifiers from this height upward.
    Skeleton { start: u64 },
    /// `count` headers walking back from this hash.
    Headers { from: B256, count: u32, point: u64 },
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum WalkError {
    #[error("header #{number} failed validation: {reason}")]
    InvalidHeader { number: u64, reason: String },
    #[error("the headers for point {point} do not form a chain")]
    BrokenChunk { point: u64 },
    #[error("the chain leads back to a genesis this node has never seen")]
    ForeignGenesis,
}

/// A verified run of headers, remembered by its ends.
///
/// The headers themselves go to the store as they are checked; only the two
/// ends are kept, which is what makes walking nine million of them affordable.
#[derive(Debug, Clone)]
struct Run {
    newest: B256,
    oldest_number: u64,
    /// The parent of the oldest header: what the run below must produce.
    oldest_parent: B256,
}

pub struct HeaderWalk {
    store: Arc<BlockStore>,
    verifier: Arc<HeaderVerifier>,

    /// The block being walked down from.
    top: Header,

    /// Skeleton starts not yet asked for, and those outstanding.
    skeleton_todo: Vec<u64>,
    skeleton_sent: HashSet<u64>,
    /// Hashes learned from skeletons, by height. Claims, not facts.
    points: BTreeMap<u64, B256>,

    /// Points whose headers are outstanding, and those already verified.
    headers_sent: HashSet<u64>,
    runs: BTreeMap<u64, Run>,

    /// The header the linked chain still needs: its height, and the hash its
    /// child already named for it.
    ///
    /// This is the whole of the walk's state. A run satisfies it when the run
    /// is keyed at that height *and* its newest header is that hash; consuming
    /// it moves the need to the run's own oldest parent. Nothing else can move
    /// it, which is why a wrong skeleton identifier cannot mislead the walk --
    /// it produces a run that simply never fits.
    need_number: u64,
    need_hash: B256,
    done: bool,
}

impl HeaderWalk {
    /// Walk down from `top`, stopping at the first block this node has
    /// already accepted as canonical.
    ///
    /// There is no floor to pass in. The queue is drained from the top down
    /// and the walk stops the moment it anchors, so a node that already has
    /// most of the chain never reaches the requests for the part it has --
    /// the anchor is the floor.
    pub fn new(
        top: Header,
        store: Arc<BlockStore>,
        verifier: Arc<HeaderVerifier>,
    ) -> Self {
        let number = top.number;
        let parent = top.parent_hash;
        let top_hash = top.hash();

        // The top of the grid: the highest multiple of the chunk size at or
        // below the checkpoint. Everything above it is one bespoke request,
        // everything below is on the grid.
        let grid_top = (number / HEADER_CHUNK) * HEADER_CHUNK;

        let mut skeleton_todo = Vec::new();
        let span = HEADER_CHUNK * SKELETON_POINTS;
        let mut start = 0u64;
        while start <= grid_top {
            skeleton_todo.push(start);
            start += span;
        }
        // Asked for from the top down: the walk finishes at the bottom, but it
        // is the top that unblocks it.
        skeleton_todo.reverse();

        let _ = parent;
        Self {
            store,
            verifier,
            top,
            skeleton_todo,
            skeleton_sent: HashSet::new(),
            points: BTreeMap::new(),
            headers_sent: HashSet::new(),
            runs: BTreeMap::new(),
            need_number: number,
            need_hash: top_hash,
            done: false,
        }
    }

    pub fn is_done(&self) -> bool {
        self.done
    }

    /// The lowest height the linked chain reaches.
    pub fn frontier(&self) -> u64 {
        self.need_number
    }

    /// Whether this node already accepted this block as canonical.
    ///
    /// Asked of the canonical index, never of "is this header stored": the
    /// walk stores headers as it goes, and a check its own writes could
    /// satisfy would be no check at all.
    fn is_ours(&self, number: u64, hash: B256) -> bool {
        self.store.canonical_hash(number).ok().flatten() == Some(hash)
    }

    /// Up to `budget` things to ask for, none of them already outstanding.
    pub fn wants(&mut self, budget: usize) -> Vec<Want> {
        let mut out = Vec::new();
        if self.done {
            return out;
        }

        // Headers first: a skeleton is only useful once its points are being
        // turned into headers, and the queue below is what finishes the walk.
        let points: Vec<u64> = self
            .points
            .keys()
            .rev()
            .copied()
            .filter(|p| !self.headers_sent.contains(p) && !self.runs.contains_key(p))
            .take(budget)
            .collect();
        for point in points {
            let Some(hash) = self.points.get(&point).copied() else { continue };
            self.headers_sent.insert(point);
            out.push(Want::Headers { from: hash, count: HEADER_CHUNK as u32, point });
            if out.len() >= budget {
                return out;
            }
        }

        // The stretch above the grid, which no skeleton point covers. It stops
        // one *above* the grid top rather than at it: the grid's own run ends
        // at its point, so a bespoke run reaching that far would overlap it and
        // the two would not link.
        let grid_top = (self.top.number / HEADER_CHUNK) * HEADER_CHUNK;
        if self.top.number > grid_top
            && !self.headers_sent.contains(&self.top.number)
            && !self.runs.contains_key(&self.top.number)
        {
            self.headers_sent.insert(self.top.number);
            out.push(Want::Headers {
                from: self.top.hash(),
                count: (self.top.number - grid_top) as u32,
                point: self.top.number,
            });
            if out.len() >= budget {
                return out;
            }
        }

        while out.len() < budget {
            let Some(start) = self.skeleton_todo.pop() else { break };
            if self.skeleton_sent.insert(start) {
                out.push(Want::Skeleton { start });
            }
        }
        out
    }

    /// Give up on an outstanding request so it can be asked of someone else.
    pub fn release(&mut self, want: &Want) {
        match want {
            Want::Skeleton { start } => {
                if self.skeleton_sent.remove(start) {
                    self.skeleton_todo.push(*start);
                }
            }
            Want::Headers { point, .. } => {
                self.headers_sent.remove(point);
            }
        }
    }

    /// Skeleton identifiers: where to ask, and nothing more.
    pub fn on_skeleton(&mut self, identifiers: &[BlockIdentifier]) {
        for id in identifiers {
            if id.number == 0 || id.number > self.top.number {
                continue;
            }
            // A point already answered for is not replaced: the run that
            // linked is the truth, whatever a later peer says.
            if self.runs.contains_key(&id.number) {
                continue;
            }
            self.points.insert(id.number, id.hash);
        }
    }

    /// A run of headers, newest first, as the protocol delivers them.
    ///
    /// Verified on its own terms here -- every header's rules, every adjacent
    /// pair, the internal chain -- and stored. Whether it belongs to *our*
    /// chain is settled by whether it links, which [`Self::advance`] decides.
    pub fn on_headers(&mut self, point: u64, headers: &[Header]) -> Result<(), WalkError> {
        self.headers_sent.remove(&point);
        if headers.is_empty() {
            return Ok(());
        }

        // Newest first, and contiguous.
        let newest = &headers[0];
        for pair in headers.windows(2) {
            let (child, parent) = (&pair[0], &pair[1]);
            if child.parent_hash != parent.hash() {
                return Err(WalkError::BrokenChunk { point });
            }
            self.verifier.verify(parent, None).map_err(|e| WalkError::InvalidHeader {
                number: parent.number,
                reason: e.to_string(),
            })?;
            self.verifier.verify_against_parent(child, parent).map_err(|e| {
                WalkError::InvalidHeader { number: child.number, reason: e.to_string() }
            })?;
        }
        // The newest header of a run is never anyone's child here, so its own
        // rules are checked directly.
        self.verifier.verify(newest, None).map_err(|e| WalkError::InvalidHeader {
            number: newest.number,
            reason: e.to_string(),
        })?;

        for header in headers {
            let _ = self.store.put_header_with_hash(header.hash(), header);
        }

        let oldest = headers.last().expect("non-empty");
        self.runs.insert(
            point,
            Run {
                newest: newest.hash(),
                oldest_number: oldest.number,
                oldest_parent: oldest.parent_hash,
            },
        );
        Ok(())
    }

    /// Follow the links as far down as they now reach.
    ///
    /// Returns how many blocks the chain grew by.
    pub fn advance(&mut self) -> Result<u64, WalkError> {
        let started_at = self.need_number;

        loop {
            // Reached ground this node already accepted: everything above is
            // now anchored to work it had already taken.
            if self.is_ours(self.need_number, self.need_hash) {
                self.done = true;
                break;
            }
            if self.need_number == 0 {
                // A chain of well-formed headers back to a genesis this node
                // has never seen is another network's, or an invented one.
                return Err(WalkError::ForeignGenesis);
            }

            // A run is keyed by the height of its newest header, so the one
            // that could satisfy the need is at exactly that height.
            let Some(run) = self.runs.get(&self.need_number).cloned() else { break };
            if run.newest != self.need_hash {
                // Keyed right, wrong chain: whoever pointed us here was on a
                // fork. Drop it and let the range be asked again.
                debug!(
                    target: "rustock::snap",
                    "run at {} is not on our chain; discarding", self.need_number
                );
                self.runs.remove(&self.need_number);
                self.points.remove(&self.need_number);
                break;
            }

            self.runs.remove(&self.need_number);
            // The run covered down to its oldest header, so what is needed
            // next is that header's parent: one lower, and named by it.
            self.need_number = run.oldest_number.saturating_sub(1);
            self.need_hash = run.oldest_parent;
        }

        Ok(started_at.saturating_sub(self.need_number))
    }
}
