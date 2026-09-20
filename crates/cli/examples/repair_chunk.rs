//! Make a segment chunk answer the lookups a foreign client issues.
//!
//! The build captured exactly the nodes *rustock* read (copy-on-read in the
//! window store). A client whose read set is a superset -- rskj reads storage
//! version cells and precompile accounts that rustock never consults -- walks
//! off the captured subtree and gets a wrong answer, not an error, because a
//! trie walk that dies on a missing node is indistinguishable from one that
//! reaches a genuine dead end.
//!
//! This pass walks, at every state root the chunk's range needs, the paths to
//! that superset of keys, serving from the chunk and falling back to the
//! archival unitrie -- and writing every fallback hit into the chunk. It is
//! the same copy-on-read discipline, applied after the fact to the reads the
//! build never issued.
//!
//! Purely additive and content-addressed: it only ever inserts nodes under
//! their own hash, so it is safe to interrupt and idempotent to re-run.
//!
//! Usage: repair_chunk <block-dir> <chunk-dir> <archive-trie> [--limit N] [--dry-run]

use rustock_execution::bridge::storage::{bridge_storage_key, bridge_storage_key_long};
use rustock_execution::precompiles::all_rsk_precompile_addresses;
use rustock_storage::{BlockStore, RocksDbTrieStore};
use rustock_trie::{account_key, code_key, storage_key, TrieKeySlice, TrieNode, TrieStore};
use alloy_primitives::{Address, B256};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::collections::HashMap;
use std::time::Instant;

const BRIDGE: Address = Address::new([0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,1,0,0,6]);

/// Every Bridge storage cell rustock has a name for. rskj's
/// BridgeStorageProvider reads these; whether rustock did in a given era
/// depends on the hard fork, so the ones it skipped are exactly the gaps.
const BRIDGE_KEYS: &[&str] = &[
    "btcTxHashesAP", "releaseRequestQueue", "releaseTransactionSet",
    "releasesOutpointsValues", "rskTxsWaitingFS", "releaseRequestQueueWithTxHash",
    "releaseTransactionSetWithTxHash", "receiveHeadersLastTimestamp",
    "nextPegoutHeight", "lockingCap", "btcTxHashAP", "coinbaseInformation",
    "btcBlockHeight", "fastBridgeHashUsedInBtcTx", "fastBridgeFederationInformation",
    "pegoutTxSigHash", "svpFundTxHashUnsigned", "svpFundTxSigned",
    "svpSpendTxHashUnsigned", "svpSpendTxWaitingForSignatures", "lockWhitelist",
    "unlimitedLockWhitelist", "blockStoreChainHead", "feePerKb",
    "newFederationBtcUTXOs", "newFederationBtcUTXOsForTestnet",
    "newFedBtcUTXOsForTestnetPostHop", "oldFederationBtcUTXOs", "newFederation",
    "oldFederation", "pendingFederation", "proposedFederation", "federationElection",
    "activeFedCreationBlockHeight", "nextFedCreationBlockHeight",
    "lastRetiredFedP2SHScript", "newFederationFormatVersion",
    "oldFederationFormatVersion", "pendingFederationFormatVersion",
    "proposedFederationFormatVersion", "storageVersion",
    // Names rustock has no constant for, because it does not implement the
    // concept. They are exactly the reads a foreign client makes and this one
    // never did, so they are the likeliest gaps.
    "federationFormatVersion", "btcBlockchainBestChainHeight",
    "btcBlockchainBlockStore", "bridgeStorageVersion",
];

/// How many synthetic Bridge storage slots to walk alongside the named ones.
///
/// A storage key is `account || 0x00 || keccak(slot)[0:10] || slot`, so the
/// slot is spread uniformly by its hash. Walking N arbitrary slots therefore
/// descends the top ~log2(N) levels of the Bridge's storage subtree in every
/// direction, and that is what an absence proof for an *unenumerated* key
/// needs: the branch point where its path dies. It does not cover every key --
/// nothing short of the whole subtree would -- but it turns "this chunk cannot
/// answer" into "this chunk answers" for the great majority of them, at about
/// a tenth of the walk cost.
const SPREAD_SLOTS: u64 = 128;

/// Chunk first, archive second -- and every archive hit is written back, which
/// is the repair.
///
/// In front of both sits a plain map, because consecutive roots walk almost
/// exactly the same nodes: the paths to a fixed key set differ only near the
/// top of the trie, so of the ~3,600 nodes a root's walk touches, all but a
/// hundred or so are the same nodes the previous root touched. Measured
/// without it: 26 roots/s, and 92k RocksDB lookups per second doing nothing
/// but re-fetching what it just fetched.
struct PatchStore {
    chunk: RocksDbTrieStore,
    archive: Arc<dyn TrieStore>,
    cache: Mutex<HashMap<Vec<u8>, Vec<u8>>>,
    cache_cap: usize,
    added: AtomicU64,
    reads: AtomicU64,
    hits: AtomicU64,
    dry: bool,
    /// Which key is being walked, and how many nodes each key had to pull
    /// from the archive. Only meaningful in dry-run, where nothing is
    /// written and so every root re-reports the same gaps.
    current_key: AtomicU64,
    per_key: Mutex<HashMap<u64, u64>>,
}

