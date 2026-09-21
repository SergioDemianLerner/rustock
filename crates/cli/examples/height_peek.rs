//! Every block stored at a height, canonical or not, read-only.
//!
//! The canonical pointer names one hash per height; the height index names all
//! of them. When a node is wedged the difference is the diagnosis: a head whose
//! height also holds a sibling is a head that may be on the losing fork.
//!
//! Usage: height_peek <data-dir> <from> [to]

use rustock_storage::BlockStore;

fn main() -> anyhow::Result<()> {
    let a: Vec<String> = std::env::args().collect();
    let store = BlockStore::open_read_only(&a[1])?;
    let from: u64 = a[2].parse()?;
    let to: u64 = a.get(3).map(|s| s.parse()).transpose()?.unwrap_or(from);

    let exec = store.exec_head()?.map(|(h, _)| h);
    println!("executed head: {exec:?}");
    for n in from..=to {
        let canon = store.canonical_hash(n)?;
        let all = store.hashes_at_height(n)?;
        if all.is_empty() && canon.is_none() {
            println!("#{n}: nothing stored");
            continue;
        }
        println!("#{n}: canonical {}, {} stored at this height",
                 canon.map(|h| format!("{h:?}")).unwrap_or("none".into()), all.len());
        for h in &all {
            let hdr = store.header(*h)?;
            let mark = if Some(*h) == canon { " <- canonical" } else { "" };
            let body = if store.body(*h)?.is_some() { "body" } else { "HEADER ONLY" };
            match hdr {
                Some(x) => println!("    {h:?}  parent {:?}  {body}{mark}", x.parent_hash),
                None => println!("    {h:?}  (no header){mark}"),
            }
        }
    }
    Ok(())
}
