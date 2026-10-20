# Measuring what parallel sublist execution buys

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
| root | `0x4622e441e12d77e832fa8b54f44801568bb7c08c57c37c9470b37e2b8f89a2e2` |

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

## What this says

The workload is **I/O bound**. Four threads reach 86% of one core on a
four-core machine, and buy 1.33× over the whole run, 1.11× once warm.

That is a real answer to the question as asked, and a narrow one. It was
measured on the most I/O-heavy and least compute-heavy transaction that
exists — a bare value transfer, 21,000 gas, two account cells — against
state that does not fit in cache. Both halves of that push the result the
same way.

What would move it:

- **Transactions that compute.** ERC-20 transfers, which the workload
  generator already builds, touch storage cells and do real work per
  access. If parallelism is worth anything here it will show there.
- **A working set that fits.** The gain shrinks as the cache warms, so a
  node whose state fits in RAM would see less, not more — but it would
  isolate the compute speedup from the I/O overlap, which is the number the
  design should be judged on.
- **Longer runs.** Eighty blocks to warm up and a visible upturn after
  means 100 blocks sees the bottom of the curve and little else.

Four sublists is above what the network allows; the two-thread row — 1.13×
over the run — is what is reachable today, and it is within the run-to-run
variance of this setup.
