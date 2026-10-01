# Syncing from rustock: rskj and rustock clients compared

**Status: in progress.** Written as the test runs, so that the environment is
recorded while it is still true rather than reconstructed afterwards.

Measures how long it takes to sync mainnet blocks 0 to 1,000,000 from a rustock
server, for an rskj client and a rustock client, by full sync and by snapshot
sync. Issues #233 (the server arguments), #234 (the rskj environment), #235
(the comparison itself).

---

## 1. What is being compared, and what is not

**Compared:** wall-clock time for a client to reach an executed head of
#1,000,000, from an empty data directory, against the same server.

**Not compared:** anything about the real network. Both clients are isolated to
a single peer, so this measures client and server implementation, not peer
quality, bandwidth or discovery.

A number on its own invites the wrong conclusion. If one client is faster it
matters whether that is execution, storage, or simply asking for more at once,
so the breakdown in §6 is part of the result rather than an appendix to it.

## 2. The machine

| | |
|---|---|
| CPU | 4 cores |
| RAM | 7 GB total, ~5 GB available |
| Kernel | 7.0.0-29-generic |
| `sda` | 76 GB SSD, root filesystem |
| `sdb` | 300 GB SSD, `/var/lib/rustock` — the production node's live database |
| `sdc` | 500 GB SSD, `/mnt/import` — bulk and scratch |

All three are non-rotational.

**4 cores and 7 GB is the most important constraint on this test.** A
production node runs on this host throughout, and both clients execute blocks,
which is CPU-bound. See §7.

## 3. The server

A rustock node serving from the production node's database, started with:

```
--read-only --follow-up false --simulate-height 1000000
```

- `--read-only` opens both databases read-only. Nothing the server does can
  write to the production node's data.
- `--follow-up false` is the instruction: serve what is held, adopt nothing.
  Without it the server would drift forward during a multi-hour run and the
  second client would face a different chain from the first.
- `--simulate-height 1000000` makes the server behave as though the chain ends
  at #1,000,000 — announcing that height, its hash, and **the total difficulty
  accumulated to it**, and refusing to serve any block above it.

Simulating the height is what makes "sync to 1M" the client's own decision. A
client told the chain ends there syncs there and reports itself finished; the
time to that point is the measurement. Nothing is stopped with a stopwatch.

The production database holds bodies across the range: blocks #1, #500,000 and
#1,000,000 all return transactions over RPC before the test begins.

## 4. Isolation

Both directions are closed, or the measurement is contaminated:

- rustock server and client: `--closed-network` (never learn node ids from
  peers, never dial them) plus `--bootnodes` naming only the counterpart
- rskj: discovery disabled, `peer.active` naming only the rustock server, no
  bootstrap list

A node that silently picks up a mainnet peer syncs from it instead, and the
timing then measures the internet.

**`--closed-network` alone is not isolation.** Found while setting this up: a
server started with `--closed-network` and no `--bootnodes` connected to **18
mainnet peers** within thirty seconds, having made 141 dial attempts. The flag
stops a node *learning* node ids from its peers; it does not stop it using the
chain's built-in bootstrap list, which is what it falls back to when
`--bootnodes` is empty.

Both are needed, and they answer different halves: `--bootnodes` replaces the
list of who to dial, `--closed-network` stops that list growing. The flag's own
help says the second half — "this does not by itself keep a node off the public
network: peers found through the one bootstrap address can introduce others" —
but not the first.

The server now points `--bootnodes` at the rskj node, so the only address it
knows is the one it is meant to talk to.

**Verified before each run, and recorded in §6:** each node has exactly one
peer and it is the intended one; the production node's peer count is unchanged.

## 5. Procedure

(To be filled in as each run completes.)

## 6. Results

(To be filled in.)

## 7. What could make these numbers misleading

Recorded before the runs, so that the list is not shaped by the results.

**The production node shares the machine.** It executes blocks, serves peers
and writes to `sdb` throughout. On 4 cores this is real contention, and it is
not constant: the production node's load varies with the chain. Mitigation:
record the production node's activity across each run rather than assume it is
negligible.

**The server reads the production database.** Read-only, so it cannot corrupt
it, but the reads compete with the production node's own on the same device.

**Page cache.** 7 GB of RAM against a database far larger. The first client to
run warms the cache for the second. Mitigation: run order is recorded, and if
the difference is close, the runs are repeated in the opposite order.

**Client and server on one host.** No network latency between them, which
flatters both clients equally but makes the absolute numbers unrepresentative
of a real sync. The comparison is still valid; the absolute figures are not a
forecast of mainnet sync time.

**Different storage engines.** rskj writes blocks, receipts, blooms, stateRoots
and an archival unitrie; rustock writes its own layout and, with the epoch
backend, collects historical state. These are not the same amount of work, and
a time difference partly reflects that. This is a genuine difference between
the clients rather than an artefact, but it should be named rather than read as
"one is faster".

**Snapshot sync may not be testable from this server.** The production node
runs the epoch trie backend with garbage collection, so historical state below
the burial window is reclaimed. A snapshot server offers a state 10,000–15,000
blocks behind its head; at a simulated head of #1,000,000 that state is long
collected. If so, the snapshot comparison needs a different server database —
noted here rather than discovered mid-run.

**JVM warm-up.** rskj is JIT-compiled; a run of this length should amortise it,
but it is not zero at the start.

## 8. Disk

Estimated 15–25 GB for both clients together, from two anchors: a part-synced
rustock directory covering #1–2,283,205 measured 27 GB, and rskj's archival
unitrie (130 GB at 9.2M blocks) is dominated by recent state this range does not
have.

Available at the start: 65 GB free on `/mnt/import`, with ~20 GB more
reclaimable from a previous test node. Actual usage is recorded in §6 rather
than left as an estimate.
