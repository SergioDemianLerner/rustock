//! Finding when an account last changed, without a consensus field for it.
//!
//! RSKIP-TBD puts `lastModifiedBlock` in the account record, where it costs
//! one proof to read. Until that activates there is no such field, and this
//! recovers the same number from state the chain already commits to, by
//! bisecting the account's own trie leaf across historical blocks.
//!
//! The trie is content-addressed, so an account's leaf changes exactly when
//! its record changes. "Did A change between `lo` and `hi`" is therefore
//! answered by comparing two verified proofs, and the block it changed in is
//! found by halving the interval — about twenty-four probes on a chain of
//! nine million blocks, against one for the consensus field.
//!
//! # What this is good for, and what it is not
//!
//! Every probe is an ordinary proof checked against a header the node
//! validated itself, so the answer is **trustless**. That is the whole
//! argument for doing it this way rather than keeping a side table of block
//! numbers: a side table sits outside every hash, so a server that drops one
//! entry erases a payment from a wallet's history and nothing detects it.
//!
//! Three limits, none of which a caller should have to discover:
//!
//! - **Netting is invisible.** Comparing leaf hashes *is* comparing
//!   `(nonce, balance, stateFlags)` by another route, so an account credited
//!   and debited back to where it started looks untouched. Content
//!   addressing cannot record that something was touched and came back; only
//!   the consensus field can.
//! - **Bisection assumes the value does not repeat** inside the interval. It
//!   holds for any EOA — one that sends has a strictly rising nonce, one
//!   that only receives a strictly rising balance — and can fail for a
//!   contract, whose balance may return to an earlier value with no nonce
//!   change. Then the search may land on an earlier transition into the same
//!   value rather than the most recent one.
//! - **The peer must be archival.** Bisection asks for state at arbitrary
//!   heights. A pruned or epoch node cannot serve it below its window.

use crate::server::RpcState;
use crate::state::resolve_header;
use crate::types::*;
use alloy_primitives::Address;
use rustock_trie::proof::{verify_path, Outcome};
use rustock_trie::{account_key, TrieKeySlice, TrieNode};
use serde_json::{json, Value};

/// An account's record as of one block: the raw bytes, or `None` when the
/// account provably is not there.
///
/// Compared rather than decoded. Two records are the same state exactly when
/// their bytes match, which is the property the trie's hashing already
/// relies on, and decoding first would only add a way to be wrong.
type Record = Option<Vec<u8>>;

/// Read `address`'s record at `block`, from the local trie if this node has
/// one and from a verified proof otherwise.
///
/// A full node answers from its own state because it can: the result is
/// identical and costs no round trip. An SPV node asks a peer and checks the
/// answer. Neither path returns anything it has not established.
async fn record_at(state: &RpcState, address: Address, block: u64) -> Result<Record, String> {
    let header = resolve_header(&format!("{block:#x}"), &state.store)
        .ok_or_else(|| format!("no block #{block} on this chain"))?;

    if let Some(trie) = &state.trie_store {
        let root_bytes = trie
            .get(header.state_root.as_slice())
            .ok_or_else(|| format!("this node does not hold the state for #{block}"))?;
        let root = TrieNode::from_message(&root_bytes, &**trie);
        return Ok(root.get(&TrieKeySlice::from_key(&account_key(&address)), &**trie));
    }

    let fetcher = state
        .proof_fetcher
        .as_ref()
        .ok_or("this node holds no state and has no peer to ask")?;
    let (account_proof, _) = fetcher.fetch_proof(block, address, Vec::new()).await?;

    match verify_path(&account_proof, &account_key(&address), header.state_root) {
        Ok(Outcome::Value(bytes)) => Ok(Some(bytes)),
        Ok(Outcome::Absent) => Ok(None),
        Err(e) => Err(format!("the peer's proof for #{block} does not check out: {e}")),
    }
}

/// What the search established.
pub struct Found {
    /// The block in which the account last changed, within the range
    /// searched. `None` means it already held this record at the bottom of
    /// the range.
    pub block: Option<u64>,
    /// Whether `block` is the whole answer.
    ///
    /// `false` means the account already held its current record at the
    /// bottom of the range, so the change that produced it is **older than
    /// the range searched** — not that there was none. A caller that reads a
    /// null `block` without reading this would turn "I did not look far
    /// enough" into "this never happened", which is the one mistake that
    /// makes a wallet silently drop a payment from its history.
    ///
    /// `true` with a `None` block means the range began at genesis and the
    /// account has held this record throughout, which genuinely is "never
    /// changed".
    pub complete: bool,
    pub from: u64,
    pub to: u64,
    /// Proofs spent. Reported because the caller is paying for them, and
    /// because it is the number the consensus field reduces to one.
    pub probes: u32,
}

