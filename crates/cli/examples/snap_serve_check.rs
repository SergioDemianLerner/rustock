//! Does snapshot sync work on the real trie?
//!
//! Serves chunks from a live mainnet state and verifies every one of them the
//! way a client would -- with nothing but the bytes the chunk carries. Reports
//! what it costs: bytes on the wire, witness overhead, and how fast.
//!
//! The trie is opened read-only, so this is safe to run beside a syncing node.
//!
//! Two modes:
//!
//!   - **sampled** (default) takes chunks spread across the whole trie, which
//!     catches structural variety: a chunk near the root looks nothing like
//!     one deep in a subtree.
//!   - **sweep** walks forward from an offset, checking that each chunk begins
//!     exactly where the last ended. That is the tiling property -- no gap, no
//!     overlap in offset space -- which is what makes "cover 0..total" the
//!     same thing as "download the state".
//!
//! Usage: snap_serve_check <data-dir> [block] [chunk-bytes] [chunks] [sweep [from]]

use alloy_primitives::keccak256;
use rustock_storage::{BlockStore, RocksDbTrieStore};
use rustock_trie::snapshot::total_size;
use rustock_trie::snapshot_proof::{prove_chunk, verify_chunk, ChunkProof, Entry};
use rustock_trie::{TrieNode, TrieStore};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

/// Counts what serving a chunk actually costs the disk.
struct Counting {
    inner: Arc<dyn TrieStore>,
    reads: AtomicU64,
    bytes: AtomicU64,
}

impl TrieStore for Counting {
    fn get(&self, key: &[u8]) -> Option<Vec<u8>> {
        self.reads.fetch_add(1, Ordering::Relaxed);
        let v = self.inner.get(key);
        if let Some(v) = &v {
            self.bytes.fetch_add(v.len() as u64, Ordering::Relaxed);
        }
        v
    }
    fn put(&self, key: &[u8], value: &[u8]) {
        self.inner.put(key, value)
    }
}

