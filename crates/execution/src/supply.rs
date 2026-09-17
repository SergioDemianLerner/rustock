//! Native-balance supply conservation.
//!
//! The RSK peg is backed 1:1 by bitcoin: the Bridge holds the entire 21 M
//! supply and a peg-in moves value out of it rather than creating any, so the
//! sum of every account's balance must not grow when a block is applied.
//!
//! This module recomputes that sum over exactly the accounts a block writes and
//! reports the net change `B`:
//!
//! - `B > 0` — native BTC appeared from nowhere. The block is invalid.
//! - `B < 0` — native BTC was destroyed. Legitimate in some cases (REMASC burns
//!   a share of fees; an EVM oddity such as a contract self-destructing to
//!   itself removes its balance), so it is recorded and not fatal.
//! - `B = 0` — conserved, the normal case.
//!
//! **Cost.** One trie read per account the block writes, against the pre-block
//! root whose nodes execution has just walked, so the reads are cache hits. A
//! block touching a dozen accounts pays a dozen lookups on top of the writes it
//! was doing anyway.

use alloy_primitives::{Address, U256};
use revm::state::{AccountStatus, EvmState};
use rustock_trie::{account_key, AccountState, TrieKeySlice, TrieNode, TrieStore};

/// One account's contribution to the net supply change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountDelta {
    pub address: Address,
    pub before: U256,
    pub after: U256,
}

impl AccountDelta {
    pub fn is_increase(&self) -> bool {
        self.after > self.before
    }
    pub fn magnitude(&self) -> U256 {
        if self.after > self.before { self.after - self.before } else { self.before - self.after }
    }
}

/// The result of checking one block.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SupplyReport {
    /// Total of every balance increase.
    pub inflow: U256,
    /// Total of every balance decrease.
    pub outflow: U256,
    /// Accounts whose balance moved, for the log when something is wrong.
    pub deltas: Vec<AccountDelta>,
}

impl SupplyReport {
    /// `B`, the net change in total supply. Positive means BTC was created.
    ///
    /// Returned as a sign plus magnitude because balances are `U256` and the
    /// difference does not fit a signed type of the same width.
    pub fn net(&self) -> (bool, U256) {
        if self.inflow >= self.outflow {
            (true, self.inflow - self.outflow)
        } else {
            (false, self.outflow - self.inflow)
        }
    }

    /// BTC was created from nothing: the block must be rejected.
    pub fn created_supply(&self) -> bool {
        self.inflow > self.outflow
    }

    /// BTC was destroyed. Worth recording, not fatal.
    pub fn destroyed_supply(&self) -> bool {
        self.outflow > self.inflow
    }

    pub fn is_balanced(&self) -> bool {
        self.inflow == self.outflow
    }

    /// The accounts that gained, largest first — what to name in an alert.
    pub fn gainers(&self) -> Vec<&AccountDelta> {
        let mut v: Vec<&AccountDelta> = self.deltas.iter().filter(|d| d.is_increase()).collect();
        v.sort_by(|a, b| b.magnitude().cmp(&a.magnitude()));
        v
    }
}

/// Balance an account holds in `root`, or zero if it is not there.
fn balance_in(root: &TrieNode, store: &dyn TrieStore, addr: &Address) -> U256 {
    let key_bytes = account_key(addr);
    let key = TrieKeySlice::from_key(&key_bytes);
    match root.get(&key, store) {
        Some(data) => AccountState::decode(&data).map(|a| a.balance).unwrap_or(U256::ZERO),
        None => U256::ZERO,
    }
}

