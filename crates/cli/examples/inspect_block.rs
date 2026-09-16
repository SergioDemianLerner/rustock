//! Execute one block against an archival trie and print what rustock computed:
//! per-transaction gas and status, block gas, paid fees, and every root. Used
//! to tell "we disagree about a fee" from "we disagree about what happened".
//!
//! Usage: inspect_block <block-dir> <archive-trie> <number>

use rustock_core::Block;
use rustock_execution::{BlockProcessor, RskHardforkConfig};
use rustock_storage::{BlockStore, RocksDbTrieStore};
use rustock_trie::{TrieNode, TrieStore};
use std::sync::Arc;

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(std::env::var("RUST_LOG").unwrap_or_else(|_| "info".into()))
        .with_target(true)
        .init();
    let a: Vec<String> = std::env::args().skip(1).collect();
    let store = Arc::new(BlockStore::open_read_only(&a[0])?);
    let trie: Arc<dyn TrieStore> = Arc::new(RocksDbTrieStore::open_read_only(&a[1])?);
    let n: u64 = a[2].parse()?;

    let hash = store.canonical_hash(n)?.expect("canonical");
    let header = store.header(hash)?.expect("header");
    let parent = store.header(header.parent_hash)?.expect("parent");
    let data = trie.get(parent.state_root.as_slice()).expect("parent state root");
    let root = TrieNode::from_message(&data, trie.as_ref());
    let (txs, oms) = store.body(hash)?.expect("body");
    let ntx = txs.len();
    let block = Block { header: header.clone(), transactions: txs, ommers: oms };

    let p = BlockProcessor::new(RskHardforkConfig::mainnet(), store.clone())
        .execute_block(&block, &root, trie.clone())?;

    println!("block #{n}  ({ntx} txs)");
    println!("  state root   computed {:?}", p.state_root_hash);
    println!("               header   {:?}   {}", header.state_root,
             if p.state_root_hash == header.state_root { "MATCH" } else { "*** DIFFER ***" });
    println!("  receipts root computed {:?}", p.receipts_root);
    println!("               header   {:?}   {}", header.receipts_root,
             if p.receipts_root == header.receipts_root { "MATCH" } else { "*** DIFFER ***" });
    println!("  gas used     computed {}  header {}   {}", p.gas_used, header.gas_used,
             if p.gas_used == header.gas_used { "MATCH" } else { "*** DIFFER ***" });
    println!("  paid fees    computed {}  header {}   {}", p.paid_fees, header.paid_fees,
             if p.paid_fees == header.paid_fees { "MATCH" } else { "*** DIFFER ***" });
    println!("\n  per-transaction receipts:");
    for (i, r) in p.receipts.iter().enumerate() {
        println!("    tx[{i}] status={} gas_used={} cumulative={} logs={}",
                 r.status, r.gas_used, r.cumulative_gas_used, r.logs.len());
    }
    Ok(())
}
