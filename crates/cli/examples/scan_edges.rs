//! Where on this chain is RSKIP-144 parallel execution actually *used*?
//!
//! A range that merely sits above the activation height proves the rule is
//! enabled, not that any block exercises it. rskj emits the edges field on
//! every header once RSKIP-144 is active -- as an empty string when the block
//! has no sublists -- so "edges present" says nothing. Only a *non-empty*
//! edges field means the block's transactions were split across sublists and
//! executed in parallel.
//!
//! Scans canonical headers and reports every block with non-empty edges, so a
//! verification range can be chosen to cover blocks that use the feature
//! rather than blocks that merely could.
//!
//! Read-only.
//!
//! ```text
//! scan_edges <data-dir> <from> <to> [step]
//! ```

use anyhow::{Context, Result};
use rustock_storage::BlockStore;

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let usage = "usage: scan_edges <data-dir> <from> <to> [step]";
    let dir = args.next().context(usage)?;
    let from: u64 = args.next().context(usage)?.parse()?;
    let to: u64 = args.next().context(usage)?.parse()?;
    let step: u64 = args.next().map(|a| a.parse()).transpose()?.unwrap_or(1);

    let store = BlockStore::open_read_only(&dir)?;
    let mut scanned = 0u64;
    let mut versioned = 0u64;
    let mut with_edges = 0u64;
    let mut max_sublists = 0usize;

    let mut n = from;
    while n <= to {
        if let Some(hash) = store.canonical_hash(n)? {
            if let Some(h) = store.header(hash)? {
                scanned += 1;
                if let Some(v) = &h.version_fields {
                    versioned += 1;
                    if let Some(edges) = v.edges.as_ref().filter(|e| !e.is_empty()) {
                        with_edges += 1;
                        max_sublists = max_sublists.max(edges.len() + 1);
                        if with_edges <= 20 {
                            println!("#{n}  edges {edges:?}  ({} sublists)", edges.len() + 1);
                        }
                    }
                }
            }
        }
        n += step;
    }

    println!("\nscanned {scanned} canonical header(s) in #{from}..=#{to} (step {step})");
    println!("  versioned (RSKIP-351)      {versioned}");
    println!("  with NON-EMPTY edges       {with_edges}");
    if with_edges > 0 {
        println!("  widest split               {max_sublists} sublists");
    } else {
        println!("  -> parallel execution is active here but no block uses it");
    }
    Ok(())
}
