# Response to the rustock consensus audit (archive `rustock-4796682`)

**Date:** 2026-09-21
**Scope:** the five findings listed under *High-severity findings* in the
audit's `README.md`.
**Branch:** `fix/audit-consensus-findings`, off `main` at `27e1ef5`.

Rustock's acceptance criterion is **agreement with deployed rskj mainnet**, not
agreement with the RSKIP text. Each finding below was checked against the rskj
source rather than against the RSKIP, and that distinction changes the verdict
on exactly one of them.

## Summary

| FRCR | Area | Verdict | Fix |
|---|---|---|---|
| FRCR-679 / -879 | BASEFEE (0x48) missing, arrowhead600→lovell700 | **Confirmed** | `3b57e9c` |
| FRCR-280 | secp256k1 ECADD/ECMUL charge 0 gas on a failed call | **Confirmed** | `77d3682` |
| FRCR-476 | `receiveHeader` missing the −20 size-mismatch code | **Confirmed** | `088b53d` |
| FRCR-75 / -76 | `getBtcTransactionConfirmations` codes, ordering, missing checks | **Confirmed** | `9f623df` |
| FRCR-77 | `receiveHeaders` fixed-cost gating "inverted" vs RSKIP-132 | **Not a defect** | none — see below |

Four of five are real, and all four are reachable on demand by an ordinary
transaction: none requires a privileged sender, a reorg, or an unusual chain
state. None of them has occurred on mainnet, which is why replay did not catch
them; see *Why our tests missed these*.

---

## FRCR-679 / -879 — BASEFEE not installed (confirmed)

**Ground truth.** rskj gates BASEFEE on `ConsensusRule.RSKIP412`:

```java
// org/ethereum/vm/VM.java:1949
case OpCodes.OP_BASEFEE:
    if (!activations.isActive(RSKIP412)) {
        throw Program.ExceptionHelper.invalidOpCode(program);
    }
    doBASEFEE();
```

and `rskj-core/src/main/resources/reference.conf:84` sets
`rskip412 = arrowhead600`. `VM.doBASEFEE` pushes
`program.getMinimumGasPrice()` for 2 gas (BASE_TIER).

**What rustock did.** rustock runs revm, which bundles BASEFEE into the LONDON
spec. `hardfork.rs` maps arrowhead600 and arrowhead631 to `SpecId::ISTANBUL`
— chosen deliberately, for the RSKIP400 calldata pricing — and only reaches
`SpecId::SHANGHAI` at lovell700. So for the ~1.1M mainnet blocks between
#6,223,700 and #7,338,024, revm's BASEFEE halted `NotActivated` and consumed
the frame's whole gas where rskj pushed a value and continued.

**Fix.** The same shape as the already-present PUSH0 (RSKIP398) workaround: an
unchecked instruction installed on the *RSK* activation height rather than on
the revm spec. It is installed for lovell700+ as well — where revm's own
BASEFEE was already correct — so the opcode no longer behaves differently on
either side of a spec boundary that has nothing to do with RSKIP412.

The value pushed is `block.basefee`, which rustock loads from
`header.minimum_gas_price` (`env.rs:37`), matching `doBASEFEE` exactly.

**Note on severity.** This is the most serious of the four, because the window
is historical: a rustock node replaying mainnet would already diverge on any
block in that range containing a BASEFEE. The other three require someone to
send the triggering input.

---

## FRCR-280 — secp256k1 precompiles charge 0 gas on a failed call (confirmed)

**Ground truth.** RSKIP197 (iris300) makes a non-OOG precompile failure a
*handled* event: the CALL pushes 0, state rolls back, and the caller is charged
exactly `getGasForData(input)` —
`Program.executePrecompiledAndHandleError` runs
`refundGas(msg.getGas() - requiredGas)` in a `finally` block.

The RSKIP516 secp256k1 contracts return a constant from `getGasForData`
*without inspecting the input*:

```java
// co/rsk/pcc/secp256k1/Secp256k1Addition.java:27
private static final long EC_256_ADDITION_GAS_COST = 150;
// co/rsk/pcc/secp256k1/Secp256k1Multiplication.java:27
private static final long EC_256_MULTIPLICATION_GAS_COST = 3000;
```

