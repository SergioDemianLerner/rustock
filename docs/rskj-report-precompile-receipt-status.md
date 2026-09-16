# rskj: a precompiled-contract throw yields a SUCCESS receipt while the sender is charged the full gas limit

**To:** RSK / rskj security team
**Severity:** Low — correctness and fee-reporting, not exploitable to take another party's funds
**Status:** consensus-frozen; observable on mainnet; cannot be corrected without a hard fork
**Source examined:** rskj `3dc9d977228d938385f0ee7e025162da5c220f1b` (2026-09-09)
**Reported by:** the Rustock project (independent Rust RSK node), found by whole-chain replay

---

## Summary

When a transaction calls a precompiled contract directly and that precompile
throws, `TransactionExecutor` records the exception for **fee** purposes but not
for **receipt** purposes. The result is a transaction that:

- reports `status = 0x1` (SUCCESS) in its receipt,
- reports `gasUsed = requiredGas + basicTxCost` in its receipt,
- but charges the sender the **entire gas limit**, crediting the difference to
  REMASC.

The receipt therefore understates the fee actually paid, by
`(gasLimit − gasUsed) × gasPrice`, and gives the sender no indication that their
call failed.

## Mechanism

Two independent fields decide two different things.

**Receipt status** is derived from `executionError`
(`TransactionExecutor.java:565`):

```java
receipt.setStatus(executionError.isEmpty() ? SUCCESS_STATUS : FAILED_STATUS);
```

`executionError` is set **only** by `execError(...)`.

**The fee** is derived from `result.getException()`, via
`buildTransactionExecutionSummary`:

```java
if (result.getException() != null) {
    summaryBuilder.markAsFailed();
}
```

and a failed `TransactionExecutionSummary` returns zero from `getLeftover()` and
`getRefund()`, so `finalization()` refunds nothing and REMASC receives
`gasLimit × gasPrice`.

The two paths that can throw treat these fields differently.

**The VM path sets both** (`TransactionExecutor.java:481`):

```java
} catch (Exception e) {
    cacheTrack.rollback();
    gasLeftover = 0;
    execError(e);            // receipt -> FAILED
    result.setException(e);  // summary -> failed
}
```

**The precompile path sets only one** (`TransactionExecutor.java:375`):

```java
} catch (VMException | RuntimeException e) {
    result.setException(e);  // summary -> failed
}                            // execError is never called: receipt stays SUCCESS
```

The missing `execError(e)` in the precompile catch block is the whole of it.

## Scope

Any precompiled contract reached directly by a transaction, whose `execute()`
throws. In practice this is the Bridge, because `Bridge.execute` deliberately
converts every internal failure into a `VMException`:

```java
} catch (Exception ex) {
    throw new VMException(String.format("Exception executing bridge: %s", ex.getMessage()), ex);
}
```

`Bridge.java` contains 27 `throw new VMException` sites, plus the
`BridgeIllegalArgumentException` paths that funnel into the same wrapper — so
malformed arguments to any Bridge method reach this behaviour.

Internal calls (depth > 1) are unaffected: `Program.executePrecompiledAndHandleError`
handles those separately and the CALL fails visibly.

## Reproduction on mainnet

**Block #9,217,796**, transaction index 1
(`0x102c9ea0bc0ba563...` is index 2; the relevant one is
`0x4ebcf42bb04dc4ff...`).

```
to        0x0000000000000000000000000000000001000006   (Bridge)
selector  0xf10b9c59  addSignature(bytes,bytes[],bytes)
gas limit 200,000      gas price 26,065,600
```

The `bytes[]` holds one 71-byte element beginning `0x9f`:

```
9f2c0070c9e4c56639df9eb25efec97a1e4886a848f49a38fcdc970c7aec9274
821124693dc8ec5c699ddee0283c71331d86b5f61b6865182e0618dfb7bb52b2
65158d1987d0e1
```

This is not a DER SEQUENCE (no `0x30` tag), so
`BtcECKey.ECDSASignature.decodeFromDER` throws inside `Bridge.addSignature`
(`Bridge.java:632`).

Observed on chain:

| | value |
|---|---|
| receipt status | `0x1` (SUCCESS) |
| receipt gasUsed | 95,488 |
| sender balance change | −5,213,120,000,000 wei = **200,000 gas** × 26,065,600 |
| REMASC balance change | +5,213,120,000,000 wei |
| fee implied by the receipt | 2,488,952,012,800 wei = 95,488 × 26,065,600 |
| **discrepancy** | **2,724,167,987,200 wei** (104,512 gas) |

The sender paid 2.1× what the receipt reports, and was told the call succeeded.

Verified independently: a from-scratch re-execution of this block reproduces the
chain's state root only when the full gas limit is charged, and reproduces the
chain's `gasUsed` and receipts root either way.

## Impact

1. **Receipt status is wrong.** A caller whose Bridge arguments are malformed is
   told the transaction succeeded. There is no on-chain signal to investigate.
2. **Fee accounting is wrong for every off-chain consumer.** Explorers, wallets,
   exchanges and accounting systems that compute a transaction's cost as
   `receipt.gasUsed × gasPrice` under-report by up to
   `(gasLimit − gasUsed) × gasPrice`. Block-level totals will not reconcile
   against balance changes.
3. **Callers lose the whole gas limit.** The loss is bounded by the limit the
   caller chose, and is borne by the caller — it is not a route to another
   party's funds. But `eth_estimateGas` returns the small figure, so a wallet
   sizing a limit from it and adding headroom will pay all of that headroom.

We do not consider this exploitable for theft, and we are not aware of any way
for a third party to force a victim into this path.

## On fixing it

This behaviour is part of consensus: mainnet state roots depend on the sender
being charged the full limit. Changing `TransactionExecutor` to call
`execError(e)` in the precompile catch block would change the receipt status of
historical transactions and, if the fee path were also aligned, the state root.
Any correction therefore needs an activation height.

The lower-risk subset is the receipt status alone, which is not committed to by
the state root but *is* committed to by the receipts root — so that too requires
a fork.

We raise it because it is a live source of incorrect fee reporting for anyone
consuming rskj receipts, and because independent implementations must reproduce
it exactly. Rustock now does, documented in its compatibility catalogue as
"§9d — `addSignature` with a non-DER signature: success receipt, full-limit fee".

## How this was found

A whole-chain replay of RSK mainnet (9,217,860 blocks executed, 7,626,860 state
roots checked against their headers) produced exactly one divergence, at
#9,217,796. Localising it to two account balances — the sender's and REMASC's —
differing by precisely the unrefunded gas identified the fee path rather than
any Bridge state.

## Contact

Please reply to the sender of this report. We are happy to supply the
reproduction tooling, the localisation output, or additional mainnet occurrences
if useful.
