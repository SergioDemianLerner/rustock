//! How many nodes would it take to make a chunk answer the lookups it cannot?
//!
//! For each sampled block in the chunk, walk the ARCHIVE from that block's
//! state root to a set of keys a foreign client reads, recording every node the
//! walk touches, and count the ones the chunk does not hold.
//!
//! Usage: patch_size <block-dir> <chunk-dir> <archive-trie> <sample-every>

use rustock_execution::bridge::storage::bridge_storage_key;
use rustock_execution::precompiles::all_rsk_precompile_addresses;
use rustock_storage::{BlockStore, RocksDbTrieStore};
use rustock_trie::{account_key, code_key, storage_key, TrieKeySlice, TrieNode, TrieStore};
use alloy_primitives::{Address, B256};
use std::collections::HashSet;
use std::sync::{Arc, Mutex};

const BRIDGE: Address = Address::new([0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,1,0,0,6]);

/// Serves from the archive and remembers every key it served.
struct Recorder { inner: Arc<dyn TrieStore>, seen: Mutex<HashSet<Vec<u8>>> }

impl TrieStore for Recorder {
    fn get(&self, key: &[u8]) -> Option<Vec<u8>> {
        let v = self.inner.get(key)?;
        self.seen.lock().unwrap().insert(key.to_vec());
        Some(v)
    }
    fn put(&self, _k: &[u8], _v: &[u8]) {}
}

fn main() -> anyhow::Result<()> {
    let a: Vec<String> = std::env::args().collect();
    let (block_dir, chunk_dir, archive_path, step) =
        (a[1].clone(), a[2].clone(), a[3].clone(), a[4].parse::<u64>()?);

    let name = std::path::Path::new(&chunk_dir).file_name().unwrap().to_string_lossy().to_string();
    let p: Vec<&str> = name.split('-').collect();
    let (start, end): (u64, u64) = (p[1].parse()?, p[2].parse()?);

    let blocks = BlockStore::open_read_only(&block_dir)?;
    let chunk: Arc<dyn TrieStore> = Arc::new(RocksDbTrieStore::open_read_only(&format!("{chunk_dir}/sealed"))?);
    let archive: Arc<dyn TrieStore> = Arc::new(RocksDbTrieStore::open_read_only(&archive_path)?);

    // The key set a foreign client reads and rustock does not.
    let mut keys: Vec<Vec<u8>> = Vec::new();
    for addr in all_rsk_precompile_addresses() {
        keys.push(account_key(&addr));
        keys.push(code_key(&addr));
        keys.push(storage_key(&addr, &B256::ZERO));
    }
    for n in ["storageVersion", "federationFormatVersion", "lockingCap",
              "btcBlockchainBestChainHeight", "newFederationBtcUTXOs",
              "pendingFederationFormatVersion", "receiveHeadersLastTimestamp"] {
        keys.push(storage_key(&BRIDGE, &B256::from(bridge_storage_key(n))));
    }

    let rec = Recorder { inner: archive.clone(), seen: Mutex::new(HashSet::new()) };
    let mut sampled = 0u64;
    let mut n = start;
    while n <= end {
        let h = blocks.header(blocks.canonical_hash(n)?.expect("hash"))?.expect("hdr");
        if let Some(rb) = archive.get(h.state_root.as_slice()) {
            rec.seen.lock().unwrap().insert(h.state_root.to_vec());
            let root = TrieNode::from_message(&rb, &rec);
            for k in &keys {
                let _ = root.get(&TrieKeySlice::from_key(k), &rec);
            }
            sampled += 1;
        }
        n += step;
    }

    let seen = rec.seen.lock().unwrap();
    let mut missing = 0usize;
    let mut missing_bytes = 0usize;
    for k in seen.iter() {
        if chunk.get(k).is_none() {
            missing += 1;
            missing_bytes += archive.get(k).map(|v| v.len()).unwrap_or(0) + k.len();
        }
    }
    let per = if sampled > 0 { missing as f64 / sampled as f64 } else { 0.0 };
    println!("chunk {name}");
    println!("  sampled roots      : {sampled}  (every {step} blocks over {}..{})", start, end);
    println!("  distinct nodes the walks touched : {}", seen.len());
    println!("  of those, absent from the chunk  : {missing}  ({:.1} KB)", missing_bytes as f64 / 1024.0);
    println!("  new nodes per sampled root       : {per:.2}");
    Ok(())
}
