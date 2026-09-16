# Range-segmented trie store

**Status:** proposal, with measurements. Nothing implemented beyond the
measurement tool (`cargo run --example trie_footprint`).

## 1. The problem

Re-executing the chain to check that a change has not broken consensus
backwards is a read-bound job, and the reads are the worst kind: a trie descent
issues *dependent* random reads, so the device sees one outstanding request at a
time.

The store makes it worse. Trie nodes are content-addressed, so a block's working
set is scattered uniformly across the whole database. Against the 131 GB
archival store the live set is 0.87% of entries, and measured traversal
throughput tracks that density rather than the algorithm: 4,092 keys/s at 0.87%
against 118,893 keys/s at 100% on the same code and the same machine — a factor
of 29 (`trie-gc-results.md` §7.4).

A segmented store is a **reordering of the same data by block number instead of
by hash**. Each segment holds exactly the nodes some block range touches, so
within a segment density is ~100% and the working set is small enough to stay
resident.

## 2. What the snapshot already gives us

The imported rskj snapshot is **archival from block 1,591,000** (RSKIP126, where
RSK switched to the Unitrie). Verified by replaying blocks at #1,591,010,
#2,000,000, #2,500,000, #3,000,000, #5,000,000, #7,000,000 and #9,000,000: every
one found its parent's state root on disk and executed with no missing nodes.
#1,590,999 and below are not there — that era predates the Unitrie.

Two consequences:

1. **Every block's pre-state is already available**, so ranges can be replayed
   in any order and in parallel *today*, with no new database. Worker *j* reads
   the state root from the header at its range start and executes forward.
2. **The build pass need not be sequential.** Each worker can record its own
   range's footprint and seal its own segments. The original proposal assumed a
   serial first pass; it is not required for the Unitrie era.

Blocks 0–1,590,999 are a separate, smaller problem: the state must be built from
genesis, serially, and pre-RSKIP126 headers carry an Orchid-format state root,
checked by converting the unitrie (`RUSTOCK_ORCHID_CHECK_INTERVAL=K`). Reaching
#1,591,000 from genesis and finding the snapshot's state root would be a strong
end-to-end check of that era.

## 3. Design

Segments `D(1)..D(n)`, each tagged with the block number `B(i)` at which it
starts. `B(i)` is recorded in the store's metadata; segments partition the block
range covered.

**Build.** Replaying a range, every node read *or* written is recorded into the
segment under construction. When it exceeds `S` bytes it is sealed and a new one
started at the current block. A sealed segment therefore holds every node its
range touches.

**Read.** For block `k`, route to the `D(i)` with `B(i) <= k < B(i+1)`. A single
lookup — unlike the epoch store, which must probe newest-to-oldest because it
does not know where a key lives.

**Invariant.** A deterministic re-execution of `[B(i), B(i+1))` reads only keys
in `D(i)`. It holds because the build recorded exactly the reads that execution
performed, and execution is a function of the block data and the starting root.

**Fallback is still required.** The invariant is conditional on *the same code*.
A change that reads state it did not read before — a fixed precompile, a
different key mapping — will miss. The store must fall through to the archival
store rather than fail, and must count misses: a rising miss rate is the signal
that the segments no longer match the code. A change that alters what is
*computed* shows up as a state root mismatch, which is the result being looked
for, not an error.

## 4. Amendments to the original proposal

1. **Build in parallel.** §2 above.
2. **Do not use RocksDB for sealed segments.** A sealed segment is an immutable
   map from a 32-byte hash to a value: no compaction, no LSM levels, no
   memtable. Better: one file holding a flat open-addressing table keyed on the
   leading 8 bytes of the hash, plus the concatenated values, `mmap`ed. Because
   trie keys *are* hashes, they are already uniformly distributed, so the table
   needs no hash function and resolves in ~1 probe. Closing a segment is
   `munmap`, which is also how its cache is dropped.
3. **Write-once, in RAM.** Build a segment in a `HashMap` and flush it sealed.
   Copy-on-read into a live RocksDB turns every read into a write; building in
   memory writes each segment exactly once, sequentially.
4. **Read-only opens.** RocksDB allows one writer per directory and that writer
   holds an exclusive lock, so parallel workers cannot each open the archival
   store read-write. `BlockStore::open_read_only` and
   `RocksDbTrieStore::open_read_only` take no lock.

