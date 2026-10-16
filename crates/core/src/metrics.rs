//! Counters and gauges, and the Prometheus text form of them.
//!
//! The node had no observability surface at all, which cost more than it
//! looks: two sync improvements were merged in October 2026 and neither
//! could be given a figure in blocks per second, because nothing recorded
//! where a sync spent its time or how many peers it held while spending it.
//! Both ended up measured by hand -- a sampled count and a microbenchmark --
//! which are answers about the parts rather than about the node.
//!
//! # Why this is hand-written
//!
//! A metrics facade and a Prometheus exporter would pull a dependency tree
//! into a workspace that checks its dependencies, to produce a text format
//! of four line shapes. What is needed here is counters, gauges and
//! cumulative durations; histograms and label cardinality are not, and they
//! are most of what those crates carry.
//!
//! # Why the metrics are statics rather than a registry
//!
//! These are updated from the sync loop and the block executor, which are
//! the hottest paths in the node. A name-keyed registry would put a lock or
//! a hash lookup on every update. A `static` is an atomic add, and the set
//! of metrics is known at compile time anyway -- the cost of that choice is
//! that adding one means editing this file, which is also what keeps the
//! exposition honest: everything declared is exported, and nothing is
//! exported that is not declared.

use std::sync::atomic::{AtomicU64, Ordering};

/// A value that only goes up: totals, never rates. Prometheus derives the
/// rate; a counter that is reset loses history it cannot recover.
pub struct Counter {
    value: AtomicU64,
    name: &'static str,
    help: &'static str,
}

impl Counter {
    pub const fn new(name: &'static str, help: &'static str) -> Self {
        Self { value: AtomicU64::new(0), name, help }
    }

    #[inline]
    pub fn inc(&self) {
        self.add(1);
    }

    #[inline]
    pub fn add(&self, n: u64) {
        // Relaxed: these are counters read by a scrape, not synchronisation
        // between threads. Ordering them would cost the hot path to make a
        // sampled number marginally fresher.
        self.value.fetch_add(n, Ordering::Relaxed);
    }

    pub fn get(&self) -> u64 {
        self.value.load(Ordering::Relaxed)
    }
}

/// A value that moves both ways: how many peers, which height.
pub struct Gauge {
    value: AtomicU64,
    name: &'static str,
    help: &'static str,
}

impl Gauge {
    pub const fn new(name: &'static str, help: &'static str) -> Self {
        Self { value: AtomicU64::new(0), name, help }
    }

    #[inline]
    pub fn set(&self, v: u64) {
        self.value.store(v, Ordering::Relaxed);
    }

    pub fn get(&self) -> u64 {
        self.value.load(Ordering::Relaxed)
    }
}

/// Cumulative time spent in one phase, in nanoseconds.
///
/// Stored as a counter and exported as seconds, because that is the unit
/// Prometheus expects and dividing once per scrape is cheaper than
/// converting on every observation.
pub struct TimeCounter {
    nanos: AtomicU64,
    name: &'static str,
    help: &'static str,
}

impl TimeCounter {
    pub const fn new(name: &'static str, help: &'static str) -> Self {
        Self { nanos: AtomicU64::new(0), name, help }
    }

    #[inline]
    pub fn add(&self, d: std::time::Duration) {
        self.nanos.fetch_add(d.as_nanos() as u64, Ordering::Relaxed);
    }

    /// Times `f` and adds what it took. Returns whatever `f` returned, so a
    /// call site can be wrapped without restructuring it.
    #[inline]
    pub fn time<T>(&self, f: impl FnOnce() -> T) -> T {
        let start = std::time::Instant::now();
        let out = f();
        self.add(start.elapsed());
        out
    }

    pub fn seconds(&self) -> f64 {
        self.nanos.load(Ordering::Relaxed) as f64 / 1e9
    }
}

// ---------------------------------------------------------------------------
// Sync
// ---------------------------------------------------------------------------

pub static BLOCKS_EXECUTED: Counter = Counter::new(
    "rustock_blocks_executed_total",
    "Blocks executed. In the node this is blocks imported; the offline replay \
     tools share the executor and so share this counter, which is why it is \
     named for what it measures rather than for what it usually means",
);

pub static BODIES_REQUESTED: Counter =
    Counter::new("rustock_bodies_requested_total", "Block bodies asked of a peer");

pub static BODIES_DERIVED: Counter = Counter::new(
    "rustock_bodies_derived_total",
    "Block bodies rebuilt from their header instead of being requested",
);

pub static BODIES_RECEIVED: Counter =
    Counter::new("rustock_bodies_received_total", "Block bodies delivered by a peer");

pub static HEADERS_RECEIVED: Counter =
    Counter::new("rustock_headers_received_total", "Headers delivered by a peer");

pub static PEERS: Gauge = Gauge::new("rustock_peers", "Connected peers");

pub static SYNC_HEIGHT: Gauge =
    Gauge::new("rustock_sync_height", "Height of the chain this node has");

pub static SYNC_TARGET: Gauge =
    Gauge::new("rustock_sync_target_height", "Best height any peer has announced");

pub static BODY_REQUESTS_IN_FLIGHT: Gauge = Gauge::new(
    "rustock_body_requests_in_flight",
    "Body requests sent and not yet answered",
);

