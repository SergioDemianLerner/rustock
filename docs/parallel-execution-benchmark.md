# Measuring what parallel sublist execution buys

> For how this fits with the other two levers — the trie store and
> prefetched access lists — and which of them is worth what, see
> [`execution-performance.md`](execution-performance.md). This document
> is the detail behind one of them.

What RSKIP-144 parallel execution is worth, measured rather than argued.

This records the method before the numbers, so that the numbers can be read
against what was actually done rather than against what one imagines was
done. Where the method falls short of what would be ideal, it says so here
rather than in a footnote to the result.

## The question

Executing a block's sublists on several threads instead of one: how much
faster, and under what conditions does it stop being faster?

The honest form of the question is not "is it faster" but **where the
crossover is**. Parallel execution pays a fixed cost per block — a
maintenance run, one execution per sublist, and one trie commit per sublist
instead of one for the whole block — and earns it back only if there is
enough work to divide. A block of three transactions has nothing to divide
and the overhead is the whole measurement.

## Why not measure on the real chain

Two reasons, and both are disqualifying on their own.

**Mainnet has no partitioned blocks at all.** RSKIP-144 is unscheduled
there (`reed810 = -1`), so no mainnet header has ever carried a non-empty
edges field.

**Testnet's partitioned blocks are too small to measure.** Of 1,020,420
canonical headers above the activation height, 203,419 split — but their
edges are `[1]`, `[1, 2]`, `[1, 3]`. One or two transactions per parallel
sublist, of about 45,000 gas each. Dividing one transaction between two
threads measures the cost of starting a thread.

So the workload is synthetic. That is a weakness and it is the reason the
state it runs against is not.

## The state it runs against

A benchmark that starts from an empty trie measures the wrong thing: the
first blocks run against a handful of nodes that are all in cache, the rate
starts absurdly high and decays as the trie grows, and the average over the
run is an artifact of the ramp. What is wanted is a steady rate — the same
at block 100 and at block 3,000 — which means starting from a trie whose
working set already exceeds anything the run will add.

### Sized against mainnet, not guessed

Mainnet's **live** state, from `docs/trie-scan-baseline.md` (a scan that
reached 49.29% before it was read):

| | |
|---|---|
| accounts | 301,074 |
| storage cells | 2,320,844 |
| distinct nodes | 5,310,449 |
| deduplicated | 339 MB of 916 MB |

Extrapolated: roughly 600,000 accounts, 4.6 million storage cells, 10.8
million nodes, and 0.7–0.9 GB. (The 131 GB archival trie is history, not
live state, and is not what a node reads while following the chain.)

The fixture is built out of accounts alone, which is the cheapest way to
reach that size and leaves the traversal about right: random 31-byte keys,
so the tree is balanced and reads scatter.

**Measured at 115 bytes per account**, stable across three sizes (116.2 at
200k, 115.5 at 2M, 114.9 at 9.3M). One gigabyte is therefore **9.3 million
accounts** — not the twenty million a first guess suggested, which would
have been 2.3 GB and more than double mainnet's live state.

### The fixture

| | |
|---|---|
| accounts | 9,300,000 |
| of which can sign | 93,000 (1%) |
| on disk | 1,019 MB |
| depth | ~23 levels of branching |
| build time | 92 s |
| root | `0x5a428782bfde84201dedffaf67632df2d7886a5b2d9d41dddff62a4ad5535d57` |

One per cent of the accounts are derived from real keys; the rest are
hashes. Nine million curve multiplications to produce addresses that will
never spend is time for nothing, but the senders have to be able to sign.
93,000 is also enough that the senders' paths down the trie do not all sit
in cache after the first block — a smaller set would have turned a fixture
built to make reads cold into one that measures a warm cache.

A `MANIFEST` beside the root records the seed, the counts and the root, so a
run cannot be given the wrong parameters for the fixture it is reading.

### Building it bottom-up, and why that was necessary

