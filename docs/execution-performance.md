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

## The method

Every number above comes from one of two measurements, against two
different states. Both are described here in full, because a performance
claim that cannot be reproduced is an anecdote.

### Why the workload is synthetic

Two reasons, each disqualifying on its own.

**Mainnet has no partitioned blocks.** RSKIP-144 is unscheduled there
(`reed810 = -1`), so no mainnet header has ever carried a non-empty edges
field. **Testnet's are too small to measure**: of 1,020,420 canonical
headers above its activation height, 203,419 split, but their edges are
`[1]`, `[1, 2]`, `[1, 3]` — one or two transactions per sublist at about
45,000 gas. Dividing one transaction between two threads measures the
cost of starting a thread.

And on today's mainnet there is nothing to measure at all: blocks are
running at roughly 1% of the gas limit, a node in follow mode executes
one in 38 ms and then idles for thirty seconds.

So the transactions are synthetic. That is the weakness, and it is the
reason the *state* they run against is not.

### The state: a 1 GB synthetic trie

Starting from an empty trie measures the wrong thing — the first blocks
run against a handful of cached nodes, the rate starts absurdly high and
decays, and the average is an artifact of the ramp. The fixture is sized
so its working set already exceeds anything a run adds.

Sized against mainnet rather than guessed. From `trie-scan-baseline.md`,
mainnet's **live** state extrapolates to ~600,000 accounts, 4.6 million
storage cells, 10.8 million nodes, 0.7–0.9 GB. (The 131 GB archival trie
is history, not what a node reads while following.) Measured at 115 bytes
per account — stable at 116.2 at 200k, 115.5 at 2M, 114.9 at 9.3M — one
gigabyte is 9.3 million accounts.

| | |
|---|---|
| accounts | 9,300,000 |
| of which can sign | 93,000 (1%) |
| on disk | 1,019 MB |
| depth | ~23 levels |
| build time | 92 s |
| root | `0x5a428782bfde84201dedffaf67632df2d7886a5b2d9d41dddff62a4ad5535d57` |
| built by | `cargo run --release --example synthetic_state -- build <dir> --accounts 9300000 --signers 93000` |

Only 1% carry real keys: nine million curve multiplications for addresses
that will never spend is time for nothing, but senders must sign. 93,000
is also enough that their paths do not all sit in cache after the first
block.

The build is bottom-up (`crates/trie/src/bulk.rs`), leaves first, each
node written once. Inserting one at a time is O(n²) — `TrieNode::put`
returns a new root and deep-copies the siblings along the path — and took
63 s for 20,000 accounts. **This layout is also why the fixture flatters
FlatDB**: writing bottom-up leaves the top of the trie contiguous at the
end of the heap, which a real store, written in traversal order over
months, does not have. Every FlatDB figure taken on the fixture is
therefore optimistic, and the one taken on real state (1.76×) is not.

### The blocks

Generated deterministically from the fixture's seed, so a run is
reproducible and the access list is knowable in advance.

| | |
|---|---|
| per transaction | **native rBTC transfer**, 21,000 gas, value 1 wei, empty calldata |
| per block | 200 transactions, **4,200,000 gas** |
| partitioning | 4 parallel sublists of 40, plus 40 sequential |
| blocks per run | **100** (20,000 transactions, 420M gas) |
| state touched | two account cells per transaction |

Senders are drawn from the 93,000 keyed accounts, a different slice each
block so no small set stays warm; recipients from all 9.3 million by a
multiplicative step coprime with the count, so reads scatter over the
whole gigabyte.

**Every recipient already exists**, which is a sharper limitation than it
sounds. `recipient` indexes modulo the fixture's account count, so no
`put` ever inserts a key that is not already in the trie, so
`TrieNode::split` is never called in a run. The insertion path — the one
that deepens the trie, allocates the extra branch node and revises
`children_size` up the spine — has never been on the clock. Everything
below measures the cost of *updating* a trie of fixed shape. A real chain
creates accounts, contracts and storage slots continuously, and nothing
here says what that costs.

**This is close to a worst case for parallel execution and should be read
that way.** A native transfer is the least compute-intensive transaction
there is: a balance check and two balance updates against two random
paths. The ratio of I/O to arithmetic is as lopsided as it can be made.
A block of contract calls does far more work per state access.
`parallel_workload` already builds ERC-20 transfers for that comparison
and it has not been run.

### The second state: the real mainnet store

The read comparison between the engines (1.76× serial, dead heat
batched) was **not** measured on the fixture. It used the node's own
epoch store, converted with the node stopped:

- 3 epochs, 25,371,307 nodes copied, **none skipped** — which establishes
  on production data that the trie store is content-addressed throughout;
