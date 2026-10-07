# Block pruning

Removes chain history — headers, bodies, receipts and the events in them, the
transaction index, the canonical mapping and total difficulty — for blocks far
enough behind the head that nothing can ask for them.

Distinct from the trie collector in [`trie-gc-design.md`](./trie-gc-design.md),
which reclaims *state*. The two are independent, reclaim different things, and
may run at the same moment.

## 1. How deep is deep enough

Two requirements stack rather than overlap:

| | |
|---|---|
| **Reorg tolerance** | Rebuilding from a reorganisation `D` blocks deep needs those blocks |
| **Block-info precompiles** | Rootstock exposes precompiles that read attributes of blocks up to **4,000** back, so executing a block at height `h` may read from `h − 4000` |

After a 4,000-block reorg the node re-executes from `head − 4000`, and executing
*that* block may itself reach a further 4,000 back. The shallowest safe
retention is therefore **8,000 blocks**.

`MIN_KEEP_DEPTH = 8_000` is a floor, not a default: `--prune-keep-depth` and the
RPC are both **clamped up** to it. A configuration asking for less is corrected,
not honoured, because the cost of being wrong is a node that cannot execute after
a reorg and cannot say why.

## 2. Genesis is always retained

Pruning leaves a hole in the middle of the chain, never a missing beginning.

Keeping block 0 costs one block and keeps every "start of the chain" lookup
working — `eth_getBlockByNumber("earliest")`, the sync fallbacks, the execution
cursor. It also means the chain's anchor is a hash fixed by the network rather
than by whatever this particular node still happens to hold.

## 3. What a database looks like afterwards

```
  #0            #1 … #floor-1          #floor … #head
  genesis       pruned (absent)        full data
  retained
```

Everything that walks the chain must tolerate the gap:

- **Walking back by `parent_hash`** stops when a header is absent.
  `ensure_canonical_lineage` already did this — its "parent not in store"
  branch predates pruning.
- **The sync connection-point search** converges to a height at or above the
  floor, because heights below it are not "ours".
- **RPC** answers `null` for a pruned height, exactly as for an unknown one.
- **Serving peers** returns nothing for pruned ranges; the peer asks elsewhere.

## 4. The floor record

Stored in the database under `prune_floor`: the lowest retained block's number,
hash, and **cumulative difficulty**.

The difficulty is the part that matters. Everything below the floor is gone, so
this is the only remaining anchor for the chain's accumulated weight — the value
that would otherwise have to be recomputed by walking blocks that no longer
exist.

It is written **before** the deletions it describes. An interrupted sweep then
understates what is held rather than claiming blocks that are already gone.
Over-reporting is the dangerous direction: it invites a reader to ask for
something deleted.

## 5. Retention is measured from the *executed* head

Not from the header head. A node may hold headers far above what it has
executed, and pruning from the header head would delete blocks the executor
still has to read:

```rust
let Some(executed) = executed else { return Ok(None) };
let head_number = head_number.min(executed);
```

A node that has executed nothing prunes nothing, even if it holds the whole
chain. This is not a refinement of the depth rule — it is what stops a node
that is importing faster than it executes from deleting its own input.

## 6. Sweeps

Pruning removes at most `max_batch` blocks per call and resumes from the floor,
so the same mechanism serves either shape:

- **Periodic**, alongside a collection cycle — one large sweep.
- **Continuous**, a little each block — `max_batch` small, called often.

It is idempotent: a second call with the same head removes nothing and leaves
the floor where it was.

*Resume starts **at** the floor, not past it.* The floor is the lowest retained
block and is the next one eligible to go; starting at `floor + 1` leaves one
block behind on every resumed sweep, stranding data below the recorded floor
that nothing would ever report or reclaim. This was a real bug, caught by the
resume test — 3,993 of 4,000 blocks removed.

## 7. Operating it

| Flag | Default | Meaning |
|---|---|---|
| `--prune-blocks` | off | Sweep automatically as the node runs |
| `--prune-every-secs` | 300 | Seconds between automatic sweeps |
| `--prune-keep-depth` | 100,000 | Blocks kept below the head; clamped up to 8,000 |
| `--prune-max-batch` | 50,000 | Most blocks one sweep may remove |
| `--prune-frozen-headers` | off | Also delete headers the freezer already holds |
| `--rpc-admin` | off | Required for the methods below |

Without `--prune-blocks` the node prunes only when asked over RPC, which is
what it did before the automatic sweep existed.

```bash
# Let the node choose the boundary from its own head
rsk_pruneBlocks []

# Prune everything below a chosen height (still clamped)
rsk_pruneBlocks [9200000]

# What both reclamation mechanisms are doing
rsk_storageStatus []
```

`rsk_pruneBlocks` returns immediately and sweeps on a blocking task.
`rsk_storageStatus` reports block pruning and trie collection together, because
an operator asking what a node deletes and what it still holds wants one answer.

