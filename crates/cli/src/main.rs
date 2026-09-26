use clap::{CommandFactory, FromArgMatches, Parser};

mod config_file;
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

/// The gas-price tracker, as `eth_gasPrice` needs it.
struct GasPriceAdapter(Arc<rustock_sync::gas_price::GasPriceTracker>);

impl rustock_rpc::server::GasPriceSource for GasPriceAdapter {
    fn gas_price(&self) -> alloy_primitives::U256 {
        self.0.gas_price()
    }
}

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

    fn pool_content(&self) -> rustock_rpc::server::PoolContent {
        let (pending, queued) = self.0.content();
        rustock_rpc::server::PoolContent { pending, queued }
    }

    fn quota_report(&self, address: &alloy_primitives::Address) -> Option<(f64, u64)> {
        self.0.quota_report_of(address)
    }
}

/// The transaction pool, as the block builder needs it. The pool recovers
/// every sender on admission, so the builder never has to do it again -- which
/// matters because a template is rebuilt every minute over the whole pool.
struct PoolTxSource(Arc<rustock_sync::TransactionPool>);

impl rustock_execution::mining::PendingTransactionSource for PoolTxSource {
    fn pending(&self) -> Vec<rustock_execution::mining::PendingTransaction> {
        self.0
            .pending_transactions()
            .into_iter()
            .map(|ptx| rustock_execution::mining::PendingTransaction {
                tx: ptx.tx,
                sender: ptx.sender,
                hash: ptx.hash,
            })
            .collect()
    }
}

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Read settings from a TOML file.
    ///
    /// Anything given on the command line wins over the file, and the file
    /// wins over the built-in default -- so an operator can override a file
    /// setting with a flag without editing the file. Unknown keys are an
    /// error rather than being ignored.
    ///
    /// One-shot operations (--import-*, --repair, --trie-tool-*, --probe-*)
    /// are command-line only: a node that re-ran an import on every start
    /// would be a trap.
    #[arg(long, value_name = "PATH")]
    config: Option<String>,

    /// Port to listen for P2P connections
    #[arg(short, long, default_value_t = 30303)]
    port: u16,

    /// Data directory
    #[arg(short, long, default_value = "./data")]
    data_dir: String,

    /// Serve JSON-RPC over WebSocket as well, on its own port.
    ///
    /// Off by default, as rskj's `rpc.providers.web.ws.enabled` is. This is
    /// the only transport that carries `eth_subscribe`, so a client wanting
    /// new heads or logs pushed to it needs this on.
    #[arg(long, default_value_t = false)]
    ws: bool,

    /// Port for the WebSocket JSON-RPC server (rskj's default is 4445).
    #[arg(long, default_value_t = 4445)]
    ws_port: u16,

    /// Addresses or CIDR blocks refused at connection time, as rskj's
    /// `peer.bannedPeerIPs`. Repeatable.
    ///
    /// Bans added later through `sco_banAddress` are persisted separately, in
    /// `banned-peers.txt` inside the data directory, and reloaded at start-up.
    #[arg(long, value_name = "ADDRESS_OR_CIDR")]
    banned_peers: Vec<String>,

    /// Record peer scoring but never act on it.
    ///
    /// rskj's `scoring.punishmentEnabled = false`. Events are still counted
    /// and `sco_peerList` still reports them, so an operator can see what
    /// would have been punished before letting it punish.
    #[arg(long, default_value_t = false)]
    no_peer_punishment: bool,

    /// Serve snapshots of this node's state to peers that ask for one.
    ///
    /// Costs disk reads and bandwidth; answers are proved, so a peer needs no
    /// further trust in this node than it would have in any other.
    #[arg(long, default_value_t = false)]
    snap_server: bool,

    /// Catch up by downloading a state instead of replaying the chain into
    /// one.
    ///
    /// The header chain to the snapshot point is verified first, under the
    /// same proof-of-work rules as a full sync, and every chunk of state is
    /// checked against that chain's state root as it arrives. What is given
    /// up is the re-execution of history, not the verification of it.
    #[arg(long, default_value_t = false)]
    snap_sync: bool,

    /// Bytes of state to ask for per chunk.
    #[arg(long, default_value_t = 100_000, value_name = "BYTES")]
    snap_chunk_bytes: u64,

    /// Chunk requests to keep in flight at once, across all peers.
    #[arg(long, default_value_t = 8, value_name = "N")]
    snap_parallel: usize,

    /// Network ID (30 for mainnet, 33 for regtest)
    #[arg(long, default_value = "30")]
    network_id: u64,

    /// Secret key for the P2P node (hex). If not provided, a random one will be used.
    #[arg(long)]
    secret_key: Option<String>,

    /// Supply conservation checking: per-transaction, per-block, both or off.
    ///
    /// Per-transaction is the default and is strictly more sensitive:
    /// block-level netting hides a creation when one transaction mints and
    /// another destroys the same amount. Per-block is kept on as a cross-check
    /// that the per-transaction deltas account for the whole block -- a
    /// disagreement means value moved outside any transaction.
    #[arg(long, value_name = "MODE", default_value = "both")]
    supply_check: String,

    /// Build the Bridge event index from stored receipts and exit.
    ///
    /// Indexes every log the Bridge emitted, keyed by event signature, so
    /// "find every peg-in" becomes a prefix scan rather than a walk over every
    /// receipt in the chain. Requires receipts (see --import-rskj-receipts).
    /// The node must be stopped.
    #[arg(long, default_value_t = false)]
    build_bridge_index: bool,

    /// Add the block-height index to an existing database and exit.
    ///
    /// Maps every height to every block hash at it, canonical or not. Uncle
    /// selection needs the blocks that lost, and the canonical pointer names
    /// only the one that won, so a database synced before this index existed
    /// cannot mine with uncles until it is built.
    ///
    /// Reads every stored header and writes one small key per block. It does
    /// not touch state, blocks or receipts, and it can be interrupted and
    /// re-run: each entry is derived from the header it indexes, so a partial
    /// run simply resumes. The node must be stopped -- RocksDB allows a single
    /// writer.
    #[arg(long, default_value_t = false)]
    build_height_index: bool,

    /// Emulate rskj's peg-in sender detection for multisig inputs: `on` or `off`.
    ///
    /// On by default, because rskj still does it and a node that stops would
    /// fork. `off` is for after the RSKIP retiring this path activates, and for
    /// measuring what the path is worth: with it off, a peg-in that depended on
    /// it is simply not classified.
    ///
    /// Whichever way it is set, a peg-in that exercises the path is logged and,
    /// if alerting is configured, mailed.
    #[arg(long, value_name = "on|off", default_value = "on")]
    rskj_multisig_senders: String,

    /// Import rskj's receipts from an extracted snapshot directory and exit.
    ///
    /// rustock only writes receipts for blocks it EXECUTES, so a node that
    /// imported its history has none for those blocks. rskj keys receipts by
    /// transaction and rustock by block, so this converts rather than copies.
    /// Point it at `database/mainnet/receipts` from an extracted snapshot.
    /// The node must be stopped: RocksDB allows a single writer.
    #[arg(long, value_name = "DIR")]
    import_rskj_receipts: Option<String>,

    /// First block for --import-rskj-receipts (default 1).
    #[arg(long)]
    receipts_from: Option<u64>,

    /// Last block for --import-rskj-receipts (default: the chain head).
    #[arg(long)]
    receipts_to: Option<u64>,

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

    /// Timezone for log timestamps: `utc`, `local`, or a fixed UTC offset
    /// such as `-03:00`.
    ///
    /// Timestamps stay RFC 3339, so the offset is part of the line rather
    /// than something a reader has to know: UTC prints `…T01:53:47.0+00:00`,
    /// `-03:00` prints `…T22:53:47.0-03:00`. Nothing downstream has to guess
    /// which one it is looking at.
    ///
    /// `local` reads the machine's offset, which is resolved once at start-up
    /// and then fixed for the life of the process -- so a node that runs
    /// across a daylight-saving transition keeps the offset it started with.
    /// A fixed offset avoids that entirely, and is exactly right for zones
    /// without DST (Argentina has had none since 2009).
    ///
    /// `allow_hyphen_values` because a negative offset starts with `-`, and
    /// clap otherwise reads `--log-timezone -03:00` as a missing value
    /// followed by an unknown `-0` flag.
    #[arg(
        long,
        default_value = "utc",
        value_name = "utc|local|±HH:MM",
        allow_hyphen_values = true
    )]
    log_timezone: String,

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

    /// Entries in the BTC stored-block cache, or 0 to disable it.
    ///
    /// Answering "which BTC block is at height H?" costs one trie read per
    /// block of depth whenever the RSKIP199 height index cannot answer, so a
    /// deep query walks thousands of nodes. The cache holds blocks the node
    /// has already read for consensus; it never pre-loads and never records an
    /// absence, so it changes latency and nothing else.
    ///
    /// About 200 bytes per entry -- ~2 MB at the default.
    #[arg(long, default_value_t = 10_000)]
    btc_block_cache_entries: usize,

    /// Serve the `mnr_*` merged-mining JSON-RPC namespace, so mining software
    /// can ask this node for work and submit Bitcoin solutions back.
    ///
    /// Off by default. With this off the namespace reports as unknown, so a
    /// node that is not a miner looks exactly like one built without mining.
    #[arg(long)]
    mine: bool,

    /// Address block rewards are paid to. Required with --mine: mining to the
    /// zero address is almost never intended, and silently doing it would burn
    /// the reward of every block found.
    #[arg(long, value_name = "ADDRESS")]
    mining_coinbase: Option<String>,

    /// Bytes to put in each mined block's `extraData`, at most 32.
    #[arg(long, default_value = "")]
    mining_extra_data: String,

    /// Seconds between rebuilds of the block template handed to miners.
    ///
    /// A template goes stale as the chain advances and as transactions arrive;
    /// rskj rebuilds once a minute and so does this.
    #[arg(long, default_value_t = 60)]
    mining_refresh_secs: u64,

    /// Blocks of history to keep below the head when pruning.
    ///
    /// Clamped up to 8,000 whatever is given: Rootstock's block-info
    /// precompiles reach 4,000 blocks back, and a reorg may re-execute from
    /// 4,000 back, so a re-executed block can read 8,000 back.
    #[arg(long, default_value_t = 100_000)]
    prune_keep_depth: u64,

    /// Most blocks one prune sweep may remove.
    #[arg(long, default_value_t = 50_000)]
    prune_max_batch: u64,

    /// Rate-limit accounts that broadcast transactions consuming large amounts
    /// of shared resources (rskj `transaction.accountTxRateLimit.enabled`).
    ///
    /// On by default, as in rskj. Each account accumulates "virtual gas" while
    /// idle and spends it when it broadcasts; an account that floods the
    /// mempool runs out and its transactions are refused entry here, so they
    /// are not relayed onward. Ordinary use never reaches the limit.
    #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
    account_tx_rate_limit: bool,

    /// Minutes between sweeps that drop accounts which have accumulated the
    /// full quota (rskj `transaction.accountTxRateLimit.cleanerPeriod`).
    ///
    /// An account at the ceiling is indistinguishable from one never seen, so
    /// keeping it costs memory and says nothing. Zero or negative disables the
    /// sweep, matching rskj.
    #[arg(long, default_value_t = 30)]
    account_tx_rate_limit_cleaner_period: i64,

    /// Most accounts tracked at once (rskj `MAX_QUOTAS_SIZE`). The map evicts
    /// the least recently accessed entry, so a flood of distinct addresses
    /// cannot grow it without bound.
    #[arg(long, default_value_t = 400_000)]
    account_tx_rate_limit_max_accounts: usize,

    /// How many seconds of idleness an account may bank, as a multiple of the
    /// per-second accrual (rskj `MAX_QUOTA_GAS_MULTIPLIER`).
    #[arg(long, default_value_t = 2_000)]
    account_tx_rate_limit_quota_multiplier: u64,

    /// Fraction of the block gas limit an account accrues per second of
    /// idleness (rskj `MAX_GAS_PER_SECOND_PERCENT`).
    #[arg(long, default_value_t = 0.9)]
    account_tx_rate_limit_gas_per_second_percent: f64,

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