fn main() -> anyhow::Result<()> {
    let a: Vec<String> = std::env::args().collect();
    if a.len() < 2 {
        eprintln!("usage: snap_serve_check <data-dir> [block] [chunk-bytes] [chunks]");
        std::process::exit(2);
    }
    let dir = &a[1];
    let chunk_bytes: u64 = a.get(3).and_then(|s| s.parse().ok()).unwrap_or(50_000);
    let want_chunks: usize = a.get(4).and_then(|s| s.parse().ok()).unwrap_or(200);

    let blocks = BlockStore::open_read_only(dir)?;

    // Where the trie lives depends on how the node was started. An epoch
    // backend keeps it beside the blocks, in its own directory; the default
    // keeps it in the block database's own column family. Either way this
    // opens it read-only, so it is safe beside a running node.
    let trie_dir = std::env::var("RUSTOCK_TRIE_DIR")
        .unwrap_or_else(|_| format!("{dir}/trie-epochs"));
    let trie: Arc<dyn TrieStore> = if std::path::Path::new(&trie_dir).is_dir() {
        eprintln!("(trie from the epoch store at {trie_dir})");
        Arc::new(rustock_storage::epoch_store::EpochTrieStore::open_read_only(
            &trie_dir,
            rustock_storage::epoch_store::EpochConfig::default(),
        )?)
    } else {
        Arc::new(RocksDbTrieStore::open_read_only(dir)?)
    };

    let counting = Arc::new(Counting {
        inner: trie.clone(),
        reads: AtomicU64::new(0),
        bytes: AtomicU64::new(0),
    });
    let trie: Arc<dyn TrieStore> = counting.clone();

    let number = match a.get(2).and_then(|s| s.parse::<u64>().ok()) {
        Some(n) => n,
        None => {
            let head = blocks.head()?.ok_or_else(|| anyhow::anyhow!("no head"))?;
            let header = blocks.header(head)?.ok_or_else(|| anyhow::anyhow!("no head header"))?;
            header.number.saturating_sub(10_000)
        }
    };

    let hash = blocks
        .canonical_hash(number)?
        .ok_or_else(|| anyhow::anyhow!("no canonical block at #{number}"))?;
    let header = blocks
        .header(hash)?
        .ok_or_else(|| anyhow::anyhow!("no header for #{number}"))?;
    let root_hash = header.state_root;

    let message = trie
        .get(root_hash.as_slice())
        .ok_or_else(|| anyhow::anyhow!("state root {root_hash:?} not in this store"))?;
    let root = TrieNode::try_from_message(&message, trie.as_ref())
        .ok_or_else(|| anyhow::anyhow!("state root does not parse"))?;

    let started = Instant::now();
    let total = total_size(&root, trie.as_ref());
    println!("block      #{number}  {hash:?}");
    println!("state root {root_hash:?}");
    println!(
        "trie size  {total} bytes ({:.1} GB), measured in {:?}",
        total as f64 / 1e9,
        started.elapsed()
    );
    println!();

    let sweep = a.get(5).is_some_and(|m| m == "sweep");
    let sweep_from: u64 = a.get(6).and_then(|s| s.parse().ok()).unwrap_or(0);

    // Sample across the whole trie rather than only the start: the shape of a
    // chunk near the root differs from one deep in a subtree.
    let mut offsets: Vec<u64> = if sweep {
        // Filled in as we go: where the next chunk starts is what is being
        // tested, so it cannot be computed in advance.
        vec![sweep_from]
    } else {
        (0..want_chunks).map(|i| (total / want_chunks as u64) * i as u64).collect()
    };
    offsets.dedup();
    let mut gaps = 0u64;

    let mut served_bytes = 0u64;
    let mut witness_bytes = 0u64;
    let mut value_bytes = 0u64;
    let mut nodes = 0u64;
    let mut serve_time = std::time::Duration::ZERO;
    let mut verify_time = std::time::Duration::ZERO;
    let mut widest_witness = 0usize;

    let mut i = 0usize;
    while i < offsets.len() && i < want_chunks {
        let from = offsets[i];
        let from = &from;
        let t0 = Instant::now();
        let proof = prove_chunk(&root, *from, chunk_bytes, trie.as_ref());
        serve_time += t0.elapsed();

        if proof.entries.is_empty() {
            println!("chunk at {from}: empty (past the end?)");
            continue;
        }

        // Exactly what a client receives: nothing from the local store.
        let wire = ChunkProof {
            entries: proof
                .entries
                .iter()
                .map(|e| Entry { message: e.message.clone(), long_values: e.long_values.clone() })
                .collect(),
            witness: proof.witness.clone(),
        };

        let t1 = Instant::now();
        let verified = verify_chunk(root_hash, *from, &wire)
            .map_err(|e| anyhow::anyhow!("chunk at offset {from} failed to verify: {e}"))?;
        verify_time += t1.elapsed();

        if verified.total != total {
            anyhow::bail!(
                "chunk at {from} reported trie size {} but it is {total}",
                verified.total
            );
        }

        // Every node must be exactly what the store holds under its hash.
        for node in &verified.nodes {
            let h = keccak256(&node.message);
            match trie.get(h.as_slice()) {
                Some(stored) if stored == node.message => {}
                Some(_) => anyhow::bail!("node {h:?} differs from the stored one"),
                None => anyhow::bail!("node {h:?} is not in the store"),
            }
        }

        nodes += verified.nodes.len() as u64;
        served_bytes += proof.entries.iter().map(|e| e.message.len() as u64).sum::<u64>();
        value_bytes += proof
            .entries
            .iter()
            .flat_map(|e| e.long_values.iter())
            .map(|v| v.len() as u64)
            .sum::<u64>();
        witness_bytes += proof.witness.iter().map(|w| w.len() as u64).sum::<u64>();
        widest_witness = widest_witness.max(proof.witness.len());

        if sweep {
            // The next chunk must begin exactly where this one ended. A gap
            // would be state nobody downloads; an overlap, state downloaded
            // twice.
            let next = verified.nodes.last().expect("non-empty").end();
            if next <= *from {
                anyhow::bail!("chunk at {from} did not advance (ended at {next})");
            }
            if next < total {
                offsets.push(next);
            }
            // Every node must sit where the one before it ended.
            let mut at = verified.nodes[0].offset;
            for node in &verified.nodes {
                if node.offset != at {
                    gaps += 1;
                    anyhow::bail!(
                        "gap inside the chunk from {from}: expected a node at {at}, found one at {}",
                        node.offset
                    );
                }
                at = node.end();
            }
        }

        if sweep && i > 0 && i % 100 == 0 {
            let reached = offsets.last().copied().unwrap_or(0);
            println!(
                "  ... {i} chunks, {nodes} nodes, at offset {reached} ({:.1}% of the trie)",
                reached as f64 * 100.0 / total as f64
            );
        }

        if i < 3 || i + 1 == offsets.len().min(want_chunks) {
            println!(
                "chunk at {from:>13}: {:>4} nodes, {:>6} B nodes, {:>6} B values, \
                 {:>2} witness nodes ({} B)",
                verified.nodes.len(),
                proof.entries.iter().map(|e| e.message.len()).sum::<usize>(),
                proof.entries.iter().flat_map(|e| e.long_values.iter()).map(|v| v.len()).sum::<usize>(),
                proof.witness.len(),
                proof.witness.iter().map(|w| w.len()).sum::<usize>(),
            );
        }
        i += 1;
    }

    let sampled = i as u64;
    if sweep {
        let reached = offsets.last().copied().unwrap_or(0);
        println!();
        println!(
            "swept {sweep_from}..{reached} of {total} ({:.1}%), {gaps} gaps",
            (reached - sweep_from) as f64 * 100.0 / total as f64
        );
    }
    let payload = served_bytes + value_bytes;
    println!();
    println!("{sampled} chunks verified, {nodes} nodes, all matching the store");
    println!(
        "wire      {} B nodes + {} B values + {} B witness = {} B",
        served_bytes,
        value_bytes,
        witness_bytes,
        payload + witness_bytes
    );
    println!(
        "witness   {:.2}% of the payload, widest {widest_witness} nodes",
        witness_bytes as f64 * 100.0 / payload.max(1) as f64
    );
    println!(
        "disk      {} reads, {} B, {:.1} reads per node served",
        counting.reads.load(Ordering::Relaxed),
        counting.bytes.load(Ordering::Relaxed),
        counting.reads.load(Ordering::Relaxed) as f64 / nodes.max(1) as f64
    );
    println!(
        "serve     {:?} total, {:?} per chunk",
        serve_time,
        serve_time / sampled as u32
    );
    println!(
        "verify    {:?} total, {:?} per chunk",
        verify_time,
        verify_time / sampled as u32
    );

    // What the whole state would cost at this rate.
    let per_byte_wire = (payload + witness_bytes) as f64 / payload.max(1) as f64;
    println!();
    println!(
        "whole state: ~{:.1} GB on the wire ({:.1}% over the {:.1} GB of trie data)",
        total as f64 * per_byte_wire / 1e9,
        (per_byte_wire - 1.0) * 100.0,
        total as f64 / 1e9
    );
    let chunks_needed = total / chunk_bytes.max(1);
    println!(
        "           ~{chunks_needed} chunks of {chunk_bytes} B; verification alone ~{:.0} s",
        verify_time.as_secs_f64() / sampled as f64 * chunks_needed as f64
    );
    Ok(())
}
