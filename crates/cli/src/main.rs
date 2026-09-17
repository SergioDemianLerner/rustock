use clap::Parser;
use rustock_core::config::ChainConfig;
use rustock_core::validation::HeaderVerifier;
use rustock_storage::BlockStore;
use rustock_networking::node::{Node, NodeConfig};
use rustock_sync::{SyncManager, SyncHandler, SyncService, TxRelay};
use rustock_trie::{AccountState, TrieKeySlice, TrieNode, TrieStore, account_key};
use std::sync::Arc;
use alloy_primitives::U256;
use anyhow::{Result, Context};
use tracing::{debug, error, info};
use tracing_subscriber::EnvFilter;

/// Reads block bodies from a synced rskj `blocks` LevelDB (keyed by block
/// hash → RLP[header, txs, ommers]) so the node can reuse an existing local
/// chain instead of re-downloading bodies from peers.
struct RskjBlockSource {
    db: rocksdb::DB,
}

impl RskjBlockSource {
    fn open(path: &str) -> Result<Self> {
        let mut opts = rocksdb::Options::default();
        let mut bbt = rocksdb::BlockBasedOptions::default();
        bbt.set_block_cache(&rocksdb::Cache::new_lru_cache(512 * 1024 * 1024));
        opts.set_block_based_table_factory(&bbt);
        let db = rocksdb::DB::open_for_read_only(&opts, path, false)
            .context("Failed to open rskj blocks LevelDB")?;
        Ok(Self { db })
    }
}

impl rustock_sync::ExternalBlockSource for RskjBlockSource {
    fn body_by_hash(
        &self,
        hash: alloy_primitives::B256,
    ) -> Option<(Vec<rustock_core::Transaction>, Vec<rustock_core::types::header::Header>)> {
        use alloy_rlp::Decodable;
        let data = self.db.get(hash.as_slice()).ok().flatten()?;
        let block = rustock_core::Block::decode(&mut &data[..]).ok()?;
        Some((block.transactions, block.ommers))
    }
}

struct TxRelaySubmitter(Arc<TxRelay>);

#[async_trait::async_trait]
impl rustock_rpc::server::TxSubmitter for TxRelaySubmitter {
    async fn submit_transaction(&self, raw_tx: alloy_primitives::Bytes) -> Result<alloy_primitives::B256, String> {
        self.0.submit_transaction(raw_tx).await
    }
}

struct PoolAdapter(Arc<rustock_sync::TransactionPool>);

impl rustock_rpc::server::TxPoolReader for PoolAdapter {
    fn get_pending_tx(
        &self,
        hash: &alloy_primitives::B256,
    ) -> Option<(rustock_core::Transaction, alloy_primitives::Address, alloy_primitives::B256)> {
        let ptx = self.0.get(hash)?;
        Some((ptx.tx, ptx.sender, ptx.hash))
    }

    fn pending_nonce(&self, addr: &alloy_primitives::Address) -> Option<u64> {
        self.0.pending_nonce(addr)
    }

