//! How fast can this node verify headers under the full consensus rules?
//!
//! A snapshot sync trusts nothing about the header chain: every header from
//! the checkpoint down to a block already in the canonical index is checked
//! with `HeaderVerifier::default_rsk`, merged-mining proof of work included.
//! For a node starting from genesis that is the whole chain, so this rate --
//! not the state download -- is what decides how long a fast sync takes.
//!
//! Reads headers from a real store, read-only, walking back by parent hash so
//! the parent rules run exactly as they do in the walk.
//!
//! Usage: header_verify_bench <data-dir> [count] [from-block]

use rustock_core::validation::HeaderVerifier;
use rustock_storage::BlockStore;
use std::sync::Arc;
use std::time::Instant;

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let dir = args.next().unwrap_or_else(|| "/var/lib/rustock".into());
    let count: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(20_000);

    let store = BlockStore::open_read_only(&dir)?;
    let config = Arc::new(rustock_core::config::ChainConfig::mainnet());
    let verifier = HeaderVerifier::default_rsk(config);

    let start = match args.next().and_then(|s| s.parse::<u64>().ok()) {
        Some(n) => store.canonical_hash(n)?.ok_or_else(|| anyhow::anyhow!("no block #{n}"))?,
        None => store.head()?.ok_or_else(|| anyhow::anyhow!("no head"))?,
    };

    // Load first, verify second, so the disk does not colour the CPU number.
    let mut headers = Vec::with_capacity(count as usize);
    let mut hash = start;
    while (headers.len() as u64) < count {
        let Some(header) = store.header(hash)? else { break };
        hash = header.parent_hash;
        let done = header.number == 0;
        headers.push(header);
        if done {
            break;
        }
    }
    println!(
        "loaded {} headers, #{} down to #{}",
        headers.len(),
        headers.first().map(|h| h.number).unwrap_or(0),
        headers.last().map(|h| h.number).unwrap_or(0)
    );

    // Wire size, because the header walk's cost is as much bytes as CPU.
    {
        use alloy_rlp::Encodable;
        let total: usize = headers.iter().map(|h| { let mut b = Vec::new(); h.encode(&mut b); b.len() }).sum();
        let avg = total as f64 / headers.len().max(1) as f64;
        println!(
            "wire: {:.0} bytes/header average -> {:.2} GB for 9.27M headers",
            avg,
            avg * 9_270_000.0 / 1e9
        );
    }

    // Newest first, so `headers[i+1]` is `headers[i]`'s parent. Each header
    // gets its own rules once and its parent relationship once, which is what
    // the walk does.
    let t = Instant::now();
    let mut failures = 0u64;
    for pair in headers.windows(2) {
        if verifier.verify(&pair[0], Some(&pair[1])).is_err() {
            failures += 1;
        }
    }
    let elapsed = t.elapsed().as_secs_f64();
    let n = headers.len().saturating_sub(1) as f64;

    println!(
        "verified {n} headers in {elapsed:.2}s -- {:.0} headers/s{}",
        n / elapsed,
        if failures > 0 { format!(" ({failures} failed)") } else { String::new() }
    );
    println!(
        "at that rate, 9.27M headers take {:.1} minutes on one core",
        9_270_000.0 / (n / elapsed) / 60.0
    );

    // Negative control. A rate this high is only meaningful if the expensive
    // rule is actually running, so tamper with the merged-mining proof and
    // confirm the verifier rejects it. If this passes, the number above is
    // measuring something cheaper than consensus.
    if headers.len() >= 2 {
        let mut bad = headers[0].clone();
        match bad.bitcoin_merged_mining_header.as_mut() {
            Some(bytes) if !bytes.is_empty() => {
                let b = bytes.to_vec();
                let mut b = b;
                b[0] ^= 0xFF;
                *bytes = b.into();
            }
            _ => println!("CONTROL SKIPPED: no merged-mining header to tamper with"),
        }
        match verifier.verify(&bad, Some(&headers[1])) {
            Err(e) => println!("control: a tampered merged-mining header is rejected ({e:?})"),
            Ok(()) => println!(
                "CONTROL FAILED: a tampered merged-mining header VERIFIED -- \
                 the proof-of-work rule is not running"
            ),
        }
    }
    Ok(())
}