/// Apply the file underneath the command line, one setting per line so the
/// mapping from key to flag is readable at a glance.
///
/// `apply` is a no-op for anything the operator typed, which is what makes the
/// precedence `command line > file > default`.
fn apply_file_config(
    matches: &clap::ArgMatches,
    f: &config_file::FileConfig,
    a: &mut Args,
) {
    use config_file::{apply, apply_opt};

    apply(matches, "port", f.node.port.as_ref(), &mut a.port);
    apply(matches, "data_dir", f.node.data_dir.as_ref(), &mut a.data_dir);
    apply(matches, "network_id", f.node.network_id.as_ref(), &mut a.network_id);
    apply_opt(matches, "secret_key", f.node.secret_key.as_ref(), &mut a.secret_key);
    apply_opt(matches, "external_ip", f.node.external_ip.as_ref(), &mut a.external_ip);
    apply(matches, "supply_check", f.node.supply_check.as_ref(), &mut a.supply_check);
    apply(matches, "rskj_multisig_senders", f.node.rskj_multisig_senders.as_ref(),
        &mut a.rskj_multisig_senders);

    apply(matches, "rpc_host", f.rpc.host.as_ref(), &mut a.rpc_host);
    apply(matches, "rpc_port", f.rpc.port.as_ref(), &mut a.rpc_port);
    apply(matches, "rpc_admin", f.rpc.admin.as_ref(), &mut a.rpc_admin);
    apply(matches, "no_rpc", f.rpc.disabled.as_ref(), &mut a.no_rpc);
    apply(matches, "ws", f.rpc.ws.as_ref(), &mut a.ws);
    apply(matches, "ws_port", f.rpc.ws_port.as_ref(), &mut a.ws_port);

    apply(matches, "max_peers", f.peers.max_peers.as_ref(), &mut a.max_peers);
    apply(matches, "max_inbound_peers", f.peers.max_inbound_peers.as_ref(),
        &mut a.max_inbound_peers);
    apply(matches, "max_inbound_per_ip", f.peers.max_inbound_per_ip.as_ref(),
        &mut a.max_inbound_per_ip);
    apply(matches, "max_inbound_per_cidr", f.peers.max_inbound_per_cidr.as_ref(),
        &mut a.max_inbound_per_cidr);
    apply(matches, "inbound_cidr_prefix", f.peers.inbound_cidr_prefix.as_ref(),
        &mut a.inbound_cidr_prefix);
    // A list, so `apply`'s "the CLI wins" rule does not fit: the two sources
    // are *combined*. An operator keeping a standing ban list in the config
    // file should still be able to add one on the command line without
    // retyping the rest.
    if let Some(banned) = &f.peers.banned_peers {
        for entry in banned {
            if !a.banned_peers.contains(entry) {
                a.banned_peers.push(entry.clone());
            }
        }
    }
    apply(matches, "no_peer_punishment", f.peers.no_peer_punishment.as_ref(),
        &mut a.no_peer_punishment);

    apply(matches, "trie_backend", f.trie.backend.as_ref(), &mut a.trie_backend);
    apply(
        matches,
        "btc_block_cache_entries",
        f.trie.btc_block_cache_entries.as_ref(),
        &mut a.btc_block_cache_entries,
    );
    apply_opt(matches, "trie_dir", f.trie.dir.as_ref(), &mut a.trie_dir);
    apply_opt(matches, "trie_node", f.trie.node.as_ref(), &mut a.trie_node);

    apply(matches, "gc_epochs", f.gc.epochs.as_ref(), &mut a.gc_epochs);
    apply(matches, "gc_rotate_mb", f.gc.rotate_mb.as_ref(), &mut a.gc_rotate_mb);
    apply(matches, "gc_burial", f.gc.burial.as_ref(), &mut a.gc_burial);
    apply(matches, "gc_check_secs", f.gc.check_secs.as_ref(), &mut a.gc_check_secs);

    apply(matches, "prune_keep_depth", f.prune.keep_depth.as_ref(), &mut a.prune_keep_depth);
    apply(matches, "prune_max_batch", f.prune.max_batch.as_ref(), &mut a.prune_max_batch);

    apply(matches, "mine", f.mining.enabled.as_ref(), &mut a.mine);
    apply_opt(matches, "mining_coinbase", f.mining.coinbase.as_ref(), &mut a.mining_coinbase);
    apply(matches, "mining_extra_data", f.mining.extra_data.as_ref(), &mut a.mining_extra_data);
    apply(matches, "mining_refresh_secs", f.mining.refresh_secs.as_ref(),
        &mut a.mining_refresh_secs);

    apply(matches, "account_tx_rate_limit", f.account_tx_rate_limit.enabled.as_ref(),
        &mut a.account_tx_rate_limit);
    apply(matches, "account_tx_rate_limit_cleaner_period",
        f.account_tx_rate_limit.cleaner_period.as_ref(),
        &mut a.account_tx_rate_limit_cleaner_period);
    apply(matches, "account_tx_rate_limit_max_accounts",
        f.account_tx_rate_limit.max_accounts.as_ref(),
        &mut a.account_tx_rate_limit_max_accounts);
    apply(matches, "account_tx_rate_limit_quota_multiplier",
        f.account_tx_rate_limit.quota_multiplier.as_ref(),
        &mut a.account_tx_rate_limit_quota_multiplier);
    apply(matches, "account_tx_rate_limit_gas_per_second_percent",
        f.account_tx_rate_limit.gas_per_second_percent.as_ref(),
        &mut a.account_tx_rate_limit_gas_per_second_percent);

    apply(matches, "log_level", f.log.level.as_ref(), &mut a.log_level);
    apply(matches, "log_to_stdout", f.log.to_stdout.as_ref(), &mut a.log_to_stdout);
    apply(matches, "log_timezone", f.log.timezone.as_ref(), &mut a.log_timezone);

    apply_opt(matches, "pegout_alerts_config", f.alerts.pegout_alerts_config.as_ref(),
        &mut a.pegout_alerts_config);
}

