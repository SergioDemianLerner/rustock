//! Sampling a peer's chain to bound the work it can be carrying.
//!
//! A peer advertises a total difficulty. Below the shipped checkpoint that
//! claim is answered by arithmetic — the work to that height is known, so
//! nothing may claim more for it. Above the checkpoint there is nothing to
//! check against, and that window is where a lie lives.
//!
//! So sample it. Ask the peer for one header every [`SAMPLE_INTERVAL`] blocks
//! above the checkpoint, verify each one's proof of work, and feed the
//! difficulties to [`rustock_core::checkpoint::ceiling_for`]. The retarget rule
//! bounds how much work the blocks *between* samples can represent, so a
//! ceiling follows for the whole chain — and a peer claiming more than the
//! ceiling is claiming something no valid chain can provide.
//!
//! # What makes this affordable
//!
//! The samples must come from the peer being judged: if its chain is
//! fabricated, nobody else holds those blocks. That puts the cost under rskj's
//! inbound limit of 1,000 messages per minute per peer — so the number of
//! samples, not the bytes, is the budget.
//!
//! With a checkpoint refreshed each release the window is small. Three months
//! of chain at [`SAMPLE_INTERVAL`] is about 340 samples: roughly twenty seconds
//! against that limit, against the ~34 minutes it would take to sample the
//! whole chain.
//!
//! # What it does not do
//!
//! It does not say *which* chain a peer is on, only how much work it may claim.
//! A peer serving a valid minority chain within the ceiling passes. It is a
//! filter on who is worth talking to, not a substitute for validating what they
//! then send.

use alloy_primitives::{B256, U256};
use rustock_core::checkpoint::{ceiling_for, CheckpointVerdict, DifficultyCheckpoint};
use rustock_core::validation::HeaderVerifier;
use rustock_core::Header;
use std::collections::BTreeMap;

/// Blocks between samples above the checkpoint.
///
/// The bound loosens roughly as `(1 + 1/divisor)^(N/2)`, so this trades
/// tightness against request count. At 768 and the post-papyrus200 divisor of
/// 400 the bound is about 1.68x the true work — loose enough to admit any
/// honest chain, tight enough that an inflated claim has nowhere to hide.
pub const SAMPLE_INTERVAL: u64 = 768;

/// Give up on a peer that has not answered this many of its samples.
///
/// A peer can always answer slowly instead of lying. That is not a lesser
/// outcome: a peer that will not substantiate its claim is as useless as one
/// that cannot, and eliminating it costs nothing.
pub const MISSING_SAMPLES_ALLOWED: usize = 4;

/// Where a sample sits and what was asked of the peer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SampleRequest {
    pub number: u64,
    pub hash: B256,
}

/// Judges one peer's advertised total difficulty.
#[derive(Debug)]
pub struct ChainSampler {
    checkpoint: DifficultyCheckpoint,
    head: u64,
    divisor: u64,
    min_difficulty: U256,
    /// Heights still to hear about, and the hash each was asked for.
    outstanding: BTreeMap<u64, B256>,
    /// Verified difficulties, by height.
    collected: BTreeMap<u64, U256>,
    pub(crate) missing: usize,
}

impl ChainSampler {
    /// Plans the samples for a peer claiming `head`, given skeleton entries
    /// `(number, hash)` it has already supplied.
    ///
    /// Only entries above the checkpoint are sampled; below it the checkpoint
    /// answers exactly and no request is needed.
    pub fn new(
        checkpoint: DifficultyCheckpoint,
        head: u64,
        skeleton: &[(u64, B256)],
        divisor: u64,
        min_difficulty: U256,
    ) -> Self {
        let mut outstanding = BTreeMap::new();
        let mut next = checkpoint.number.saturating_add(SAMPLE_INTERVAL);
        for &(number, hash) in skeleton {
            if number > checkpoint.number && number >= next {
                outstanding.insert(number, hash);
                next = number.saturating_add(SAMPLE_INTERVAL);
            }
        }
        Self {
            checkpoint,
            head,
            divisor,
            min_difficulty,
            outstanding,
            collected: BTreeMap::new(),
            missing: 0,
        }
    }

