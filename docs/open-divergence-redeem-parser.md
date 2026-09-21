# Open divergence: which redeem scripts count as multisig

**Status: open. Not fixed. Needs a decision, because the fix is a faithful port
of consensus-critical Java and a partial one would be worse than the gap.**

Found 2026-09-21 while closing the OP_PUSHDATA4 divergence (PR #39). Same
function, same call path, different cause.

## The gap in one line

rustock asks *"is this script shaped like a multisig?"*. rskj asks *"can a
redeem-script parser be built for this, and does it report M > 0?"*. Those are
not the same question, and rskj answers yes to three shapes rustock answers no
to.

## What rskj actually does

`co.rsk.bitcoinj:bitcoinj-thin:0.14.4-rsk-18` — the artifact
`rskj-core/build.gradle:355` pins. `Script.isSentToMultiSig`:

```java
public boolean isSentToMultiSig() {
    try {
        return this.getRedeemScriptParser().getM() > 0;
    } catch (ScriptException e) {
        return false;
    }
}
```

`RedeemScriptParserFactory.get` returns one of five parsers:

| parser | selected when | `getM()` |
|---|---|---|
| `NonStandardErpRedeemScriptParserHardcoded` | chunks equal one hardcoded testnet script | **−1**, deliberately |
| `FlyoverRedeemScriptParser` | 32-byte push + `OP_DROP` prefix | inner parser's, **via a recursive factory call** |
| `StandardRedeemScriptParser` | `OP_M` … `OP_N` `OP_CHECKMULTISIG[VERIFY]` | `decodePositiveN(chunk 0)` |
| `P2shErpRedeemScriptParser` | `OP_NOTIF` … `OP_ELSE` push `OP_CSV` `OP_DROP` … `OP_ENDIF` | inner standard parser's |
| `NonStandardErpRedeemScriptParser` | similar, `OP_ENDIF` in the penultimate position | inner standard parser's |

Everything else throws, which the `catch` turns into `false`.

So **four of the five shapes answer true.** The only script that answers false
by construction is the hardcoded testnet federation, whose parser returns −1 on
purpose to preserve testnet consensus.

## Three distinct differences, each verified by test

`crates/execution/src/bridge/peg.rs`, tests
`is_sent_to_multisig_rejects_erp_and_flyover_redeems_unlike_rskj` and
`is_sent_to_multisig_requires_op_n_not_a_pushed_number`. They pin rustock's
**current** answers and say so; they are not assertions that the answers are
right.

1. **P2SH-ERP redeems.** rustock requires the last chunk to be
   `OP_CHECKMULTISIG[VERIFY]`. An ERP redeem ends in `OP_ENDIF`, so rustock
   stops there. rskj matches the ERP structure and delegates inward.

2. **Flyover redeems.** A 32-byte push and `OP_DROP` wrap an inner redeem.
   rustock counts those two chunks against the `OP_N` key count and fails the
   `chunks.len() == 3 + num_keys` test. rskj strips them and calls the factory
   **recursively**, so a flyover-wrapped ERP redeem also resolves.

3. **A pushed M or N.** bitcoinj's `decodePositiveN` accepts a pushed number as
   well as `OP_1..OP_16`; rustock's `decode_op_n` accepts only the opcodes.

## Why it matters

`is_sent_to_multisig` gates two branches of `classify_pegin_sender`: P2SH-multisig
and P2SH-P2WSH. That function decides whether a peg-in has an identifiable
sender, and therefore where a rejected peg-in is refunded.

The input is attacker-controlled. Anyone can build a P2SH address whose redeem
script is ERP-shaped or flyover-wrapped and peg in from it. rskj would classify
the sender; rustock returns `None` and — per `BtcLockSenderProvider` semantics —
takes the path that does not mark the transaction as processed. Two nodes, two
outcomes, same block.

Nothing on mainnet has triggered it: the chain replays clean to #9,230,008. This
is latent, and reachable on purpose rather than by accident.

## Why it is not fixed here

A faithful fix is roughly 200 lines porting five interdependent Java classes
whose control flow runs on exceptions, into the middle of peg-in classification.
At least three subtleties are already known — recursive flyover resolution, the
inner-redeem extraction each ERP parser performs, and `decodePositiveN`'s
push-or-opcode acceptance — and each is a place where a mis-port would make
rustock classify something rskj does not.

That direction is worse than the current gap. Today rustock **fails closed**: it
declines to classify a sender rskj would classify. A wrong port fails **open**,
inventing senders and refund addresses rskj never derives.

So this wants deliberate work with its own verification, not an opportunistic
patch alongside an unrelated fix.

## What a fix would need

1. Port `isRedeemLikeScript`, `hasStandardRedeemScriptStructure`,
   `hasFlyoverRedeemScriptStructure`, `hasP2shErpRedeemScriptStructure`,
   `hasNonStandardErpRedeemScriptStructure` and the inner-redeem extractors,
   from the pinned tag rather than from classic bitcoinj — it is a fork.
2. Reproduce `decodePositiveN`, including its push form and its throwing
   behaviour, since the `catch` is load-bearing control flow.
3. Carry the hardcoded testnet script across as a constant, and keep its −1.
4. Replay peg-in-dense ranges. #7,300,000–#7,400,000 holds 4,445
   `rejected_pegin` events and is the natural target; the whole chain, across
   the 97 segbuild chunks in parallel, is the complete answer.

## Not investigated

Whether rustock's other uses of redeem scripts — `is_erp_redeem`,
`redeem_script_threshold`, `spending_redeem_keys` — carry the same assumption.
They parse federation redeems, which rustock constructs itself, so the input is
not attacker-controlled there. That makes them lower priority, not proven
correct.
