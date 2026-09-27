//! Node health alerting: stalled, behind the network, or ahead of it.
//!
//! Three conditions, each of which means the node is not doing its job and
//! each of which an operator would otherwise learn about from a log they were
//! not reading:
//!
//! 1. **Stalled** — the executed head has not moved.
//! 2. **Behind** — the best peer is more than `block_gap` blocks ahead.
//! 3. **Ahead** — this node is more than `block_gap` blocks ahead of *every*
//!    peer.
//!
//! # Why the third one is not redundant
//!
//! Being ahead of the whole network sounds harmless and is not. It means
//! either that this node is following a chain nobody else is — a fork it
//! accepted and they rejected — or that every peer it has is stale, which
//! makes it blind whatever its own height says. Both look like health from
//! inside: blocks are arriving and executing.
//!
//! # Why a condition must persist before it alerts
//!
//! Every one of these is momentarily true in normal operation. A reorg stalls
//! the executed head for seconds; a peer announces a block before this node
//! has it, so it is briefly behind; this node mines or receives a block first,
//! so it is briefly ahead. Alerting on the instantaneous reading would mean
//! mailing an operator several times an hour about a node that is fine.
//!
//! So each condition carries a timer, and it must hold continuously for
//! `for_secs` before anything is sent. Any reading that clears the condition
//! resets the timer — the requirement is *sustained*, not *cumulative*.
//!
//! # The cooldown deliberately survives recovery
//!
//! After an alert is sent that alarm goes quiet for `cooldown_secs`, and the
//! cooldown is **not** cleared when the condition resolves. A node that
//! flaps — stalling, recovering, stalling again — is exactly the case where
//! per-occurrence mail is useless, because the first message already said
//! what is wrong and the next fifty add nothing. One a day per alarm is a
//! report; one per occurrence is a filter rule waiting to happen.
//!
//! The three alarms are independent: a cooldown on one says nothing about the
//! others.

use crate::alert::Alert;
use std::time::{Duration, Instant};

/// How far apart this node and the network must be before it counts, and how
/// long each condition must hold.
#[derive(Debug, Clone)]
pub struct HealthThresholds {
    /// Blocks of difference before "behind" or "ahead" is true at all.
    pub block_gap: u64,
    /// How long a condition must hold continuously before it is reported.
    pub for_secs: u64,
    /// How long that alarm stays quiet afterwards.
    pub cooldown_secs: u64,
}

impl Default for HealthThresholds {
    fn default() -> Self {
        Self { block_gap: 10, for_secs: 600, cooldown_secs: 86_400 }
    }
}

/// Which alarm, for keeping three independent timers and cooldowns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Alarm {
    Stalled,
    BehindPeers,
    AheadOfPeers,
}

/// One alarm's timer and cooldown.
#[derive(Debug, Default)]
struct AlarmState {
    /// When the condition most recently became true, or `None` while it is
    /// false.
    since: Option<Instant>,
    /// When this alarm last sent, whether or not the condition has since
    /// cleared.
    last_sent: Option<Instant>,
}

impl AlarmState {
    /// Fire now unless this alarm is still in its cooldown.
    ///
    /// For conditions that time themselves, such as the stall, whose duration
    /// is measured from when the height last changed rather than from when the
    /// monitor noticed. Passing such a condition through [`Self::step`] would
    /// make it wait `for_secs` twice.
    fn fire_if_cool(&mut self, now: Instant, t: &HealthThresholds) -> bool {
        if let Some(last) = self.last_sent {
            if now.duration_since(last) < Duration::from_secs(t.cooldown_secs) {
                return false;
            }
        }
        self.last_sent = Some(now);
        true
    }

    /// Advance this alarm and say whether it should fire now.
    ///
    /// For conditions read fresh at each poll, where the timer has to be this
    /// alarm's own.
    fn step(&mut self, holding: bool, now: Instant, t: &HealthThresholds) -> bool {
        if !holding {
            // Cleared: the timer restarts from scratch next time. The cooldown
            // is left alone on purpose -- see the module docs.
            self.since = None;
            return false;
        }

        let since = *self.since.get_or_insert(now);
        if now.duration_since(since) < Duration::from_secs(t.for_secs) {
            return false;
        }
        if let Some(last) = self.last_sent {
            if now.duration_since(last) < Duration::from_secs(t.cooldown_secs) {
                return false;
            }
        }
        self.last_sent = Some(now);
        true
    }
}

