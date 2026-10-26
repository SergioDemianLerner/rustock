# Single-pass gas estimation, and where it is looser than it needs to be

`eth_estimateGas` is asked for the smallest gas limit at which a transaction
succeeds. The obvious way to answer is to binary search: execute at the cap,
then halve the interval until it closes — about two dozen executions. rskj
does not do that. It executes once and derives the answer from three things
recorded during the run, in `EthModule.internalEstimateGas`:

```java
long estimatedGas = reversibleExecutionResult.getMovedRemainingGasToChild() ?
        reversibleExecutionResult.getGasUsed() + reversibleExecutionResult.getDeductedRefund() :
        reversibleExecutionResult.getMaxGasUsed();

if (reversibleExecutionResult.isCallWithValuePerformed()) {
    estimatedGas += GasCost.STIPEND_CALL;
}
```

Rustock now implements the same algorithm (`crates/execution/src/gas_estimator.rs`),
with the previous binary search retained behind
`--gas-estimator-binary-search` so the two can be compared on the same node.

Comparing them turned up **two** places where the single-pass answer differs
from the binary search's. They are not the same kind of thing, and conflating
them is easy, so this note separates them.

- §1 is **not a defect.** The single pass answers a stricter question and its
  answer is exactly minimal for that question.
- §2 **is** an over-estimate, by up to 50%, with no benefit that we can
  identify. It is inherited from rskj and is a one-line fix on both sides.

---

## 1. The peak: a larger answer, and the right one

### What happens

A contract calls out with a fixed, generous allowance:

```
CALL(gas = 100000, callee, value = 0, ...)
```

and the callee spends almost nothing.

Measured on a regtest fixture (`setup_estimation_state` in
`crates/rpc/src/tests.rs`):

| | |
|---|---|
| total consumed | ~30,700 |
| single-pass estimate | **112,021** |
| binary search | ~30,700 |

### Why the single pass is right

RSK never adopted EIP-150's 63/64 retention. `rsk_instructions` overrides the
CALL family to rskj's rule:

```
calleeGas = min(remainingGas, requestedGas + STIPEND_CALL)
```

So a CALL asking for 100,000 when only 11,000 remains **does not fail** — the
callee silently receives 11,000. The transaction therefore still succeeds at a
limit far below the peak, and that is the limit binary search converges on.

But the inner call then runs on different gas than it did when the estimate
was taken. A caller that does not check the CALL's return value carries on and
does something else. Binary search has found a limit at which the transaction
*does not revert*; it has not found a limit at which the transaction *does
what it did*.

The single pass reports `maxGasUsed`, the high-water mark of the caller's
commitment. That is precisely `gasBeforeCall + 100000` — the smallest limit at
which the callee still receives the full allowance it asked for. **It is not a
conservative margin; it is the exact minimum for the stronger property.**

This is the behaviour worth keeping, and `the_estimate_covers_the_allowance_a_call_asks_for`
pins it, asserting both that the single pass covers the allowance and that
binary search does not.

---

## 2. The refund: a 50% over-estimate, on both clients

### What happens

Take the other branch — `movedRemainingGasToChild` — and give the transaction
a storage refund. A contract that clears a slot and then calls out with `GAS`,
handing over everything it has left:

```
SSTORE(slot1, 0)                      // slot1 was non-zero: earns a refund
CALL(gas = GAS, callee, 0, ...)       // hands over all remaining gas
STOP
```

Measured on a regtest fixture (`setup_refund_state`, same file). The telemetry
from the single execution:

```
gas_used = 26726      (pre-refund)
refunded = 13363      (= gas_used / 2, the cap binding)
moved_all_to_child = true
```

| | |
|---|---|
| single-pass estimate | **40,089**  (26,726 + 13,363) |
| binary search | **26,726** |

26,726 is not a guess: binary search reaches it by executing there and
observing success. The single-pass answer is **50% higher than necessary**,
which is the worst case, because the refund is capped at `gasUsed / 2`.

### Why this one is wrong

The two branches of the formula treat the refund inconsistently:

| branch | expression | refund included? |
|---|---|---|
| not moved | `maxGasUsed` | no |
| moved | `gasUsed + deductedRefund` | **yes** |

Both `maxGasUsed` and `gasUsed` are *pre-refund* figures. That is worth
establishing carefully, because the field names invite the opposite reading:

- `ProgramResult.refundGas(long)` **does** subtract from `gasUsed` — but it is
  called only from `Program.refundGas`, which is unused gas coming back from a
  **child call**. Not the end-of-transaction refund.
- The end-of-transaction refund is applied in
  `TransactionExecutor.refundGas()`:

  ```java
  long gasRefund = Math.min(result.getFutureRefund(), result.getGasUsed() / 2);
  result.addDeductedRefund(gasRefund);
  result.setGasUsedBeforeRefunds(result.getGasUsed());
  gasLeftover = GasCost.add(gasLeftover, gasRefund);
  ```

  It credits `gasLeftover` and records the amount. **It never touches
  `result.gasUsed`.**

So `getGasUsed()` is consumption before any refund, and the refund is credited
at the very end. A caller must be able to cover the consumption; it never has
to cover the refund, because the refund is money coming back, not going out.

Adding `deductedRefund` therefore inflates the estimate by an amount the
transaction demonstrably does not need — as the measurement above shows, by
executing successfully at the smaller figure.

The asymmetry is the tell. If the refund genuinely had to be held, the
`maxGasUsed` branch would need it too, and it does not have it.

### What we considered and rejected

- **"It compensates for the refund cap moving when the limit changes."** It
  does not move. The cap is `min(futureRefund, gasUsed / 2)`, and `gasUsed` is
  a property of the execution, not of the limit.
- **"It is a safety margin for the behaviour change in this branch."** The
  `movedRemainingGasToChild` branch has already given up on preserving
  behaviour — that is why it abandons the peak. But a margin of "half the gas
  used, but only when a refund happens to be present, and zero otherwise" is
  not a margin anyone would design. It is proportional to the wrong thing.

### The fix

Drop the term:

```java
long estimatedGas = reversibleExecutionResult.getMovedRemainingGasToChild() ?
        reversibleExecutionResult.getGasUsed() :          // was: + getDeductedRefund()
        reversibleExecutionResult.getMaxGasUsed();
```

In Rustock the equivalent is one line in `EstimateTelemetry::estimate`
(`crates/execution/src/gas_estimator.rs`).

### Status

Rustock currently **reproduces rskj's behaviour**, including this
over-estimate, and pins it with `the_refund_branch_overshoots_by_the_refund`
so that it cannot change silently. The deliberate choice is to agree with rskj
rather than to be quietly better than it; if the rskj side drops the term, the
Rustock side and that test should be updated together.

---

## Reproducing

Both fixtures are regtest-only and live in `crates/rpc/src/tests.rs`:

```
cargo test -p rustock-rpc -- the_estimate_covers_the_allowance_a_call_asks_for
cargo test -p rustock-rpc -- the_refund_branch_overshoots_by_the_refund
cargo test -p rustock-rpc -- the_exactimator_never_undercuts_binary_search
```

To compare the two estimators on a running node, set
`[rpc] gas_estimator_binary_search = true` (or pass
`--gas-estimator-binary-search`) and re-issue the same `eth_estimateGas`.
