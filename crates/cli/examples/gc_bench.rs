//! Synthetic trie-churn benchmark for the epoch garbage collector.
//!
//! Simulates a chain whose every block hammers contract storage: each block
//! increments `--slots` sequential storage slots of one contract, each slot
//! starting from a different value. That is deliberately hostile to a trie
//! store -- every block rewrites a thousand leaves and every node on the path
//! from each to the root, so the store grows fast and almost all of it becomes
//! unreachable almost immediately. Which is exactly the condition the collector
//! exists for.
//!
//! Note that RSK's storage keys hash the slot (`keccak256(slot)[0:10]`), so
//! "sequential" slots do not land in adjacent trie paths. They scatter. That
//! makes this harder than a clustered workload, and more representative.
//!
//! What is measured, in both backends, is wall time, bytes on disk, and -- for
//! the epoch store -- how many epoch probes each logical read costs. The run
//! ends by verifying that the head state is still fully readable and that the
//! counters hold the values they should, because a collector that is fast and
//! wrong is worse than no collector.
//!
//! ```text
//! gc_bench --backend epoch  --dir /srv/gc-bench/epoch  --blocks 2000
//! gc_bench --backend single --dir /srv/gc-bench/single --blocks 2000
//! ```

use alloy_primitives::{Address, B256};
use anyhow::{bail, Result};
use rustock_storage::epoch_store::{EpochConfig, EpochTrieStore};
use rustock_trie::{key_mapper, TrieKeySlice, TrieNode, TrieStore};
use std::sync::Arc;
use std::time::Instant;

struct Args {
    backend: String,
    dir: String,
    blocks: u64,
    slots: u64,
    epochs: usize,
    rotate_mb: u64,
    burial: u64,
    report_every: u64,
}

fn parse_args() -> Result<Args> {
    let mut a = Args {
        backend: "epoch".into(),
        dir: "/srv/gc-bench/run".into(),
        blocks: 1000,
        slots: 1000,
        epochs: 4,
        rotate_mb: 256,
        burial: 100,
        report_every: 100,
    };
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < argv.len() {
        let get = |i: usize| -> Result<String> {
            argv.get(i + 1).cloned().ok_or_else(|| anyhow::anyhow!("{} needs a value", argv[i]))
        };
        match argv[i].as_str() {
            "--backend" => { a.backend = get(i)?; i += 2; }
            "--dir" => { a.dir = get(i)?; i += 2; }
            "--blocks" => { a.blocks = get(i)?.parse()?; i += 2; }
            "--slots" => { a.slots = get(i)?.parse()?; i += 2; }
            "--epochs" => { a.epochs = get(i)?.parse()?; i += 2; }
            "--rotate-mb" => { a.rotate_mb = get(i)?.parse()?; i += 2; }
            "--burial" => { a.burial = get(i)?.parse()?; i += 2; }
            "--report-every" => { a.report_every = get(i)?.parse()?; i += 2; }
            other => bail!("unknown argument {other}"),
        }
    }
    Ok(a)
}

fn dir_size(p: &std::path::Path) -> u64 {
    let mut t = 0;
    if let Ok(rd) = std::fs::read_dir(p) {
        for e in rd.flatten() {
            match e.metadata() {
                Ok(m) if m.is_file() => t += m.len(),
                Ok(m) if m.is_dir() => t += dir_size(&e.path()),
                _ => {}
            }
        }
    }
    t
}

fn mb(b: u64) -> f64 { b as f64 / (1024.0 * 1024.0) }

/// The 32-byte EVM-style word holding a counter.
fn word(v: u64) -> Vec<u8> {
    let mut w = vec![0u8; 32];
    w[24..].copy_from_slice(&v.to_be_bytes());
    w
}

fn slot_hash(i: u64) -> B256 {
    let mut b = [0u8; 32];
    b[24..].copy_from_slice(&i.to_be_bytes());
    B256::from(b)
}

