//! The progress watchdog: a generic liveness net under the whole sync
//! subsystem.
//!
//! Stage 1 of `docs/sync-redesign.md`. It knows nothing about *why* the node
//! might be stuck. It knows only that a full cycle produced no progress, which
//! is the one thing every stall to date has had in common.
//!
//! # The measure
//!
//! ```text
//!   Φ = ( peer_best − executed.number ,   // how far behind we are
//!         outstanding_requests ,           // work in flight
//!         −age_of_oldest_request )         // and how stale it is
//! ```
//!
//! ordered lexicographically, smaller being better.
//!
//! **Φ is anchored to the *executed* head, not the downloaded head.** Stall 3
//! -- twenty hours, zero errors -- was exactly the failure of measuring the
//! wrong frontier: the downloaded head sat at the tip, so the node looked
//! caught up while executing nothing.
//!
//! # Failed to decrease, not unchanged
//!
//! The design document stated the progress obligation as *"every transition
//! must strictly **decrease** Φ, or be a no-op, or be an explicit retreat"*,
//! and then stated the watchdog as *"fire if k consecutive rounds leave Φ
//! **unchanged**"*. Those are not the same test, and stall 6 fell through the
//! gap between them: the executed head was frozen while `peer_best` kept
//! rising, so Φ's first component **grew** -- 354, 355, … 364 -- and a test for
//! "unchanged" would have stayed quiet through all 106 rounds of it.
//!
//! So the test here is the stronger one: fire when Φ has not reached a
//! strictly better value within the window. Growing is not progress.
//!
//! # Not firing spuriously
//!
//! Two cases need care, and both are handled by the measure rather than by
//! special-casing:
//!
//! * **Idle at the tip.** Φ's first component is zero; there is nothing to make
//!   progress towards, so the watchdog stands down entirely.
//! * **A long phase where the executed head cannot move** -- a header round, or
//!   a long body download. Anchoring to execution means Φ's first component is
//!   flat there, which is why Φ is lexicographic: outstanding requests falling
//!   as chunks complete is a strictly better Φ, so a round that is working
//!   banks a new best and resets the window.
//!
//! The doc flagged this as *"the sort of detail that produces stall number
//! six"*, and it did.

use std::time::{Duration, Instant};

/// How long Φ may fail to improve before the first escalation.
///
/// Comfortably longer than a header round so a single slow round cannot
/// trip it, and far shorter than the hours every stall so far has cost.
pub const STALL_WINDOW: Duration = Duration::from_secs(180);

/// Retreat depth for the first escalation, doubling thereafter.
pub const RETREAT_BASE: u64 = 1;

/// Cap on a single retreat. A node that retreats without bound walks back to
/// genesis; a retreat that has not restored progress by this depth is a
/// different failure and deserves its own alarm, not more retreating.
pub const RETREAT_CAP: u64 = 512;

/// The progress measure. Lexicographic, smaller is better.
///
/// `neg_age_secs` is negated so that a request getting older is a *smaller*
/// Φ: within a phase that cannot move the first two components, a response
/// still being waited for is not evidence of a stall.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Phi {
    pub behind: u64,
    pub outstanding: u64,
    pub neg_age_secs: i64,
}

impl Phi {
    pub fn new(peer_best: u64, executed: u64, outstanding: usize, oldest_request: Option<Duration>) -> Self {
        Self {
            behind: peer_best.saturating_sub(executed),
            outstanding: outstanding as u64,
            neg_age_secs: -(oldest_request.map_or(0, |d| d.as_secs() as i64)),
        }
    }

    /// Nothing to make progress towards.
    pub fn at_tip(&self) -> bool {
        self.behind == 0
    }
}

/// What the watchdog wants done, in increasing order of disruption.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Escalation {
    /// Drop whatever round is in flight and re-verify lineage from the
    /// executed head.
    RetreatAndReverify { blocks: u64 },
    /// Re-run the connection-point search from scratch: the assumption about
    /// where we join the peer's chain is itself suspect.
    ResearchConnectionPoint,
    /// Widen the retreat. Exponential, capped at [`RETREAT_CAP`].
    WidenRetreat { blocks: u64 },
    /// Past the cap. Nothing further is worth trying automatically; this is a
    /// different failure and says so.
    Exhausted { blocks: u64 },
}

/// Tracks Φ and decides when the node is not syncing, whatever it believes.
#[derive(Debug)]
pub struct ProgressWatchdog {
    /// Best (smallest) Φ seen since the window opened.
    best: Option<Phi>,
    /// When the current window opened -- the last time Φ strictly improved.
    window_opened: Instant,
    /// How many times we have escalated without Φ improving since.
    level: u32,
    window: Duration,
}

impl Default for ProgressWatchdog {
    fn default() -> Self {
        Self::new(STALL_WINDOW)
    }
}