/// Account for every balance change a block makes.
///
/// `pre_root` is the state before the block, `state` the changes about to be
/// applied. The set of accounts considered mirrors `apply_state_changes`
/// exactly, including the self-destruct-then-recreated case: an account that is
/// destroyed and not touched again ends at zero, while one that is touched
/// afterwards ends at its new balance (mainnet #3,173,807).
pub fn account_supply_change(
    pre_root: &TrieNode,
    store: &dyn TrieStore,
    state: &EvmState,
) -> SupplyReport {
    let mut report = SupplyReport::default();

    for (addr, account) in state {
        let destroyed = account.status.contains(AccountStatus::SelfDestructed);
        let touched = account.is_touched();
        if !destroyed && !touched {
            // apply_state_changes writes nothing for this account.
            continue;
        }

        let before = balance_in(pre_root, store, addr);
        let after = if destroyed && !touched { U256::ZERO } else { account.info.balance };
        if before == after {
            continue;
        }

        if after > before {
            report.inflow += after - before;
        } else {
            report.outflow += before - after;
        }
        report.deltas.push(AccountDelta { address: *addr, before, after });
    }

    report
}

/// What an observer is told when the supply does not balance.
#[derive(Debug, Clone)]
pub struct SupplyViolation {
    pub block: u64,
    /// True when supply was created (block rejected); false when destroyed.
    pub created: bool,
    /// Size of the imbalance in wei.
    pub amount: U256,
    /// The accounts that moved, largest gain first.
    pub deltas: Vec<AccountDelta>,
}

type Observer = Box<dyn Fn(&SupplyViolation) + Send + Sync>;
static OBSERVER: std::sync::OnceLock<Observer> = std::sync::OnceLock::new();

/// Install the process-wide observer, once.
///
/// The execution crate must not depend on the alerting crate -- alerting
/// already depends on execution -- so the node wires the two together at
/// startup instead. A tool that replays blocks simply installs nothing, which
/// is how the replay harness stays silent and sends no mail.
pub fn set_observer<F>(f: F) -> Result<(), &'static str>
where
    F: Fn(&SupplyViolation) + Send + Sync + 'static,
{
    OBSERVER.set(Box::new(f)).map_err(|_| "supply observer already installed")
}

