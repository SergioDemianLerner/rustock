//! Read and write key sets per parallel sublist, and the collision rule
//! RSKIP-144 makes a condition of a block's validity.
//!
//! From RSKIP-144, where sublists execute in parallel:
//!
//! > When all parallel sublists have finished processing, the `readMap`s and
//! > `writeMap`s are scanned to find connections between transactions of
//! > different threads.
//! > - If a key belongs to a `writeMap` and a `readMap` of another parallel
//! >   sublist, then the block is considered invalid.
//! > - If a key belongs to a `writeMap` and a `writeMap` of another parallel
//! >   sublist, then the block is also invalid.
//!
//! This is a rule a validator applies, not a property it may assume the
//! miner established. The distinction matters because the obvious shortcut
//! is wrong: executing the sublists sequentially does *not* stand in for the
//! check. Where sublists conflict, sequential and parallel execution agree
//! in some interleavings and differ in others, so a sequential executor that
//! skips this accepts blocks rskj rejects -- and does so silently, having
//! computed a state root nobody else would reach.
//!
//! Read against read is not a collision. Two sublists may read the same key
//! freely; only a write makes an ordering observable.

use alloy_primitives::{Address, B256};
use revm::state::{AccountStatus, EvmState};
use rustock_trie::{account_key, code_key, storage_key, storage_prefix_key};
use std::cell::RefCell;
use std::collections::HashSet;

/// Which sublist a key belongs to. rskj keys these by thread id, since each
/// sublist runs on its own thread; here they are the sublist's index, which
/// is the same thing said without reference to how execution is scheduled.
pub type SublistId = usize;

/// The keys each sublist read and wrote.
///
/// Keys are whole unitrie keys, not storage keys alone. rskj records every
/// key passing through its repository's `internalGet`, `internalGetValueLength`
/// and `internalGetValueHash` as a read, everything through `internalPut` as
/// a write, and an account's key as a write when the account is deleted. So
/// an account's balance or nonce counts, and two sublists crediting the same
/// address collide -- which is the intended reading, and stricter than
/// RSKIP-144's prose, which says "storage key" throughout.
#[derive(Debug, Default, Clone)]
pub struct SublistKeys {
    read: HashSet<Vec<u8>>,
    written: HashSet<Vec<u8>>,
}

/// Per-sublist key sets for one block.
#[derive(Debug, Default, Clone)]
pub struct CollisionTracker {
    sublists: Vec<SublistKeys>,
    /// Which sublist is executing. Reads and writes are attributed here.
    current: SublistId,
}

impl CollisionTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Directs subsequent reads and writes at `id`.
    pub fn set_sublist(&mut self, id: SublistId) {
        if self.sublists.len() <= id {
            self.sublists.resize_with(id + 1, SublistKeys::default);
        }
        self.current = id;
    }

    pub fn note_read(&mut self, key: &[u8]) {
        self.ensure(self.current);
        self.sublists[self.current].read.insert(key.to_vec());
    }

    pub fn note_write(&mut self, key: &[u8]) {
        self.ensure(self.current);
        self.sublists[self.current].written.insert(key.to_vec());
    }

    fn ensure(&mut self, id: SublistId) {
        if self.sublists.len() <= id {
            self.sublists.resize_with(id + 1, SublistKeys::default);
        }
    }

    pub fn clear(&mut self) {
        self.sublists.clear();
        self.current = 0;
    }

    pub fn sublist_count(&self) -> usize {
        self.sublists.len()
    }

    /// The first collision between two sublists, if there is one.
    ///
    /// Every unordered pair is compared, so a collision is reported once.
    /// Within a sublist nothing collides: a sublist may read and write the
    /// same key as much as it likes, because its own transactions are
    /// ordered with respect to each other.
    pub fn first_collision(&self) -> Option<Collision> {
        for (a, ka) in self.sublists.iter().enumerate() {
            for (b, kb) in self.sublists.iter().enumerate().skip(a + 1) {
                if let Some(key) = intersect(&ka.written, &kb.written) {
                    return Some(Collision { kind: CollisionKind::WriteWrite, a, b, key });
                }
                if let Some(key) = intersect(&ka.written, &kb.read) {
                    return Some(Collision { kind: CollisionKind::WriteRead, a, b, key });
                }
                if let Some(key) = intersect(&ka.read, &kb.written) {
                    return Some(Collision { kind: CollisionKind::WriteRead, a: b, b: a, key });
                }
            }
        }
        None
    }

    pub fn detect_collision(&self) -> bool {
        self.first_collision().is_some()
    }
}