    fn pool_status(&self) -> (usize, usize) {
        self.0.status()
    }
}

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Port to listen for P2P connections
    #[arg(short, long, default_value_t = 30303)]
    port: u16,

    /// Data directory
    #[arg(short, long, default_value = "./data")]
    data_dir: String,

    /// Network ID (30 for mainnet, 33 for regtest)
    #[arg(long, default_value = "30")]
    network_id: u64,

    /// Secret key for the P2P node (hex). If not provided, a random one will be used.
    #[arg(long)]
    secret_key: Option<String>,

    /// Run a one-off repair against the database and exit, instead of starting
    /// the node. The node must be stopped: RocksDB allows a single writer.
    ///
    /// Tasks:
    ///   tx-index   rebuild the transaction index from block bodies, so
    ///              eth_getTransactionByHash and eth_getTransactionReceipt can
    ///              find transactions in blocks that were imported rather than
    ///              executed. Touches no trie and executes nothing.
    #[arg(long, value_name = "TASK")]
    repair: Option<String>,

    /// First block for --repair (default 1).
    #[arg(long)]
    repair_from: Option<u64>,

    /// Last block for --repair (default: the chain head).
    #[arg(long)]
    repair_to: Option<u64>,

    /// Log level: trace, debug, info, warn, error.
    /// Can also use RUST_LOG-style directives, e.g. "info,rustock_sync=debug".
    #[arg(long, default_value = "info")]
    log_level: String,

    /// Log to stdout instead of a file. By default logs are written to
    /// <data-dir>/rustock.log with automatic daily rotation.
    #[arg(long, default_value_t = false)]
    log_to_stdout: bool,

    /// JSON-RPC server port
    #[arg(long, default_value_t = 4444)]
    rpc_port: u16,

    /// JSON-RPC server bind address
    #[arg(long, default_value = "127.0.0.1")]
    rpc_host: String,

    /// Disable the JSON-RPC server
    #[arg(long, default_value_t = false)]
    no_rpc: bool,

    /// External IP address to advertise to peers (e.g., 203.0.113.42).
    /// If not set, 127.0.0.1 is used.
    #[arg(long)]
    external_ip: Option<std::net::IpAddr>,

    /// Path to a synced rskj `blocks` LevelDB (e.g. ~/.rsk/mainnet/database/blocks).
    /// When set, block bodies are read from it before requesting them from
    /// peers — reusing an existing local chain instead of re-downloading.
    #[arg(long)]
    import_blocks_db: Option<String>,

    /// Target number of outbound peer connections to maintain (rskj's
    /// `maxActivePeers` default is 30). Higher values give more headroom to
    /// absorb dead/unreachable nodes and recover faster after a network blip.
    #[arg(long, default_value_t = 25)]
    max_peers: usize,

    /// Maximum simultaneous inbound peer connections. Bounds the work an
    /// attacker can force by opening connections: each one costs an ECIES
    /// handshake (secp256k1 ECDH + Keccak) plus per-connection buffers.
    #[arg(long, default_value_t = 32)]
    max_inbound_peers: usize,

    /// Maximum simultaneous inbound connections from a single IP address, so
    /// one host cannot occupy every inbound slot on its own.
    #[arg(long, default_value_t = 4)]
    max_inbound_per_ip: usize,

    /// Maximum simultaneous inbound connections from a single network block.
    /// A per-IP cap alone is sidestepped by anyone holding a subnet, so this
    /// bounds the whole block as well (rskj's peer.filter.maxConnections).
    #[arg(long, default_value_t = 16)]
    max_inbound_per_cidr: usize,

    /// IPv4 prefix length defining a network block for --max-inbound-per-cidr.
    #[arg(long, default_value_t = 24)]
    inbound_cidr_prefix: u8,

    /// Import an extracted rskj database, then exit.
    ///
    /// Expects the directory holding rskj's datasource subdirectories, e.g.
    /// `/mnt/import/extract/database/mainnet` (containing `blocks/` and
    /// `unitrie/`). The two schemas differ structurally, so this converts
    /// rather than copies; see rustock_storage::rskj_import for the details.
    /// Blocks already present are skipped, so an import over a partially
    /// synced datadir fast-forwards.
    #[arg(long)]
    import_rskj: Option<String>,

    /// Write per-phase import metrics to this CSV, for comparing runs.
    #[arg(long)]
    import_metrics: Option<String>,

    /// During import, look up each entry and skip ones already held.
    ///
    /// Off by default. Both datasources are content-addressed, so rewriting an
    /// entry we already hold writes identical bytes -- the check is a
    /// performance trade, not a correctness one. It costs one point lookup per
    /// entry (~700M of them for the state trie), which only pays off when the
    /// destination already holds most of the source. Use it when topping up a
    /// nearly-complete datadir; leave it off for a fresh or mostly-fresh one.
    #[arg(long)]
    import_skip_existing: bool,

    /// Route import writes through the write-ahead log.
    ///
    /// Off by default, which is the right choice: the WAL exists to recover
    /// unflushed writes after a crash, but a failed import is re-run from the
    /// source rather than repaired. Enabling it writes every entry twice.
    #[arg(long)]
    import_enable_wal: bool,

    /// Memtable size during import, in MiB. Larger means fewer, larger flushes
    /// and less write amplification, at the cost of dirty data held in RAM.
    #[arg(long, default_value_t = 256)]
    import_write_buffer_mb: usize,

    /// Leave auto-compaction enabled during import.
    ///
    /// Off by default: compacting while the load is still running repeatedly
    /// rewrites data that is about to change. Disabled, the import does one
    /// compaction pass at the end instead.
    #[arg(long)]
    import_auto_compaction: bool,

    /// Path to a TOML configuration file for peg-out monitoring and alerting.
    ///
    /// Entirely optional: without it no watcher runs and nothing changes. The
    /// watcher only reads blocks, receipts and Bridge state the node has
    /// already committed, so it cannot affect consensus or block processing.
    /// See `docs/pegout-alerts.md` and `pegout-alerts.example.toml`.
    #[arg(long)]
    pegout_alerts_config: Option<String>,

    /// Background flush/compaction threads during import. 0 uses the CPU count.
    #[arg(long, default_value_t = 0)]
    import_parallelism: usize,

    /// Threads for the trie copy. 0 uses the CPU count; 1 forces the sequential
    /// path.
    ///
    /// Parallel by default because the copy is CPU-bound, not disk-bound: it
    /// sustained 48.6 MB/s against a volume measured at 358 MB/s sequential.
    /// Trie keys are hashes, so the keyspace splits into equal contiguous
    /// ranges and the threads share nothing but the destination handle.
    #[arg(long, default_value_t = 0)]
    import_unitrie_threads: usize,

    /// Probe an rskj unitrie for specific node hashes, then exit.
    ///
    /// Answers whether a snapshot is archival or pruned: pass historical state
    /// roots and see whether their nodes are present. That determines whether
    /// blocks can be re-executed in parallel (every pre-state already on disk)
    /// or only sequentially.
    #[arg(long, num_args = 1.., value_delimiter = ',')]
    probe_unitrie: Vec<String>,

    /// Directory holding the rskj unitrie to probe.
    #[arg(long)]
    probe_source: Option<String>,

    /// Skip the trie and block phases and rebuild only the chain metadata.
    ///
    /// Useful when those phases already completed and only the derived
    /// canonical chain and total difficulty are missing -- rerunning the whole
    /// import to redo them would rewrite ~180 GB for nothing.
    #[arg(long)]
    import_metadata_only: bool,

    /// Rebuild chain metadata for this block range only, then exit.
    ///
    /// For benchmarking the metadata phase without paying for a full pass --
    /// the full rebuild took 5 hours on RSK mainnet, which is far too slow a
    /// loop for trying optimisations. Requires a completed full pass, since it
    /// seeds itself from the existing canonical hash and total difficulty at
    /// the range boundaries. Non-destructive: it recomputes exactly the values
    /// already stored.
    #[arg(long, num_args = 2, value_names = ["FROM", "TO"])]
    metadata_range: Vec<u64>,

    /// Load chain metadata from a dump of rskj's MapDB index (the fast path).
    ///
    /// Produced by tools/dump-index. Reads canonical hash and cumulative
    /// difficulty straight from what rskj already computed, instead of deriving
    /// them by walking parentHash back from the tip -- a chain of 9.2M dependent
    /// random lookups measured at ~1,065 blocks/s, roughly five hours.
    ///
    /// Pass the dump sorted by block number so writes land in key order.
    #[arg(long)]
    import_index_dump: Option<String>,

    /// Derive chain metadata by walking parentHash on disk.
    ///
    /// The original method, kept for comparison and for cases where memory is
    /// too tight for --metadata-in-memory. Slow: the walk is a chain of
    /// dependent random lookups that cannot be batched, measured at ~1,065
    /// blocks/s, roughly five hours for RSK mainnet.
    #[arg(long)]
    metadata_by_walk: bool,

    /// Rebuild chain metadata using an in-memory header index (the fast path).
    ///
    /// Scans the headers column family sequentially into a hash -> (number,
    /// parent, difficulty) map, walks the chain through RAM, then writes in
    /// batches. Costs roughly 150 bytes per header -- about 2 GB for RSK
    /// mainnet -- and needs no JVM or rskj index file.
    #[arg(long)]
    metadata_in_memory: bool,
    /// Scan the Unitrie reachable from a state root and print statistics, then
    /// exit. Defaults to the last executed block; pass --trie-tool-block for
    /// another. Opens the database read-only, so it can run while the node does.
    #[arg(long)]
    trie_tool: bool,

    /// Block whose state root to scan with --trie-tool.
    #[arg(long)]
    trie_tool_block: Option<u64>,

    /// Seconds between progress lines during --trie-tool. 0 disables.
    ///
    /// Wall-clock rather than a node count: the node rate varies by orders of
    /// magnitude with cache warmth, so a count-based interval reports either
    /// constantly or almost never. A scan silent for twenty minutes is also
    /// indistinguishable from one that has hung, which is how an earlier
    /// quadratic version of this scan went unnoticed.
    #[arg(long, default_value_t = 30)]
    trie_tool_progress: u64,

    /// Database to scan or inspect. Defaults to --data-dir.
    ///
    /// Accepts either a node datadir or a standalone trie snapshot written by
    /// --trie-tool-copy. A snapshot is recognised by its metadata file and
    /// carries its own state root, so no block lookup is needed -- or possible,
    /// since a snapshot holds no headers.
    #[arg(long)]
    trie_tool_source: Option<String>,

    /// Copy the scanned state into a new standalone trie database.
    ///
    /// The scan already reads every live node, so extracting them costs the
    /// write and nothing more. The result is under a gigabyte against a 129 GB
    /// source, and is immutable, which makes re-running the statistics a matter
    /// of minutes instead of the better part of an hour -- and makes repeated
    /// runs comparable, since the live store keeps moving as the node syncs.
    ///
    /// Pass a directory, or give the flag alone to name it after the state root.
    #[arg(long, num_args = 0..=1, default_missing_value = "")]
    trie_tool_copy: Option<String>,

    /// Delete the copy target first if it already exists.
    ///
    /// Off by default. Every key in a trie database is a hash, so a snapshot
    /// written over unrelated data would neither collide nor complain -- it
    /// would just quietly carry whatever was there before.
    #[arg(long)]
    trie_tool_copy_overwrite: bool,

    /// Directory for a detached trie store.
    ///
    /// Used by --trie-backend external and epoch. Defaults to
    /// <data-dir>/trie-<backend>. Pointing this at different directories is how
    /// one switches between trie stores -- a full archival one and a collected
    /// one, say -- without touching headers, bodies or the chain index.
    #[arg(long)]
    trie_dir: Option<String>,

    /// Trie storage backend: "single" (in the main database), "external" (its
    /// own database, no collection) or "epoch" (its own databases, collected).
    ///
    /// Defaults to single, which is what the node has always done: one
    /// database, every historical version retained, unbounded growth. The epoch
    /// backend bounds the size by periodically reclaiming state older than the
    /// retention window -- which means historical state queries below that
    /// window stop working. That is a real trade, so it is opt-in.
    #[arg(long, default_value = "single")]
    trie_backend: String,

    /// Epochs to keep with --trie-backend epoch. Minimum 3.
    ///
    /// More epochs means smaller, more frequent sweeps and less duplication,
    /// at the cost of one more place a read miss has to look.
    #[arg(long, default_value_t = 4)]
    gc_epochs: usize,

    /// Rotate the newest epoch once it reaches this many MB.
    #[arg(long, default_value_t = 1024)]
    gc_rotate_mb: u64,

    /// Blocks a state root must be buried by before it is collected against.
    ///
    /// Must exceed the deepest reorganisation worth surviving: collecting
    /// against a root that is later reorganised away discards the state the
    /// chain needs to rebuild.
    #[arg(long, default_value_t = 4_000)]
    gc_burial: u64,

    /// Enable administrative JSON-RPC methods.
    ///
    /// Off by default. These act on the node rather than answering questions
    /// about the chain -- rsk_collectTrie deletes a database -- and the RPC
    /// server binding to localhost is a deployment detail, not an access
    /// control decision. With this off the methods report as unknown, so a node
    /// without them is indistinguishable from one that never had them.
    #[arg(long)]
    rpc_admin: bool,

    /// Seconds between checks of whether the store needs collecting.
    #[arg(long, default_value_t = 60)]
    gc_check_secs: u64,

    /// Print everything about the node at this path, then exit.
    ///
    /// Path notation is <hex>/<bits>, e.g. "a3f0/13"; bare hex means four bits
    /// per digit. Reads --trie-tool-source. Paths printed by --trie-tool can
    /// be passed straight back in.
    #[arg(long)]
    trie_node: Option<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(&args.log_level));

    std::fs::create_dir_all(&args.data_dir).context("Failed to create data directory")?;

    let _guard = if args.log_to_stdout {
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_target(false)
            .init();
        None
    } else {
        let file_appender = tracing_appender::rolling::daily(&args.data_dir, "rustock.log");
        let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);

        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_target(false)
            .with_ansi(false)
            .with_writer(non_blocking)
            .init();

        let log_path = std::path::Path::new(&args.data_dir).join("rustock.log");
        eprintln!("Logging to {}", log_path.display());
        eprintln!("Use --log-to-stdout to log to the console instead.");
        eprintln!("Tail the log: tail -f {}", log_path.display());

        Some(guard)
    };

    info!("Starting Rustock on port {}...", args.port);

    let config = match args.network_id {
        30 => ChainConfig::mainnet(),
        31 => ChainConfig::testnet(),
        _ => ChainConfig::regtest(),
    };
    let config = Arc::new(config);
    // Block-hash computation (RSKIP92) needs the activation heights before
    // any header is decoded.
    config.activation_heights.clone().install();

    if let Some(task) = args.repair.clone() {
        let store = Arc::new(BlockStore::open(&args.data_dir)?);
        match task.as_str() {
            "tx-index" => run_repair_tx_index(&store, args.repair_from, args.repair_to)?,
            other => anyhow::bail!("unknown repair task {other:?}; known tasks: tx-index"),
        }
        return Ok(());
    }

    if args.metadata_in_memory {
        let store = Arc::new(BlockStore::open(&args.data_dir)?);
        rustock_storage::rskj_import::install_signal_handlers();
        let wal = if args.import_enable_wal {
            rustock_storage::rskj_import::WalMode::Enabled
        } else {
            rustock_storage::rskj_import::WalMode::Disabled
        };
        let mut csv = match args.import_metrics.as_deref() {
            Some(p) => Some(std::fs::OpenOptions::new().create(true).append(true).open(p)?),
            None => None,
        };
        let tip = rustock_storage::rskj_import::rebuild_chain_metadata_in_memory(
            &store,
            wal,
            csv.as_mut().map(|f| f as &mut dyn std::io::Write),
        )?;
        info!("Chain metadata rebuilt in memory, head #{tip}");
        return Ok(());
    }

    if let Some(dump) = args.import_index_dump.clone() {
        let store = Arc::new(BlockStore::open(&args.data_dir)?);
        rustock_storage::rskj_import::install_signal_handlers();
        let wal = if args.import_enable_wal {
            rustock_storage::rskj_import::WalMode::Enabled
        } else {
            rustock_storage::rskj_import::WalMode::Disabled
        };
        let mut csv = match args.import_metrics.as_deref() {
            Some(p) => Some(std::fs::OpenOptions::new().create(true).append(true).open(p)?),
            None => None,
        };
        let tip = rustock_storage::rskj_import::load_metadata_from_dump(
            &store,
            std::path::Path::new(&dump),
            wal,
            csv.as_mut().map(|f| f as &mut dyn std::io::Write),
        )?;
        info!("Chain metadata loaded from index dump, head #{tip}");
        return Ok(());
    }

    if !args.metadata_range.is_empty() {
        if args.metadata_range.len() != 2 {
            anyhow::bail!("--metadata-range takes exactly two values: FROM TO");
        }
        let (from, to) = (args.metadata_range[0], args.metadata_range[1]);
        let store = Arc::new(BlockStore::open(&args.data_dir)?);
        rustock_storage::rskj_import::install_signal_handlers();
        let mut csv = match args.import_metrics.as_deref() {
            Some(p) => Some(std::fs::OpenOptions::new().create(true).append(true).open(p)?),
            None => None,
        };
        rustock_storage::rskj_import::rebuild_chain_metadata_range(
            &store,
            from,
            to,
            csv.as_mut().map(|f| f as &mut dyn std::io::Write),
        )?;
        return Ok(());
    }

    if !args.probe_unitrie.is_empty() {
        let dir = args.probe_source.clone()
            .ok_or_else(|| anyhow::anyhow!("--probe-unitrie needs --probe-source"))?;
        let mut opts = rocksdb::Options::default();
        opts.create_if_missing(false);
        opts.set_max_open_files(256);
        let path = std::path::Path::new(&dir).join("unitrie");
        let db = rocksdb::DB::open_for_read_only(&opts, &path, false)?;
        for hexkey in &args.probe_unitrie {
            let clean = hexkey.trim_start_matches("0x");
            let key = match hex::decode(clean) {
                Ok(k) => k,
                Err(e) => { println!("{hexkey}: BAD HEX ({e})"); continue; }
            };
            match db.get(&key)? {
                Some(v) => println!("{hexkey}: PRESENT ({} bytes)", v.len()),
                None => println!("{hexkey}: ABSENT"),
            }
        }
        return Ok(());
    }

    if let Some(src) = args.import_rskj.clone() {
        let parallelism = if args.import_parallelism == 0 {
            std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4)
        } else {
            args.import_parallelism
        } as i32;
        let tuning = rustock_storage::ImportTuning {
            write_buffer_mb: args.import_write_buffer_mb,
            parallelism,
            auto_compaction: args.import_auto_compaction,
        };
        info!("Import tuning: {tuning:?}");
        // Opened with import settings rather than the node's: these trades are
        // safe only because a failed import is re-run, not repaired.
        let store = Arc::new(BlockStore::open_for_import(&args.data_dir, tuning)?);
        return run_rskj_import(
            &src,
            &store,
            args.import_metrics.as_deref(),
            args.import_skip_existing,
            !args.import_enable_wal,
            !args.import_auto_compaction,
            args.import_metadata_only,
            if args.import_unitrie_threads == 0 {
                std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4)
            } else {
                args.import_unitrie_threads
            },
        );
    }

    if let Some(path) = args.trie_node.as_deref() {
        let src = args.trie_tool_source.as_deref().unwrap_or(&args.data_dir);
        return run_trie_node(src, args.trie_tool_block, path);
    }

    if args.trie_tool || args.trie_tool_block.is_some() || args.trie_tool_copy.is_some() {
        let src = args.trie_tool_source.as_deref().unwrap_or(&args.data_dir);
        return run_trie_tool(
            src,
            args.trie_tool_block,
            args.trie_tool_progress,
            args.trie_tool_copy.as_deref(),
            args.trie_tool_copy_overwrite,
        );
    }

    let store = Arc::new(BlockStore::open(&args.data_dir)?);

    let genesis_hash = setup_genesis(&store, &config)?;
    info!("Genesis Hash: {:?}", genesis_hash);

    let verifier = Arc::new(HeaderVerifier::default_rsk(config.clone()));

    let key_path = std::path::Path::new(&args.data_dir).join("node.key");

    let secret_key_bytes = if let Some(hex_key) = args.secret_key {
        let mut bytes = [0u8; 32];
        hex::decode_to_slice(hex_key, &mut bytes).context("Invalid hex for secret key")?;
        bytes
    } else if key_path.exists() {
        let hex_key = std::fs::read_to_string(&key_path).context("Failed to read node.key")?;
        let mut bytes = [0u8; 32];
        hex::decode_to_slice(hex_key.trim(), &mut bytes).context("Invalid hex in node.key")?;
        info!("Loaded existing node identity from {:?}", key_path);
        bytes
    } else {
        use rand::RngCore;
        let mut bytes = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut bytes);

        std::fs::create_dir_all(&args.data_dir).context("Failed to create data directory")?;
        std::fs::write(&key_path, hex::encode(bytes)).context("Failed to save node.key")?;

        info!("Generated and saved new node identity to {:?}", key_path);
        bytes
    };

    let signing_key = k256::ecdsa::SigningKey::from_slice(&secret_key_bytes)?;
    let verifying_key = signing_key.verifying_key();
    let encoded_point = verifying_key.to_encoded_point(false);
    let node_id = alloy_primitives::B512::from_slice(&encoded_point.as_bytes()[1..]);

    let (best_hash, best_td, best_number) = if let Some(head_hash) = store.head()? {
        let td = store.total_difficulty(head_hash)?.unwrap_or(U256::ZERO);
        let number = store.header(head_hash)?
            .map(|h| h.number)
            .unwrap_or(0);
        (head_hash, td, number)
    } else {
        (genesis_hash, U256::ZERO, 0)
    };

    let node_config = NodeConfig {
        client_id: "Rustock/0.1.0".to_string(),
        listen_port: args.port,
        id: node_id,
        chain_id: config.chain_id,
        network_id: config.network_id,
        genesis_hash,
        best_hash,
        best_block_number: best_number,
        total_difficulty: best_td,
        bootnodes: config.bootnodes(),
        secret_key: secret_key_bytes,
        discovery_port: args.port + 1,
        data_dir: args.data_dir.clone(),
        external_ip: args.external_ip,
        max_outbound_peers: args.max_peers,
        max_inbound_peers: args.max_inbound_peers,
        max_inbound_per_ip: args.max_inbound_per_ip,
        max_inbound_per_cidr: args.max_inbound_per_cidr,
        inbound_cidr_prefix: args.inbound_cidr_prefix,
    };

    let peer_store = Arc::new(rustock_networking::peers::PeerStore::new());
    let sync_manager = Arc::new(SyncManager::new(store.clone(), verifier, peer_store.clone()));

    let (event_tx, event_rx) = tokio::sync::mpsc::unbounded_channel();
    let sync_handler = Arc::new(SyncHandler::new(sync_manager.clone(), event_tx));
    let gc_config = rustock_storage::epoch_store::EpochConfig {
        epochs: args.gc_epochs,
        burial_depth: args.gc_burial,
        rotate_bytes: args.gc_rotate_mb * (1 << 20),
    };
    let backend = rustock_storage::epoch_store::TrieBackend::parse(&args.trie_backend, gc_config)?;

    // The node only ever holds an `Arc<dyn TrieStore>`, so this is the entire
    // extent of the choice: nothing downstream knows which store it got.
    let trie_dir = args.trie_dir.clone().unwrap_or_else(|| {
        let suffix = if backend.is_collecting() { "trie-epochs" } else { "trie" };
        std::path::Path::new(&args.data_dir).join(suffix).to_string_lossy().into_owned()
    });
    // Always say where the trie is. With three backends and a migration in the
    // field, "which trie is this node actually reading?" is the first question
    // worth answering in any log, and the answer should not depend on which
    // backend happens to be selected.
    match &backend {
        rustock_storage::epoch_store::TrieBackend::Single => info!(
            "Trie store: single backend (trie_nodes column family inside {})",
            args.data_dir
        ),
        rustock_storage::epoch_store::TrieBackend::External => {
            info!("Trie store: external backend at {trie_dir} (no collection)")
        }
        rustock_storage::epoch_store::TrieBackend::Epochs(cfg) => info!(
            "Trie store: epoch backend at {trie_dir} (collecting; N={}, rotate at {} MB, burial {} blocks)",
            cfg.epochs,
            cfg.rotate_bytes / (1 << 20),
            cfg.burial_depth
        ),
    }

    // Refuse configurations that would come up with no state instead of failing.
    //
    // RocksDB is asked to create missing column families, so opening a database
    // whose trie was moved out recreates `trie_nodes` empty and the node starts
    // against a blank trie -- no error, no missing file, just a node that cannot
    // execute anything and looks like it needs a resync. That is worth stopping
    // for; the operator can always choose a backend, but they cannot recover
    // state that was quietly declared absent.
    {
        let has_chain = store.exec_head()?.is_some() || store.head()?.is_some();
        let detached_dir_populated = std::path::Path::new(&trie_dir)
            .read_dir()
            .map(|mut d| d.next().is_some())
            .unwrap_or(false);

        if !backend.is_detached() && has_chain && store.trie_cf_is_empty() {
            anyhow::bail!(
                "This database has no trie in it, but --trie-backend is \"{}\".\n\
                 \n\
                 Its trie column family is empty, which means the trie was moved \
                 into its own database (see examples/detach_trie.rs) and this \
                 column family was recreated empty on open. Starting like this \
                 would run the node against a blank trie.\n\
                 \n\
                 Start it against the detached trie instead:\n\
                 \x20   --trie-backend external --trie-dir <path-to-trie-db>\n\
                 \x20   --trie-backend epoch    --trie-dir <path-to-epoch-store>",
                args.trie_backend
            );
        }

        if backend.is_detached() && has_chain && !detached_dir_populated && !store.trie_cf_is_empty() {
            anyhow::bail!(
                "--trie-backend {} points at {}, which is empty, while this \
                 database still holds its own trie.\n\
                 \n\
                 Starting would create an empty trie store and the node would be \
                 unable to execute. Move the trie out first:\n\
                 \n\
                 \x20   cargo run --release --example detach_trie -- {} {}\n\
                 \n\
                 then start with the same --trie-dir. The source database is \
                 opened read-only and is not modified.",
                args.trie_backend, trie_dir, args.data_dir, trie_dir
            );
        }
    }

    let epoch_store: Option<Arc<rustock_storage::epoch_store::EpochTrieStore>> = match &backend {
        rustock_storage::epoch_store::TrieBackend::Epochs(cfg) => {
            Some(Arc::new(rustock_storage::epoch_store::EpochTrieStore::open(
                &trie_dir,
                cfg.clone(),
            )?))
        }
        _ => None,
    };
    let external_store: Option<Arc<dyn rustock_trie::TrieStore>> = match &backend {
        rustock_storage::epoch_store::TrieBackend::External => Some(Arc::new(
            rustock_storage::RocksDbTrieStore::open(std::path::Path::new(&trie_dir))?,
        )),
        _ => None,
    };
    let trie_store_for_exec: Arc<dyn rustock_trie::TrieStore> = match &epoch_store {
        // No write cache in front of the epoch store yet: CachedTrieStore wraps
        // a RocksDB handle rather than a TrieStore, so it cannot sit on top of
        // this one without a refactor. Execution therefore pays a real read and
        // write per node here, which is a known cost of turning the collector on.
        Some(e) => e.clone(),
        None => match &external_store {
            Some(e) => e.clone(),
            None => Arc::new(rustock_storage::CachedTrieStore::with_defaults(store.db().clone())),
        },
    };

    // The pool and RPC read state too. With a detached trie they must read the
    // same store as execution, or they answer from a trie the node is no longer
    // writing to.
    let detached_for_readers: Option<Arc<dyn rustock_trie::TrieStore>> =
        if backend.is_detached() { Some(trie_store_for_exec.clone()) } else { None };


    let trie_store_for_pool: Arc<dyn rustock_trie::TrieStore> = match &detached_for_readers {
        Some(t) => t.clone(),
        None => Arc::new(rustock_storage::RocksDbTrieStore::from_db(store.db().clone())),
    };
    let pool = Arc::new(rustock_sync::TransactionPool::new(
        rustock_sync::txpool::PoolConfig::default(),
        config.chain_id.into(),
        store.clone(),
        trie_store_for_pool.clone(),
    ));

    let hardfork_cfg = rustock_execution::RskHardforkConfig::for_network(config.chain_id as u64);
    let block_processor = rustock_execution::BlockProcessor::new(
        hardfork_cfg.clone(),
        store.clone(),
    );
    info!("Block processor wired (hardfork config: {:?})", hardfork_cfg);

    let initial_state_root = load_or_build_state(&store, &config, trie_store_for_exec.as_ref())?;
    info!("Initial state root: {:?}", initial_state_root.compute_hash(trie_store_for_exec.as_ref()));

    // Background collection. Kept out of the block-processing path on purpose:
    // mark and drain only read the oldest epoch and write the newest, and block
    // processing also writes the newest, so the two need no coordination beyond
    // the brief lock the sweep takes.
    if let Some(es) = epoch_store.clone() {
        let store_for_gc = store.clone();
        let burial = args.gc_burial;
        let every = std::time::Duration::from_secs(args.gc_check_secs.max(1));
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(every).await;
                if !es.should_collect() {
                    continue;
                }
                // Collect against a buried block, never the head. Collecting
                // against the head would leave nothing to fall back to if the
                // chain reorganised past it.
                let root = (|| {
                    let head_hash = store_for_gc.head().ok().flatten()?;
                    let head = store_for_gc.header(head_hash).ok().flatten()?;
                    let target = head.number.checked_sub(burial)?;
                    let hash = store_for_gc.canonical_hash(target).ok().flatten()?;
                    let header = store_for_gc.header(hash).ok().flatten()?;
                    Some((header.state_root, target))
                })();
                let Some((root, at)) = root else {
                    debug!(target: "rustock::gc", "No block buried {burial} deep yet; not collecting");
                    continue;
                };
                let head_now = store_for_gc
                    .head()
                    .ok()
                    .flatten()
                    .and_then(|h| store_for_gc.header(h).ok().flatten())
                    .map(|h| h.number)
                    .unwrap_or(at);
                // A root the store cannot resolve is normal for a while after
                // seeding: the burial depth reaches below the seeded block until
                // the chain advances past it. That is a "not yet", not a fault,
                // and logging it as an error once a minute would bury the real
                // ones.
                if rustock_trie::TrieStore::get(es.as_ref(), root.as_slice()).is_none() {
                    debug!(
                        target: "rustock::gc",
                        "Collection root #{at} is not in the store yet; waiting for the \
                         chain to advance past the seeded block"
                    );
                    continue;
                }
                info!(target: "rustock::gc", "Collecting against #{at} ({root:?})");
                if let Err(e) = es.collect(root, at, head_now) {
                    error!(target: "rustock::gc", "Collection failed: {e:?}");
                }
            }
        });
    }

    let mut sync_service = SyncService::new(sync_manager.clone(), peer_store.clone(), event_rx)
        .with_tx_pool(pool.clone())
        .with_block_processor(block_processor, trie_store_for_exec, initial_state_root);
    if let Some(path) = &args.import_blocks_db {
        let source = RskjBlockSource::open(path)?;
        info!("Sourcing block bodies from rskj LevelDB at {path} (peers only for what it lacks)");
        sync_service = sync_service.with_block_source(Arc::new(source));
    }
    let tx_relay = Arc::new(TxRelay::with_pool(peer_store.clone(), pool.clone()));

    let mut node = Node::with_peer_store(node_config, peer_store.clone());
    node.add_handler(sync_handler);
    node.add_handler(tx_relay.clone());

    // Peg-out monitoring. Deliberately not wired into execution or sync: it
    // polls what has already been committed, so a slow mail server or a bug in
    // it cannot delay a block.
    if let Some(path) = &args.pegout_alerts_config {
        match start_pegout_alerts(path, store.clone(), trie_store_for_pool.clone()) {
            Ok(Some(handle)) => {
                info!("Peg-out alert watcher started from {path}");
                let _ = handle;
            }
            Ok(None) => info!("Peg-out alerts configured but disabled in {path}"),
            // A malformed alert configuration must not stop the node: it is an
            // observability feature, and a node that refuses to start because
            // its mail settings are wrong is worse than one that runs unwatched.
            Err(e) => error!("Peg-out alerts disabled — {path}: {e:#}"),
        }
    }

    // Supervise the sync task: a panic inside it would otherwise be swallowed
    // by tokio and leave the node running but silently stalled.
    let sync_handle = tokio::spawn(sync_service.start());
    tokio::spawn(async move {
        if let Err(e) = sync_handle.await {
            tracing::error!("Sync service terminated unexpectedly: {e}. Shutting down.");
            std::process::exit(1);
        }
    });

    if !args.no_rpc {
        let trie_store: Arc<dyn rustock_trie::TrieStore> = match &detached_for_readers {
            Some(t) => t.clone(),
            None => Arc::new(rustock_storage::RocksDbTrieStore::from_db(store.db().clone())),
        };

        let rpc_state = rustock_rpc::server::RpcState {
            store: store.clone(),
            peer_store: peer_store.clone(),
            config: config.clone(),
            tx_submitter: Some(Arc::new(TxRelaySubmitter(tx_relay.clone()))),
            trie_store: Some(trie_store),
            hardfork_cfg: Some(hardfork_cfg),
            filter_store: Arc::new(rustock_rpc::logs::FilterStore::new()),
            tx_pool: Some(Arc::new(PoolAdapter(pool.clone()))),

            epoch_store: epoch_store.clone(),
            admin_enabled: args.rpc_admin,
            gc_burial: args.gc_burial,
        };
        let rpc_host = args.rpc_host.clone();
        let rpc_port = args.rpc_port;
        tokio::spawn(async move {
            if let Err(e) = rustock_rpc::server::start_rpc_server(&rpc_host, rpc_port, rpc_state).await {
                tracing::error!("RPC server error: {}", e);
            }
        });
    }

    node.start().await?;

    Ok(())
}