The first attempt inserted accounts one at a time and managed about **300
per second, getting worse**: four hours for five million, extrapolating
badly. `TrieNode::put` returns a new root rather than mutating, which is
what makes the trie persistent and is right for execution — but rebuilding
the path clones the siblings along it, and `NodeRef::Node` owns its subtree,
so cloning one deep-copies everything beneath it. In memory that is O(n) per
insertion and O(n²) for the load.

Putting an LRU in front of the store made it **slower**, 63 s to 136 s for
20,000 accounts. That is what pointed at the clone rather than at the
reads: a bigger batch keeps more of the tree in memory, and more of the tree
in memory is more to copy.

Given all the entries up front the trie can be built the other way — leaves
first, each node written once and never modified, nothing cloned because
nothing changes. The entries must be sorted, which is what makes a subtree a
contiguous range and lets the build close one and forget it. Memory becomes
the depth of the recursion rather than the size of the tree.

**200,000 accounts went from twenty-one minutes to 2.1 seconds.**

The test that matters for correctness is that a bulk-built trie has the same
root hash as one built by repeated insertion, over sizes from one entry to a
thousand. The subtlety it caught: a node small enough is written *inside*
its parent rather than referenced by hash, so handing the parent a hash
produces a different root — wrong at two entries while five hundred still
matched, because at five hundred almost nothing is small enough to embed.

The fixture's store deliberately has no write-ahead log and no fsync. A
crash leaves a directory to delete and rebuild, which is the right trade for
a file whose only purpose is to be read by a benchmark, and the wrong trade
for anything else — which is why it lives in an example rather than in the
storage crate where a node could reach it.

## The workload

Transfers between accounts **that already exist in the fixture**. That
choice does more work than it looks:

- **No new accounts, so the trie's depth never changes.** Updating an
  account rewrites its leaf and its ancestors; the structure is untouched.
  A run that added accounts would be measuring a deepening trie as well as a
  growing one, and could not separate the two.
- **Reads and writes scatter over the whole gigabyte**, because recipients
  are drawn from all 9.3 million. A workload confined to a hundred reused
  accounts would have every path in cache from the second block, and the
  fixture would be ballast.

Senders are drawn from the 93,000 keyed accounts, recipients from all
9.3 million. Every transfer is native rBTC — see **The transactions** below
for the mix and the gas, which turn out to decide the result.

### What the trie does during a run

Nothing is collected, so the garbage accumulates and the directory grows.
For `U` accounts touched per block in a trie of depth 23, the nodes
rewritten are about `2U` for the shared top of the tree, plus `U × (23 −
log₂U)` for the disjoint tails, plus `U` leaves:

| transactions/block | nodes rewritten | per block | over 3,000 blocks |
|---|---|---|---|
| 50 | ~1,900 | 222 KB | 666 MB |
| 100 | ~3,700 | 420 KB | 1.26 GB |
| 200 | ~6,900 | 795 KB | 2.33 GB |
| 500 | ~16,000 | 1.84 MB | 5.5 GB |

The runs below use the 200 row over 100 blocks, which predicts about 78 MB
of growth. Measured: the directory went from 1,019 MB to 1.1 GB, so the
estimate is the right order and a little low — RocksDB keeps obsolete
versions until compaction.

The growth is **not an artifact to remove**. A real node lives it, and the
I/O it causes is part of what is being measured. It does mean the rate may
drift down over a run, and that drift has to be distinguished from a cache
effect — which is why the result is reported as a time series rather than an
average.

### The garbage collector will not run

The epoch store collects only state roots buried `burial_depth` blocks
deep, and the production configuration is:

```rust
EpochConfig { epochs: 4, burial_depth: 4_000, rotate_bytes: 1 << 30 }
```

A run of 100 blocks is **far shorter than the burial depth**, so no root
ever becomes eligible and the collector runs zero times — regardless of how many
rotations the writes trigger. That is convenient here, because the collector
would be noise for this question, but it means these numbers say nothing
about what collection costs. Measuring that needs its own longer run.