    /// Adds skeleton identifiers, picking the ones that fall on the sampling
    /// interval. Called as identifiers arrive rather than all at once,
    /// because a skeleton response covers only part of the window.
    pub fn absorb_skeleton(&mut self, identifiers: &[(u64, B256)]) {
        for &(number, hash) in identifiers {
            if number <= self.checkpoint.number || self.collected.contains_key(&number) {
                continue;
            }
            let since = number - self.checkpoint.number;
            if since % SAMPLE_INTERVAL == 0 {
                self.outstanding.entry(number).or_insert(hash);
            }
        }
    }

    /// The samples still wanted.
    pub fn wanted(&self) -> Vec<SampleRequest> {
        self.outstanding
            .iter()
            .map(|(&number, &hash)| SampleRequest { number, hash })
            .collect()
    }

    pub fn is_complete(&self) -> bool {
        self.outstanding.is_empty()
    }

    /// Takes a header offered as a sample.
    ///
    /// The header must be the one that was asked for — same height, same hash —
    /// and must pass its own rules, proof of work included. A sample that is
    /// not verified bounds nothing, so an unverified one is no better than a
    /// missing one and is counted as such.
    pub fn offer(&mut self, header: &Header, verifier: &HeaderVerifier) -> bool {
        let number = header.number;
        let Some(&expected) = self.outstanding.get(&number) else {
            return false; // not asked for
        };
        if header.hash() != expected {
            self.outstanding.remove(&number);
            self.missing += 1;
            return false;
        }
        if verifier.verify(header, None).is_err() {
            self.outstanding.remove(&number);
            self.missing += 1;
            return false;
        }
        self.outstanding.remove(&number);
        self.collected.insert(number, header.difficulty);
        true
    }

    /// Records that a sample will not be answered.
    pub fn give_up_on(&mut self, number: u64) {
        if self.outstanding.remove(&number).is_some() {
            self.missing += 1;
        }
    }

    /// Whether too much of the chain went unsubstantiated to judge it.
    pub fn too_many_missing(&self) -> bool {
        self.missing > MISSING_SAMPLES_ALLOWED
    }

    /// The ceiling implied by the checkpoint and the samples gathered.
    pub fn ceiling(&self) -> U256 {
        let samples: Vec<(u64, U256)> =
            self.collected.iter().map(|(&n, &d)| (n, d)).collect();
        ceiling_for(
            &self.checkpoint,
            &samples,
            self.head,
            self.divisor,
            self.min_difficulty,
        )
    }

    /// Judges a claim against the ceiling.
    pub fn judge(&self, claimed: U256) -> CheckpointVerdict {
        let ceiling = self.ceiling();
        if claimed > ceiling {
            CheckpointVerdict::Impossible { claimed, ceiling }
        } else {
            CheckpointVerdict::Plausible { ceiling }
        }
    }