/// Try to resume from persisted state, falling back to genesis if unavailable.
///
/// On restart, loads the Unitrie root recorded at the last EXECUTED block
/// (`exec_head`). The header's state_root cannot be used: before RSKIP126 it
/// holds the legacy Orchid root, and the download head may be far past the
/// executed head. Children resolve lazily via `NodeRef::Hash` lookups.
fn load_or_build_state(
    store: &BlockStore,
    config: &ChainConfig,
    trie_store: &dyn TrieStore,
) -> Result<TrieNode> {
    if let Some((exec_hash, state_root)) = store.exec_head()? {
        if let Some(data) = trie_store.get(state_root.as_slice()) {
            let root = TrieNode::from_message(&data, trie_store);
            let verified = root.compute_hash(trie_store);
            if verified == state_root {
                let number = store.header(exec_hash)?.map(|h| h.number).unwrap_or(0);
                info!(
                    "Resuming from executed block #{} (state root: {:?})",
                    number, state_root
                );
                return Ok(root);
            }
            tracing::warn!(
                "Exec head state root verification failed: computed={verified:?}, \
                 expected={state_root:?}. Rebuilding from genesis."
            );
        } else {
            tracing::warn!(
                "Exec head state root {:?} not found in trie store. \
                 Rebuilding from genesis.",
                state_root
            );
        }
    }

    build_genesis_state(config, trie_store)
}

