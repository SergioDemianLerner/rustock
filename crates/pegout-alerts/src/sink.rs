//! Where an alert goes.
//!
//! Sending happens on the watcher's own task, never on a path the node needs.
//! A sink that blocks — an unreachable SMTP server, a slow DNS lookup — delays
//! only further alerts, and the queue that feeds it is bounded so a long
//! outage drops alerts rather than growing without limit.

use crate::alert::Alert;
use crate::config::{EmailConfig, TlsMode};

/// Somewhere an alert can be delivered.
pub trait AlertSink: Send + Sync {
    /// Deliver one alert. Errors are reported by the caller and never
    /// propagated into the node.
    fn deliver(&self, alert: &Alert) -> anyhow::Result<()>;
    fn name(&self) -> &'static str;
}

/// Always on. Every peg-out and every alert is logged whether or not mail is
/// configured, so the record exists even when delivery fails.
pub struct LogSink;

impl AlertSink for LogSink {
    fn deliver(&self, alert: &Alert) -> anyhow::Result<()> {
        tracing::warn!(target: "rustock::pegout_alerts", "ALERT: {}", alert.subject());
        tracing::info!(target: "rustock::pegout_alerts", "{}", alert.body());
        Ok(())
    }
    fn name(&self) -> &'static str { "log" }
}

/// SMTP delivery.
///
/// Built once and reused. Credentials are taken from configuration and never
/// logged; a delivery failure reports the server's message, which may name the
/// recipient but not the password.
pub struct SmtpSink {
    to: Vec<lettre::message::Mailbox>,
    from: lettre::message::Mailbox,
    transport: lettre::SmtpTransport,
    subject_prefix: Option<&'static str>,
}

impl SmtpSink {
    pub fn new(cfg: &EmailConfig) -> anyhow::Result<Self> {
        use lettre::transport::smtp::authentication::Credentials;

        let from: lettre::message::Mailbox = cfg
            .from
            .parse()
            .map_err(|e| anyhow::anyhow!("pegout_alerts.email.from is not an address: {e}"))?;
        let to = cfg
            .to
            .iter()
            .map(|t| {
                t.parse::<lettre::message::Mailbox>()
                    .map_err(|e| anyhow::anyhow!("pegout_alerts.email.to contains {t:?}: {e}"))
            })
            .collect::<anyhow::Result<Vec<_>>>()?;

        let builder = match cfg.tls {
            TlsMode::None => {
                lettre::SmtpTransport::builder_dangerous(&cfg.smtp_host).port(cfg.smtp_port)
            }
            TlsMode::Starttls => lettre::SmtpTransport::starttls_relay(&cfg.smtp_host)
                .map_err(|e| anyhow::anyhow!("SMTP STARTTLS setup for {}: {e}", cfg.smtp_host))?
                .port(cfg.smtp_port),
            TlsMode::Implicit => lettre::SmtpTransport::relay(&cfg.smtp_host)
                .map_err(|e| anyhow::anyhow!("SMTP TLS setup for {}: {e}", cfg.smtp_host))?
                .port(cfg.smtp_port),
        };

        let builder = match (&cfg.username, &cfg.password) {
            (Some(u), Some(p)) => builder.credentials(Credentials::new(u.clone(), p.clone())),
            _ => builder,
        };

        Ok(Self {
            to,
            from,
            transport: builder.build(),
            subject_prefix: cfg.subject_prefix_enabled.then_some("[rustock peg-out] "),
        })
    }
}

impl AlertSink for SmtpSink {
    fn deliver(&self, alert: &Alert) -> anyhow::Result<()> {
        use lettre::Transport;
        let subject = match self.subject_prefix {
            Some(p) => format!("{p}{}", alert.subject()),
            None => alert.subject(),
        };
        for to in &self.to {
            let email = lettre::Message::builder()
                .from(self.from.clone())
                .to(to.clone())
                .subject(subject.clone())
                .body(alert.body())
                .map_err(|e| anyhow::anyhow!("building the message: {e}"))?;
            self.transport
                .send(&email)
                .map_err(|e| anyhow::anyhow!("sending to {to}: {e}"))?;
        }
        Ok(())
    }
    fn name(&self) -> &'static str { "smtp" }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::EmailConfig;

    /// Records what it was given, so the watcher's behaviour can be tested
    /// without a mail server.
    pub struct RecordingSink(pub std::sync::Mutex<Vec<String>>);
    impl AlertSink for RecordingSink {
        fn deliver(&self, alert: &Alert) -> anyhow::Result<()> {
            self.0.lock().unwrap().push(alert.subject());
            Ok(())
        }
        fn name(&self) -> &'static str { "recording" }
    }

    #[test]
    fn the_log_sink_never_fails() {
        let a = Alert::InTransitAboveThreshold { block: 1, total_sats: 2, threshold_sats: 1, pegout_count: 1 };
        assert!(LogSink.deliver(&a).is_ok());
    }

    #[test]
    fn an_unparseable_from_address_is_rejected_at_construction() {
        let cfg = EmailConfig {
            enabled: true,
            to: vec!["ops@example.com".into()],
            from: "not an address".into(),
            smtp_host: "localhost".into(),
            ..Default::default()
        };
        assert!(SmtpSink::new(&cfg).is_err(), "bad `from` must fail at startup, not at alert time");
    }

    #[test]
    fn a_valid_config_builds_without_contacting_the_server() {
        let cfg = EmailConfig {
            enabled: true,
            to: vec!["ops@example.com".into()],
            from: "node@example.com".into(),
            smtp_host: "smtp.example.com".into(),
            smtp_port: 587,
            tls: TlsMode::Starttls,
            ..Default::default()
        };
        assert!(SmtpSink::new(&cfg).is_ok());
    }
}