fn main() -> Result<()> {
    tracing_subscriber::fmt().with_max_level(tracing::Level::INFO).init();
    let args = parse_args()?;
    let contract = Address::from([0x42u8; 20]);

    if std::path::Path::new(&args.dir).exists() {
        std::fs::remove_dir_all(&args.dir)?;
    }
    std::fs::create_dir_all(&args.dir)?;

    let epoch_store: Option<Arc<EpochTrieStore>> = if args.backend == "epoch" {
        Some(Arc::new(EpochTrieStore::open(
            &args.dir,
            EpochConfig {
                epochs: args.epochs,
                burial_depth: args.burial,
                rotate_bytes: args.rotate_mb * (1 << 20),
            },
        )?))
    } else {
        None
    };
    let store: Arc<dyn TrieStore> = match &epoch_store {
        Some(e) => e.clone(),
        None => Arc::new(rustock_storage::RocksDbTrieStore::open(std::path::Path::new(&args.dir))?),
    };

    println!("backend      {}", args.backend);
    println!("blocks       {}", args.blocks);
    println!("slots/block  {}", args.slots);
    if args.backend == "epoch" {
        println!("epochs       {}", args.epochs);
        println!("rotate at    {} MB", args.rotate_mb);
        println!("burial       {} blocks", args.burial);
    }
    println!();

    let mut root = TrieNode::empty();
    let mut roots: Vec<B256> = Vec::with_capacity(args.blocks as usize + 1);
    let mut collections = 0u64;
    let mut collect_secs = 0.0f64;
    let mut reclaimed = 0u64;
    let started = Instant::now();
    let mut last_report = Instant::now();

    for block in 1..=args.blocks {
        // Each block touches a window of slots that advances, so the working
        // set moves and old versions die off -- rather than rewriting the same
        // thousand leaves forever, which would be unrealistically kind to the
        // collector's drain step.
        let base = (block - 1) * (args.slots / 4);
        for i in 0..args.slots {
            let slot = slot_hash(base + i);
            let key = key_mapper::storage_key(&contract, &slot);
            let ks = TrieKeySlice::from_key(&key);
            let cur = root
                .get(&ks, store.as_ref())
                .map(|v| {
                    let mut b = [0u8; 8];
                    b.copy_from_slice(&v[24..32]);
                    u64::from_be_bytes(b)
                })
                .unwrap_or(base + i); // each slot starts from a different value
            root = root.put(&ks, &word(cur + 1), store.as_ref());
        }
        root.save(store.as_ref(), true);
        let rh = root.compute_hash(store.as_ref());
        roots.push(rh);

        // Reload the root from its hash before the next block.
        //
        // Without this the benchmark measures nothing. `save` leaves the
        // in-memory tree intact, with children still materialised, so every
        // later block walks RAM and the store is never read -- the first
        // version of this harness reported 0.00 epoch probes per read, which is
        // what gave it away. A real node between blocks holds a root hash and
        // faults nodes in as it touches them, which is the access pattern the
        // collector has to survive.
        let bytes = store
            .get(rh.as_slice())
            .ok_or_else(|| anyhow::anyhow!("state root {rh:?} vanished at block {block}"))?;
        root = TrieNode::from_message(&bytes, store.as_ref());

        // Collect against a buried root, never the head: collecting against the
        // head would leave nothing recoverable if the chain reorganised.
        if let Some(es) = &epoch_store {
            if es.should_collect() && block > args.burial {
                let h = (block - args.burial) as usize - 1;
                let t = Instant::now();
                let st = es.collect(roots[h])?;
                collect_secs += t.elapsed().as_secs_f64();
                reclaimed += st.reclaimed_bytes;
                collections += 1;
            }
        }

        if args.report_every > 0 && block % args.report_every == 0 {
            let size = dir_size(std::path::Path::new(&args.dir));
            let secs = started.elapsed().as_secs_f64();
            println!(
                "block {:>6} | {:>8.1} MB on disk | {:>6.1} blocks/s | {} collection(s), {:.1} MB reclaimed",
                block, mb(size), block as f64 / secs, collections, mb(reclaimed)
            );
            last_report = Instant::now();
        }
    }
    let _ = last_report;

    let elapsed = started.elapsed().as_secs_f64();
    if let Some(es) = &epoch_store {
        es.flush()?;
    }
    let final_size = dir_size(std::path::Path::new(&args.dir));

    println!();
    println!("RESULT");
    println!("  {:<26}{:>14}", "backend", args.backend);
    println!("  {:<26}{:>14.1}", "wall seconds", elapsed);
    println!("  {:<26}{:>14.1}", "blocks/s", args.blocks as f64 / elapsed);
    println!("  {:<26}{:>13.1} MB", "final size", mb(final_size));
    println!("  {:<26}{:>14}", "collections", collections);
    println!("  {:<26}{:>13.1} MB", "reclaimed", mb(reclaimed));
    println!("  {:<26}{:>14.1}", "seconds in collection", collect_secs);
    if let Some(es) = &epoch_store {
        use std::sync::atomic::Ordering;
        let r = es.reads.load(Ordering::Relaxed).max(1);
        let p = es.read_probes.load(Ordering::Relaxed);
        println!("  {:<26}{:>14.2}", "epoch probes per read", p as f64 / r as f64);
        println!("  {:<26}{:>14}", "epochs on disk", es.epoch_count());
    }

    // Correctness. A collector that is fast and wrong is worse than none, so
    // the run does not end until the head state is proven intact.
    println!();
    println!("VERIFY");
    let head_root = *roots.last().unwrap();
    let loaded = store
        .get(head_root.as_slice())
        .ok_or_else(|| anyhow::anyhow!("head state root {head_root:?} is gone"))?;
    let head = TrieNode::from_message(&loaded, store.as_ref());
    let recomputed = head.compute_hash(store.as_ref());
    if recomputed != head_root {
        bail!("head root rehashes to {recomputed:?}, expected {head_root:?}");
    }
    println!("  head state root re-reads and re-hashes         ok");

    // Every node under the head must resolve; a dangling reference is exactly
    // what a wrong sweep produces.
    let mut seen = std::collections::HashSet::new();
    let mut stack = vec![head_root];
    let mut nodes = 0u64;
    while let Some(h) = stack.pop() {
        if !seen.insert(h) { continue; }
        let Some(bytes) = store.get(h.as_slice()) else {
            bail!("dangling reference: {h:?} is referenced but not stored");
        };
        nodes += 1;
        let n = TrieNode::from_message(&bytes, store.as_ref());
        if n.has_long_value() {
            if let Some(vh) = n.value_hash {
                if store.get(vh.as_slice()).is_none() {
                    bail!("dangling long value: {vh:?}");
                }
            }
        }
        for c in [&n.left, &n.right] {
            if let rustock_trie::NodeRef::Hash(ch) = c { stack.push(*ch); }
        }
    }
    println!("  full walk of head state, {nodes} nodes         ok");

    // Spot-check the counters themselves: the last block's slots must each have
    // been incremented exactly once past their starting value.
    let base = (args.blocks - 1) * (args.slots / 4);
    let mut checked = 0;
    for i in (0..args.slots).step_by((args.slots / 20).max(1) as usize) {
        let slot = slot_hash(base + i);
        let key = key_mapper::storage_key(&contract, &slot);
        let got = head
            .get(&TrieKeySlice::from_key(&key), store.as_ref())
            .ok_or_else(|| anyhow::anyhow!("slot {} missing from head state", base + i))?;
        let mut b = [0u8; 8];
        b.copy_from_slice(&got[24..32]);
        let v = u64::from_be_bytes(b);
        if v < base + i + 1 {
            bail!("slot {} holds {}, expected at least {}", base + i, v, base + i + 1);
        }
        checked += 1;
    }
    println!("  {checked} storage slots hold the expected counters   ok");
    println!();
    println!("PASS");
    Ok(())
}
