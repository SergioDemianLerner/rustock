//! Renders `eth_getUncleByBlockHashAndIndex` against the real mainnet store,
//! read-only, alongside the running node.
//!
//! Written to check the method on data the unit tests cannot supply: mainnet
//! uncle headers, with their merged-mining fields and their real hashes. It
//! also answers a question the implementation depends on -- whether this node
//! actually has the uncle's own block stored, which is the branch where rskj
//! returns the uncle's transactions instead of an empty block.
//!
//! ```text
//! cargo run --release --example uncle_rpc_check -- /var/lib/rustock 5000000 200
//! ```
//! Scans `count` canonical blocks downward from `start`, reporting every block
//! that has uncles.

use anyhow::{Context, Result};
use serde_json::json;

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let dir = args.next().context("usage: uncle_rpc_check <datadir> <start> [count]")?;
    let start: u64 = args.next().context("missing start height")?.parse()?;
    let count: u64 = args.next().unwrap_or_else(|| "200".into()).parse()?;

    let store = rustock_storage::BlockStore::open_read_only(&dir)
        .with_context(|| format!("opening {dir} read-only"))?;

    let mut with_uncles = 0u64;
    let mut uncles_seen = 0u64;
    let mut uncles_stored_as_blocks = 0u64;

    for n in (start.saturating_sub(count)..=start).rev() {
        let Some(hash) = store.canonical_hash(n)? else { continue };
        let Some(header) = store.header(hash)? else { continue };
        if header.uncle_count == 0 {
            continue;
        }
        with_uncles += 1;

        for idx in 0..header.uncle_count {
            let resp = rustock_rpc::eth::eth_get_uncle_by_block_hash_and_index(
                json!(1),
                &json!([format!("{:#x}", hash), format!("{:#x}", idx)]),
                &store,
            );
            let result = resp.result.context("no result")?;
            anyhow::ensure!(
                !result.is_null(),
                "block {n} claims {} uncles but index {idx} was null",
                header.uncle_count
            );
            uncles_seen += 1;

            let uncle_hash: alloy_primitives::B256 =
                result["hash"].as_str().unwrap().parse()?;
            let stored = store.body(uncle_hash)?.is_some();
            if stored {
                uncles_stored_as_blocks += 1;
            }
            if with_uncles <= 3 {
                println!(
                    "#{n} uncle[{idx}] {} number={} txs={} td={} size={} storedBody={}",
                    &result["hash"].as_str().unwrap()[..18],
                    result["number"],
                    result["transactions"].as_array().unwrap().len(),
                    result["totalDifficulty"],
                    result["size"],
                    stored,
                );
            }
        }

        // One past the end must be null, not an error.
        let resp = rustock_rpc::eth::eth_get_uncle_by_block_hash_and_index(
            json!(1),
            &json!([format!("{:#x}", hash), format!("{:#x}", header.uncle_count)]),
            &store,
        );
        anyhow::ensure!(resp.error.is_none(), "out-of-range index errored at #{n}");
        anyhow::ensure!(
            resp.result == Some(serde_json::Value::Null),
            "out-of-range index was not null at #{n}"
        );
    }

    println!(
        "\nscanned {count} blocks below #{start}: {with_uncles} with uncles, \
         {uncles_seen} uncles rendered, {uncles_stored_as_blocks} of which this \
         node also has as a stored block"
    );
    Ok(())
}
