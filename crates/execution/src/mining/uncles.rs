//! Uncle selection.
//!
//! An uncle is a block that lost: mined on the same parent as one of the
//! recent ancestors, but not the one the chain kept. Including it pays both
//! its miner and the includer, so REMASC rewards depend on this -- and
//! `uncle_count` feeds both the difficulty calculation and the uncle byte of
//! the fork-detection data. A wrong uncle list is a consensus fault rather
//! than a missed reward, which is why every case this cannot resolve returns
//! no uncles instead of a guess.
//!
//! Ported from rskj `co.rsk.core.bc.FamilyUtils`. The candidates are the
//! *family* of the block being mined -- its recent ancestors and their
//! siblings -- less the ancestors themselves and less any uncle an ancestor
//! already included. Finding the siblings needs every block at a height,
//! canonical or not; that is `BlockStore::hashes_at_height`, and rustock had
//! no index able to answer it until one was added for this.

use alloy_primitives::B256;
use rustock_core::Header;
use rustock_storage::BlockStore;
use std::collections::HashSet;
use tracing::warn;

/// How far back an uncle may be. rskj `Constants.getUncleGenerationLimit`.
pub const UNCLE_GENERATION_LIMIT: u64 = 7;
/// How many uncles a block may carry. rskj `Constants.getUncleListLimit`.
pub const UNCLE_LIST_LIMIT: usize = 10;

/// Headers to include as uncles in a block mined on `parent_hash`.
///
/// Ordered by height and then by hash, so two nodes building the same block
/// agree, and truncated to [`UNCLE_LIST_LIMIT`] -- a longer list is rejected
/// outright rather than trimmed by the validator.
pub fn select_uncles(store: &BlockStore, block_number: u64, parent_hash: B256) -> Vec<Header> {
    let ancestors = ancestor_headers(store, block_number, parent_hash);
    let ancestor_hashes: HashSet<B256> = ancestors.iter().map(|(hash, _)| *hash).collect();

    let Some(used) = used_uncles(store, &ancestors) else {
        return Vec::new();
    };

    let mut candidates: Vec<(u64, B256, Header)> = siblings_of_ancestors(store, &ancestors)
        .into_iter()
        .filter(|(hash, _)| !ancestor_hashes.contains(hash) && !used.contains(hash))
        .map(|(hash, header)| (header.number, hash, header))
        .collect();

    // Deterministic: the index hands hashes back in whatever order they sit in
    // the column family, and two nodes mining the same block must agree.
    candidates.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
    candidates.truncate(UNCLE_LIST_LIMIT);
    candidates.into_iter().map(|(_, _, header)| header).collect()
}

/// The ancestors of the block being mined, oldest first, back as far as the
/// generation limit allows. rskj `FamilyUtils.getAncestors`.
fn ancestor_headers(
    store: &BlockStore,
    block_number: u64,
    parent_hash: B256,
) -> Vec<(B256, Header)> {
    let floor = block_number.saturating_sub(UNCLE_GENERATION_LIMIT);
    let mut chain = Vec::new();
    let mut cursor = parent_hash;

    loop {
        let Ok(Some(header)) = store.header(cursor) else {
            break;
        };
        if header.number < floor {
            break;
        }
        let parent = header.parent_hash;
        let is_genesis = header.number == 0;
        chain.push((cursor, header));
        if is_genesis {
            break;
        }
        cursor = parent;
    }

    chain.reverse();
    chain
}

/// Uncles the ancestors already included, which may not be included twice.
/// rskj `FamilyUtils.getUsedUncles`.
///
/// `None` means the question could not be answered, not that the answer is
/// "none": an ancestor's header says how many uncles it carried, so a missing
/// body is detectable, and offering an uncle that an ancestor already used
/// produces a block every peer rejects. Seven bodies at most, all recent and
/// canonical, so on a node that executed them this does not happen.
fn used_uncles(store: &BlockStore, ancestors: &[(B256, Header)]) -> Option<HashSet<B256>> {
    let mut used = HashSet::new();
    for (hash, header) in ancestors {
        match store.body(*hash) {
            Ok(Some((_, ommers))) => used.extend(ommers.iter().map(|o| o.hash())),
            _ if header.uncle_count > 0 => {
                warn!(
                    target: "rustock::mining",
                    "Ancestor #{} ({hash:?}) carried {} uncles but its body is unreadable; \
                     mining without uncles rather than risk reusing one",
                    header.number, header.uncle_count
                );
                return None;
            }
            _ => {}
        }
    }
    Some(used)
}

/// Blocks that share a parent with one of the ancestors but are not that
/// ancestor: the siblings that lost. rskj `FamilyUtils.getFamily`, less the
/// ancestors it also returns.
fn siblings_of_ancestors(store: &BlockStore, ancestors: &[(B256, Header)]) -> Vec<(B256, Header)> {
    let mut out: Vec<(B256, Header)> = Vec::new();
    let mut seen: HashSet<B256> = HashSet::new();

    for window in ancestors.windows(2) {
        let (ancestor_parent_hash, _) = window[0];
        let (ancestor_hash, ancestor) = &window[1];

        let hashes = match store.hashes_at_height(ancestor.number) {
            Ok(hashes) => hashes,
            Err(e) => {
                warn!(
                    target: "rustock::mining",
                    "Height index unreadable at #{}: {e}", ancestor.number
                );
                continue;
            }
        };

        for hash in hashes {
            if hash == *ancestor_hash || !seen.insert(hash) {
                continue;
            }
            let Ok(Some(header)) = store.header(hash) else {
                continue;
            };
            // A sibling, not merely a block at the same height: it has to hang
            // off the same parent, or it belongs to a fork that diverged
            // earlier and is not this block's family at all.
            if header.parent_hash != ancestor_parent_hash {
                continue;
            }
            out.push((hash, header));
        }
    }

    out
}