## The subtests

Four runs, identical but for the thread count:

| run | threads | parallel sublists |
|---|---|---|
| 1 | 1 | 4 |
| 2 | 2 | 4 |
| 3 | 3 | 4 |
| 4 | 4 | 4 |

The blocks are partitioned into four parallel sublists in every run, so the
work is identical and only the concurrency available to execute it changes.
Run 1 is the baseline: the same partitioned blocks executed one sublist
after another.

**Every run executes the same 100 blocks.** Not the same duration: the
first attempt time-boxed each run to a minute, which let the thread counts
run different numbers of blocks and so sample different parts of the decay
curve — the faster configuration got further into the warm region and was
credited for it. Comparing rates requires comparing the same work. One
hundred blocks is about three and a half minutes at one thread and under
three at four.

### Four sublists is above the consensus limit, deliberately

`TX_EXECUTION_THREADS` is **2** — rskj's value, and therefore the network's.
RSKIP-144 itself specifies `maxTransactionExecutionThreads = 4`, and the
code is the stricter of the two.

**Blocks partitioned four ways are not valid on today's network**, and the
benchmark does not pretend otherwise: it measures the execution engine at a
partition width the current rule forbids, to answer what raising the cap to
the RSKIP's own value would buy. The consensus constant is not changed; the
width is a parameter of the benchmark alone.

The two-thread row is the one that describes what the network could do
today.

## What is measured, and how

Per block: wall-clock execution time, transactions, gas, and the trie's size
on disk. Written as a CSV time series, one row per block, so that cold start
and drift are visible rather than averaged away.

Reported per run: milliseconds per block by decile, which is the shape of
the question "is there a cold start, and does it get worse as the trie
grows". Also the run's **%CPU**, from `/usr/bin/time`, which is what
distinguishes a run that is waiting from one that is computing — and the
measurement that turned out to carry the answer.

### Grafana was asked for and is not available

**Grafana is not installed on this machine.** Prometheus is, and it scrapes
the node at `127.0.0.1:9898` every 30 seconds.

Neither helps much here. The benchmark is not the node, so it is not a
scrape target; and a 30-second interval over a five-minute run is ten
points, which cannot show a cold start that lasts a few seconds. The CSV is
per block, which is three orders of magnitude finer, and it is a file rather
than a database so the runs can be compared and kept.

If the series is wanted in Grafana afterwards, the CSV imports; that is a
better order of operations than lowering the scrape interval and hoping the
run lines up with it.

## Running conditions

The mainnet node is **stopped for the duration**. It shares a 7 GB machine
with four cores, and a node following the chain competes for exactly the
resources being measured. Earlier measurements on this host were explicit
about what a single-shot benchmark on a busy machine is worth.

Between runs the fixture is restored from a backup taken before the first
one, because each run leaves its own writes behind and the second run would
otherwise start from a trie the first one grew.

```sh
cd /mnt/import/tn-snapshot/regtest-parallel-bench
rm -rf run
cp -a  state-1gb.backup run
```

Each run works on a throwaway copy called `run` and `state-1gb.backup` is
never opened by the benchmark, so no run can damage the fixture by
crashing part way through.

### And the page cache is evicted before each run

The `cp -a` above leaves the whole gigabyte in the page cache, so a run
started straight after it reads from RAM and measures a cold start that is
not there. The first run of the day sees a cold disk and every later one
does not, which makes the runs incomparable in the one dimension the
benchmark is about.

`drop_caches` is not available — `/proc` is read-only on this host — so the
cache is evicted by filling it with something else, reading 4 GB of
unrelated data on a machine with 7 GB of RAM:

```sh
# as run.sh does it: 64 MB from each of up to 200 unrelated files,
# stopping at 4 GB
dd if="$f" of=/dev/null bs=1M count=64 status=none
```

Crude, and sufficient: `free` shows the fixture's pages gone afterwards, and
the first block of each run costs what a cold first block costs.

## What the benchmark was given

