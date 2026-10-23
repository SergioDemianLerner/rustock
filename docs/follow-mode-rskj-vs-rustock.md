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

### Update, 2026-10-23: the 14% is historical

That figure no longer describes the node. Two windows measured the same way,
on the same production node, seventeen days later:

| | 2026-10-06 (§1 above) | 2026-10-23 morning | 2026-10-23 night |
|---|---:|---:|---:|
| window | 2 h 08 m | 7.96 h | 2.93 h |
| head advanced | 264 | 899 | 333 |
| re-execution events | 37 | 4 | 3 |
| **as a share of blocks** | **14.0%** | **0.4%** | **0.9%** |
| `rolling execution back` | — | 0 | 0 |
| executed head orphaned, adopted instead | — | 103 | 115 |
| batches logged `re-adopted without re-executing` | — | 147 / 359 | 86 / 149 |
| reorgs (not under our control) | — | 26.0/h | 53.7/h |

Two things changed. The rollback path has stopped firing altogether: churn at
the tip is absorbed by the cheaper orphan-adoption path instead, and **over
half** of all batch executions now report `re-adopted without re-executing`.
And the three remaining events in the night window are all the
`record_execution` refusal quoted in §3 -- `Executed #N but could not record
it` -- which is the guard working rather than failing.

Note the morning window predates the fixes of 2026-10-23 (#326, #327, #328),
and already reads 0.4%. The decline is not attributable to them.

What the night window *does* show about those fixes, for the record: wall time
lost to the 120 s execution watchdog went from 35.4% to 0.0%, across a period
when the network ran at 2.05x the morning's reorg rate. That is a separate
failure class from the one this note is about -- see #323.

**The number to watch is still the right number; its value is simply no longer
14%.** A return toward it would mean rollback has stopped converging.

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
- **2026-10-23, #9,350,770** — the same shape, traced end to end. Follow mode
  executed the block and produced `0xdf2486a5…`, exactly the header's root.
  A reorg then retreated the committed `exec_head` to #9,350,769 without
  telling the in-memory root, which still held "after #9,350,770". Twenty-four
  seconds later the body pipeline re-executed #9,350,770 against that
  in-memory root — applying the block on top of itself — and computed
  `0x6fa62d19…`:

  ```
  18:59:56.4  Executed block #9350770, state root: 0xdf2486a5…   (== header)
  19:00:20.8  Block #9350770 execution failed: state root mismatch:
              header=0xdf2486a5…, computed=0x6fa62d19…
  19:00:20.9  WARN In-memory state root 0xdf2486a5… disagrees with the
              executed head's committed root
  ```

  Note the last two lines. `resync_state_root_with_exec_head`, the guard that
  exists for precisely this, fired **0.1 s after** the pipeline had already
  executed: it runs once per tick, and the execution landed in the gap between
  the retreat and the next tick.

  The block held one transaction and `gasUsed = 0` — REMASC alone, body derived
  from the header. That is why it surfaced as a root mismatch rather than
  `NonceTooLow`: REMASC carries no signature and is never nonce-checked
  (`recover_senders` returns `Address::ZERO` for it), so the second application
  re-ran the fee distribution instead of being rejected. **The two symptoms are
  one bug.** A duplicated block with real transactions fails the nonce check
  before any state diverges and is named `NonceTooLow`; a duplicated empty
  block has nothing to validate, executes "successfully", and is caught only by
  the root comparison at the end.

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

## 7. Verdict: which algorithm should rustock use?

Asked directly — is rskj's algorithm better or worse for rustock? — the answer
is that it is two separable rules, and they have opposite answers.

rskj's design is doing two independent things:

1. **Execute each block against its own parent's state**, resolved from the
   block, not from an ambient pointer.
2. **Retain the computed state of every branch**, filed per block by
   `stateRootHandler.register(...)`, so chain selection is pointer work.

Rule 1 is the one that prevents every incident in §4. Rule 2 is the one that
costs storage. They are usually described together, but rustock can take the
first without the second.

### Rule 1: adopt it

Every incident in §4 — 2026-09-23, 2026-09-24, 2026-10-23 — is the same
sentence: *the executor applied a block to a state that was not that block's
parent*. No amount of guarding fixes this class, because the guard is a timer
and the bug is a race; 2026-10-23 lost that race by 0.1 s. The defect is that
execution reads its input from a mutable global (`current_state_root`) when
that input is fully determined by the block itself.

In rustock this is a small change, not an architectural one, because the
pieces already exist:

- `ExecFn` already takes the state root as its first argument
  (`Arc<dyn Fn(TrieNode, u64, Vec<(B256, Header)>) -> ExecOutcome>`), so
  execution is already parameterised by the state to run against.
- The parent's state root needs no new index: **the parent header already
  carries it**. rskj needs `stateRootHandler` because its state root is not
  the header's; rustock verifies computed-against-header on every block, so
  `parent.state_root` *is* the answer.
- Loading a root by hash is the two-line lookup #326 already performs:
  `trie_store.get(root)` then `TrieNode::from_message`.

So `spawn_execution` would resolve the parent of `pending[0]` and load
`parent.state_root` instead of passing `self.current_state_root`. The executed
head stays exactly as it is — a canonical pointer for sync resume and for the
collector — but stops being the *execution input*. A block then cannot be
applied to the wrong state, because the state is chosen by the block's own
parent hash.

The one case needing a decision: the parent's state may not be in the trie
store (a deep reorg, or an epoch that has aged out). Today that is implicit and
produces a wrong answer; after the change it is an explicit lookup failure, and
refusing is strictly better than computing a root nobody asked for.

### Rule 2: do not adopt it

Retaining every losing branch's state is the wrong trade for rustock, and §5
explains why: the `epoch` collector drops state **by epoch age, not by
reachability**. A sibling survives because it sits in a young epoch, and is
correctly dropped once that epoch ages into `E₀`.

Registering losing branches in a durable per-block index would create entries
pointing at state the collector is designed to reclaim. The failure mode would
be a registry resolving to missing nodes tens of thousands of blocks later —
quieter and harder to diagnose than the loud mismatch we get today. Making the
collector mark sibling roots live instead would mean retaining branch state
indefinitely, which is the storage bill rustock's design exists to avoid (§6).

### Summary

| | rskj rule | for rustock |
|---|---|---|
| Execute against the block's own parent | yes | **adopt** — removes the whole §4 class, and the plumbing already exists |
| Retain every branch's state per block | yes | **reject** — incompatible with age-based epoch collection, for the reasons in §5 |

So: **better in its execution-context rule, worse in its state-retention rule.**
The useful half is also the cheap half. Adopting rule 1 alone leaves rustock
with rskj's correctness property on execution input and its own storage
economics — which is a better position than either client occupies today.

What this does *not* fix: reorg cost. rustock would still re-execute after a
reorg where rskj does not. §1's update shows that cost is now 0.4–0.9% of
blocks rather than 14%, so it is no longer the part worth optimising.
