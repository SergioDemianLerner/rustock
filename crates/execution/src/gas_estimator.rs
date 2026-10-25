//! Single-pass gas estimation — the "exactimator".
//!
//! `eth_estimateGas` asks for the smallest gas limit at which a transaction
//! still succeeds. The obvious way to answer is to binary search: execute at
//! the cap, then halve the interval until it closes. That costs about
//! `log2(cap)` executions — roughly two dozen — and it is what this node did.
//!
//! It is not necessary. One execution at the cap already knows the answer, if
//! you record the right three things while it runs. The design is Sergio
//! Demian Lerner's, and rskj has carried it since
//! `EthModule.internalEstimateGas`:
//!
//! ```java
//! long estimatedGas = reversibleExecutionResult.getMovedRemainingGasToChild() ?
//!         reversibleExecutionResult.getGasUsed() + reversibleExecutionResult.getDeductedRefund() :
//!         reversibleExecutionResult.getMaxGasUsed();
//! if (reversibleExecutionResult.isCallWithValuePerformed()) {
//!     estimatedGas += GasCost.STIPEND_CALL;
//! }
//! ```
//!
//! ## Why the peak, and not the total
//!
//! rskj's gas counter is not monotonic. At a CALL it adds the *whole*
//! allowance handed to the callee, and when the callee returns it subtracts
//! whatever came back (`ProgramResult.refundGas` is a subtraction). So the
//! counter rises to the full commitment and falls again.
//!
//! The peak is the interesting number. A contract that calls out with a fixed
//! generous stipend -- `CALL(100000, ...)` to a callee that uses 5,000 --
//! finishes having consumed 5,000, but the caller had to *hold* 100,000 at
//! that instant or the call could not have been made. Bill the total and the
//! transaction runs out of gas; bill the peak and it does not. That is what
//! binary search discovers the slow way.
//!
//! ## Why the fallback
//!
//! When a callee is handed every last unit of the caller's remaining gas, the
//! peak stops saying anything: it is just the cap we happened to run at.
//! rskj detects that case (`calleeGas == remainingGas`) and switches to
//! `gasUsed + deductedRefund`, which is the honest total — what was spent,
//! plus the refunds that are credited at the end but must be *supplied* up
//! front.
//!
//! Note this is RSK-specific and the reason the rule is this simple here: RSK
//! never adopted EIP-150's 63/64 retention, so a caller really can hand over
//! everything (see `rsk_instructions`, which overrides the CALL family to
//! `calleeGas = min(remaining, requested + stipend)`). On a chain with the
//! 63/64 rule the "all of it" case is unreachable and the detection would
//! need to compare against the retained fraction instead.
//!
//! ## The stipend
//!
//! A CALL that transfers value grants the callee a 2,300-gas stipend it did
//! not have to pay for. The caller still has to be able to cover it, so the
//! estimate carries it.

use alloy_primitives::Address;
use revm::context::ContextTr;
use revm::inspector::Inspector;
use revm::interpreter::{
    CallInputs, CallOutcome, CreateInputs, CreateOutcome, Interpreter, InterpreterTypes,
};

/// rskj `GasCost.STIPEND_CALL`.
pub const STIPEND_CALL: u64 = 2_300;

/// What one execution has to report for the estimate to be computable.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EstimateTelemetry {
    /// Gas consumed before the end-of-transaction refund -- revm's
    /// `ResultGas::spent`, which is the same quantity rskj's `getGasUsed()`
    /// holds. Not `used()`, which is after the refund and would under-report.
    pub gas_used: u64,
    /// Refunds credited at the end. They reduce what is *charged* and not
    /// what has to be *available*, so an estimate has to add them back.
    pub refunded: u64,
    /// rskj's `maxGasUsed`: the high-water mark of the root frame's
    /// commitment, counting a callee's whole allowance from the moment it is
    /// handed over.
    pub max_committed: u64,
    /// Some call was given every unit the caller had left, so `max_committed`
    /// is an artefact of the cap we ran at rather than a property of the
    /// transaction.
    pub moved_all_to_child: bool,
    /// Some call transferred value, so the callee's stipend has to be
    /// covered.
    pub call_with_value: bool,
}