Recorded so the run can be repeated, not inferred from the result:

| | |
|---|---|
| RocksDB block cache | **256 MB**, with bloom filters at 10 bits/key |
| peak process RSS | **375–411 MB** measured across the four runs |
| machine | 4 cores, 7 GB, mainnet node stopped |
| fixture | 9.3M accounts, 1,019 MB, root `0x5a428782…` |
| blocks per run | 100 (20,000 transactions, 420M gas) |
| transactions per block | 200, in 4 parallel sublists of 40 plus 40 sequential |

The cache size is the node's own: `epoch_store.rs` sets
`BLOCK_CACHE_BYTES = 256 MB` and the same bloom filter terms. An earlier
attempt ran against RocksDB defaults — no cache at all — and measured 4.5
seconds a block, almost all of it disk. That is a measurement of a
configuration no node uses.

Fixture and runner live in
`/mnt/import/tn-snapshot/regtest-parallel-bench/`. **That path is
misleading and should be moved**: this is regtest data, not testnet. It sits
there only because `/mnt/import`'s root is owned by root and this account
cannot create a top-level directory. One `mv` fixes it; a README in the
directory says so.

## The transactions

**Every transaction is a plain rBTC transfer.** No tokens, no contract
calls, no ERC-20: 100% native, 0% anything else.

| | |
|---|---|
| per transaction | 21,000 gas, value 1 wei, empty calldata |
| per block | 200 transactions, **4,200,000 gas** |
| state touched | two account cells — the sender's and the recipient's |

That choice is worth stating plainly because it decides the result. A
native transfer is the **least compute-intensive transaction there is**: a
balance check and two balance updates, against two random paths down a
23-level trie — about 46 node reads scattered over nine million accounts.
The ratio of I/O to arithmetic is as lopsided as it can be made.

So this workload is close to a worst case for parallel execution, and the
numbers below should be read that way. A block of contract calls does far
more work per state access, and the `parallel_workload` tool already builds
ERC-20 transfers for exactly that comparison. Running the benchmark against
them is the obvious next measurement and has not been done.

## Results, 2026-10-20

Four runs of 100 blocks each, identical but for the thread count, each
starting from a pristine copy of the fixture with the page cache evicted
first. 20,000 transactions and 420 million gas per run.

| threads | wall | tx/s | vs 1 thread | **CPU** |
|---|---|---|---|---|
| 1 | 217.9 s | 93 | — | 48% |
| 2 | 193.1 s | 105 | 1.13× | 71% |
| 3 | 200.7 s | 101 | 1.09× | 72% |
| 4 | **163.3 s** | 125 | **1.33×** | 86% |

### The bottleneck is the disk, and the CPU column proves it

On a four-core machine a compute-bound run at four threads approaches
**400%** CPU. This one reaches **86%** — less than one core fully busy. The
single-threaded run manages 48%, meaning it spends about half its time
waiting rather than computing.

That is the whole story of the table above. Adding threads does not buy
more computation, because computation was never the constraint; it buys
*overlapping waits*, and only until the disk itself saturates. Which is why
four threads returns 1.33× rather than anything near four.

### Warm, the gain nearly vanishes

Per-block milliseconds by tenth of each run:

| threads | 1 | 2 | 3 | 4 | 5 | 6 | 7 | **8** | 9 | 10 |
|---|---|---|---|---|---|---|---|---|---|---|
| 1 | 5,829 | 2,835 | 2,405 | 1,832 | 1,605 | 1,724 | 1,223 | **1,184** | 1,518 | 1,363 |
| 2 | 4,562 | 2,463 | 1,957 | 1,670 | 1,443 | 1,321 | 1,195 | **1,092** | 1,338 | 1,995 |
| 3 | 3,507 | 2,648 | 2,160 | 1,876 | 1,639 | 1,527 | 1,333 | **1,226** | 1,525 | 2,355 |
| 4 | 2,636 | 2,068 | 1,795 | 1,571 | 1,404 | 1,249 | 1,189 | **1,067** | 1,243 | 1,830 |

