//! Isolates read cost: one lookup in a large store versus N lookups across
//! small ones.
//!
//! The churn benchmark measured whole-block cost, which mixes reads, writes,
//! compaction and collection. This does nothing but read, so the epoch read
//! path can be compared against a single-database read path directly.
//!
//! Keys are sampled from what each store actually holds, then fetched in random
//! order. Both a warm run (page cache primed by the sampling pass) and a cold
//! run (page cache dropped) are reported, because the two answer different
//! questions: warm shows CPU and structural overhead, cold shows what happens
//! when the store does not fit in memory.

use anyhow::{Context, Result};
use rustock_storage::epoch_store::{EpochConfig, EpochTrieStore};
use rustock_trie::TrieStore;
use std::time::Instant;

fn sample_keys(dir: &str, want: usize) -> Result<Vec<Vec<u8>>> {
    let mut keys = Vec::with_capacity(want);
    let mut opts = rocksdb::Options::default();
    opts.create_if_missing(false);

    // An epoch store is a directory of databases; a single store is one.
    let mut dirs: Vec<std::path::PathBuf> = std::fs::read_dir(dir)?
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with("epoch-"))
        .map(|e| e.path())
        .collect();
    dirs.sort();
    if dirs.is_empty() {
        dirs.push(std::path::PathBuf::from(dir));
    }

    let per = want / dirs.len() + 1;
    for d in &dirs {
        let db = rocksdb::DB::open_cf_for_read_only(&opts, d, ["trie_nodes"], false)
            .with_context(|| format!("opening {}", d.display()))?;
        let cf = db.cf_handle("trie_nodes").context("no trie_nodes cf")?;
        let mut it = db.raw_iterator_cf(cf);
        it.seek_to_first();
        let mut n = 0;
        // Stride through rather than taking a prefix, so the sample is spread
        // across the keyspace instead of clustered at its start.
        let mut skip = 0;
        while it.valid() && n < per {
            if skip % 7 == 0 {
                if let Some(k) = it.key() {
                    if k.len() == 32 {
                        keys.push(k.to_vec());
                        n += 1;
                    }
                }
            }
            skip += 1;
            it.next();
        }
    }
    keys.truncate(want);
    Ok(keys)
}

fn shuffle(keys: &mut [Vec<u8>]) {
    // Deterministic xorshift, so both backends get the same access order shape.
    let mut s: u64 = 0x9E3779B97F4A7C15;
    for i in (1..keys.len()).rev() {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        keys.swap(i, (s % (i as u64 + 1)) as usize);
    }
}

fn time_reads(store: &dyn TrieStore, keys: &[Vec<u8>]) -> (f64, u64) {
    let t = Instant::now();
    let mut hits = 0u64;
    for k in keys {
        if store.get(k).is_some() {
            hits += 1;
        }
    }
    (t.elapsed().as_secs_f64(), hits)
}

fn drop_page_cache() {
    use std::io::Write;
    let _ = std::process::Command::new("sync").status();
    if let Ok(mut f) = std::fs::OpenOptions::new().write(true).open("/proc/sys/vm/drop_caches") {
        let _ = f.write_all(b"3");
    }
}

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let backend = args.next().unwrap_or_else(|| "single".into());
    let dir = args.next().unwrap_or_else(|| "/srv/gc-bench/single".into());
    let n: usize = args.next().and_then(|s| s.parse().ok()).unwrap_or(200_000);

    let mut keys = sample_keys(&dir, n)?;
    shuffle(&mut keys);
    println!("backend {backend}  dir {dir}  keys {}", keys.len());

    let store: Box<dyn TrieStore> = if backend == "epoch" {
        Box::new(EpochTrieStore::open(
            &dir,
            EpochConfig { epochs: 4, burial_depth: 0, rotate_bytes: u64::MAX },
        )?)
    } else {
        Box::new(rustock_storage::RocksDbTrieStore::open(std::path::Path::new(&dir))?)
    };

    // Warm: the sampling pass and this store's own open have primed the cache.
    let (warm, hits) = time_reads(store.as_ref(), &keys);
    println!(
        "  warm  {:>8.2} s  {:>10.0} reads/s  ({} hits)",
        warm, keys.len() as f64 / warm, hits
    );

    drop(store);
    drop_page_cache();

    let store: Box<dyn TrieStore> = if backend == "epoch" {
        Box::new(EpochTrieStore::open(
            &dir,
            EpochConfig { epochs: 4, burial_depth: 0, rotate_bytes: u64::MAX },
        )?)
    } else {
        Box::new(rustock_storage::RocksDbTrieStore::open(std::path::Path::new(&dir))?)
    };
    let (cold, hits) = time_reads(store.as_ref(), &keys);
    println!(
        "  cold  {:>8.2} s  {:>10.0} reads/s  ({} hits)",
        cold, keys.len() as f64 / cold, hits
    );
    Ok(())
}
