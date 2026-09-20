//! What does a segment chunk NOT contain that a consumer needs?
//!
//! Two questions, both answered by measurement:
//!
//!   1. Replaying the chunk's own first blocks against the chunk alone: which
//!      keys have to come from the archive, and how many?
//!   2. For the precompile accounts, is the path present in the chunk at all?
//!
//! Usage: audit_chunk <block-dir> <chunk-dir> <archive-trie> <blocks>

use rustock_core::Block;
use rustock_execution::precompiles::all_rsk_precompile_addresses;
use rustock_execution::{BlockProcessor, RskHardforkConfig};
use rustock_storage::{BlockStore, RocksDbTrieStore};
use rustock_trie::{account_key, code_key, storage_key, TrieKeySlice, TrieNode, TrieStore};
use alloy_primitives::{Address, B256};

const BRIDGE: Address = Address::new([0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,1,0,0,6]);
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// Chunk first, archive second -- and every archive hit is recorded, because
/// each one is a node the chunk was supposed to hold and does not.
struct AuditStore {
    chunk: Arc<dyn TrieStore>,
    archive: Arc<dyn TrieStore>,
    from_chunk: AtomicU64,
    from_archive: AtomicU64,
    archive_keys: Mutex<Vec<String>>,
    record: bool,
}

/// The chunk alone. A key it does not hold is a miss, counted, not fetched.
struct ChunkOnly { inner: Arc<dyn TrieStore>, misses: AtomicU64 }

impl TrieStore for ChunkOnly {
    fn get(&self, key: &[u8]) -> Option<Vec<u8>> {
        let v = self.inner.get(key);
        if v.is_none() { self.misses.fetch_add(1, Ordering::Relaxed); }
        v
    }
    fn put(&self, _key: &[u8], _value: &[u8]) {}
}

impl TrieStore for AuditStore {
    fn get(&self, key: &[u8]) -> Option<Vec<u8>> {
        if let Some(v) = self.chunk.get(key) {
            self.from_chunk.fetch_add(1, Ordering::Relaxed);
            return Some(v);
        }
        let v = self.archive.get(key)?;
        self.from_archive.fetch_add(1, Ordering::Relaxed);
        if self.record {
            let mut k = self.archive_keys.lock().unwrap();
            if k.len() < 64 {
                k.push(key.iter().map(|b| format!("{b:02x}")).collect());
            }
        }
        Some(v)
    }
    fn put(&self, _key: &[u8], _value: &[u8]) {}
}

