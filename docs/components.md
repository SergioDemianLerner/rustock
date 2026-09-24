# Major components

What was built in this repository between **2026-06-05 and 2026-09-24**: 319
commits, and the codebase grown from ~38,000 to ~98,600 lines of Rust with the
test count going from 819 to 1,535. Components marked *extended* existed in
outline before that window; the rest are new.

The organising constraint throughout is that rustock must agree with rskj
**bit for bit**, because a disagreement is a chain fork rather than a bug. That
is why so many of these components are either a port of a specific rskj class
or an instrument for proving equivalence with one.

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

## The two-way peg — Bridge precompile *(extended: 7,400 → 22,400 lines)*

The contract at `0x…01000006` that moves BTC to and from RSK: peg-in
registration with partial Merkle proofs, peg-out queueing and BTC transaction
building, federation governance and migration, the BTC header chain, the lock
whitelist, locking cap, and the fee market for peg-outs.

It is the largest and most consensus-critical component, and the one where
rskj's behaviour is least like anything documented — bitcoinj byte orders,
Java `HashMap` iteration order deciding which peg-out is paid, and a dozen
frozen bugs that have to be reproduced exactly because the chain's history
depends on them.

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

## JSON-RPC surface *(extended: 2,900 → 6,500 lines)*

The `eth_`, `net_`, `web3_`, `rsk_`, `txpool_`, `debug_` and `mnr_` namespaces,
shaped to rskj's output rather than go-ethereum's.

That distinction is the whole of the work. rskj's answers differ from geth's in
ways that look like typos and are not — bare hex where geth prefixes `0x`, an
array where geth has an object, JSON numbers where geth has hex strings — and a
client written against rskj silently reads nothing from a geth-shaped node.

## VM tracer

An opcode-level tracer in the shape of rskj's `DetailedProgramTrace`, running
through the same execution handler as a real block so it cannot describe a
different execution from the one the chain performed.

It is the foundation for `debug_traceTransaction` and the `trace_*` namespace:
the difference between knowing a transaction failed and knowing why. Structured
so that tracing provably does not change gas, output or logs.

## The rskj compatibility catalogue

Thirty documents recording, with rskj source citations, every place where
rustock had to reproduce something that is not in any specification: Java
artifacts (`HashMap` ordering, `BigInteger` sign bytes), deliberate RSK
divergences from Ethereum, frozen bugs, and rskj-versus-geth RPC differences.

A from-scratch client can read every line of rskj and still fork, because the
behaviour that matters is often an accident of the JVM rather than a decision
anyone wrote down. This catalogue is where each of those was found, proven
against mainnet, and written down so it is found once.
