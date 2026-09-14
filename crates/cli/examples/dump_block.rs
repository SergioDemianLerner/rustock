//! Writes a block's stored bytes to disk, for attaching to a bug report.
//!
//! Emits the header RLP, the body RLP, and a manifest of the header fields as
//! text. Opens the database read-only.
//!
//! ```text
//! dump_block /var/lib/rustock 9236893 /tmp/out
//! ```

use anyhow::{Context, Result};
use alloy_primitives::B256;

fn main() -> Result<()> {
    let mut a = std::env::args().skip(1);
    let dir = a.next().context("usage: dump_block <datadir> <block> <outdir>")?;
    let num: u64 = a.next().context("missing block")?.parse()?;
    let out = a.next().context("missing outdir")?;
    std::fs::create_dir_all(&out)?;

    let mut o = rocksdb::Options::default();
    o.create_if_missing(false);
    o.set_max_open_files(256);
    let cfs = ["headers", "block_numbers", "total_difficulty", "block_bodies",
               "receipts", "tx_index", "trie_nodes"];
    let db = rocksdb::DB::open_cf_for_read_only(&o, &dir, cfs, false)
        .with_context(|| format!("opening {dir}"))?;

    let cf_n = db.cf_handle("block_numbers").context("no block_numbers")?;
    let cf_h = db.cf_handle("headers").context("no headers")?;
    let cf_b = db.cf_handle("block_bodies").context("no block_bodies")?;

    let hash = db.get_cf(cf_n, num.to_be_bytes())?
        .ok_or_else(|| anyhow::anyhow!("no canonical block at #{num}"))?;
    let hash = B256::from_slice(&hash);

    let header = db.get_cf(cf_h, hash.as_slice())?
        .ok_or_else(|| anyhow::anyhow!("header missing"))?;
    std::fs::write(format!("{out}/block-{num}-header.rlp"), &header)?;
    println!("header  {} bytes -> block-{num}-header.rlp", header.len());

    match db.get_cf(cf_b, hash.as_slice())? {
        Some(body) => {
            std::fs::write(format!("{out}/block-{num}-body.rlp"), &body)?;
            println!("body    {} bytes -> block-{num}-body.rlp", body.len());
        }
        None => println!("body    absent"),
    }

    println!("hash    {hash:?}");
    Ok(())
}