/// Parse `--log-timezone` into a UTC offset.
///
/// `utc`, `local`, or a fixed `±HH:MM` / `±HHMM` / `±HH`.
///
/// `local` needs the offset the caller read before the runtime started; when
/// that could not be determined this returns an error rather than quietly
/// logging in UTC, because a timestamp in a timezone the operator did not ask
/// for is worse than a refusal to start. They can pass the offset instead.
fn parse_log_timezone(
    spec: &str,
    local_offset: Option<time::UtcOffset>,
) -> Result<time::UtcOffset> {
    let spec = spec.trim();
    match spec.to_ascii_lowercase().as_str() {
        "utc" | "z" | "" => return Ok(time::UtcOffset::UTC),
        "local" => {
            return local_offset.ok_or_else(|| {
                anyhow::anyhow!(
                    "--log-timezone local: this machine's UTC offset could not be \
                     determined. Pass the offset instead, for example \
                     --log-timezone -03:00"
                )
            })
        }
        _ => {}
    }

    let (sign, rest) = match spec.split_at_checked(1) {
        Some(("+", rest)) => (1i8, rest),
        Some(("-", rest)) => (-1i8, rest),
        _ => {
            return Err(anyhow::anyhow!(
                "--log-timezone {spec:?}: expected `utc`, `local`, or a signed offset \
                 such as -03:00"
            ))
        }
    };

    // The shape is checked rather than the colons stripped. Stripping them
    // accepts `-1:2` and reads it as `-12:00`, which is not a parse failure
    // the operator would ever see -- every log line would simply carry the
    // wrong offset. Two digits mean two digits.
    let digits: Vec<char> = rest.chars().collect();
    let two = |a: char, b: char| -> Option<i8> {
        (a.is_ascii_digit() && b.is_ascii_digit())
            .then(|| (a as i8 - b'0' as i8) * 10 + (b as i8 - b'0' as i8))
    };
    let (hours, minutes) = match digits.as_slice() {
        [h1, h2] => (two(*h1, *h2), Some(0)),
        [h1, h2, m1, m2] => (two(*h1, *h2), two(*m1, *m2)),
        [h1, h2, ':', m1, m2] => (two(*h1, *h2), two(*m1, *m2)),
        _ => {
            return Err(anyhow::anyhow!(
                "--log-timezone {spec:?}: expected ±HH, ±HHMM or ±HH:MM"
            ))
        }
    };
    let (Some(hours), Some(minutes)) = (hours, minutes) else {
        return Err(anyhow::anyhow!(
            "--log-timezone {spec:?}: expected ±HH, ±HHMM or ±HH:MM"
        ));
    };
    if !(0..=59).contains(&minutes) {
        return Err(anyhow::anyhow!("--log-timezone {spec:?}: minutes must be 00-59"));
    }

    time::UtcOffset::from_hms(sign * hours, sign * minutes, 0).map_err(|_| {
        // `time` caps offsets at ±25:59:59; the real world stops at ±14:00.
        anyhow::anyhow!("--log-timezone {spec:?}: offset out of range")
    })
}

