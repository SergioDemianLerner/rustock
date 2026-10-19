//! Deciding which sublist each transaction goes in (RSKIP-144).
//!
//! A port of rskj's `ParallelizeTransactionHandler`, kept deliberately close
//! to it. The partition a miner chooses is not consensus -- a validator only
//! checks that the sublists do not collide, and RSKIP-144 presents this as a
//! "suggested miner implementation" -- but matching it means our blocks are
//! partitioned the way the network's other miner would partition them, and it
//! means rskj's own tests can be the oracle for ours.
//!
//! # The shape
//!
//! The miner executes transactions **in order**, against the state as it
//! accumulates, and after each one asks where it goes using only the keys
//! that transaction touched. The decision is greedy and never revisited: a
//! transaction placed in a sublist stays there, and the maps grow as it goes.
//!
//! That the placement is decided from an execution against the *sequential*
//! state, while the sublists will later run against the parent state, is an
//! approximation -- and it is why the validator re-checks rather than
//! trusting the header. See [`crate::collision`].
//!
//! # The rule
//!
//! Given a transaction's read and written keys, in this order:
//!
//! 1. **Sender.** A sender already placed in the sequential sublist pins the
//!    transaction there. One placed in a parallel sublist becomes the initial
//!    candidate.
//! 2. **Reads against earlier writes.** A key this transaction reads that
//!    some sublist wrote connects them.
//! 3. **Writes against earlier writes.**
//! 4. **Writes against earlier reads.** More than one earlier reader is an
//!    immediate loss.
//! 5. **No connection:** the parallel sublist with the least gas used that
//!    the transaction fits in.
//!
//! At steps 2 to 4, a *second, different* candidate means the transaction
//! would connect two parallel sublists, and the handler gives up and takes
//! the sequential one rather than merging them. rskj's comment calls this the
//! "greedy decision", and it is the main thing a cleverer partitioner would
//! do better.
//!
//! Two asymmetries are easy to lose in a port and are load bearing:
//!
//! - the write map is `key -> one sublist`, so the **last writer wins**,
//!   while the read map is `key -> many sublists`;
//! - a sender is recorded with "insert if absent" for a parallel sublist but
//!   **overwritten** for the sequential one, so a sender that lands there
//!   once stays there for the rest of the block.

use alloy_primitives::Address;
use std::collections::{BTreeSet, HashMap};

/// Index into the partition: `0..parallel_count` are the parallel sublists,
/// and the last is the sequential one.
pub type SublistId = usize;

/// rskj `BlockUtils.SEQUENTIAL_THREAD_COUNT`.
const SEQUENTIAL_SUBLISTS: u64 = 1;

/// The gas limit of each sublist, as rskj computes it
/// (`BlockUtils.getSublistGasLimit`).
///
/// Three cases, none of them in RSKIP-144, which says only that every
/// sublist's limit is `block gas limit / (N + 1)`:
///
/// - the sequential sublist keeps the division remainder, so it is never
///   exactly equal to a parallel one unless the division is exact;
/// - below `min_sequential`, the sequential sublist is guaranteed that much
///   and the parallel ones divide what is left;
/// - and a block gas limit at or under `min_sequential` leaves the parallel
///   sublists with **nothing**, which forbids any parallel sublist at all.
pub fn sublist_gas_limits(
    block_gas_limit: u64,
    parallel_count: u64,
    min_sequential: u64,
) -> (u64, u64) {
    let total = parallel_count + SEQUENTIAL_SUBLISTS;
    if block_gas_limit / total >= min_sequential {
        let parallel = block_gas_limit / total;
        return (parallel, block_gas_limit - parallel_count * parallel);
    }
    if block_gas_limit <= min_sequential {
        return (0, block_gas_limit);
    }
    let parallel = (block_gas_limit - min_sequential) / parallel_count;
    (parallel, block_gas_limit - parallel_count * parallel)
}

/// What a transaction touched, as the partitioner needs to see it.
#[derive(Debug, Clone, Default)]
pub struct TxFootprint {
    pub sender: Address,
    /// Charged against the sublist's limit before the transaction is placed.
    /// rskj tests the declared `gasLimit`, not the gas the transaction used.
    pub gas_limit: u64,
    /// Added to the sublist's running total once it is placed.
    pub gas_used: u64,
    pub reads: Vec<Vec<u8>>,
    pub writes: Vec<Vec<u8>>,
}

