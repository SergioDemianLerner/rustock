//! The account modification witness (RSKIP draft, unnumbered).
//!
//! Two fields on the account record let a client that does not execute
//! blocks detect native rBTC movement, which the logs bloom cannot show
//! because a value transfer emits no log:
//!
//! - `lastModifiedBlock` — the block in which the account's `(nonce,
//!   balance, stateFlags)` last changed;
//! - `intraBlockCount` — how many *further* changes happened in that block,
//!   beyond the first.
//!
//! ## Why counting happens per transaction
//!
//! The draft says "every committed modification". The place the account
//! record is actually written -- [`crate::state::apply_state_changes`] --
//! sees one merged `EvmState` for the whole block, so a credit and a debit
//! that cancel inside the block are indistinguishable there from nothing
//! having happened. Counting at that layer would reproduce, inside a block,
//! exactly the netting blindness the witness exists to remove across blocks.
//!
//! So the count is taken per **transaction**, on the journal as it stands
//! immediately after each one, against revm's per-transaction
//! `original_info` baseline -- the same vantage point
//! [`crate::supply::transaction_supply_change`] already uses, and no
//! database reads. A transaction that leaves the record exactly as it found
//! it does not count, whatever it did in between.
//!
//! The residual gap is a single transaction that credits and debits the same
//! account equally within itself: that counts once, and shows no net change.
//! Narrowing it further would mean hooking revm's journal per frame, and
//! itemising such a movement needs traces regardless.

use alloy_primitives::Address;
use revm::state::{AccountStatus, EvmState};
use std::cell::RefCell;
use std::collections::HashMap;

/// How many transactions changed each account's subject record in a block.
pub type ModificationCounts = HashMap<Address, u64>;

/// What [`crate::state::apply_state_changes`] needs to write the witness.
pub struct WitnessContext<'a> {
    /// The block being committed, which is what `lastModifiedBlock` becomes
    /// for every account this block changed.
    pub block: u64,
    pub counts: &'a ModificationCounts,
}

/// Add one transaction's modifications to the block's running counts.
///
/// `transaction_id` selects the accounts this transaction actually touched;
/// revm stamps it on each account as it journals.
pub fn count_transaction_modifications(
    state: &EvmState,
    transaction_id: usize,
    counts: &mut ModificationCounts,
) {
    for (addr, account) in state {
        if account.transaction_id != transaction_id {
            continue; // untouched by this transaction
        }
        if account.status.contains(AccountStatus::SelfDestructed) {
            // The whole subtree goes away, record and witness together.
            // There is nothing left to witness, and a later transaction in
            // the same block that re-creates the address starts the count
            // again -- which is correct, because it is a new account.
            continue;
        }
        // The subject record, excluding the witness fields themselves: a
        // witness that counted its own updates would make every
        // modification look like two. `state_flags` is not in revm's model
        // and is written as zero by `apply_state_changes`, so nonce and
        // balance are the whole of it here.
        let before = &account.original_info;
        let after = &account.info;
        if before.nonce != after.nonce || before.balance != after.balance {
            *counts.entry(*addr).or_insert(0) += 1;
        }
    }
}