/// The timestamp format: RFC 3339 with **one** subsecond digit.
///
/// `2026-09-25T19:02:09.1-03:00`.
///
/// The well-known `Rfc3339` formatter prints full nanosecond precision
/// (`.147085805`), which is nine digits of noise on every line of a node
/// whose interesting events are hundreds of milliseconds apart. RFC 3339
/// allows any number of `time-secfrac` digits, so one is still conformant and
/// still parses.
///
/// The offset is kept mandatory, so it travels with every timestamp -- `Z`
/// for UTC, `-03:00` otherwise -- and neither a reader nor a parser has to be
/// told separately which zone a line is in.
///
/// The cost is that two events inside the same tenth of a second are no
/// longer ordered by their timestamps. The journal preserves arrival order
/// regardless, and the durations that matter are logged as explicit
/// milliseconds rather than inferred from timestamps.
const LOG_TIMESTAMP: &[time::format_description::BorrowedFormatItem<'static>] = time::macros::format_description!(
    "[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:1][offset_hour sign:mandatory]:[offset_minute]"
);

/// The timer the subscriber formats timestamps with.
fn log_timer(
    spec: &str,
    local_offset: Option<time::UtcOffset>,
) -> Result<
    tracing_subscriber::fmt::time::OffsetTime<
        &'static [time::format_description::BorrowedFormatItem<'static>],
    >,
> {
    let offset = parse_log_timezone(spec, local_offset)?;
    Ok(tracing_subscriber::fmt::time::OffsetTime::new(offset, LOG_TIMESTAMP))
}

/// The machine's UTC offset, read **before** the tokio runtime exists.
///
/// The `time` crate refuses to determine the local offset once a process is
/// multithreaded, and it is right to: another thread calling `setenv("TZ")`
/// concurrently with `localtime_r` is a data race in C, and the crate will
/// not paper over it. `tracing_subscriber`'s `LocalTime` hits exactly this
/// and is gated behind an `--cfg unsound_local_offset` build flag as a
/// result.
///
/// So the offset is read here, in the last moment this process is
/// single-threaded, and then held fixed. That is also why
/// `--log-timezone local` cannot follow a daylight-saving transition: the
/// offset is decided once, at start-up. A fixed `±HH:MM` says so plainly.
fn resolve_local_offset() -> Option<time::UtcOffset> {
    time::UtcOffset::current_local_offset().ok()
}

fn main() -> Result<()> {
    // Before anything spawns a thread.
    let local_offset = resolve_local_offset();
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(run(local_offset))
}

