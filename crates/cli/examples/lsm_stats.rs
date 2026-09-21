//! LSM shape and table properties of a trie store: level layout, and whether
//! the SSTs carry bloom filters at all.
//!
//! A filter block is written by the *writer* at flush and compaction time. A
//! reader cannot add one later, so a store written without a filter policy
//! makes every point lookup pay an index probe and a data-block read per
//! candidate file, for every reader, forever.
//!
//! Usage: lsm_stats <path> [<path>...]

use rocksdb::{Options, DB};

fn main() -> anyhow::Result<()> {
    for path in std::env::args().skip(1) {
        let mut opts = Options::default();
        opts.create_if_missing(false);
        let db = match DB::open_cf_for_read_only(&opts, &path, vec!["trie_nodes"], false) {
            Ok(d) => d,
            Err(e) => { println!("{path}: cannot open ({e})"); continue }
        };
        let cf = db.cf_handle("trie_nodes").unwrap();
        println!("== {path}");
        for p in ["rocksdb.levelstats", "rocksdb.aggregated-table-properties"] {
            if let Ok(Some(v)) = db.property_value_cf(cf, p) {
                if p.ends_with("levelstats") {
                    println!("{v}");
                } else {
                    for field in v.split(';') {
                        let f = field.trim();
                        if f.starts_with("# entries") || f.starts_with("filter size")
                            || f.starts_with("raw key size") || f.starts_with("data size")
                            || f.starts_with("index size") || f.starts_with("# data blocks") {
                            println!("  {f}");
                        }
                    }
                }
            }
        }
    }
    Ok(())
}