/// Build the genesis state trie from the chain config's alloc entries.
///
/// Each alloc entry becomes an account in the Unitrie. RSKj's mainnet
/// genesis only includes the Bridge balance (no storage). The genesis
/// header state root uses a pre-RSKIP126 format that doesn't match
/// our Unitrie hash, so we skip the comparison.
fn build_genesis_state(config: &ChainConfig, trie_store: &dyn TrieStore) -> Result<TrieNode> {
    let alloc = config.genesis_alloc();
    if alloc.is_empty() {
        return Ok(TrieNode::empty());
    }

    let mut root = TrieNode::empty();

    for entry in &alloc {
        let key_bytes = account_key(&entry.address);
        let key = TrieKeySlice::from_key(&key_bytes);
        let acct = AccountState::new(entry.nonce, entry.balance);
        root = root.put(&key, &acct.encode(), trie_store);
    }

    let computed = root.compute_hash(trie_store);
    info!("Genesis Unitrie hash: {computed:?}");

    root.save(trie_store, true);
    Ok(root)
}

fn setup_genesis(store: &BlockStore, config: &ChainConfig) -> Result<alloy_primitives::B256> {
    if let Some(genesis) = store.canonical_hash(0)? {
        return Ok(genesis);
    }

    let genesis = config.genesis_header();
    let hash = config.known_genesis_hash().unwrap_or_else(|| genesis.hash());

    store.put_header_with_hash(hash, &genesis)?;
    store.put_total_difficulty(hash, genesis.difficulty)?;
    store.put_canonical_hash(0, hash)?;
    store.set_head(hash)?;
    Ok(hash)
}