/// Which key, between which sublists, and in which direction. Carried rather
/// than reduced to a boolean because a block rejected for this is a block
/// somebody has to diagnose, and "collision" alone names nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Collision {
    pub kind: CollisionKind,
    /// For `WriteRead`, the sublist that wrote.
    pub a: SublistId,
    pub b: SublistId,
    pub key: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CollisionKind {
    WriteWrite,
    WriteRead,
}

impl std::fmt::Display for Collision {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let key = alloy_primitives::hex::encode(&self.key);
        match self.kind {
            CollisionKind::WriteWrite => write!(
                f, "sublists {} and {} both write 0x{key}", self.a, self.b
            ),
            CollisionKind::WriteRead => write!(
                f, "sublist {} writes 0x{key}, which sublist {} reads", self.a, self.b
            ),
        }
    }
}

/// Smaller set outside, so the scan is bounded by the smaller of the two.
///
/// The *smallest* common key rather than the first one found: a block
/// rejected for this has to be diagnosed, and two nodes comparing notes
/// should name the same key. `HashSet` iteration order would not.
fn intersect(x: &HashSet<Vec<u8>>, y: &HashSet<Vec<u8>>) -> Option<Vec<u8>> {
    let (small, large) = if x.len() <= y.len() { (x, y) } else { (y, x) };
    small.iter().filter(|k| large.contains(*k)).min().cloned()
}


// ---------------------------------------------------------------------------
// Recording the sets from execution
// ---------------------------------------------------------------------------
/// Records the raw-bytes cells transaction `index` wrote.
///
/// Bridge and REMASC byte-array state bypasses revm's journal entirely
/// (see [`crate::raw_storage`]), so [`record_transaction`] cannot see it,
/// and two sublists that both drove the Bridge into writing one of its cells
/// would otherwise look independent. Writes only: the read side of that
/// overlay has no hook, which leaves a sublist that merely *reads* Bridge
/// state unrecorded -- again in the direction that misses a collision rather
/// than inventing one.
pub fn record_raw_writes(index: usize, writes: &[crate::raw_storage::RawWrite]) {
    RECORDER.with(|r| {
        let mut slot = r.borrow_mut();
        let Some(rec) = slot.as_mut() else { return };
        let Some(sublist) = sublist_of(index, &rec.edges) else { return };
        rec.tracker.set_sublist(sublist);
        for (addr, cell, _) in writes {
            if rec.excluded.contains(addr) {
                continue;
            }
            rec.tracker.note_write(&storage_key(addr, &B256::from(*cell)));
        }
    });
}


/// Which sublist transaction `index` belongs to, or `None` for the
/// **sequential** set.
///
/// `edges` is the header's `txExecutionSublistsEdges`: with edges
/// `[e0, e1]` rskj builds two parallel sublists, `[0, e0)` and `[e0, e1)`,
/// and runs `[e1, n)` afterwards on the main thread
/// (`BlockExecutor.executeParallel`). The sequential set is deliberately
/// outside the check -- `detectCollision` is called before it runs, which is
/// the whole point of having it: it is where a miner puts the transactions
/// that *do* conflict.
pub fn sublist_of(index: usize, edges: &[i16]) -> Option<SublistId> {
    let id = edges.iter().filter(|&&e| (e as usize) <= index).count();
    (id < edges.len()).then_some(id)
}

