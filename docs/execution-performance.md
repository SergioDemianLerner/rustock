# Three ways to make block execution faster, and what each is worth

October 2026. A synthesis of three lines of work that turned out to be
the same line: parallel sublist execution (RSKIP-144), replacing the
trie store with FlatDB, and prefetching a block's access list.

Each was measured on its own and all three were measured together. They
do not add up, and the reason they do not is the most useful thing here.

The detail lives in
[`parallel-execution-benchmark.md`](parallel-execution-benchmark.md),
[`flatdb.md`](flatdb.md) and [`profiling.md`](profiling.md). This is the
part you need to decide what to build.

---

## The short version

Against one thread on RocksDB, executing 100 synthetic blocks of 200
native transfers each against a 1 GB state:

| | wall | speedup |
|---|---|---|
| baseline — RocksDB, one thread | 181.7 s | — |
| four threads | 125.6 s | 1.45× |
| access list, prefetched 16 ways | 72.4 s | **2.51×** |
| FlatDB instead of RocksDB | 58.2 s | **3.12×** |
| **FlatDB + access list, one thread** | **50.4 s** | **3.61×** |
| FlatDB + access list + four threads | 58.0 s | 3.13× |

Three things are worth reading twice.

**Parallel execution is the weakest of the three levers.** It is also
the only one that needs a consensus change.

**The best configuration uses one thread.** Adding threads to the
fastest setup makes it slower.

**The levers do not compose.** All three are ways of not waiting on a
read. Once one of them has removed the waiting, the others have nothing
to remove — which is why the three together are worse than two of them.

---

## Why they do not compose

Every one of these is an answer to the same question: *the CPU is idle,
what is it waiting for?*

Executing a block means walking the trie. Each account touched is a
descent of about 23 nodes, and each node's address is the hash stored in
its parent — so the reads are **dependent**: you cannot ask for the next
one until the last has returned. The device sees one outstanding request
and spends the rest of its time idle. On the original benchmark that
showed up as 48% CPU on a single thread: half the run was waiting.

- **Parallel sublists** overlap the waiting by running several
  independent transaction streams at once.
- **A prefetched access list** overlaps it by descending every path the
  block will touch before execution starts, N at a time.
- **A faster store** removes the waiting rather than overlapping it.

Stack them and the second one arrives to find the queue already empty.
FlatDB plus the access list leaves execution at 92% CPU on one core —
there is no idle time left for threads to fill, and the threads then
cost more than they save.

---

## Lever 1: parallel sublist execution

RSKIP-144 splits a block's transactions into sublists that can execute
concurrently, with the trie as the merge point. Implemented, tested
against rskj's own partitioner, and measured.

| threads | wall | CPU |
|---|---|---|
| 1 | 217.9 s | 48% |
| 2 | 193.1 s | 71% |
| 3 | 200.7 s | 72% |
| 4 | 163.3 s | 86% |

**1.33× over the run, 1.11× once warm.** On four cores, work that was
compute-bound would approach 400% CPU; 86% is less than one core, which
is how the I/O bound was established in the first place.

Two limits, and only one of them is about I/O.

**Consensus caps it at two.** `TX_EXECUTION_THREADS` is 2 — rskj's
value, and therefore the network's. RSKIP-144 itself specifies 4. Every
figure above four-way is what raising the cap *would* buy, not what is
available. At two threads the measured gain is 1.13%, inside the
run-to-run spread.