impl EstimateTelemetry {
    /// The smallest gas limit this transaction is expected to succeed at.
    ///
    /// `cap` is the ceiling the caller asked about -- the block gas limit, or
    /// an explicit `gas` in the call object. The answer is clamped to it for
    /// the same reason rskj clamps: returning more than the caller can
    /// actually supply is not an estimate, it is a refusal in disguise.
    pub fn estimate(&self, cap: u64) -> u64 {
        let mut estimate: u64 = if self.moved_all_to_child {
            // rskj's branch, and note what `gas_used` is here: the *pre-refund*
            // figure. rskj's `getGasUsed()` is never reduced by the end-of-
            // transaction refund -- `TransactionExecutor.refundGas` credits
            // `gasLeftover` instead -- so this sum is consumption plus the
            // refund on top, and is deliberately the looser of the two
            // answers. It is only reached when the peak has stopped meaning
            // anything.
            self.gas_used.saturating_add(self.refunded)
        } else {
            // The peak, alone. The refund is credited back at the end, so the
            // caller never has to hold it; the peak is the whole requirement.
            self.max_committed
        };
        // Whatever the branch, the estimate can never be below what the
        // transaction actually spent. A transfer to an account with no code
        // never enters the interpreter, so there is no peak to report and
        // this floor is the entire answer.
        //
        // It has to be applied *here*, before the stipend: flooring
        // afterwards would discard the stipend whenever the peak came out
        // below the spend, and the estimate would then be too small to
        // execute -- which is the one way an estimator can be useless rather
        // than merely imprecise.
        estimate = estimate.max(self.gas_used);
        if self.call_with_value {
            estimate = estimate.saturating_add(STIPEND_CALL);
        }
        estimate.min(cap)
    }
}

/// Records the three signals `EstimateTelemetry` needs, and nothing else.
///
/// Scoped to the root frame deliberately. rskj keeps a counter per frame and
/// takes the maximum across them when a child's result is merged into its
/// parent's, but a child's peak is bounded by the allowance its parent
/// committed, and that commitment is already in the parent's own peak. The
/// root's peak therefore dominates every descendant's, and tracking one frame
/// avoids carrying a stack of counters that can only ever agree with it.
/// Frame depth of the transaction's own call. revm announces the root frame
/// through the `call` hook exactly as it announces a nested one, so the root
/// is depth 1 and a child of the root is depth 2.
const ROOT_DEPTH: usize = 1;

#[derive(Debug, Default)]
pub struct EstimationInspector {
    /// Frames entered so far: 0 before the root is announced, 1 while the
    /// root runs, 2+ inside a child.
    depth: usize,
    /// The root frame's gas limit, learned from the first step.
    root_limit: u64,
    /// The root frame's remaining gas as of its last completed step.
    root_remaining: u64,
    /// Whether the root frame has been stepped at all. A transaction that is
    /// a plain value transfer never enters the interpreter.
    stepped: bool,
    max_committed: u64,
    moved_all_to_child: bool,
    call_with_value: bool,
}

impl EstimationInspector {
    pub fn new() -> Self {
        Self::default()
    }

    /// Fold the inspector's observations together with the figures only the
    /// finished execution knows.
    pub fn into_telemetry(self, gas_used: u64, refunded: u64) -> EstimateTelemetry {
        EstimateTelemetry {
            gas_used,
            refunded,
            // A transaction that never ran bytecode has no peak to report;
            // its cost is entirely intrinsic, which `gas_used` already holds.
            max_committed: if self.stepped { self.max_committed } else { 0 },
            moved_all_to_child: self.moved_all_to_child,
            call_with_value: self.call_with_value,
        }
    }

    /// Shared by the call and create hooks: a frame is being entered.
    fn enter_frame(&mut self, value_is_nonzero: bool) {
        // Only a CALL *opcode* counts, which is what rskj's `VM.doCALL`
        // flags. The transaction's own value is already paid for by the
        // intrinsic cost and grants no stipend, so the root frame's entry
        // must not set this.
        if value_is_nonzero && self.depth >= ROOT_DEPTH {
            self.call_with_value = true;
        }
        if self.depth == ROOT_DEPTH && self.stepped && self.root_remaining == 0 {
            // The opcode has already charged its own cost and the callee's
            // allowance, and left the root with nothing. That is rskj's
            // `calleeGas == remainingGas`.
            self.moved_all_to_child = true;
        }
        self.depth += 1;
    }

    fn leave_frame(&mut self) {
        self.depth = self.depth.saturating_sub(1);
    }

    fn observe_root(&mut self, limit: u64, remaining: u64) {
        if self.depth != ROOT_DEPTH {
            return;
        }
        self.stepped = true;
        if self.root_limit == 0 {
            self.root_limit = limit;
        }
        let committed = self.root_limit.saturating_sub(remaining);
        if committed > self.max_committed {
            self.max_committed = committed;
        }
        self.root_remaining = remaining;
    }
}

