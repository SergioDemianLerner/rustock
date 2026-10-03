# Full sync from rustock: rskj and rustock clients compared

Measures how long it takes an rskj client and a rustock client to **full sync**
mainnet blocks from the same rustock server, executing every block from
genesis. Issues #233 (the server arguments), #234 (the rskj environment), #235
(the comparison itself).

Snapshot sync between the two is a separate exercise with separate results; see
`cross-client-snap-sync.md`.

---

## 1. What is being compared, and what is not

**Compared:** wall-clock time for a client to full sync to an executed head,
from an empty data directory, against the same server -- every block downloaded
and executed, no snapshot involved.

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

One client at a time, against the same server, with **the production node
stopped** for the duration. Production shares four cores and the `sdb` device
the server reads from, and leaving it running would have put an uncontrolled
variable in the middle of the measurement.

```
systemctl stop rustock                 # production, 19:45:29 UTC
./server.sh                            # --read-only --follow-up false
                                       # --simulate-height 10000
<client>                               # rskj, then rustock
systemctl start rustock                # 20:05 UTC, ~20 minutes down
```

The target was reduced from #1,000,000 to **#10,000** to get a first comparison
in an hour rather than a day. At 10,000 blocks the chain is nearly empty, which
is a real limit on what the result means — see §7.

## 6. Results

### Full sync to #10,000

| | rskj 9.1.0-SNAPSHOT | rustock |
|---|---|---|
| wall clock | **725 s** (12.1 min) | **65 s** |
| rate | 13.8 blocks/s | 153.8 blocks/s |
| database | 0.06 GB | 0.15 GB (0.07 data + 0.08 trie) |
| peers | 1 | 1 |

**rustock was 11.2x faster over this range.**

### Both reached the same chain, verified

A speed difference means nothing if the clients did different work. Both
finished at block #10,000 with:

```
hash       0x5147931463d6f2e6713ef44d2553a123aee54ed1184c5ecfabe54cec351d2c4e
stateRoot  0x83ff52f9edc4e931f548ee1064d4a393c6a2c4032127f0f18bce579efb7be680
```

The state root is the part that matters: it is what execution produces, so
matching it means rustock ran the blocks rather than trusting them. Confirmed
by reopening the client's database afterwards and asking it directly, not by
reading its own log -- a `Processed N blocks, state root:` line in rustock's
output reports a different value, which is a misleading log message and is
noted as such rather than taken as evidence either way.

### Conditions during the runs

| | rskj | rustock |
|---|---|---|
| started | 19:45:51 UTC | 20:02:38 UTC |
| server binary | **without** #237 | with #237 |
| production node | stopped | stopped |
| page cache | cold for the server's reads | warmed by the rskj run |

### Run 2: reversed order, cold cache first

The first run had rustock going second, with the page cache warmed by rskj. To
settle whether that explained the gap, the whole thing was repeated with the
order swapped and the page cache dropped first, so the *first* client faced a
genuinely cold cache. Both runs this time also faced the same server binary,
removing the other asymmetry.

| client | cold cache | warm cache | cache is worth |
|---|---|---|---|
| rustock | **80 s** | **65 s** | 19% |
| rskj | **725 s** | **658 s** | 9% |

Like for like:

| | ratio |
|---|---|
| both cold | **9.1x** |
| both warm | **10.1x** |
| rustock cold vs rskj warm (worst case for rustock) | **8.2x** |

**The ordering advantage was real and small.** Page cache is worth 15 s to
rustock and 67 s to rskj; the gap to explain was 645 s. Whichever way the order
runs, rustock is between eight and eleven times faster over this range.

The more interesting number is that the warm cache barely helps rskj -- 9%,
against 19% for rustock. Its bottleneck over this range is not reading from the
server.

### What this does not establish

**10,000 blocks of early chain is not a representative sample.** These blocks
are nearly empty; the comparison is dominated by per-block overhead rather than
by executing transactions. A node syncing the real chain spends most of its
time on blocks far heavier than these, and nothing here predicts that.

~~**rustock ran second, with a warm page cache.**~~ **Settled by run 2.** Cache
is worth 19% to rustock and 9% to rskj, against a gap of roughly ten times.
Reversing the order moves the ratio from 11.2x to 9.1x and changes nothing
about the conclusion.

~~**The rskj run used a server without the status fix (#237).**~~ **Removed in
run 2**, where both clients faced the same server binary.

**Neither client was tuned.** rskj ran with `-Xmx3g` on a 7 GB machine and
otherwise stock settings; rustock ran with its defaults. Someone who knows
either client well could likely move these numbers, and nothing here is a
statement about the best each can do.

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