/// Per-thread recorder, off unless a block asks for it.
///
/// The thread-local follows [`crate::silent`]: the alternative is a tracker
/// threaded through `execute_block`, the handler and every precompile, and
/// the consensus path is the code least worth disturbing for the sake of a
/// check that is inert on every block mainnet has ever produced.
struct Recorder {
    tracker: CollisionTracker,
    edges: Vec<i16>,
    /// Addresses whose account cell is written outside any sublist, so
    /// attributing it to one would invent a collision. See [`start`].
    excluded: Vec<Address>,
}

thread_local! {
    static RECORDER: RefCell<Option<Recorder>> = const { RefCell::new(None) };
}

/// Starts recording this block's sublists, discarding anything already held.
///
/// `excluded` are addresses rskj's parallel path does not write from inside a
/// sublist, and which must therefore not be recorded as sublist writes:
///
/// - **the fee beneficiary.** rustock credits it once per transaction, from
///   inside the transaction. rskj does not: `TransactionListExecutor` builds
///   every executor with `postponeFeePayment = true`, so under RSKIP-144 the
///   fees are accumulated per sublist and settled afterwards. Recording the
///   per-transaction credit would make every sublist write REMASC's account
///   cell, and *every* split block would collide write-write.
/// - **precompiles maintained at an activation block.** rskj's
///   `maintainPrecompiledContractStorageRoots` runs on the block's own
///   repository before any sublist starts, so those writes belong to no
///   sublist either.
///
/// Excluding an address drops its genuine accesses too -- a transaction that
/// really does send value to REMASC stops being recorded. That is the safe
/// direction: a missed collision leaves the block accepted, which is what
/// this node did before the check existed, whereas an invented one rejects a
/// block the network considers valid.
pub fn start(edges: &[i16], excluded: &[Address]) {
    RECORDER.with(|r| {
        *r.borrow_mut() = Some(Recorder {
            tracker: CollisionTracker::new(),
            edges: edges.to_vec(),
            excluded: excluded.to_vec(),
        })
    });
}

/// Adds an address to the exclusion list of an in-progress recording.
pub fn exclude(addr: Address) {
    RECORDER.with(|r| {
        if let Some(rec) = r.borrow_mut().as_mut() {
            if !rec.excluded.contains(&addr) {
                rec.excluded.push(addr);
            }
        }
    });
}

pub fn is_recording() -> bool {
    RECORDER.with(|r| r.borrow().is_some())
}

/// Stops recording and returns what was collected, or `None` if nobody
/// called [`start`].
pub fn finish() -> Option<CollisionTracker> {
    RECORDER.with(|r| r.borrow_mut().take()).map(|r| r.tracker)
}

