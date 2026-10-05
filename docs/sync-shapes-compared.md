# Descending and ascending header sync, compared

Two ways to establish the header chain a snapshot sync rests on, both measured
against RSK mainnet. This records what each one cost and what each one proves,
and it is honest about which numbers are solid and which are not.

- **Descending** — `snap::headers`, the shipped default. Walks down from the
  checkpoint a peer offers until the links reach ground this node already has.
- **Ascending** — `snap::forward`, behind `--snap-forward-headers`. Walks up
  from ground this node already has to the checkpoint. See
  `docs/header-first-sync.md` for the design.

Method in `docs/snap-sync-test-method.md`. Both fetch the same descending
192-header chunks — the wire has no forward request — and both link them by
hash. What differs is which end is trusted.

## Results

| | descending | ascending |
|---|---|---|
| header phase to #9,285,000 | **6,971 s** measured | **6,799 s** extrapolated |
| state download, 875 MB | 158 s | **805 s** |
| blocks and execution | 405 s | ~120 s |
| peak RSS | ~300–400 MB | **208 MB** |
| bytes on the wire | 21.42 GB | 20.76 GB |
| total difficulty | **lower bound** | **exact** |
| resumable | no | **yes** |

Measured on the same host, against a server offering the same checkpoint
#9,285,000.

### The header figure is not a clean comparison

The descending 6,971 s is a single clean run (2026-10-01/02), but against a
*live* server, which is the flaw `snap-sync-test-method.md` exists to remove.

The ascending 6,799 s is **extrapolated** from the only stretch that ran
clean — 544,896 blocks in 399 s, 1,366 blocks/s — because the full run never
completed in one piece. It spanned three processes, two stalls and a
memory-pressured middle segment on a swapping host; its slow segment managed
479 blocks/s. The extrapolation assumes a rate the rest of the run never
sustained, so treat **0.975x as provisional and flattering to the ascent**.

What can be said: the two are within a few percent on a single peer, which is
what the design note predicted, since the argument for ascending was never
single-peer speed.

### State download is three times slower, and unexplained

158 s against 805 s for the same 875 MB from the same server. Both used eight
100,000-byte chunks in flight. The ascending run's server was `--read-only` and
the descending run's was live, which should favour the former. Not diagnosed.

## What ascending buys

**An exact total difficulty, rather than a lower bound.** RSK counts uncle
difficulty, and uncle headers travel only in bodies, so a trunk-header walk
cannot sum the chain's work. The descending walk adds `header.difficulty` and
counts what it skipped in `uncles_omitted`; on mainnet that understates by
about 48%. The ascent takes `BlockHeadersWithUncles` (RSKIP-698) and proves the
uncle work with three checks: `ommers_hash` binds the list to a header whose
proof of work covers it, each uncle carries its own merged-mining proof, and an
uncle already counted under another block is refused.

The run ended `header chain established to #9285000 by ascent: cumulative
difficulty 60991210088409103883734905416 (exact, uncles proven)`, and the state
root it then downloaded matched the server's byte for byte.

**Resumability.** A descending walk establishes nothing until its links reach
ground, so it stages everything and promotes at the end; interrupted, it starts
again. The ascent commits each run as it links. Demonstrated twice: interrupted
at #544,896 and resumed at #551,040, and resumed at #9,281,664 after an
out-of-memory kill five hours in.

**It removes the sampling gate's reason to exist.** With the chain established
from genesis upward, the offered checkpoint is checked by an index lookup
instead of a statistical bound. See `docs/difficulty-gate.md` for what that
machinery costs.

## What it costs

**Uncle headers.** 10.47 GB on mainnet, roughly doubling the header stream.
That is the price of proving the work rather than assuming it, and no header
field avoids it — see RSKIP-699's closure for why a committed total is an
assertion, not a proof.

**An `rsk/63` peer.** Against `rsk/62` — every rskj node today — the ascent
falls back to the descending walk rather than compute a lower bound and call it
exact.

