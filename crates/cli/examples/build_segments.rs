//! Replay a block range, verifying every state root, and build the range's
//! trie segments as a by-product.
//!
//! One process per range. Ranges are independent because the archival snapshot
//! holds every block's pre-state from #1,591,000 (docs/trie-segments-design.md
//! §2), so a worker seeds from the header's parent state root and goes forward;
//! the range starting at genesis builds its state from the chain config instead.
//!
//! Reads are served by a sliding window of in-memory tables, falling back to
//! this worker's own sealed segments and then to the archival trie. Everything
//! read or written lands in the newest table, so each sealed segment is
//! self-contained for its block range.
//!
//! Verification is not a separate pass: from #1,591,000 every header carries a
//! Unitrie root, and a mismatch stops this worker at that block.
//!
//! Work is claimed, not assigned. Predicting the cost of a block range failed
//! twice here -- balanced on trie-nodes-touched one worker drew a 142-hour
//! range against another's 8, and rebalanced on sampled gas it was still 44
//! against 24, because gas buys different amounts of real work in different
//! eras. Workers instead claim the next unclaimed chunk when they finish one,
//! so the split self-corrects and the pooled ETA becomes meaningful.
//!
//! Each chunk produces its own segment database, which is what a range-scoped
//! segment *is*: dense, self-contained, and routable by height.
//!
//! Usage: build_segments <worker-id> <block-dir> <archive-trie> <chunks-dir>
//!                       <out-root> <seal-mb> <tables> <progress-file>

use rustock_core::config::ChainConfig;
use rustock_core::Block;
use rustock_execution::{BlockProcessor, RskHardforkConfig};
use rustock_storage::window_store::{WindowConfig, WindowStore};
use rustock_storage::{BlockStore, RocksDbTrieStore};
use rustock_trie::{account_key, AccountState, TrieKeySlice, TrieNode, TrieStore};
use std::io::Write;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Instant;

const UNITRIE_FROM: u64 = 1_591_000;
const PROGRESS_EVERY: u64 = 200;
/// Blocks between dropping the materialized trie tree and re-rooting from the
/// hash.
///
/// `execute_block` returns a `TrieNode` whose children are materialized as
/// execution touches them, and carrying it forward accumulates the whole
/// touched subtree in memory: measured at ~800 MB per worker against a 145 MB
/// window, i.e. the tree was five times the cache it was supposed to be
/// feeding, and four workers spent 3.2 GB of a 7.7 GB machine on it. Every one
/// of those nodes is already in the window store, so the tree is pure
/// duplication -- re-rooting from the hash makes it lazy again and the nodes
/// fault back in from memory.
const REROOT_EVERY: u64 = 500;
/// Blocks between checkpoints. Each one seals the resident window, so it costs
/// a refill afterwards; rare enough not to matter, frequent enough that a kill
/// costs minutes rather than hours.
const CHECKPOINT_EVERY: u64 = 50_000;

fn checkpoint_every() -> u64 {
    std::env::var("RUSTOCK_CHECKPOINT_EVERY")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|v| *v > 0)
        .unwrap_or(CHECKPOINT_EVERY)
}

fn write_progress(path: &str, fields: &[(&str, String)]) {
    let body: String = fields.iter().map(|(k, v)| format!("{k}={v}\n")).collect();
    let tmp = format!("{path}.tmp");
    if std::fs::write(&tmp, body).is_ok() {
        let _ = std::fs::rename(&tmp, path);
    }
}

/// Atomically take the next chunk. `rename` is the claim: exactly one worker
/// can win it, with no lock file and no coordinator.
/// `prefer_start`: the block after the chunk this worker just finished.
///
/// Taking the adjacent chunk keeps the sliding window warm across the seam.
/// Measured, a cold window costs about 40% for its first ~10,000 blocks -- one
/// worker went from 4.3 to 7.6 blocks/s as it warmed -- so a worker that hops
/// to an unrelated range pays that again every chunk. Adjacency avoids it in
/// the common case and costs nothing when no adjacent chunk is free.
fn claim_next(chunks: &std::path::Path, id: &str, prefer_start: Option<u64>) -> Option<(u64, u64, String)> {
    let mut todo: Vec<_> = std::fs::read_dir(chunks).ok()?
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|n| n.ends_with(".todo"))
        .collect();
    todo.sort();
    if let Some(want) = prefer_start {
        if let Some(pos) = todo.iter().position(|n| {
            n.split('-').nth(1).and_then(|v| v.parse::<u64>().ok()) == Some(want)
        }) {
            let adj = todo.remove(pos);
            todo.insert(0, adj);
        }
    }
    for name in todo {
        let from = chunks.join(&name);
        let stem = name.trim_end_matches(".todo").to_string();
        let to = chunks.join(format!("{stem}.run-{id}"));
        if std::fs::rename(&from, &to).is_ok() {
            let parts: Vec<&str> = stem.split('-').collect();
            if parts.len() >= 3 {
                if let (Ok(a), Ok(b)) = (parts[1].parse::<u64>(), parts[2].parse::<u64>()) {
                    return Some((a, b, stem));
                }
            }
            let _ = std::fs::rename(&to, &from);
        }
    }
    None
}

