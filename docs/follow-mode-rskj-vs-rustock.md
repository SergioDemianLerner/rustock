# Following the tip: rskj's design and rustock's

Two clients, two ways of handling the blocks that compete to be the tip. rskj
executes every competing block on arrival and keeps chain selection as a
separate pointer move; rustock maintains a single *executed head* that must sit
on the canonical chain, and rolls execution back when a sibling wins.

This note exists because the difference is invisible until you read rustock's
log at the tip and find it re-executing blocks several times an hour, with no
equivalent in rskj. Neither design is wrong. They pay for the same guarantee in
different currencies.

## 1. The symptom

A rustock node following mainnet logs pairs like this:

```
WARN Executed head #9302668 (0x55a6…) is not canonical at its height
     (Some(0x0ff3…)); rolling execution back onto the canonical chain
WARN Execution resumed at #9302667 (0x9e3f…) (was #9302668 (0x55a6…));
     1 block(s) will be re-executed
```

Measured on the production node over 2 h 08 m (2026-10-06 23:31 → 2026-10-07
01:39), while keeping pace with the network at ~29 s/block:

| | |
|---|---|
| head advanced | 9,303,017 → 9,303,281 (**264 blocks**) |
| re-execution events | **37** — 35 × 1 block, 1 × 2, 1 × 3 |
| trigger: executed head not canonical at its height | 25 |
| trigger: executed head has no descendant among stored blocks | 15 |
| trigger: executed head orphaned by a canonical child | 12 |
| trigger: head re-verification retreat | 11 |

So roughly **14% of blocks involve a rollback**, almost always of depth 1.

> **Counting note.** The log carries only 71 `Executed block #` lines over that
> window against 264 blocks of progress, because most blocks are executed in
> batches logged as `Processed N blocks`. Using those lines as the denominator
> inflates the rate to ~52%. Use the head delta.

## 2. rskj: execute on arrival, select afterwards

**Source**: `rskj-core/src/main/java/co/rsk/core/bc/BlockChainImpl.java:188`
(`internalTryToConnect`), `co/rsk/core/bc/SelectionRule.java`.

rskj does **not** wait to see which block wins. Every block that connects is
validated and executed immediately, and only then considered for the tip:

```java
result = blockExecutor.execute(null, 0, block, parent.getHeader(), false, noValidation, true);
boolean isValid = noValidation ? true : blockExecutor.validate(block, result);
...
stateRootHandler.register(block.getHeader(), result.getFinalState());

// only now is the tip decided
if (SelectionRule.shouldWeAddThisBlock(totalDifficulty, status.getTotalDifficulty(), block, bestBlock)) {
    if (bestBlock != null && !bestBlock.isParentOf(block)) { blockStore.reBranch(block); }
    switchToBlockChain(block, totalDifficulty);          // ImportResult.IMPORTED_BEST
} else {
    extendAlternativeBlockChain(block, totalDifficulty); // ImportResult.IMPORTED_NOT_BEST
}
```

Three properties follow, and they are the whole difference:

1. **Execution context is the block's own parent**, not a global head —
   `blockExecutor.execute(…, block, parent.getHeader(), …)`. A sibling is not an
   anomaly; it is a branch with its own state.
2. **The computed state is filed per block**, via
   `stateRootHandler.register(block.getHeader(), result.getFinalState())`, for
   losing branches as much as for the winner.
3. **Chain selection executes nothing.** Both arms are pointer work:

   ```java
   private void switchToBlockChain(Block block, BlockDifficulty td) { setStatus(block, td); }
   private void extendAlternativeBlockChain(Block block, BlockDifficulty td) { storeBlock(block, td, false); }
   ```

   `blockStore.reBranch(block)` re-points the canonical index. **A reorg in rskj
   re-executes nothing**, because every block on both branches was already
   executed exactly once when it arrived.