/// Runs an rskj database import and returns; the node is not started.
///
/// Order matters: state first, then blocks, then the derived chain metadata.
/// If the import is interrupted, re-running it resumes -- every phase skips
/// what is already present.
fn run_rskj_import(
    src: &str,
    store: &std::sync::Arc<BlockStore>,
    metrics_path: Option<&str>,
    skip_existing: bool,
    disable_wal: bool,
    compact_first: bool,
    metadata_only: bool,
    unitrie_threads: usize,
) -> Result<()> {
    use rustock_storage::rskj_import::{self, WalMode, WriteMode};
    use std::path::Path;

    // Catch SIGTERM/SIGINT so a stop request flushes rather than discarding
    // memtables. Import writes bypass the WAL, so an uncaught termination
    // silently loses data that a phase has already reported as written.
    rskj_import::install_signal_handlers();
    info!("Signal handlers installed: SIGTERM/SIGINT will flush and stop cleanly (never use SIGKILL)");

    let mode = if skip_existing { WriteMode::SkipExisting } else { WriteMode::Unconditional };
    let wal = if disable_wal { WalMode::Disabled } else { WalMode::Enabled };
    info!("Import write mode: {mode:?}, WAL: {wal:?}");

    let src = Path::new(src);
    if !src.join("blocks").is_dir() || !src.join("unitrie").is_dir() {
        anyhow::bail!(
            "{} does not look like an rskj database directory (expected blocks/ and unitrie/ inside)",
            src.display()
        );
    }

    let mut csv: Option<std::fs::File> = match metrics_path {
        Some(p) => {
            let exists = Path::new(p).exists();
            let mut f = std::fs::OpenOptions::new().create(true).append(true).open(p)?;
            if !exists {
                use std::io::Write;
                writeln!(f, "phase,scanned,written,skipped,bytes,elapsed_secs")?;
            }
            Some(f)
        }
        None => None,
    };

    let started = std::time::Instant::now();
    info!("Importing rskj database from {}", src.display());

    if metadata_only {
        info!("Metadata-only run: skipping the trie and block phases");
    }

    // State first: if this is interrupted the blocks are still absent, so the
    // node will not come up believing it has a chain it cannot execute.
    let unitrie_phase = if metadata_only {
        Default::default()
    } else if unitrie_threads == 1 {
        rskj_import::import_unitrie(
            src,
            store.db(),
            mode,
            wal,
            csv.as_mut().map(|f| f as &mut dyn std::io::Write),
        )?
    } else {
        rskj_import::import_unitrie_parallel(
            src,
            store.db(),
            mode,
            wal,
            unitrie_threads,
            csv.as_mut().map(|f| f as &mut dyn std::io::Write),
        )?
    };

    let block_stats = if metadata_only {
        Default::default()
    } else {
        rskj_import::import_blocks(
        src,
        store,
        mode,
        wal,
        csv.as_mut().map(|f| f as &mut dyn std::io::Write),
    )?
    };

    // Compact BEFORE the metadata rebuild, not after.
    //
    // The two write phases above leave the database as a deep stack of
    // un-merged L0 files when auto-compaction is disabled. The metadata rebuild
    // that follows is read-heavy -- roughly 18.5M random header lookups over
    // 9.2M blocks -- and every one of those has to probe bloom filters across
    // each un-merged file. Compacting first was measured to matter: against 735
    // uncompacted SSTs the backward walk managed only ~7 MB/s at 10% CPU.
    if compact_first {
        let t = std::time::Instant::now();
        info!("Compacting before metadata rebuild (the read-heavy phase)");
        store.compact_all()?;
        info!("Compaction finished in {:.0}s", t.elapsed().as_secs_f64());
    }

    // Derive what rskj keeps in its MapDB index, which is not readable here.
    //
    // The tip comes from the block import rather than BlockStore::head: head
    // still points at whatever this node had synced before the import, and is
    // only moved once the metadata rebuild below completes.
    let tip = if metadata_only {
        // No block phase ran, so find the tip by scanning the headers we hold.
        store.highest_header()?.ok_or_else(|| {
            anyhow::anyhow!("metadata-only run but no headers present; run a full import first")
        })?
    } else {
        if block_stats.tip_number == 0 && block_stats.scanned == 0 {
            anyhow::bail!("no blocks found in the source datasource");
        }
        block_stats.tip_hash
    };
    info!("Source tip is {tip:?}; rebuilding canonical chain and difficulty");
    let tip_number = rskj_import::rebuild_chain_metadata(
        store,
        tip,
        csv.as_mut().map(|f| f as &mut dyn std::io::Write),
    )?;

    if rskj_import::shutdown_requested() {
        info!("Import stopped early by signal; phases completed so far are flushed and durable");
        return Ok(());
    }

    info!(
        "Import complete in {:.0}s: {} trie nodes, {} blocks, head #{}",
        started.elapsed().as_secs_f64(),
        unitrie_phase.written,
        block_stats.written,
        tip_number
    );

    Ok(())
}