    /// Rejects a claim that the checkpoint alone already disproves.
    ///
    /// Free: no samples, no requests. A peer claiming more work than the
    /// checkpoint allows *for a height at or below it* is refuted by
    /// arithmetic.
    pub fn refuted_by_checkpoint_alone(
        checkpoint: &DifficultyCheckpoint,
        claimed: U256,
        claimed_height: u64,
    ) -> bool {
        claimed_height <= checkpoint.number && claimed > checkpoint.cumulative_difficulty
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use rustock_core::checkpoint::MAINNET_CHECKPOINT;

    const DIV: u64 = 400;
    const MIN: U256 = U256::from_limbs([7_000_000_000_000_000u64, 0, 0, 0]);

    pub(crate) fn header_at(number: u64, difficulty: U256) -> Header {
        Header {
            number,
            difficulty,
            parent_hash: B256::ZERO,
            ommers_hash: rustock_execution::processor::compute_ommers_hash(&[]),
            beneficiary: alloy_primitives::Address::ZERO,
            state_root: B256::ZERO,
            transactions_root: rustock_core::ordered_tx_trie_root(&[], true),
            receipts_root: B256::ZERO,
            logs_bloom: Default::default(),
            extension_data: None,
            gas_limit: U256::from(8_000_000),
            gas_used: 0,
            timestamp: number * 15,
            extra_data: alloy_primitives::Bytes::default(),
            paid_fees: U256::ZERO,
            minimum_gas_price: U256::ZERO,
            uncle_count: 0,
            umm_root: None,
            bitcoin_merged_mining_header: None,
            bitcoin_merged_mining_merkle_proof: None,
            bitcoin_merged_mining_coinbase_transaction: None,
            cached_hash: None,
            cached_hash_for_merged_mining: None,
        }
    }

    fn skeleton(from: u64, count: u64, step: u64) -> Vec<(u64, B256)> {
        (1..=count)
            .map(|i| {
                let n = from + i * step;
                (n, header_at(n, MAINNET_CHECKPOINT.difficulty).hash())
            })
            .collect()
    }

    /// Nothing below the checkpoint is sampled: the checkpoint answers it.
    #[test]
    fn only_the_window_above_the_checkpoint_is_sampled() {
        let cp = MAINNET_CHECKPOINT;
        let mut sk = skeleton(cp.number, 4, SAMPLE_INTERVAL);
        sk.push((cp.number - 5000, B256::repeat_byte(1)));
        sk.push((cp.number, B256::repeat_byte(2)));

        let s = ChainSampler::new(cp, cp.number + 4 * SAMPLE_INTERVAL, &sk, DIV, MIN);
        assert!(s.wanted().iter().all(|r| r.number > cp.number));
        assert_eq!(s.wanted().len(), 4);
    }

    /// A claim the checkpoint alone disproves costs no requests at all.
    #[test]
    fn a_claim_below_the_checkpoint_is_refuted_for_free() {
        let cp = MAINNET_CHECKPOINT;
        assert!(ChainSampler::refuted_by_checkpoint_alone(
            &cp,
            cp.cumulative_difficulty + U256::from(1u64),
            cp.number,
        ));
        assert!(!ChainSampler::refuted_by_checkpoint_alone(
            &cp,
            cp.cumulative_difficulty,
            cp.number,
        ));
        assert!(
            !ChainSampler::refuted_by_checkpoint_alone(
                &cp,
                cp.cumulative_difficulty * U256::from(2u64),
                cp.number + 1,
            ),
            "above the checkpoint height it takes sampling, not arithmetic"
        );
    }

    /// A sample that is not the block that was asked for buys nothing, and is
    /// counted against the peer.
    #[test]
    fn a_substituted_sample_is_not_accepted() {
        let cp = MAINNET_CHECKPOINT;
        let sk = skeleton(cp.number, 2, SAMPLE_INTERVAL);
        let mut s = ChainSampler::new(cp, cp.number + 2 * SAMPLE_INTERVAL, &sk, DIV, MIN);
        let verifier = HeaderVerifier::new();

        // Right height, wrong block.
        let impostor = header_at(cp.number + SAMPLE_INTERVAL, cp.difficulty * U256::from(9u64));
        assert!(!s.offer(&impostor, &verifier));
        assert_eq!(s.wanted().len(), 1, "the slot is spent either way");
    }

    /// An honest chain sitting at the checkpoint's difficulty passes.
    #[test]
    fn an_honest_claim_passes() {
        let cp = MAINNET_CHECKPOINT;
        let n = 8u64;
        let sk = skeleton(cp.number, n, SAMPLE_INTERVAL);
        let head = cp.number + n * SAMPLE_INTERVAL;
        let mut s = ChainSampler::new(cp, head, &sk, DIV, MIN);
        let verifier = HeaderVerifier::new();
        for (number, _) in &sk {
            assert!(s.offer(&header_at(*number, cp.difficulty), &verifier));
        }
        assert!(s.is_complete());

        let honest = cp.cumulative_difficulty + cp.difficulty * U256::from(n * SAMPLE_INTERVAL);
        match s.judge(honest) {
            CheckpointVerdict::Plausible { .. } => {}
            other => panic!("an honest chain must pass: {other:?}"),
        }
    }

    /// The attack: a claim far above anything the sampled chain could carry.
    #[test]
    fn an_inflated_claim_is_refused() {
        let cp = MAINNET_CHECKPOINT;
        let n = 8u64;
        let sk = skeleton(cp.number, n, SAMPLE_INTERVAL);
        let head = cp.number + n * SAMPLE_INTERVAL;
        let mut s = ChainSampler::new(cp, head, &sk, DIV, MIN);
        let verifier = HeaderVerifier::new();
        for (number, _) in &sk {
            s.offer(&header_at(*number, cp.difficulty), &verifier);
        }
        match s.judge(cp.cumulative_difficulty * U256::from(100u64)) {
            CheckpointVerdict::Impossible { .. } => {}
            other => panic!("a 100x claim must be refused: {other:?}"),
        }
    }

    /// A peer that simply does not answer is eliminated rather than waited on.
    #[test]
    fn a_peer_that_will_not_answer_is_given_up_on() {
        let cp = MAINNET_CHECKPOINT;
        let sk = skeleton(cp.number, 10, SAMPLE_INTERVAL);
        let mut s = ChainSampler::new(cp, cp.number + 10 * SAMPLE_INTERVAL, &sk, DIV, MIN);
        assert!(!s.too_many_missing());
        for (number, _) in sk.iter().take(MISSING_SAMPLES_ALLOWED + 1) {
            s.give_up_on(*number);
        }
        assert!(s.too_many_missing(), "an unresponsive peer must not be waited on forever");
    }
}

/// What a [`SamplingGate`] wants next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateAction {
    /// Ask for skeleton identifiers from this height, to learn the hashes of
    /// blocks in the window. The gate cannot request a header without a hash.
    RequestSkeleton { start: u64 },
    /// Ask for the single header at this hash.
    RequestHeader { number: u64, hash: B256 },
}