## 5. Integration

`TrieBackend` gains a fourth variant alongside `Single`, `External` and
`Epochs`, and `open_backend` a fourth arm returning `Arc<dyn TrieStore>`. Every
consumer already takes the trait object, so nothing else changes.

Shared with the epoch store: the multi-database lifecycle, the incremental
byte counter that triggers rotation, the read counters. Not shared: mark, drain,
sweep and rotate have no analogue — a segment store never reclaims anything.

`put` on a segment store in replay mode discards. A verification run must not
mutate the artifact it is verifying against.

## 6. Measurements

Method: `cargo run --release -p rustock-cli --example trie_footprint --
<block-dir> <trie-dir> <start> <count> [csv]`. It replays canonical blocks
through a wrapper that records every key read or written -- the set a segment
covering that range would have to hold -- and writes nothing. Blocks come from
the running node's store opened read-only; state comes from the detached
archival trie (`/mnt/import/rustock-trie`, 131 GB, 1,246,651,400 nodes).
Machine: 4 vCPU, 7 GB RAM, network-attached SSD, production node running
concurrently.

### 6.1 Footprint per block, by era

2,000 blocks at each of three heights. "W" is the intercept of a linear fit to
the second half of the curve: the part of the footprint that is *not* explained
by per-block churn, i.e. the shared working set a segment would duplicate.

| Era | Distinct nodes | Raw bytes | B/node | Nodes/block | KB/block | W |
|---|---|---|---|---|---|---|
| #2,000,000 | 112,239 | 11.6 MB | 103.3 | 54.3 | 5.60 | 0.39 MB |
| #5,000,000 | 261,543 | 28.3 MB | 108.2 | 136.4 | 14.39 | −0.47 MB |
| #9,200,000 | 287,349 | 31.8 MB | 110.8 | 148.1 | 15.63 | 0.56 MB |

**The curve is linear and the intercept is zero in every era** (±0.5 MB, which
is the noise floor at this sample size). There is no hot set that a segment
would have to re-import from its predecessors.

That is not obvious a priori, but it follows from what a trie write does: it
rewrites the path from the changed leaf to the root, and the nodes it *reads*
to do so are the previous version of that same path, written by a recent block
-- almost always inside the same segment. Cross-segment reads are the rare case
of state untouched for the whole span of a segment.

All 6,000 blocks replayed with **0 state root mismatches** and **0 missing
nodes**. All three ranges are below the import head #9,230,008, so they had
never been executed by rustock before.

### 6.2 Worker scaling

Replay issues *dependent* random reads -- each node's address is known only once
the previous read returns -- so one worker leaves the device almost idle.
Independent workers on disjoint ranges are the only way to raise queue depth.
200 blocks per worker, ranges 30,000 blocks apart in one era so that per-block
cost is comparable and no two workers share cache:

| Workers | Aggregate | Per worker |
|---|---|---|
| 1 | 7.69 blocks/s | 7.69 |
| 2 | 9.52 | 4.76 |
| 4 | 17.78 | 4.44 |
| 8 | **26.23** | 3.28 |
| 16 | 23.53 | 1.47 |

**3.4x on four cores, peaking at eight workers** -- confirming the job is
latency-bound, not CPU-bound. At 16 workers throughput fell and the kernel
OOM-killed two workers, which sets the memory ceiling on this machine.

An earlier version of this table was wrong and is worth recording: workers were
placed 600,000 blocks apart, so each larger configuration added *slower* heights
and the wall clock tracked the slowest worker rather than contention. It showed
7.02 blocks/s at four workers and 19.20 at twelve -- noise shaped like a result.

### 6.3 Segment-scale footprint

§6.1 measures 2,000-block ranges; a real segment spans ten to forty times that,
and whether `W` stays small over that span is what the disk budget rests on.
Three 20,000-block runs, one per era:

| Run | Nodes | Footprint | Nodes/block | KB/block | Fitted `W` | Rate |
|---|---|---|---|---|---|---|
| #2,500,000 | 1,299,003 | 133.3 MB | 67.4 | 6.90 | −4.7 MB | 10.53 blk/s |
| #5,000,000 | 2,240,249 | 231.0 MB | 107.2 | 10.95 | +12.0 MB | 5.72 blk/s |
| #9,150,000 | 2,985,485 | 309.4 MB | 163.7 | 16.82 | −27.0 MB | 4.52 blk/s |