/// Scans the Unitrie from one state root and prints statistics.
///
/// Opens read-only: the scan is a pure reader, and requiring the node to stop
/// for it would make the tool far less useful. A read-only handle sees the
/// database as of the last flush, which is what we want anyway -- a consistent
/// snapshot rather than a moving target.
/// Opens a trie source and resolves which state root to start from.
///
/// Two shapes are accepted. A node datadir carries headers, so a root can be
/// chosen by block or taken from the executed head. A snapshot carries only
/// trie nodes and its own metadata, so the root comes from there -- and a block
/// cannot be selected, because a snapshot holds exactly one state and no
/// headers to look anything up in.
fn open_trie_source(
    dir: &str,
    block: Option<u64>,
) -> Result<(std::sync::Arc<rocksdb::DB>, alloy_primitives::B256, Option<u64>)> {
    if rustock_storage::trie_snapshot::is_snapshot(dir) {
        let meta = rustock_storage::trie_snapshot::read_meta(dir)?;
        if block.is_some() {
            anyhow::bail!(
                "{dir} is a trie snapshot: it holds one state ({:?}) and no headers, so a                  block cannot be selected. Drop --trie-tool-block, or point at a node datadir.",
                meta.root
            );
        }
        let mut opts = rocksdb::Options::default();
        opts.create_if_missing(false);
        opts.set_max_open_files(512);
        let db = rocksdb::DB::open_cf_for_read_only(&opts, dir, ["trie_nodes"], false)
            .with_context(|| format!("opening snapshot {dir} read-only"))?;
        info!(
            "Snapshot {dir}: root {:?}, {} nodes, taken {}",
            meta.root, meta.nodes, meta.created
        );
        return Ok((std::sync::Arc::new(db), meta.root, meta.block));
    }
    open_node_trie_source(dir, block)
}

