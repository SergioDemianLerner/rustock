# Supply conservation: rejecting blocks that create rBTC

## Specification

Written as a specification rather than a narrative so it can be implemented
independently — by rskj, or by any other RSK node.

### Rationale

The RSK peg is backed 1:1 by bitcoin. The Bridge contract holds the entire
supply, and a peg-in transfers value *out of* the Bridge's balance rather than
creating it. It follows that no block may increase the total of all account
balances. A node that enforces this detects any bug — in the Bridge, in a
precompile, in fee accounting, in the EVM itself — whose effect is to conjure
native currency, regardless of how the bug was reached.

### Definition

For a scope S (one transaction, or one block), over every account whose balance
the scope modifies:

```
  inflow(S)  = Σ max(0, balance_after − balance_before)
  outflow(S) = Σ max(0, balance_before − balance_after)
  B(S)       = inflow(S) − outflow(S)
```

`balance_before` is the balance at the start of S; `balance_after` the balance
at its end. An account destroyed within S and not subsequently re-created has
`balance_after = 0`. Accounts merely read are excluded: they are not modified,
so including them would report changes that did not occur.

### Required behaviour

| Condition | Meaning | Required action |
|---|---|---|
| `B > 0` | native currency was created | **Reject the block.** Log the amount and the accounts that gained. |
| `B < 0` | native currency was destroyed | **Accept the block.** Log the amount and the accounts that lost. |
| `B = 0` | conserved | Proceed silently. |

Rejection must occur **before** the block's state changes are persisted, so a
rejected block leaves no trace in the state database.

`B < 0` is deliberately not an error. Destruction has legitimate causes — a
contract self-destructing to itself, and any burn of fees — and destroying
currency cannot steal from anyone. It is logged because an unexplained burn is
still worth knowing about.

### Scope: per transaction and per block

Both scopes must be checked, and neither subsumes the other.

**Per transaction** is the primary check. Block-level netting conceals a
creation whenever one transaction mints an amount and another destroys the same
amount: `B(block)` is then zero while two transactions are individually wrong.

**Per block** is a cross-check. If `Σ B(transaction) ≠ B(block)`, value moved
outside any transaction — for example in block-level bookkeeping performed
outside the transaction loop. That discrepancy is itself a finding and should be
reported.

### Cases an implementation must get right

1. **Fees must not appear as burns.** Where an implementation debits gas from
   the sender and credits it to the block beneficiary within the same
   transaction, `B = 0`. If instead fees were debited per transaction and
   credited later, every ordinary transaction would report `B < 0` and the
   crediting transaction `B > 0` — the latter being indistinguishable from an
   actual mint. An implementation whose fee flow works that way must model the
   fee explicitly rather than treat the crediting step as a creation.

2. **The fee-distribution contract (REMASC on RSK) must not appear to mint.**
   It pays out currency it already holds: its own balance falls while
   recipients' rise, so `B ≤ 0`.

3. **Peg-in must net to zero.** The Bridge's balance falls by exactly what the
   recipient gains. A peg-in that produced `B > 0` would mean the peg was no
   longer 1:1 backed, which is the condition this check exists to detect.

4. **Destroyed-then-recreated accounts.** An account destroyed and re-created
   within the same block must be accounted once, at its final balance, not
   twice.

5. **Accounts that are read but not written must be excluded**, or ordinary
   blocks will report spurious changes.

### Non-goals

The check does not verify that value moved *legitimately*, only that none was
created. A transaction that steals another account's balance conserves supply
and will not be caught here.

## What is computed

For each scope -- each transaction, and each block -- over exactly the accounts
it modifies:

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

## Implementation: both scopes

Both are implemented and both are on by default, selectable with
`--supply-check both|per-transaction|per-block|off`.

### Per transaction (primary)

Runs after each transaction, inside `execute_block`, at all three points a
transaction can leave the loop — the REMASC system call, the
free-bridge-transaction path, and the normal path (the first two `continue`
rather than falling through).

