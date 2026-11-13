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
    /// The block in which the account last changed, or `None` when it has
    /// held its current record since genesis.
    pub block: Option<u64>,
    /// Proofs spent. Reported because the caller is paying for them, and
    /// because it is the number the consensus field reduces to one.
    pub probes: u32,
}

/// The block in which `address` last changed, at or before `hi`.
///
/// Bisects on "does the record at this block already equal the record at
/// `hi`". The predicate is false below the change and true from it onward,
/// so the smallest block satisfying it is the block the change happened in.
pub async fn find_last_change(
    state: &RpcState,
    address: Address,
    hi: u64,
) -> Result<Found, String> {
    let mut probes = 0u32;
    let target = record_at(state, address, hi).await?;
    probes += 1;

    if hi == 0 {
        return Ok(Found { block: None, probes });
    }

    // Genesis already holds the current record, so nothing in range changed
    // it. Distinguished from "changed at block 0", which cannot happen:
    // block 0 is where every account's history begins rather than a block
    // that modified anything.
    let mut lo = 0u64;
    if record_at(state, address, lo).await? == target {
        return Ok(Found { block: None, probes: probes + 1 });
    }
    probes += 1;

    // Invariant: record(lo) != target, record(hi) == target. Each step
    // halves the gap, so the two meet at the boundary between them.
    let mut hi = hi;
    while hi - lo > 1 {
        let mid = lo + (hi - lo) / 2;
        if record_at(state, address, mid).await? == target {
            hi = mid;
        } else {
            lo = mid;
        }
        probes += 1;
    }

    Ok(Found { block: Some(hi), probes })
}

/// `rsk_lastAccountChange(address[, blockNumber])`.
///
/// Answers the question `lastModifiedBlock` will answer directly once
/// RSKIP-TBD activates, so a wallet can be written against it now. The
/// `witnessSource` field says which supplied the answer, so a wallet need
/// never guess whether the netting caveat applies.
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

    let block_param = params.get(1).and_then(|v| v.as_str()).unwrap_or("latest");
    let Some(header) = resolve_header(block_param, &state.store) else {
        return JsonRpcResponse::error(id, INTERNAL_ERROR, "Cannot resolve block");
    };

    match find_last_change(state, address, header.number).await {
        Ok(found) => JsonRpcResponse::success(
            id,
            json!({
                "address": format!("{address:#x}"),
                "lastModifiedBlock": found.block.map(|b| format!("{b:#x}")),
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