**Maturity.** Three stalls were found by running it, all now fixed: unanswered
requests were never reissued, a target off the 192-block grid was unreachable,
and a single missing skeleton point stranded every run above it. The descending
walk has years of mainnet behind it; this has two runs.

## Following afterwards

After the ascending sync completed, the client ran 30 minutes on the public
network in ordinary full-sync mode: caught up from #9,296,448 to the live tip,
stayed within 1–5 blocks of it, executed across #9,299,107–#9,299,165, and
recorded **no hard stalls**. 18 reorgs and 3 pipeline restarts in that window,
which is the ordinary sibling-reorg churn of this chain and not specific to
either shape. Peak RSS 415 MB.

## How the ascent scales across peers

The argument for ascending was never single-peer speed: it was that a chunk is
permanently valuable as soon as it links, so wide fan-out is cheap to attempt.
That is now measured.

Method and harness: `docs/snap-sync-test-method.md` and
`tools/snap-scale-test/`. Each figure is a 180-second steady-state window on a
client ascending from genesis, three repeats per group, interleaved.

| servers | runs (blocks/s) | mean | sd | vs 1 | per server |
|---|---|---|---|---|---|
| 1 | 952, 998, 1,035 | 995 | 42 | 1.00x | 995 |
| 2 | 2,406, 2,437, 2,514 | 2,452 | 56 | **2.46x** | 1,226 |
| 3 | 3,549, 3,796, 3,715 | 3,687 | 126 | **3.71x** | 1,229 |
| 4 | 4,873, 4,974, 3,730 | 4,526 | 691 | **4.55x** | 1,131 |

It scales, and close to linearly: adding a fourth server still bought 23% over
three. At four servers the header phase would finish in roughly half an hour
rather than two.

### Why it scales, which is not the obvious reason

Not bandwidth, and not disk. Before building the rig the bottleneck was
measured directly:

- the whole exchange moves about **4 MB/s**, which is nothing;
- the client sits at **13-24% of one core**, the server at **~22%**;
- raising `--snap-parallel` from 8 to 32 -- four times the in-flight budget --
  changed the rate by **8%**.

What is actually serialised is the server. Sampling its threads shows **one
thread doing all the serving** at ~23%, every other thread near zero, and each
response taking ~116 ms of which only ~27 ms is CPU. The rest is blocking on
disk reads, one at a time.

So each additional server adds an independent serving pipeline. That is also
why several servers can share one disk without competing: they are bound by
I/O *latency*, not throughput, and their waits overlap.

### Where it stops

Per-server throughput is flat from two to three (1,226 then 1,229) and falls at
four (1,131), where the spread also widens sharply -- sd 691 against 56 and
126. That is the first sign of the four-core host contending, not of the
protocol running out. Four servers, four client connections and a client on
four cores is the limit of what this machine can say.

### What it implies

The single-threaded serving path is the thing worth fixing. It caps what any
one peer can give a client, descending or ascending, and it is the likeliest
explanation for the state download taking 805 s here against 158 s in the
earlier run. A client with four peers works around it; a client with one
cannot.

### Superlinear, and not explained

Two servers gave 2.46x and three gave 3.71x -- about 1.23x per server above
linear. The single-server baseline appears depressed rather than the multi-peer
cases inflated, most likely because one connection leaves gaps in the client's
request pipeline that further connections fill. That is a hypothesis, not a
measurement, and it is recorded as one.

## Verdict

Ascending is sound, completes end to end, and gives an exact total difficulty
and a resumable sync that descending cannot. Against a single peer it is not
yet proven faster, and the one clean number it has is an extrapolation.

Against several peers it is a different proposition: 4.55x on four servers,
which no amount of tuning gets from one. That is the case for the shape, and it
is now measured rather than argued.

What would settle it is a single uninterrupted run of each shape on an unloaded
host against the same frozen server. Neither has had that.

## Provenance

- descending: `docs/cross-client-snap-sync.md`, run of 2026-10-01/02
- ascending: issue #262, runs of 2026-10-04 and 2026-10-05
- fixes found by those runs: #263, #264, #265