`W` scatters between −27 MB and +12 MB across the three, which is the honest
reading: **the intercept is indistinguishable from zero at ±27 MB**, the noise
floor of a 20,000-block fit. It does not grow with segment length — a
20,000-block segment duplicates no more than a 2,000-block one. Taking the
worst case at face value (+12 MB per 20,000 blocks) puts duplication at 1--2% of
the total, which is the number the budget should carry.

The longer runs give *lower* per-block costs than the 2,000-block samples
(mid era 10.95 vs 14.39 KB/block). Short windows land on busy or quiet stretches;
the 20,000-block figures are the ones to use.

66,000 blocks have now replayed with **0 state root mismatches** and **0 missing
nodes**, across 67.8M trie reads. All are below the import head #9,230,008, so
none had ever been executed by rustock.

### 6.4 Projection over the Unitrie era

Per-era rates from §6.3 applied to #1,591,000..#9,230,008 (7,639,008 blocks):

| Span | Blocks | KB/block | Size |
|---|---|---|---|
| #1,591,000--#3,750,000 | 2,159,000 | 6.90 | 14.9 GB |
| #3,750,000--#7,000,000 | 3,250,000 | 10.95 | 35.6 GB |
| #7,000,000--#9,230,008 | 2,230,008 | 16.82 | 37.5 GB |

| | |
|---|---|
| Distinct nodes touched | **859M** — 69% of the archive's 1,247M |
| Raw key+value | **88.0 GB** |
| With a 12 B/entry hash index | **98.3 GB** |
| Single-worker replay against the archive | 14.7 days |
| Eight-worker replay against the archive | **4.3 days** |

The segmented store comes out *smaller* than the archive it is built from, by
about a third: 859M nodes against 1,247M. The 388M-node gap is not measured
here, and the likeliest explanation is that the archive holds nodes canonical
replay never touches — the state rskj wrote while executing competing branches,
and RSK forks constantly (1.105 uncles per block over the range sampled during
the follow-mode work). Worth confirming before relying on the smaller figure;
the safe planning number is the archive's own 131 GB.

### 6.5 Where the time actually goes

The estimate that resident segments make replay CPU-bound was worth testing
rather than asserting. Replaying the same 1,000-block range three times in a
row, so the second and third passes could be served from page cache:

| Pass | Rate |
|---|---|
| Cold | 2.87 blocks/s |
| Warm (immediate repeat) | 3.05 blocks/s |
| Warm (second repeat) | 2.87 blocks/s |

**Six percent.** The page cache buys nothing, and the reason is read
amplification. Measured directly, by sampling the process's block-device reads
over a complete 500-block replay:

| | |
|---|---|
| Useful trie data touched | 10.8 MB (86,340 nodes) |
| Read from the block device | **1,651 MB** |
| Read amplification | **152x** |

A scattered 131 GB store answers a 32-byte key by fetching the whole SST block
that contains it. 86,340 keys land in ~86,340 different blocks, so 10.8 MB of
nodes drags 1.65 GB through the device — and 1.65 GB per 500 blocks is far more
than a 7 GB machine can retain, which is why repeating the range does not help.

The same sample shows what the bottleneck is *not*. One worker uses **20% of one
core** and pulls **3.8 MB/s**, with the system 67% idle and 13--17% in I/O wait.
Neither CPU nor bandwidth is close to saturated: the job is bound by the latency
of dependent random reads, one at a time.

This is the real case for segmenting, and it is stronger than the density
argument in §1. A 512 MB segment spanning ~30,000 recent-era blocks costs 512 MB
of device reads once and then serves from RAM. The same 30,000 blocks against
the archive cost ~99 GB of device reads — **roughly 190x more I/O**.

### 6.5a A prediction that did not survive measurement

The materialized `TrieNode` tree that execution carries forward was identified
as ~800 MB per worker of the ~1.1 GB resident, and re-rooting from the hash
every 500 blocks was expected to reclaim most of it. **It did not.** With
re-rooting in place, resident set stayed at ~1.1 GB against a ~150 MB window,
and the memory turned out to be 1,083 MB of private dirty heap spread across
many allocations rather than one structure.