#[derive(Debug, Clone)]
struct Sublist {
    gas_used: u64,
    gas_limit: u64,
    transactions: Vec<usize>,
}

impl Sublist {
    fn fits(&self, gas_limit: u64) -> bool {
        self.gas_used.saturating_add(gas_limit) <= self.gas_limit
    }
}

/// Assigns transactions to sublists as a block is built.
#[derive(Debug, Clone)]
pub struct SublistPartitioner {
    /// The one sublist that wrote each key. Last writer wins, as in rskj.
    written_by: HashMap<Vec<u8>, SublistId>,
    /// Every sublist that read each key. `BTreeSet` rather than a hash set
    /// so that a single-reader lookup is deterministic; rskj reads
    /// `iterator().next()` there and is saved only by having already
    /// returned when the set has more than one element.
    read_by: HashMap<Vec<u8>, BTreeSet<SublistId>>,
    sender_of: HashMap<Address, SublistId>,
    sublists: Vec<Sublist>,
}

impl SublistPartitioner {
    /// `parallel_count` parallel sublists plus the sequential one.
    pub fn new(parallel_count: usize, block_gas_limit: u64, min_sequential: u64) -> Self {
        let (parallel_limit, sequential_limit) =
            sublist_gas_limits(block_gas_limit, parallel_count as u64, min_sequential);
        let mut sublists: Vec<Sublist> = (0..parallel_count)
            .map(|_| Sublist { gas_used: 0, gas_limit: parallel_limit, transactions: Vec::new() })
            .collect();
        sublists.push(Sublist {
            gas_used: 0,
            gas_limit: sequential_limit,
            transactions: Vec::new(),
        });
        Self {
            written_by: HashMap::new(),
            read_by: HashMap::new(),
            sender_of: HashMap::new(),
            sublists,
        }
    }

    fn sequential(&self) -> SublistId {
        self.sublists.len() - 1
    }

    fn is_sequential(&self, id: SublistId) -> bool {
        id == self.sequential()
    }

    /// Place transaction `index`, or refuse it.
    ///
    /// `None` means the transaction fits in neither its candidate sublist nor
    /// the sequential one, and the miner drops it from the block. Nothing is
    /// recorded in that case: a refused transaction must not connect anything.
    pub fn add(&mut self, index: usize, tx: &TxFootprint) -> Option<SublistId> {
        let mut candidate = self.candidate_for(tx);

        if !self.sublists[candidate].fits(tx.gas_limit) {
            if self.is_sequential(candidate) {
                return None;
            }
            candidate = self.sequential();
            if !self.sublists[candidate].fits(tx.gas_limit) {
                return None;
            }
        }

        self.place(index, candidate, tx);
        Some(candidate)
    }

    /// Place transaction `index` in the sequential sublist, or refuse it.
    ///
    /// For a transaction that reached REMASC or the Bridge
    /// ([`crate::precompiles::SEQUENTIAL_ONLY_PRECOMPILES`]): it may go
    /// nowhere else, and the block is invalid if it does. Its keys are still
    /// recorded, so later transactions are placed knowing about it.
    pub fn add_to_sequential(&mut self, index: usize, tx: &TxFootprint) -> Option<SublistId> {
        let sequential = self.sequential();
        if !self.sublists[sequential].fits(tx.gas_limit) {
            return None;
        }
        self.place(index, sequential, tx);
        Some(sequential)
    }

    /// The REMASC transaction, which closes the sequential sublist.
    ///
    /// Unconditional: rskj's `addRemascTransaction` does not test the gas
    /// limit, because the block is not a block without it.
    pub fn add_remasc(&mut self, index: usize, gas_used: u64) -> SublistId {
        let sequential = self.sequential();
        self.sublists[sequential].gas_used += gas_used;
        self.sublists[sequential].transactions.push(index);
        sequential
    }

    fn place(&mut self, index: usize, id: SublistId, tx: &TxFootprint) {
        self.sublists[id].gas_used += tx.gas_used;
        self.sublists[id].transactions.push(index);

        for key in &tx.reads {
            self.read_by.entry(key.clone()).or_default().insert(id);
        }

        // Overwritten for the sequential sublist, kept for a parallel one:
        // once a sender is sequential it stays sequential.
        if self.is_sequential(id) {
            self.sender_of.insert(tx.sender, id);
        } else {
            self.sender_of.entry(tx.sender).or_insert(id);
        }

        for key in &tx.writes {
            self.written_by.insert(key.clone(), id);
        }
    }

