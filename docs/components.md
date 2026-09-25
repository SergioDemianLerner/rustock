# Major components

What was built in this repository between **2026-06-05 and 2026-09-25**: 474
commits, and the codebase grown from ~38,000 to ~105,100 lines of Rust with
the test count going from 819 to 1,591. Components marked *extended* existed
in outline before that window; the rest are new.

The organising constraint throughout is that rustock must agree with rskj
**bit for bit**, because a disagreement is a chain fork rather than a bug. That
is why so many of these components are either a port of a specific rskj class
or an instrument for proving equivalence with one.

If you know rskj and want to find the file that answers to a class you already
know, [docs/rskj-to-rustock-map.md](rskj-to-rustock-map.md) maps the two
codebases package by package, and lists what each side has that the other does
not.

---

## rskj database import

Reads a running rskj node's RocksDB — unitrie state, blocks, receipts — and
converts it into rustock's own column-family layout, resuming if interrupted.

Without it a new node would have to execute 9.2 million blocks from genesis
before it could do anything. With it, the node starts from mainnet state, which
is what made every other component testable against real data rather than
fixtures.

## Whole-chain replay

Re-executes the entire mainnet history block by block and compares each
resulting state root against the one in the header.

This is the instrument the rest of the work depends on. It does not add a
feature; it turns "we believe this matches rskj" into a measurement, and it
found consensus bugs in the Bridge that no unit test would have — including
ones where rustock and rskj agreed for millions of blocks and then diverged at
a single transaction.

## The two-way peg — Bridge precompile *(extended: 7,400 → 22,700 lines)*

The contract at `0x…01000006` that moves BTC to and from RSK: peg-in
registration with partial Merkle proofs, peg-out queueing and BTC transaction
building, federation governance and migration, the BTC header chain, the lock
whitelist, locking cap, and the fee market for peg-outs.

It is the largest and most consensus-critical component, and the one where
rskj's behaviour is least like anything documented — bitcoinj byte orders,
Java `HashMap` iteration order deciding which peg-out is paid, and a dozen
frozen bugs that have to be reproduced exactly because the chain's history
depends on them.

**All 70 methods in the table now have a dispatch arm.** Twenty-two of them
used to fall through a catch-all that answered with empty bytes — which
decodes as zero, or as the empty string, rather than failing. A federator
client asking `getStateForBtcReleaseClient` got a plausible-looking nothing.
A test over the dispatch source now fails when a row is added without an arm,
so "unimplemented" has to be written down rather than left as a silent gap.

## Merged mining

Builds block templates, serves the `mnr_*` RPC that mining software speaks,
validates and imports submitted solutions, selects uncles, and handles the
Bitcoin coinbase, Merkle proof and RSKIP110 fork-detection data.

Without it the node can follow the chain but never contribute to it. Merged
mining is how RSK inherits Bitcoin's hashpower, so this is the difference
between an observer and a participant.

## Unitrie storage and epoch garbage collection

RSK's single trie for accounts, code and storage, plus a mark-and-sweep
collector that works over rotating epochs rather than the whole store.

State grows without bound otherwise: every historical state is retained, while
only a recent window is reachable. The epoch design lets the collector mark
from one deep root and sweep whole epochs, which is what makes the collection
affordable on a 130 GB store.

## Segmented trie tooling

Splits the trie into self-contained chunks that can be distributed and read
independently, with tooling to build, verify and repair them.

Useful for analysis and distribution — and the exercise that established a
precise limit worth knowing: the chunks are self-contained for *rustock's own*
read set, and a foreign client with a different access pattern will hit missing
nodes.

## Sync: the position layer, coherence invariants and the Φ watchdog *(rebuilt)*

Three storage pointers — head, executed head, canonical index — unified behind
a single transition type that writes them in one batch; eight stated invariants
(I1–I8) checked continuously; and a progress measure Φ that detects a node that
believes it is syncing and is not.

Nearly every outage in this node's history was two of those three pointers
disagreeing, and they used to be written from a dozen places. The redesign makes
the incoherent states unrepresentable rather than merely unlikely, and the
rollback decision is a pure function simulated over thousands of generated
chains and forks.

**Four of the eight invariants are now repaired, not only reported**, and that
change was earned rather than designed. The module was built to make
disagreements *loud*; on 2026-09-24 a one-block sibling fork at the tip left
execution on the losing side and I5 named it correctly every thirty-five
seconds for twelve minutes while nothing consumed the report. An invariant
that is only ever printed is a diagnostic. In production the repair now fires
several times an hour on ordinary tip forks, almost always one block deep —
and the violation is never logged at all, because it is fixed before it is old
enough to be worth reporting.

## Transaction pool and account rate limiter

A mempool with nonce-gap handling and pending/queued separation, plus a port of
rskj's `TxQuotaChecker`: each account accumulates "virtual gas" and pays a cost
computed from six factors when it broadcasts or replaces a transaction.

The limiter is what stops one account flooding the shared mempool by
broadcast-then-replace. It is consensus-adjacent rather than consensus-critical,
but a node without it is trivially degraded by a single peer.

## Gas price tracker

Ports rskj's `GasPriceTracker`: a percentile over the last 512 transactions'
gas prices, floored at the block minimum × 1.1, with a 50-block window deciding
whether the fee market is "working".

It answers two questions the node previously got wrong. `eth_gasPrice` was
reporting the *validity floor* rather than the price to pay for inclusion, so a
wallet trusting it during congestion would underpay; and the rate limiter's
low-gas-price factor was pinned to 1 instead of ranging to 4, making a
floor-priced flood four times cheaper than rskj allows.

## Supply conservation

