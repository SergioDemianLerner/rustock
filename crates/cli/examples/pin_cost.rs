//! What does pinning the snapshot checkpoint cost the collector?
//!
//! The garbage collector marks from one root: the state at `head - burial`.
//! A snapshot server also has to keep the checkpoint it offers, which sits far
//! deeper and is not reachable from that root (#143). Pinning it is a second
//! seed for the same breadth-first mark, so the cost is the **delta** -- the
//! nodes reachable from the checkpoint that the collection root did not
//! already reach.
//!
//! This measures that delta on a real store. Both the block store and the trie
//! are opened read-only, so it is safe to run beside a live node.
//!
//! Usage: pin_cost <data-dir> [burial] [checkpoint-distance] [checkpoint-rounding]

use alloy_primitives::B256;
use rustock_storage::epoch_store::{EpochConfig, EpochTrieStore};
use rustock_storage::BlockStore;
use rustock_trie::{NodeRef, TrieNode, TrieStore};
use std::collections::HashSet;
use std::time::Instant;

/// Breadth-first from `seeds`, adding to `live`. Returns (visited, missing).
///
/// Deliberately the same shape as the collector's `walk`: a key is queued only
/// when it is new to `live`, which is what makes a second seed cost only what
/// the first did not reach.
fn walk(store: &dyn TrieStore, live: &mut HashSet<B256>, seeds: Vec<B256>) -> (u64, u64) {
    let mut frontier: Vec<B256> = seeds.into_iter().filter(|h| live.insert(*h)).collect();
    let (mut visited, mut missing) = (0u64, 0u64);

    while !frontier.is_empty() {
        let mut next = Vec::new();
        for hash in &frontier {
            let Some(bytes) = store.get(hash.as_slice()) else {
                missing += 1;
                continue;
            };
            visited += 1;
            let node = TrieNode::from_message(&bytes, store);
            if node.has_long_value() {
                if let Some(vh) = node.value_hash {
                    live.insert(vh);
                }
            }
            for child in [&node.left, &node.right] {
                if let NodeRef::Hash(h) = child {
                    if live.insert(*h) {
                        next.push(*h);
                    }
                }
            }
        }
        frontier = next;
    }
    (visited, missing)
}

fn root_at(store: &BlockStore, number: u64) -> Option<(u64, B256)> {
    let hash = store.canonical_hash(number).ok()??;
    let header = store.header(hash).ok()??;
    Some((number, header.state_root))
}

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let dir = args.next().unwrap_or_else(|| "/var/lib/rustock".into());
    let burial: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(4_000);
    let distance: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(10_000);
    let rounding: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(5_000);

    let store = BlockStore::open_read_only(&dir)?;
    let trie = EpochTrieStore::open_read_only(
        std::path::Path::new(&dir).join("trie-epochs"),
        EpochConfig::default(),
    )?;

    let head_hash = store.head()?.ok_or_else(|| anyhow::anyhow!("no head"))?;
    let head = store.header(head_hash)?.ok_or_else(|| anyhow::anyhow!("no head header"))?;
    println!("head #{}", head.number);

    let collection = head
        .number
        .checked_sub(burial)
        .and_then(|n| root_at(&store, n))
        .ok_or_else(|| anyhow::anyhow!("no block buried {burial} deep"))?;

    let checkpoint_number = {
        let rounded = head.number - (head.number % rounding.max(1));
        rounded.saturating_sub(distance)
    };
    let pins: Vec<(u64, B256)> = [checkpoint_number, checkpoint_number.saturating_sub(rounding)]
        .into_iter()
        .filter(|n| *n > 0)
        .filter_map(|n| root_at(&store, n))
        .collect();

    println!("collection root #{} {:?}", collection.0, collection.1);
    for (n, r) in &pins {
        println!("pin             #{n} {r:?}");
    }

    let mut live = HashSet::new();

    let t = Instant::now();
    let (visited, missing) = walk(&trie, &mut live, vec![collection.1]);
    println!(
        "\ncollection root: {visited} nodes in {:.1}s{}",
        t.elapsed().as_secs_f64(),
        if missing > 0 { format!(" -- {missing} MISSING") } else { String::new() }
    );
    let base = live.len();

    for (number, root) in &pins {
        let t = Instant::now();
        let before = live.len();
        let (_, missing) = walk(&trie, &mut live, vec![*root]);
        let added = live.len() - before;
        println!(
            "pin #{number}: +{added} entries ({:.2}% of {base}) in {:.1}s{}",
            100.0 * added as f64 / base as f64,
            t.elapsed().as_secs_f64(),
            if missing > 0 {
                format!(" -- {missing} MISSING, this checkpoint is already unservable")
            } else {
                String::new()
            }
        );
    }

    println!(
        "\nlive set {} -> {} ({:+.2}%)",
        base,
        live.len(),
        100.0 * (live.len() - base) as f64 / base as f64
    );
    Ok(())
}