/// Where a gate has got to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateOutcome {
    /// Still gathering.
    Pending,
    /// The peer's claim is possible. Not a statement that it is honest.
    Passed { ceiling: U256 },
    /// The claim is more work than any chain through these samples could
    /// carry, or the peer would not substantiate it.
    Rejected(GateRejection),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateRejection {
    /// Refuted by arithmetic against the checkpoint alone.
    BelowCheckpoint { claimed: U256, allowed: U256 },
    /// Refuted by the sampled ceiling.
    AboveCeiling { claimed: U256, ceiling: U256 },
    /// The peer did not answer enough of its samples. A peer that will not
    /// substantiate its claim is no more useful than one that cannot.
    Unsubstantiated { missing: usize },
}

/// Drives the sampling of one peer, start to verdict.
///
/// Two phases, because a header can only be requested by hash and the gate
/// starts with none: collect skeleton identifiers across the window above the
/// checkpoint, then request the sampled ones.
///
/// The skeleton step is not free but it is cheap — one request covers
/// `skeleton_span` blocks — and both phases together stay well inside rskj's
/// inbound limit of 1,000 messages per minute per peer for a checkpoint
/// refreshed at release cadence.
#[derive(Debug)]
pub struct SamplingGate {
    sampler: ChainSampler,
    claimed: U256,
    checkpoint_number: u64,
    head: u64,
    /// Blocks one skeleton response covers.
    skeleton_span: u64,
    /// Next skeleton start not yet asked for.
    next_skeleton: u64,
    skeletons_outstanding: usize,
    /// Identifiers learned but not yet turned into sample requests.
    learned: BTreeMap<u64, B256>,
    started: bool,
}

impl SamplingGate {
    pub fn new(
        checkpoint: DifficultyCheckpoint,
        claimed: U256,
        head: u64,
        divisor: u64,
        min_difficulty: U256,
        skeleton_span: u64,
    ) -> Self {
        let checkpoint_number = checkpoint.number;
        Self {
            sampler: ChainSampler::new(checkpoint, head, &[], divisor, min_difficulty),
            claimed,
            checkpoint_number,
            head,
            skeleton_span: skeleton_span.max(1),
            next_skeleton: checkpoint_number,
            skeletons_outstanding: 0,
            learned: BTreeMap::new(),
            started: false,
        }
    }