fn main() -> anyhow::Result<()> {
    let a: Vec<String> = std::env::args().collect();
    let (block_dir, chunk_dir, archive, nblocks) =
        (a[1].clone(), a[2].clone(), a[3].clone(), a[4].parse::<u64>()?);

    let name = std::path::Path::new(&chunk_dir)
        .file_name().unwrap().to_string_lossy().to_string();
    let p: Vec<&str> = name.split('-').collect();
    let start: u64 = p[1].parse()?;
    let end: u64 = p[2].parse()?;

    let blocks = Arc::new(BlockStore::open_read_only(&block_dir)?);
    let chunk: Arc<dyn TrieStore> = Arc::new(RocksDbTrieStore::open_read_only(
        &format!("{chunk_dir}/sealed"))?);
    let archive: Arc<dyn TrieStore> = Arc::new(RocksDbTrieStore::open_read_only(&archive)?);

    // ---- Question 2 first: precompile paths, chunk-only ----
    //
    // A chunk-only lookup returning None is ambiguous: the key may genuinely
    // be absent, or the walk may have died on a node the chunk does not hold.
    // Telling them apart is the whole point, so count the store misses the
    // walk incurred and compare the answer with the archive's.
    println!("== can the chunk ANSWER these lookups, or only guess? ==");
    println!("   archive = truth, chunk = what the chunk says, miss = nodes the walk could not load");
    for probe_block in [start, (start + end) / 2, end] {
        let h = blocks.header(blocks.canonical_hash(probe_block)?.expect("hash"))?.expect("hdr");
        let root_bytes = match chunk.get(h.state_root.as_slice()) {
            Some(d) => d,
            None => { println!("  #{probe_block}: own state root NOT in chunk"); continue }
        };
        println!("  -- at #{probe_block} --");
        let counting = Arc::new(AuditStore {
            chunk: chunk.clone(), archive: archive.clone(),
            from_chunk: AtomicU64::new(0), from_archive: AtomicU64::new(0),
            archive_keys: Mutex::new(Vec::new()), record: false,
        });
        let arch_root = TrieNode::from_message(
            &archive.get(h.state_root.as_slice()).expect("root in archive"), archive.as_ref());

        for addr in all_rsk_precompile_addresses() {
            let short = format!("{addr:?}");
            let short = short[short.len() - 8..].to_string();
            for (what, key) in [
                ("account", account_key(&addr)),
                ("code   ", code_key(&addr)),
                ("slot0  ", storage_key(&addr, &B256::ZERO)),
            ] {
                let truth = arch_root.get(&TrieKeySlice::from_key(&key), archive.as_ref());
                let before = counting.from_archive.load(Ordering::Relaxed);
                // chunk-only: a ChunkOnly store that reports misses instead of
                // falling back.
                let only = ChunkOnly { inner: chunk.clone(), misses: AtomicU64::new(0) };
                let croot = TrieNode::from_message(&root_bytes, &only);
                let got = croot.get(&TrieKeySlice::from_key(&key), &only);
                let _ = before;
                let miss = only.misses.load(Ordering::Relaxed);
                let agree = truth.is_some() == got.is_some();
                if miss > 0 || !agree {
                    println!("     {short} {what}  archive={:<7} chunk={:<7} miss={miss} {}",
                             if truth.is_some() { "present" } else { "absent" },
                             if got.is_some() { "present" } else { "absent" },
                             if agree { "(same answer, but unproven)" } else { "*** WRONG ANSWER ***" });
                }
            }
        }
        // A few Bridge storage cells, the case the rskj reader actually hit.
        for name in ["federationFormatVersion", "storageVersion", "lockingCap",
                     "btcBlockchainBestChainHeight", "newFederationBtcUTXOs"] {
            let slot = B256::from(rustock_execution::bridge::storage::bridge_storage_key(name));
            let key = storage_key(&BRIDGE, &slot);
            let truth = arch_root.get(&TrieKeySlice::from_key(&key), archive.as_ref());
            let only = ChunkOnly { inner: chunk.clone(), misses: AtomicU64::new(0) };
            let croot = TrieNode::from_message(&root_bytes, &only);
            let got = croot.get(&TrieKeySlice::from_key(&key), &only);
            let miss = only.misses.load(Ordering::Relaxed);
            let agree = truth.is_some() == got.is_some();
            if miss > 0 || !agree {
                println!("     bridge/{name:<28} archive={:<7} chunk={:<7} miss={miss} {}",
                         if truth.is_some() { "present" } else { "absent" },
                         if got.is_some() { "present" } else { "absent" },
                         if agree { "(same answer, but unproven)" } else { "*** WRONG ANSWER ***" });
            }
        }
    }

    // ---- Question 1: replay from the chunk's first block ----
    println!("\n== replaying #{start}..#{} against the chunk, archive as fallback ==", start + nblocks - 1);
    let audit = Arc::new(AuditStore {
        chunk: chunk.clone(), archive: archive.clone(),
        from_chunk: AtomicU64::new(0), from_archive: AtomicU64::new(0),
        archive_keys: Mutex::new(Vec::new()), record: true,
    });
    let store: Arc<dyn TrieStore> = audit.clone();

    let h = blocks.header(blocks.canonical_hash(start)?.expect("hash"))?.expect("hdr");
    let ph = blocks.header(h.parent_hash)?.expect("parent");
    let data = store.get(ph.state_root.as_slice()).expect("seed root");
    let mut root = TrieNode::from_message(&data, store.as_ref());

    let processor = BlockProcessor::new(RskHardforkConfig::mainnet(), blocks.clone());
    for n in start..start + nblocks {
        let before = audit.from_archive.load(Ordering::Relaxed);
        let hash = blocks.canonical_hash(n)?.expect("hash");
        let header = blocks.header(hash)?.expect("hdr");
        let (txs, oms) = blocks.body(hash)?.expect("body");
        let expected = header.state_root;
        let p = processor.execute_block(
            &Block { header, transactions: txs, ommers: oms }, &root, store.clone())?;
        if p.state_root_hash != expected {
            println!("  #{n}: STATE ROOT MISMATCH");
            break;
        }
        let after = audit.from_archive.load(Ordering::Relaxed);
        if after != before {
            println!("  #{n}: {} node(s) had to come from the archive", after - before);
        }
        root = p.new_state_root;
    }

    println!("\nfrom chunk   : {}", audit.from_chunk.load(Ordering::Relaxed));
    println!("from archive : {}", audit.from_archive.load(Ordering::Relaxed));
    for k in audit.archive_keys.lock().unwrap().iter() {
        println!("  archive-only key 0x{k}");
    }
    Ok(())
}
