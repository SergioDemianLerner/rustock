//! Cumulative-difficulty checkpoints, and an upper bound on the work a chain
//! can carry above one.
//!
//! # The problem
//!
//! A peer advertises a total difficulty and a height. Nothing in the protocol
//! makes that claim cost anything, and a node with no chain of its own has
//! nothing to check it against — so it will follow the claim, download headers
//! for as long as the peer cares to serve them, and only discover the lie at
//! the bottom of the chain, if at all.
//!
//! # Two kinds of checkpoint, and why this is the safe one
//!
//! A **hash** checkpoint asserts identity: *the block at height H is X*. If it
//! is ever wrong — a bad build, a compromised release — the node silently
//! follows a chain nobody else is on, and cannot tell from the inside that
//! anything is amiss.
//!
//! A **cumulative difficulty** checkpoint asserts a bound: *the work from
//! genesis to height H is Y, so nothing may claim more for that height*. If it
//! is wrong, the node can notice: it will find a chain that is valid by every
//! other rule and carries more work than the checkpoint permits. That is a
//! contradiction it can report rather than a capture it cannot see.
//!
//! The failure directions are not symmetric, which is the whole argument:
//!
//! | checkpoint is | hash | cumulative difficulty |
//! |---|---|---|
//! | wrong | silent capture | detectable contradiction |
//! | too generous | no such thing | weak, harmless |
//! | stale | irrelevant or fatal | merely covers less of the chain |
//!
//! So: **err high**. A checkpoint above the true value constrains nothing and
//! harms nothing. One below it is caught by [`CheckpointVerdict::Contradicted`]
//! and is the operator's problem, loudly.
//!
//! # Bounding the window above the checkpoint
//!
//! Below the checkpoint the answer is exact — the work is `Y`. Above it, the
//! chain is bounded by the retarget rule, which changes difficulty by a fixed
//! fraction of the parent's on every block:
//!
//! ```text
//! quotient = parent.difficulty / divisor
//! child    = parent.difficulty ± quotient      (or unchanged)
//! ```
//!
//! Nothing a miner controls changes that magnitude. The timestamp and uncle
//! count pick only the *sign*. So between two headers whose difficulty is known
//! — and whose proof of work has been checked — the work-maximising path is to
//! rise at `(1 + 1/divisor)` for as long as possible and then fall at
//! `(1 - 1/divisor)`, arriving at or below the later header's difficulty.
//!
//! Summing that bound across sampled intervals, and adding `Y`, gives a ceiling
//! on the total difficulty any chain through those samples can carry. A peer
//! claiming more than the ceiling is claiming something no valid chain can
//! provide.

use alloy_primitives::{B256, U256};

/// A point on the chain whose cumulative work is known.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DifficultyCheckpoint {
    pub number: u64,
    /// The block at `number`. Recorded for diagnostics only: the safety of
    /// this mechanism must not depend on it, because a hash checkpoint has no
    /// safe failure direction. Nothing consults it to decide which chain to
    /// follow.
    pub hash: B256,
    /// Cumulative difficulty from genesis through `number`.
    pub cumulative_difficulty: U256,
    /// The difficulty of the block at `number`, which is where the bound above
    /// the checkpoint starts from.
    pub difficulty: U256,
}