    /// Whether the checkpoint alone settles it, before any request is made.
    pub fn refuted_immediately(
        checkpoint: &DifficultyCheckpoint,
        claimed: U256,
        height: u64,
    ) -> Option<GateRejection> {
        if ChainSampler::refuted_by_checkpoint_alone(checkpoint, claimed, height) {
            return Some(GateRejection::BelowCheckpoint {
                claimed,
                allowed: checkpoint.cumulative_difficulty,
            });
        }
        None
    }

    /// The next things to ask the peer for.
    pub fn poll(&mut self) -> Vec<GateAction> {
        self.started = true;
        let mut out = Vec::new();

        // Turn whatever identifiers are in hand into sample requests.
        let sampled: Vec<(u64, B256)> =
            self.learned.iter().map(|(&n, &h)| (n, h)).collect();
        if !sampled.is_empty() {
            self.learned.clear();
            self.sampler.absorb_skeleton(&sampled);
        }
        for req in self.sampler.wanted() {
            out.push(GateAction::RequestHeader { number: req.number, hash: req.hash });
        }

        // And keep the skeleton walk moving until the window is covered.
        while self.next_skeleton < self.head && self.skeletons_outstanding < 4 {
            out.push(GateAction::RequestSkeleton { start: self.next_skeleton });
            self.next_skeleton = self.next_skeleton.saturating_add(self.skeleton_span);
            self.skeletons_outstanding += 1;
        }
        out
    }

    pub fn on_skeleton(&mut self, identifiers: &[(u64, B256)]) {
        self.skeletons_outstanding = self.skeletons_outstanding.saturating_sub(1);
        for &(number, hash) in identifiers {
            if number > self.checkpoint_number && number <= self.head {
                self.learned.insert(number, hash);
            }
        }
    }

    pub fn on_header(&mut self, header: &Header, verifier: &HeaderVerifier) {
        self.sampler.offer(header, verifier);
    }

    pub fn give_up_on(&mut self, number: u64) {
        self.sampler.give_up_on(number);
    }

    /// The verdict, if one can be reached yet.
    pub fn outcome(&self) -> GateOutcome {
        if self.sampler.too_many_missing() {
            return GateOutcome::Rejected(GateRejection::Unsubstantiated {
                missing: self.sampler.missing,
            });
        }
        // Judge as soon as the window is covered and every sample answered.
        let covered = self.next_skeleton >= self.head && self.skeletons_outstanding == 0;
        if !self.started || !covered || !self.sampler.is_complete() || !self.learned.is_empty() {
            return GateOutcome::Pending;
        }
        match self.sampler.judge(self.claimed) {
            CheckpointVerdict::Plausible { ceiling } => GateOutcome::Passed { ceiling },
            CheckpointVerdict::Impossible { claimed, ceiling } => {
                GateOutcome::Rejected(GateRejection::AboveCeiling { claimed, ceiling })
            }
            // `judge` never returns this; a contradicted checkpoint is about
            // this node's own chain, not a peer's claim.
            CheckpointVerdict::Contradicted { .. } => GateOutcome::Pending,
        }
    }
}

#[cfg(test)]
mod gate_tests {
    use super::tests::header_at;
    use super::*;
    use rustock_core::checkpoint::MAINNET_CHECKPOINT;

    const DIV: u64 = 400;
    const MIN: U256 = U256::from_limbs([7_000_000_000_000_000u64, 0, 0, 0]);
    const SPAN: u64 = 192 * 20; // one skeleton response, as the server serves it