At the best point — the eighth tenth, around block 80 — four threads is
**1.11×** one thread. The 1.33× in the headline table is earned almost
entirely in the cold opening, where there is more waiting to overlap.

**1.11× is the honest figure for this workload once warm**, and it is small
enough to say that parallel execution of native transfers against a disk is
not worth much. That is a statement about the workload, not about the
design: see the note on transaction mix above.

### Both of the things we were looking for are there

**A cold start, and a large one.** The first tenth is two to five times
slower than the eighth. With a gigabyte of state against a 256 MB cache the
warm-up takes about eighty blocks — far more than the 25-block runs this
replaced could see, and the reason those numbers were labelled preliminary.

**And it gets worse as the trie grows.** Every run reaches its best around
block 80 and then *slows again*: the tenth tenth is 15% to 90% worse than
the eighth. The trie grew from 1,019 MB to about 1.1 GB over the run, and
the cost of that growth — more nodes, RocksDB compaction, a colder cache
per byte — shows up as the upturn at the right of every row.

So the rate is not uniform over the run and cannot be made so by running
longer: it falls, bottoms out, and rises. An average over any window is an
artifact of which part of that curve the window covers, which is why the
series matters more than the mean.

### The three-thread anomaly was ours, and is fixed

In the 25-block runs three threads came out *slower* than two on the cold
blocks — 5,089 ms against 4,096 — which was `execute_block_in_parallel`
chunking sublists into fixed waves of `execution_threads`. Four sublists on
three threads is a wave of three and then a wave of one, with two threads
idle while the straggler finishes.

Workers now pull the next unclaimed sublist instead. The cold-phase
inversion is gone: three threads' first tenth is 3,507 ms against two
threads' 4,562. What remains is a 4% difference in the total, which at this
variance is not a signal.

### The exec/write split is not comparable across runs

The single-threaded run reports 552 ms of "write" per block and the others
about 34 ms. That is where the trie commit is counted, not how long it
takes: with one thread the block takes the single-run path and the caller
applies the changes; with two or more the sublists commit inside
`execute_block`. **Only the total is comparable**, which is what the wall
clock and the tx/s column use.

## Where the time actually goes, and what moved it

The result above says the work waits on the disk. The obvious next question
is *which* disk traffic, and the answer decided what was worth changing.

### Reads, not writes

Device bytes over 30 blocks, from `/proc/<pid>/io`:

| | per 30 blocks | per block |
|---|---|---|
| read | **963 MB** | 32 MB |
| written | 566 MB | 18.9 MB |

The writes are ~24× the ~795 KB of trie nodes a block actually produces,
which is RocksDB compaction, not the write path — `write_bytes` came back
within 1% of 566 MB in *every* configuration tried, including one with the
WAL switched off. In absolute terms it is about 9 MB/s, which this SSD does
not notice.

The reads are the cost, and the mechanism is amplification: ~32 MB fetched
to serve ~6,000 useful nodes of ~115 bytes each. **Trie keys are hashes, so
there is no locality at all** — a 4 KB SST block holds ~35 unrelated nodes
and a lookup wants one of them. This is also why a *bigger* RocksDB block
cache made things worse: at 1536 MB it took memory from the OS page cache,
which was serving those reads better, and the run went from 2,129 to 2,512
ms/block.

### Five changes, measured one at a time

30 blocks, 4 threads, fresh fixture and evicted page cache before each run.
`n` is how many times each was run; the spread on this machine is wide
enough that single runs decide nothing.

| | change | n | mean | vs baseline |
|---|---|---|---|---|
| A | no cache (`RocksDbTrieStore`) | 3 | 73.0 s | — |
| B | `CachedTrieStore` as it was | 3 | 123.8 s | **0.59×** |
| C | B, lock released before the read | 6 | **54.5 s** | **1.34×** |
| D | C + cache sharded 16 ways | 6 | 57.2 s | 1.28× |
| D2 | *control*: D on a recompacted fixture | 2 | 65.7 s | — |
| E | D2 + 1 KB SST blocks | 2 | 64.4 s | 1.02× vs D2 |
| F | E + no WAL | 3 | 63.6 s | 1.01× vs E |