/// Records what transaction `index` read and wrote. A no-op unless [`start`]
/// was called, and for transactions in the sequential set.
///
/// `transaction_id` is the journal's id for this transaction, captured before
/// it ran. revm stamps it onto every account and slot the transaction first
/// touches -- including one merely *read*, because a warm entry left by an
/// earlier transaction is cold again to this one
/// (`JournalInner::load_account_mut_optional`) -- so the stamp is exactly
/// "accessed here", and an entry carrying an older id was not touched.
///
/// The classification is read-or-write, from the per-transaction baseline
/// revm keeps alongside each entry:
///
/// - the account cell is **read** whenever the account was loaded, which is
///   what rskj's `getAccountState` -> `internalGet(accountKey)` records;
/// - it is **written** when `info` has moved away from `original_info`
///   (balance, nonce or code hash), which is what reaches
///   `updateAccountState` -> `internalPut`, or when the account was
///   destroyed, which rskj records as a write of the same cell;
/// - a storage cell is **read** when the slot was touched and **written**
///   when its present value differs from the value the transaction found.
///
/// **One known gap**, in the safe direction: a write that the transaction
/// itself reverted is recorded here as a read, because revm has already
/// undone it by the time the transaction ends. rskj's tracker is monotonic --
/// `startTracking` hands the *same* tracker to every nested repository, so a
/// rolled-back write stays in the set. Our collision set is therefore a
/// subset of rskj's: we can miss a block rskj rejects, but never reject one
/// rskj accepts.
pub fn record_transaction(index: usize, transaction_id: usize, state: &EvmState) {
    RECORDER.with(|r| {
        let mut slot = r.borrow_mut();
        let Some(rec) = slot.as_mut() else { return };
        let Some(sublist) = sublist_of(index, &rec.edges) else { return };
        rec.tracker.set_sublist(sublist);

        for (addr, account) in state {
            if account.transaction_id != transaction_id {
                continue; // untouched by this transaction
            }
            if rec.excluded.contains(addr) {
                continue;
            }

            let account_key = account_key(addr);
            rec.tracker.note_read(&account_key);

            let destroyed = account.status.contains(AccountStatus::SelfDestructed)
                || account.is_selfdestructed_locally();
            if destroyed || account.info != *account.original_info {
                rec.tracker.note_write(&account_key);
            }

            // A contract deployed here writes its code cell and the
            // storage-prefix marker (rskj `setupContract`). Both are plain
            // unitrie cells under the same account. Gated on there being
            // code, which is what `apply_state_changes` gates the same two
            // writes on: a CREATE that returns nothing gets neither cell in
            // rskj either (`TransactionExecutor.create`: "without code nor
            // storage. It doesn't even call setupContract()").
            if account.is_created_locally()
                && account.info.code.as_ref().is_some_and(|c| !c.is_empty())
            {
                rec.tracker.note_write(&code_key(addr));
                rec.tracker.note_write(&storage_prefix_key(addr));
            }

            for (slot_key, cell) in &account.storage {
                if cell.transaction_id != transaction_id {
                    continue;
                }
                let key = storage_key(addr, &B256::from(*slot_key));
                rec.tracker.note_read(&key);
                if cell.present_value != cell.original_value {
                    rec.tracker.note_write(&key);
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Ported from rskj's `ReadWrittenKeysTrackerTest`, which is the
    /// authority on what counts as a collision. The names say the rule, so
    /// they are kept close to the originals.
    fn key(n: u8) -> Vec<u8> {
        vec![n; 32]
    }

    fn tracker(f: impl Fn(&mut CollisionTracker)) -> CollisionTracker {
        let mut t = CollisionTracker::new();
        f(&mut t);
        t
    }

    #[test]
    fn a_new_tracker_has_no_keys_and_no_collision() {
        let t = CollisionTracker::new();
        assert_eq!(t.sublist_count(), 0);
        assert!(!t.detect_collision());
    }

    #[test]
    fn if_a_sublist_reads_and_writes_the_same_key_there_is_no_collision() {
        let t = tracker(|t| {
            t.set_sublist(0);
            t.note_read(&key(1));
            t.note_write(&key(1));
        });
        assert!(!t.detect_collision(), "a sublist is ordered with respect to itself");
    }

    #[test]
    fn if_a_sublist_writes_the_same_key_twice_there_is_no_collision() {
        let t = tracker(|t| {
            t.set_sublist(0);
            t.note_write(&key(1));
            t.note_write(&key(1));
        });
        assert!(!t.detect_collision());
    }

    #[test]
    fn if_a_sublist_reads_the_same_key_twice_there_is_no_collision() {
        let t = tracker(|t| {
            t.set_sublist(0);
            t.note_read(&key(1));
            t.note_read(&key(1));
        });
        assert!(!t.detect_collision());
    }

    #[test]
    fn if_two_sublists_write_nothing_there_is_no_collision() {
        let t = tracker(|t| {
            t.set_sublist(0);
            t.note_read(&key(1));
            t.set_sublist(1);
            t.note_read(&key(2));
        });
        assert!(!t.detect_collision());
    }

    #[test]
    fn if_two_sublists_read_different_keys_there_is_no_collision() {
        let t = tracker(|t| {
            t.set_sublist(0);
            t.note_read(&key(1));
            t.set_sublist(1);
            t.note_read(&key(2));
        });
        assert!(!t.detect_collision());
    }

    /// The one that is easy to get wrong by symmetry: two sublists reading
    /// the same key is fine. Only a write makes an order observable.
    #[test]
    fn if_two_sublists_read_the_same_key_there_is_no_collision() {
        let t = tracker(|t| {
            t.set_sublist(0);
            t.note_read(&key(1));
            t.set_sublist(1);
            t.note_read(&key(1));
        });
        assert!(!t.detect_collision(), "read against read is not a collision");
    }

    #[test]
    fn if_two_sublists_write_different_keys_there_is_no_collision() {
        let t = tracker(|t| {
            t.set_sublist(0);
            t.note_write(&key(1));
            t.set_sublist(1);
            t.note_write(&key(2));
        });
        assert!(!t.detect_collision());
    }

    #[test]
    fn if_two_sublists_write_the_same_key_there_is_a_collision() {
        let t = tracker(|t| {
            t.set_sublist(0);
            t.note_write(&key(1));
            t.set_sublist(1);
            t.note_write(&key(1));
        });
        let c = t.first_collision().expect("write against write collides");
        assert_eq!(c.kind, CollisionKind::WriteWrite);
        assert_eq!(c.key, key(1));
    }

    #[test]
    fn if_one_sublist_writes_what_another_reads_there_is_a_collision() {
        let t = tracker(|t| {
            t.set_sublist(0);
            t.note_write(&key(1));
            t.set_sublist(1);
            t.note_read(&key(1));
        });
        let c = t.first_collision().expect("write against read collides");
        assert_eq!(c.kind, CollisionKind::WriteRead);
        assert_eq!((c.a, c.b), (0, 1), "the writer is named first");
    }

    /// The same pair in the other order must also collide, and must still
    /// name the writer first. A check written only one way round passes the
    /// test above and misses half the real cases.
    #[test]
    fn if_one_sublist_reads_what_another_writes_there_is_a_collision() {
        let t = tracker(|t| {
            t.set_sublist(0);
            t.note_read(&key(1));
            t.set_sublist(1);
            t.note_write(&key(1));
        });
        let c = t.first_collision().expect("read against write collides");
        assert_eq!(c.kind, CollisionKind::WriteRead);
        assert_eq!((c.a, c.b), (1, 0), "sublist 1 wrote, sublist 0 read");
    }

    /// rskj's `collisionWithLongerSets`: four sublists, ten distinct read
    /// keys each, then ten distinct written keys each -- all disjoint, so
    /// nothing collides however large the sets get. Then one shared write
    /// makes it collide.
    #[test]
    fn disjoint_sets_do_not_collide_however_large() {
        let mut t = CollisionTracker::new();
        let per = 10u8;
        for sublist in 0..4u8 {
            t.set_sublist(sublist as usize);
            for i in 0..per {
                t.note_read(&[sublist, 0, i]);
            }
        }
        assert!(!t.detect_collision(), "distinct reads do not collide");

        for sublist in 0..4u8 {
            t.set_sublist(sublist as usize);
            for i in 0..per {
                t.note_write(&[sublist, 1, i]);
            }
        }
        assert!(!t.detect_collision(), "distinct writes do not collide either");

        // One key written by two sublists is enough.
        t.set_sublist(0);
        t.note_write(&[9, 9, 9]);
        t.set_sublist(3);
        t.note_write(&[9, 9, 9]);
        assert!(t.detect_collision());
    }

    #[test]
    fn clearing_empties_every_sublist() {
        let mut t = tracker(|t| {
            t.set_sublist(0);
            t.note_write(&key(1));
            t.set_sublist(1);
            t.note_write(&key(1));
        });
        assert!(t.detect_collision());
        t.clear();
        assert_eq!(t.sublist_count(), 0);
        assert!(!t.detect_collision());
    }

    // -----------------------------------------------------------------
    // Attribution and recording
    // -----------------------------------------------------------------

    /// With edges `[3, 5]` rskj runs `[0,3)` and `[3,5)` in parallel and
    /// `[5, n)` afterwards -- so index 5 onwards belongs to no sublist the
    /// collision check covers.
    #[test]
    fn transactions_are_attributed_to_the_sublist_that_runs_them() {
        let edges = [3i16, 5];
        let of = |i| sublist_of(i, &edges);
        assert_eq!((of(0), of(1), of(2)), (Some(0), Some(0), Some(0)));
        assert_eq!((of(3), of(4)), (Some(1), Some(1)));
        assert_eq!(of(5), None, "the sequential set is not checked");
        assert_eq!(of(99), None);
    }

    /// A block with no edges declares no split, so nothing is attributed.
    #[test]
    fn a_block_that_does_not_split_has_no_sublists() {
        assert_eq!(sublist_of(0, &[]), None);
    }

    /// The REMASC transaction is the block's last, so it always lands in the
    /// sequential set and never collides with anything.
    #[test]
    fn the_last_transaction_is_always_sequential() {
        assert_eq!(sublist_of(7, &[2i16, 5]), None);
    }

    fn account(tid: usize, balance: u64, original: u64) -> revm::state::Account {
        let mut acct = revm::state::Account {
            info: revm::state::AccountInfo {
                balance: alloy_primitives::U256::from(balance),
                ..Default::default()
            },
            original_info: Box::new(revm::state::AccountInfo {
                balance: alloy_primitives::U256::from(original),
                ..Default::default()
            }),
            transaction_id: tid,
            storage: Default::default(),
            status: AccountStatus::Touched,
        };
        acct.storage.clear();
        acct
    }

    fn state_of(addr: Address, acct: revm::state::Account) -> EvmState {
        let mut st = EvmState::default();
        st.insert(addr, acct);
        st
    }

    const A: Address = Address::new([0xaa; 20]);
    const B: Address = Address::new([0xbb; 20]);

    #[test]
    fn two_sublists_crediting_the_same_account_collide() {
        start(&[1, 2], &[]);
        // Sublist 0's transaction moves A's balance.
        record_transaction(0, 10, &state_of(A, account(10, 7, 5)));
        // Sublist 1's transaction moves it again.
        record_transaction(1, 11, &state_of(A, account(11, 9, 7)));
        let t = finish().expect("recording was started");
        let c = t.first_collision().expect("both sublists write A");
        assert_eq!(c.kind, CollisionKind::WriteWrite);
        assert_eq!(c.key, account_key(&A));
    }

    #[test]
    fn two_sublists_merely_reading_the_same_account_do_not_collide() {
        start(&[1, 2], &[]);
        record_transaction(0, 10, &state_of(A, account(10, 5, 5)));
        record_transaction(1, 11, &state_of(A, account(11, 5, 5)));
        let t = finish().unwrap();
        assert!(!t.detect_collision(), "a balance that did not move is a read");
    }

    #[test]
    fn a_write_in_one_sublist_against_a_read_in_another_collides() {
        start(&[1, 2], &[]);
        record_transaction(0, 10, &state_of(A, account(10, 5, 5))); // read
        record_transaction(1, 11, &state_of(A, account(11, 9, 5))); // write
        let t = finish().unwrap();
        let c = t.first_collision().expect("sublist 1 writes what sublist 0 read");
        assert_eq!((c.kind, c.a, c.b), (CollisionKind::WriteRead, 1, 0));
    }

    #[test]
    fn different_accounts_in_different_sublists_do_not_collide() {
        start(&[1, 2], &[]);
        record_transaction(0, 10, &state_of(A, account(10, 9, 5)));
        record_transaction(1, 11, &state_of(B, account(11, 9, 5)));
        assert!(!finish().unwrap().detect_collision());
    }

    /// An entry left over from an earlier transaction carries that
    /// transaction's id, and must not be attributed to this one -- otherwise
    /// every account the block ever touched would appear in every sublist.
    #[test]
    fn entries_stamped_by_an_earlier_transaction_are_skipped() {
        start(&[1, 2], &[]);
        let mut st = state_of(A, account(10, 9, 5));
        st.insert(B, account(3, 9, 5)); // touched by transaction 3, not 10
        record_transaction(0, 10, &st);
        record_transaction(1, 11, &state_of(B, account(11, 9, 5)));
        assert!(
            !finish().unwrap().detect_collision(),
            "sublist 0 did not touch B in this transaction"
        );
    }

    /// Transactions in the sequential set run after the check, so what they
    /// touch cannot invalidate the block.
    #[test]
    fn the_sequential_set_is_not_recorded() {
        start(&[1, 2], &[]);
        record_transaction(0, 10, &state_of(A, account(10, 9, 5)));
        record_transaction(2, 12, &state_of(A, account(12, 11, 9)));
        assert!(!finish().unwrap().detect_collision());
    }

    /// Why the fee beneficiary is excluded: rustock credits it inside every
    /// transaction, rskj (with `postponeFeePayment`) does not. Without the
    /// exclusion every split block would collide write-write on it.
    #[test]
    fn an_excluded_address_is_not_recorded_at_all() {
        start(&[1, 2], &[A]);
        record_transaction(0, 10, &state_of(A, account(10, 9, 5)));
        record_transaction(1, 11, &state_of(A, account(11, 13, 9)));
        let t = finish().unwrap();
        assert!(!t.detect_collision());
        assert_eq!(t.sublist_count(), 2, "the sublists still exist, just empty");
    }

    #[test]
    fn storage_cells_collide_on_their_own_key() {
        let slot = alloy_primitives::U256::from(42u64);
        let cell = |tid, present: u64, original: u64| revm::state::EvmStorageSlot {
            original_value: alloy_primitives::U256::from(original),
            present_value: alloy_primitives::U256::from(present),
            transaction_id: tid,
            is_cold: false,
        };
        start(&[1, 2], &[]);
        let mut a0 = account(10, 5, 5);
        a0.storage.insert(slot, cell(10, 1, 0)); // written
        record_transaction(0, 10, &state_of(A, a0));
        let mut a1 = account(11, 5, 5);
        a1.storage.insert(slot, cell(11, 0, 0)); // only read
        record_transaction(1, 11, &state_of(A, a1));
        let t = finish().unwrap();
        let c = t.first_collision().expect("one writes the slot the other reads");
        assert_eq!(c.kind, CollisionKind::WriteRead);
        assert_eq!(c.key, storage_key(&A, &B256::from(slot)));
    }

    /// Bridge state does not go through revm's journal, so without this the
    /// two sublists below would look independent.
    #[test]
    fn two_sublists_writing_the_same_bridge_cell_collide() {
        let cell = alloy_primitives::U256::from(7u64);
        let write = |v: u8| vec![(A, cell, Some(vec![v]))];
        start(&[1, 2], &[]);
        record_raw_writes(0, &write(1));
        record_raw_writes(1, &write(2));
        let t = finish().unwrap();
        let c = t.first_collision().expect("both sublists write the cell");
        assert_eq!(c.kind, CollisionKind::WriteWrite);
        assert_eq!(c.key, storage_key(&A, &B256::from(cell)));
    }

    #[test]
    fn a_bridge_cell_written_only_by_the_sequential_set_does_not_collide() {
        let cell = alloy_primitives::U256::from(7u64);
        start(&[1, 2], &[]);
        record_raw_writes(0, &[(A, cell, Some(vec![1]))]);
        record_raw_writes(2, &[(A, cell, Some(vec![2]))]);
        assert!(!finish().unwrap().detect_collision());
    }

    #[test]
    fn nothing_is_recorded_until_a_block_asks_for_it() {
        assert!(!is_recording());
        record_transaction(0, 10, &state_of(A, account(10, 9, 5)));
        assert!(finish().is_none());
    }

    /// Non-adjacent sublists must be compared too: a loop that only checks
    /// neighbours passes everything above and misses this.
    #[test]
    fn sublists_that_are_not_adjacent_are_still_compared() {
        let t = tracker(|t| {
            t.set_sublist(0);
            t.note_write(&key(7));
            t.set_sublist(1);
            t.note_read(&key(8));
            t.set_sublist(2);
            t.note_write(&key(7));
        });
        let c = t.first_collision().expect("0 and 2 collide");
        assert_eq!((c.a, c.b), (0, 2));
    }
}
