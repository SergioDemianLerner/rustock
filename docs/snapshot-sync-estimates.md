# How long should a snapshot sync take?

**A prediction, made before the fact, so that it can be wrong.**

Written 2026-09-27, against mainnet at #9,274,938. Nobody has yet run a
rustock snapshot sync end to end. What follows is assembled from parts that
*were* measured, with the arithmetic joining them shown, so that when a real
sync is run the answer is either "close" or "here is exactly which term was
wrong". A number recorded only after the event teaches nothing.

The bottom line: **about 30 minutes to an hour and a half** for a node starting
from nothing to reach the tip and follow it, against **days** for a full sync.

---

## 1. What has to happen

| phase | what it costs |
|---|---|
| Header chain, genesis → checkpoint | 9.27M headers downloaded and verified |
| State at the checkpoint | ~0.94 GB, 9,400 cells of 100 KB |
| Blocks from the checkpoint to the tip | 6,000 blocks downloaded and executed |
| *then* canonical index backfill | 415 MB, background, non-blocking |

The first three gate "following the tip". The fourth does not: the node is
already in consensus and following before it starts, and what it unlocks is
RPC about old heights and the ability to serve history back to the network.

## 2. The header chain: 10–20 minutes

**Verification is not the constraint.** `examples/header_verify_bench`
measures `HeaderVerifier::default_rsk` — every static rule, every parent rule,
merged-mining proof of work included — at **136,000 headers/s**. That is
**1.1 minutes of one core** for all 9.27M.

That number looked too good, so the bench carries a negative control: it
tampers with a header's merged-mining field and requires rejection
(`BitcoinPowInvalid`). The rule runs. Checking merged-mining proof of work is
a double-SHA256 over an 80-byte Bitcoin header plus a Merkle path — mining is
what costs, not verifying.

**Bytes are the constraint, and there are more of them than expected.**
Sampled at five points across the chain:

| height | bytes/header |
|---|---|
| #1,000,000 | 1,060 |
| #3,000,000 | 1,122 |
| #5,000,000 | 1,061 |
| #7,000,000 | 1,094 |
| #9,000,000 | 1,117 |

Strikingly flat, and it makes the header chain **~10 GB on the wire** — an
order of magnitude more than the state. *Snapshot sync is mostly a header
download.* (`snap/indexer.rs` quotes 5.5 GB; that is the on-disk size after
RocksDB compression, not what crosses the wire. Both are right about different
things.)

**The arithmetic.** 9.27M headers ÷ 192 per request = **48,300 requests**,
pipelined at `--snap-parallel` (default 8), so **~6,040 rounds**. Each round
moves 8 × 211 KB = **1.7 MB**.

A round takes whichever is slower, latency or bandwidth. At 50 ms RTT, staying
latency-bound needs 34 MB/s aggregate. Below that, bandwidth decides:

| aggregate throughput | round | header phase |
|---|---|---|
| 34 MB/s or better | 50 ms (latency-bound) | **5 min** |
| 16 MB/s | 105 ms | **11 min** |
| 8 MB/s | 210 ms | **21 min** |

**Prediction: 10–20 minutes.**

**What the parallel walk is worth.** The serial walk — ask 192, take the
oldest one's parent, ask again — is 48,300 *dependent* round trips. At 50 ms
that is 40 minutes; at 150 ms, two hours, before a single byte of state is
requested. That is rskj's shape, and it is the cost the skeleton removes.

## 3. The state: 2 minutes warm, 35 cold

0.94 GB in ~9,400 cells of 100 KB, also at depth 8. Measured server cost per
cell (#9,272,510):

| serving path | per cell |
|---|---|
| from cache | 0.46 ms + one read |
| warm traversal | 29 ms |
| cold traversal | **1.73 s** |

Whether the *server* has computed that cell before is what decides this phase:

- **Servers with warm caches:** round-trip bound. 9,400 ÷ 8 × ~60 ms ≈ **2 min**.
- **Every cell built cold:** 9,400 ÷ 8 × 1.73 s ≈ **34 min**.

The fixed offset grid is what makes the warm case reachable at all: every
client in a 5,000-block window wants the same cells, so the first client pays
and the rest do not. The per-peer rate limit (8 MB/s) is not binding — at depth
8 the whole state is ~2 minutes of one peer's allowance.

**Prediction: 2 minutes if the servers have served before, up to 35 if you are
the first client they have ever seen.**

## 4. The last 6,000 blocks: 15–45 minutes

Downloaded and executed normally. This node executes a tip block in
**0.1–0.2 s** wall clock — but against a warm state, and a freshly
snap-synced node's state is entirely cold, every read a miss.

**Prediction: 15 min if execution stays near tip speed, 45 if cold state costs
3×. This is the least certain term**, and the one most worth instrumenting
first. `replay_bench` could pin it down but currently expects a plain trie
store, not the epoch backend.

## 5. Total, and the background tail

| | best | worst |
|---|---|---|
| headers | 10 min | 20 min |
| state | 2 min | 35 min |
| 6,000 blocks | 15 min | 45 min |
| **to following the tip** | **~27 min** | **~1 h 40** |

Then, already following and yielding to the block loop: the canonical index
backfill, **415 MB at 1,770 headers/s cold ≈ 1.5 hours**. It records a cursor
before each pause, so a node that restarts daily still converges.

Against a full sync — executing 9.27 million blocks — which is days.

## 6. How to check this

Run a node from an empty data directory with `--snap-sync`, against peers that
have `--snap-server`, and record:

1. wall clock from start to the first `entering follow mode`;
2. the split between the header phase, the state phase and the 6,000 blocks;
3. whether the servers' caches were warm (were you the first client?);
4. aggregate download throughput during the header phase;
5. `--snap-parallel`, which the analysis above says should matter more than
   anything else available to tune.

The most likely way this document is wrong: **§4**, because it extrapolates
warm-state execution to cold. The most likely thing to be right: **§2**, since
both of its terms are measured and the arithmetic between them is short.

If §2 is wrong, the interesting question is whether peers actually serve
headers at the rate assumed — nothing here measures a real peer, only this
node's own costs.