The re-rooting was kept -- it is cheap and bounds a structure that is otherwise
unbounded -- but the ~800 MB attribution was arithmetic, not measurement, and it
was wrong. What did halve throughput and then recover it was swap pressure from
four workers, not the tree.

### 6.6 What a resident working set actually does

A 100-block range, replayed three times, small enough that its footprint fits in
cache:

| Pass | Rate | Device reads |
|---|---|---|
| Cold | 2.63 blocks/s | 1,989 MB |
| Warm | **26.75 blocks/s** | **0 MB** |
| Warm again | 27.86 blocks/s | 0 MB |

**10.6x, with the device untouched.** The second pass is what a resident segment
looks like, so this is the segmented rate measured rather than projected: a
warm worker runs at **27--35 blocks/s** (35 with the read-only block cache
configured as below), against 4.5 cold.

Note the amplification at this size: 1,989 MB of device reads for 3.0 MB of
useful nodes, **656x**. Fewer blocks amortise the SST blocks less, so short
ranges suffer most.

This also explains what more RAM would and would not do. Caching works
spectacularly when the working set fits and does nothing when it does not: the
same experiment over 1,000 blocks (footprint 3.3 GB, above what this machine can
hold) gained 6%, against 960% for 100 blocks (2.0 GB). A full replay pass never
revisits a block, so its working set is the whole 131 GB store; enlarging the
cache from 4.5 GB to 12.5 GB moves the cliff from ~100 blocks of footprint to
~300 and leaves a single forward pass essentially unchanged. Reorganising the
data is what removes the 656x, not caching more of it.

### 6.7 Core scaling, with the I/O removed

Every worker replaying the *same* warm range, so the data is shared and cached
and only CPU is in play. Device reads were zero at every worker count:

| Workers | Aggregate | Per worker | Scaling |
|---|---|---|---|
| 1 | 4.57 blocks/s | 4.57 | 1.00x |
| 2 | 9.09 | 4.55 | 1.99x |
| 3 | 13.32 | 4.44 | 2.91x |
| 4 | 14.30 | 3.58 | 3.13x |
| 6 | 13.89 | 2.31 | 3.04x |

Confirmed over a longer range: 1.00x, 1.95x, 2.84x, 3.23x. **Linear until the
cores run out** -- the plateau at ~3.2x on four cores is the production node
taking the rest. So once the working set is resident, throughput tracks core
count, and doubling cores roughly doubles it until something else binds.

Two things would bind next. Each worker needs its segment resident, so RAM has
to scale with workers -- 8 x 512 MB plus the OS does not fit in 7.7 GB, which is
where added memory finally pays, and it pays only after segmenting. And replay
still reads a header and a body per block from the block store; at 280 blocks/s
that is ~560 IOPS against a device that saturates near 2,700.

### 6.8 The device

| Sequential read, direct I/O | 211 MB/s |
|---|---|
| Random 16K, queue depth 1 | 1,286 IOPS, **0.78 ms** each |
| Saturated, 4--8 workers | ~43 MB/s |

0.78 ms is network-attached latency, and one worker doing dependent reads sits
close to that ceiling on its own. Local NVMe (~0.08 ms) would lift the cold path
without any code change.

### 6.9 Configuring a read-only handle

These handles exist to be opened many at once, and the obvious configurations
are both wrong on a small machine:

| `max_open_files` | Block cache | Warm rate, 1 worker | 8 workers on one warm range |
|---|---|---|---|
| unbounded | default | 27 blocks/s | collapses to 12, hits disk |
| 128 | default | 5.9 | stable, 0 device reads |
| unbounded | 64 MB, index+filter cached | **35** | memory-tight above 3 |

An unbounded table cache pins an index and filter block per SST -- hundreds of
MB per handle against 2,002 files -- and evicts the page cache the readers
depend on. Capping `max_open_files` fixes the memory and costs 4.6x in file
churn. A bounded block cache holding index and filter blocks does both, and is
what `open_read_only` now configures.

### 6.10 Sliding-window miss rate

The build design Sergio proposed keeps the last few block-ranges' nodes in RAM
and falls back further only on a miss, so the whole thing turns on a number:
what fraction of reads touch state older than the resident window.

