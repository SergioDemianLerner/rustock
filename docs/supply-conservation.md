# Supply conservation: rejecting blocks that create rBTC

## The requirement

As originally stated:

> When a transaction is processed, I guess there is a moment where a set of
> changes to the state are applied. At that point it should be possible to
> account for all native balance changes and make sure that no bitcoins were
> created from thin air. I recall that the bridge has the full 21M bitcoins in
> its balance, so when a peg-in occurs, and bitcoins are unlocked, the net
> change should also be zero. Let B be OUTFLOWS-INFLOWS. [...] If B is positive
> (meaning the transaction created more native BTC than existed before), then
> mark the block as invalid, and log it. If the net balance is negative, then
> log it. It could be the case of a SELFDESTRUCT destroying paying itself or
> other EVM rarity. [...] A special case may be the REMASC transaction, which
> can burn rBTC, but I don't think it can have a positive B.

The premise is exact: the peg is backed 1:1 by bitcoin and the Bridge holds the
entire 21 M supply, so a peg-in **moves** value out of the Bridge rather than
minting any. The sum of every account balance must never grow.

## What is computed

For each block, over exactly the accounts that block writes:

```
B = Σ (balance_after − balance_before)
```

| | meaning | action |
|---|---|---|
| `B > 0` | rBTC appeared from nowhere | **block rejected** (`ProcessError::SupplyCreated`), logged at ERROR, alert raised |
| `B < 0` | rBTC was destroyed | logged at WARN, **block accepted** |
| `B = 0` | conserved | normal, silent |

A negative `B` is not treated as a fault because it has legitimate causes: a
contract self-destructing to itself removes its own balance, and REMASC may burn
a share of fees. Recording it is still worthwhile — an unexplained burn is worth
knowing about even when it cannot steal anything.

`before` is read from the pre-block trie; `after` is the balance about to be
written. The set of accounts considered mirrors `apply_state_changes` exactly,
including the self-destruct-then-recreated case: an account destroyed and not
touched again ends at zero, while one touched afterwards ends at its new balance
(mainnet #3,173,807). Accounts merely *read* during execution are ignored —
they are not written, so counting them would invent changes that never happened.

## Why per block, not per transaction

The requirement supposed changes are applied per transaction. They are not.

The executor holds a **single revm journal across the whole block**. Each
transaction mutates that journal, and the merged state is written once, in
`apply_state_changes`, called from `execute_block`. There is no per-transaction
application step to hook.

Per-transaction deltas are not simply available either:

- `revm`'s `Account` carries `original_info`, which *is* refreshed on cold load
  within each transaction — so it does encode a per-transaction baseline. But it
  is documented as serving Block Access Lists, not as a general "balance before
  this transaction" contract. Building a consensus-affecting rejection rule on
  an internal whose purpose is something else is the kind of coupling that
  breaks silently on a dependency bump.
- Reconstructing per-transaction balances independently would mean snapshotting
  every touched account's balance after each transaction — real cost on the
  hot path, for attribution rather than detection.

**The block is also the right unit for the action.** What gets rejected is a
block, not a transaction: a block is what the node accepts or refuses. A
per-transaction check would still have to reject the whole block.

Attribution is not lost. The log names the accounts that moved, largest first,
which is more useful than a transaction index when the question is *where the
value came from* — a transaction index tells you where to look, an account tells
you what happened.

## Cost

One trie read per account the block writes, against the pre-block root whose
nodes execution has just walked, so the reads are cache hits. A block touching a
dozen accounts pays a dozen lookups on top of writes it was already doing.

No measurable effect was observed on replay throughput (13,925 blocks at 36
blocks/s with the check active, against 23 blocks/s for an earlier 4,000-block
run on a colder cache).

## Alerting

Execution must not depend on the alerting crate — alerting already depends on
execution. So execution exposes an observer that the node installs at startup:

```
block processing  →  supply::report()  →  observer  →  mpsc channel
                                                          ↓
                                            alert watcher task  →  sinks (log, SMTP)
```

The observer runs on the thread executing blocks, so it only enqueues and
returns. Delivery happens on the watcher's own task, preserving the constraint
from the peg-out alerting work: **mail must never block block processing.**

The alert is `Alert::SupplyNotConserved`, deduplicated per block. A tool that
replays blocks installs no observer, which is how the diagnostic harness stays
silent and sends no mail whatever it finds.

## Validation

`examples/check_supply` replays a block range through the same `execute_block`
the node uses, with a trie store that discards writes.

| range | blocks | result |
|---|---:|---|
| #9,244,161..#9,248,160 | 4,000 | all conserved, 0 created, 0 destroyed |
| #9,234,347..#9,248,271 | 13,925 | all conserved, 0 created, 0 destroyed |

The second range was chosen to contain two complete peg-out lifecycles:

```
9,233,637 request → 9,233,721 release_requested → 9,237,722 pegout_confirmed
                                                → 9,237,733 release_btc
9,239,933 request → 9,239,935 release_requested → 9,243,937 pegout_confirmed
                                                → 9,243,961 release_btc
```

`B` was exactly zero on every block, including all 2,783 `update_collections`
and 10 `add_signature` blocks. Every state root also matched its header, which
independently confirms the read-only harness does not perturb the replay.

**REMASC did not burn in this range.** The requirement anticipated it might; in
13,925 blocks it never produced a negative `B`, so its fee handling is a
transfer rather than a burn here. The negative path is implemented and unit
tested but has not been exercised by real data.

### "Read-only" means read-only at the database layer

The first version of the harness discarded writes at the `TrieStore` layer but
opened the epoch store read-write, and RocksDB flushed a recovered write-ahead
log into a new SST 0.4 s after opening — before a single block was replayed.
Nothing was lost (WAL recovery materialises what was already committed, and 4,000
subsequent state-root matches prove the store was intact), but the claim was
wrong.

Each epoch is now opened with `open_cf_for_read_only`, which neither replays the
WAL nor writes anything. Verified by fingerprinting every file's path, mtime and
size before and after a 4,000-block run: identical. **Discarding writes above
the database is not sufficient; the database must be opened read-only.**

## Coverage, and what is not yet verified

Replayed so far: **17,925 blocks, 0.194% of the chain.** The limit is state
availability — those runs used the epoch trie backend, whose retained window
reaches back only to ~#9,234,347.

Three ways to extend it:

1. **Sample across eras** using the archival trie (`/mnt/import/rustock-trie`,
   the full unitrie for blocks 1..9,230,000). Hours, and it exercises the paths
   that differ across hard forks.
2. **Whole chain, parallelised** — the chunked, work-stealing approach from the
   segment build. Comparable to the original whole-chain verification, which
   took 36h29m across four workers.
3. **Incidentally** — the check now lives in `execute_block`, so any future
   re-verification pass carries it for free. The pending receipts-root pass
   would cover the chain at no extra cost.

**The peg-in direction is untested by replay.** No `lock_btc` or `pegin_btc`
event occurs in any block that can currently be both located and replayed. This
matters: a peg-in is where the Bridge *pays out*, and a bug there would mint
rather than move. The unit test
`a_bridge_payout_moves_value_rather_than_creating_it` covers the arithmetic, but
no real peg-in has been run through the check. Closing that gap needs a peg-in
at or below #9,230,000 replayed against the archival trie — now findable via the
Bridge event index.