impl TrieStore for PatchStore {
    fn get(&self, key: &[u8]) -> Option<Vec<u8>> {
        self.reads.fetch_add(1, Ordering::Relaxed);
        {
            let c = self.cache.lock().unwrap();
            if let Some(v) = c.get(key) {
                self.hits.fetch_add(1, Ordering::Relaxed);
                return Some(v.clone());
            }
        }
        let v = match self.chunk.get(key) {
            Some(v) => v,
            None => {
                let v = self.archive.get(key)?;
                self.added.fetch_add(1, Ordering::Relaxed);
                if self.dry {
                    *self.per_key.lock().unwrap()
                        .entry(self.current_key.load(Ordering::Relaxed)).or_insert(0) += 1;
                }
                if !self.dry {
                    self.chunk.put(key, &v);
                }
                v
            }
        };
        let mut c = self.cache.lock().unwrap();
        // Flat cap with a wholesale clear: the working set is small and
        // re-warms in one root, so an eviction policy would cost more to
        // maintain than it saves.
        if c.len() >= self.cache_cap {
            c.clear();
        }
        c.insert(key.to_vec(), v.clone());
        Some(v)
    }
    fn put(&self, _key: &[u8], _value: &[u8]) {}
}

fn keys_to_walk() -> Vec<Vec<u8>> {
    let mut keys: Vec<Vec<u8>> = Vec::new();
    for addr in all_rsk_precompile_addresses() {
        keys.push(account_key(&addr));
        keys.push(code_key(&addr));
        keys.push(storage_key(&addr, &B256::ZERO));
    }
    for n in BRIDGE_KEYS {
        keys.push(storage_key(&BRIDGE, &B256::from(bridge_storage_key(n))));
        keys.push(storage_key(&BRIDGE, &B256::from(bridge_storage_key_long(n))));
    }
    for i in 0..SPREAD_SLOTS {
        keys.push(storage_key(&BRIDGE, &B256::from(alloy_primitives::U256::from(i))));
    }
    // Sorted: consecutive walks then share their prefix in the block cache.
    keys.sort();
    keys.dedup();
    keys
}

/// Labels for `keys_to_walk`, in the same order after the same sort.
fn key_names() -> Vec<String> {
    let mut pairs: Vec<(Vec<u8>, String)> = Vec::new();
    for addr in all_rsk_precompile_addresses() {
        let a = format!("{addr:?}");
        let a = a[a.len() - 8..].to_string();
        pairs.push((account_key(&addr), format!("{a}/account")));
        pairs.push((code_key(&addr), format!("{a}/code")));
        pairs.push((storage_key(&addr, &B256::ZERO), format!("{a}/slot0")));
    }
    for n in BRIDGE_KEYS {
        pairs.push((storage_key(&BRIDGE, &B256::from(bridge_storage_key(n))), format!("bridge/{n}")));
        pairs.push((storage_key(&BRIDGE, &B256::from(bridge_storage_key_long(n))), format!("bridge/{n}#long")));
    }
    pairs.sort();
    pairs.dedup_by(|a, b| a.0 == b.0);
    pairs.into_iter().map(|(_, n)| n).collect()
}

