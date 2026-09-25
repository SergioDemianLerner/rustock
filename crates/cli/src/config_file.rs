//! Node settings from a TOML file, underneath the command line.
//!
//! # Precedence
//!
//! ```text
//!   command line   >   config file   >   built-in default
//! ```
//!
//! That order is the point. An operator debugging a live node must be able to
//! override a file setting with a flag without editing the file, and every
//! existing invocation must keep working unchanged.
//!
//! Getting it right needs more than "apply the file, then apply the flags":
//! with clap's derive, a flag that was never passed still arrives carrying its
//! default, indistinguishable from one the user typed. So the merge asks clap
//! where each value came from (`ValueSource`) and leaves anything typed on the
//! command line alone.
//!
//! # What belongs here
//!
//! Runtime settings only. The binary also carries about thirty one-shot
//! operations -- `--import-rskj`, `--repair`, `--trie-tool`, `--probe-unitrie`
//! -- which run and then exit. Those have no place in a config file: a node
//! that re-ran an import on every start would be a trap, not a convenience.
//! `ONE_SHOT` lists them, and a test asserts every other flag is configurable,
//! so a new setting cannot be added without deciding which it is.

use clap::parser::ValueSource;
use clap::ArgMatches;
use serde::Deserialize;

/// Settings the file may supply. Every field is optional: absent means "leave
/// whatever the command line or the default decided".
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileConfig {
    #[serde(default)]
    pub node: NodeSection,
    #[serde(default)]
    pub rpc: RpcSection,
    #[serde(default)]
    pub peers: PeersSection,
    #[serde(default)]
    pub trie: TrieSection,
    #[serde(default)]
    pub gc: GcSection,
    #[serde(default)]
    pub prune: PruneSection,
    #[serde(default)]
    pub mining: MiningSection,
    #[serde(default)]
    pub account_tx_rate_limit: RateLimitSection,
    #[serde(default)]
    pub log: LogSection,
    #[serde(default)]
    pub alerts: AlertsSection,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeSection {
    pub port: Option<u16>,
    pub data_dir: Option<String>,
    pub network_id: Option<u64>,
    pub secret_key: Option<String>,
    /// Parsed as an IP address, matching the flag's type -- a malformed value
    /// is rejected when the file is read rather than misused later.
    pub external_ip: Option<std::net::IpAddr>,
    pub supply_check: Option<String>,
    /// `"on"` / `"off"`, as the flag takes.
    pub rskj_multisig_senders: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RpcSection {
    pub host: Option<String>,
    pub port: Option<u16>,
    pub admin: Option<bool>,
    pub disabled: Option<bool>,
    /// Serve JSON-RPC over WebSocket too (rskj `providers.web.ws.enabled`).
    pub ws: Option<bool>,
    /// WebSocket port (rskj `providers.web.ws.port`, default 4445).
    pub ws_port: Option<u16>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PeersSection {
    pub max_peers: Option<usize>,
    pub max_inbound_peers: Option<usize>,
    pub max_inbound_per_ip: Option<usize>,
    pub max_inbound_per_cidr: Option<usize>,
    pub inbound_cidr_prefix: Option<u8>,
    /// Addresses or CIDR blocks refused at connection time, as rskj's
    /// `peer.bannedPeerIPs`.
    pub banned_peers: Option<Vec<String>>,
    /// Count scoring events but never punish (rskj
    /// `scoring.punishmentEnabled = false`).
    pub no_peer_punishment: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TrieSection {
    pub backend: Option<String>,
    pub dir: Option<String>,
    pub node: Option<String>,
    /// Entries in the BTC stored-block cache; 0 disables it.
    pub btc_block_cache_entries: Option<usize>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GcSection {
    pub epochs: Option<usize>,
    pub rotate_mb: Option<u64>,
    pub burial: Option<u64>,
    pub check_secs: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PruneSection {
    pub keep_depth: Option<u64>,
    pub max_batch: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MiningSection {
    pub enabled: Option<bool>,
    pub coinbase: Option<String>,
    pub extra_data: Option<String>,
    pub refresh_secs: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RateLimitSection {
    pub enabled: Option<bool>,
    pub cleaner_period: Option<i64>,
    pub max_accounts: Option<usize>,
    pub quota_multiplier: Option<u64>,
    pub gas_per_second_percent: Option<f64>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LogSection {
    pub level: Option<String>,
    pub to_stdout: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AlertsSection {
    pub pegout_alerts_config: Option<String>,
}

impl FileConfig {
    pub fn load(path: &str) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("reading {path}: {e}"))?;
        // `deny_unknown_fields` throughout: a misspelled key must be an error,
        // not a setting silently left at its default. Someone who writes
        // `keep_dept = 500` deserves to be told, not to discover it months
        // later when the disk fills.
        toml::from_str(&text).map_err(|e| anyhow::anyhow!("parsing {path}: {e}"))
    }
}

/// Apply `file` to `target` for one setting, unless the command line supplied
/// it. `id` is clap's argument id, which for the derive API is the field name.
pub fn apply<T: Clone>(matches: &ArgMatches, id: &str, file: Option<&T>, target: &mut T) {
    if matches.value_source(id) == Some(ValueSource::CommandLine) {
        return; // typed by the operator; the file does not get to override it
    }
    if let Some(value) = file {
        *target = value.clone();
    }
}

/// Same, for a setting whose flag is `Option<T>` (no default).
pub fn apply_opt<T: Clone>(
    matches: &ArgMatches,
    id: &str,
    file: Option<&T>,
    target: &mut Option<T>,
) {
    if matches.value_source(id) == Some(ValueSource::CommandLine) {
        return;
    }
    if let Some(value) = file {
        *target = Some(value.clone());
    }
}

/// One-shot operations: they run and exit, so they are command-line only.
/// Listing them explicitly (rather than inferring) means adding a flag forces a
/// decision about which kind it is -- see `every_runtime_flag_is_configurable`.
pub const ONE_SHOT: &[&str] = &[
    "build_bridge_index",
    "build_height_index",
    "import_rskj_receipts",
    "receipts_from",
    "receipts_to",
    "repair",
    "repair_from",
    "repair_to",
    "import_blocks_db",
    "import_rskj",
    "import_metrics",
    "import_skip_existing",
    "import_enable_wal",
    "import_write_buffer_mb",
    "import_auto_compaction",
    "import_parallelism",
    "import_unitrie_threads",
    "probe_unitrie",
    "probe_source",
    "import_metadata_only",
    "metadata_range",
    "import_index_dump",
    "metadata_by_walk",
    "metadata_in_memory",
    "trie_tool",
    "trie_tool_block",
    "trie_tool_progress",
    "trie_tool_source",
    "trie_tool_copy",
    "trie_tool_copy_overwrite",
    "config",
];

/// Every runtime setting the file can supply. Kept beside the merge so the two
/// cannot drift; the test below asserts it covers every flag that is not
/// one-shot.
pub const CONFIGURABLE: &[&str] = &[
    "port",
    "data_dir",
    "network_id",
    "secret_key",
    "external_ip",
    "supply_check",
    "rskj_multisig_senders",
    "rpc_host",
    "rpc_port",
    "rpc_admin",
    "no_rpc",
    "ws",
    "ws_port",
    "max_peers",
    "max_inbound_peers",
    "max_inbound_per_ip",
    "max_inbound_per_cidr",
    "inbound_cidr_prefix",
    "banned_peers",
    "no_peer_punishment",
    "btc_block_cache_entries",
    "trie_backend",
    "trie_dir",
    "trie_node",
    "gc_epochs",
    "gc_rotate_mb",
    "gc_burial",
    "gc_check_secs",
    "prune_keep_depth",
    "prune_max_batch",
    "mine",
    "mining_coinbase",
    "mining_extra_data",
    "mining_refresh_secs",
    "account_tx_rate_limit",
    "account_tx_rate_limit_cleaner_period",
    "account_tx_rate_limit_max_accounts",
    "account_tx_rate_limit_quota_multiplier",
    "account_tx_rate_limit_gas_per_second_percent",
    "log_level",
    "log_to_stdout",
    "pegout_alerts_config",
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Args;
    use clap::{CommandFactory, FromArgMatches};

    fn parse(argv: &[&str]) -> (clap::ArgMatches, Args) {
        let matches = Args::command().get_matches_from(argv);
        let args = Args::from_arg_matches(&matches).expect("parses");
        (matches, args)
    }

    fn write(toml_text: &str) -> tempfile::NamedTempFile {
        use std::io::Write;
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(toml_text.as_bytes()).unwrap();
        f.flush().unwrap();
        f
    }

    /// The precedence the whole design turns on. An operator debugging a live
    /// node must be able to override a file setting with a flag without
    /// editing the file.
    #[test]
    fn command_line_beats_file_beats_default() {
        let file = write(
            r#"
            [node]
            port = 40404
            [gc]
            burial = 9999
            "#,
        );
        let path = file.path().to_str().unwrap();

        // File only: the file wins over the default (30303).
        let (m, mut a) = parse(&["rustock", "--config", path]);
        crate::apply_file_config(&m, &FileConfig::load(path).unwrap(), &mut a);
        assert_eq!(a.port, 40404, "file beats default");
        assert_eq!(a.gc_burial, 9999);

        // Flag as well: the flag wins over the file.
        let (m, mut a) = parse(&["rustock", "--config", path, "--port", "50505"]);
        crate::apply_file_config(&m, &FileConfig::load(path).unwrap(), &mut a);
        assert_eq!(a.port, 50505, "command line beats file");
        assert_eq!(a.gc_burial, 9999, "and leaves the rest of the file alone");
    }

    /// The subtle case, and the reason the merge consults `ValueSource` rather
    /// than comparing against the default: a flag typed with *exactly* its
    /// default value must still beat the file. Comparing values could not tell
    /// `--port 30303` from not passing `--port` at all.
    #[test]
    fn a_flag_typed_with_its_default_value_still_beats_the_file() {
        let file = write("[node]\nport = 40404\n");
        let path = file.path().to_str().unwrap();
        let (m, mut a) = parse(&["rustock", "--config", path, "--port", "30303"]);
        crate::apply_file_config(&m, &FileConfig::load(path).unwrap(), &mut a);
        assert_eq!(a.port, 30303, "explicitly typed, even though it is the default");
    }

    /// An empty file changes nothing.
    #[test]
    fn an_empty_file_leaves_every_default_alone() {
        let file = write("");
        let path = file.path().to_str().unwrap();
        let (m, mut a) = parse(&["rustock", "--config", path]);
        let before = a.port;
        crate::apply_file_config(&m, &FileConfig::load(path).unwrap(), &mut a);
        assert_eq!(a.port, before);
        assert_eq!(a.gc_burial, 4000);
    }

    /// A typo must be an error. Someone who writes `keep_dept` deserves to be
    /// told, not to discover months later that pruning used the default.
    #[test]
    fn an_unknown_key_is_an_error() {
        let file = write("[prune]\nkeep_dept = 500\n");
        let err = FileConfig::load(file.path().to_str().unwrap()).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("keep_dept"), "the error must name the key: {msg}");
    }

    /// Including an unknown *section*.
    #[test]
    fn an_unknown_section_is_an_error() {
        let file = write("[prunning]\nkeep_depth = 500\n");
        assert!(FileConfig::load(file.path().to_str().unwrap()).is_err());
    }

    /// A value of the wrong type is an error too, and names the setting.
    #[test]
    fn a_malformed_value_is_an_error() {
        let file = write("[node]\nport = \"thirty thousand\"\n");
        let err = FileConfig::load(file.path().to_str().unwrap()).unwrap_err();
        assert!(err.to_string().contains("port"), "{err}");
    }

    /// A missing file is an error naming the path, not a silent fallback to
    /// defaults -- an operator who passes --config means it.
    #[test]
    fn a_missing_file_is_an_error() {
        let err = FileConfig::load("/nonexistent/rustock/node.toml").unwrap_err();
        assert!(err.to_string().contains("/nonexistent/rustock/node.toml"), "{err}");
    }

    /// The drift guard. Every flag is either a runtime setting the file can
    /// supply, or a one-shot operation that is command-line only. Adding a
    /// flag without deciding which fails here, so the config cannot silently
    /// fall behind the command line.
    #[test]
    fn every_runtime_flag_is_configurable() {
        let cmd = Args::command();
        let mut unclassified = Vec::new();
        for arg in cmd.get_arguments() {
            let id = arg.get_id().as_str();
            if id == "help" || id == "version" {
                continue;
            }
            if ONE_SHOT.contains(&id) || CONFIGURABLE.contains(&id) {
                continue;
            }
            unclassified.push(id.to_string());
        }
        assert!(
            unclassified.is_empty(),
            "these flags are in neither ONE_SHOT nor CONFIGURABLE -- decide which \
             each one is: {unclassified:?}"
        );
    }

    /// And the reverse: nothing is listed that is not a real flag, so the
    /// lists cannot rot as flags are renamed or removed.
    #[test]
    fn the_configurable_list_names_only_real_flags() {
        let cmd = Args::command();
        let ids: Vec<String> = cmd
            .get_arguments()
            .map(|a| a.get_id().as_str().to_string())
            .collect();
        for listed in CONFIGURABLE.iter().chain(ONE_SHOT.iter()) {
            assert!(
                ids.iter().any(|id| id == listed),
                "`{listed}` is listed but is not a flag"
            );
        }
    }

    /// The shipped example must parse, and must exercise every section.
    ///
    /// Without this the example rots: a renamed key or a new section would
    /// leave the documented file quietly wrong, and `deny_unknown_fields`
    /// would then reject it for anyone who copied it.
    #[test]
    fn the_example_file_parses_and_covers_every_section() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../node.example.toml");
        let cfg = FileConfig::load(path)
            .unwrap_or_else(|e| panic!("node.example.toml does not parse: {e}"));

        // Every section must actually be present, or the example stops being a
        // reference for the settings it omits.
        assert!(cfg.node.port.is_some(), "[node]");
        assert!(cfg.rpc.port.is_some(), "[rpc]");
        assert!(cfg.peers.max_peers.is_some(), "[peers]");
        assert!(cfg.trie.backend.is_some(), "[trie]");
        assert!(cfg.gc.burial.is_some(), "[gc]");
        assert!(cfg.prune.keep_depth.is_some(), "[prune]");
        assert!(cfg.mining.enabled.is_some(), "[mining]");
        assert!(cfg.account_tx_rate_limit.enabled.is_some(), "[account_tx_rate_limit]");
        assert!(cfg.log.level.is_some(), "[log]");
    }

    /// No flag may be in both lists; the two are a partition.
    #[test]
    fn the_two_lists_do_not_overlap() {
        for id in CONFIGURABLE {
            assert!(!ONE_SHOT.contains(id), "`{id}` is in both lists");
        }
    }
}