/// What the node looked like at one poll.
#[derive(Debug, Clone, Copy)]
pub struct Sample {
    /// The block this node has executed to. Stall detection watches this one
    /// rather than the download head: a node fetching blocks it never
    /// executes is stalled in every way that matters.
    pub executed: u64,
    /// The best block this node holds, which is what a peer's best compares
    /// against.
    pub best: u64,
    /// The highest block any connected peer claims, and how many peers there
    /// were. `None` means no peer had told us anything yet.
    pub best_peer: Option<u64>,
    pub peer_count: usize,
}

/// Tracks the three conditions across polls.
pub struct HealthMonitor {
    thresholds: HealthThresholds,
    stalled: AlarmState,
    behind: AlarmState,
    ahead: AlarmState,
    /// The executed height last seen, and when it last changed.
    last_executed: Option<u64>,
    executed_since: Option<Instant>,
}

impl HealthMonitor {
    pub fn new(thresholds: HealthThresholds) -> Self {
        Self {
            thresholds,
            stalled: AlarmState::default(),
            behind: AlarmState::default(),
            ahead: AlarmState::default(),
            last_executed: None,
            executed_since: None,
        }
    }

    /// Feed one reading and collect whatever should be mailed.
    pub fn observe(&mut self, s: Sample, now: Instant) -> Vec<Alert> {
        let t = self.thresholds.clone();
        let mut out = Vec::new();

        // -- 1. stalled ---------------------------------------------------
        //
        // Timed from when the height last *changed*, not from when the
        // condition was noticed, so a node already stuck when the monitor
        // starts is still found. The first reading establishes the baseline
        // and cannot itself be a stall.
        let moved = self.last_executed != Some(s.executed);
        if moved {
            self.last_executed = Some(s.executed);
            self.executed_since = Some(now);
        }
        let stalled_for = self
            .executed_since
            .map(|at| now.duration_since(at))
            .unwrap_or_default();
        let is_stalled = !moved && stalled_for >= Duration::from_secs(t.for_secs);
        // `stalled_for` is already the duration the condition has held, so this
        // alarm needs only the cooldown. Running it through `step` would start
        // a second timer and make the first alert take twice as long.
        if is_stalled && self.stalled.fire_if_cool(now, &t) {
            out.push(Alert::NodeStalled {
                executed: s.executed,
                best: s.best,
                stalled_secs: stalled_for.as_secs(),
                peer_count: s.peer_count,
                best_peer: s.best_peer,
            });
        }

        // -- 2 and 3. behind and ahead ------------------------------------
        //
        // Both need a peer to compare against. With none, neither condition is
        // decidable and both timers stand still rather than reading as
        // satisfied -- a node with no peers is a different problem, and
        // reporting it as "ahead of every peer" would be true and useless.
        let (behind_now, ahead_now) = match s.best_peer {
            Some(peer_best) => (
                peer_best.saturating_sub(s.best) > t.block_gap,
                s.best.saturating_sub(peer_best) > t.block_gap,
            ),
            None => (false, false),
        };

        if self.behind.step(behind_now, now, &t) {
            out.push(Alert::NodeBehindPeers {
                best: s.best,
                executed: s.executed,
                best_peer: s.best_peer.unwrap_or(0),
                behind_by: s.best_peer.unwrap_or(0).saturating_sub(s.best),
                peer_count: s.peer_count,
                for_secs: t.for_secs,
            });
        }
        if self.ahead.step(ahead_now, now, &t) {
            out.push(Alert::NodeAheadOfPeers {
                best: s.best,
                executed: s.executed,
                best_peer: s.best_peer.unwrap_or(0),
                ahead_by: s.best.saturating_sub(s.best_peer.unwrap_or(0)),
                peer_count: s.peer_count,
                for_secs: t.for_secs,
            });
        }

        out
    }