// ---------------------------------------------------------------------------
// Execution
// ---------------------------------------------------------------------------

pub static GAS_USED: Counter = Counter::new(
    "rustock_gas_used_total",
    "Gas executed. Blocks get heavier with height, so this is the work rate \
     that blocks per second understates late and overstates early",
);

pub static TRANSACTIONS_EXECUTED: Counter =
    Counter::new("rustock_transactions_executed_total", "Transactions executed");

pub static EXECUTION_SECONDS: TimeCounter = TimeCounter::new(
    "rustock_execution_seconds_total",
    "Time spent executing blocks",
);

/// Everything exported, in the order it is exported.
///
/// Kept as one list so the exposition cannot drift from the declarations:
/// a metric added above and forgotten here simply would not appear, which is
/// the failure that makes a dashboard quietly wrong.
fn encode_all(out: &mut String) {
    let counters: &[&Counter] = &[
        &BLOCKS_EXECUTED,
        &BODIES_REQUESTED,
        &BODIES_DERIVED,
        &BODIES_RECEIVED,
        &HEADERS_RECEIVED,
        &GAS_USED,
        &TRANSACTIONS_EXECUTED,
    ];
    let gauges: &[&Gauge] =
        &[&PEERS, &SYNC_HEIGHT, &SYNC_TARGET, &BODY_REQUESTS_IN_FLIGHT];
    let times: &[&TimeCounter] = &[&EXECUTION_SECONDS];

    for c in counters {
        push_metric(out, c.name, c.help, "counter", &c.get().to_string());
    }
    for g in gauges {
        push_metric(out, g.name, g.help, "gauge", &g.get().to_string());
    }
    for t in times {
        push_metric(out, t.name, t.help, "counter", &format!("{:.6}", t.seconds()));
    }
}

fn push_metric(out: &mut String, name: &str, help: &str, kind: &str, value: &str) {
    // HELP text is a single line in the exposition format, so a help string
    // wrapped across source lines must be flattened or it truncates the
    // metric and corrupts everything after it.
    let help: String = help.split_whitespace().collect::<Vec<_>>().join(" ");
    out.push_str("# HELP ");
    out.push_str(name);
    out.push(' ');
    out.push_str(&help);
    out.push('\n');
    out.push_str("# TYPE ");
    out.push_str(name);
    out.push(' ');
    out.push_str(kind);
    out.push('\n');
    out.push_str(name);
    out.push(' ');
    out.push_str(value);
    out.push('\n');
}

/// The Prometheus text exposition of everything declared above.
pub fn gather() -> String {
    let mut out = String::with_capacity(2048);
    encode_all(&mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_counter_only_goes_up() {
        let c = Counter::new("x", "y");
        assert_eq!(c.get(), 0);
        c.inc();
        c.add(4);
        assert_eq!(c.get(), 5);
    }

    #[test]
    fn a_gauge_moves_both_ways() {
        let g = Gauge::new("x", "y");
        g.set(9);
        assert_eq!(g.get(), 9);
        g.set(2);
        assert_eq!(g.get(), 2);
    }

    #[test]
    fn a_time_counter_accumulates_as_seconds() {
        let t = TimeCounter::new("x", "y");
        t.add(std::time::Duration::from_millis(1500));
        t.add(std::time::Duration::from_millis(500));
        assert!((t.seconds() - 2.0).abs() < 1e-9);
    }

    #[test]
    fn timing_a_closure_returns_what_it_returned() {
        let t = TimeCounter::new("x", "y");
        assert_eq!(t.time(|| 41 + 1), 42);
        assert!(t.seconds() >= 0.0);
    }

    /// The exposition is parsed by something that is strict about it, so the
    /// shape is pinned rather than eyeballed.
    #[test]
    fn the_exposition_has_help_type_and_value_for_every_metric() {
        let text = gather();
        let names: Vec<&str> = text
            .lines()
            .filter_map(|l| l.strip_prefix("# TYPE "))
            .map(|l| l.split(' ').next().unwrap())
            .collect();
        assert!(!names.is_empty());
        for name in &names {
            assert!(text.contains(&format!("# HELP {name} ")), "{name} has no HELP");
            let kind_line = text
                .lines()
                .find(|l| l.starts_with(&format!("# TYPE {name} ")))
                .unwrap();
            let kind = kind_line.rsplit(' ').next().unwrap();
            assert!(matches!(kind, "counter" | "gauge"), "{name} has type {kind}");
            assert!(
                text.lines().any(|l| l.starts_with(&format!("{name} "))),
                "{name} has no sample line"
            );
        }
    }

    /// A help string wrapped in the source must not become two lines, which
    /// would truncate the metric and corrupt every metric after it.
    #[test]
    fn a_wrapped_help_string_stays_on_one_line() {
        let text = gather();
        for line in text.lines() {
            assert!(!line.is_empty(), "a blank line would end the exposition early");
        }
        // GAS_USED's help is wrapped across source lines.
        let help = text
            .lines()
            .find(|l| l.starts_with("# HELP rustock_gas_used_total "))
            .expect("gas help");
        assert!(help.contains("work rate"), "the wrapped tail was lost: {help}");
    }
}