So an off-curve point or an out-of-range coordinate still costs 150 / 3000.

**What rustock did.** `rskip197_required_gas_on_error` is rustock's table of
those costs and listed only BN128 add/mul/pairing and BLAKE2F. The call site
reads it as `…​.unwrap_or(0)`, so a missing entry is not an error — it is a
**free** failed call. Any transaction calling either secp256k1 precompile with
a bad point kept more gas than on rskj, from reed800 onwards.

**Fix.** Added the two entries. The docstring now states the invariant the
table has to hold and that `None` means "provably never reaches this path", so
an omission reads as the silent undercharge it is.

---

## FRCR-476 — `receiveHeader` size mismatch (confirmed)

**Ground truth.**

```java
// co/rsk/peg/Bridge.java:568-586
if (!BtcTransactionFormatUtils.isBlockHeaderSize(headerArg.length, activations)) {
    logger.warn("Unexpected BTC header received (size mismatch). Aborting processing.");
    return RECEIVE_HEADER_ERROR_SIZE_MISTMATCH;   // = -20, Bridge.java:243
}
```

```java
// co/rsk/peg/utils/BtcTransactionFormatUtils.java
public static boolean isBlockHeaderSize(int size, ActivationConfig.ForBlock activations) {
    return (activations.isActive(ConsensusRule.RSKIP124) && size == MIN_BLOCK_HEADER_SIZE) ||
        (!activations.isActive(ConsensusRule.RSKIP124) && size >= MIN_BLOCK_HEADER_SIZE
            && size <= MAX_BLOCK_HEADER_SIZE);
}
```

Two properties matter, and rustock had neither.

1. **The rule is exact.** With RSKIP124 active, only 80 bytes is accepted.
   rustock accepted anything ≥ 80 and truncated to the first 80, so an 81-byte
   submission that rskj rejects outright was *processed as a valid header* —
   and, if its PoW held, written into the Bridge's BTC chain. That is a state
   divergence, not merely a return-value one.
2. **−20 is a return value, not an exception.** The call succeeds and the
   caller reads −20. rustock raised a precompile error for a short header,
   which post-RSKIP197 reverts and pushes 0, and pre-RSKIP197 fails the call
   consuming all forwarded gas. Either way a relayer contract branching on the
   return value takes a different branch.

`receiveHeader` is transaction-callable and unrestricted, so both are reachable
by anyone.

**Fix.** Added `is_block_header_size` mirroring the Java (including the
pre-RSKIP124 80..=85 window) and the `ERR_SIZE_MISMATCH = -20` return.

---

## FRCR-75 / -76 — `getBtcTransactionConfirmations` (confirmed)

**Ground truth.** `BridgeSupport.java:90-94` declares the codes and
`BridgeSupport.java:2194-2235` fixes their order:

```text
  block == null                          -> -1  INEXISTENT_BLOCK_HASH
  max(0, bestHeight - height) > 4320     -> -4  BLOCK_TOO_OLD
  getStoredBlockAtMainChainHeight(h)
        != block, or null                -> -2  BLOCK_NOT_IN_BEST_CHAIN
        threw BlockStoreException        -> -3  INCONSISTENT_BLOCK
  merkle branch does not reduce          -> -5  INVALID_MERKLE_BRANCH
  otherwise      bestChainHeight - block.getHeight() + 1
```

**What rustock did.** It returned −1 for an unknown block, then jumped
straight to the merkle check and reported **−2** for a bad branch. There was
no depth check (the constant existed, spelled `_CONFIRMATION_ERR_BLOCK_TOO_OLD`
with a leading underscore to silence the dead-code warning, and with the wrong
value −3), and no best-chain check at all.

The missing best-chain check is the substantive one. A BTC block on an orphaned
fork is still in Bridge storage — `connect_block` persists every valid header
and only moves the head on strictly more work. rustock would therefore report a
**positive confirmation count** for a transaction that exists only on a fork,
where rskj returns −2. Everything downstream of this method trusts that number.