    /// Which sublist the transaction belongs with, ignoring gas.
    fn candidate_for(&self, tx: &TxFootprint) -> SublistId {
        let sequential = self.sequential();
        let mut candidate = self.sender_of.get(&tx.sender).copied();

        if candidate.is_some_and(|id| self.is_sequential(id)) {
            return sequential;
        }

        // Reads against earlier writes.
        for key in &tx.reads {
            if let Some(&writer) = self.written_by.get(key) {
                if self.is_sequential(writer) {
                    return sequential;
                }
                match candidate {
                    None => candidate = Some(writer),
                    Some(current) if current != writer => return sequential,
                    Some(_) => {}
                }
            }
        }

        for key in &tx.writes {
            // Writes against earlier writes.
            if let Some(&writer) = self.written_by.get(key) {
                if self.is_sequential(writer) {
                    return sequential;
                }
                match candidate {
                    None => candidate = Some(writer),
                    Some(current) if current != writer => return sequential,
                    Some(_) => {}
                }
            }

            // Writes against earlier reads.
            if let Some(readers) = self.read_by.get(key) {
                if readers.len() > 1 {
                    return sequential;
                }
                if let Some(&reader) = readers.iter().next() {
                    match candidate {
                        None => candidate = Some(reader),
                        Some(current) if current != reader => return sequential,
                        Some(_) => {}
                    }
                }
            }
        }

        candidate.unwrap_or_else(|| self.least_used_parallel(tx.gas_limit).unwrap_or(sequential))
    }

    /// The parallel sublist with the least gas used that `gas_limit` fits in.
    fn least_used_parallel(&self, gas_limit: u64) -> Option<SublistId> {
        self.sublists
            .iter()
            .enumerate()
            .take(self.sequential())
            .filter(|(_, s)| s.fits(gas_limit))
            .min_by_key(|(_, s)| s.gas_used)
            .map(|(id, _)| id)
    }

    pub fn gas_used_in(&self, id: SublistId) -> u64 {
        self.sublists[id].gas_used
    }

    pub fn gas_used_in_sequential(&self) -> u64 {
        self.sublists[self.sequential()].gas_used
    }

    /// The transactions of every sublist, parallel ones first and the
    /// sequential one last -- the order they must appear in the block.
    pub fn transactions_in_order(&self) -> Vec<usize> {
        self.sublists.iter().flat_map(|s| s.transactions.iter().copied()).collect()
    }

