use rustock_storage::BlockStore;
fn main() -> anyhow::Result<()> {
    let s = BlockStore::open_read_only("/mnt/import/tn-snapshot/vnode")?;
    let exec = 7139651u64;
    let processing = exec - 50; // testnet REMASC maturity
    let ph = s.header(s.canonical_hash(processing)?.unwrap())?.unwrap();
    println!("exec #{exec}, processing #{processing}");
    println!("  processing: hash {:?}\n              paid_fees {} uncle_count {}",
             ph.hash(), ph.paid_fees, ph.uncle_count);
    println!("  siblings (ommers with number == {processing}) found in:");
    for h in processing + 1..=exec {
        let Some(hash) = s.canonical_hash(h)? else { continue };
        let (_, om) = s.body(hash)?.unwrap_or_default();
        for o in &om {
            if o.number == processing {
                println!("    included at #{h}: hash {:?} paid_fees {} uncle_count {}",
                         o.hash(), o.paid_fees, o.uncle_count);
            }
        }
    }
    Ok(())
}
