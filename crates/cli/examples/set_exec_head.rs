//! Rewinds (or advances) the executed head to a chosen canonical block.
//!
//! Needed when the trie backend is swapped. The epoch store starts seeded with
//! one state -- the one a snapshot was taken at -- while `exec_head` still
//! names whatever block the node last executed against the old store. Those
//! must agree, or the node comes up pointing at a state root its store cannot
//! resolve.
//!
//! Also the way back: reverting to the single backend requires rewinding
//! `exec_head` to a block the old store still has state for.
//!
//! The node must be stopped; RocksDB allows one writer.
//!
//! ```text
//! set_exec_head /var/lib/rustock 9234226
//! set_exec_head /var/lib/rustock 9234226 --dry-run
//! ```

use anyhow::{bail, Context, Result};
use rustock_storage::BlockStore;

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let dir = args.next().context("usage: set_exec_head <datadir> <block> [--dry-run]")?;
    let block: u64 = args.next().context("missing block number")?.parse()?;
    let dry = args.next().map(|a| a == "--dry-run").unwrap_or(false);

    let store = BlockStore::open(&dir).with_context(|| format!("opening {dir}"))?;

    let hash = store
        .canonical_hash(block)?
        .ok_or_else(|| anyhow::anyhow!("no canonical block at #{block}"))?;
    let header = store
        .header(hash)?
        .ok_or_else(|| anyhow::anyhow!("header missing for #{block} ({hash:?})"))?;

    if let Some((cur_hash, cur_root)) = store.exec_head()? {
        let cur_num = store.header(cur_hash)?.map(|h| h.number);
        println!("current exec head  #{cur_num:?} {cur_hash:?}");
        println!("  state root       {cur_root:?}");
    } else {
        println!("current exec head  (none)");
    }
    println!("new exec head      #{block} {hash:?}");
    println!("  state root       {:?}", header.state_root);

    // Refuse to point the node at a block whose header predates the Unitrie.
    // Before RSKIP126 the state_root field holds the legacy Orchid root, which
    // is not a trie node hash and will never resolve.
    if block < 1_591_000 {
        bail!("#{block} predates RSKIP126 (mainnet #1,591,000); its state root is not a Unitrie hash");
    }

    if dry {
        println!("\ndry run: nothing written");
        return Ok(());
    }

    store.set_exec_head(hash, header.state_root)?;
    println!("\nexec head set to #{block}");
    println!("The node will re-execute from here. Its trie store must hold the");
    println!("state for this block, or execution will halt on the first block after it.");
    Ok(())
}