/// Fold one sublist's counts into the block's.
///
/// Under RSKIP-144 the parallel sublists have disjoint write sets -- the
/// collision rule invalidates a block where two of them write the same cell
/// -- so for any account at most one parallel sublist contributes. The
/// sequential sublist may genuinely add to an account a parallel one
/// changed, and adding is what that should do.
pub fn merge_counts(into: &mut ModificationCounts, from: ModificationCounts) {
    for (addr, n) in from {
        *into.entry(addr).or_insert(0) += n;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::U256;
    use revm::state::{Account, AccountInfo};

    fn account(tx: usize, before: u64, after: u64) -> Account {
        let mut a = Account {
            info: AccountInfo { balance: U256::from(after), ..Default::default() },
            ..Default::default()
        };
        a.original_info = AccountInfo { balance: U256::from(before), ..Default::default() }.into();
        a.transaction_id = tx;
        a
    }

    #[test]
    fn a_changed_balance_counts_once_per_transaction() {
        let addr = Address::repeat_byte(1);
        let mut counts = ModificationCounts::new();

        let mut state = EvmState::default();
        state.insert(addr, account(0, 0, 5));
        count_transaction_modifications(&state, 0, &mut counts);
        assert_eq!(counts.get(&addr), Some(&1));

        // A second transaction moving it back must count again -- this is
        // the netting case the whole field exists for.
        let mut state = EvmState::default();
        state.insert(addr, account(1, 5, 0));
        count_transaction_modifications(&state, 1, &mut counts);
        assert_eq!(counts.get(&addr), Some(&2));
    }

    #[test]
    fn an_unchanged_record_does_not_count() {
        let addr = Address::repeat_byte(2);
        let mut counts = ModificationCounts::new();
        let mut state = EvmState::default();
        state.insert(addr, account(0, 7, 7));
        count_transaction_modifications(&state, 0, &mut counts);
        assert!(counts.is_empty(), "a touch that changes nothing is not a modification");
    }

    #[test]
    fn another_transactions_accounts_are_ignored() {
        let addr = Address::repeat_byte(3);
        let mut counts = ModificationCounts::new();
        let mut state = EvmState::default();
        state.insert(addr, account(4, 0, 9));
        count_transaction_modifications(&state, 0, &mut counts);
        assert!(counts.is_empty());
    }

    #[test]
    fn merging_sublists_adds() {
        let addr = Address::repeat_byte(5);
        let mut a: ModificationCounts = [(addr, 2)].into_iter().collect();
        let b: ModificationCounts = [(addr, 3)].into_iter().collect();
        merge_counts(&mut a, b);
        assert_eq!(a.get(&addr), Some(&5));
    }
}

// ---------------------------------------------------------------------------
// Collection during execution
// ---------------------------------------------------------------------------
//
// The three places a transaction finishes sit several frames inside the
// block executor, beside the `collision` and `silent` recorders, which solve
// the same plumbing problem the same way. A thread-local keeps the counting
// out of signatures that exist for consensus reasons, and gives the parallel
// sublists -- each on its own thread -- somewhere to accumulate before the
// coordinator folds them together with [`absorb`].

thread_local! {
    /// `None` means the rule is not active for the block being executed,
    /// which is the normal case until it activates.
    static COUNTS: RefCell<Option<ModificationCounts>> = const { RefCell::new(None) };
}

/// Begin counting for a block. Idempotent.
pub fn start() {
    COUNTS.with(|c| *c.borrow_mut() = Some(ModificationCounts::new()));
}

/// Stop counting and take what was collected.
pub fn finish() -> ModificationCounts {
    COUNTS.with(|c| c.borrow_mut().take()).unwrap_or_default()
}

/// Take what has been counted so far, leaving counting on.
///
/// A split block commits in stages -- maintenance, then each parallel
/// sublist, then the sequential one -- and each stage must be applied with
/// the counts belonging to it. Draining between stages is what keeps a
/// later stage from re-applying an earlier one's count.
pub fn drain() -> ModificationCounts {
    COUNTS.with(|c| match c.borrow_mut().as_mut() {
        Some(v) => std::mem::take(v),
        None => ModificationCounts::new(),
    })
}

/// Whether this thread is counting. False makes [`note_transaction`] a no-op,
/// so the pre-activation path pays nothing.
pub fn is_counting() -> bool {
    COUNTS.with(|c| c.borrow().is_some())
}

/// Fold counts collected on another thread into this one's.
pub fn absorb(counts: ModificationCounts) {
    if counts.is_empty() {
        return;
    }
    COUNTS.with(|c| {
        if let Some(into) = c.borrow_mut().as_mut() {
            merge_counts(into, counts);
        }
    });
}

/// Record one transaction's modifications. A no-op unless [`start`] was
/// called, so this is safe to call unconditionally.
pub fn note_transaction(state: &EvmState, transaction_id: usize) {
    COUNTS.with(|c| {
        if let Some(counts) = c.borrow_mut().as_mut() {
            count_transaction_modifications(state, transaction_id, counts);
        }
    });
}
