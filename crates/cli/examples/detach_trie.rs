//! Copies the `trie_nodes` column family out of a node database into a
//! standalone trie database.
//!
//! The trie is ~83% of the node's database and has nothing in common with the
//! rest of it: it is content-addressed, written once, read randomly, and never
//! updated in place, while headers and bodies are keyed by number or hash and
//! are comparatively tiny. Separating them allows the trie to be swapped,
//! copied or replaced -- which is what makes it possible to keep a full
//! archival trie and a collected one side by side and point the node at either.
//!
//! Source is opened **read-only**, so this can be run against a copy, and
//! against a datadir whose node is stopped without any risk of modifying it.
//!
//! ```text
//! detach_trie /var/lib/rustock /var/lib/rustock-trie [threads]
//! ```
//!
//! Afterwards, run the node with `--trie-backend external --trie-dir <dest>`.
//! The source column family is left alone; reclaiming its space is a separate,
//! destructive step.

use anyhow::{Context, Result};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

const CF_TRIE: &str = "trie_nodes";

fn main() -> Result<()> {
    tracing_subscriber::fmt().with_max_level(tracing::Level::INFO).init();
    let mut args = std::env::args().skip(1);
    let src_dir = args.next().context("usage: detach_trie <src-datadir> <dest-dir> [threads]")?;
    let dst_dir = args.next().context("missing destination")?;
    let threads: usize = args
        .next()
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4));

    if std::path::Path::new(&dst_dir).exists() {
        anyhow::bail!("{dst_dir} already exists; remove it or choose another path");
    }

    let mut sopts = rocksdb::Options::default();
    sopts.create_if_missing(false);
    sopts.set_max_open_files(512);
    let cfs = ["headers", "block_numbers", "total_difficulty", "block_bodies",
               "receipts", "tx_index", CF_TRIE];
    let src = rocksdb::DB::open_cf_for_read_only(&sopts, &src_dir, cfs, false)
        .with_context(|| format!("opening {src_dir} read-only"))?;
    let src = Arc::new(src);

    let mut dopts = rocksdb::Options::default();
    dopts.create_if_missing(true);
    dopts.create_missing_column_families(true);
    // A bulk load of content-addressed keys in random order, written once.
    // Large memtables keep that from becoming many small sorted runs.
    dopts.set_write_buffer_size(256 * 1024 * 1024);
    dopts.set_max_write_buffer_number(4);
    dopts.set_disable_auto_compactions(true);
    dopts.increase_parallelism(threads as i32);
    let mut block = rocksdb::BlockBasedOptions::default();
    block.set_bloom_filter(10.0, false);
    let mut cfo = rocksdb::Options::default();
    cfo.set_block_based_table_factory(&block);
    let dst = rocksdb::DB::open_cf_descriptors(
        &dopts,
        &dst_dir,
        vec![rocksdb::ColumnFamilyDescriptor::new(CF_TRIE, cfo)],
    )
    .with_context(|| format!("creating {dst_dir}"))?;
    let dst = Arc::new(dst);

    println!("copying {CF_TRIE}: {src_dir} -> {dst_dir} ({threads} threads)");
    let copied = Arc::new(AtomicU64::new(0));
    let bytes = Arc::new(AtomicU64::new(0));
    let started = Instant::now();

    // Trie keys are hashes, so they are uniform over the keyspace: splitting on
    // the leading byte gives ranges of near-equal size, and each thread can
    // iterate its own range with no coordination.
    std::thread::scope(|scope| -> Result<()> {
        let mut handles = Vec::new();
        for t in 0..threads {
            let (src, dst, copied, bytes) =
                (src.clone(), dst.clone(), copied.clone(), bytes.clone());
            let lo = (t * 256 / threads) as u8;
            let hi_excl = ((t + 1) * 256 / threads) as u16;
            handles.push(scope.spawn(move || -> Result<()> {
                let cf_s = src.cf_handle(CF_TRIE).context("source has no trie_nodes")?;
                let cf_d = dst.cf_handle(CF_TRIE).context("dest has no trie_nodes")?;
                let mut it = src.raw_iterator_cf(cf_s);
                it.seek(&[lo]);
                let mut batch = rocksdb::WriteBatch::default();
                let mut pending = 0usize;
                let mut pending_bytes = 0usize;
                while it.valid() {
                    let (Some(k), Some(v)) = (it.key(), it.value()) else { break };
                    if !k.is_empty() && (k[0] as u16) >= hi_excl {
                        break;
                    }
                    batch.put_cf(cf_d, k, v);
                    pending += 1;
                    pending_bytes += k.len() + v.len();
                    if pending_bytes >= 64 * 1024 * 1024 {
                        dst.write(std::mem::take(&mut batch)).context("write batch")?;
                        copied.fetch_add(pending as u64, Ordering::Relaxed);
                        bytes.fetch_add(pending_bytes as u64, Ordering::Relaxed);
                        pending = 0;
                        pending_bytes = 0;
                    }
                    it.next();
                }
                if pending > 0 {
                    dst.write(batch).context("final batch")?;
                    copied.fetch_add(pending as u64, Ordering::Relaxed);
                    bytes.fetch_add(pending_bytes as u64, Ordering::Relaxed);
                }
                Ok(())
            }));
        }

        // Progress from the main thread while the copy runs.
        let done = Instant::now();
        let _ = done;
        loop {
            std::thread::sleep(std::time::Duration::from_secs(30));
            if handles.iter().all(|h| h.is_finished()) {
                break;
            }
            let c = copied.load(Ordering::Relaxed);
            let b = bytes.load(Ordering::Relaxed);
            let s = started.elapsed().as_secs_f64().max(0.001);
            println!(
                "  {c} nodes, {:.1} GB, {:.0} nodes/s, {:.1} MB/s",
                b as f64 / 1e9, c as f64 / s, b as f64 / s / 1e6
            );
        }
        for h in handles {
            h.join().map_err(|_| anyhow::anyhow!("copy thread panicked"))??;
        }
        Ok(())
    })?;

    let c = copied.load(Ordering::Relaxed);
    let b = bytes.load(Ordering::Relaxed);
    let s = started.elapsed().as_secs_f64();
    println!("copied {c} nodes, {:.1} GB in {:.0}s ({:.0} nodes/s)", b as f64 / 1e9, s, c as f64 / s);

    println!("flushing and compacting");
    dst.flush().context("flush")?;
    dst.compact_range(None::<&[u8]>, None::<&[u8]>);
    println!("done. Start the node with:");
    println!("  --trie-backend external --trie-dir {dst_dir}");
    Ok(())
}
