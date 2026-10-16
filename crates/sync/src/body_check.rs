//! Does this body belong to that header?
//!
//! A peer answers a body request with transactions and uncles, and nothing in
//! the message ties them to the header they were asked for. The header commits
//! to both -- `transactions_root` and `ommers_hash` -- so the tie is one hash
//! away, and checking it **at receipt** is the only moment the supplying peer
//! is unambiguous. By the time the block reaches execution it has been through
//! a store and a buffer, and the answer to "who sent this" is gone.
//!
//! Cheap, too: two hashes against a body already in memory, versus discovering
//! the same thing after execution has built a state from it.

use rustock_core::{Header, Transaction};

/// What about this body fails to match the header, if anything.
///
/// `None` means the body is the one the header commits to. The string is for
/// the log and is deliberately coarse -- a peer learns nothing from it that it
/// did not already know, having constructed the mismatch.
pub(crate) fn body_mismatch(
    header: &Header,
    transactions: &[Transaction],
    ommers: &[Header],
) -> Option<&'static str> {
    use rustock_core::ordered_tx_trie_root;

    // The transaction root's encoding changed at the unitrie fork, and this
    // has no hardfork table to consult, so either encoding is accepted: both
    // are genuine, and matching one of them is what proves the transaction
    // set. The question is "are these the right transactions", not "which era
    // is this" -- the executor checks the era-correct one later, against the
    // hardfork table it does have.
    let wanted = header.transactions_root;
    if ordered_tx_trie_root(transactions, true) != wanted
        && ordered_tx_trie_root(transactions, false) != wanted
    {
        return Some("transactions");
    }

    if rustock_execution::processor::compute_ommers_hash(ommers) != header.ommers_hash {
        return Some("uncles");
    }

    None
}

/// The body this header determines, when it determines one.
///
/// A block is `[header, transactions, uncles]`. The uncle list cannot be
/// recovered -- `ommers_hash` is a one-way commitment -- but a block with no
/// uncles carrying only its REMASC transaction has a body that follows from
/// its height alone, and REMASC is a pure function of that height.
///
/// The test is [`body_mismatch`] itself, which is the point: a derived body
/// is accepted on exactly the evidence a peer's answer would have to satisfy,
/// the transactions root and the uncles hash. Derivation therefore cannot
/// admit anything a peer could not have sent; it can only fail to produce
/// something a peer would have.
///
/// Costs one small serialisation and a one-entry trie hash. No disk read, no
/// network, no state lookup -- which matters, because this runs against every
/// header in a sync, and a test more expensive than a request would eat the
/// saving it exists to create.
pub(crate) fn derivable_body(header: &Header) -> Option<Vec<Transaction>> {
    let transactions = vec![Transaction::remasc(header.number)];
    body_mismatch(header, &transactions, &[]).is_none().then_some(transactions)
}

#[cfg(test)]
mod derivation_tests {
    use super::*;
    use rustock_core::ordered_tx_trie_root;

    fn header_committing_to(txs: &[Transaction], rskip126: bool, number: u64) -> Header {
        let mut h = crate::tests::dummy_header(number, Default::default(), Default::default());
        h.transactions_root = ordered_tx_trie_root(txs, rskip126);
        h.ommers_hash = rustock_execution::processor::compute_ommers_hash(&[]);
        h
    }

    /// The ordinary case: an empty block, both trie eras.
    #[test]
    fn an_empty_block_yields_its_body() {
        for rskip126 in [false, true] {
            let number = 4_000_000;
            let txs = vec![Transaction::remasc(number)];
            let header = header_committing_to(&txs, rskip126, number);
            assert_eq!(derivable_body(&header), Some(txs), "rskip126={rskip126}");
        }
    }

    /// A block whose header commits to uncles cannot be derived, however its
    /// transactions look: the uncle headers are not recoverable.
    #[test]
    fn a_block_with_uncles_is_not_derivable() {
        let number = 4_000_000;
        let txs = vec![Transaction::remasc(number)];
        let mut header = header_committing_to(&txs, true, number);
        header.ommers_hash = alloy_primitives::B256::repeat_byte(0x11);
        assert_eq!(derivable_body(&header), None);
    }

    /// A block carrying real transactions is not derivable: its root commits
    /// to a set that is not REMASC alone.
    #[test]
    fn a_block_with_transactions_is_not_derivable() {
        let number = 4_000_000;
        let mut header = header_committing_to(&[Transaction::remasc(number)], true, number);
        header.transactions_root = alloy_primitives::B256::repeat_byte(0x22);
        assert_eq!(derivable_body(&header), None);
    }

    /// The REMASC transaction depends on the height, so a body derived for
    /// the wrong one must not satisfy the header.
    #[test]
    fn the_body_of_another_height_does_not_satisfy_this_header() {
        let number = 4_000_000;
        let txs = vec![Transaction::remasc(number)];
        let header = header_committing_to(&txs, true, number);
        let wrong = vec![Transaction::remasc(number + 1)];
        assert!(body_mismatch(&header, &wrong, &[]).is_some());
    }
}