impl ProgressWatchdog {
    pub fn new(window: Duration) -> Self {
        Self { best: None, window_opened: Instant::now(), level: 0, window }
    }

    /// How long Φ has failed to improve.
    pub fn stalled_for(&self, now: Instant) -> Duration {
        now.saturating_duration_since(self.window_opened)
    }

    /// The best Φ seen in this window, for logging.
    pub fn best(&self) -> Option<Phi> {
        self.best
    }

    /// How many escalations have fired without Φ improving since.
    pub fn level(&self) -> u32 {
        self.level
    }

    /// Record an observation. Returns an escalation when Φ has failed to
    /// improve for longer than the window.
    ///
    /// Call this on every tick; it is a comparison and a clock read.
    pub fn observe(&mut self, phi: Phi, now: Instant) -> Option<Escalation> {
        // Idle at the tip is not a stall. Reset rather than merely skip, so a
        // node that catches up and later falls behind starts a fresh window
        // instead of inheriting a stale one.
        if phi.at_tip() {
            self.reset(now);
            return None;
        }

        match self.best {
            // Strictly better: real progress. New window, and escalation
            // starts from the bottom again.
            Some(best) if phi < best => {
                self.best = Some(phi);
                self.window_opened = now;
                self.level = 0;
                return None;
            }
            None => {
                self.best = Some(phi);
                self.window_opened = now;
                return None;
            }
            _ => {}
        }

        if now.saturating_duration_since(self.window_opened) < self.window {
            return None;
        }

        // Escalating counts as an event: the next window starts now, so the
        // ladder climbs one rung per window rather than every tick.
        self.window_opened = now;
        self.level = self.level.saturating_add(1);
        Some(self.escalation_for(self.level))
    }

    fn escalation_for(&self, level: u32) -> Escalation {
        match level {
            1 => Escalation::RetreatAndReverify { blocks: RETREAT_BASE },
            2 => Escalation::ResearchConnectionPoint,
            _ => {
                // Doubling from the base, with level 3 the first widening.
                let steps = level.saturating_sub(2);
                let blocks = RETREAT_BASE.saturating_mul(1u64 << steps.min(20));
                if blocks > RETREAT_CAP {
                    Escalation::Exhausted { blocks: RETREAT_CAP }
                } else {
                    Escalation::WidenRetreat { blocks }
                }
            }
        }
    }