Checks, per block and per transaction, that execution did not create rBTC from
nothing, and rejects the block if it did.

RSK's supply is bounded by the BTC locked in the peg. A bug that mints rBTC is
the worst failure this node could have, and it would otherwise be invisible —
state roots would agree with a rskj that had the same bug, and nothing else
looks at the total.

## Block validity rules

The eight rules rskj applies to every incoming block before executing it —
transaction gas-price bounds, the REMASC transaction, uncle constraints,
extra-data limits, fork-detection data — that rustock did not have.

These are the rules that *reject* a block. Whole-chain replay cannot exercise
them, because mainnet contains no invalid block: the only way to have them is
to write them from rskj's source and test them directly.

## Block pruning

Deletes headers, bodies, receipts and transaction-index entries below a
retention floor, in one batch per block so the store is never half-pruned.

Full history is not needed to follow the chain, and disk is the binding
constraint on a node that also holds a 130 GB trie.

## Bridge event index

Indexes the events the Bridge emits — peg-ins, peg-out requests, releases,
federation changes — so peg activity can be queried by height and type rather
than by re-scanning receipts.

Peg activity is the thing operators most need to see, and it is the part
hardest to reconstruct after the fact.

## Peg-out monitoring and alerting

Watches the Bridge for peg-outs above a threshold, reports the total value in
transit on a schedule, and emails alerts.

A peg-out is the only operation on this chain where a bug loses real BTC. This
is the component that means a human finds out within minutes rather than from
someone else's incident report.

## JSON-RPC surface *(extended: 2,900 → 8,100 lines)*

The `eth_`, `net_`, `web3_`, `rsk_`, `txpool_`, `debug_`, `trace_`, `sco_` and
`mnr_` namespaces, shaped to rskj's output rather than go-ethereum's.

That distinction is the whole of the work. rskj's answers differ from geth's in
ways that look like typos and are not — bare hex where geth prefixes `0x`, an
array where geth has an object, JSON numbers where geth has hex strings — and a
client written against rskj silently reads nothing from a geth-shaped node.

## Transaction tracing — the `debug_` and `trace_` namespaces

One inspector, rendered two ways. `debug_traceTransaction` and the block
variants give the opcode stream in the shape of rskj's `DetailedProgramTrace`;
`trace_transaction`, `trace_block`, `trace_get` and `trace_filter` give the
call tree — what an explorer shows as internal transactions.

Tracing a historical transaction is harder than it sounds, because its
pre-state does not exist anywhere: only per-block roots are stored, so the
whole block is re-executed from its parent with the tracer switched on at one
index. The consensus block executor is *shared* rather than copied for this,
which is the load-bearing decision — two copies of that loop would drift, and
this codebase has already had that failure twice in its orphan-recovery paths.

Both namespaces come from the same inspector so they cannot disagree about the
same transaction. The rest of the work is rskj's shape, which is full of
things a client written against Parity will get wrong: **a call to the Bridge
never appears in a call tree at all** (rskj emits no subtrace for a
precompile), a failed CREATE vanishes with its whole subtree, `trace_get`
indexes the block rather than walking the transaction, and `trace_filter`
matches whole transactions rather than individual traces.

## WebSocket subscriptions

`eth_subscribe` over a WebSocket on its own port: `newHeads`, `logs` and
`newPendingTransactions`, fed by one typed chain-event channel that the sync
service publishes to as it follows the tip.

Without it a client has to poll, which is slower and — for logs — lossy
across a reorg, exactly where it most needs to be told. The care is in the
retraction: a reorg re-sends the abandoned branch's logs with `removed: true`
before the replacements arrive, because this node sees a tip fork several
times an hour and a subscriber that never heard about one would accumulate
logs from blocks no longer on the chain.

## Peer scoring, punishment and banning

A port of rskj's `co.rsk.scoring`: fifteen event counters per peer, kept by
node id **and** by address, a punishment whose length grows on repeat
offences, and address/CIDR bans that persist across restarts — with the
`sco_*` namespace to inspect and override it all.

It was the one defensive gap against rskj. Before it, a peer feeding bad
headers or flooding the mempool was sidelined for minutes and then welcomed
back, with no operator control and no visibility. The care in it is aimed at
the failure nobody notices: an honest-but-unreliable peer must never be
punished, because a node that excludes peers rskj keeps ends up partitioned.

## Read-only diagnostics

Thirty-eight command-line tools over a live database, seventeen of which open
it **read-only** — no lock, no WAL replay, no writes, so they can be pointed
at the production store while it is running or wedged. `resume_point` answers
"where would execution resume" by applying the very function the node applies;
`height_peek` lists every block stored at a height, canonical or not;
`diff_roots` and `diff_state` locate where two state roots disagree.

They exist because a wedged node is a bad place to learn things. On
2026-09-24 the node was stopped mid-stall and both fixes were checked against
the frozen database before any code was deployed — `resume_point` said
"resume at #9,268,362, one block re-executed", which is exactly what the fix
does, and `height_peek` turned up two facts live RPC probing had missed. The
alternative is inferring the store's state from log lines, which is how the
same day's first investigation went to the wrong place entirely.

## The rskj compatibility catalogue

Thirty-three documents recording, with rskj source citations, every place
where rustock had to reproduce something that is not in any specification:
Java artifacts (`HashMap` ordering, `BigInteger` sign bytes, `RskAddress`
rendering the zero address as `null`), deliberate RSK divergences from
Ethereum, frozen bugs, and rskj-versus-geth RPC differences.

A from-scratch client can read every line of rskj and still fork, because the
behaviour that matters is often an accident of the JVM rather than a decision
anyone wrote down. This catalogue is where each of those was found, proven
against mainnet, and written down so it is found once.
