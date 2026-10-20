//! What a FlatDB store actually holds: record sizes, and how many cross
//! the threshold that decides whether a key is stored or recomputed.
//!
//! Two questions this answers that nothing else does.
//!
//! `AVERAGE_RECORD_BYTES` sizes every index, and it was derived from the
//! synthetic fixture — all account leaves, all short. Real state is not
//! that, so the divisor deserves checking against a real store before it
//! is trusted to keep the rehash from happening.
//!
//! And the fixture contains no record over 1,024 bytes at all, which
//! means the stored-key path, the long-record verification and the
//! threshold itself have never run on anything real.
//!
//! ```text
//! flatdb_stats <dir> [--expected-keys N]
//! ```

use anyhow::{Context, Result};
use rustock_flatdb::{FlatDb, Options, STORE_KEY_THRESHOLD};

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let dir = args.next().context("usage: flatdb_stats <dir> [--expected-keys N]")?;
    let mut expected_keys = 13_421_772u64;
    let rest: Vec<String> = args.collect();
    let mut i = 0;
    while i + 1 < rest.len() {
        if rest[i] == "--expected-keys" {
            expected_keys = rest[i + 1].parse()?;
        }
        i += 2;
    }

    let db = FlatDb::open(
        &dir,
        Options { expected_keys, read_only: true, ..Options::default() },
    )?;

    // Powers of two, which is enough resolution to see where the mass is
    // and exactly where the threshold falls.
    const BUCKETS: usize = 20;
    let mut counts = [0u64; BUCKETS];
    let mut bytes = [0u64; BUCKETS];
    let mut total = 0u64;
    let mut total_bytes = 0u64;
    let mut long_records = 0u64;
    let mut longest = 0usize;
    // Keys of the records that store their key, so they can be read back
    // through the verifying path.
    let mut long_keys: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();

    db.for_each_record(&mut |key, data| {
        let _key = key;
        let len = data.len();
        let bucket = (usize::BITS - len.max(1).leading_zeros()) as usize;
        let bucket = bucket.min(BUCKETS - 1);
        counts[bucket] += 1;
        bytes[bucket] += len as u64;
        total += 1;
        total_bytes += len as u64;
        if len > STORE_KEY_THRESHOLD {
            long_records += 1;
            long_keys.push((_key.to_vec(), data.to_vec()));
        }
        longest = longest.max(len);
    });

    println!("{dir}");
    println!("records        {total}");
    println!("value bytes    {:.2} GB", total_bytes as f64 / 1e9);
    println!();
    println!("{:>12}  {:>11}  {:>7}  {:>11}", "value bytes", "records", "share", "mean");
    for (b, count) in counts.iter().enumerate() {
        if *count == 0 {
            continue;
        }
        let low = if b == 0 { 0 } else { 1usize << (b - 1) };
        println!(
            "{:>5}-{:<6}  {count:>11}  {:>6.1}%  {:>11.0}",
            low,
            (1usize << b) - 1,
            100.0 * *count as f64 / total as f64,
            bytes[b] as f64 / *count as f64,
        );
    }

    println!();
    println!("longest        {longest} bytes");
    println!(
        "over {STORE_KEY_THRESHOLD:>4}      {long_records} ({:.3}%) -- these store their key \
         and are verified on the hit",
        100.0 * long_records as f64 / total.max(1) as f64
    );

    // The figure that sizes every index. On disk a record costs its value
    // plus two magic bytes, a length field, and a key when it has one.
    let overhead: u64 = total * 3 + long_records * (32 + 1);
    let on_disk = total_bytes + overhead;
    println!();
    println!(
        "mean record    {:.1} bytes on disk ({:.1} of value)",
        on_disk as f64 / total.max(1) as f64,
        total_bytes as f64 / total.max(1) as f64
    );
    println!(
        "AVERAGE_RECORD_BYTES is 80; keys per 1 GB epoch at that divisor is {}, \
         at the measured mean it is {}",
        (1u64 << 30) / 80,
        (1u64 << 30) / (on_disk / total.max(1)).max(1)
    );

    // A walk of the trie follows children, and a long value hangs off a
    // node's value hash rather than being one -- so descending the trie
    // never reads these. Reading them back here is what exercises the
    // stored-key path and the verification that rides on it (#316)
    // against real contract code, which no fixture contains.
    if !long_keys.is_empty() {
        let started = std::time::Instant::now();
        let mut matched = 0u64;
        let mut wrong = 0u64;
        for (key, expected) in &long_keys {
            match db.get(key) {
                Some(got) if got == *expected => matched += 1,
                _ => wrong += 1,
            }
        }
        println!();
        println!(
            "read back      {matched} of {} long records verified, {wrong} failed, {:.0} ms",
            long_keys.len(),
            started.elapsed().as_secs_f64() * 1000.0
        );
        anyhow::ensure!(wrong == 0, "{wrong} long records did not read back intact");
    }
    Ok(())
}
