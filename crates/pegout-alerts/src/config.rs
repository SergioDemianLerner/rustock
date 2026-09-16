//! Configuration, read from a TOML file.
//!
//! Every threshold and address is configurable because the sensible value is an
//! operational judgement, not a property of the chain. Defaults are set so that
//! a file with only `enabled = true` behaves the way the feature was asked for:
//! alert above 100 BTC per peg-out and per output, and above 200 BTC in transit.

use serde::Deserialize;
use std::path::Path;

/// One BTC in satoshis. Thresholds are given in BTC in the file because that is
/// how people think about peg-outs; everything internal is satoshis.
pub const SATOSHIS_PER_BTC: u64 = 100_000_000;

fn default_true() -> bool { true }
fn default_poll_secs() -> u64 { 5 }
fn default_pegout_btc() -> f64 { 100.0 }
fn default_output_btc() -> f64 { 100.0 }
fn default_in_transit_btc() -> f64 { 200.0 }
fn default_confirmations() -> u64 { 4_000 }
fn default_smtp_port() -> u16 { 587 }
fn default_queue() -> usize { 256 }

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub pegout_alerts: PegoutAlerts,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PegoutAlerts {
    /// Off unless asked for: a node that has not been configured for alerting
    /// should not start a watcher.
    #[serde(default)]
    pub enabled: bool,
    /// How often to look for newly executed blocks.
    #[serde(default = "default_poll_secs")]
    pub poll_interval_secs: u64,
    /// Where to begin. `None` starts at the node's current executed head, which
    /// is what you want when switching this on for the first time — otherwise
    /// it would replay and re-alert the entire chain.
    #[serde(default)]
    pub start_block: Option<u64>,
    /// Alert when a single peg-out request exceeds this.
    #[serde(default = "default_pegout_btc")]
    pub pegout_alert_btc: f64,
    /// Alert when any non-change output of a peg-out BTC transaction exceeds
    /// this.
    #[serde(default = "default_output_btc")]
    pub output_alert_btc: f64,
    /// Alert when the total value of peg-outs awaiting confirmation exceeds
    /// this.
    #[serde(default = "default_in_transit_btc")]
    pub in_transit_alert_btc: f64,
    /// Confirmations a peg-out needs before it leaves "in transit". Mainnet
    /// uses 4,000; it is configurable because testnet and regtest do not.
    #[serde(default = "default_confirmations")]
    pub confirmations: u64,
    /// Bound on undelivered alerts. Alerts are dropped rather than queued
    /// without limit if a mail server is unreachable for a long time; the drop
    /// is counted and logged.
    #[serde(default = "default_queue")]
    pub max_queued_alerts: usize,
    /// scriptPubKeys (hex) that are federation change, not a transfer to an end
    /// user. Outputs paying these are excluded from the per-output check and
    /// from the in-transit total.
    ///
    /// Empty by default, which means **every** output is treated as a user
    /// output. That over-alerts on a large change output; it can never hide a
    /// real transfer. Fill this in to silence the change output precisely.
    #[serde(default)]
    pub federation_change_scripts: Vec<String>,
    #[serde(default)]
    pub email: EmailConfig,
}

