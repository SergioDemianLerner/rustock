//! Look up rskj's `stateRoots` translation table.
//!
//! When rskj migrated to the Unitrie at RSKIP126 it kept a mapping so that
//! pre-migration blocks could still be served: a block header below
//! wasabi100 carries an **Orchid** state root, which names no Unitrie node,
//! and this table translates it to the Unitrie root holding the same state.
//!
//! That is exactly what a replay of the early chain needs. Without it a
//! pre-wasabi100 header cannot address state in any Unitrie store, however
//! complete -- which looks identical to the state having been pruned.
//!
//! ```text
//! rskj_state_roots <stateRoots-dir> <0xhash>...
//! ```
//!
//! Prints `<queried> -> <mapped>` for each hit, nothing for a miss.

use anyhow::{Context, Result};
use rocksdb::{Options, DB};

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let dir = args.next().context("usage: rskj_state_roots <dir> <0xhash>...")?;
    let db = DB::open_for_read_only(&Options::default(), &dir, false)
        .with_context(|| format!("opening {dir} read-only"))?;

    let mut any = false;
    for a in args {
        let hex = a.strip_prefix("0x").unwrap_or(&a);
        let key = alloy_primitives::hex::decode(hex)
            .with_context(|| format!("{a} is not hex"))?;
        match db.get(&key)? {
            Some(v) => {
                any = true;
                println!("0x{hex} -> 0x{}", alloy_primitives::hex::encode(&v));
            }
            None => println!("0x{hex} -> (not present)"),
        }
    }
    if !any {
        // Said rather than left as silence: an empty result could mean the
        // table does not hold these keys, or that it is keyed the other way.
        eprintln!("no hits; sampling three entries to show how the table is keyed:");
        for (i, item) in db.iterator(rocksdb::IteratorMode::Start).enumerate().take(3) {
            let (k, v) = item?;
            eprintln!(
                "  key {} bytes 0x{}  ->  value {} bytes 0x{}",
                k.len(),
                alloy_primitives::hex::encode(&k),
                v.len(),
                alloy_primitives::hex::encode(&v)
            );
            let _ = i;
        }
    }
    Ok(())
}
