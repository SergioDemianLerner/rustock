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

It also bounds the payoff. Freeing the reads lets a worker approach one full
core, ~5x its current 20%, and four cores cap the total. Expect on the order of
60--85 blocks/s aggregate against 26 blocks/s now: **~1.3 days for the Unitrie
era against ~4.3 days**. That is a projection from the CPU headroom, not a
measurement of a segment store that does not exist yet.

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

Err small rather than large. A larger `S` barely reduces duplication — 2.4% of
the total at 512 MB against 1.2% at 1 GB, about 1 GB of difference — and costs
the ability to run the workers the scaling curve says are worth running.

If the machine grows, spend the RAM on workers first and `S` second, until
replay stops being latency-bound.

### 7.2 Total size

**~88 GB** of raw key+value bytes over the Unitrie era. As sealed mmap files
with a flat hash index (~12 B/entry over ~859M entries) that is **~98 GB**; in
RocksDB with compression, ~70 GB. Either fits the 253 GB free on `/var/lib`.

This is *less* than the 131 GB archive, not more. The proposal assumed
duplication would dominate; the measurement says duplication is 1--2%, and that
the segments additionally drop nodes canonical replay never reads (§6.4). Budget
for 131 GB and expect to use ~98 GB.

### 7.3 What it buys

| Pass | Against the archive | Against segments |
|---|---|---|
| Device I/O per 30,000 blocks | ~99 GB | ~0.5 GB |
| Aggregate throughput | 26 blocks/s (8 workers) | 60--85 blocks/s (projected) |
| Full Unitrie-era replay | ~4.3 days | **~1.3 days** (projected) |

The I/O row is measured (§6.5). The throughput row is projected from the 20%
per-worker CPU utilisation measured there, capped by four cores; it should be
confirmed against a real segment store before being relied on.

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