**It needs no database reads.** revm refreshes an account's `original_info`
when the account is cold-loaded *within a transaction*
(`JournalInner::load_account_mut_optional`), so for every account a transaction
touched, `info.balance − original_info.balance` is that transaction's effect,
already in memory. `Account::transaction_id` records which transaction last
touched an account, so the block's accumulated state map filters down to the
transaction just executed.

This reads a revm internal. The field is public, but its doc comment describes
it as serving Block Access Lists, and the refresh happens to sit in the general
cold-load path rather than a BAL-specific one. A future revm could change that
without considering it breaking, so `per_transaction_baseline_holds` pins the
property actually depended on: if `original_info` stops being a per-transaction
baseline, that test fails rather than the check silently mis-accounting.

### Per block (cross-check)

Runs in `execute_block` before `apply_state_changes`, reading each modified
account's pre-block balance from the trie.

Kept on alongside the per-transaction check because the executor touches the
journal *outside* `transact_one` — loading and touching REMASC before the system
call, bumping the sender's nonce in the free-bridge path, warming the Bridge
account. Those are outside any transaction, so a pure per-transaction sum would
miss any balance change made there. If the per-transaction deltas ever fail to
account for the block's real change, the difference is exactly that, and worth
surfacing.

### An earlier, mistaken conclusion

An earlier version of this document argued the check *could not* be done per
transaction, on the grounds that the executor keeps one journal for the whole
block and that `original_info` served BAL. The first is true and irrelevant; the
second is contradicted by the code. Recorded here because the argument was
plausible and someone will make it again.

## Cost

**Per transaction: no I/O at all**, the baselines being in memory. CPU is
O(transactions x accounts) per block, since revm clears its per-transaction
entry log at commit and the block's accumulated account map must be filtered
instead. RSK's 6.8 M block gas limit bounds that at roughly 200,000 pointer
comparisons for a maximally full block -- under a millisecond against the tens
of milliseconds such a block takes to execute -- and about twenty comparisons
for a typical block of 2.14 transactions.

**Per block: one trie read per account the block writes**, against the pre-block
root whose nodes execution has just walked, so the reads are cache hits. A block
touching a dozen accounts pays a dozen lookups on top of writes it was already
doing.

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
the node uses, with a trie store that discards writes. State comes from the
epoch backend (`--trie-dir`) for recent blocks, or the archival unitrie
(`--archive-trie`) for anything older than the epoch window — which is
everything interesting, since peg-ins are old.

Both checks have been run against replayed mainnet blocks.

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

### Peg-ins

The peg-in direction is the one where a bug would *mint* rather than move, since
it is where the Bridge pays out. Four were replayed, spanning both event eras
and nearly seven million blocks of history:

| block | event | note | `B` |
|---|---:|---|---|
| #2,395,450 | `lock_btc` | the first peg-in in the chain | 0 |
| #3,600,471 | `lock_btc` | legacy era | 0 |
| #9,209,189 | `pegin_btc` | modern era | 0 |
| #9,227,761 | `pegin_btc` | most recent | 0 |

All conserved. The Bridge pays out of its own 21 M balance rather than creating
anything — verified rather than argued from design. These runs had the
per-transaction check active, so they also exercise its attribution across the
REMASC and free-bridge paths on real blocks, which fixtures cannot settle.

Locating them was the hard part until the Bridge event index existed. Peg-ins
are rare: 1,945 in the chain's history (408 `lock_btc`, 1,537 `pegin_btc`)
against 992,261 `update_collections`, which is why none fell in the 18,000-block
window reachable before. With the index it is a prefix scan taking
milliseconds; the equivalent `eth_getLogs` sweep was killed for memory.

State for these blocks comes from the archival unitrie
(`--archive-trie`), the epoch backend having long since collected it.

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

**The peg-in direction is now covered** (see Validation above): four peg-ins
across both event eras, all conserving. That was the gap worth closing first,
and the Bridge event index is what made the blocks findable.

What remains unverified is *breadth*, not direction: the checks have seen a few
tens of thousands of blocks out of 9.25 M. Every era and every peg direction has
been sampled, but most blocks have not been replayed.