/// Walk every path to `keys` from `node`, in one descent.
///
/// `TrieNode::get` restarts at the root for each key, so 103 keys that share
/// most of their prefix cost 103 full descents -- measured at 3,592 node reads
/// per root, against roughly 850 distinct nodes on the union of those paths.
/// This is the same logic as `TrieNode::find`, carrying the whole key set down
/// together and splitting it at each branch, so each node on the union is
/// visited once.
///
/// Nothing is returned: resolving a child is what pulls it through the patch
/// store, which is the entire point.
fn walk_all(node: &TrieNode, keys: &[TrieKeySlice], store: &dyn TrieStore) {
    let mut go_left: Vec<TrieKeySlice> = Vec::new();
    let mut go_right: Vec<TrieKeySlice> = Vec::new();
    for k in keys {
        if node.shared_path.length() > k.length() {
            continue;
        }
        let common = k.common_path(&node.shared_path);
        if common.length() < node.shared_path.length() {
            continue; // the key leaves the trie here; nothing below to fetch
        }
        if common.length() == k.length() {
            continue; // the key ends at this node
        }
        let rest = k.slice(common.length() + 1, k.length());
        if k.get(common.length()) == 0 { go_left.push(rest) } else { go_right.push(rest) }
    }
    if !go_left.is_empty() {
        if let Some(c) = node.left.resolve(store) { walk_all(&c, &go_left, store) }
    }
    if !go_right.is_empty() {
        if let Some(c) = node.right.resolve(store) { walk_all(&c, &go_right, store) }
    }
}

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let dry = args.iter().any(|a| a == "--dry-run");
    let stride: u64 = args.iter().position(|a| a == "--stride")
        .and_then(|i| args.get(i + 1)).and_then(|s| s.parse().ok()).unwrap_or(1);
    let limit: Option<u64> = args.iter().position(|a| a == "--limit")
        .and_then(|i| args.get(i + 1)).and_then(|s| s.parse().ok());
    let (block_dir, chunk_dir, archive_path) =
        (args[1].clone(), args[2].clone(), args[3].clone());

    let name = std::path::Path::new(&chunk_dir).file_name().unwrap().to_string_lossy().to_string();
    let p: Vec<&str> = name.split('-').collect();
    let (start, end): (u64, u64) = (p[1].parse()?, p[2].parse()?);

    let blocks = BlockStore::open_read_only(&block_dir)?;
    let archive: Arc<dyn TrieStore> = Arc::new(RocksDbTrieStore::open_read_only(&archive_path)?);
    let chunk = if dry {
        RocksDbTrieStore::open_read_only(&format!("{chunk_dir}/sealed"))?
    } else {
        RocksDbTrieStore::open(&format!("{chunk_dir}/sealed"))?
    };
    let cache_cap: usize = std::env::var("REPAIR_CACHE")
        .ok().and_then(|v| v.parse().ok()).unwrap_or(400_000);
    let store = PatchStore {
        chunk, archive,
        cache: Mutex::new(HashMap::with_capacity(cache_cap / 4)),
        cache_cap,
        added: AtomicU64::new(0), reads: AtomicU64::new(0), hits: AtomicU64::new(0), dry,
        current_key: AtomicU64::new(u64::MAX), per_key: Mutex::new(HashMap::new()),
    };

    let keys = keys_to_walk();
    let slices: Vec<TrieKeySlice> = keys.iter().map(|k| TrieKeySlice::from_key(k)).collect();
    let ckpt = format!("{chunk_dir}/repair.checkpoint");
    // A client executing block n reads at root(n-1), so the roots this range
    // needs are those of blocks start-1 ..= end.
    let first_root_block = if start == 0 { 0 } else { start - 1 };
    let ckpt = if stride > 1 { format!("{chunk_dir}/repair.checkpoint.s{stride}") } else { ckpt };
    let mut from = std::fs::read_to_string(&ckpt).ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .map(|n| n + stride).unwrap_or(first_root_block);
    if from < first_root_block { from = first_root_block }
    let last = limit.map(|l| (from + l - 1).min(end)).unwrap_or(end);

    eprintln!("[{name}] roots #{from}..#{last} stride {stride} ({} keys each){}",
              keys.len(), if dry { "  DRY RUN" } else { "" });
    let t0 = Instant::now();
    let total = (last.saturating_sub(from) / stride) + 1;
    let mut done = 0u64;
    let mut last_log = Instant::now();

    let mut n = from;
    while n <= last {
        let nn = n;
        n += stride;
        let hash = match blocks.canonical_hash(nn)? { Some(h) => h, None => break };
        let header = match blocks.header(hash)? { Some(h) => h, None => break };
        // The chunk must hold the root node itself: a consumer seeds from it.
        if let Some(msg) = store.get(header.state_root.as_slice()) {
            let root = TrieNode::from_message(&msg, &store);
            walk_all(&root, &slices, &store);
        }
        done += 1;
        if done % 2000 == 0 || nn + stride > last {
            if !dry { std::fs::write(&ckpt, nn.to_string())?; }
            if last_log.elapsed().as_secs() >= 30 || nn + stride > last {
                let rate = done as f64 / t0.elapsed().as_secs_f64();
                let left = (total - done) as f64 / rate.max(0.001);
                eprintln!("[{name}] {:.1}% #{nn} | {:.0} roots/s | added {} | ETA {:.0}m",
                          100.0 * done as f64 / total as f64, rate,
                          store.added.load(Ordering::Relaxed), left / 60.0);
                last_log = Instant::now();
            }
        }
    }

    store.chunk.flush();
    if dry {
        let names = key_names();
        let pk = store.per_key.lock().unwrap();
        let mut v: Vec<(&u64, &u64)> = pk.iter().collect();
        v.sort_by(|a, b| b.1.cmp(a.1));
        eprintln!("[{name}] keys that pulled nodes from the archive:");
        for (i, c) in v.iter().take(30) {
            eprintln!("    {:>7} nodes  {}", c, names.get(**i as usize).map(|s| s.as_str()).unwrap_or("?"));
        }
        eprintln!("    ({} of {} keys pulled nothing)", keys.len() - v.len(), keys.len());
    }
    let r = store.reads.load(Ordering::Relaxed);
    let h = store.hits.load(Ordering::Relaxed);
    eprintln!("[{name}] DONE in {:.1}m | roots {done} | reads {r} | cache {:.1}% | nodes added {}",
              t0.elapsed().as_secs_f64() / 60.0,
              if r > 0 { 100.0 * h as f64 / r as f64 } else { 0.0 },
              store.added.load(Ordering::Relaxed));
    Ok(())
}