impl<CTX, INTR> Inspector<CTX, INTR> for EstimationInspector
where
    CTX: ContextTr,
    INTR: InterpreterTypes,
{
    fn step(&mut self, interp: &mut Interpreter<INTR>, _context: &mut CTX) {
        // Before the opcode runs, so the limit is known even for a frame
        // whose first instruction reverts.
        if self.depth == ROOT_DEPTH && self.root_limit == 0 {
            self.root_limit = interp.gas.limit();
            self.stepped = true;
        }
    }

    fn step_end(&mut self, interp: &mut Interpreter<INTR>, _context: &mut CTX) {
        // After the opcode, which is the point that matters: a CALL has by
        // now charged both its own cost and the whole allowance it is about
        // to hand over, so this is where the commitment peaks.
        self.observe_root(interp.gas.limit(), interp.gas.remaining());
    }

    fn call(&mut self, _context: &mut CTX, inputs: &mut CallInputs) -> Option<CallOutcome> {
        self.enter_frame(!inputs.value.get().is_zero());
        None
    }

    fn call_end(&mut self, _context: &mut CTX, _inputs: &CallInputs, _outcome: &mut CallOutcome) {
        self.leave_frame();
    }

    fn create(&mut self, _context: &mut CTX, inputs: &mut CreateInputs) -> Option<CreateOutcome> {
        self.enter_frame(!inputs.value().is_zero());
        None
    }

    fn create_end(
        &mut self,
        _context: &mut CTX,
        _inputs: &CreateInputs,
        _outcome: &mut CreateOutcome,
    ) {
        self.leave_frame();
    }
}

/// Unused, but it keeps the import honest: the inspector is generic over the
/// context and needs `Address` in scope for the trait bounds revm expects.
const _: Option<Address> = None;

#[cfg(test)]
mod tests {
    use super::*;

    /// A plain transfer never enters the interpreter, so there is no peak to
    /// report; the floor on `gas_used` is the whole answer.
    #[test]
    fn a_plain_transfer_is_carried_by_the_floor() {
        let t = EstimateTelemetry { gas_used: 21_000, ..Default::default() };
        assert_eq!(t.estimate(10_000_000), 21_000);
    }

    /// The floor is applied before the stipend, not after. Flooring
    /// afterwards would swallow the 2,300 and return an estimate too small to
    /// execute.
    #[test]
    fn the_floor_does_not_swallow_the_stipend() {
        let t = EstimateTelemetry {
            gas_used: 30_721,
            max_committed: 28_000, // below the spend
            call_with_value: true,
            ..Default::default()
        };
        assert_eq!(t.estimate(10_000_000), 30_721 + STIPEND_CALL);
    }

    /// With no call in the picture the peak is the whole story, and the
    /// refund is credited back rather than held -- so it is not added.
    #[test]
    fn a_refund_alone_does_not_raise_the_estimate() {
        let t = EstimateTelemetry {
            gas_used: 30_000,
            refunded: 4_800,
            max_committed: 30_000,
            ..Default::default()
        };
        assert_eq!(t.estimate(10_000_000), 30_000);
    }

    /// The case binary search exists for: a callee handed far more than it
    /// spends. The caller had to hold the whole allowance.
    #[test]
    fn the_peak_wins_over_the_total() {
        let t = EstimateTelemetry {
            gas_used: 26_000,      // what was actually consumed
            max_committed: 121_000, // what had to be available at the CALL
            ..Default::default()
        };
        assert_eq!(t.estimate(10_000_000), 121_000);
    }

    /// When everything was handed over, the peak is just the cap we ran at
    /// and says nothing; the total is the honest answer.
    #[test]
    fn handing_over_everything_falls_back_to_the_total() {
        let t = EstimateTelemetry {
            gas_used: 26_000,
            refunded: 1_000,
            max_committed: 9_999_999, // an artefact of the cap
            moved_all_to_child: true,
            ..Default::default()
        };
        assert_eq!(t.estimate(10_000_000), 27_000);
    }

    #[test]
    fn a_value_bearing_call_carries_the_stipend() {
        let t = EstimateTelemetry {
            gas_used: 30_000,
            max_committed: 30_000,
            call_with_value: true,
            ..Default::default()
        };
        assert_eq!(t.estimate(10_000_000), 30_000 + STIPEND_CALL);
    }

    /// rskj clamps, and so does this: an estimate above what the caller can
    /// supply is not an estimate.
    #[test]
    fn the_estimate_is_clamped_to_the_cap() {
        let t = EstimateTelemetry { gas_used: 50_000, max_committed: 90_000, ..Default::default() };
        assert_eq!(t.estimate(60_000), 60_000);
    }
}