rustock also carried a code with no rskj counterpart, `−4 BTC_NOT_READY`,
returned when the BTC chain head was absent. rskj has no such case because
`ensureBtcBlockChain()` guarantees a head.

**Fix.** The method now seeds the chain (rskj `ensureBtcBlockChain`), then runs
the four checks in rskj's order with rskj's codes, using the pre-existing
main-chain height index. `stored_block_at_main_chain_height` was refactored to
preserve Java's three-way outcome (`StoredBlock` / `null` /
`BlockStoreException`) as a `MainChainLookup` enum, because −2 and −3 depend on
which one occurred; the RSKIP199 search-depth limit from
`RepositoryBtcBlockStoreWithCache:213-233` is now modelled too. The 4320 bound
is read from `BridgeConstants` — the same field the gas estimator uses — so the
price and the answer cannot drift apart.

---

## FRCR-77 — `receiveHeaders` cost gating (**not a defect**)

The finding reports that rustock's RSKIP-132 fixed-cost branches are inverted
relative to the spec: it charges 25,000 when RSKIP132 is active and 66,000
when it is not, where the RSKIP text says the opposite.

The rustock code is right, and so is the audit's reading of the RSKIP — the two
simply disagree, and mainnet follows the code:

```java
// co/rsk/peg/Bridge.java, receiveHeadersGetCost
final long BASE_COST = activations.isActive(ConsensusRule.RSKIP132) ? 25_000L : 66_000L;
```

rustock contains the identical ternary. Changing rustock to match the RSKIP
would *introduce* a fork. We are treating the RSKIP-132 text as stale and have
left the code alone. If the auditor's brief is RSKIP conformance rather than
mainnet conformance, this row should be re-filed against the RSKIP.

This is the only row in the high-severity table where the RSKIP and rskj
disagree, so the distinction cost one finding out of five — worth noting for
the rest of the report, which we have not yet worked through.

---

## Tests added

Every fix ships with a test that fails on the parent commit.

| Test | File | What it pins |
|---|---|---|
| `rskip197_required_gas_covers_secp256k1` | `precompiles.rs` | 150 / 3000 for the two RSKIP516 addresses, independent of input |
| `secp256k1_off_curve_point_is_a_non_oog_error` | `precompiles.rs` | that the path is reachable at all — an off-curve point yields a non-OOG `Err`, not OOG |
| `rskip197_required_gas_bn128_and_blake2f` | `precompiles.rs` | the rest of the table, so it is read as a whole |
| `test_basefee_activates_at_arrowhead600` | `executor.rs` | end to end: BASEFEE invalid at #6,223,699; at #6,223,700, #6,549,300 and #7,338,024 it returns the block's `minimumGasPrice` |
| `receive_header_size_rule_matches_rskj` | `bridge/btc_chain.rs` | `isBlockHeaderSize` on both sides of RSKIP124, and `ERR_SIZE_MISMATCH == -20` |
| `test_receive_header_size_mismatch_returns_minus_20` | `executor.rs` | end to end: 0/79/81/85 bytes → −20; 80 bytes still reaches `BridgeSupport` (−3) |
| `btc_transaction_confirmation_codes_match_rskj` | `bridge/tx.rs` | the five codes as a block, transcribed from `BridgeSupport.java:90-94`, and MAX_DEPTH = 4320 on all three networks |
| `test_btc_transaction_confirmations_codes_and_best_chain_check` | `executor.rs` | end to end on regtest: builds a 3-block chain plus a sibling forking at height 1, then checks −1, −2 (fork block), −5 (bad branch), the positive count, **and** that the best-chain check precedes the merkle check |

Four of the eight drive the real Bridge precompile through
`RskExecutor::execute_block` and assert on the ABI-encoded `int256` actually
returned, rather than on an internal function's result. That was deliberate:
three of the four defects lived in the gap between "the logic is right" and
"the caller sees what rskj's caller sees".