`SelectionRule.shouldWeAddThisBlock` compares total difficulty first, then — on
a tie — paid fees (a challenger with more than 2× the incumbent's fees wins),
and finally the lower block hash, provided fees are at least half the
incumbent's. Note that this is a *selection* rule evaluated after execution, not
a reason to defer execution.

## 3. rustock: one executed head, rolled back when it is wrong

**Source**: `crates/sync/src/service.rs` (`record_execution` at :202,
`EXECUTION_OFF_CHAIN_GRACE` at :58, the rollback at :2110, the resume log at
:2469), `crates/sync/src/invariant.rs:112` (`Violation::ExecutedOffChain`, I5).

rustock executes the block it has just downloaded and advances a single
*executed head* marker. A separate invariant checker compares that marker
against the canonical index. When the block it executed turns out not to be
canonical at its height, and the disagreement persists for
`EXECUTION_OFF_CHAIN_GRACE` (10 s), it calls `roll_execution_back`, retreats to
the common ancestor and executes forward along the canonical chain.

The refusal in `record_execution` is deliberate and load-bearing:

> `Transition::Executed` refuses a block that is not canonical at its height,
> which happens whenever a reorg lands between executing a block and recording
> it. A caller that ignores the refusal goes on to claim the block executed and
> to advance its in-memory state root to a block the node is not on — which is
> how #9,267,779 came to be logged as `Executed block` one line after the store
> refused it, on 2026-09-24 at 14:01:15.

The 10-second grace exists so that transient disagreement, resolved by the next
block, does not cause rollback thrash.

## 4. Side by side

| | rskj | rustock |
|---|---|---|
| When a competing block is executed | immediately, on arrival | immediately, on arrival |
| Execution context | the block's **own parent** | the current **executed head** |
| State of a losing branch | computed and registered | computed, then unwound |
| Reorg cost | pointer move (`reBranch` + `setStatus`) | roll back, re-execute forward |
| Can the same height be executed twice? | no | yes |
| Failure mode if the invariant is missed | — (no such marker) | executed head off-chain, or naming state the store lacks |

**Neither design waits.** The common intuition that rskj defers execution until
the tip settles is not what the code does; what rskj avoids is not execution but
*re-execution*.

**Total EVM work is comparable.** When both siblings arrive, rskj executes both.
rustock executes the loser, rolls back, then executes the winner — also two. The
extra cost in rustock is not EVM throughput but state unwinding, in-memory root
reloads, and the coherence re-check.

**What differs is the correctness surface.** rustock's single marker is a thing
that can be wrong; rskj has no equivalent to be wrong. Two recorded incidents
are of exactly that shape:

- **2026-09-24, #9,267,779** — execution recorded against a block the store had
  refused; the coherence check had to reload the committed root five seconds
  later.
- **2026-09-23** — "a rollback left execution on an abandoned branch; by the
  time that branch became canonical again its state had been collected. I5
  cleared itself, I7 did not, and the node sat in a retry loop failing every
  block with `NonceTooHigh { state: 0 }`."

## 5. Does the trie collector endanger a branch near the tip?

**No.** This is worth stating because rustock's `epoch` trie backend *does*
discard historical state, which invites the worry that a sibling's state could
be collected before the branch is re-adopted. Under the current design and
parameters it cannot, for a reason stronger than parameter sizing.

**Source**: `docs/trie-gc-design.md` §4.3, §5.

The collection cycle sweeps only the oldest epoch `E₀`, and precondition P2 /
invariant **I8 — Write separation** gates it:

> `E₀` may be swept only if every block whose writes went into `E₀` is at or
> below `H`.

`H` is the collection root, chosen at burial depth `D` below the head
(`--gc-burial`, default and production value **4000 blocks**). State written for
any block above `H` — canonical or not — is in a younger epoch and is not a
candidate for the sweep. A sibling produced at the tip is written into the
current epoch `E_{N-1}`, which is the newest, not `E₀`.

Measured on the production node (archive blocks, `epoch` trie, `N=4`,
rotate 1024 MB), historical **state** is present at 10,000 and 60,000 blocks
below the head and gone by 70,000 — so the practical window is far deeper than
any tip-level reorg.

One precision worth keeping, because it is the part that is easy to get wrong:
a sibling's state survives **by epoch age, not by reachability**. The mark phase
computes the live set from `root(H)` plus pinned roots only; a non-canonical
sibling is not reachable from `H` and is never marked live. It survives because
it sits in a young epoch, and it is dropped — correctly — once that epoch ages
into `E₀`, tens of thousands of blocks later.

> **Open thread.** The 2026-09-23 incident quoted in §4 describes a branch whose
> state *had* been collected by the time it became canonical again. Under
> `D = 4000` and a ~60–70k-block epoch window, that is not reachable from a
> tip-level reorg; it would require an abandonment spanning the whole window.
> Either the parameters differed then, or the state was missing for a different
> reason and the comment's attribution is loose. Worth confirming before citing
> that incident as evidence against the current collector.

## 6. What each design buys

**rskj's shape** costs storage — the state of every losing branch is retained —
and buys a reorg that is free and an execution path with no marker to
desynchronise.

**rustock's shape** costs a rollback per losing race, plus the invariant
machinery (I5, I7 and the coherence checks) needed to detect and repair a marker
that has gone wrong. It buys a single, simple notion of "where execution is",
and it composes with the `epoch` collector: rustock does not have to keep the
state of every branch it ever saw.

The 14% figure in §1 is the running cost of that choice at the tip. It is not a
defect, and the node keeps pace with the network while paying it — but it is the
number to watch, because a rise in it is the first sign that rollback has stopped
converging.