impl Default for PegoutAlerts {
    fn default() -> Self {
        Self {
            enabled: false,
            poll_interval_secs: default_poll_secs(),
            start_block: None,
            pegout_alert_btc: default_pegout_btc(),
            output_alert_btc: default_output_btc(),
            in_transit_alert_btc: default_in_transit_btc(),
            confirmations: default_confirmations(),
            max_queued_alerts: default_queue(),
            federation_change_scripts: Vec::new(),
            email: EmailConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct EmailConfig {
    /// Path to a `KEY=VALUE` file holding the secrets this section refers to.
    ///
    /// Credentials must never be written into the configuration file, because
    /// that file lives next to the code and gets copied, diffed and pasted.
    /// Put them in a file owned by the node user with mode 0600 and refer to
    /// them here as `${SMTP_PASSWORD}`; the watcher expands those at startup
    /// and the expanded values are never logged.
    #[serde(default)]
    pub env_file: Option<String>,
    /// When false (the default) alerts are logged and nothing is sent, which is
    /// how this runs until SMTP credentials are supplied.
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub to: Vec<String>,
    #[serde(default)]
    pub from: String,
    #[serde(default)]
    pub smtp_host: String,
    #[serde(default = "default_smtp_port")]
    pub smtp_port: u16,
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub password: Option<String>,
    /// `none`, `starttls` or `implicit`.
    #[serde(default)]
    pub tls: TlsMode,
    #[serde(default = "default_true")]
    pub subject_prefix_enabled: bool,
}

#[derive(Debug, Clone, Copy, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum TlsMode {
    None,
    #[default]
    Starttls,
    Implicit,
}

impl PegoutAlerts {
    pub fn pegout_alert_sats(&self) -> u64 { btc_to_sats(self.pegout_alert_btc) }
    pub fn output_alert_sats(&self) -> u64 { btc_to_sats(self.output_alert_btc) }
    pub fn in_transit_alert_sats(&self) -> u64 { btc_to_sats(self.in_transit_alert_btc) }

    /// Load the referenced env file and expand every `${NAME}` in the email
    /// section. Runs before `validate`, so a reference that resolves to an
    /// empty string is caught as a missing setting.
    pub fn resolve_secrets(&mut self) -> anyhow::Result<()> {
        let vars = match &self.email.env_file {
            Some(p) => read_env_file(Path::new(p))?,
            None => std::collections::HashMap::new(),
        };
        self.email.from = expand(&self.email.from, &vars)?;
        self.email.smtp_host = expand(&self.email.smtp_host, &vars)?;
        for t in self.email.to.iter_mut() {
            *t = expand(t, &vars)?;
        }
        if let Some(u) = &self.email.username {
            self.email.username = Some(expand(u, &vars)?);
        }
        if let Some(p) = &self.email.password {
            self.email.password = Some(expand(p, &vars)?);
        }
        Ok(())
    }

    /// Reject a configuration that cannot do what it claims, at startup rather
    /// than at the moment an alert needs to go out.
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.email.enabled {
            if self.email.to.is_empty() {
                anyhow::bail!("pegout_alerts.email.enabled is true but `to` is empty");
            }
            if self.email.from.trim().is_empty() {
                anyhow::bail!("pegout_alerts.email.enabled is true but `from` is empty");
            }
            if self.email.smtp_host.trim().is_empty() {
                anyhow::bail!("pegout_alerts.email.enabled is true but `smtp_host` is empty");
            }
        }
        if self.poll_interval_secs == 0 {
            anyhow::bail!("pegout_alerts.poll_interval_secs must be greater than zero");
        }
        Ok(())
    }
}

/// BTC (as written in the config) to satoshis, rounded to the nearest satoshi.
pub fn btc_to_sats(btc: f64) -> u64 {
    (btc * SATOSHIS_PER_BTC as f64).round().max(0.0) as u64
}

/// Satoshis to a BTC string, for humans reading an alert.
pub fn sats_to_btc_string(sats: u64) -> String {
    format!("{}.{:08}", sats / SATOSHIS_PER_BTC, sats % SATOSHIS_PER_BTC)
}

impl Config {
    pub fn load(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let path = path.as_ref();
        let text = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("reading {}: {e}", path.display()))?;
        let mut cfg: Config = toml::from_str(&text)
            .map_err(|e| anyhow::anyhow!("parsing {}: {e}", path.display()))?;
        cfg.pegout_alerts.resolve_secrets()?;
        cfg.pegout_alerts.validate()?;
        Ok(cfg)
    }
}