    /// Drives a gate to a verdict against a peer whose window is `blocks`
    /// long, answering every request honestly at `difficulty`.
    fn run_honest(blocks: u64, claimed: U256) -> GateOutcome {
        let cp = MAINNET_CHECKPOINT;
        let head = cp.number + blocks;
        let mut gate = SamplingGate::new(cp, claimed, head, DIV, MIN, SPAN);
        let verifier = HeaderVerifier::new();

        for _ in 0..64 {
            let actions = gate.poll();
            if actions.is_empty() && gate.outcome() != GateOutcome::Pending {
                break;
            }
            for action in actions {
                match action {
                    GateAction::RequestSkeleton { start } => {
                        // As a server would: identifiers every 192 blocks.
                        let ids: Vec<(u64, B256)> = (0..20)
                            .map(|i| start + (i + 1) * 192)
                            .filter(|n| *n <= head)
                            .map(|n| (n, header_at(n, cp.difficulty).hash()))
                            .collect();
                        gate.on_skeleton(&ids);
                    }
                    GateAction::RequestHeader { number, .. } => {
                        gate.on_header(&header_at(number, cp.difficulty), &verifier);
                    }
                }
            }
        }
        gate.outcome()
    }

    /// An honest peer's claim survives the gate.
    #[test]
    fn an_honest_peer_passes_the_gate() {
        let cp = MAINNET_CHECKPOINT;
        let blocks = 20_000u64;
        let honest = cp.cumulative_difficulty + cp.difficulty * U256::from(blocks);
        match run_honest(blocks, honest) {
            GateOutcome::Passed { .. } => {}
            other => panic!("an honest peer must pass: {other:?}"),
        }
    }

    /// The attack: a claim far beyond what the sampled chain can carry.
    #[test]
    fn an_inflated_claim_is_rejected_by_the_gate() {
        let cp = MAINNET_CHECKPOINT;
        match run_honest(20_000, cp.cumulative_difficulty * U256::from(50u64)) {
            GateOutcome::Rejected(GateRejection::AboveCeiling { .. }) => {}
            other => panic!("a 50x claim must be rejected: {other:?}"),
        }
    }

    /// A claim the checkpoint disproves needs no requests whatsoever.
    #[test]
    fn a_claim_below_the_checkpoint_never_reaches_the_network() {
        let cp = MAINNET_CHECKPOINT;
        assert!(matches!(
            SamplingGate::refuted_immediately(
                &cp,
                cp.cumulative_difficulty + U256::from(1u64),
                cp.number
            ),
            Some(GateRejection::BelowCheckpoint { .. })
        ));
        assert!(SamplingGate::refuted_immediately(&cp, cp.cumulative_difficulty, cp.number)
            .is_none());
    }

    /// Answering slowly must not be a way to pass. A peer that will not
    /// substantiate its claim is eliminated on the same terms as one that
    /// cannot.
    #[test]
    fn a_silent_peer_is_rejected_rather_than_waited_on() {
        let cp = MAINNET_CHECKPOINT;
        let head = cp.number + 20_000;
        let mut gate =
            SamplingGate::new(cp, cp.cumulative_difficulty, head, DIV, MIN, SPAN);

        for _ in 0..8 {
            for action in gate.poll() {
                match action {
                    GateAction::RequestSkeleton { start } => {
                        let ids: Vec<(u64, B256)> = (0..20)
                            .map(|i| start + (i + 1) * 192)
                            .filter(|n| *n <= head)
                            .map(|n| (n, header_at(n, cp.difficulty).hash()))
                            .collect();
                        gate.on_skeleton(&ids);
                    }
                    // Every header request goes unanswered.
                    GateAction::RequestHeader { number, .. } => gate.give_up_on(number),
                }
            }
        }
        match gate.outcome() {
            GateOutcome::Rejected(GateRejection::Unsubstantiated { .. }) => {}
            other => panic!("a peer that answers nothing must be rejected: {other:?}"),
        }
    }

    /// The gate does not reach a verdict before the window is covered —
    /// otherwise a peer could pass by answering only the first few samples.
    #[test]
    fn no_verdict_before_the_window_is_covered() {
        let cp = MAINNET_CHECKPOINT;
        let mut gate = SamplingGate::new(
            cp,
            cp.cumulative_difficulty,
            cp.number + 200_000,
            DIV,
            MIN,
            SPAN,
        );
        assert_eq!(gate.outcome(), GateOutcome::Pending);
        let _ = gate.poll();
        assert_eq!(
            gate.outcome(),
            GateOutcome::Pending,
            "a verdict before the window is covered would be a way past the gate"
        );
    }
}