    /// The header's `txExecutionSublistsEdges`: where each parallel sublist
    /// ends, in the block's own transaction indices.
    ///
    /// Empty when nothing is parallel, which is how rskj spells "this block
    /// is not partitioned" -- `getTransactionsPerSublistInOrder` returns an
    /// empty array rather than one naming empty sublists.
    pub fn edges(&self) -> Vec<i16> {
        let mut edges = Vec::new();
        let mut at = 0i16;
        for sublist in self.sublists.iter().take(self.sequential()) {
            if sublist.transactions.is_empty() {
                continue;
            }
            at += sublist.transactions.len() as i16;
            edges.push(at);
        }
        edges
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Ported from rskj's `ParallelizeTransactionHandlerTest`, which is the
    /// authority on this algorithm. Names are kept recognisably close to the
    /// originals so the two can be read side by side.
    ///
    /// The fixture is rskj's: a block gas limit of 8,160,000 over two
    /// parallel sublists, regtest's minimum sequential gas limit, and
    /// transactions of 16,000 gas from distinct senders.
    const BLOCK_GAS_LIMIT: u64 = 8_160_000;
    /// rskj `Constants.regtest().getMinSequentialSetGasLimit()`.
    const MIN_SEQUENTIAL: u64 = 6_800_000;
    const TX_GAS: u64 = 16_000;

    fn handler() -> SublistPartitioner {
        SublistPartitioner::new(2, BLOCK_GAS_LIMIT, MIN_SEQUENTIAL)
    }

    const SEQUENTIAL: SublistId = 2;

    fn sender(n: u8) -> Address {
        Address::repeat_byte(n)
    }

    fn key(n: u8) -> Vec<u8> {
        vec![1, 2, 3, n]
    }

    /// A transaction of the fixture's ordinary size.
    fn tx(s: u8, reads: &[Vec<u8>], writes: &[Vec<u8>]) -> TxFootprint {
        TxFootprint {
            sender: sender(s),
            gas_limit: TX_GAS,
            gas_used: TX_GAS,
            reads: reads.to_vec(),
            writes: writes.to_vec(),
        }
    }

    /// One gas short of filling a parallel sublist, as rskj's `bigTx` is.
    fn big_tx(s: u8, reads: &[Vec<u8>], writes: &[Vec<u8>]) -> TxFootprint {
        let (parallel, _) = sublist_gas_limits(BLOCK_GAS_LIMIT, 2, MIN_SEQUENTIAL);
        TxFootprint {
            sender: sender(s),
            gas_limit: parallel - 1,
            gas_used: parallel - 1,
            reads: reads.to_vec(),
            writes: writes.to_vec(),
        }
    }

    // -- the gas limits themselves ------------------------------------

    /// The three branches of `BlockUtils.getSublistGasLimit`, none of which
    /// RSKIP-144 contains: it says every sublist gets `L / (N + 1)`.
    #[test]
    fn the_sequential_sublist_keeps_the_remainder() {
        // 8,160,000 / 3 = 2,720,000, which is below regtest's 6,800,000
        // minimum, so the guaranteed-sequential branch applies.
        let (parallel, sequential) = sublist_gas_limits(BLOCK_GAS_LIMIT, 2, MIN_SEQUENTIAL);
        assert_eq!(parallel, (BLOCK_GAS_LIMIT - MIN_SEQUENTIAL) / 2);
        assert_eq!(sequential, BLOCK_GAS_LIMIT - 2 * parallel);
        assert!(sequential >= MIN_SEQUENTIAL);
    }

    #[test]
    fn an_even_division_gives_the_sequential_sublist_no_more_than_the_others() {
        // 30,000,000 / 3 = 10,000,000, at or above the 10,000,000 mainnet
        // minimum, so the plain branch applies and the division is exact.
        let (parallel, sequential) = sublist_gas_limits(30_000_000, 2, 10_000_000);
        assert_eq!((parallel, sequential), (10_000_000, 10_000_000));
    }

    #[test]
    fn a_block_too_small_for_the_minimum_forbids_parallel_sublists() {
        let (parallel, sequential) = sublist_gas_limits(5_000_000, 2, 6_800_000);
        assert_eq!(parallel, 0, "nothing fits in a parallel sublist");
        assert_eq!(sequential, 5_000_000);
    }

    // -- placement ----------------------------------------------------

    #[test]
    fn a_new_handler_has_nothing_and_no_edges() {
        let h = handler();
        assert_eq!(h.transactions_in_order(), Vec::<usize>::new());
        assert_eq!(h.edges(), Vec::<i16>::new());
        assert_eq!(h.gas_used_in(0), 0);
        assert_eq!(h.gas_used_in_sequential(), 0);
    }

    #[test]
    fn the_first_transaction_goes_in_the_first_parallel_sublist() {
        let mut h = handler();
        assert_eq!(h.add(0, &tx(1, &[], &[])), Some(0));
        assert_eq!(h.gas_used_in(0), TX_GAS);
        assert_eq!(h.edges(), vec![1]);
    }

    /// Two reads of the same key do not connect anything: only a write makes
    /// an ordering observable, so they balance across the sublists.
    #[test]
    fn two_transactions_reading_the_same_key_go_in_different_sublists() {
        let mut h = handler();
        assert_eq!(h.add(0, &tx(1, &[key(0)], &[])), Some(0));
        assert_eq!(h.add(1, &tx(2, &[key(0)], &[])), Some(1));
        assert_eq!(h.gas_used_in_sequential(), 0);
        assert_eq!(h.edges(), vec![1, 2]);
    }

    #[test]
    fn two_transactions_writing_the_same_key_go_in_the_same_sublist() {
        let mut h = handler();
        assert_eq!(h.add(0, &tx(1, &[], &[key(0)])), Some(0));
        assert_eq!(h.add(1, &tx(2, &[], &[key(0)])), Some(0));
        assert_eq!(h.gas_used_in(0), 2 * TX_GAS);
        assert_eq!(h.edges(), vec![2]);
    }

    #[test]
    fn two_transactions_writing_different_keys_go_in_different_sublists() {
        let mut h = handler();
        assert_eq!(h.add(0, &tx(1, &[], &[key(0)])), Some(0));
        assert_eq!(h.add(1, &tx(2, &[], &[key(1)])), Some(1));
        assert_eq!(h.edges(), vec![1, 2]);
    }

    /// Write then read, and read then write, both connect.
    #[test]
    fn a_read_of_an_earlier_write_joins_its_sublist() {
        let mut h = handler();
        assert_eq!(h.add(0, &tx(1, &[], &[key(0)])), Some(0));
        assert_eq!(h.add(1, &tx(2, &[key(0)], &[])), Some(0));
        assert_eq!(h.edges(), vec![2]);
    }

    #[test]
    fn a_write_of_an_earlier_read_joins_its_sublist() {
        let mut h = handler();
        assert_eq!(h.add(0, &tx(1, &[key(0)], &[])), Some(0));
        assert_eq!(h.add(1, &tx(2, &[], &[key(0)])), Some(0));
        assert_eq!(h.edges(), vec![2]);
    }

    /// The greedy give-up: a transaction that would join two parallel
    /// sublists is not allowed to merge them, so it goes sequential.
    #[test]
    fn a_transaction_colliding_with_two_sublists_goes_sequential() {
        let mut h = handler();
        assert_eq!(h.add(0, &tx(1, &[], &[key(0)])), Some(0));
        assert_eq!(h.add(1, &tx(2, &[], &[key(1)])), Some(1));
        assert_eq!(h.gas_used_in_sequential(), 0);

        // Reads what sublist 1 wrote and writes what sublist 0 wrote.
        assert_eq!(h.add(2, &tx(3, &[key(1)], &[key(0)])), Some(SEQUENTIAL));
        assert_eq!(h.gas_used_in_sequential(), TX_GAS);
        assert_eq!(h.transactions_in_order(), vec![0, 1, 2]);
        assert_eq!(h.edges(), vec![1, 2]);
    }

    #[test]
    fn a_write_of_a_key_two_sublists_read_goes_sequential() {
        let mut h = handler();
        assert_eq!(h.add(0, &tx(1, &[key(0)], &[])), Some(0));
        assert_eq!(h.add(1, &tx(2, &[key(0)], &[])), Some(1));
        assert_eq!(h.gas_used_in_sequential(), 0);

        assert_eq!(h.add(2, &tx(3, &[], &[key(0)])), Some(SEQUENTIAL));
        assert_eq!(h.gas_used_in_sequential(), TX_GAS);
        assert_eq!(h.edges(), vec![1, 2]);
    }

    #[test]
    fn a_read_of_two_keys_written_by_different_sublists_goes_sequential() {
        let mut h = handler();
        assert_eq!(h.add(0, &tx(1, &[], &[key(0)])), Some(0));
        assert_eq!(h.add(1, &tx(2, &[], &[key(1)])), Some(1));
        assert_eq!(h.add(2, &tx(3, &[key(0), key(1)], &[])), Some(SEQUENTIAL));
    }

    // -- the sender ---------------------------------------------------

    /// Same sender, same sublist: their nonces order them, so they are
    /// connected whatever else they touch.
    #[test]
    fn two_transactions_from_one_sender_share_a_sublist() {
        let mut h = handler();
        assert_eq!(h.add(0, &tx(1, &[], &[key(0)])), Some(0));
        assert_eq!(h.add(1, &tx(1, &[], &[key(1)])), Some(0));
        assert_eq!(h.gas_used_in(0), 2 * TX_GAS);
        assert_eq!(h.edges(), vec![2]);
    }

    /// And once a sender is in the sequential sublist it stays there -- the
    /// asymmetry between `put` and `putIfAbsent` in rskj.
    #[test]
    fn a_sender_that_lands_in_the_sequential_sublist_stays_there() {
        let mut h = handler();
        h.add(0, &tx(1, &[], &[key(0)]));
        h.add(1, &tx(2, &[], &[key(1)]));
        // Sender 3 is forced sequential by colliding with both.
        assert_eq!(h.add(2, &tx(3, &[key(1)], &[key(0)])), Some(SEQUENTIAL));
        // And its next transaction follows it, touching nothing at all.
        assert_eq!(h.add(3, &tx(3, &[], &[])), Some(SEQUENTIAL));
        assert_eq!(h.gas_used_in_sequential(), 2 * TX_GAS);
    }

    #[test]
    fn a_sender_colliding_with_its_own_earlier_sublist_stays_in_it() {
        let mut h = handler();
        assert_eq!(h.add(0, &tx(1, &[], &[key(0)])), Some(0));
        assert_eq!(h.add(1, &tx(1, &[key(0)], &[])), Some(0));
        assert_eq!(h.gas_used_in_sequential(), 0);
    }

    /// Connected to its own sublist by the sender and to another by a key:
    /// two candidates, so sequential.
    #[test]
    fn a_sender_in_one_sublist_colliding_with_another_goes_sequential() {
        let mut h = handler();
        assert_eq!(h.add(0, &tx(1, &[], &[key(0)])), Some(0));
        assert_eq!(h.add(1, &tx(2, &[], &[key(1)])), Some(1));
        // Sender 1 is already in sublist 0; this one also writes sublist 1's key.
        assert_eq!(h.add(2, &tx(1, &[], &[key(1)])), Some(SEQUENTIAL));
    }

    // -- gas ----------------------------------------------------------

    #[test]
    fn a_transaction_that_does_not_fit_its_candidate_falls_to_the_sequential() {
        let mut h = handler();
        assert_eq!(h.add(0, &big_tx(1, &[], &[key(0)])), Some(0));
        // Connected to sublist 0 by the key, but there is one gas left there.
        assert_eq!(h.add(1, &tx(2, &[], &[key(0)])), Some(SEQUENTIAL));
        assert_eq!(h.gas_used_in_sequential(), TX_GAS);
    }

    #[test]
    fn an_independent_transaction_takes_the_emptier_parallel_sublist() {
        let mut h = handler();
        h.add(0, &tx(1, &[], &[key(0)]));
        h.add(1, &tx(1, &[], &[key(1)])); // same sender, so sublist 0 again
        assert_eq!(h.gas_used_in(0), 2 * TX_GAS);
        assert_eq!(h.gas_used_in(1), 0);
        assert_eq!(h.add(2, &tx(2, &[], &[key(2)])), Some(1), "the emptier one");
    }

    #[test]
    fn when_every_parallel_sublist_is_full_an_independent_transaction_goes_sequential() {
        let mut h = handler();
        assert_eq!(h.add(0, &big_tx(1, &[], &[])), Some(0));
        assert_eq!(h.add(1, &big_tx(2, &[], &[])), Some(1));
        assert_eq!(h.add(2, &tx(3, &[], &[])), Some(SEQUENTIAL));
    }

    /// Nothing fits anywhere, so the transaction is refused -- and leaves no
    /// trace, or it would connect transactions it is not even in the block
    /// with.
    #[test]
    fn a_transaction_that_fits_nowhere_is_refused_and_records_nothing() {
        let mut h = handler();
        h.add(0, &big_tx(1, &[], &[]));
        h.add(1, &big_tx(2, &[], &[]));
        let (_, sequential_limit) = sublist_gas_limits(BLOCK_GAS_LIMIT, 2, MIN_SEQUENTIAL);
        let enormous = TxFootprint {
            sender: sender(3),
            gas_limit: sequential_limit + 1,
            gas_used: sequential_limit + 1,
            reads: vec![],
            writes: vec![key(9)],
        };
        assert_eq!(h.add(2, &enormous), None);
        assert_eq!(h.gas_used_in_sequential(), 0);
        assert_eq!(h.transactions_in_order(), vec![0, 1]);

        // Its key was not recorded, so a later writer of it is unaffected.
        assert_eq!(h.add(3, &tx(4, &[], &[key(9)])), Some(SEQUENTIAL));
    }

    // -- the sequential sublist's own gravity -------------------------

    /// Anything connected to the sequential sublist is pulled into it, in
    /// all four directions.
    #[test]
    fn a_collision_with_the_sequential_sublist_is_sequential() {
        for (reads, writes, prior_reads, prior_writes) in [
            (vec![key(0)], vec![], vec![], vec![key(0)]), // read what it wrote
            (vec![], vec![key(0)], vec![], vec![key(0)]), // write what it wrote
            (vec![], vec![key(0)], vec![key(0)], vec![]), // write what it read
        ] {
            let mut h = handler();
            // Force a transaction into the sequential sublist first.
            h.add(0, &tx(1, &[], &[key(7)]));
            h.add(1, &tx(2, &[], &[key(8)]));
            let forced = TxFootprint {
                sender: sender(3),
                gas_limit: TX_GAS,
                gas_used: TX_GAS,
                reads: [vec![key(8)], prior_reads].concat(),
                writes: [vec![key(7)], prior_writes].concat(),
            };
            assert_eq!(h.add(2, &forced), Some(SEQUENTIAL));

            assert_eq!(
                h.add(3, &tx(4, &reads, &writes)),
                Some(SEQUENTIAL),
                "reads {reads:?} writes {writes:?}"
            );
        }
    }

    // -- REMASC and the sequential-only transactions ------------------

    #[test]
    fn the_remasc_transaction_closes_the_sequential_sublist() {
        let mut h = handler();
        h.add(0, &tx(1, &[], &[key(0)]));
        assert_eq!(h.add_remasc(1, 0), SEQUENTIAL);
        assert_eq!(h.transactions_in_order(), vec![0, 1]);
        assert_eq!(h.edges(), vec![1], "the sequential sublist is not an edge");
    }

    /// A Bridge call goes to the sequential sublist and its keys are still
    /// recorded, so later transactions are placed knowing about it.
    #[test]
    fn a_sequential_only_transaction_still_records_its_keys() {
        let mut h = handler();
        assert_eq!(h.add_to_sequential(0, &tx(1, &[], &[key(0)])), Some(SEQUENTIAL));
        assert_eq!(h.add(1, &tx(2, &[key(0)], &[])), Some(SEQUENTIAL));
    }

    /// rskj `writeAfterWriteToSequentialForOutOfGasShouldGoToSequential`,
    /// and sharper than its name: with a block gas limit of 3,000,000
    /// against regtest's 6,800,000 minimum, the parallel sublists get **no
    /// gas at all**, so every transaction is sequential and the block
    /// declares no partition. The branch is reachable from a real
    /// configuration, not only from arithmetic.
    #[test]
    fn a_block_below_the_sequential_minimum_puts_everything_in_sequence() {
        let mut h = SublistPartitioner::new(2, 3_000_000, MIN_SEQUENTIAL);
        let spend = |s: u8, gas: u64, reads: &[Vec<u8>], writes: &[Vec<u8>]| TxFootprint {
            sender: sender(s),
            gas_limit: gas,
            gas_used: gas,
            reads: reads.to_vec(),
            writes: writes.to_vec(),
        };
        assert_eq!(h.add(0, &spend(1, 800_000, &[], &[key(0)])), Some(SEQUENTIAL));
        assert_eq!(h.add(1, &spend(2, 300_000, &[], &[key(0)])), Some(SEQUENTIAL));
        assert_eq!(h.add(2, &spend(3, 100_000, &[key(0)], &[])), Some(SEQUENTIAL));
        assert_eq!(h.edges(), Vec::<i16>::new());
    }

    /// Two transactions placed in the sequential sublist directly, as two
    /// Bridge calls from one sender would be.
    #[test]
    fn two_sequential_only_transactions_from_one_sender_both_land_there() {
        let mut h = handler();
        assert_eq!(h.add_to_sequential(0, &tx(1, &[], &[key(0)])), Some(SEQUENTIAL));
        assert_eq!(h.add_to_sequential(1, &tx(1, &[], &[key(1)])), Some(SEQUENTIAL));
        assert_eq!(h.gas_used_in_sequential(), 2 * TX_GAS);
        assert_eq!(h.edges(), Vec::<i16>::new());
    }

    /// A block with nothing parallel declares no partition at all, rather
    /// than one naming empty sublists.
    #[test]
    fn an_all_sequential_block_has_no_edges() {
        let mut h = handler();
        h.add_to_sequential(0, &tx(1, &[], &[key(0)]));
        h.add_remasc(1, 0);
        assert_eq!(h.edges(), Vec::<i16>::new());
    }
}
