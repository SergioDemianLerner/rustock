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
9.3 million.

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

A run of 3,000 blocks is **shorter than the burial depth**, so no root ever
becomes eligible and the collector runs zero times — regardless of how many
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

Each run is sized to a few minutes. The block count is calibrated from a
short dry run rather than fixed in advance, because a count that takes two
minutes at four threads takes considerably longer at one, and the point is
to compare rates, not to compare how long we were willing to wait.

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

Reported per run: the rate over the first 10% of blocks against the last
10%, which is the shape of the question "is there a cold start, and does it
get worse as the trie grows".

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
rm -rf  /mnt/import/tn-snapshot/bench/state-1gb
cp -a   /mnt/import/tn-snapshot/bench/state-1gb.backup \
        /mnt/import/tn-snapshot/bench/state-1gb
```

## What the benchmark was given

Recorded so the run can be repeated, not inferred from the result:

| | |
|---|---|
| RocksDB block cache | **256 MB**, with bloom filters at 10 bits/key |
| peak process RSS | **324 MB** |
| machine | 4 cores, 7 GB, mainnet node stopped |
| fixture | 9.3M accounts, 1,019 MB, root `0x5a428782…` |
| blocks per run | 25 (5,000 transactions) |
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

## Results, 2026-10-20

Preliminary. Four runs of 25 blocks each, identical but for the thread
count, each starting from a pristine copy of the fixture with the page
cache evicted first.

| threads | wall | tx/s | vs 1 thread |
|---|---|---|---|
| 1 | 87.1 s | 58 | — |
| 2 | 60.8 s | 83 | 1.43× |
| 3 | 67.0 s | 75 | 1.30× |
| 4 | **52.5 s** | 96 | **1.66×** |

Same 5,000 transactions in every run, so the wall clock compares directly.

### The runs never reach a steady state

Per-block milliseconds, by fifths of each run:

| threads | 1st | 2nd | 3rd | 4th | 5th |
|---|---|---|---|---|---|
| 1 | 7,617 | 3,121 | 2,509 | 2,130 | 1,911 |
| 2 | 4,096 | 2,471 | 2,016 | 1,851 | 1,591 |
| 3 | 5,089 | 2,491 | 2,118 | 1,978 | 1,591 |
| 4 | 3,745 | 2,032 | 1,735 | 1,512 | 1,340 |

Every run is **still getting faster when it ends**. The first five blocks
take two to four times as long as the last five, and the last five are not
a plateau. So the headline table above is not a steady-state throughput; it
is an average dominated by a warm-up that had not finished.

This is the cold start the fixture was built to expose, and it is larger
than expected: a gigabyte of state against a 256 MB cache takes well over
25 blocks to settle. **A measurement of steady-state throughput needs a
much longer run** — the obvious next step, and the reason these numbers are
labelled preliminary.

### Parallelism helps most while the cache is cold

Speedup against one thread, cold fifth against warm fifth:

| threads | coldest fifth | warmest fifth |
|---|---|---|
| 2 | 1.86× | 1.20× |
| 3 | 1.50× | 1.20× |
| 4 | **2.03×** | **1.43×** |

The gain roughly halves as the cache warms. That is consistent with what
dominates here: 200 transactions touch about 9,200 trie nodes scattered
over nine million accounts, and when those are disk reads the threads are
mostly hiding each other's I/O latency rather than computing in parallel.
Warm, there is less latency to hide and the gain shrinks towards what the
execution itself is worth.

Which also means **these numbers do not transfer to a node whose working
set fits in cache**. There the parallel gain would be smaller again.

### Three threads is worse than two, and that is our bug

Three threads is slower than two in the cold fifth (5,089 ms against
4,096) and identical warm. The cause is in the implementation, not in the
workload: `execute_block_in_parallel` runs the sublists in **fixed waves**
of `execution_threads`. Four sublists on three threads is a wave of three
and then a wave of one, and the second wave leaves two threads idle while
one sublist finishes alone.

A work-stealing pool, or simply assigning sublists to threads rather than
chunking them, would remove this. It is worth fixing before anyone reads a
thread-count curve from this benchmark as a property of the design.

### The exec/write split is not comparable across runs

The single-threaded run reports 505 ms of "write" per block and the others
about 31 ms. That is an artifact of where the trie commit happens: with one
thread the block takes the single-run path and the caller applies the
changes, so the commit lands in the write column; with two or more the
sublists commit inside `execute_block` and it lands in execution. **Only
the total is comparable**, which is what the wall clock and the tx/s column
use.

### Trie growth

Each run grew the fixture from 1,019 MB to about 1.1 GB over 25 blocks,
which is roughly 4 MB a block against the 795 KB the analytical estimate
predicted. The gap is RocksDB write amplification on an uncompacted store,
plus the parallel path committing once per sublist rather than once per
block — intermediate roots a single commit would not write. Worth measuring
properly rather than estimating, over a longer run.

## What this says so far

Four threads over four sublists is **1.66× one thread** on this workload,
and about **1.43×** once the cache is warm. The warm figure is the one to
quote, and it is the lower bound of the two.

Two threads — the partition width the network actually allows today — is
**1.43× cold and 1.20× warm**.

All of it rests on 25-block runs that never reached a steady state, on one
workload shape, on a host with four cores, and with a known scheduling bug
costing the three-thread case. It is enough to say that parallel execution
is worth something here and roughly how much. It is not enough to put a
number in a release note.