One further defect was found while running these, and fixed in the same batch:
`add_signature_rejects_key_not_in_redeem_script` in `bridge/peg.rs` had been
separated from its `#[test]` attribute by a later insertion, so it had
**never run**. The compiler said so, twice, as a warning.

---

# Why our tests missed these, and what would have caught them

The uncomfortable answer first: rustock's test suite is large (662 tests
in the execution crate alone, 1,253 across the workspace) and its strongest check — replaying 9.2M mainnet
blocks and comparing state roots — is genuinely strong. Neither could have
found any of these four. The reasons are structural, not a matter of writing
more of the same tests.

## 1. All four defects are *absent* code, and coverage cannot measure absence

BASEFEE was not installed. The best-chain check was not written. The depth
check was not written. Two rows were missing from a lookup table. Line and
branch coverage are defined over code that exists; a missing branch has no
line to leave uncovered. Our coverage numbers were, and would have remained,
unaffected by all four.

**What to do instead: measure coverage over the specification surface, not the
source.** Enumerate the things rskj *has* and assert rustock has a test for
each:

- every `ConsensusRule` in rskj's `ConsensusRule.java` × its activation in
  `reference.conf` → a rustock activation-height test;
- every opcode rskj's `VM.java` gates on a consensus rule → an
  opcode-activation test at the boundary block, both sides;
- every Bridge method in `Bridge.java`'s method table → a test per declared
  return code;
- every precompile → a test for its error path as well as its happy path.

These lists are machine-readable out of the rskj tree. A generated test that
fails with *"RSKIP412 is gated in VM.java and has no rustock activation test"*
would have found the BASEFEE gap the day it was introduced. This is, in effect,
what the audit did by hand — which is why it found them and we did not.

## 2. The tests were written from the same reading as the code

Every rustock feature was ported by reading rskj, then writing a test asserting
what the port does. A test authored alongside the implementation inherits the
implementation's misreadings. It is a check on *transcription*, not on
*understanding*.

The BASEFEE case is the sharpest. `hardfork.rs` already had
`assert_eq!(cfg.spec_id(6_223_700), SpecId::ISTANBUL)` and
`assert!(cfg.has_push0(6_223_700))`. Both passed. Both were right. There was
simply nothing anywhere in the repository that said "RSKIP412 is *also*
arrowhead600", because the only list of what arrowhead600 contains was the code
itself.

**What to do: make the ground truth an artifact, not a memory.** rskj's
`reference.conf` is 300 lines and fully mechanical. Vendoring it and asserting
rustock's `RskNetworkUpgrade` mapping against it — every rule, not the ones we
happened to implement — turns "did we notice RSKIP412?" from a question about
attention into a test failure.

## 3. Constants were tested where they are *used*, not where they are *declared*

`receive_header_result_codes_match_rskj` already existed and pinned five of the
six `receiveHeader` codes against rskj. It missed −20 for a precise reason:
−20 is declared in `Bridge.java`, and the test was written from
`BridgeSupport.java`. The test transcribed one Java constant block faithfully
and did not know the other existed.

`getBtcTransactionConfirmations` had no such test at all, and its constants had
never been compared against `BridgeSupport.java:90-94` as a block — which is
how the −2/−5 swap survived.

**What to do: transcribe declaration blocks whole.** One rustock test per rskj
constant block, copying *every* constant in it, including the ones rustock does
not use yet — an unused one is exactly the signal that a code path is missing.
The two new tests do this.

## 4. `_unused` and warnings converted "not implemented" into "fine"

`_CONFIRMATION_ERR_BLOCK_TOO_OLD` carried a leading underscore to silence the
dead-code warning. That underscore is the whole bug in miniature: it took
"we have not implemented the depth check" and made it compile quietly, with the
wrong value, for as long as anyone cared to leave it.

The same pattern produced a second defect found during this work: a `#[test]`
attribute in `bridge/peg.rs` had been separated from its function by a later
insertion, so `add_signature_rejects_key_not_in_redeem_script` never ran. The
compiler emitted `duplicate_macro_attributes` and `dead_code` on every build.
Nobody read them.

**What to do:**

