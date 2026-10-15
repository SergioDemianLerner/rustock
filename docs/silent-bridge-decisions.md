# Bridge decisions that emit no event

The Bridge's event log is the usual way to ask what the peg has done, and it
has two limits that compound. It does not log every decision, and what it does
log arrived late: `lock_btc` and `release_requested` at papyrus200 (#2,392,700
on mainnet), `rejected_pegin` and `unrefundable_pegin` at iris300 (#3,614,800).

Mainnet's only two peg-in refunds happened at **#2,745,770** and **#2,851,909**,
between those two activations. `release_requested` places them; nothing in the
log says they were refunds, why, where the money went, or what kind of address
it went to. An event-only search for refunds returns zero, which is true of the
index and false of the chain.

This is what the silent-decision store is for: a replay records the decisions
as it makes them, so the replay is paid for once rather than per question.

## Using it

Recording rides on `verify_range`, which already executes every block against
a trusted pre-state:

```
verify_range <data-dir> <state-dir> <from> <to> --record-silent <store-dir>
scan_silent  <store-dir> [kind] [--from N] [--to N] [--coverage] [--account]
```

Two properties are load-bearing:

- **Only verified blocks contribute.** A block whose state root disagrees with
  the chain was executed by this implementation and not by the network, so its
  decisions are ours, not the chain's. Those blocks become recorded *gaps*,
  with the reason they failed, and a gap is cleared if the block is verified
  later (`scan_silent --prune-gaps` sweeps a store written before that was so).
- **Coverage is stored, so silence is not mistaken for absence.** `scan_silent`
  says up front whether the range it was asked about was ever replayed. "No
  refunds in #1..#2,392,699" and "nobody has looked" are different answers.

Each coverage span records the build that produced it. Set `RUSTOCK_BUILD` or
run from anywhere — the build is read from the crate's own git directory, not
the current one.

## Regenerating it

The store is **derived**: delete it and everything below rebuilds it. What it
costs is dominated by how many blocks must be replayed, so the whole method is
to narrow that set first.

### 1. Find the blocks worth replaying

Replaying every block is never necessary. Two ways to narrow:

```sh
# From the event index, where the decision leaves an event (arrowhead600 on):
bridge_events /var/lib/rustock 'rejected_pegin(bytes32,int256)' \
  | grep -oE '#[0-9]+' | tr -d '#' | sort -nu > blocks.txt      # 33,447 blocks

# From the transactions, where it does not (anything, any era):
scan_pegin_calls /var/lib/rustock <from> <to> --list-blocks > blocks.txt
```

`scan_pegin_calls` reads blocks without executing them — ~900 blocks/s on the
early chain. **It sees only top-level calls**: `registerBtcTransaction` is
`TransactionCallable` with no `isContractTx` restriction, so a contract may
call it and that call is invisible without execution. Flyover peg-ins are
*always* internal (`registerFlyoverBtcTransaction` requires `call_depth > 1`),
so they are never on such a list. Use it to narrow, never to conclude.

### 2. Replay them, recording

State source by era — this is the part that is easy to get wrong:

| era | source | extra |
|---|---|---|
| ≥ wasabi100 (#1,591,000) | rskj unitrie, or `/mnt/import/rustock-trie` | `--state-format rustock` for the latter |
| < wasabi100 | rskj unitrie | **`--state-roots /mnt/import/rskj-snapshot/stateRoots`** |

Below wasabi100 a header's `stateRoot` is an **orchid** root and names no
unitrie node, so without the translation table every block is unverifiable and
looks pruned. See `mainnet-pegin-refunds.md` for the two blind alleys this
cost, and do not re-walk them.

```sh
verify_range /var/lib/rustock /mnt/import/rskj-snapshot/unitrie \
    --blocks blocks.txt --network-id 30 \
    --state-roots /mnt/import/rskj-snapshot/stateRoots \
    --cache-mb 384 --max-failures 0 \
    --scratch <scratch-dir> --record-silent <store-dir>
```

Operational notes, each learned the hard way:

- **Chunk long runs.** The scratch trie grows ~70 KB/block and is deleted
  between chunks; `/home/paseo/silent-run/run.sh` does this in 1,000-block
  chunks with a `.done` marker each, so an interruption costs one chunk. It
  also waits for free disk rather than starting work it cannot finish.
- **Keep `--cache-mb` modest** (384-1024). The box has 7.7 GB and runs the
  production node.
- **A full-disk write failure aborts the run** rather than producing wrong
  roots quietly — but do not rely on that; watch the free space.

### 3. Read it

```sh
scan_silent <store-dir> --account            # value never returned, by reason
scan_silent <store-dir> pegin_refunded       # every refund, with its address
scan_silent <store-dir> --coverage           # what has been replayed, by which build
```

`--account` totals over **distinct `btcTxHash`**, not over records. A rejected
peg-in is marked processed only under `RSKIP459 && !RSKIP551`, so outside that
window the same bitcoin transaction can be resubmitted indefinitely and is
rejected afresh each time. On mainnet 37,268 rejection events are **9**
transactions; counting events overstates the value lost ~350-fold.

### What the store currently holds

| range | blocks | how |
|---|---|---|
| #6,916 – #2,391,061 | 129 | every pre-papyrus200 peg-in block |
| #6,223,964 – #8,488,233 | 33,447 | every block with a `rejected_pegin` event |

Everything else is uncovered, and `scan_silent` says so for any range touching
it.

## Recording never changes execution

Records are derived, never read back, and deleting the store loses no state.
The sink is off unless a tool turns it on, so an ordinary block pays one
thread-local read per decision point.

The sharp edge is that some Bridge *checks write*. `verifyLockDoesNotSurpassLockingCap`
lazily initializes the locking-cap cell, and before RSKIP181 rskj does not
re-evaluate it in the rejection branch at all. A recorder that called the real
check to name the rejection reason would write a cell the chain never wrote and
change the state root. So the pre-RSKIP181 reason comes from
`locking_cap_would_allow`, which reads the same inputs and decides the same way
without the write; where the chain *does* evaluate the cap, the record reads the
reason off that evaluation rather than repeating it.

`recording_does_not_change_what_is_written` pins this for the whitelist consumer
— the one hook inside a state-mutating function — and a `--record-silent` run
over #2,745,768..=#2,745,772 verifies clean, which is the end-to-end form of the
same check: if recording wrote anything, that block would diverge.

## Captured

| kind | when | what the chain logged |
|---|---|---|
| `pegin_refunded` | v0 rejected by the whitelist, the locking cap or a multisig sender; v1 rejected by the cap; v1 with a malformed RSKT payload | `rejected_pegin` from RSKIP181 only, and never the refund address, sender type or peg-in version; a whitelist-only rejection logs nothing in any era |
| `pegin_rejected_no_refund` | amount below minimum; undetermined sender; sender type not processable before RSKIP143; v1 rejection with no refund address available | `rejected_pegin` + `unrefundable_pegin` from RSKIP379 only |
| `pegin_accepted` | a credited peg-in, v0 or v1 | `lock_btc` from RSKIP146, `pegin_btc` from RSKIP170 — neither carries the sender type or derived address |
| `refund_build_failed` | a refund was due but no transaction could be built, so nothing was returned | nothing, in any era (rskj logs a warn line) |
| `btc_tx_marked_processed` | a peg-out, migration, old-federation spend or SVP fund/spend tx was registered and marked | nothing, in any era |
| `whitelist_entry_consumed` | a one-off whitelist entry was spent by a peg-in | nothing, in any era |

Every record carries `btcTxHash`, `rskTxHash`, `amountSat` and `reason`, plus a
`logged` flag saying whether the chain also emitted an event for this decision —
which is what makes the gap between the two visible rather than inferred.

Reasons use rskj's own names (`PEGIN_CAP_SURPASSED`,
`LEGACY_PEGIN_MULTISIG_SENDER`, `LEGACY_PEGIN_UNDETERMINED_SENDER`,
`INVALID_AMOUNT`, `PEGIN_V1_INVALID_PAYLOAD`), with two of ours where rskj has
no name because it emits no event: `LOCK_WHITELIST_REJECTED` and
`SENDER_NOT_PROCESSABLE_PRE_RSKIP143`.

## Not captured

Deliberately:

- **`updateCollections`.** A periodic maintenance call whose decisions are made
  every few blocks for the life of the chain; recording them would bury
  everything else.

Not yet, and each would need the same care about writes as the cap check did:

- Peg-out request rejections (`releaseBtc` below minimum, contract-caller
  rules) — these mostly *do* have events from RSKIP185/RSKIP146.
- Pegout batching and confirmation-set movements beyond the processed mark.
- Federation governance: commits, rollbacks, SVP state transitions other than
  the registering transaction.
- Fee-per-kb and lock-whitelist administrative votes.
- `receiveHeaders` rejections (a BTC header refused by the chain rules).

## What it found

Nine peg-in refunds on mainnet, where the event log alone shows two:

```
#268,846 .. #1,923,835   7 refunds, 2.78799874 BTC   LOCK_WHITELIST_REJECTED
#2,745,770               1 refund, 20.00000000 BTC   PEGIN_CAP_SURPASSED
#2,851,909               1 refund, 19.70976990 BTC   LEGACY_PEGIN_MULTISIG_SENDER
```

The seven are invisible to any event-based method: a whitelist rejection emits
nothing, in any era. The two that are visible agree with the event-based
analysis on every field, derived here from execution rather than from pairing
`release_requested` against `registerBtcTransaction`.

And 0.06519234 BTC of peg-in value kept and never returned over
#6,223,964..#8,488,233 — 94% of it from peg-ins sent from native segwit
(bech32) addresses, whose sender rskj cannot classify, so no refund address can
be derived and the bitcoin stays with the federation.

Full analysis in `/home/paseo/doc/mainnet-pegin-refunds.md` (outside git).