All five produce the same final state root,
`0xfea7e236…97efdc`, so these are differences in speed and in nothing else.

**B is the finding.** The store the node runs was **1.7× slower than no
cache at all** under four threads, at 45% CPU and in one run 33%. Its `get`
held the single `Mutex` across the RocksDB lookup, so every cache miss — a
disk read, the thing this workload is made of — queued all four sublists
behind it. Adding a cache made the node slower, and parallel execution gave
back the concurrency it had just arranged.

**C is the fix and it is the whole win.** Releasing the lock before the
read: 2.27× over B, 1.34× over having no cache. Concurrent misses on the
same key now duplicate the fetch, which is waste and never a wrong answer —
a node is immutable and keyed by its own hash.

**D, E and F bought nothing.** Sharding the cache was aimed at lock
contention that no longer existed once the disk read was outside the lock;
what remains inside is a hash lookup, too short for four threads to collide
on. 1 KB SST blocks did not pay either, which refutes the prediction that
followed from the amplification figure above. The WAL result is the direct
test of "maybe it is writes": 1%.

Recompacting the fixture made it *slower* (D2 at 65.7 s against D at 52.8
s, CPU falling from 77% to 58%), which is unexplained. The control earns
its place: without it that loss would have been charged to the block size.

### Cache size matters more than expected, and not monotonically

With the lock fixed, 40 blocks, four interleaved repetitions each:

