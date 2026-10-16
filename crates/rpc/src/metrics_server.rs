//! The `/metrics` endpoint.
//!
//! Served on its own listener rather than as a route on the JSON-RPC server,
//! for two reasons. A scrape is not an RPC call and should not be reachable
//! wherever RPC is: the RPC port is commonly bound to localhost or put behind
//! auth, and metrics are commonly scraped from elsewhere on a private
//! network. And keeping them apart means a metrics scrape cannot contend
//! with, or be rate-limited alongside, real RPC traffic.
//!
//! Off unless a port is configured. A node that was not asked to expose
//! metrics opens no socket.

use axum::{routing::get, Router};
use std::net::SocketAddr;
use tracing::info;

/// Starts the metrics server on the given host and port.
pub async fn start_metrics_server(host: &str, port: u16) -> anyhow::Result<()> {
    let app = Router::new().route("/metrics", get(serve)).route("/", get(index));

    let addr: SocketAddr = format!("{host}:{port}").parse()?;
    info!("Metrics server listening on {addr}/metrics");

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

async fn serve() -> ([(axum::http::HeaderName, &'static str); 1], String) {
    // The content type Prometheus expects for the text exposition format.
    // Without it some scrapers fall back to guessing, and a guess that lands
    // on OpenMetrics rejects the body for having no `# EOF`.
    (
        [(axum::http::header::CONTENT_TYPE, "text/plain; version=0.0.4; charset=utf-8")],
        rustock_core::metrics::gather(),
    )
}

/// Something to find when a person opens the port in a browser, which is how
/// a misconfigured scrape is usually diagnosed.
async fn index() -> &'static str {
    "rustock metrics: /metrics\n"
}

#[cfg(test)]
mod tests {
    /// The body is what a scraper parses, so its shape is checked here as
    /// well as in the core: this is the one that would break if the handler
    /// ever wrapped or trimmed it.
    #[tokio::test]
    async fn the_endpoint_serves_the_exposition() {
        let (headers, body) = super::serve().await;
        assert_eq!(headers[0].0, axum::http::header::CONTENT_TYPE);
        assert!(headers[0].1.starts_with("text/plain"));
        assert!(body.contains("# TYPE rustock_blocks_executed_total counter"));
        assert!(body.ends_with('\n'), "exposition must end with a newline");
    }
}