    /// Whether an alarm is currently within its cooldown, for tests and for
    /// reporting state.
    #[cfg(test)]
    pub(crate) fn in_cooldown(&self, alarm: Alarm, now: Instant) -> bool {
        let state = match alarm {
            Alarm::Stalled => &self.stalled,
            Alarm::BehindPeers => &self.behind,
            Alarm::AheadOfPeers => &self.ahead,
        };
        state.last_sent.is_some_and(|last| {
            now.duration_since(last) < Duration::from_secs(self.thresholds.cooldown_secs)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn thresholds() -> HealthThresholds {
        HealthThresholds { block_gap: 10, for_secs: 600, cooldown_secs: 86_400 }
    }

    fn sample(executed: u64, best: u64, best_peer: Option<u64>) -> Sample {
        Sample { executed, best, best_peer, peer_count: best_peer.map_or(0, |_| 5) }
    }

    fn at(base: Instant, secs: u64) -> Instant {
        base + Duration::from_secs(secs)
    }

    /// **The stall must fire at ten minutes, not twenty.** The duration is
    /// measured from when the height last changed, so running it through the
    /// alarm's own timer as well would silently double the delay -- which is
    /// exactly the bug this test was written for.
    #[test]
    fn a_stall_alerts_after_exactly_the_configured_wait() {
        let mut m = HealthMonitor::new(thresholds());
        let t0 = Instant::now();

        // Baseline. The first reading cannot itself be a stall.
        assert!(m.observe(sample(100, 100, Some(100)), t0).is_empty());

        // Same height, but not for long enough yet.
        assert!(m.observe(sample(100, 100, Some(100)), at(t0, 599)).is_empty());

        let fired = m.observe(sample(100, 100, Some(100)), at(t0, 600));
        assert_eq!(fired.len(), 1, "should alert at 600s, not 1200s");
        assert!(matches!(fired[0], Alert::NodeStalled { executed: 100, .. }));
    }

    /// A height that moves resets the clock, so a slow but living node is
    /// never reported.
    #[test]
    fn a_moving_height_never_stalls() {
        let mut m = HealthMonitor::new(thresholds());
        let t0 = Instant::now();
        for step in 0..20u64 {
            // One block every 300s: slow, but moving.
            let alerts = m.observe(sample(100 + step, 100 + step, Some(100 + step)), at(t0, step * 300));
            assert!(alerts.is_empty(), "a moving node alerted at step {step}");
        }
    }

    /// **The cooldown survives recovery.** A node that flaps must not mail on
    /// every occurrence -- the first message already said what is wrong.
    #[test]
    fn an_alarm_stays_quiet_for_the_cooldown_even_after_recovering() {
        let mut m = HealthMonitor::new(thresholds());
        let t0 = Instant::now();

        m.observe(sample(100, 100, Some(100)), t0);
        assert_eq!(m.observe(sample(100, 100, Some(100)), at(t0, 600)).len(), 1);
        assert!(m.in_cooldown(Alarm::Stalled, at(t0, 601)));

        // Recovers, then stalls again well inside the cooldown.
        m.observe(sample(101, 101, Some(101)), at(t0, 700));
        for secs in [1_400u64, 2_000, 40_000, 86_000] {
            assert!(
                m.observe(sample(101, 101, Some(101)), at(t0, secs)).is_empty(),
                "alerted again at {secs}s, inside the 24h cooldown"
            );
        }

        // Past the cooldown it may speak again.
        assert!(!m.in_cooldown(Alarm::Stalled, at(t0, 700 + 86_401)));
        assert_eq!(
            m.observe(sample(101, 101, Some(101)), at(t0, 700 + 86_401)).len(),
            1,
            "must alert again once the cooldown has passed"
        );
    }

    /// Did any alert of this shape fire?
    fn has_behind(alerts: &[Alert]) -> bool {
        alerts.iter().any(|a| matches!(a, Alert::NodeBehindPeers { .. }))
    }

    /// Behind the network by more than the gap, sustained.
    ///
    /// The height advances throughout: a node that is merely behind is still
    /// making progress, and holding it still would make this a test of the
    /// stall alarm wearing the wrong name.
    #[test]
    fn falling_behind_alerts_only_once_sustained() {
        let mut m = HealthMonitor::new(thresholds());
        let t0 = Instant::now();
        // `tick` advances both us and the network, holding the gap at `gap`.
        let mut tick = |m: &mut HealthMonitor, secs: u64, gap: u64| {
            let ours = 100 + secs / 10;
            m.observe(sample(ours, ours, Some(ours + gap)), at(t0, secs))
        };

        assert!(!has_behind(&tick(&mut m, 0, 11)));
        assert!(!has_behind(&tick(&mut m, 300, 11)));

        // Caught up briefly -- the timer restarts, because the requirement is
        // sustained rather than cumulative.
        assert!(!has_behind(&tick(&mut m, 400, 0)));
        assert!(!has_behind(&tick(&mut m, 500, 11)));
        assert!(
            !has_behind(&tick(&mut m, 1_050, 11)),
            "550s into the second spell is not yet 600s"
        );

        let fired = tick(&mut m, 1_101, 11);
        assert!(has_behind(&fired), "601s into the second spell must alert");
        assert!(fired.iter().any(|a| matches!(a, Alert::NodeBehindPeers { behind_by: 11, .. })));
    }

    /// Exactly at the gap is not over it.
    #[test]
    fn a_gap_of_exactly_ten_is_not_an_alert() {
        let mut m = HealthMonitor::new(thresholds());
        let t0 = Instant::now();
        for secs in [0u64, 600, 1_200, 2_000] {
            let ours = 100 + secs / 10;
            let alerts = m.observe(sample(ours, ours, Some(ours + 10)), at(t0, secs));
            assert!(
                !has_behind(&alerts),
                "10 behind should not alert; the threshold is 'more than'"
            );
        }
    }

    /// Ahead of every peer is its own alarm, and fires independently of the
    /// others' cooldowns.
    #[test]
    fn being_ahead_of_every_peer_alerts_independently() {
        let mut m = HealthMonitor::new(thresholds());
        let t0 = Instant::now();

        // Stalled *and* ahead: two different alarms, two different messages.
        m.observe(sample(200, 200, Some(150)), t0);
        let fired = m.observe(sample(200, 200, Some(150)), at(t0, 601));
        assert_eq!(fired.len(), 2, "a stall and an ahead-of-peers, not one");
        assert!(fired.iter().any(|a| matches!(a, Alert::NodeStalled { .. })));
        assert!(fired.iter().any(|a| matches!(a, Alert::NodeAheadOfPeers { ahead_by: 50, .. })));
    }

    /// **With no peers, neither comparison is decidable.** Reporting a
    /// peerless node as "ahead of every peer" would be true and useless.
    #[test]
    fn no_peers_means_no_comparison_alarm() {
        let mut m = HealthMonitor::new(thresholds());
        let t0 = Instant::now();
        for secs in [0u64, 600, 1_200, 90_000] {
            let alerts = m.observe(
                Sample { executed: 500, best: 500, best_peer: None, peer_count: 0 },
                at(t0, secs),
            );
            assert!(
                !alerts.iter().any(|a| matches!(
                    a,
                    Alert::NodeBehindPeers { .. } | Alert::NodeAheadOfPeers { .. }
                )),
                "a peerless node must not be reported as ahead or behind"
            );
        }
    }

    /// The three alarms hold separate cooldowns.
    #[test]
    fn the_alarms_do_not_share_a_cooldown() {
        let mut m = HealthMonitor::new(thresholds());
        let t0 = Instant::now();

        // Behind while still advancing, so only the behind alarm is in play.
        m.observe(sample(80, 80, Some(100)), t0);
        let fired = m.observe(sample(90, 90, Some(110)), at(t0, 601));
        assert!(has_behind(&fired), "sustained 20 behind must alert");
        assert!(
            !fired.iter().any(|a| matches!(a, Alert::NodeStalled { .. })),
            "the height moved, so nothing is stalled"
        );
        assert!(m.in_cooldown(Alarm::BehindPeers, at(t0, 602)));
        assert!(!m.in_cooldown(Alarm::Stalled, at(t0, 602)), "an unrelated alarm is unaffected");

        // Now the height stops moving. The stall fires on its own schedule
        // despite the other alarm being silenced.
        let fired = m.observe(sample(90, 90, Some(110)), at(t0, 601 + 601));
        assert!(
            fired.iter().any(|a| matches!(a, Alert::NodeStalled { .. })),
            "a silenced behind-alarm must not silence the stall"
        );
        assert!(
            !has_behind(&fired),
            "the behind alarm is still inside its 24h cooldown"
        );
    }
}

/// Where a reading comes from. An interface rather than the concrete stores,
/// so the polling loop can be tested without a chain or a network.
#[async_trait::async_trait]
pub trait HealthSource: Send + Sync {
    async fn sample(&self) -> Option<Sample>;
}

/// Poll `source`, feed the monitor, and deliver what it raises.
///
/// Deliberately owns nothing the node needs. It reads two numbers and a peer
/// table; if it stalls on an unreachable mail server the node does not notice,
/// which is the same property the peg-out watcher has and for the same reason.
pub async fn run(
    source: std::sync::Arc<dyn HealthSource>,
    thresholds: HealthThresholds,
    poll_interval: Duration,
    sinks: std::sync::Arc<Vec<Box<dyn crate::sink::AlertSink>>>,
) {
    let mut monitor = HealthMonitor::new(thresholds);
    let mut ticker = tokio::time::interval(poll_interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        ticker.tick().await;
        let Some(sample) = source.sample().await else { continue };

        for alert in monitor.observe(sample, Instant::now()) {
            tracing::warn!(
                target: "rustock::health",
                "node health alert: {}", alert.subject()
            );
            for sink in sinks.iter() {
                if let Err(e) = sink.deliver(&alert) {
                    tracing::error!(
                        target: "rustock::health",
                        "sink {} could not deliver {:?}: {e:#}", sink.name(), alert.subject()
                    );
                }
            }
        }
    }
}