/// Read a `KEY=VALUE` file. Blank lines and `#` comments are skipped, values
/// may be quoted, and a `export ` prefix is tolerated so the same file can be
/// sourced by a shell.
fn read_env_file(path: &Path) -> anyhow::Result<std::collections::HashMap<String, String>> {
    let meta = std::fs::metadata(path)
        .map_err(|e| anyhow::anyhow!("reading {}: {e}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = meta.permissions().mode() & 0o077;
        if mode != 0 {
            // Refuse rather than warn: a credential readable by other users on
            // the box is already disclosed, and continuing would imply it is
            // not.
            anyhow::bail!(
                "{} is readable by group or others (mode {:o}); chmod 600 it",
                path.display(),
                meta.permissions().mode() & 0o777
            );
        }
    }
    let text = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("reading {}: {e}", path.display()))?;
    let mut out = std::collections::HashMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line);
        if let Some((k, v)) = line.split_once('=') {
            let v = v.trim();
            let v = v
                .strip_prefix('"').and_then(|x| x.strip_suffix('"'))
                .or_else(|| v.strip_prefix('\'').and_then(|x| x.strip_suffix('\'')))
                .unwrap_or(v);
            out.insert(k.trim().to_string(), v.to_string());
        }
    }
    Ok(out)
}

/// Replace every `${NAME}` with a value from `vars`, falling back to the
/// process environment. An unresolved reference is an error: silently leaving
/// `${SMTP_PASSWORD}` as the literal password would authenticate as that string
/// and fail with a confusing message much later.
fn expand(value: &str, vars: &std::collections::HashMap<String, String>) -> anyhow::Result<String> {
    let mut out = String::with_capacity(value.len());
    let mut rest = value;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find('}') else {
            anyhow::bail!("unterminated ${{ in configuration value");
        };
        let name = &after[..end];
        let resolved = vars
            .get(name)
            .cloned()
            .or_else(|| std::env::var(name).ok())
            .ok_or_else(|| anyhow::anyhow!("${{{name}}} is not set in the env file or environment"))?;
        out.push_str(&resolved);
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_the_requested_thresholds() {
        let c = PegoutAlerts::default();
        assert_eq!(c.pegout_alert_sats(), 100 * SATOSHIS_PER_BTC);
        assert_eq!(c.output_alert_sats(), 100 * SATOSHIS_PER_BTC);
        assert_eq!(c.in_transit_alert_sats(), 200 * SATOSHIS_PER_BTC);
        assert_eq!(c.confirmations, 4_000);
        assert!(!c.enabled, "must be off unless asked for");
        assert!(!c.email.enabled, "must not mail unless asked for");
    }

    #[test]
    fn a_minimal_file_parses() {
        let c: Config = toml::from_str("[pegout_alerts]\nenabled = true\n").unwrap();
        assert!(c.pegout_alerts.enabled);
        assert_eq!(c.pegout_alerts.pegout_alert_sats(), 100 * SATOSHIS_PER_BTC);
    }

    #[test]
    fn thresholds_are_overridable() {
        let c: Config = toml::from_str(
            "[pegout_alerts]\nenabled = true\npegout_alert_btc = 12.5\nin_transit_alert_btc = 0.5\n",
        ).unwrap();
        assert_eq!(c.pegout_alerts.pegout_alert_sats(), 1_250_000_000);
        assert_eq!(c.pegout_alerts.in_transit_alert_sats(), 50_000_000);
    }

    #[test]
    fn a_typo_is_rejected_rather_than_ignored() {
        // deny_unknown_fields: a misspelled threshold must not silently keep
        // the default and leave the operator believing it took effect.
        let r: Result<Config, _> =
            toml::from_str("[pegout_alerts]\nenabled = true\npegout_alert_bt = 5.0\n");
        assert!(r.is_err());
    }

    #[test]
    fn email_without_a_recipient_is_rejected() {
        let c: Config = toml::from_str(
            "[pegout_alerts]\nenabled = true\n[pegout_alerts.email]\nenabled = true\nfrom = \"a@b\"\nsmtp_host = \"h\"\n",
        ).unwrap();
        assert!(c.pegout_alerts.validate().is_err());
    }

    #[test]
    fn btc_formatting_is_exact() {
        assert_eq!(sats_to_btc_string(100 * SATOSHIS_PER_BTC), "100.00000000");
        assert_eq!(sats_to_btc_string(1), "0.00000001");
        assert_eq!(sats_to_btc_string(250_000_000), "2.50000000");
    }
}

