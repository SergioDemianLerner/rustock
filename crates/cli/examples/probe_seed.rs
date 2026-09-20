//! Is each segment chunk missing the parent state root of its own first block?
//!
//! Usage: probe_seed <block-dir> <segbuild-dir>

use rustock_storage::{BlockStore, RocksDbTrieStore};
use rustock_trie::TrieStore;

fn main() -> anyhow::Result<()> {
    let a: Vec<String> = std::env::args().collect();
    let (block_dir, seg_dir) = (a[1].clone(), a[2].clone());
    let blocks = BlockStore::open_read_only(&block_dir)?;

    let mut names: Vec<(u64, u64, u64, String)> = Vec::new();
    for e in std::fs::read_dir(&seg_dir)? {
        let n = e?.file_name().to_string_lossy().to_string();
        let p: Vec<&str> = n.split('-').collect();
        if p.len() == 3 {
            if let (Ok(i), Ok(s), Ok(x)) = (p[0].parse(), p[1].parse(), p[2].parse()) {
                names.push((i, s, x, n));
            }
        }
    }
    names.sort();

    let (mut missing, mut present) = (0u32, 0u32);
    for (idx, start, end, name) in &names {
        let sealed = format!("{seg_dir}/{name}/sealed");
        let store = RocksDbTrieStore::open_read_only(&sealed)?;
        // Parent state root of the chunk's first block.
        let (label, root) = if *start <= 1 {
            ("genesis".to_string(), None)
        } else {
            let h = blocks.header(blocks.canonical_hash(*start)?.expect("hash"))?.expect("header");
            let p = blocks.header(h.parent_hash)?.expect("parent");
            (format!("#{}", p.number), Some(p.state_root))
        };
        let root = match root {
            Some(r) => r,
            None => { println!("{idx:03} {start:>9}..{end:<9} genesis chunk, no seed"); continue }
        };
        let have_seed = store.get(root.as_slice()).is_some();
        // Also: is the root of the chunk's LAST block present? (control)
        let lh = blocks.header(blocks.canonical_hash(*end)?.expect("hash"))?.expect("header");
        let have_last = store.get(lh.state_root.as_slice()).is_some();
        if have_seed { present += 1 } else { missing += 1 }
        println!("{idx:03} {start:>9}..{end:<9} seed {label:>10} {:>7}   own last root {:>7}",
                 if have_seed { "PRESENT" } else { "MISSING" },
                 if have_last { "present" } else { "MISSING" });
    }
    println!("\nseed node missing in {missing} chunks, present in {present}");
    Ok(())
}