- 10,519,952 nodes resolved by descending from the executed state root;
- every record over the 1,024-byte threshold read back and verified
  (5,589 + 450 + 475, none failed) — contract code up to 24,573 bytes,
  which the fixture contains none of;
- mean record 73.5 bytes on disk across the three epochs.

Both sides therefore hold identical data with identical roots, on the
same physical disk.

### Running conditions

| | |
|---|---|
| machine | 4 cores, 7 GB, **mainnet node stopped for the duration** |
| RocksDB block cache | 256 MB with bloom filters at 10 bits/key — the node's own `BLOCK_CACHE_BYTES` |
| peak RSS | 375–411 MB (RocksDB), 972 MB of which 10 MB anonymous (FlatDB) |
| flush cadence | every 100 blocks, the node's own, for both engines |
| per run | fresh copy of the fixture, page cache evicted, fixed block count |

**A fixed block count, not a fixed time.** Time-boxing lets the faster
configuration execute more blocks and so sample a different part of a
decay curve that falls steeply for the first eighty.

**The page cache is evicted by name.** `posix_fadvise(POSIX_FADV_DONTNEED)`
on the store's own files, not by reading unrelated data to flush them out
— see the methodology section at the end for why that does not work.
`drop_caches` is unavailable: `/proc` is read-only on this host.

**The exec/write split in the CSV is not comparable across thread
counts.** With one thread the caller applies the state changes and the
time lands in `write`; with two or more the sublists commit inside
`execute_block` and it lands in `execution`. Only the total is
comparable, and the totals are what every figure above uses.

### Reproducing it

```sh
# the fixture, once
cargo run --release --example synthetic_state -- build <dir> \
    --accounts 9300000 --signers 93000

# optionally convert it to FlatDB
cargo run --release --example flatdb_convert -- <dir> <flat-dir> \
    --expected-keys 10m

# one run; --flatdb and --prefetch select the levers
cargo run --release --example loadtest -- <dir> \
    --threads 4 --sublists 4 --txs 200 --blocks 100 \
    --flush-every 100 --prefetch 16 --csv run.csv
```

Each run prints the final state root. **All eight runs of the thread
sweep, and all four of the prefetch matrix, end on the same root** —
these are differences in speed and in nothing else.

The fixture and the runner live in
`/mnt/import/tn-snapshot/regtest-parallel-bench/`. That path is
misleading — it is regtest data, not testnet — and sits there only
because `/mnt/import`'s root is not writable by the account that built
it.

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
at 200 transactions a block and 32% at 40 — it *shrinks as writes grow*.

That was first read as a lock inside the parallel region, and the
store's writer mutex was removed to test it. Nothing got faster: 1/2/3/4
threads went from 64.1 / 67.2 / 62.8 / 58.7 s to 64.4 / 67.2 / 63.4 /
57.6 s. The duller explanation is the right one — more transactions mean
more **sequential** work per block, not more contention. More nodes to
hash when recomputing the root, more to walk in the ordered commit, more
to write; work that grows with block size and that no thread count
divides. [`flatdb-writer-lock.md`](flatdb-writer-lock.md) records the
disproof.

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

## Where a block's time actually goes

Until this was instrumented, the sequential share was inferred from
differences between whole runs. Measured directly — node stopped, three
interleaved repetitions, medians, 60 blocks of 200 transfers at four
threads:

| phase | ms/block | parallel? |
|---|---|---|
| maintenance run | 0.0 | no |
| **sublist execution** | **84.2** | **yes** |
| **ordered commit** | **431.7** | no |
| root hashing | 5.1 | no |
| lazy reload | 0.4 | no |

**The commit costs 5.1× what executing the transactions costs**, and
80–84% of the measured work is sequential whatever the thread count.

Running the 200 transfers takes 84 ms. Applying what they changed to the
trie, one sublist after another, takes 432.

The mechanism is the one the design implies and nobody had priced. Each
sublist computed its changes against the same `base_root`, so
`apply_state_changes` runs once per sublist, each rewriting the top of
the trie on top of the previous one's result. Four sublists means the
upper trie is rebuilt four times, in sequence. That is the work parallel
execution *adds*, and it is five times the work it divides.

Three of the four phases that were listed as unprofiled turn out to be
negligible — maintenance, root hashing and reload total 5.5 ms. The
fourth is everything.

### Why these numbers are trustworthy where the wall clock is not

The wall clock for one thread came out 86.8 / 41.0 / 37.8 s across three
runs of identical code: a 2.3× spread. The phase figures over the same
runs repeated within 2% — sublists 83.9 / 84.2 / 108.8, commit 430.4 /
439.3 / 431.7.

That is the expected split. Wall clock includes cold-start I/O, which is
what varies wildly on this host. The phases measure CPU over structures
already in memory.