/// Log an imbalance and hand it to the observer, if one is installed.
///
/// Logging happens either way, so the record exists whether or not the node was
/// built with mail support.
pub fn report(block: u64, report: &SupplyReport) {
    let (positive, amount) = report.net();
    let violation = SupplyViolation {
        block,
        created: positive,
        amount,
        deltas: {
            let mut d = report.deltas.clone();
            d.sort_by(|a, b| b.magnitude().cmp(&a.magnitude()));
            d
        },
    };

    if violation.created {
        tracing::error!(
            target: "rustock::supply",
            block, amount = %amount,
            "SUPPLY CREATED: block #{block} would increase the native supply by {amount} wei; rejecting it"
        );
    } else {
        tracing::warn!(
            target: "rustock::supply",
            block, amount = %amount,
            "supply destroyed: block #{block} burns {amount} wei"
        );
    }
    for d in violation.deltas.iter().take(8) {
        tracing::info!(
            target: "rustock::supply",
            block,
            account = %d.address,
            before = %d.before,
            after = %d.after,
            "  {} {}", if d.is_increase() { "gained" } else { "lost" }, d.magnitude()
        );
    }

    if let Some(obs) = OBSERVER.get() {
        obs(&violation);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use revm::state::{Account, AccountInfo};
    use rustock_trie::MemoryTrieStore;

    fn store_with(balances: &[(Address, u64)]) -> (MemoryTrieStore, TrieNode) {
        let store = MemoryTrieStore::new();
        let mut root = TrieNode::empty();
        for (addr, bal) in balances {
            let acct = AccountState::new(U256::ZERO, U256::from(*bal));
            let key_bytes = account_key(addr);
            root = root.put(&TrieKeySlice::from_key(&key_bytes), &acct.encode(), &store);
        }
        (store, root)
    }

    fn touched(balance: u64) -> Account {
        let mut a = Account::from(AccountInfo { balance: U256::from(balance), ..Default::default() });
        a.mark_touch();
        a
    }

    #[test]
    fn a_plain_transfer_conserves_supply() {
        let (from, to) = (Address::repeat_byte(0xAA), Address::repeat_byte(0xBB));
        let (store, root) = store_with(&[(from, 100), (to, 0)]);

        let mut state = EvmState::default();
        state.insert(from, touched(40));   // -60
        state.insert(to, touched(60));     // +60

        let r = account_supply_change(&root, &store, &state);
        assert!(r.is_balanced(), "{r:?}");
        assert_eq!(r.net(), (true, U256::ZERO));
        assert!(!r.created_supply() && !r.destroyed_supply());
    }

    /// The case this whole module exists for: value appearing with no source.
    #[test]
    fn minting_out_of_nothing_is_detected() {
        let who = Address::repeat_byte(0xCC);
        let (store, root) = store_with(&[(who, 5)]);

        let mut state = EvmState::default();
        state.insert(who, touched(1_000));

        let r = account_supply_change(&root, &store, &state);
        assert!(r.created_supply(), "{r:?}");
        assert_eq!(r.net(), (true, U256::from(995)));
        assert_eq!(r.gainers()[0].address, who);
    }

    /// A burn is reported but is not a reason to reject a block: REMASC burns a
    /// share of fees, and a contract self-destructing to itself removes its own
    /// balance.
    #[test]
    fn a_burn_is_negative_and_not_a_creation() {
        let who = Address::repeat_byte(0xDD);
        let (store, root) = store_with(&[(who, 500)]);

        let mut state = EvmState::default();
        state.insert(who, touched(200));

        let r = account_supply_change(&root, &store, &state);
        assert!(r.destroyed_supply() && !r.created_supply(), "{r:?}");
        assert_eq!(r.net(), (false, U256::from(300)));
    }

    /// A self-destructed account that is not touched again ends at zero, and its
    /// whole balance counts as destroyed -- matching what `apply_state_changes`
    /// writes (it deletes the subtree).
    #[test]
    fn a_self_destructed_account_loses_its_whole_balance() {
        let gone = Address::repeat_byte(0xEE);
        let (store, root) = store_with(&[(gone, 750)]);

        let mut acct = Account::from(AccountInfo { balance: U256::from(750), ..Default::default() });
        acct.mark_selfdestruct();
        let mut state = EvmState::default();
        state.insert(gone, acct);

        let r = account_supply_change(&root, &store, &state);
        assert_eq!(r.net(), (false, U256::from(750)), "{r:?}");
    }

    /// An account present in the state map but never touched is not written by
    /// `apply_state_changes`, so it must not be accounted either -- otherwise a
    /// merely-read account would look like a change.
    #[test]
    fn untouched_accounts_are_ignored() {
        let read_only = Address::repeat_byte(0x11);
        let (store, root) = store_with(&[(read_only, 900)]);

        let mut state = EvmState::default();
        // Loaded during execution, balance unchanged, never touched.
        state.insert(read_only, Account::from(AccountInfo { balance: U256::from(900), ..Default::default() }));

        let r = account_supply_change(&root, &store, &state);
        assert!(r.is_balanced());
        assert!(r.deltas.is_empty(), "{r:?}");
    }

    /// A peg-in in miniature: the Bridge pays a user from its own balance, so
    /// nothing is created however large the movement.
    #[test]
    fn a_bridge_payout_moves_value_rather_than_creating_it() {
        let bridge = crate::precompiles::BRIDGE_ADDR;
        let user = Address::repeat_byte(0x22);
        let (store, root) = store_with(&[(bridge, 21_000_000), (user, 0)]);

        let mut state = EvmState::default();
        state.insert(bridge, touched(20_999_000));
        state.insert(user, touched(1_000));

        let r = account_supply_change(&root, &store, &state);
        assert!(r.is_balanced(), "a peg-in must not change the total: {r:?}");
    }
}