/// Re-take chunks this worker was running when it died, so a restart resumes
/// them (the chunk's own checkpoint says where it had reached).
fn reclaim_own(chunks: &std::path::Path, id: &str) {
    let suffix = format!(".run-{id}");
    if let Ok(rd) = std::fs::read_dir(chunks) {
        for e in rd.flatten() {
            let n = e.file_name().to_string_lossy().to_string();
            if n.ends_with(&suffix) {
                let stem = n.trim_end_matches(&suffix);
                let _ = std::fs::rename(chunks.join(&n), chunks.join(format!("{stem}.todo")));
            }
        }
    }
}

fn main() -> anyhow::Result<()> {
    let a: Vec<String> = std::env::args().skip(1).collect();
    if a.len() < 8 {
        eprintln!("usage: build_segments <worker-id> <block-dir> <archive-trie> <chunks-dir> <out-root> <seal-mb> <tables> <progress-file>");
        std::process::exit(2);
    }
    let (id, block_dir, archive, chunks_dir, out_root) = (&a[0], &a[1], &a[2], &a[3], &a[4]);
    let seal_mb: u64 = a[5].parse()?;
    let tables: usize = a[6].parse()?;
    let progress = a[7].clone();
    let chunks = std::path::PathBuf::from(chunks_dir);
    reclaim_own(&chunks, id);

    let t_all = Instant::now();
    let mut total_done: u64 = 0;
    let mut prefer: Option<u64> = None;
    loop {
        let Some((start, end, stem)) = claim_next(&chunks, id, prefer) else {
            eprintln!("[w{id}] no chunks left");
            break;
        };
        let out_dir = format!("{out_root}/{stem}");
        eprintln!("[w{id}] claimed {stem} (#{start}..#{end})");
        match run_chunk(id, block_dir, archive, &out_dir, start, end, seal_mb, tables,
                        &progress, &t_all, &mut total_done) {
            Ok(n) => {
                let _ = std::fs::rename(chunks.join(format!("{stem}.run-{id}")),
                                        chunks.join(format!("{stem}.done")));
                prefer = Some(end + 1);
                eprintln!("[w{id}] finished {stem}: {n} blocks");
            }
            Err(e) => {
                eprintln!("[w{id}] chunk {stem} FAILED: {e:?}");
                let _ = std::fs::rename(chunks.join(format!("{stem}.run-{id}")),
                                        chunks.join(format!("{stem}.failed")));
                return Err(e);
            }
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn run_chunk(
    id: &str, block_dir: &str, archive: &str, out_dir: &str,
    start: u64, end: u64, seal_mb: u64, tables: usize,
    progress: &str, t_all: &Instant, total_done: &mut u64,
) -> anyhow::Result<u64> {

    let store = Arc::new(BlockStore::open_read_only(block_dir)?);
    let fallback: Arc<dyn TrieStore> = Arc::new(RocksDbTrieStore::open_read_only(archive)?);
    let window = Arc::new(WindowStore::new(
        WindowConfig {
            seal_bytes: seal_mb * 1024 * 1024,
            tables,
            dir: std::path::PathBuf::from(out_dir),
        },
        Some(fallback.clone()),
    )?);
    let win: Arc<dyn TrieStore> = window.clone();

    // Resume from a checkpoint if this range was already partly built.
    let resume = WindowStore::read_checkpoint(std::path::Path::new(out_dir));
    let mut start = start;
    let mut seeded_root: Option<TrieNode> = None;
    if let Some((last, root_bytes)) = resume {
        if last >= start && last < end {
            let data = win.get(&root_bytes).or_else(|| fallback.get(&root_bytes));
            match data {
                Some(d) => {
                    seeded_root = Some(TrieNode::from_message(&d, win.as_ref()));
                    eprintln!("[w{id}] resuming at #{} from checkpoint", last + 1);
                    start = last + 1;
                }
                None => eprintln!("[w{id}] checkpoint at #{last} names a root that is not in the \
                                   sealed store; starting the range over"),
            }
        }
    }

    // Seed the starting state.
    let mut root = if let Some(r) = seeded_root { r } else if start <= 1 {
        let config = ChainConfig::mainnet();
        let mut r = TrieNode::empty();
        for e in &config.genesis_alloc() {
            let k = account_key(&e.address);
            r = r.put(&TrieKeySlice::from_key(&k), &AccountState::new(e.nonce, e.balance).encode(), win.as_ref());
        }
        r.save(win.as_ref(), true);
        eprintln!("[w{id}] genesis root {:?}", r.compute_hash(win.as_ref()));
        r
    } else {
        let h = store.header(store.canonical_hash(start)?.expect("hash"))?.expect("header");
        let p = store.header(h.parent_hash)?.expect("parent");
        let data = fallback.get(p.state_root.as_slice()).ok_or_else(|| {
            anyhow::anyhow!("[w{id}] parent state root {:?} for #{} not in the archive", p.state_root, p.number)
        })?;
        eprintln!("[w{id}] seeded from #{} root {:?}", p.number, p.state_root);
        TrieNode::from_message(&data, fallback.as_ref())
    };

    let processor = BlockProcessor::new(RskHardforkConfig::mainnet(), store.clone());
    let ckpt_every = checkpoint_every();
    let t0 = Instant::now();
    let total = end - start + 1;
    let mut verified = 0u64;
    let mut done = 0u64;
    let chunk_base = *total_done;

    for n in start..=end {
        window.set_block(n);
        let hash = match store.canonical_hash(n)? { Some(h) => h, None => { eprintln!("[w{id}] no canonical hash at #{n}"); break } };
        let header = match store.header(hash)? { Some(h) => h, None => break };
        let (txs, oms) = match store.body(hash)? { Some(b) => b, None => { eprintln!("[w{id}] no body at #{n}"); break } };
        let expected = header.state_root;
        let p = processor.execute_block(&Block { header, transactions: txs, ommers: oms }, &root, win.clone())?;

        if n >= UNITRIE_FROM {
            if p.state_root_hash != expected {
                eprintln!("[w{id}] STATE ROOT MISMATCH at #{n}: computed {:?} header {:?}",
                          p.state_root_hash, expected);
                write_progress(progress, &[
                    ("id", id.to_string()), ("state", "FAILED".into()),
                    ("block", n.to_string()), ("start", start.to_string()), ("end", end.to_string()),
                    ("done", done.to_string()), ("total", total.to_string()),
                    ("elapsed_s", format!("{:.0}", t0.elapsed().as_secs_f64())),
                ]);
                window.finish()?;
                anyhow::bail!("state root mismatch at #{n}");
            }
            verified += 1;
        }
        let p_root_hash = p.state_root_hash;
        root = p.new_state_root;
        done += 1;

        if done % REROOT_EVERY == 0 {
            match win.get(p_root_hash.as_slice()) {
                Some(data) => root = TrieNode::from_message(&data, win.as_ref()),
                None => eprintln!("[w{id}] re-root at #{n}: root {p_root_hash:?} not readable; \
                                   keeping the materialized tree"),
            }
        }

        if done % ckpt_every == 0 {
            window.checkpoint(n, p_root_hash.as_slice())?;
        }

        if done % PROGRESS_EVERY == 0 || n == end {
            let el = t_all.elapsed().as_secs_f64();
            let rate = (chunk_base + done) as f64 / el.max(0.001);
            let reads = window.reads.load(Ordering::Relaxed).max(1);
            write_progress(progress, &[
                ("id", id.to_string()), ("state", "running".into()),
                ("block", n.to_string()), ("start", start.to_string()), ("end", end.to_string()),
                ("done", (chunk_base + done).to_string()), ("total", total.to_string()),
                ("elapsed_s", format!("{el:.0}")), ("rate", format!("{rate:.2}")),
                ("verified", verified.to_string()),
                ("segments", window.segment_count().to_string()),
                ("sealed_bytes", window.sealed_bytes.load(Ordering::Relaxed).to_string()),
                ("resident_bytes", window.resident_bytes().to_string()),
                ("fallback_pct", format!("{:.3}", window.hits_fallback.load(Ordering::Relaxed) as f64 * 100.0 / reads as f64)),
                ("sealed_pct", format!("{:.3}", window.hits_sealed.load(Ordering::Relaxed) as f64 * 100.0 / reads as f64)),
            ]);
        }
    }

    window.finish()?;
    *total_done += done;
    let el = t0.elapsed().as_secs_f64();
    write_progress(progress, &[
        ("id", id.to_string()), ("state", "running".into()),
        ("block", end.to_string()), ("start", start.to_string()), ("end", end.to_string()),
        ("done", done.to_string()), ("total", total.to_string()),
        ("elapsed_s", format!("{el:.0}")), ("rate", format!("{:.2}", done as f64 / el.max(0.001))),
        ("verified", verified.to_string()),
        ("segments", window.segment_count().to_string()),
        ("sealed_bytes", window.sealed_bytes.load(Ordering::Relaxed).to_string()),
    ]);
    let _ = std::io::stderr().flush();
    eprintln!("[w{id}] chunk done: {done} blocks, {verified} state roots verified, {} segments, {:.0} blk/s",
              window.segment_count(), done as f64 / el.max(0.001));
    Ok(done)
}
