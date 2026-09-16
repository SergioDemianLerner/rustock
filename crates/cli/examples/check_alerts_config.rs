//! Validate a peg-out alert configuration without starting a node or sending
//! mail: parse it, resolve `${NAME}` references against the env file, and
//! report what would happen. Secrets are never printed — only whether they
//! resolved and how long they are.
//!
//! Usage: check_alerts_config <config.toml>

fn main() -> anyhow::Result<()> {
    let path = std::env::args().nth(1).expect("usage: check_alerts_config <config.toml>");
    let cfg = rustock_pegout_alerts::Config::load(&path)?.pegout_alerts;

    println!("configuration {path} is valid\n");
    println!("  enabled              {}", cfg.enabled);
    println!("  poll interval        {}s", cfg.poll_interval_secs);
    println!("  peg-out threshold    {} BTC", cfg.pegout_alert_btc);
    println!("  output threshold     {} BTC", cfg.output_alert_btc);
    println!("  in-transit threshold {} BTC", cfg.in_transit_alert_btc);
    println!("  confirmations        {}", cfg.confirmations);
    println!("  change scripts       {}", cfg.federation_change_scripts.len());

    println!("\n  email                {}", if cfg.email.enabled { "enabled" } else { "disabled (log only)" });
    if cfg.email.enabled {
        println!("  smtp                 {}:{} ({:?})", cfg.email.smtp_host, cfg.email.smtp_port, cfg.email.tls);
        println!("  from                 {}", cfg.email.from);
        println!("  to                   {}", cfg.email.to.join(", "));
        // Report resolution without disclosing the value.
        let shown = |o: &Option<String>| match o {
            Some(v) if v.is_empty() => "resolved but EMPTY".to_string(),
            Some(v) if v.contains("${") => "UNRESOLVED".to_string(),
            Some(v) => format!("resolved, {} chars", v.len()),
            None => "not set".to_string(),
        };
        println!("  username             {}", shown(&cfg.email.username));
        println!("  password             {}", shown(&cfg.email.password));
        #[cfg(feature = "smtp")]
        {
            rustock_pegout_alerts::SmtpSink::new(&cfg.email)?;
            println!("\n  SMTP transport builds. No mail was sent.");
        }
        #[cfg(not(feature = "smtp"))]
        println!(
            "\n  NOTE: this binary was built without the `smtp` feature, so the\n  \
             transport was not checked and the node would refuse to start the\n  \
             watcher with email enabled. Rebuild with --features smtp."
        );
    }
    Ok(())
}