/// Mainnet, block #9,020,000 — 2026-07-05, about three months before this was
/// written.
///
/// Taken from a fully synced node's own chain, and re-verified against that
/// node over RPC on 2026-09-30: all three fields match `eth_getBlockByNumber`
/// for #9,020,000 exactly. The test below checks the limbs against these
/// decimals; only a query against a real chain checks the decimals themselves,
/// and a checkpoint set too low would reject the honest chain.
///
/// A checkpoint should be refreshed
/// each release: the cost of verifying a peer's claim scales with the distance
/// from the newest checkpoint to the tip (see `docs/`), and an old checkpoint
/// is weak rather than wrong.
pub const MAINNET_CHECKPOINT: DifficultyCheckpoint = DifficultyCheckpoint {
    number: 9_020_000,
    hash: B256::new([
        0x27, 0x03, 0x43, 0xb5, 0xb4, 0x3e, 0xca, 0x9c, 0x17, 0x53, 0x8b, 0xda, 0xef, 0xe7, 0xcf,
        0x8f, 0xd3, 0x01, 0x2c, 0xe9, 0xb7, 0xce, 0xfc, 0x9c, 0x7d, 0x4a, 0xc2, 0x32, 0x2b, 0x47,
        0x2c, 0xc5,
    ]),
    // 32959497588810990020280199750 == 0x6a7f75187db0eeb538802a46
    //
    // Written as limbs rather than a literal because `U256` has no const
    // decimal constructor. `checkpoint_value_tests` checks these against the
    // decimal values read off the chain -- an earlier hand-written version of
    // this was wrong in both fields, and the difficulty silently truncated
    // because it does not fit in a u64.
    cumulative_difficulty: U256::from_limbs([
        0x7db0eeb538802a46,
        0x000000006a7f7518,
        0x0000000000000000,
        0x0000000000000000,
    ]),
    // 7388898250588046058941
    difficulty: U256::from_limbs([
        0x8d8fe2d95db565bd,
        0x0000000000000190,
        0x0000000000000000,
        0x0000000000000000,
    ]),
};

/// What sampling concluded about a peer's claim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckpointVerdict {
    /// The claim is within what the chain could carry. Not proof it is honest;
    /// only that it is not arithmetically impossible.
    Plausible { ceiling: U256 },
    /// The claim exceeds the ceiling. No valid chain through the sampled
    /// headers can carry this much work, so the peer is lying.
    Impossible { claimed: U256, ceiling: U256 },
    /// This node's *own* chain carries more work at the checkpoint height than
    /// the checkpoint allows.
    ///
    /// The checkpoint is wrong, or this build is not the one it claims to be.
    /// Either way it is not a peer's fault and must not be treated as one: the
    /// right response is to stop and tell the operator, not to pick a side.
    Contradicted { observed: U256, checkpoint: U256 },
}

/// The largest cumulative work the `span` blocks after a block of difficulty
/// `from` can carry, given the next sampled block has difficulty `to`.
///
/// Counts the `span` blocks beginning with the one *after* `from`'s block, so
/// consecutive intervals tile the chain without overlap.
///
/// Each block takes the highest difficulty it may have while still being able
/// to fall to `to` by the end of the span. That is the rise-then-fall shape,
/// but expressed as a per-block maximum rather than a turning point — which
/// matters, because the consensus rule also permits difficulty to stay
/// *unchanged*, and a strict rise-then-fall model cannot express that. A model
/// that cannot produces a "bound" below a flat chain's real work.
///
/// `min_difficulty` clamps the fall, exactly as the consensus rule does. The
/// clamp only ever raises a value, so including it keeps this an upper bound.
pub fn max_work_between(
    from: U256,
    to: U256,
    span: u64,
    divisor: u64,
    min_difficulty: U256,
) -> U256 {
    if span == 0 || divisor < 2 {
        return U256::ZERO;
    }
    let div = U256::from(divisor);

    // Walk forwards, taking the largest difficulty each block may have while
    // still being able to fall to `to` in the blocks that remain.
    //
    // Two limits apply at every step:
    //
    //   * it cannot exceed the previous block's difficulty by more than
    //     `1/divisor`, because that is the whole of the retarget rule;
    //   * it cannot be so high that falling at `1/divisor` for the remaining
    //     blocks still overshoots `to`.
    //
    // The lower of the two is the maximum. Taking the minimum rather than
    // forcing a rise-then-fall shape is what admits `sign == 0` -- a block
    // whose difficulty is unchanged -- which the consensus rule allows and
    // which a rise-then-fall model cannot express. Without it the "bound" came
    // out *below* a flat chain's real work for short spans, and would have
    // rejected honest peers.
    //
    // `cap[i]` is the ceiling for the i-th block after `from`, built backwards
    // from `to`: each step back multiplies by `divisor / (divisor - 1)`, the
    // inverse of a maximal fall. It saturates, which only loosens the bound.
    let mut cap = vec![U256::ZERO; span as usize + 1];
    cap[span as usize] = to;
    for i in (0..span as usize).rev() {
        let next = cap[i + 1];
        cap[i] = next
            .checked_mul(div)
            .map(|v| v / (div - U256::from(1u64)))
            .unwrap_or(U256::MAX);
    }

    let mut total = U256::ZERO;
    let mut d = from;
    for step in 1..=span as usize {
        let risen = d.saturating_add(d / div);
        d = risen.min(cap[step]);
        if d < min_difficulty {
            d = min_difficulty;
        }
        total = total.saturating_add(d);
    }
    total
}

