//! What a batched read buys, measured on the workload that uses one.
//!
//! `get_many` exists because a trie traversal issues *dependent* reads —
//! each address known only once the previous read returns — so the device
//! sees one outstanding request at a time. Keys discovered at the same
//! level are independent of each other, and the sweep's marking phase is
//! the one place in the store that has a whole level in hand at once.
//!
//! So this walks the trie level by level, exactly as marking does, and
//! times it.
//!
//! **One mode per run, by design.** Serving each level both ways inside
//! one process cannot measure anything: whichever pass goes first pays
//! the cold cache and the second reads what it warmed. Doing it that way
//! reported 419x for batching, and then 0.05x with the order reversed --
//! the same number measured from either end, and neither of them about
//! batching. Run the tool twice, with the page cache evicted before each.
//!
//! ```text
//! batch_read_bench <dir> --mode serial|batch [--flatdb] [--levels N]
//! batch_read_bench <epoch-root> --epochs --root <hex> --mode ... [--flatdb]
//! ```
//!
//! `--epochs` opens the directory as an epoch store rather than a single
//! trie, reading newest epoch first exactly as the node does, which is
//! what makes a measurement against a real store represent the real read
//! path. The state root then has to be given: an epoch store has no ROOT
//! file, because which root is current is the block database's business.

use anyhow::{Context, Result};
use rustock_trie::{NodeRef, TrieNode, TrieStore};
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Instant;

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let dir = args.next().context("usage: batch_read_bench <dir> [--flatdb] [--levels N]")?;
    let mut flatdb = false;
    let mut levels = 14usize;
    let mut cache_mb = 256usize;
    let mut expected_keys = 10_000_000u64;
    let mut batch_mode = false;
    let mut mode_given = false;
    let mut epochs = false;
    let mut explicit_root: Option<String> = None;
    let mut flat_threads = 16usize;
    let rest: Vec<String> = args.collect();
    let mut i = 0;
    while i < rest.len() {
        match rest[i].as_str() {
            "--flatdb" => {
                flatdb = true;
                i += 1;
            }
            "--levels" => {
                levels = rest[i + 1].parse()?;
                i += 2;
            }
            "--cache-mb" => {
                cache_mb = rest[i + 1].parse()?;
                i += 2;
            }
            "--mode" => {
                batch_mode = match rest[i + 1].as_str() {
                    "batch" => true,
                    "serial" => false,
                    other => anyhow::bail!("--mode takes serial or batch, not {other:?}"),
                };
                mode_given = true;
                i += 2;
            }
            "--flat-threads" => {
                flat_threads = rest[i + 1].parse()?;
                i += 2;
            }
            "--epochs" => {
                epochs = true;
                i += 1;
            }
            "--root" => {
                explicit_root = Some(rest[i + 1].clone());
                i += 2;
            }
            "--expected-keys" => {
                expected_keys = rest[i + 1].parse()?;
                i += 2;
            }
            other => anyhow::bail!("unknown option {other}"),
        }
    }

    anyhow::ensure!(
        mode_given,
        "--mode serial|batch is required: serving both ways in one process measures \
         the page cache rather than the batching"
    );

    let store: Arc<dyn TrieStore> = if epochs {
        // The node's own configuration, so the layering and the read order
        // are the ones production uses.
        let config = rustock_storage::epoch_store::EpochConfig {
            epochs: 4,
            burial_depth: 4000,
            rotate_bytes: 1 << 30,
        };
        if flatdb {
            Arc::new(rustock_storage::epoch_store::EpochTrieStore::open_read_only_with(
                &dir,
                config,
                Arc::new(
                    rustock_storage::epoch_backend::FlatFactory::new(expected_keys)
                        .with_read_threads(flat_threads),
                ),
            )?)
        } else {
            Arc::new(rustock_storage::epoch_store::EpochTrieStore::open_read_only(
                &dir, config,
            )?)
        }
    } else if flatdb {
        Arc::new(rustock_storage::FlatTrieStore::open(
            &dir,
            rustock_flatdb::Options {
                expected_keys,
                read_only: true,
                ..rustock_flatdb::Options::default()
            },
        )?)
    } else {
        Arc::new(rustock_storage::RocksDbTrieStore::open_with_cache(
            &dir,
            cache_mb * 1024 * 1024,
        )?)
    };

    // When a single flatdb store is under test the batch can be served
    // directly, which is what lets the two mechanisms be varied.
    let flat_direct: Option<rustock_flatdb::FlatDb> = if flatdb && !epochs {
        Some(rustock_flatdb::FlatDb::open(
            &dir,
            rustock_flatdb::Options {
                expected_keys,
                read_only: true,
                ..rustock_flatdb::Options::default()
            },
        )?)
    } else {
        None
    };

    let root: alloy_primitives::B256 = match explicit_root {
        Some(hex) => hex.trim().parse().context("--root is not a 32-byte hex hash")?,
        None => std::fs::read_to_string(format!("{dir}/ROOT"))
            .context("no --root given and the directory has no ROOT file")?
            .trim()
            .parse()?,
    };

    println!(
        "{} {} at {dir}",
        if flatdb { "flatdb" } else { "rocksdb" },
        if epochs { "epoch store" } else { "single trie" }
    );
    println!("root {root:?}");
    println!("mode {}", if batch_mode { "batch" } else { "serial" });
    println!("{:>6}  {:>9}  {:>11}", "level", "nodes", "ms");

    let mut frontier = vec![root];
    let mut seen: HashSet<alloy_primitives::B256> = HashSet::new();
    seen.insert(root);
    let mut total_serial = 0f64;
    let mut total_batch = 0f64;

    for level in 0..levels {
        if frontier.is_empty() {
            break;
        }
        let keys: Vec<Vec<u8>> = frontier.iter().map(|h| h.as_slice().to_vec()).collect();

        let t = Instant::now();
        let batched = if batch_mode {
            match &flat_direct {
                Some(db) => db.get_many_on(&keys, flat_threads),
                None => store.get_many(&keys),
            }
        } else {
            keys.iter().map(|k| store.get(k)).collect()
        };
        let ms = t.elapsed().as_secs_f64() * 1000.0;

        println!("{level:>6}  {:>9}  {ms:>11.1}", keys.len());
        if batch_mode {
            total_batch += ms;
        } else {
            total_serial += ms;
        }

        let mut next = Vec::new();
        for bytes in batched.into_iter().flatten() {
            let node = TrieNode::from_message(&bytes, store.as_ref());
            for child in [&node.left, &node.right] {
                if let NodeRef::Hash(h) = child {
                    if seen.insert(*h) {
                        next.push(*h);
                    }
                }
            }
        }
        frontier = next;
    }

    println!();
    println!("total   {:.0} ms", total_serial + total_batch);
    Ok(())
}
