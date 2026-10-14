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
rskj's own pre-state:

```
verify_range <data-dir> <rskj-unitrie> <from> <to> --record-silent <store-dir>
scan_silent  <store-dir> [kind] [--from N] [--to N] [--coverage]
```

Two properties are load-bearing:

- **Only verified blocks contribute.** A block whose state root disagrees with
  the chain was executed by this implementation and not by the network, so its
  decisions are ours, not the chain's. Those blocks become recorded *gaps*,
  with the reason they failed.
- **Coverage is stored, so silence is not mistaken for absence.** `scan_silent`
  says up front whether the range it was asked about was ever replayed. "No
  refunds in #1..#2,392,699" and "nobody has looked" are different answers.

Each coverage span records the build that produced it. A replay is only as good
as the rules that ran it, and that is the only way to tell later which rows came
from which code.

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

## What it reproduced

Replaying the two known refund blocks recovers both, with the fields the event
log never carried:

```
#2745770 tx[1]  PEGIN_CAP_SURPASSED           20.00000000 BTC  v0  P2PKH
                refund 1BAfwW2h4dqyY5zGEMPZL48nUBwekAghg3  (P2PKH)   logged: false
#2851909 tx[2]  LEGACY_PEGIN_MULTISIG_SENDER  19.70976990 BTC  v0  P2SH-MULTISIG
                refund 35JUi1FxabGdhygLhnNUEFG4AgvpNMgxK1  (P2SH)    logged: false
```

These agree with the independent event-based analysis in
`mainnet-pegin-refunds.md` on every field, derived here from execution rather
than from pairing `release_requested` against `registerBtcTransaction`.