fn open_node_trie_source(
    data_dir: &str,
    block: Option<u64>,
) -> Result<(std::sync::Arc<rocksdb::DB>, alloy_primitives::B256, Option<u64>)> {
    let mut opts = rocksdb::Options::default();
    opts.create_if_missing(false);
    opts.set_max_open_files(512);
    let cfs = ["headers", "block_numbers", "total_difficulty", "block_bodies",
               "receipts", "tx_index", "trie_nodes"];
    let db = rocksdb::DB::open_cf_for_read_only(&opts, data_dir, cfs, false)
        .with_context(|| format!("opening {data_dir} read-only"))?;
    let db = std::sync::Arc::new(db);

    // Resolve the root: an explicit block's header, or the last executed block.
    let (root, resolved_block) = match block {
        Some(n) => {
            let cf_num = db.cf_handle("block_numbers")
                .ok_or_else(|| anyhow::anyhow!("missing block_numbers column family"))?;
            let hash = db.get_cf(cf_num, n.to_be_bytes())?
                .ok_or_else(|| anyhow::anyhow!("no canonical block at #{n}"))?;
            let cf_h = db.cf_handle("headers")
                .ok_or_else(|| anyhow::anyhow!("missing headers column family"))?;
            let raw = db.get_cf(cf_h, &hash)?
                .ok_or_else(|| anyhow::anyhow!("header missing for block #{n}"))?;
            let mut slice: &[u8] = &raw;
            let h = <rustock_core::Header as alloy_rlp::Decodable>::decode(&mut slice)?;
            // Before Wasabi100 (RSKIP126, mainnet #1,591,000) the header's
            // state_root is the legacy Orchid root, not a Unitrie node hash, so
            // it will not resolve in the trie store. Say so rather than
            // reporting a bare "not found".
            if n < 1_591_000 {
                anyhow::bail!(
                    "block #{n} predates RSKIP126 (mainnet #1,591,000): its header holds the \
                     legacy Orchid state root, not a Unitrie node hash. Choose a later block."
                );
            }
            (h.state_root, Some(n))
        }
        None => {
            // The last EXECUTED block, not the last downloaded one: only the
            // executed head is guaranteed to have its state present.
            let raw = db.get(b"exec_head")?
                .ok_or_else(|| anyhow::anyhow!(
                    "no exec head recorded; pass --trie-tool-block to choose a block"
                ))?;
            if raw.len() < 64 {
                anyhow::bail!("exec head record is malformed");
            }
            let hash = alloy_primitives::B256::from_slice(&raw[0..32]);
            let root = alloy_primitives::B256::from_slice(&raw[32..64]);
            let cf_h = db.cf_handle("headers")
                .ok_or_else(|| anyhow::anyhow!("missing headers column family"))?;
            let number = db.get_cf(cf_h, hash.as_slice())?.and_then(|raw| {
                let mut slice: &[u8] = &raw;
                <rustock_core::Header as alloy_rlp::Decodable>::decode(&mut slice).ok().map(|h| h.number)
            });
            (root, number)
        }
    };
    Ok((db, root, resolved_block))
}