| LRU entries | mean | spread |
|---|---|---|
| 50,000 (the node's default) | 78.3 s | 54.6 – 105.7 |
| 200,000 | **70.8 s** | 62.7 – 74.6 |
| 500,000 | 97.3 s | 83.2 – 111.9 |

200,000 is both faster and far steadier. 500,000 is clearly worse, for the
same reason the 1536 MB block cache was: on a 7 GB machine it competes with
the page cache that is doing the real work. The right size is a property of
the machine, not of the trie, and 50,000 looks low for a gigabyte of state.

## An access list is worth more than the threads

100 blocks, 200 tx/block, the 1 GB fixture, `--prefetch 16`. The
generator knows the sender and the recipient of every transfer, so the
access list is **exact**: this simulates EIP-2930 at 100% coverage and
is therefore an upper bound. If perfect foreknowledge buys little, no
scheme that has to transmit, verify or guess the list can buy more.

| engine | threads | prefetch | wall | CPU |
|---|---|---|---|---|
| RocksDB | 1 | — | 181.7 s | 53% |
| RocksDB | 1 | 16 | **72.4 s** | 112% |
| RocksDB | 4 | — | 125.6 s | 96% |
| RocksDB | 4 | 16 | 72.7 s | 131% |
| FlatDB | 1 | — | 58.2 s | 74% |
| FlatDB | 1 | **16** | **50.4 s** | 92% |
| FlatDB | 4 | — | 57.3 s | 103% |
| FlatDB | 4 | 16 | 58.0 s | 106% |

Against the 1-thread RocksDB baseline, each lever on its own:

| | | |
|---|---|---|
| FlatDB alone | 58.2 s | **3.12×** |
| access list alone | 72.4 s | **2.51×** |
| four threads alone | 125.6 s | 1.45× |
| FlatDB + access list, one thread | **50.4 s** | **3.61×** |
| all three together | 58.0 s | 3.13× |

### Three things worth saying plainly

**Prefetching beats parallel execution, and it is not close.** One
thread with the access list (72.4 s) beats four threads without it
(125.6 s) by 1.73×. On RocksDB the list is worth 2.51× where the thread
count is worth 1.45×.

**The best configuration uses one thread.** FlatDB plus the access list
on a single thread is the fastest of the eight, and adding threads to it
makes it *slower* — 50.4 s against 58.0 s. Parallel sublist execution
contributes nothing once reads stop being the constraint, and then costs
something: each sublist rewrites the top of the trie independently
against the same base, and the writes serialise on FlatDB's writer mutex
(#319).

**The levers do not add up, because they attack the same bottleneck.**
All three — a faster store, a prefetched access list, more threads —
are ways of not waiting on a read. Once one of them has removed the
waiting, the others have nothing left to remove. That is why
FlatDB + prefetch + threads (3.13×) is worse than FlatDB + prefetch
alone (3.61×).

### Why a single descent cannot be batched

An access list gives *paths*, derived from addresses. A
content-addressed store retrieves a node by its **hash**, and a child's
hash is only learned by reading its parent — so knowing the destination
does not let a descent skip to it. What the list does give is N
descents that are independent *of each other*, and those can be in
flight at once. That is the whole mechanism, and it is why the gain
tracks how slow a single read is: RocksDB 2.51×, FlatDB 1.16×.

A path-keyed store — Geth's pathdb, Erigon's flat state — could jump
straight to the leaf. This one cannot, and that is a consequence of
content addressing rather than an oversight.

### What this does not establish

- The fixture is accounts only and its bottom-up layout favours FlatDB;
  against the real mainnet store the serial read advantage was 1.76×,
  not 3.1×.
- Native transfers touch two accounts each and the list is trivially
  predictable. Contract calls reach storage slots that depend on
  execution, which is exactly where a real access list is hardest to
  produce and most likely to be incomplete.
- 100% coverage is the ceiling, not a forecast.
- RSK has no EIP-2930 and never adopted EIP-2929, so there is no gas
  incentive for a sender to supply a list. It would have to come from a
  peer that has already executed the block — which is sound, because a
  prefetch hint that is wrong costs a wasted read and nothing else. It
  needs no consensus change, unlike the witness scheme measured in
  `profiling.md`.

## What this says

The workload is **I/O bound**. Four threads reach 86% of one core on a
four-core machine, and buy 1.33× over the whole run, 1.11× once warm.

That is a real answer to the question as asked, and a narrow one. It was
measured on the most I/O-heavy and least compute-heavy transaction that
exists — a bare value transfer, 21,000 gas, two account cells — against
state that does not fit in cache. Both halves of that push the result the
same way.

It also turned up something the question did not ask for and that matters
more than the answer: **the node's own trie store was holding a lock across
its disk reads**, which made it slower than no cache at all as soon as more
than one thread was executing. That is fixed, and the fix is worth 1.34×
against the unchanged baseline — more than the thread count buys.

What would move the parallel number itself:

- **Transactions that compute.** ERC-20 transfers, which the workload
  generator already builds, touch storage cells and do real work per
  access. If parallelism is worth anything here it will show there.
- **A working set that fits.** The gain shrinks as the cache warms, so a
  node whose state fits in RAM would see less, not more — but it would
  isolate the compute speedup from the I/O overlap, which is the number the
  design should be judged on.
- **Longer runs.** Eighty blocks to warm up and a visible upturn after
  means 100 blocks sees the bottom of the curve and little else.
- **Attacking read amplification directly.** 32 MB read to serve 6,000
  nodes is the real tax. Smaller SST blocks did not touch it; what might is
  storing the trie with keys that preserve locality rather than hashes, or
  caching decoded nodes rather than the blocks they arrive in.

Four sublists is above what the network allows; the two-thread row — 1.13×
over the run — is what is reachable today, and it is within the run-to-run
variance of this setup.

### A caution about this machine

Repeated runs of the same configuration vary by 20% and occasionally more
(50,000 entries at 40 blocks ranged from 54.6 s to 105.7 s). Four cores and
7 GB, with the state a seventh of RAM, is a setup where the page cache
decides a lot and the eviction between runs does not control it perfectly.
Every claim above rests on repeated interleaved runs, and the ones that
rest on two are labelled as such. Differences under about 10% on this host
should be treated as unproven.