`trie_window` records, for every read, how many blocks ago that node was last
written during the replay, and histograms it. A window of `W` blocks then misses
on everything older than `W`, plus everything never written during the replay --
counted separately, because that floor is genuinely cold state that no window
size reaches.

Recent era (#9,150,000), 40,000 blocks with the first 10,000 discarded as
warm-up: 52,668,616 reads counted, 9,276,067 writes, 1,756 reads/block.

| Window | Size @16.8 KB/blk | Miss rate | Misses/block |
|---|---|---|---|
| 256 blk | 4 MB | 9.75% | 171 |
| 1,024 | 17 MB | 6.52% | 115 |
| 4,096 | 67 MB | 4.27% | 75 |
| 8,192 | 135 MB | 3.75% | 66 |
| 16,384 | 269 MB | 3.47% | 61 |
| **32,768** | **538 MB** | **3.31%** | **58** |
| 65,536+ | 1 GB+ | 3.309% | 58 |

**The curve saturates at ~32,768 blocks, about 540 MB.** Beyond that the window
buys nothing: what remains is the cold floor of 3.309%.

A pilot at #2,500,000 gave a floor of 3.74% on a much shorter sample, so the
shape looks era-independent.

### 6.11 Why the fallback target decides the design

58 misses per block reads like a verdict: at the 0.78 ms of §6.8 that is 45
ms/block against a ~32 ms CPU-bound budget (§6.6), which is I/O-bound again.

But those are *reads*, and reads repeat 10.2x on the same range (§6.1:
30,556,982 reads over 2,985,485 distinct nodes). So it is ~5.7 distinct cold
nodes per block, and whether the repeats are absorbed decides everything:

| Fallback target | Useful nodes per 16 KB device read | Cost per block |
|---|---|---|
| The 131 GB archive | ~1 (152x amplification, §6.5) | ~45 ms -- **I/O-bound** |
| A dense sealed segment | ~120 | ~4.4 ms -- **14% overhead** |

The amplification does not change the latency of one cold read; it changes
whether the page cache can absorb the other 9.2. That is the whole argument for
sealed dense tables over the archive as the miss path, and it is the difference
between a 14% tax and losing about 40% of throughput.

Keep the archive reachable as a correctness net that should never fire.

## 7. Answers

### 7.1 Segment size `S`

With `W` within noise of zero (§6.3), the usual tension nearly disappears:
making segments smaller costs 1--2% in duplicated bytes, not the multiple the
proposal assumed. The total is ~88 GB at any practical `S`, and the segment
count is simply `88 GB / S`. `S` is therefore chosen almost entirely by memory
and worker count:

    S  ≈  (usable RAM) / (concurrent workers)

On this machine, ~4.5 GB is usable with the node running, and §6.2 puts the
throughput peak at eight workers, with sixteen OOM-killed. That gives
**S = 512 MB**, ~176 segments over the Unitrie era, spanning ~30,000 blocks in
the recent era and ~74,000 in the early one.

§6.10 arrives at the same figure independently and for a better reason: the
miss-rate curve saturates at a window of ~32,768 recent-era blocks, ~540 MB.
Below that, misses rise steeply -- 58/block at 32k against 171/block at 256 --
and above it the window buys nothing at all.

Err small rather than large. A larger `S` barely reduces duplication — 2.4% of
the total at 512 MB against 1.2% at 1 GB, about 1 GB of difference — and costs
the ability to run the workers the scaling curve says are worth running.

If the machine grows, spend the RAM on workers first and `S` second, until
replay stops being latency-bound.

### 7.2 Total size

**Measured: 137 GB** across 97 chunk databases and 1,517 sealed segments
(§10). The projection below was 88 GB raw / ~98 GB with an index, and it was
low by 40% -- see §10.2 for why.

The projection reasoned that the segments would come out *smaller* than the
131 GB archive, because duplication is 1--2% (§6.10) and canonical replay never
reads the non-canonical state rskj wrote (§6.4). Both of those hold. What the
projection missed is that a build split into 97 independently-seeded chunks pays
a cold start each time; see §10.2.

### 7.3 What it buys

| Pass | Against the archive | Against segments |
|---|---|---|
| Device I/O per 30,000 blocks | ~99 GB | ~0.5 GB |
| Per-worker rate | 4.5 blocks/s | **27--35** (measured warm, §6.6) |
| Aggregate on 4 cores | 26 blocks/s (8 workers) | ~90--110 (3 workers x scaling) |
| Full Unitrie-era replay | ~4.3 days | **~0.9 days** |

Both the I/O row and the per-worker rate are measured (§6.5, §6.6); the
aggregate applies the core scaling of §6.7. What is not measured is a real
segment store -- these come from a page cache standing in for one.

**On buying hardware.** More RAM alone does almost nothing (§6.6): a forward
pass never revisits a block, so the cache cannot cover it. More cores do almost
nothing *first*, because at 8 workers the job uses ~17% of the four cores it
has. Segment the store and the order reverses -- cores scale it linearly
(§6.7) and RAM has to keep up with the workers. Faster storage helps in either
order (§6.8).

The build pass costs one archive-speed traversal -- ~4.3 days -- so it pays for
itself after roughly one repeat run. Worth it for a regression harness that runs
often; not worth it for a single verification.

## 8. Blocks 0--1,590,999: a recorded-roots baseline

Sergio's proposal for the pre-RSKIP126 era: record the Unitrie root each block
*computes* -- not the one its header declares, which is Orchid-format -- into a
side database, and check later runs against it. 1,591,000 blocks x 32 bytes is
~51 MB.

**It is serial only once.** The design above calls this era serial by necessity;
that is wrong, and the baseline is what makes it wrong. The era cannot be
range-parallelised today only because no historical state exists to start a
worker mid-chain -- unlike the Unitrie era, where the snapshot is archival. A
baseline pass supplies the missing starting roots, and if it also writes its
nodes into segments, later runs replay 0--1,590,999 in parallel like the rest of
the chain. Record the root keyed so that a worker starting at block N can look
up its parent's.

**It needs an anchor, or it preserves whatever rustock does today.** A recorded
baseline checks rustock against itself: a wrong root at #400,000 becomes
permanent and every later run agrees with it. That is regression detection, not
correctness -- worth having, but only trustworthy if something independent
validates it.

One independent check exists and is a single comparison. **#1,591,000 is the
first block whose header carries a Unitrie root.** Replaying 0 -> #1,591,000
from genesis and matching that header validates the entire cumulative pre-Unitrie
state -- every balance, nonce, code blob and storage cell that survived to that
point. Trust the baseline only if that anchor holds.

The finer-grained check would be the Orchid conversion, per block or at
intervals, which is what rskj 1.x did while building its Unitrie from genesis.
It is unavailable today: since rustock started writing REMASC's `siblings`
storage cell, `orchid_state_root` no longer reproduces the pre-126 header root
(`quirks-frozen-bugs.md` §9c, logged as an open item). Measured here, the
failure is *not* uniform -- blocks #1 to ~#6,000 mismatch and #8,000 to #20,000
all match -- so it depends on the shape of the REMASC storage subtree rather
than being broken outright. Fixing the converter's `addStorageBytes` handling
would turn the anchor from one end-of-era comparison into a bisection tool.

**Version-tag the database.** A deliberate change to trie encoding or key
mapping legitimately changes every recorded root; without a schema tag that
reads as 1,591,000 failures instead of one "baseline is stale".

### 8.1 Cost

`cargo run --release -p rustock-cli --example pre_unitrie_roots -- <block-dir>
<trie-dir> <count> [orchid-interval] [csv]` replays from genesis, recording the
computed root per block and optionally running the Orchid check.

| | |
|---|---|
| Replay rate, first 20,000 blocks | **99 blocks/s** |
| Orchid conversion at 20,000 blocks | 214 ms, growing with trie size |
| Baseline database | ~51 MB |

Early blocks run 20x faster than Unitrie-era ones (4--5 blocks/s): they are
nearly empty and the whole trie stays resident, which is the same locality
effect §6.5 measures from the other side.

## 9. What this does not answer

- **The Orchid converter.** §8 wants it for per-block ground truth below
  #1,591,000, and it does not currently reproduce the header root. Whether the
  #1,591,000 anchor itself holds is also untested -- nobody has replayed the era
  from genesis to the end.
- **The projected 60--85 blocks/s.** §6.5 measures the I/O saved and the CPU
  headroom available, not the rate a real segment store achieves.
- **Bodies.** Replay also reads a header and a body per block from the 35 GB
  block store, also keyed by hash and so also scattered. At ~2 reads/block
  against ~1,400 trie reads/block this is noise, but it is not zero.

---

## 10. The build, as it actually ran

The whole-chain build ran 15-16 September 2026 on the machine described in §6:
4 vCPU, 7.7 GB RAM, production node running throughout.

### 10.1 Result

| | |
|---|---|
| Blocks executed | **9,217,860** of 9,230,008 |
| State roots checked (>= #1,591,000) | **7,626,860** |
| Divergences found | **1** — block #9,217,796 (§10.4) |
| Chunks | 96 of 97 complete, 1 failed at the divergence |
| Segments | 1,517 across 97 chunk databases |
| Size on disk | **137 GB** |
| Elapsed | 36h 29m |

Blocks #9,217,796--#9,229,943 (12,148) are unverified: the failing chunk stopped
at the mismatch, by design.

### 10.2 Why 137 GB and not 88

The projection (§6.4) modelled one continuous window over the whole era. The
build instead ran 97 independently-seeded chunks, and **each chunk pays a cold
start**: its window begins empty, so every node it reads from the archive is
copied into that chunk's own database. Nodes live in the working set of several
adjacent chunks get written once per chunk rather than once overall.

That cost was understood -- §6.10 measures a cold window at ~40% slower for its
first 10,000 blocks, which is why workers prefer the adjacent chunk -- but its
effect on *size* was not modelled. The 1--2% duplication figure in §6.10 is the
duplication *within* a continuous window; across chunk boundaries it is much
larger.

The fix, if the artifact is rebuilt, is fewer and larger chunks, or merging
adjacent chunk databases afterwards from the recorded boundaries. Neither
requires re-executing anything.

### 10.3 What the pass does and does not verify

It compares **only the state root**. `execute_block` validates nothing;
`process_block` is the one that also checks transactions root, ommers hash, gas
used, receipts root and logs bloom. The tool called the former.

So the result is: **no divergence in the state transition** across the Unitrie
era. Receipts roots, gas used and blooms are unchecked, as are all headers
(no `HeaderVerifier` runs) and every block below #1,591,000 except through the
cumulative anchor (§8).

A second pass using `process_block` closes that gap, and reading from dense
local segments rather than the archive it should cost **~15--19 h on this
machine, ~7.5 h on eight cores** -- against the 36.5 h this build took.

### 10.4 The divergence

Block **#9,217,796**: computed `0x34ef1353...`, header `0x0c47553b...`.

Genuine, not an artifact:

- the header matches public-node.rsk.co exactly (hash, state root, parent,
  3 txs, 0 uncles, 151,732 gas);
- the parent #9,217,795 verified, so the pre-state was right;
- reproduced from an independent seed (#9,217,789) with an empty segment store,
  producing the same computed root -- so not a `WindowStore` artifact;
- 84,448 blocks in the same chunk verified before it.

The block's second transaction calls the Bridge (`0x01000006`) with selector
`0xf10b9c59`, which decodes as
`addSignature(bytes federatorPublicKey, bytes[] signatures, bytes rskTxHash)` --
a federator signing a peg-out release. 95,488 gas, status success, **zero logs**,
which suggests this signature did not complete the release, so the divergence is
in how partial-signature state is recorded rather than in the release path.

Unfixed and uninvestigated beyond this. The next steps would be to diff
post-block Bridge storage against the chain to localise the cell, and to check
whether other `addSignature` calls diverge -- one occurrence in 7.6M verified
roots suggests a specific condition rather than a broken method.

### 10.5 Operational notes

- **Work stealing was necessary.** Static balancing failed twice: on
  trie-nodes-touched one worker drew a 142-hour range against another's 8, and
  rebalanced on sampled gas it was still 44 against 24. Cost per block tracks
  EVM gas, and gas buys different amounts of work in different eras.
- **Four workers were worse than three.** Measured, the fourth contributed ~4%
  of throughput for 25% of the memory; removing it cost 0.9 blocks/s and the
  freed page cache returned more than that.
- **The run survived an OOM kill** (a `cargo` link step on the same box took the
  memory) and several restarts, because chunks are claimed and checkpointed.
  A killed worker re-takes its own chunk and resumes.
