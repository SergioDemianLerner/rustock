//! Prints stored block metadata for given heights, read-only.
//!
//! Opens the database alongside a running node, so it can be used to check what
//! is actually on disk without stopping anything. Written to confirm that
//! imported blocks carry a total difficulty in a form the node can read.
//!
//! ```text
//! cargo run --release --example block_info -- /var/lib/rustock 1000000 5000000
//! ```

use anyhow::{Context, Result};
use alloy_primitives::B256;

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let dir = args.next().context("usage: block_info <datadir> <height>...")?;
    let heights: Vec<u64> = args.filter_map(|a| a.parse().ok()).collect();

    let mut opts = rocksdb::Options::default();
    opts.create_if_missing(false);
    opts.set_max_open_files(256);
    let cfs = ["headers", "block_numbers", "total_difficulty", "block_bodies",
               "receipts", "tx_index", "trie_nodes"];
    let db = rocksdb::DB::open_cf_for_read_only(&opts, &dir, cfs, false)
        .with_context(|| format!("opening {dir} read-only"))?;

    let cf_num = db.cf_handle("block_numbers").context("no block_numbers cf")?;
    let cf_td = db.cf_handle("total_difficulty").context("no total_difficulty cf")?;
    let cf_hdr = db.cf_handle("headers").context("no headers cf")?;

    println!("{:>12}  {:<10} {:>6}  {:<22}  {}", "height", "hash", "td len", "td decoded", "header");
    for n in heights {
        let Some(hash) = db.get_cf(cf_num, n.to_be_bytes())? else {
            println!("{n:>12}  (no canonical hash)");
            continue;
        };
        let hash = B256::from_slice(&hash);
        let raw = db.get_cf(cf_td, hash.as_slice())?;
        let hdr_ok = db.get_cf(cf_hdr, hash.as_slice())?.is_some();
        match raw {
            None => println!("{n:>12}  {:<10} {:>6}  {:<22}  {}", &format!("{hash:?}")[2..10], "-", "absent", hdr_ok),
            Some(bytes) => {
                let decoded = rustock_storage::decode_td(&bytes)
                    .map(|v| v.to_string())
                    .unwrap_or_else(|e| format!("UNREADABLE: {e}"));
                println!(
                    "{n:>12}  {:<10} {:>6}  {:<22}  {}",
                    &format!("{hash:?}")[2..10],
                    bytes.len(),
                    decoded,
                    hdr_ok
                );
            }
        }
    }
    Ok(())
}