    /// Forget the window. Called when the node reaches the tip, and by the
    /// service after a change big enough that the old measure is meaningless.
    pub fn reset(&mut self, now: Instant) {
        self.best = None;
        self.window_opened = now;
        self.level = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn phi(behind: u64, outstanding: u64, age: i64) -> Phi {
        Phi { behind, outstanding, neg_age_secs: -age }
    }

    #[test]
    fn a_shrinking_gap_never_escalates() {
        let mut w = ProgressWatchdog::new(Duration::from_secs(60));
        let t0 = Instant::now();
        for (i, behind) in (0..20).zip((100..120).rev()) {
            let t = t0 + Duration::from_secs(i * 30);
            assert_eq!(w.observe(phi(behind, 4, 1), t), None, "escalated while catching up");
        }
    }

    /// Stall 6. The executed head was frozen while `peer_best` kept rising, so
    /// Φ's first component GREW: 354, 355, … 364. The design document's
    /// watchdog fired on "Φ unchanged", which this is not -- and it would have
    /// stayed quiet through all 106 rounds of the real stall.
    #[test]
    fn a_growing_gap_escalates_even_though_phi_is_never_unchanged() {
        let mut w = ProgressWatchdog::new(Duration::from_secs(180));
        let t0 = Instant::now();

        let mut fired = None;
        for i in 0..20u64 {
            let t = t0 + Duration::from_secs(i * 30);
            // behind grows by one each observation: never equal, never better.
            if let Some(e) = w.observe(phi(354 + i, 1, 1), t) {
                fired = Some((i, e));
                break;
            }
        }

        let (i, escalation) = fired.expect("the watchdog stayed quiet while falling further behind");
        assert!(i >= 6, "fired before the window elapsed (observation {i})");
        assert_eq!(escalation, Escalation::RetreatAndReverify { blocks: RETREAT_BASE });
    }

    /// The precise case the old wording missed, stated as a unit test: Φ that
    /// only ever gets worse must be treated as no progress, not as change.
    #[test]
    fn phi_that_only_worsens_is_not_progress() {
        let mut w = ProgressWatchdog::new(Duration::from_secs(10));
        let t0 = Instant::now();
        assert_eq!(w.observe(phi(100, 0, 0), t0), None);
        assert_eq!(w.observe(phi(101, 0, 0), t0 + Duration::from_secs(5)), None);
        assert!(w.observe(phi(102, 0, 0), t0 + Duration::from_secs(11)).is_some());
    }

    /// Idle at the tip is not a stall: there is nothing to make progress
    /// towards, and a watchdog that fires there would fire on every healthy
    /// node every night.
    #[test]
    fn sitting_at_the_tip_never_escalates() {
        let mut w = ProgressWatchdog::new(Duration::from_secs(1));
        let t0 = Instant::now();
        for i in 0..100u64 {
            let t = t0 + Duration::from_secs(i * 10);
            assert_eq!(w.observe(phi(0, 0, 0), t), None);
        }
    }

    /// A header round that is working holds the executed head still -- Φ's
    /// first component cannot move -- so the progress signal has to come from
    /// the second component. This is the case the design document warned was
    /// "the sort of detail that produces stall number six".
    #[test]
    fn a_working_header_round_banks_progress_through_outstanding_requests() {
        let mut w = ProgressWatchdog::new(Duration::from_secs(60));
        let t0 = Instant::now();
        // Same gap throughout; chunks completing one by one.
        for (i, outstanding) in (0..4u64).zip([4, 3, 2, 1]) {
            let t = t0 + Duration::from_secs(i * 25);
            assert_eq!(w.observe(phi(500, outstanding, 2), t), None);
        }
    }

    /// … but a round that merely churns -- requests going out and timing out
    /// without the gap ever closing -- must not be mistaken for that.
    #[test]
    fn a_churning_round_does_not_bank_progress_forever() {
        let mut w = ProgressWatchdog::new(Duration::from_secs(60));
        let t0 = Instant::now();
        let mut fired = false;
        for i in 0..40u64 {
            let t = t0 + Duration::from_secs(i * 10);
            // Oscillates between 2 and 1 outstanding, gap unchanged: after the
            // first dip to 1 there is no new best to be had.
            let outstanding = if i % 2 == 0 { 2 } else { 1 };
            if w.observe(phi(500, outstanding, 3), t).is_some() {
                fired = true;
                break;
            }
        }
        assert!(fired, "a churning round never escalated");
    }

    #[test]
    fn escalation_climbs_one_rung_per_window_and_is_capped() {
        let mut w = ProgressWatchdog::new(Duration::from_secs(10));
        let t0 = Instant::now();
        w.observe(phi(100, 0, 0), t0);

        let mut seen = Vec::new();
        for i in 1..16u64 {
            let t = t0 + Duration::from_secs(i * 11);
            if let Some(e) = w.observe(phi(100, 0, 0), t) {
                seen.push(e);
            }
        }

        assert_eq!(seen[0], Escalation::RetreatAndReverify { blocks: 1 });
        assert_eq!(seen[1], Escalation::ResearchConnectionPoint);
        assert_eq!(seen[2], Escalation::WidenRetreat { blocks: 2 });
        assert_eq!(seen[3], Escalation::WidenRetreat { blocks: 4 });
        // Retreat must be bounded: a node that retreats without limit walks
        // back to genesis.
        let last = seen.last().unwrap();
        assert_eq!(*last, Escalation::Exhausted { blocks: RETREAT_CAP });
        for e in &seen {
            if let Escalation::WidenRetreat { blocks } = e {
                assert!(*blocks <= RETREAT_CAP);
            }
        }
    }

    /// Real progress resets the ladder: a node that recovers and stalls again
    /// starts from the gentlest escalation, not from wherever it left off.
    #[test]
    fn progress_resets_the_escalation_ladder() {
        let mut w = ProgressWatchdog::new(Duration::from_secs(10));
        let t0 = Instant::now();
        w.observe(phi(100, 0, 0), t0);
        assert!(w.observe(phi(100, 0, 0), t0 + Duration::from_secs(11)).is_some());
        assert_eq!(w.level(), 1);

        // The gap closes: progress.
        assert_eq!(w.observe(phi(50, 0, 0), t0 + Duration::from_secs(12)), None);
        assert_eq!(w.level(), 0);

        // Stalling again starts at the bottom of the ladder.
        let e = w.observe(phi(50, 0, 0), t0 + Duration::from_secs(30)).unwrap();
        assert_eq!(e, Escalation::RetreatAndReverify { blocks: 1 });
    }

    #[test]
    fn phi_orders_lexicographically_with_the_gap_dominating() {
        assert!(phi(10, 0, 0) < phi(11, 0, 0));
        assert!(phi(10, 5, 0) < phi(11, 0, 0), "a smaller gap wins over fewer requests");
        assert!(phi(10, 1, 0) < phi(10, 2, 0));
        // A request that has been waiting longer is a smaller Φ: still waiting
        // is not evidence of a stall.
        assert!(phi(10, 1, 9) < phi(10, 1, 3));
    }

    #[test]
    fn phi_is_built_from_the_executed_head_not_the_downloaded_one() {
        // Stall 3: downloaded at the tip, executed 2,000 behind.
        let p = Phi::new(9_000_000, 8_998_000, 0, None);
        assert_eq!(p.behind, 2_000);
        assert!(!p.at_tip());
    }
}