### The commit is where parallel execution loses its money

Instrumenting the single-threaded path too — it applies the changes in
the caller rather than inside the executor, so it had been reporting
nothing — makes the cost explicit. Identical work, 401 accounts changed
per block, three interleaved repetitions:

| | commit, ms/block | |
|---|---|---|
| one thread | 303.8 / 320.2 / 308.6 | median **308.6** |
| four threads | 459.3 / 479.0 / 614.8 | median **479.0** |

**Four sublists cost 55% more to commit than one**, for the same
changes. Per repetition: 1.51×, 1.50×, 1.99× — the third from a round
where the machine slowed and both phases rose together.

So the trade parallel execution actually makes, at this block size, is
to save about 84 ms of execution and spend about 170 ms more on the
commit. It loses, and that is why four threads barely beat one here and
lost outright once the store was fast and the access list prefetched.

The mechanism is the one the design implies: each sublist computes its
changes against the same `base_root`, so `apply_state_changes` runs once
per sublist and each rebuilds the upper trie on top of the previous
one's result. One thread rebuilds it once.

### And the per-account cost grows with the block

Four threads, medians of three:

| accounts | commit | per account |
|---|---|---|
| 101 | 98.8 ms | 978 µs |
| 201 | 174.9 ms | 870 µs |
| 401 | 479.0 ms | 1,195 µs |

Doubling 201 to 401 accounts multiplies the commit by 2.74. Doubling 101
to 201 multiplies it by 1.77. **Suggestive of superlinearity and not
established** — the 101-account point measured 80.1 / 98.8 / 121.7, a
52% spread, because a small block amortises its fixed costs worst and
the noise with them.

A candidate worth testing: `TrieNode::put` returns a new root, and
`NodeRef::Node` owns its subtree, so rebuilding a path deep-copies the
siblings along it. That is exactly the O(n²) that made the fixture build
take 63 s for 20,000 accounts and was fixed there by building bottom-up.
The commit still inserts one account at a time.

Hashing is **not** a candidate: `hash_cache` is a `OnceLock` and
`put_impl` never touches `compute_hash` or `to_message`, so building the
trie hashes nothing. The whole dirty subtree is hashed once at the end,
for 5.1 ms.

### What to attack

Not the store, not prefetching, not the thread count — those make the
84 ms smaller. The 432 ms is the commit, and the obvious question is
whether four sequential `apply_state_changes` can become one: the
sublists touch disjoint accounts by construction, so their changes could
in principle be merged and applied in a single pass over the trie.
Whether that preserves the ordering consensus depends on, and is
constrained by, what the sequential sublist is allowed to observe.

**Not yet measured:** the single-threaded path does not go through
`execute_block_in_parallel`, so it reports no phases at all. Its
breakdown is unknown, and it is the path the node runs on today.

## What is left

With FlatDB and a perfect access list, execution sits at 92% CPU on one
core and 50.4 s for 100 blocks. The remaining time is not I/O. By
Amdahl, at most a third of a block is parallelisable; the rest is:

- the block-level maintenance run,
- the ordered commit of each sublist,
- the root hashing, which walks every dirty node,
- the lazy reload of the root between blocks.

All four are sequential by construction. Three of them are now measured
— see "Where a block's time actually goes" above; the ordered commit is
the whole story and the other three round to nothing.

**The insertion path is the gap left.** Because every recipient
pre-exists in the fixture, no run has ever split a trie path or
deepened the trie. The commit cost measured here is the cost of
rewriting a spine that already has the right shape. Extending the
workload to draw a share of recipients from beyond the fixture's account
count is a small change to `recipient` in `loadtest.rs`, and it is the
measurement that would tell us whether account creation is cheaper than
an update (its descent stops at the divergence point, so it may be) or
dearer (it allocates an extra node per insertion and grows the store).

What has been ruled out is the store's writer mutex. It was the obvious
suspect — scaling tracked write volume, which looks exactly like a lock
inside the parallel region — and removing it changed nothing measurable.
[`flatdb-writer-lock.md`](flatdb-writer-lock.md) records that, along
with the regression the same work introduced: batching the writes made
one thread 1.16–1.80× faster and four threads slower than one, because
publication is ordered and a reservation is now a whole sublist rather
than a single record.

---

## Recommendations

**Deploy FlatDB.** 1.76× on the read pattern execution actually uses, a
thirtyfold smaller memory demand, no compaction, and a simpler
durability story. Running on the mainnet node now. The remaining work
is the ordered-publication regression in
[`flatdb-writer-lock.md`](flatdb-writer-lock.md) §4 and the operational
items in [`flatdb-rollout.md`](flatdb-rollout.md).

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