fn run_trie_tool(
    source: &str,
    block: Option<u64>,
    progress_every: u64,
    copy_to: Option<&str>,
    copy_overwrite: bool,
) -> Result<()> {
    use rustock_storage::trie_tool;
    use rustock_storage::trie_snapshot::SnapshotWriter;

    let (db, root, resolved_block) = open_trie_source(source, block)?;

    // An empty string means the flag was given without a value: name the
    // directory after the root, which is the one label guaranteed to identify
    // the contents.
    let target = copy_to.map(|t| {
        if t.is_empty() {
            format!("{root:?}").trim_start_matches("0x").to_string()
        } else {
            t.to_string()
        }
    });

    let mut writer = match target.as_deref() {
        Some(t) => {
            info!("Copying scanned state to {t}");
            Some(SnapshotWriter::create(t, copy_overwrite)?)
        }
        None => None,
    };

    info!("Scanning Unitrie from state root {root:?}");
    let store = rustock_storage::RocksDbTrieStore::from_db(db);
    let started = std::time::Instant::now();
    let stats = trie_tool::scan_with_options(
        &store,
        root,
        trie_tool::ScanOptions {
            interval_secs: progress_every,
            snapshot: writer.as_mut(),
            ..Default::default()
        },
    )?;
    trie_tool::report(&stats, root, resolved_block, started.elapsed().as_secs_f64());

    if let Some(w) = writer {
        let meta = w.finish(root, resolved_block, source)?;
        println!("SNAPSHOT");
        println!("  {:<34}{:>14}", "directory", target.as_deref().unwrap_or("-"));
        println!("  {:<34}{:>14}", "nodes written", meta.nodes);
        println!("  {:<34}{:>14}", "long values written", meta.long_values);
        println!("  {:<34}{:>14}", "bytes written", meta.bytes);
        println!();
        println!("  Re-scan it with:");
        println!("    rustock --trie-tool --trie-tool-source {}",
            target.as_deref().unwrap_or("-"));
    }
    Ok(())
}

/// Prints every field of the node at one path.
fn run_trie_node(source: &str, block: Option<u64>, path: &str) -> Result<()> {
    use rustock_storage::trie_inspect;

    let bits = trie_inspect::parse_path(path)?;
    let (db, root, _) = open_trie_source(source, block)?;
    let store = rustock_storage::RocksDbTrieStore::from_db(db);

    match trie_inspect::find_by_path(&store, root, &bits)? {
        Some(found) => {
            println!("state root      {root:?}");
            print!("{}", trie_inspect::describe(&found, &store));
        }
        None => {
            println!("state root      {root:?}");
            println!(
                "No node at {}. The path either leaves the trie, or ends partway through a \
                 shared path -- a run of bits the trie keeps compressed inside one node, where \
                 no node exists to address.",
                trie_inspect::format_path(&bits)
            );
        }
    }
    Ok(())
}

/// Build and spawn the peg-out watcher. Returns `None` when the file disables
/// it. Errors mean a bad configuration and are reported without stopping the
/// node.
fn start_pegout_alerts(
    path: &str,
    store: Arc<BlockStore>,
    trie: Arc<dyn rustock_trie::TrieStore>,
) -> anyhow::Result<Option<tokio::task::JoinHandle<()>>> {
    use rustock_pegout_alerts::{AlertSink, Config, LogSink, Watcher};

    let config = Config::load(path)?;
    let cfg = config.pegout_alerts;
    if !cfg.enabled {
        return Ok(None);
    }

    // The log sink is always present, so the record exists even if mail fails.
    let mut sinks: Vec<Box<dyn AlertSink>> = vec![Box::new(LogSink)];
    if cfg.email.enabled {
        #[cfg(feature = "smtp")]
        sinks.push(Box::new(rustock_pegout_alerts::SmtpSink::new(&cfg.email)?));
        #[cfg(not(feature = "smtp"))]
        return Err(rustock_pegout_alerts::sink::smtp_unavailable());
        info!(
            "Peg-out alerts will be emailed to {} via {}:{}",
            cfg.email.to.join(", "), cfg.email.smtp_host, cfg.email.smtp_port
        );
    } else {
        info!("Peg-out alerts will be logged only (email disabled)");
    }

    let change_scripts = cfg.change_scripts()?;

    // Thresholds are retuned by editing the file: the watcher re-reads it when
    // its modification time changes, so a threshold change needs no restart.
    let watcher = Watcher::new(store, trie, cfg, sinks, change_scripts)?.reloading_from(path);
    Ok(Some(tokio::spawn(watcher.run())))
}


/// Rebuild the transaction index over a block range, reporting progress.
///
/// Reads bodies only -- no trie, no execution -- so it can repair the history a
/// node imported rather than executed. Single-threaded on purpose: it is I/O
/// bound on one RocksDB instance that allows a single writer anyway.
fn run_repair_tx_index(
    store: &Arc<BlockStore>,
    from: Option<u64>,
    to: Option<u64>,
) -> anyhow::Result<()> {
    use std::time::Instant;

    let head = match store.head()? {
        Some(hash) => store.header(hash)?.map(|h| h.number).unwrap_or(0),
        None => 0,
    };
    let from = from.unwrap_or(1);
    let to = to.unwrap_or(head);
    if to < from {
        anyhow::bail!("--repair-to ({to}) is below --repair-from ({from})");
    }
    let total = to - from + 1;

    info!("repair tx-index: blocks #{from}..#{to} ({total} blocks), chain head #{head}");
    info!("repair tx-index: reading block bodies only; no trie access, no execution");

    let started = Instant::now();
    let report_every = 10_000u64;
    let (blocks, txs, skipped) = store.repair_tx_index(from, to, report_every, |block, done, txs| {
        let elapsed = started.elapsed().as_secs_f64();
        let scanned = block.saturating_sub(from) + 1;
        let pct = scanned as f64 * 100.0 / total as f64;
        let bps = scanned as f64 / elapsed.max(0.001);
        let tps = txs as f64 / elapsed.max(0.001);
        let eta = if bps > 0.0 { (total - scanned) as f64 / bps } else { 0.0 };
        info!(
            "repair tx-index: {pct:.2}% (#{block}) | {done} blocks indexed, {txs} txs | \
             {bps:.0} blocks/s, {tps:.0} tx/s | elapsed {} | ETA {}",
            fmt_duration(elapsed),
            fmt_duration(eta)
        );
    })?;

    let elapsed = started.elapsed().as_secs_f64();
    info!(
        "repair tx-index: DONE in {} | {blocks} blocks indexed, {txs} transactions, \
         {skipped} blocks skipped (no canonical hash or no body) | {:.0} blocks/s, {:.0} tx/s",
        fmt_duration(elapsed),
        blocks as f64 / elapsed.max(0.001),
        txs as f64 / elapsed.max(0.001),
    );
    Ok(())
}

/// Seconds as `2h 04m 31s`, for progress lines a human reads.
fn fmt_duration(secs: f64) -> String {
    let s = secs.max(0.0) as u64;
    let (h, m, sec) = (s / 3600, (s % 3600) / 60, s % 60);
    if h > 0 { format!("{h}h {m:02}m {sec:02}s") } else if m > 0 { format!("{m}m {sec:02}s") } else { format!("{sec}s") }
}
