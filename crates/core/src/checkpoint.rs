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
/// Taken from a fully synced node's own chain. A checkpoint should be refreshed
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
