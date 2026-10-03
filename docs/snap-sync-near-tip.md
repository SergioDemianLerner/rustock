# Snapshot sync near the tip, rustock to rustock

A snapshot sync of RSK mainnet from a rustock server to a fresh rustock client,
run on 2026-10-02 against the chain head at #9,288,884. It is the first run in
which the header walk fetched the uncle headers alongside the trunk headers, so
the work behind the checkpoint was established exactly rather than bounded from
below.

## Result

| | |
|---|---|
| total | **7,545 s (2 h 05 m 45 s)**, 1,231 blocks/s |
| head reached | #9,288,884 |
| final state root | `0x9dcfe963533cb43c5176d116693d330b3cf1998ac05d0826d61197648cb15e20` |
| server's state root at that height | identical |

The client executed the blocks above the checkpoint and arrived at the same
state root the server holds, which is what makes the run a verification rather
than a transfer.

## Where the time went

| phase | duration | share |
|---|---|---|
| status exchange | 11 s | 0.1% |
| **header walk to genesis** | **6,971 s** | **92.4%** |
| state download, 875 MB | 158 s | 2.1% |
| blocks and execution | 405 s | 5.4% |

The walk dominates. It read 9,255,000 headers back to #0 at about 1,230
headers/s, verifying each header's own rules, each adjacent pair's, and the
cumulative work they represent. The state — the thing a snapshot sync exists to
avoid computing — took under three minutes.

That ratio is the finding. Downloading the state is cheap; proving the
checkpoint deserves to be trusted is what costs, and it costs roughly 44 times
as much.

## What was exercised

The walk requested headers with their referenced uncles over `rsk/63`
(`BLOCK_HEADERS_WITH_UNCLES_REQUEST`), which the server answered from block
bodies. In RSK, total difficulty advances by the trunk block's difficulty plus
every uncle's, and uncle headers travel only in the body, so a walk fed bare
headers can only compute a lower bound on the chain's work. With the uncles in
hand the figure matches what rskj computes.

Three checks were corrected to accept totals that count the uncles; before
them, a snapshot sync against a server whose totals were right failed five
times in a row and fell back to a full sync:

- the accumulation itself, which added only the header difficulty;
- `check_chain_shape`, which required the offered totals to grow by exactly the
  header difficulty;
- the checkpoint ceiling, which allowed the claim to exceed established work by
  only the checkpoint's own difficulty.

## Setup

- One machine, both nodes. Server: the production database opened **read-only**
  with `--follow-up false`, its own identity, `--snap-server`, offering the
  state at #9,275,000 (head − 10,000). Client: an empty database, `--snap-sync
  --exit-when-synced`.
- Isolated with `--closed-network` plus `--bootnodes` on both sides, so each
  spoke only to the other. `--closed-network` alone is not isolation; without
  bootnodes a node still finds mainnet peers.
- The production node was stopped for the duration.

Note that `--bootnodes host:port` names a peer's **discovery** port, which is
the listen port plus one. Pointing it at the listen port produces a node that
starts cleanly, logs nothing unusual, and never connects.

## Resource use

Measured from `/proc/<pid>/status`, so resident memory only — the kernel's page
cache for the databases sits outside it.

| | start of walk | 1 h into walk |
|---|---|---|
| client | 292 MB | 378 MB |
| server | 428 MB | 581 MB |

Client memory grew with the walk rather than staying flat, at roughly 100 MB
per 2.2 M headers. It was not sampled during the state download, so the peak is
not known; that is a gap in this run rather than a result.

Disk was the heavier cost. The client's database reached **19 GB**, about 3,200
bytes for each ~1,085-byte header, because the walk writes every header into
RocksDB and into the staging freezer, with indexes over both. Most of it is
scaffolding for a phase whose output is discarded once the chain is verified.

## What this does not establish

- One client, one server, one machine, one run. No competing traffic, no
  latency, no packet loss, and the two processes shared a disk.
- The server read a warm production database; a cold one would be slower.
- It says nothing about rskj. rskj speaks `rsk/62` and cannot be asked for
  headers with uncles, so a walk against it would still bound the work from
  below.