- `#![deny(dead_code)]` on the consensus crates, with any genuine exception
  carrying an explicit `#[allow]` and a `TODO(rskj): …` naming what is missing;
- `-D warnings` in CI, so `duplicate_macro_attributes` fails the build;
- a CI check that the number of `#[test]` attributes equals the number of tests
  the harness actually ran.

The last is three lines of shell and would have caught the dead test
immediately.

## 5. A lookup table with a plausible-looking default

FRCR-280 is a class, not an incident. `rskip197_required_gas_on_error` maps an
address to a gas cost, and its call site reads `…​.unwrap_or(0)`. A missing row
therefore does not fail, does not warn, and does not look wrong — it produces
`0`, which is a perfectly ordinary gas figure.

**What to do: make the default loud.** Where a table must be exhaustive over a
known set, match exhaustively over a closed enum of the RSK precompile
addresses instead of over `Address` with a `_ =>` arm; adding a new precompile
then does not compile until its gas-on-error is decided. Where that is
impractical, the default should be `debug_assert!`-guarded or should return a
sentinel the caller must handle, never a silently valid value.

## 6. Replay proves agreement on the traffic that happened, not on the traffic an attacker would send

This is the most important one, because replay is our best tool and it is
structurally blind here.

All four defects are reachable on demand: an off-curve point passed to ECADD,
an 81-byte header, a confirmation query naming a fork block, a contract using
BASEFEE. None of them has happened on mainnet in a way that reached the defect,
so 9.2M blocks of replay agreed perfectly. Replay validates the *observed*
input distribution. Consensus code must be correct on the *adversarial* one,
and those are different distributions — the whole point of an attack is that
it is not in the history.

**What to do, in rough order of value per unit of effort:**

1. **Port rskj's own error-path tests.** rskj already has tests for every one
   of these codes — `BridgeSupportTest` and `BridgeTest` between them cover
   `getBtcTransactionConfirmations` returning −1 through −5 and
   `receiveHeader` returning −20. This is the cheapest and highest-yield item
   on the list, and it is already the stated plan in
   `docs/test-coverage-vs-rskj.md`; the audit shows the plan was being worked
   happy-path first. **Error paths should be ported before happy paths**, since
   the happy paths are the ones replay already covers.
2. **Differential fuzzing against rskj as an oracle.** `rskj_sender_compat.rs`
   already demonstrates the shape: record rskj's answers for a set of inputs,
   then assert rustock reproduces them. Generalizing it — drive rskj in a
   harness, generate random Bridge calldata and precompile inputs, compare
   return value and gas — attacks precisely the input distribution replay
   cannot reach. Bridge methods with integer return codes are the ideal first
   target: the oracle is one `int`, and the input space is small enough to fuzz
   meaningfully.
3. **Mutation testing on the consensus crates.** Flipping `-5` to `-2`,
   deleting the depth check, or removing a table row should each break a test.
   Where it does not, the missing test is named for you. `cargo-mutants`
   over `crates/execution/src/bridge` would have reported the
   `getBtcTransactionConfirmations` gap without anyone reading rskj.
4. **Event-targeted replay.** Instead of replaying block ranges, index mainnet
   for the blocks that exercised each Bridge method, each precompile and each
   rarely-used opcode, and require the replay set to include at least one of
   each. Our current ranges are chosen by height, which weights common
   transactions and under-weights exactly the rare paths where the bugs are.
   (This would not have caught these four — they never occurred — but it fixes
   the neighbouring blind spot, where a path *did* occur, just not in the range
   we replayed.)

## What this batch changes in practice

Items 1, 3, 4 and 5 above are cheap and local, and the fixes here include the
first instalments: constants transcribed as blocks, the orphaned test
reattached, and the gas table documented with its invariant. Items on the
specification-surface and differential-fuzzing side are larger and are the
right next piece of work — they are what turns "we read rskj carefully" into
something a build can check.

The honest summary for the auditor: rustock's verification was strong on
*fidelity of what was implemented* and had almost nothing pointed at
*completeness of what was implemented*. All five high-severity findings — the
four real ones and the one we are contesting — sit in that gap.
