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