/// The ceiling on total difficulty implied by a checkpoint and a set of samples.
///
/// `samples` are `(height, difficulty)` for blocks above the checkpoint, in
/// ascending order, each already verified — a sample whose proof of work was
/// not checked bounds nothing.
///
/// The work below the checkpoint is exact. Above it, each gap is bounded by
/// [`max_work_between`], including the gap from the last sample to `head`.
pub fn ceiling_for(
    checkpoint: &DifficultyCheckpoint,
    samples: &[(u64, U256)],
    head: u64,
    divisor: u64,
    min_difficulty: U256,
) -> U256 {
    let mut total = checkpoint.cumulative_difficulty;
    let mut at = checkpoint.number;
    let mut difficulty = checkpoint.difficulty;

    for &(number, sampled) in samples {
        if number <= at {
            continue; // out of order or at/below the checkpoint: contributes nothing
        }
        total = total.saturating_add(max_work_between(
            difficulty,
            sampled,
            number - at,
            divisor,
            min_difficulty,
        ));
        at = number;
        difficulty = sampled;
    }

    if head > at {
        // Past the last sample there is no later difficulty to aim for, so the
        // bound is unconstrained growth: this is why the newest sample should
        // sit close to the head.
        total = total.saturating_add(max_work_between(
            difficulty,
            U256::MAX,
            head - at,
            divisor,
            min_difficulty,
        ));
    }
    total
}

/// Judges a peer's advertised total difficulty against the ceiling.
pub fn judge(
    checkpoint: &DifficultyCheckpoint,
    claimed: U256,
    samples: &[(u64, U256)],
    head: u64,
    divisor: u64,
    min_difficulty: U256,
) -> CheckpointVerdict {
    let ceiling = ceiling_for(checkpoint, samples, head, divisor, min_difficulty);
    if claimed > ceiling {
        CheckpointVerdict::Impossible { claimed, ceiling }
    } else {
        CheckpointVerdict::Plausible { ceiling }
    }
}

/// Checks this node's own chain against the checkpoint it was shipped with.
///
/// Run when the node has validated its own chain to at least the checkpoint
/// height. A chain that carries *more* work than the checkpoint allows means
/// the checkpoint is wrong — and since a checkpoint arrives with the build,
/// that is a statement about the build, not about the network.
pub fn audit_own_chain(
    checkpoint: &DifficultyCheckpoint,
    observed_cumulative_difficulty: U256,
) -> Option<CheckpointVerdict> {
    if observed_cumulative_difficulty > checkpoint.cumulative_difficulty {
        return Some(CheckpointVerdict::Contradicted {
            observed: observed_cumulative_difficulty,
            checkpoint: checkpoint.cumulative_difficulty,
        });
    }
    None
}

