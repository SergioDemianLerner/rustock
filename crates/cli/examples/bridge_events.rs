//! Query the Bridge event index: every occurrence of one event, in block order.
//!
//! The index is keyed `topic0 || block || tx_index || log_index`, so this is a
//! prefix scan rather than a walk over receipts. Finding every peg-in in the
//! chain takes milliseconds; the equivalent `eth_getLogs` sweep took long
//! enough to be killed for memory.
//!
//! Usage:
//!   bridge_events <data-dir> <event-signature|0xtopic> [from] [to]
//!   bridge_events <data-dir> --list            summarise every event type
//!
//! Examples:
//!   bridge_events /var/lib/rustock 'pegin_btc(address,bytes32,bytes,int256)'
//!   bridge_events /var/lib/rustock 0xdc5eba8b... 9000000 9230000

use alloy_primitives::B256;
use rustock_storage::BlockStore;


/// The Bridge events worth naming, so output reads as words not hashes.
const KNOWN: &[&str] = &[
    // Taken from crates/execution/src/bridge/events.rs, which is authoritative
    // for what rustock emits. Guessing these from memory produced signatures
    // that hashed to topics present in no block -- silently reporting zero
    // peg-ins across the whole chain.
    "lock_btc(address,bytes32,string,int256)",
    "pegin_btc(address,bytes32,int256,int256)",
    "rejected_pegin(bytes32,int256)",
    "unrefundable_pegin(bytes32,int256)",
    "release_request_received(address,bytes,uint256)",
    "release_request_received(address,string,uint256)",
    "release_request_rejected(address,uint256,int256)",
    "release_requested(bytes32,bytes32,uint256)",
    "release_btc(bytes32,bytes)",
    "pegout_confirmed(bytes32,uint256)",
    "pegout_transaction_created(bytes32,bytes)",
    "batch_pegout_created(bytes32,bytes)",
    "add_signature(bytes32,address,bytes)",
    "update_collections(address)",
    "commit_federation(bytes,string,bytes,string,int256)",
    "commit_federation_failed(bytes,int256)",
];

fn topic_of(sig: &str) -> B256 {
    B256::from(alloy_primitives::keccak256(sig.as_bytes()).0)
}

/// Events the early Bridge emitted with a LITERAL ASCII topic, right-aligned
/// and zero-padded to 32 bytes, rather than the keccak hash of a signature.
///
/// They run from the genesis era to #2,392,692, where the modern
/// keccak-hashed topics take over at #2,392,704. Between them they account for
/// 279,806 events -- a fifth of the index -- so a reader that only knows
/// signature hashes silently omits the entire early history of the peg.
const LEGACY_ASCII_TOPICS: &[&str] = &[
    "update_collections_topic",
    "add_signature_topic",
    "release_btc_topic",
    "commit_federation_topic",
    // Listed for completeness; these appear in rskj's early Bridge alongside
    // the four above and cost nothing to recognise if they ever show up.
    "lock_btc_topic",
    "release_request_received_topic",
    "release_requested_topic",
];

/// The topic an ASCII-named legacy event uses: the name right-aligned in 32
/// bytes, zero-padded on the left.
fn legacy_topic(name: &str) -> B256 {
    let mut out = [0u8; 32];
    let b = name.as_bytes();
    out[32 - b.len()..].copy_from_slice(b);
    B256::from(out)
}

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let data_dir = args.next().expect("usage: bridge_events <data-dir> <event|--list> [from] [to]");
    let what = args.next().expect("usage: bridge_events <data-dir> <event|--list> [from] [to]");
    let from: u64 = args.next().map(|s| s.parse()).transpose()?.unwrap_or(0);
    let to: u64 = args.next().map(|s| s.parse()).transpose()?.unwrap_or(u64::MAX);

    let store = BlockStore::open(&data_dir)?;

    if what == "--all" {
        // Enumerate what the index actually holds. The index stores every
        // Bridge log whatever its topic, so this needs no list of signatures
        // and cannot miss an event nobody thought to name.
        let mut names: std::collections::HashMap<B256, String> =
            KNOWN.iter().map(|s| (topic_of(s), s.to_string())).collect();
        for n in LEGACY_ASCII_TOPICS {
            names.insert(legacy_topic(n), format!("{n}  [ascii topic, pre-#2392704]"));
        }
        let topics = store.bridge_event_topics()?;
        let total: u64 = topics.iter().map(|t| t.1).sum();
        println!("{} distinct event types, {total} events total", topics.len());
        let mut rows = topics;
        rows.sort_by(|a, b| b.1.cmp(&a.1));
        for (topic, count, first, last) in rows {
            match names.get(&topic) {
                Some(sig) => println!("  {:<48} {:>9}  #{first}..#{last}", sig, count),
                None => println!("  {:<48} {:>9}  #{first}..#{last}  <UNRECOGNISED>", topic, count),
            }
        }
        return Ok(());
    }

    if what == "--list" {
        println!("{:<34} {:>10}  {}", "event", "count", "first..last");
        for sig in KNOWN {
            let hits = store.scan_bridge_events(topic_of(sig), 0, u64::MAX)?;
            if hits.is_empty() {
                println!("{:<34} {:>10}  -", sig.split('(').next().unwrap(), 0);
            } else {
                println!(
                    "{:<34} {:>10}  #{}..#{}",
                    sig.split('(').next().unwrap(),
                    hits.len(),
                    hits.first().unwrap().0,
                    hits.last().unwrap().0
                );
            }
        }
        return Ok(());
    }

    let topic = if let Some(hex) = what.strip_prefix("0x") {
        B256::from_slice(&alloy_primitives::hex::decode(hex)?)
    } else if what.ends_with("_topic") {
        legacy_topic(&what)   // an early ASCII-topic event, named directly
    } else {
        topic_of(&what)
    };

    let hits = store.scan_bridge_events(topic, from, to)?;
    println!("topic {topic}  ->  {} occurrences", hits.len());
    for (block, tx_index, log_index, tx_hash, topics, data) in &hits {
        println!(
            "  #{block:<9} tx{tx_index} log{log_index}  {}  topics={} data={}B",
            tx_hash,
            topics.len(),
            data.len()
        );
    }
    Ok(())
}
