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

## 5. Sweeps

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

## 6. Operating it

| Flag | Default | Meaning |
|---|---|---|
| `--prune-keep-depth` | 100,000 | Blocks kept below the head; clamped up to 8,000 |
| `--prune-max-batch` | 50,000 | Most blocks one sweep may remove |
| `--rpc-admin` | off | Required for the methods below |

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

Nothing triggers pruning automatically. The node prunes only when asked.

## 7. Tests

`crates/storage/src/pruner.rs`:

| Test | Pins |
|---|---|
| `keeps_at_least_the_minimum_depth_whatever_the_configuration_says` | the 8,000 clamp |
| `genesis_is_never_pruned` | §2 |
| `prunes_nothing_on_a_chain_shorter_than_the_retention_depth` | short chains are not an error |
| `removes_headers_bodies_receipts_and_the_transaction_index` | what is actually deleted |
| `records_a_floor_that_can_be_read_back` | §4, including the difficulty anchor |
| `pruning_is_resumable_and_idempotent` | §5, and the off-by-one it caught |
| `the_chain_above_the_floor_stays_walkable_by_parent_hash` | §3 |

## 8. Not yet done

- **No automatic trigger.** Pruning happens only on request.
- **Not measured on a real database.** The tests use synthetic chains; timings
  and reclaimed bytes on a mainnet-sized store are unknown.
- **Log filtering is unexamined.** `eth_getLogs` over a pruned range returns
  nothing rather than an error saying the range is gone, which is honest but
  may not be what a caller expects.
- **The connection-point search is not floor-aware.** It converges above the
  floor by accident rather than by construction; an explicit clamp would be
  clearer.