**Amdahl caps it well below the thread count.** The parallel region is
only the sublists. The block-level maintenance run, the ordered commit,
the root hashing and the lazy reload are all sequential. Measured on
FlatDB, where reads no longer hide it, the parallel fraction is **16%**
at 200 transactions a block and 32% at 40 — and it *shrinks as writes
grow*, because the writes inside the sublists serialise on the store's
writer mutex ([#319](https://github.com/SergioDemianLerner/rustock/issues/319)).

With a fast store, four threads buy 1.09×.

### What it is still for

Not throughput on today's chain. RSKIP-144 is unscheduled on mainnet and
mainnet blocks are running at about 1% of the gas limit; a node in follow
mode executes a block in 38 ms and then idles for thirty seconds.

It matters if the gas limit rises, if block utilisation rises, or if the
transaction mix shifts to work that computes rather than work that waits.
All three of those change the shape of the problem, and the implementation
has to exist before any of them can be tested.

---

## Lever 2: FlatDB

A port of rskj's `flatdb-simplified` branch: an append-only
memory-mapped heap of records with an open-addressed index of 8-byte
slots over it. The index stores **no key bytes at all**, because the key
is the hash of the value, so a probe resolves a collision by fetching
the candidate and recomputing `H(value)`.

Running on the mainnet node since 2026-10-20 21:59 UTC.

### What it is worth

On the real mainnet store, 65,535 nodes, same data on both sides:

| | serial | batched (16 threads) |
|---|---|---|
| RocksDB | 74,450 ms | **8,166 ms** |
| FlatDB | **42,425 ms** | 8,372 ms |

**Serial: 1.76×. Batched: a dead heat.**

The serial column is the one that counts. `get_many` has exactly two
production callers — the GC sweep's marking phase and the diagnostic
tools. Block execution issues dependent reads through
`NodeRef::try_resolve → store.get()`, one at a time, and no amount of
engineering changes that: you cannot ask for a node whose hash you have
not read yet.

### Why the synthetic figure was higher

The fixture measured 3.5×. It was built bottom-up by `bulk.rs`, so the
top of its trie sits contiguously at the end of the heap and a walk is
nearly a sequential read. Real state is written in traversal order over
months and spread across three epochs. **1.76× is the figure to carry.**

### Memory is the clearer win

Peak resident memory, same workload:

| | RssAnon | RssFile | total |
|---|---|---|---|
| RocksDB, 256 MB block cache | **334 MB** | 11 MB | 345 MB |
| FlatDB | **10 MB** | 962 MB | 972 MB |

FlatDB's total is three times larger and its *demand* is thirty times
smaller. Almost all of it is file-backed — page cache the kernel owns
and can reclaim. RocksDB's 334 MB is anonymous: under pressure the
kernel can only swap it.

On the live node, after 87 minutes, `RssAnon` settled at **60 MB**.

That is operational rather than cosmetic. Where something else needs
memory, FlatDB degrades — pages evicted, next probe faults, reads slow —
and RocksDB competes. On a host that has already had a long job
OOM-killed by a 2 GB cache, degrading is the behaviour worth having. It
also explains two results that looked paradoxical at the time: a 1,536 MB
RocksDB block cache was *slower* than a 256 MB one, and a 500k-entry
node LRU cost 38%. An anonymous cache takes memory from the page cache
doing the real work; a mapped store has one pool and cannot make that
mistake.

### And it has no compaction

RocksDB wrote 19 MB to the device per block to persist about 795 KB of
trie nodes — 24× amplification, essentially all compaction, and
invariant across every configuration tried. FlatDB's heap is a
sequential append at 1×. Its counterweight is the index: slots scatter
by hash, so persisting them touches up to 6,900 distinct 4 KiB pages. The
flush cadence amortises that, and whether the trade nets out has **not
been measured on real data**.

---

## Lever 3: the access list

The largest surprise, and the cheapest to deploy.

An [EIP-2930](https://eips.ethereum.org/EIPS/eip-2930) access list names
the accounts and storage slots a transaction will touch. It carries no
proofs and costs about 20 bytes an address — three orders of magnitude
less than shipping a Merkle witness, which `profiling.md` measured at
46 KB a block and ruled out for catch-up at 510 GB over the chain.

Simulated here at **100% coverage**: the benchmark generates its own
blocks, so it knows every account each one touches. That makes the result
an upper bound — if perfect foreknowledge buys little, nothing that has
to transmit, verify or guess a list can buy more.

| engine | threads | prefetch | wall |
|---|---|---|---|
| RocksDB | 1 | — | 181.7 s |
| RocksDB | 1 | 16 | **72.4 s** |
| FlatDB | 1 | — | 58.2 s |
| FlatDB | 1 | 16 | **50.4 s** |

**2.51× on RocksDB, 1.16× on FlatDB.** The gain tracks how slow a single
read is, which is the signature of a mechanism that overlaps waiting
rather than removing it.

### A single descent still cannot be batched

This is the part that is specific to our storage and easy to get wrong.

An access list gives **paths**, derived from addresses. A
content-addressed store retrieves a node by its **hash**, and a child's
hash is only learned by reading its parent. Knowing the destination does
not let a descent skip to it.

What the list gives is N descents that are independent *of each other*.
Each stays internally dependent; they can be in flight at once. That is
the entire mechanism.

A path-keyed store — Geth's pathdb, Erigon's flat state — could jump
straight to the leaf and would gain considerably more. Ours cannot, and
that is a consequence of content addressing rather than an oversight.

### Why it needs no consensus change

**A prefetch hint is allowed to be wrong.** An entry that is not needed
costs a wasted read; one that is missing costs nothing, because the read
happens as it always did. Nothing downstream depends on the list being
complete or honest.

That is the decisive difference from a witness, which *replaces* state
and must therefore be complete, correct and agreed upon.

RSK has no EIP-2930 and never adopted EIP-2929, so a sender has no gas
incentive to supply a list. It would have to come from the peer that
already executed the block and has the read set in hand — which is both
sound and free of protocol risk.

### What it does not establish

- Native transfers touch two accounts each and the list is trivially
  predictable. Contract calls reach storage slots that depend on
  execution itself, which is exactly where a real list is hardest to
  produce and most likely to be incomplete. **This has not been measured
  on contract traffic.**
- The benefit lives where reads are cold, which is catch-up. Following
  the tip the state is warm and there is nothing to save — the same
  conclusion `profiling.md` reached about witnesses by a different route.

---

## What is left

With FlatDB and a perfect access list, execution sits at 92% CPU on one
core and 50.4 s for 100 blocks. The remaining time is not I/O. By
Amdahl, at most a third of a block is parallelisable; the rest is:

- the block-level maintenance run,
- the ordered commit of each sublist,
- the root hashing, which walks every dirty node,
- the lazy reload of the root between blocks,
- and, inside the sublists, the writes serialising on the store's
  writer mutex.

That last one has a known fix and a known ceiling of about 10% of wall
clock ([#319](https://github.com/SergioDemianLerner/rustock/issues/319)),
with the naive version of it disproved — `size` is both the allocation
cursor and the publication barrier, so reserving ahead of writing breaks
the release/acquire pair that makes lock-free reads sound.

The other four are sequential by construction and have not been
profiled. **That is where the next measurement should go**, because it
is now the larger share.

---

## Recommendations

**Deploy FlatDB.** 1.76× on the read pattern execution actually uses, a
thirtyfold smaller memory demand, no compaction, and a simpler
durability story. Running on the mainnet node now. The remaining work is
in issues #319 and the operational items in
[`flatdb-rollout.md`](flatdb-rollout.md).

**Pursue the access list, as a peer-supplied hint.** The cheapest
intervention measured and the second largest, with no consensus
exposure. Measure it against contract traffic before designing the wire
format, because that is where its coverage assumption is weakest.

**Keep parallel execution; do not expect throughput from it.** It is
consensus-capped at two threads today, Amdahl-capped well below that,
and worth 1.09× once the store is fast. Its value is in being ready if
the gas limit or the transaction mix changes, not in what it returns on
the chain as it runs today.

**Measure the sequential section next.** It is now the majority of a
block's time and nobody has looked at it.

---

## How these numbers were produced, and how they were produced wrongly first

Five methodological faults, recorded because each produced a confident
number that was wrong, and three of them survived into a written
conclusion before being caught.

**Time-boxed runs.** Giving each configuration the same wall clock let
the faster one execute more blocks, sampling a different part of a decay
curve that falls steeply for the first eighty blocks. Fixed by running a
fixed block count.

**Evicting the page cache by filling it.** Reading 6 GB of unrelated
data does not reliably evict anything: Linux favours dropping pages it
has seen once sequentially over pages recently touched, which is exactly
backwards. It produced 376 ms and 66,870 ms for the same configuration —
a 178× spread. Fixed with `posix_fadvise(POSIX_FADV_DONTNEED)` on the
store's own files, which names what to drop and takes a second.

**Measuring two modes in one process.** Serving each trie level serially
*and* batched in the same run makes whichever goes first pay for the cold
cache. Serial-first reported 419× for batching; batch-first reported
0.05×. Neither number was about batching. Fixed by one mode per process.

**A stale output file.** A wait loop keyed on a completion marker
returned instantly because a previous experiment's output file already
contained it, and reported the wrong run's numbers.

**Reasoning about the kernel instead of measuring it.** `get_many` was
first written to prefetch with `MADV_WILLNEED` rather than fan out across
threads, on the sound-sounding argument that a mapped read is a page
fault and the kernel could overlap the batch without any threads.
Measured: fanning out 8.3 s, prefetching alone 37.0 s, **both 26.3 s** —
the advice competes with the threads' own faults. The reasoning was clean
and the kernel does not behave that way.

The pattern in all five is the same. Every one of them made the answer
*better* than the truth in the direction the experiment was hoping for,
and none of them was caught by thinking harder — only by a repetition
that disagreed with itself.