async fn run(local_offset: Option<time::UtcOffset>) -> Result<()> {
    // Parse once, keeping the matches so the merge below can ask clap which
    // values the operator actually typed -- with the derive API a flag that was
    // never passed still arrives carrying its default.
    let matches = Args::command().get_matches();
    let mut args = Args::from_arg_matches(&matches)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    if let Some(path) = args.config.clone() {
        let file = config_file::FileConfig::load(&path)?;
        apply_file_config(&matches, &file, &mut args);
    }
    let args = args;

    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(&args.log_level));

    std::fs::create_dir_all(&args.data_dir).context("Failed to create data directory")?;

    let timer = log_timer(&args.log_timezone, local_offset)?;

    let _guard = if args.log_to_stdout {
        // Colour only when stdout is a terminal. Under systemd it is a pipe
        // into journald, and the escapes are then *stored* in the journal --
        // invisible in `journalctl`'s default view, which strips them, and
        // very visible the moment anyone uses `-o cat`, pipes to a file, or
        // greps. The file path has always passed `false` here; the stdout
        // path assumed a human was watching.
        let colour = std::io::IsTerminal::is_terminal(&std::io::stdout());
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_target(false)
            .with_timer(timer)
            .with_ansi(colour)
            .init();
        None
    } else {
        let file_appender = tracing_appender::rolling::daily(&args.data_dir, "rustock.log");
        let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);

        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_target(false)
            .with_ansi(false)
            .with_timer(timer)
            .with_writer(non_blocking)
            .init();

        let log_path = std::path::Path::new(&args.data_dir).join("rustock.log");
        eprintln!("Logging to {}", log_path.display());
        eprintln!("Use --log-to-stdout to log to the console instead.");
        eprintln!("Tail the log: tail -f {}", log_path.display());

        Some(guard)
    };

    // Say the offset once, in words, even though every line already carries
    // it. A reader skimming a log for a time they remember should not have to
    // notice a `-03:00` suffix to realise the whole file is not in UTC.
    let offset = parse_log_timezone(&args.log_timezone, local_offset)?;
    let (oh, om, _) = offset.as_hms();
    if offset.is_utc() {
        info!("Log timestamps are UTC (GMT+00:00)");
    } else {
        info!(
            "Log timestamps are GMT{}{:02}:{:02} -- every line carries the offset",
            if oh < 0 || om < 0 { '-' } else { '+' },
            oh.abs(),
            om.abs(),
        );
    }

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

    match args.supply_check.as_str() {
        "both" => {}
        "per-transaction" | "tx" => rustock_execution::supply::set_per_block(false),
        "per-block" | "block" => rustock_execution::supply::set_per_transaction(false),
        "off" | "none" => {
            rustock_execution::supply::set_per_transaction(false);
            rustock_execution::supply::set_per_block(false);
            tracing::warn!("supply conservation checking is DISABLED");
        }
        other => anyhow::bail!(
            "unknown --supply-check {other:?}; expected both, per-transaction, per-block or off"
        ),
    }
    info!(
        "supply check: per-transaction {}, per-block {}",
        rustock_execution::supply::per_transaction_enabled(),
        rustock_execution::supply::per_block_enabled()
    );

    if args.build_bridge_index {
        let store = Arc::new(BlockStore::open(&args.data_dir)?);
        rustock_storage::rskj_import::install_signal_handlers();
        run_build_bridge_index(&store, args.receipts_from, args.receipts_to)?;
        return Ok(());
    }

    if args.build_height_index {
        let store = Arc::new(BlockStore::open(&args.data_dir)?);
        rustock_storage::rskj_import::install_signal_handlers();
        run_build_height_index(&store)?;
        return Ok(());
    }

    if let Some(dir) = args.import_rskj_receipts.clone() {
        let store = Arc::new(BlockStore::open(&args.data_dir)?);
        rustock_storage::rskj_import::install_signal_handlers();
        run_import_receipts(&store, &dir, args.receipts_from, args.receipts_to)?;
        return Ok(());
    }

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
    // The manager takes ownership below; snapshot sync needs the same rules.
    let verifier_for_snap = verifier.clone();

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
    let pool_config = rustock_sync::txpool::PoolConfig {
        quota: rustock_sync::quota::QuotaConfig {
            enabled: args.account_tx_rate_limit,
            cleaner_period_minutes: args.account_tx_rate_limit_cleaner_period,
            max_quotas_size: args.account_tx_rate_limit_max_accounts,
            max_quota_gas_multiplier: args.account_tx_rate_limit_quota_multiplier,
            max_gas_per_second_percent: args.account_tx_rate_limit_gas_per_second_percent,
        },
        ..Default::default()
    };
    // One tracker, shared: the rate limiter's low-gas-price factor and
    // `eth_gasPrice` must not disagree about what the market is doing.
    // rskj primes both its windows from the block store at start-up, so a
    // restart does not leave the node answering with the block minimum until
    // 512 fresh transactions have gone by.
    let gas_price_tracker = Arc::new(rustock_sync::gas_price::GasPriceTracker::default());
    gas_price_tracker.initialize_from_store(&store);

    let pool = Arc::new(
        rustock_sync::TransactionPool::new(
            pool_config,
            config.chain_id.into(),
            store.clone(),
            trie_store_for_pool.clone(),
        )
        .with_gas_price_tracker(gas_price_tracker.clone()),
    );

    // Every line in the transaction path logs at `debug` or `trace` and this
    // node runs at `info`, so the pool is otherwise entirely unobservable: an
    // empty `txpool_status` is equally consistent with healthy traffic that has
    // already been mined and with no transaction ever arriving. Report on a
    // schedule, and stay silent when nothing happened so a line always means
    // something did.
    {
        let pool_for_report = pool.clone();
        tokio::spawn(async move {
            const REPORT_EVERY: std::time::Duration = std::time::Duration::from_secs(300);
            let mut ticker = tokio::time::interval(REPORT_EVERY);
            ticker.tick().await; // the first tick fires immediately
            loop {
                ticker.tick().await;
                let a = pool_for_report.take_activity();
                if a.is_empty() {
                    continue;
                }
                let rejected = a.rejected();
                // Reasons, most common first, so a change in the mix is visible
                // without turning on debug logging.
                let reasons = if a.rejections.is_empty() {
                    "-".to_string()
                } else {
                    a.rejections
                        .iter()
                        .map(|(reason, n)| format!("{reason}={n}"))
                        .collect::<Vec<_>>()
                        .join(" ")
                };
                tracing::info!(
                    target: "rustock::txpool",
                    "Mempool in the last {}m: {} offered, {} accepted, {rejected} rejected, \
                     {} mined | {reasons}",
                    REPORT_EVERY.as_secs() / 60,
                    a.offered,
                    a.accepted,
                    a.mined,
                );
                // One sender refused many times is the rate limiter working as
                // designed; many senders refused once each is it misfiring on
                // ordinary traffic. Only say so when it actually refused something.
                if a.quota_senders > 0 {
                    let worst = a
                        .quota_worst
                        .map(|(addr, n)| format!("{addr} ({n})"))
                        .unwrap_or_else(|| "-".to_string());
                    tracing::info!(
                        target: "rustock::txpool",
                        "Rate limiter refused {} sender(s); most refused: {worst}",
                        a.quota_senders,
                    );
                }
            }
        });
    }

    // rskj runs the equivalent sweep on a `TxQuotaCleanerTimer`.
    if let Some(period) = pool.quota_cleaner_period() {
        let pool_for_cleaner = pool.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(period);
            ticker.tick().await; // the first tick fires immediately
            loop {
                ticker.tick().await;
                let dropped = pool_for_cleaner.clean_quotas();
                tracing::debug!(target: "rustock::txpool", "Quota sweep dropped {dropped} accounts");
            }
        });
        tracing::info!(
            "Account tx rate limiting on; quota sweep every {} min",
            period.as_secs() / 60
        );
    } else if !args.account_tx_rate_limit {
        tracing::info!("Account tx rate limiting disabled");
    }

    let hardfork_cfg = rustock_execution::RskHardforkConfig::for_network(config.chain_id as u64);
    let btc_block_cache = if args.btc_block_cache_entries > 0 {
        let cache = Arc::new(rustock_execution::BtcBlockCache::new(
            args.btc_block_cache_entries,
        ));
        info!(
            "BTC block cache: {} entries (~{} MB at full occupancy), demand-populated",
            args.btc_block_cache_entries,
            args.btc_block_cache_entries * 200 / (1024 * 1024)
        );
        Some(cache)
    } else {
        info!("BTC block cache disabled");
        None
    };

    let mut block_processor = rustock_execution::BlockProcessor::new(
        hardfork_cfg.clone(),
        store.clone(),
    );
    if let Some(cache) = &btc_block_cache {
        block_processor = block_processor.with_btc_block_cache(cache.clone());
    }
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

    // The chain-event channel that feeds `eth_subscribe`. Created only when
    // the WebSocket server is on: without a receiver every publish is a
    // no-op, but not creating it at all makes that explicit.
    let events = args.ws.then(rustock_core::events::channel);

    // Peer scoring: one service shared by the node (which refuses banned
    // addresses before the handshake), the sync service and the transaction
    // relay (which feed it events), and the RPC layer (which reports and
    // overrides it). Bans live in the data directory so they survive a
    // restart -- rskj's do not.
    let scoring = Arc::new(rustock_networking::scoring::ScoringService::open(
        std::path::Path::new(&args.data_dir),
        &args.banned_peers,
        rustock_networking::scoring::DEFAULT_NODE_CAPACITY,
        !args.no_peer_punishment,
    ));

    let snap_config = rustock_sync::SnapConfig {
        server_enabled: args.snap_server,
        client_enabled: args.snap_sync,
        chunk_bytes: args.snap_chunk_bytes,
        max_in_flight: args.snap_parallel.max(1),
        ..rustock_sync::SnapConfig::default()
    };
    if args.snap_server {
        info!(
            "Snapshot server enabled: serving state at #(head - {}) in chunks of up to {} bytes",
            snap_config.checkpoint_distance, snap_config.max_chunk_bytes
        );
        sync_handler.attach_snap_server(Arc::new(rustock_sync::SnapServer::new(
            store.clone(),
            trie_store_for_exec.clone(),
            snap_config.clone(),
        )));
    }

    let mut sync_service = SyncService::new(sync_manager.clone(), peer_store.clone(), event_rx)
        .with_tx_pool(pool.clone())
        .with_block_processor(block_processor, trie_store_for_exec, initial_state_root)
        .with_gas_price_tracker(gas_price_tracker.clone())
        .with_scoring(Some(scoring.clone()))
        .with_events(events.clone());

    if args.snap_sync {
        info!(
            "Snapshot sync enabled: verifying the header chain to the snapshot point \
             before downloading state, {} chunks of {} bytes in flight",
            snap_config.max_in_flight, snap_config.chunk_bytes
        );
        sync_service.start_snap_sync(snap_config.clone(), verifier_for_snap.clone());
    }

    // Taken before the service is moved into its task: the RPC layer reads the
    // same gauge the sync loop publishes, so `debug_wireProtocolQueueSize`
    // answers with the live backlog.
    let wire_queue_depth = sync_service.wire_queue_depth();

    if let Some(path) = &args.import_blocks_db {
        let source = RskjBlockSource::open(path)?;
        info!("Sourcing block bodies from rskj LevelDB at {path} (peers only for what it lacks)");
        sync_service = sync_service.with_block_source(Arc::new(source));
    }
    // `Processed N blocks` says the node is alive but nothing about what it is
    // processing: a node keeping up with an idle chain and one keeping up with
    // a saturated one produce the same line. Report what the blocks actually
    // contained, on the same cadence as the mempool summary, and stay silent
    // when no block was executed -- at the tip that is roughly one every 30
    // seconds, so an empty window is itself worth noticing.
    {
        let activity = sync_service.chain_activity();
        let cache_for_report = btc_block_cache.clone();
        tokio::spawn(async move {
            const REPORT_EVERY: std::time::Duration = std::time::Duration::from_secs(300);
            let mut ticker = tokio::time::interval(REPORT_EVERY);
            ticker.tick().await; // the first tick fires immediately
            loop {
                ticker.tick().await;
                let a = activity.take();
                if a.is_empty() {
                    continue;
                }
                let range = match a.first_block {
                    Some(first) if first != a.last_block => {
                        format!("#{first}..#{}", a.last_block)
                    }
                    _ => format!("#{}", a.last_block),
                };
                // ns/gas is the figure to watch across windows. Gas measures
                // work, so cost per unit of work should hold steady whatever
                // the chain is doing; a rising figure means each unit is
                // costing more than it used to. Blocks vary too much in size
                // for time-per-block to be comparable the same way.
                tracing::info!(
                    target: "rustock::sync",
                    "Chain in the last {}m: {} block(s) {range}, {:.1} tx/block, \
                     {:.0} gas/block, {:.1}% full | {:.1} ms/block cpu ({:.1} wall), \
                     {:.1} ns/gas cpu ({:.1} wall)",
                    REPORT_EVERY.as_secs() / 60,
                    a.blocks,
                    a.avg_transactions(),
                    a.avg_gas_used(),
                    a.fullness_percent(),
                    a.avg_cpu_ms_per_block(),
                    a.avg_wall_ms_per_block(),
                    a.cpu_nanos_per_gas(),
                    a.wall_nanos_per_gas(),
                );

                // Reported alongside the chain numbers so the cache's value is
                // visible next to the cost it is meant to reduce. A hit rate
                // that stays at zero means the workload never repeats a BTC
                // lookup, and the memory is being spent for nothing.
                if let Some(cache) = &cache_for_report {
                    let s = cache.stats.snapshot();
                    if s.lookups() > 0 {
                        tracing::info!(
                            target: "rustock::sync",
                            "BTC block cache: {}/{} entries (~{} KB), {:.1}% hit rate                              ({} hits, {} misses, {} evictions)",
                            cache.len(),
                            cache.capacity(),
                            cache.approx_bytes() / 1024,
                            s.hit_rate().unwrap_or(0.0),
                            s.hits,
                            s.misses,
                            s.evictions,
                        );
                    }
                }
            }
        });
    }

    let tx_relay = Arc::new(
        TxRelay::with_pool(peer_store.clone(), pool.clone())
            .with_scoring(Some(scoring.clone()))
            .with_events(events.clone()),
    );

    let mut node =
        Node::with_peer_store(node_config, peer_store.clone()).with_scoring(scoring.clone());
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

    // The miner. It shares the store and trie store with sync but holds its
    // own block processor: `BlockProcessor` is a handle over those two, and
    // the sync service has taken ownership of the one built above.
    let miner: Option<Arc<rustock_execution::MinerServer>> = if args.mine {
        let coinbase_address = match &args.mining_coinbase {
            Some(raw) => raw.parse::<alloy_primitives::Address>().map_err(|e| {
                anyhow::anyhow!("--mining-coinbase is not an address: {e}")
            })?,
            None => anyhow::bail!("--mine requires --mining-coinbase: a miner must say where its rewards go"),
        };
        let extra_data = args.mining_extra_data.clone().into_bytes();
        if extra_data.len() > 32 {
            anyhow::bail!(
                "--mining-extra-data is {} bytes; the header field holds at most 32",
                extra_data.len()
            );
        }

        let mining_config = rustock_execution::MiningConfig {
            coinbase_address,
            extra_data: alloy_primitives::Bytes::from(extra_data),
            ..Default::default()
        };
        let builder = rustock_execution::BlockTemplateBuilder::new(
            Arc::new(rustock_execution::BlockProcessor::new(
                hardfork_cfg.clone(),
                store.clone(),
            )),
            trie_store_for_pool.clone(),
            config.clone(),
            hardfork_cfg.clone(),
            mining_config,
        );
        let chain = Arc::new(rustock_execution::mining::StoreChainAccess {
            store: store.clone(),
            trie_store: trie_store_for_pool.clone(),
        });
        let server = Arc::new(rustock_execution::MinerServer::new(
            builder,
            chain,
            Arc::new(PoolTxSource(pool.clone())),
            config.clone(),
        ));
        info!("Mining enabled; rewards to {coinbase_address}");

        // Rebuild the template on a timer. Nothing else does: the sync service
        // owns the execution loop and does not know the miner exists, so
        // without this a pool would keep being handed work for a parent the
        // chain left behind and every solution would be refused.
        let refresh = std::time::Duration::from_secs(args.mining_refresh_secs.max(1));
        let server_for_refresh = server.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(refresh);
            loop {
                ticker.tick().await;
                let server = server_for_refresh.clone();
                let built = tokio::task::spawn_blocking(move || server.build_work()).await;
                match built {
                    Ok(Ok(_)) => {}
                    // Expected while the node is still syncing: there is no
                    // executed head to build on yet.
                    Ok(Err(e)) => debug!(target: "rustock::mining", "No new work: {e}"),
                    Err(e) => error!(target: "rustock::mining", "Work rebuild panicked: {e}"),
                }
            }
        });

        Some(server)
    } else {
        None
    };

    if !args.no_rpc {
        let trie_store: Arc<dyn rustock_trie::TrieStore> = match &detached_for_readers {
            Some(t) => t.clone(),
            None => Arc::new(rustock_storage::RocksDbTrieStore::from_db(store.db().clone())),
        };

        let rpc_state = rustock_rpc::server::RpcState {
            store: store.clone(),
            wire_queue_depth: Some(wire_queue_depth.clone()),
            gas_price: Some(Arc::new(GasPriceAdapter(gas_price_tracker.clone()))),
            peer_store: peer_store.clone(),
            config: config.clone(),
            tx_submitter: Some(Arc::new(TxRelaySubmitter(tx_relay.clone()))),
            trie_store: Some(trie_store),
            hardfork_cfg: Some(hardfork_cfg),
            filter_store: Arc::new(rustock_rpc::logs::FilterStore::new()),
            tx_pool: Some(Arc::new(PoolAdapter(pool.clone()))),

            epoch_store: epoch_store.clone(),
            miner: miner.clone().map(|m| m as Arc<dyn rustock_rpc::mnr::MiningService>),
            admin_enabled: args.rpc_admin,
            gc_burial: args.gc_burial,
            prune_keep_depth: args.prune_keep_depth,
            prune_max_batch: args.prune_max_batch,
            scoring: Some(scoring.clone()),
            events: events.clone(),
        };
        // The WebSocket server shares the same `RpcState`, so every method
        // available over HTTP works over the socket too -- plus
        // `eth_subscribe`, which only means anything on a connection that
        // stays open.
        if args.ws {
            let ws_host = args.rpc_host.clone();
            let ws_port = args.ws_port;
            let ws_state = rpc_state.clone();
            tokio::spawn(async move {
                if let Err(e) = rustock_rpc::ws::start_ws_server(&ws_host, ws_port, ws_state).await
                {
                    tracing::error!("RPC WebSocket server error: {}", e);
                }
            });
        }

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
    // Block processing raises supply violations on its own thread; it must not
    // block there, so it pushes into this queue and the watcher delivers.
    let (tx, rx) = std::sync::mpsc::channel();
    let tx2 = tx.clone();
    if let Err(e) = rustock_execution::supply::set_observer(move |v| {
        let top: Vec<String> = v.deltas.iter().take(8)
            .map(|d| format!("{} {} -> {}", d.address, d.before, d.after))
            .collect();
        // send() fails only once the watcher is gone; nothing to do about it
        // here, and the violation is already in the log.
        let _ = tx.send(rustock_pegout_alerts::Alert::SupplyNotConserved {
            block: v.block,
            created: v.created,
            amount_wei: v.amount.to_string(),
            top_accounts: top,
        });
    }) {
        tracing::warn!("supply observer not installed: {e}");
    }

    // The same route for a peg-in that leaned on the sender-detection path we
    // intend to remove. Block processing must not block on mail, so it pushes
    // into the same queue the watcher drains.
    if let Err(e) = rustock_execution::bridge::rskj_sender_compat::set_observer(move |s| {
        let _ = tx2.send(rustock_pegout_alerts::Alert::LegacyMultisigPegin {
            block: s.block,
            btc_txid: s.btc_txid.clone(),
            shape: s.shape.to_string(),
            refund_hash160: s.refund_hash160.clone(),
        });
    }) {
        tracing::warn!("multisig-sender observer not installed: {e}");
    }

    let watcher = Watcher::new(store, trie, cfg, sinks, change_scripts)?
        .reloading_from(path)
        .with_inbox(rx);
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

/// Import rskj's receipts, reporting progress the way the repair does.
fn run_import_receipts(
    store: &Arc<BlockStore>,
    dir: &str,
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
    anyhow::ensure!(to >= from, "--receipts-to ({to}) is below --receipts-from ({from})");
    let total = to - from + 1;

    info!("import receipts: blocks #{from}..#{to} ({total} blocks) from {dir}");
    info!("import receipts: blocks that already have receipts are skipped, so this resumes");

    let started = Instant::now();
    let (blocks, receipts, skipped, missing) = rustock_storage::rskj_import::import_receipts(
        store,
        std::path::Path::new(dir),
        from,
        to,
        10_000,
        |block, done, receipts| {
            let elapsed = started.elapsed().as_secs_f64();
            let scanned = block.saturating_sub(from) + 1;
            let pct = scanned as f64 * 100.0 / total as f64;
            let bps = scanned as f64 / elapsed.max(0.001);
            let eta = if bps > 0.0 { (total - scanned) as f64 / bps } else { 0.0 };
            info!(
                "import receipts: {pct:.2}% (#{block}) | {done} blocks, {receipts} receipts | \
                 {bps:.0} blocks/s | elapsed {} | ETA {}",
                fmt_duration(elapsed), fmt_duration(eta)
            );
        },
    )?;

    let elapsed = started.elapsed().as_secs_f64();
    info!(
        "import receipts: DONE in {} | {blocks} blocks written, {receipts} receipts, \
         {skipped} blocks skipped, {missing} transactions not found in the source",
        fmt_duration(elapsed)
    );
    if missing > 0 {
        tracing::warn!(
            "import receipts: {missing} transactions had no receipt in the source; \
             those blocks were left without receipts rather than written partially"
        );
    }
    Ok(())
}

/// Build the block-height index over an existing database.
fn run_build_height_index(store: &Arc<BlockStore>) -> anyhow::Result<()> {
    let already = !store.height_index_is_empty()?;
    if already {
        info!(
            "A height index is already present; re-running to cover anything added since. \
             This is safe: entries are derived from the headers they index."
        );
    }

    let started = std::time::Instant::now();
    info!("Building the block-height index. This reads every stored header.");

    let indexed = store.build_height_index(250_000, |count| {
        info!("  indexed {count} blocks");
    })?;

    let elapsed = started.elapsed();
    info!(
        "Height index built: {indexed} blocks in {:.1}s. Uncle selection can now find \
         the blocks that lost at each height.",
        elapsed.as_secs_f64()
    );
    Ok(())
}

/// Build the Bridge event index, reporting progress like the other bulk tasks.
fn run_build_bridge_index(
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
    anyhow::ensure!(to >= from, "range is empty");
    let total = to - from + 1;
    let bridge = rustock_execution::precompiles::BRIDGE_ADDR;

    info!("bridge index: blocks #{from}..#{to} ({total} blocks), Bridge at {bridge}");

    let started = Instant::now();
    let (scanned, events, no_receipts) = rustock_storage::rskj_import::build_bridge_event_index(
        store, bridge, from, to, 100_000,
        |block, scanned, events| {
            let elapsed = started.elapsed().as_secs_f64();
            let done = block.saturating_sub(from) + 1;
            let bps = done as f64 / elapsed.max(0.001);
            info!(
                "bridge index: {:.2}% (#{block}) | {scanned} blocks with receipts, {events} events | \
                 {bps:.0} blocks/s | elapsed {} | ETA {}",
                done as f64 * 100.0 / total as f64,
                fmt_duration(elapsed),
                fmt_duration(if bps > 0.0 { (total - done) as f64 / bps } else { 0.0 })
            );
        },
    )?;

    info!(
        "bridge index: DONE in {} | {scanned} blocks scanned, {events} Bridge events indexed, \
         {no_receipts} blocks had no receipts",
        fmt_duration(started.elapsed().as_secs_f64())
    );
    if no_receipts > 0 {
        tracing::warn!(
            "bridge index: {no_receipts} blocks had no receipts and contributed nothing; \
             run --import-rskj-receipts first for a complete index"
        );
    }
    Ok(())
}

#[cfg(test)]
mod log_timezone_tests {
    use super::parse_log_timezone;
    use time::UtcOffset;

    fn offset(h: i8, m: i8) -> UtcOffset {
        UtcOffset::from_hms(h, m, 0).unwrap()
    }

    /// The timestamp format, pinned.
    ///
    /// Nanosecond precision was nine digits of noise on every line; one digit
    /// is enough for a node whose interesting events are hundreds of
    /// milliseconds apart. This test is what stops the well-known `Rfc3339`
    /// formatter drifting back in, since it looks like the obvious choice.
    #[test]
    fn a_timestamp_has_one_subsecond_digit_and_a_numeric_offset() {
        use time::macros::datetime;

        let t = datetime!(2026-09-25 19:02:09.147085805 -03:00);
        let rendered = t.format(&super::LOG_TIMESTAMP).unwrap();
        assert_eq!(rendered, "2026-09-25T19:02:09.1-03:00");
        assert_eq!(rendered.len(), 27, "27 characters, not 35");

        // UTC renders as `+00:00`, not `Z`. Both are RFC 3339; the numeric
        // form keeps every line the same width, which is the point of the
        // exercise.
        let utc = datetime!(2026-09-25 22:02:09.9 +00:00);
        assert_eq!(utc.format(&super::LOG_TIMESTAMP).unwrap(), "2026-09-25T22:02:09.9+00:00");

        // Truncation, not rounding: `.98` must not become `.10` of the next
        // second.
        let nearly = datetime!(2026-09-25 22:02:09.98 +00:00);
        assert_eq!(
            nearly.format(&super::LOG_TIMESTAMP).unwrap(),
            "2026-09-25T22:02:09.9+00:00"
        );
    }

    #[test]
    fn utc_is_the_default_and_its_spellings() {
        for spec in ["utc", "UTC", "Utc", "z", "Z", ""] {
            assert_eq!(parse_log_timezone(spec, None).unwrap(), UtcOffset::UTC, "{spec:?}");
        }
    }

    /// Buenos Aires: UTC-3, and no daylight saving since 2009, so a fixed
    /// offset is correct for it year-round.
    #[test]
    fn a_signed_offset_parses_in_every_accepted_spelling() {
        for spec in ["-03:00", "-0300", "-03"] {
            assert_eq!(parse_log_timezone(spec, None).unwrap(), offset(-3, 0), "{spec:?}");
        }
        for spec in ["+05:30", "+0530"] {
            assert_eq!(parse_log_timezone(spec, None).unwrap(), offset(5, 30), "{spec:?}");
        }
        assert_eq!(parse_log_timezone("+00:00", None).unwrap(), UtcOffset::UTC);
    }

    /// The sign applies to the minutes too. `-03:30` is three and a half
    /// hours *behind* UTC, not three hours behind and thirty minutes ahead --
    /// which is what `from_hms(-3, 30, 0)` would mean.
    #[test]
    fn the_sign_carries_to_the_minutes() {
        let parsed = parse_log_timezone("-03:30", None).unwrap();
        assert_eq!(parsed, offset(-3, -30));
        let (h, m, _) = parsed.as_hms();
        assert_eq!((h, m), (-3, -30));
    }

    /// `local` without a resolved offset is an error, not a silent fallback
    /// to UTC: a timestamp in a timezone the operator did not ask for is
    /// worse than a refusal to start.
    #[test]
    fn local_without_a_resolvable_offset_is_an_error() {
        let err = parse_log_timezone("local", None).unwrap_err().to_string();
        assert!(err.contains("could not be determined"), "{err}");
        assert!(err.contains("-03:00"), "the error should suggest the way out: {err}");

        assert_eq!(parse_log_timezone("local", Some(offset(-3, 0))).unwrap(), offset(-3, 0));
    }

    /// clap must accept a value that begins with `-`.
    ///
    /// Without `allow_hyphen_values`, `--log-timezone -03:00` fails with
    /// "unexpected argument '-0' found" -- clap reads the value as another
    /// flag. Found by running the binary; no unit test on the parser could
    /// have caught it, because the parser is never reached.
    #[test]
    fn a_negative_offset_survives_argument_parsing() {
        use clap::Parser;
        let args = super::Args::try_parse_from([
            "rustock-cli",
            "--log-timezone",
            "-03:00",
        ])
        .expect("a leading-minus value must parse");
        assert_eq!(args.log_timezone, "-03:00");
        assert_eq!(parse_log_timezone(&args.log_timezone, None).unwrap(), offset(-3, 0));

        // The `=` form has to keep working too.
        let args =
            super::Args::try_parse_from(["rustock-cli", "--log-timezone=-0300"]).unwrap();
        assert_eq!(parse_log_timezone(&args.log_timezone, None).unwrap(), offset(-3, 0));

        // And the default is still UTC when the flag is absent.
        let args = super::Args::try_parse_from(["rustock-cli"]).unwrap();
        assert_eq!(args.log_timezone, "utc");
    }

    #[test]
    fn nonsense_is_rejected_with_a_usable_message() {
        for spec in [
            "America/Argentina/Buenos_Aires",
            "-3",       // one digit
            "03:00",    // unsigned
            "-03:99",   // minutes out of range
            "-0:0",     // one digit either side
            "-1:2",     // would become -12:00 if colons were merely stripped
            "-03:0",
            "-3:00",
            "abc",
        ] {
            let err = parse_log_timezone(spec, None).unwrap_err().to_string();
            assert!(err.contains(spec), "the message should quote the input: {err}");
        }
        // An IANA name is the most likely wrong guess, so make sure it fails
        // loudly rather than being read as some prefix.
        assert!(parse_log_timezone("America/Argentina/Buenos_Aires", None).is_err());
    }
}