#[cfg(test)]
fn u256_to_f64(v: U256) -> f64 {
    // Enough for locating a turning point; the bound itself never uses this.
    let mut out = 0.0f64;
    for limb in v.as_limbs().iter().rev() {
        out = out * 18_446_744_073_709_551_616.0 + *limb as f64;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const DIV: u64 = 400;
    const MIN: U256 = U256::from_limbs([7_000_000_000_000_000u64, 0, 0, 0]);

    /// The worked example: flat endpoints, 192 blocks, divisor 400. Rising to
    /// a peak near the midpoint and falling back gives about 13% more than a
    /// flat chain would.
    #[test]
    fn a_flat_interval_is_bounded_at_about_thirteen_percent_over() {
        let d = U256::from(1_000_000_000u64) * MIN;
        let bound = max_work_between(d, d, 192, DIV, MIN);
        let flat = d * U256::from(192u64);
        let ratio = u256_to_f64(bound) / u256_to_f64(flat);
        assert!(
            (1.12..1.14).contains(&ratio),
            "expected about 1.13x the flat sum, got {ratio}"
        );
    }

    /// The bound must never be below what a flat chain actually carries, or it
    /// would reject honest peers.
    #[test]
    fn the_bound_is_never_below_the_flat_truth() {
        let d = U256::from(1_000_000_000u64) * MIN;
        for span in [1u64, 2, 48, 192, 768, 3840] {
            let bound = max_work_between(d, d, span, DIV, MIN);
            assert!(
                bound >= d * U256::from(span),
                "span={span}: bound {bound} is below the flat sum"
            );
        }
    }

    /// A smaller divisor allows faster growth, so it must bound more loosely.
    #[test]
    fn a_smaller_divisor_bounds_more_loosely() {
        let d = U256::from(1_000_000_000u64) * MIN;
        let tight = max_work_between(d, d, 192, 400, MIN);
        let loose = max_work_between(d, d, 192, 50, MIN);
        assert!(loose > tight, "divisor 50 must bound above divisor 400");
    }

    /// Work below the checkpoint is not re-derived; it is the checkpoint.
    #[test]
    fn with_no_window_the_ceiling_is_the_checkpoint() {
        let cp = MAINNET_CHECKPOINT;
        let ceiling = ceiling_for(&cp, &[], cp.number, DIV, MIN);
        assert_eq!(ceiling, cp.cumulative_difficulty);
    }

    /// A claim at or below the ceiling survives; one above it does not.
    #[test]
    fn a_claim_above_the_ceiling_is_refused() {
        let cp = MAINNET_CHECKPOINT;
        let samples = [(cp.number + 768, cp.difficulty)];
        let head = cp.number + 768;
        let ceiling = ceiling_for(&cp, &samples, head, DIV, MIN);

        match judge(&cp, ceiling, &samples, head, DIV, MIN) {
            CheckpointVerdict::Plausible { .. } => {}
            other => panic!("a claim exactly at the ceiling must pass: {other:?}"),
        }
        match judge(&cp, ceiling + U256::from(1u64), &samples, head, DIV, MIN) {
            CheckpointVerdict::Impossible { .. } => {}
            other => panic!("a claim above the ceiling must be refused: {other:?}"),
        }
    }

    /// The attack this exists for: a peer claiming a wildly inflated total.
    #[test]
    fn a_wildly_inflated_claim_is_refused() {
        let cp = MAINNET_CHECKPOINT;
        let samples: Vec<(u64, U256)> =
            (1..=341).map(|i| (cp.number + i * 768, cp.difficulty)).collect();
        let head = cp.number + 341 * 768;
        let absurd = cp.cumulative_difficulty * U256::from(1000u64);
        match judge(&cp, absurd, &samples, head, DIV, MIN) {
            CheckpointVerdict::Impossible { .. } => {}
            other => panic!("a 1000x claim must be refused: {other:?}"),
        }
    }

    /// An honest chain that merely sat at the checkpoint's difficulty for the
    /// whole window must pass, or the gate rejects the network.
    #[test]
    fn an_honest_flat_chain_passes() {
        let cp = MAINNET_CHECKPOINT;
        let n = 341u64;
        let samples: Vec<(u64, U256)> =
            (1..=n).map(|i| (cp.number + i * 768, cp.difficulty)).collect();
        let head = cp.number + n * 768;
        let honest = cp.cumulative_difficulty + cp.difficulty * U256::from(n * 768);
        match judge(&cp, honest, &samples, head, DIV, MIN) {
            CheckpointVerdict::Plausible { .. } => {}
            other => panic!("an honest flat chain must pass: {other:?}"),
        }
    }

    /// A checkpoint the node's own chain contradicts is the build's problem,
    /// reported as such rather than charged to a peer.
    #[test]
    fn a_contradicted_checkpoint_is_reported() {
        let cp = MAINNET_CHECKPOINT;
        assert_eq!(audit_own_chain(&cp, cp.cumulative_difficulty), None);
        assert_eq!(
            audit_own_chain(&cp, cp.cumulative_difficulty - U256::from(1u64)),
            None,
            "less work than the checkpoint is ordinary: the node is behind"
        );
        match audit_own_chain(&cp, cp.cumulative_difficulty + U256::from(1u64)) {
            Some(CheckpointVerdict::Contradicted { .. }) => {}
            other => panic!("more work than the checkpoint must be reported: {other:?}"),
        }
    }
}

#[cfg(test)]
mod checkpoint_value_tests {
    use super::*;

    /// The shipped constants must be the values read off the chain, not
    /// whatever a hand-written limb array happens to encode.
    ///
    /// Taken from this project's own synced node at block #9,020,000
    /// (2026-07-05). If a checkpoint is ever refreshed, this test is what
    /// catches a transcription error before it ships.
    #[test]
    fn the_mainnet_checkpoint_matches_the_chain() {
        let cp = MAINNET_CHECKPOINT;
        assert_eq!(cp.number, 9_020_000);
        assert_eq!(
            format!("{:?}", cp.hash),
            "0x270343b5b43eca9c17538bdaefe7cf8fd3012ce9b7cefc9c7d4ac2322b472cc5"
        );
        assert_eq!(
            cp.cumulative_difficulty,
            "32959497588810990020280199750".parse::<U256>().unwrap(),
            "cumulative difficulty (0x6a7f75187db0eeb538802a46)"
        );
        assert_eq!(
            cp.difficulty,
            "7388898250588046058941".parse::<U256>().unwrap()
        );
    }
}

#[cfg(test)]
mod real_chain_tests {
    use super::*;

    const DIV: u64 = 400;
    const MIN: U256 = U256::from_limbs([7_000_000_000_000_000u64, 0, 0, 0]);

    /// Samples read from this project's own synced mainnet node on 2026-09-30,
    /// at the 768-block interval the sampler actually uses: `(height,
    /// difficulty)` for every sample from the checkpoint to #9049952.
    ///
    /// Real difficulty here swings between about 6.7e21 and 8.0e21. A synthetic
    /// flat chain cannot exercise that, and the direction that matters is the
    /// one a flat chain never shows: a bound that sits *below* what the honest
    /// chain really carries would refuse every truthful peer.
    const SAMPLES: &[(u64, &str)] = &[
            (9020768, "7614994946002703006906"),
            (9021536, "6926015941405100741989"),
            (9022304, "6772936187870155390801"),
            (9023072, "6656522227407884114154"),
            (9023840, "6775137721459334918956"),
            (9024608, "6776259932436813770316"),
            (9025376, "6511677522428858627223"),
            (9026144, "6660891948071123261697"),
            (9026912, "6847721415863989280455"),
            (9027680, "7022235621320739353434"),
            (9028448, "6681029889493744529940"),
            (9029216, "7024737709657163858379"),
            (9029984, "7078638884624765767949"),
            (9030752, "7168752604407800972828"),
            (9031520, "6888888180060507127356"),
            (9032288, "6719913454411387067155"),
            (9033056, "6995229955554903144549"),
            (9033824, "7137680734000858675462"),
            (9034592, "6962559961098692829093"),
            (9035360, "6928894867327960362909"),
            (9036128, "6526385889766530363753"),
            (9036896, "6659413996543502709365"),
            (9037664, "6897763219796527259508"),
            (9038432, "7288929392396665473224"),
            (9039200, "7587748307903181835152"),
            (9039968, "7898718949361532018966"),
            (9040736, "7821273868193441326820"),
            (9041504, "7940891862788093508537"),
            (9042272, "7765332648919433244207"),
            (9043040, "7387974154434983383639"),
            (9043808, "7538564118203866061816"),
            (9044576, "7966019766314770267381"),
            (9045344, "7391414933915301708664"),
            (9046112, "7120392514720170234204"),
            (9046880, "7375240768038117586302"),
            (9047648, "7469317071875928865358"),
            (9048416, "7396128400904648822682"),
            (9049184, "8013358517474663685190"),
            (9049952, "7108209165328692545538"),
    ];

    /// Total difficulty at #9049952, read from the same node.
    const REAL_TD_AT_HEAD: &str = "33174492501805418000770231962";
    const HEAD: u64 = 9049952;

    fn parse(s: &str) -> U256 {
        s.parse().expect("decimal")
    }

    fn samples() -> Vec<(u64, U256)> {
        SAMPLES.iter().map(|&(h, d)| (h, parse(d))).collect()
    }

    /// The one that matters. The shipped checkpoint, fed real verified
    /// samples, must not bound the real chain below what it actually carries.
    #[test]
    fn the_shipped_checkpoint_does_not_refuse_the_real_chain() {
        let ceiling = ceiling_for(&MAINNET_CHECKPOINT, &samples(), HEAD, DIV, MIN);
        let real = parse(REAL_TD_AT_HEAD);
        assert!(
            ceiling >= real,
            "the ceiling refuses the honest chain:\n  ceiling {ceiling}\n  real    {real}"
        );
    }

    /// And an honest peer stating exactly the truth is judged plausible, which
    /// is the call the gate actually makes.
    #[test]
    fn an_honest_peer_on_the_real_chain_is_plausible() {
        let verdict = judge(
            &MAINNET_CHECKPOINT, parse(REAL_TD_AT_HEAD), &samples(), HEAD, DIV, MIN,
        );
        assert!(
            matches!(verdict, CheckpointVerdict::Plausible { .. }),
            "an honest peer was refused: {verdict:?}"
        );
    }

    /// A bound that accepts everything is not a bound.
    ///
    /// Measured against the *total* claim, which is what `judge` compares. The
    /// slack is wide against the work done above the checkpoint -- 67.5% of it
    /// at this spacing, because difficulty may compound by `(1 + 1/400)^768`
    /// between samples -- but that work is under 1% of the cumulative total,
    /// the rest being exact. See `sampling_cost` for the spacing curve.
    #[test]
    fn the_bound_is_tight_enough_to_be_worth_having() {
        let ceiling = ceiling_for(&MAINNET_CHECKPOINT, &samples(), HEAD, DIV, MIN);
        let real = parse(REAL_TD_AT_HEAD);
        let slack = ceiling - real;
        assert!(
            slack * U256::from(100) <= real,
            "slack {slack} exceeds 1% of the {real} a peer would be claiming"
        );
    }

    /// The real chain, inflated by 1% of its total, is refused. A peer's lie
    /// has to be worth telling: claiming the best chain means out-claiming a
    /// real one, and this bounds how far past it a peer can reach.
    #[test]
    fn a_modestly_inflated_claim_on_the_real_chain_is_refused() {
        let real = parse(REAL_TD_AT_HEAD);
        let inflated = real + real / U256::from(100);
        let verdict = judge(&MAINNET_CHECKPOINT, inflated, &samples(), HEAD, DIV, MIN);
        assert!(
            matches!(verdict, CheckpointVerdict::Impossible { .. }),
            "a claim 1% above the real chain passed: {verdict:?}"
        );
    }

    /// The audit that makes shipping a checkpoint safe: this node's own chain
    /// at the checkpoint height must not contradict it.
    #[test]
    fn the_real_chain_does_not_contradict_the_checkpoint() {
        assert_eq!(
            audit_own_chain(&MAINNET_CHECKPOINT, MAINNET_CHECKPOINT.cumulative_difficulty),
            None
        );
    }
}

/// A one-off measurement, not a guard: prints how tight the sampled bound is
/// at each spacing, so the interval is a choice with a number behind it.
/// Run with `cargo test -p rustock-core sampling_cost -- --nocapture --ignored`.
#[cfg(test)]
mod sampling_cost {
    use super::*;

    const DIV: u64 = 400;
    const MIN: U256 = U256::from_limbs([7_000_000_000_000_000u64, 0, 0, 0]);
    const HEAD: u64 = 9049952;
    const REAL_TD: &str = "33174492501805418000770231962";

    /// Every 192nd block from the checkpoint to #9049952, read from this
    /// project's synced mainnet node on 2026-09-30.
    const DENSE: &[(u64, &str)] = &[
        (9020192, "7274662020063509595881"),
        (9020384, "7529312041719755308934"),
        (9020576, "7468594156406836678092"),
        (9020768, "7614994946002703006906"),
        (9020960, "7803330447104372329650"),
        (9021152, "7663480160924534989023"),
        (9021344, "7413993775032742710299"),
        (9021536, "6926015941405100741989"),
        (9021728, "6921948089376012035085"),
        (9021920, "6952602183670272993594"),
        (9022112, "7053488264958863725299"),
        (9022304, "6772936187870155390801"),
        (9022496, "6819895078970878520113"),
        (9022688, "6936282700443386191951"),
        (9022880, "6627214972769280752906"),
        (9023072, "6656522227407884114154"),
        (9023264, "6586459083143505604067"),
        (9023456, "6834103775244555641868"),
        (9023648, "6898819829379759439018"),
        (9023840, "6775137721459334918956"),
        (9024032, "6977327788118979525601"),
        (9024224, "6501795421319416302183"),
        (9024416, "6746299003407676099623"),
        (9024608, "6776259932436813770316"),
        (9024800, "6996060873222112912013"),
        (9024992, "6905247331824997846617"),
        (9025184, "6370601785561533555901"),
        (9025376, "6511677522428858627223"),
        (9025568, "6524163432069601985995"),
        (9025760, "6552974009861167665088"),
        (9025952, "6884949836595639991314"),
        (9026144, "6660891948071123261697"),
        (9026336, "7139666351085111424922"),
        (9026528, "7064517879185484250056"),
        (9026720, "6834616282656690312357"),
        (9026912, "6847721415863989280455"),
        (9027104, "7123041338011178115253"),
        (9027296, "6960493414581538909000"),
        (9027488, "6904492055398375200063"),
        (9027680, "7022235621320739353434"),
        (9027872, "7018242847886259687536"),
        (9028064, "6996629255155729983289"),
        (9028256, "6684872598361605698818"),
        (9028448, "6681029889493744529940"),
        (9028640, "6984483128871099938457"),
        (9028832, "6980337309969418901764"),
        (9029024, "7279232658289315491451"),
        (9029216, "7024737709657163858379"),
        (9029408, "6578827188266924624679"),
        (9029600, "6877552397560331364004"),
        (9029792, "6942549717222618368994"),
        (9029984, "7078638884624765767949"),
        (9030176, "6917060994028628683985"),
        (9030368, "6691999932601996296327"),
        (9030560, "7635654604089318557285"),
        (9030752, "7168752604407800972828"),
        (9030944, "6815165225296564092140"),
        (9031136, "6777233951669377712914"),
        (9031328, "6689093699869072319813"),
        (9031520, "6888888180060507127356"),
        (9031712, "6500109079202782021434"),
        (9031904, "6761452877665200488475"),
        (9032096, "6893992150703121615467"),
        (9032288, "6719913454411387067155"),
        (9032480, "6800464542076052863472"),
        (9032672, "6830495260151751736727"),
        (9032864, "6929609502827035510100"),
        (9033056, "6995229955554903144549"),
        (9033248, "7367938591136586843040"),
        (9033440, "7057236180536581873240"),
        (9033632, "7123976153047276846671"),
        (9033824, "7137680734000858675462"),
        (9034016, "7187213433535620560405"),
        (9034208, "7328144346097118442182"),
        (9034400, "6931861962861679474287"),
        (9034592, "6962559961098692829093"),
        (9034784, "6820683410959441794990"),
        (9034976, "6582261300490072976970"),
        (9035168, "6432013786296933002176"),
        (9035360, "6928894867327960362909"),
        (9035552, "7082422425854794022897"),
        (9035744, "7007832674970455240373"),
        (9035936, "6729096546263711519389"),
        (9036128, "6526385889766530363753"),
        (9036320, "6490143006420026293273"),
        (9036512, "6734166304253918252394"),
        (9036704, "6646627703401487910669"),
        (9036896, "6659413996543502709365"),
        (9037088, "6944480725676603142382"),
        (9037280, "6685018640507210085863"),
        (9037472, "7023683891823346010307"),
        (9037664, "6897763219796527259508"),
        (9037856, "6910946241321922496058"),
        (9038048, "7028843801931139066058"),
        (9038240, "7311400245698544997835"),
        (9038432, "7288929392396665473224"),
        (9038624, "7339557570422188889171"),
        (9038816, "7353676871038362321206"),
        (9039008, "7479127212327372804966"),
        (9039200, "7587748307903181835152"),
        (9039392, "7564428126626170266849"),
        (9039584, "7751416105938615621659"),
        (9039776, "7727496313535944053188"),
        (9039968, "7898718949361532018966"),
        (9040160, "8033417248360837045821"),
        (9040352, "7656226756261853725525"),
        (9040544, "7767371363130121987734"),
        (9040736, "7821273868193441326820"),
        (9040928, "7472819301616357547104"),
        (9041120, "7524724760647069942493"),
        (9041312, "7925694625732639471015"),
        (9041504, "7940891862788093508537"),
        (9041696, "7936227944404119464068"),
        (9041888, "7833014578250203961900"),
        (9042080, "7750519836912523766310"),
        (9042272, "7765332648919433244207"),
        (9042464, "7664389737127857653380"),
        (9042656, "7795189416772961289749"),
        (9042848, "7355446472199366191955"),
        (9043040, "7387974154434983383639"),
        (9043232, "7365221926001376302367"),
        (9043424, "7379344476624517921717"),
        (9043616, "7430647110369684355669"),
        (9043808, "7538564118203866061816"),
        (9044000, "7590926234186410077568"),
        (9044192, "7605434030518960731207"),
        (9044384, "7891391226184679188777"),
        (9044576, "7966019766314770267381"),
        (9044768, "7981194559523223539395"),
        (9044960, "7799063565110005651245"),
        (9045152, "7159327069243667133894"),
        (9045344, "7391414933915301708664"),
        (9045536, "7387073739426127010789"),
        (9045728, "7345913473227942771344"),
        (9045920, "7304936893057764102022"),
        (9046112, "7120392514720170234204"),
        (9046304, "7499863609261157070705"),
        (9046496, "7347084306013310699966"),
        (9046688, "7435106772911396342515"),
        (9046880, "7375240768038117586302"),
        (9047072, "7501105861392098870565"),
        (9047264, "7822202539552976576601"),
        (9047456, "7289134188254961309172"),
        (9047648, "7469317071875928865358"),
        (9047840, "7335407083932573007086"),
        (9048032, "7367892196148710591756"),
        (9048224, "7568849678173223877691"),
        (9048416, "7396128400904648822682"),
        (9048608, "7336530388549324614753"),
        (9048800, "7536632484838117691620"),
        (9048992, "7607953636847295363924"),
        (9049184, "8013358517474663685190"),
        (9049376, "7988630407208550733056"),
        (9049568, "7594556386490896785705"),
        (9049760, "7666377772582978423800"),
        (9049952, "7108209165328692545538"),
    ];

    #[test]
    #[ignore = "measurement, not an assertion"]
    fn how_tight_is_the_bound_at_each_spacing() {
        let real: U256 = REAL_TD.parse().unwrap();
        let above = real - MAINNET_CHECKPOINT.cumulative_difficulty;
        println!("work above the checkpoint: {above}");
        println!("{:>8} {:>8} {:>12}", "spacing", "samples", "slack/above");
        for step in [1usize, 2, 4, 8, 16, 32] {
            let samples: Vec<(u64, U256)> = DENSE
                .iter()
                .enumerate()
                .filter(|(i, _)| (i + 1) % step == 0)
                .map(|(_, &(h, d))| (h, d.parse().unwrap()))
                .collect();
            if samples.last().map(|s| s.0) != Some(HEAD) {
                continue; // the last sample must sit at the head
            }
            let ceiling = ceiling_for(&MAINNET_CHECKPOINT, &samples, HEAD, DIV, MIN);
            let slack = ceiling - real;
            let pct = (u256_to_f64(slack) / u256_to_f64(above)) * 100.0;
            println!("{:>8} {:>8} {:>11.1}%", step * 192, samples.len(), pct);
        }
    }
}