#[cfg(test)]
mod example_file_tests {
    /// The shipped example must stay valid: an example that does not parse is
    /// worse than none.
    #[test]
    fn the_shipped_example_parses_and_validates() {
        let text = include_str!("../../../pegout-alerts.example.toml");
        let cfg: super::Config = toml::from_str(text).expect("example must parse");
        cfg.pegout_alerts.validate().expect("example must validate");
        assert!(cfg.pegout_alerts.enabled);
        assert!(!cfg.pegout_alerts.email.enabled, "example must not mail by default");
    }
}

#[cfg(test)]
mod secret_tests {
    use super::*;
    use std::io::Write;

    fn env_file(body: &str, mode: u32) -> tempfile::NamedTempFile {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(body.as_bytes()).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(f.path(), std::fs::Permissions::from_mode(mode)).unwrap();
        }
        f
    }

    #[test]
    fn secrets_come_from_the_env_file_not_the_config() {
        let f = env_file("SMTP_USER=someuser\nSMTP_PASSWORD=s3cr3t\n", 0o600);
        let mut c = PegoutAlerts {
            email: EmailConfig {
                enabled: true,
                env_file: Some(f.path().display().to_string()),
                to: vec!["ops@example.com".into()],
                from: "node@example.com".into(),
                smtp_host: "smtp.example.com".into(),
                username: Some("${SMTP_USER}".into()),
                password: Some("${SMTP_PASSWORD}".into()),
                ..Default::default()
            },
            ..Default::default()
        };
        c.resolve_secrets().unwrap();
        assert_eq!(c.email.username.as_deref(), Some("someuser"));
        assert_eq!(c.email.password.as_deref(), Some("s3cr3t"));
    }

    #[test]
    fn a_world_readable_env_file_is_refused() {
        let f = env_file("SMTP_PASSWORD=s3cr3t\n", 0o644);
        let mut c = PegoutAlerts {
            email: EmailConfig {
                enabled: true,
                env_file: Some(f.path().display().to_string()),
                password: Some("${SMTP_PASSWORD}".into()),
                ..Default::default()
            },
            ..Default::default()
        };
        let err = c.resolve_secrets().unwrap_err().to_string();
        assert!(err.contains("readable by group or others"), "{err}");
    }

    #[test]
    fn an_unresolved_reference_is_an_error_not_a_literal_password() {
        let f = env_file("SMTP_USER=someuser\n", 0o600);
        let mut c = PegoutAlerts {
            email: EmailConfig {
                enabled: true,
                env_file: Some(f.path().display().to_string()),
                password: Some("${SMTP_PASSWORD}".into()),
                ..Default::default()
            },
            ..Default::default()
        };
        let err = c.resolve_secrets().unwrap_err().to_string();
        assert!(err.contains("SMTP_PASSWORD"), "{err}");
    }

    #[test]
    fn quoted_and_exported_lines_parse() {
        let f = env_file("export SMTP_USER=\"quoted user\"\n# a comment\n\nSMTP_PASSWORD=plain\n", 0o600);
        let vars = read_env_file(f.path()).unwrap();
        assert_eq!(vars.get("SMTP_USER").unwrap(), "quoted user");
        assert_eq!(vars.get("SMTP_PASSWORD").unwrap(), "plain");
    }

    #[test]
    fn a_from_address_with_a_display_name_survives_expansion() {
        let f = env_file("SMTP_FROM=Alerts <alerts@example.com>\n", 0o600);
        let vars = read_env_file(f.path()).unwrap();
        assert_eq!(expand("${SMTP_FROM}", &vars).unwrap(), "Alerts <alerts@example.com>");
    }
}