### The automatic sweep

With `--prune-blocks`, the node sweeps on its own every `--prune-every-secs`
(default 300). The flag is off by default and deliberately so: turning it on is
not a tuning change, it deletes history that only a resync can bring back.

Two gates stand in front of every automatic sweep.

**The snapshot gate.** `prune_allowed` starts *closed* and is opened only once
the sync service knows no snapshot session is coming:

```rust
if self.snap.is_none() && self.snap_restart.is_none() {
    self.prune_allowed.store(true, Ordering::Relaxed);
}
```

It starts closed rather than being inferred later from the absence of a
session, because "no session yet" and "no session ever" are different states
and only the second is safe. A snapshot sync fetches blocks below its
checkpoint; a sweep running beside it would delete them as they arrive.

**The freezer gate.** `plan_prune` will not pass the freezer's uncle progress:

```rust
if let Some(f) = self.freezer() {
    let frozen_uncles = f.uncles_end_number();
    if frozen_uncles == 0 { return Ok(None); }
    target = target.min(frozen_uncles - 1);
}
```

Uncle headers exist only in block bodies, so deleting a body before the freezer
has copied them destroys the chain's own record of the work it absorbed, and no
later pass can rebuild it. This is a real race rather than a theoretical one:
the freezer works below `head − FREEZE_DEPTH` (20,000) while the pruner works
below `head − keep_depth` (8,000 floor). The pruner's range is the shallower,
so left alone it reaches every block first. See
[`freezer-estimates.md`](./freezer-estimates.md).

### Announce before deleting

A sweep tells peers the range is narrowing *before* it narrows it. `plan_prune`
is the single source of truth shared by the announcement and the deletion, so
the range a node advertises and the range it deletes cannot disagree.

The ordering matters in one direction only. Announcing first means a peer may
briefly believe we hold less than we do, and ask elsewhere for something we
still have — wasteful, harmless. Deleting first means a peer believes we hold
something already gone, and asks us for it. The announcement is an upper bound;
the floor recorded after the sweep is what was actually achieved.

## 8. Tests

`crates/storage/src/pruner.rs`:

| Test | Pins |
|---|---|
| `keeps_at_least_the_minimum_depth_whatever_the_configuration_says` | the 8,000 clamp |
| `genesis_is_never_pruned` | §2 |
| `prunes_nothing_on_a_chain_shorter_than_the_retention_depth` | short chains are not an error |
| `removes_headers_bodies_receipts_and_the_transaction_index` | what is actually deleted |
| `receipts_are_pruned_with_their_blocks` | receipts go |
| `bridge_events_survive_the_blocks_they_came_from` | bridge events stay |
| `records_a_floor_that_can_be_read_back` | §4, including the difficulty anchor |
| `pruning_is_resumable_and_idempotent` | §6, and the off-by-one it caught |
| `the_chain_above_the_floor_stays_walkable_by_parent_hash` | §3 |
| `a_sweep_starts_where_the_block_database_begins` | a pruned or imported store sweeps from its real bottom |
| `an_ordinary_node_still_sweeps_from_the_bottom` | that the above did not change the ordinary case |
| `retention_is_measured_from_the_executed_head_not_the_header_head` | §5 |
| `a_node_that_has_executed_nothing_prunes_nothing` | §5, the degenerate case |
| `a_node_that_is_keeping_up_prunes_as_before` | that §5 did not change the ordinary case |
| `the_plan_matches_what_the_sweep_does` | announcement and deletion share one source |
| `planning_changes_nothing` | `plan_prune` has no side effects |
| `the_announced_floor_is_never_below_the_achieved_one` | the safe direction of the announcement |
| `a_node_with_nothing_to_prune_has_no_plan` | nothing to say means nothing announced |
| `pruning_waits_for_the_freezer_to_copy_the_uncles` | the freezer gate |
| `nothing_is_pruned_before_the_freezer_has_started` | an empty uncle store stops the sweep |
| `the_keep_depth_still_binds_once_the_freezer_is_ahead` | the gate holds the pruner back, it does not replace the depth rule |

## 9. Not yet done

- **Not measured on a real database.** The tests use synthetic chains; timings
  and reclaimed bytes on a mainnet-sized store are unknown. No production node
  has run with `--prune-blocks`.
- **A node without a freezer is unguarded.** `--freezer false` with
  `--prune-blocks` discards the uncle headers along with the bodies, and the
  chain's accumulated work becomes unrecomputable below the floor. Correct —
  there is no second copy to wait for — but it is a configuration that quietly
  gives something up.
- **Log filtering is unexamined.** `eth_getLogs` over a pruned range returns
  nothing rather than an error saying the range is gone, which is honest but
  may not be what a caller expects.
- **The connection-point search is not floor-aware.** It converges above the
  floor by accident rather than by construction; an explicit clamp would be
  clearer.