/// The lowest block this node can still answer about.
///
/// A pruning or epoch store collects old state, and a probe below that floor
/// does not come back slow — it fails. Searching from zero on such a node
/// therefore fails on its very first probe, which is both useless and
/// confusing, so the floor is the default bottom of the range.
///
/// `None` on a node with no local trie: an SPV node cannot know its peer's
/// floor, and guessing one would only move the error.
fn retention_floor(state: &RpcState) -> Option<u64> {
    state.trie_store.as_ref().and_then(|t| t.collected_below())
}

/// The block in which `address` last changed, within `lo..=hi`.
///
/// Bisects on "does the record at this block already equal the record at
/// `hi`". The predicate is false below the change and true from it onward,
/// so the smallest block satisfying it is the block the change happened in.
///
/// Bounding the range matters for more than cost. The search is only as good
/// as the state its node can still reach, and a probe below that is an error
/// rather than a slow answer — so a caller that cannot say where to stop
/// gets a failure that looks like the method is broken.
pub async fn find_last_change(
    state: &RpcState,
    address: Address,
    lo: u64,
    hi: u64,
) -> Result<Found, String> {
    if lo > hi {
        return Err(format!("empty range: from #{lo} is above to #{hi}"));
    }

    let mut probes = 0u32;
    let target = record_at(state, address, hi).await?;
    probes += 1;

    if lo == hi {
        return Ok(Found { block: None, complete: lo == 0, from: lo, to: hi, probes });
    }

    // The bottom of the range already holds this record, so whatever
    // produced it happened before the range. Only when the range starts at
    // genesis does that mean "never changed"; otherwise the caller has to
    // look further back, and `complete` is how it is told so.
    let mut low = lo;
    if record_at(state, address, low).await? == target {
        return Ok(Found {
            block: None,
            complete: lo == 0,
            from: lo,
            to: hi,
            probes: probes + 1,
        });
    }
    probes += 1;

    // Invariant: record(low) != target, record(high) == target. Each step
    // halves the gap, so the two meet at the boundary between them.
    let mut high = hi;
    while high - low > 1 {
        let mid = low + (high - low) / 2;
        if record_at(state, address, mid).await? == target {
            high = mid;
        } else {
            low = mid;
        }
        probes += 1;
    }

    Ok(Found { block: Some(high), complete: true, from: lo, to: hi, probes })
}

/// `rsk_lastAccountChange(address[, toBlock[, fromBlock]])`.
///
/// Answers the question `lastModifiedBlock` will answer directly once
/// RSKIP-TBD activates, so a wallet can be written against it now.
///
/// `fromBlock` bounds the search. It defaults to the lowest block this node
/// can still reach rather than to genesis, because on a pruning or epoch
/// node a probe below that floor fails outright — searching from zero would
/// fail on the first probe and look like a broken method rather than a node
/// that does not keep the state being asked about.
pub async fn rsk_last_account_change(
    id: Value,
    params: &Value,
    state: &RpcState,
) -> JsonRpcResponse {
    let Some(address) = params
        .get(0)
        .and_then(|v| v.as_str())
        .and_then(|s| s.parse::<Address>().ok())
    else {
        return JsonRpcResponse::error(id, INVALID_PARAMS, "Missing or invalid address");
    };

    let to_param = params.get(1).and_then(|v| v.as_str()).unwrap_or("latest");
    let Some(to_header) = resolve_header(to_param, &state.store) else {
        return JsonRpcResponse::error(id, INTERNAL_ERROR, "Cannot resolve the upper block");
    };

    let from = match params.get(2).and_then(|v| v.as_str()) {
        Some(s) => match resolve_header(s, &state.store) {
            Some(h) => h.number,
            None => {
                return JsonRpcResponse::error(id, INVALID_PARAMS, "Cannot resolve the lower block")
            }
        },
        None => retention_floor(state).unwrap_or(0),
    };

    if from > to_header.number {
        return JsonRpcResponse::error(
            id,
            INVALID_PARAMS,
            format!("empty range: from #{from} is above to #{}", to_header.number),
        );
    }

    match find_last_change(state, address, from, to_header.number).await {
        Ok(found) => JsonRpcResponse::success(
            id,
            json!({
                "address": format!("{address:#x}"),
                "lastModifiedBlock": found.block.map(|b| format!("{b:#x}")),
                // False means the change is OLDER than the range searched,
                // not that there was none. A caller reading a null block
                // without this would turn "I did not look far enough" into
                // "this never happened".
                "complete": found.complete,
                "searchedFrom": format!("{:#x}", found.from),
                "searchedTo": format!("{:#x}", found.to),
                "probes": found.probes,
                // Named rather than implied. A wallet showing history to a
                // user needs to know whether movements that cancelled out
                // could be missing from it, and that depends entirely on
                // which of the two sources answered.
                "witnessSource": "searched",
            }),
        ),
        Err(e) => JsonRpcResponse::error(id, INTERNAL_ERROR, e),
    }
}
