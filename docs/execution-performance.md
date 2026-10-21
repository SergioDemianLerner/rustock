# Three ways to make block execution faster, and the defect that was worth more than all three

October 2026. A synthesis of three lines of work that turned out to be
the same line: parallel sublist execution (RSKIP-144), replacing the
trie store with FlatDB, and prefetching a block's access list.

Each was measured on its own and all three were measured together. They
do not add up, and the reason they do not is one of the two most useful
things here.

The other is that profiling the three of them eventually pointed at a
line of code none of them was about. Applying a block's changes to the
trie took time quadratic in the number of accounts the block touched,
because the type holding a child node could not share one. Fixing that
was worth more than the three levers put together, and it moved where
the next bottleneck is. Sections written before it are marked.

The detail lives in
[`parallel-execution-benchmark.md`](parallel-execution-benchmark.md),
[`flatdb.md`](flatdb.md) and [`profiling.md`](profiling.md). This is the
part you need to decide what to build.

---

## The short version

**Start with the fourth thing, which is not a lever and is worth more
than the three that are.** Applying a block's changes to the trie cost
quadratic time in the number of accounts the block touched, because
`NodeRef::Node` held a `Box<TrieNode>` and a `Box` cannot share — so
every descent deep-copied the subtrees it passed. One `Arc` later, the
commit is 15× cheaper and a block is 3.4× faster end to end. See
"Where a block's time actually goes".

| 40 blocks, 200 transfers, FlatDB, medians of three | tx/s |
|---|---|
| one thread | 401 → **926** |
| four threads | 392 → **1,357** |
| one thread + access list | 471 → **1,530** |
| four threads + access list | 411 → **1,582** |

Left of the arrow is the code before the copying was removed, right of
it after. Everything below the next heading that predates it is marked.

---

Of the three levers proper, measured against one thread on RocksDB,
100 synthetic blocks of 200 native transfers against a 1 GB state
**before the copying fix**:

| | wall | speedup |
|---|---|---|
| baseline — RocksDB, one thread | 181.7 s | — |
| four threads | 125.6 s | 1.45× |
| access list, prefetched 16 ways | 72.4 s | **2.51×** |
| FlatDB instead of RocksDB | 58.2 s | **3.12×** |
| **FlatDB + access list, one thread** | **50.4 s** | **3.61×** |
| FlatDB + access list + four threads | 58.0 s | 3.13× |

Three things were worth reading twice, and one of them did not survive.

**Parallel execution is the weakest of the three levers.** It is also
the only one that needs a consensus change. Still true.

**The best configuration uses one thread.** ~~Adding threads to the
fastest setup makes it slower.~~ **Withdrawn.** That was an artefact of
the quadratic commit, which four sublists paid four times. Re-measured
after the fix, one thread and four are *indistinguishable* with the
access list prefetched — 1,530 against 1,582 tx/s, medians of three,
with 15% spread on each. Without the access list four threads now win
1.47×. Adding threads is no longer a loss; it is still not a reason to
add threads.

**The levers do not compose.** All three are ways of not waiting on a
read. Once one of them has removed the waiting, the others have nothing
to remove. Still true, and sharper now: with the commit cheap, the
access list is worth 1.65× on one thread (926 → 1,530) where it was
worth 1.17× before (401 → 471) — acceleration that was being thrown
away into a phase that dominated the block.

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

**The load test measures execution and commit, and nothing else.**
Senders are passed to `execute_block` rather than recovered from
signatures, so `ecrecover` never runs. The header carries
`difficulty: 1`, no merged-mining fields, and zeroes for
`transactions_root` and `receipts_root`. A `BlockProcessor` is built
and deliberately not called (`let _ = &processor;`). So signature
recovery, proof-of-work and header validation, the transaction and
receipt tries, and persisting the block are outside every number in
this document. All of them are per-transaction CPU work that divides
cleanly across threads, so their absence makes parallel execution look
*worse* than it is, not better.

**This is also close to a worst case for parallel execution and should
be read that way.** A native transfer is the least compute-intensive transaction
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

> The 431.7 includes the sequential sublist's *execution*, which the
> phase timer wrongly folded into the commit; see "Splitting the commit
> across sublists costs nothing" below. It does not change what this
> section goes on to find — the commit was quadratic, and that is
> measured directly rather than through this table — but a fifth of
> that number is transactions being run, not trie work.

**The commit cost 5.1× what executing the transactions cost**, and
80–84% of the measured work was sequential whatever the thread count.

That is what sent the next measurement at the commit, and the commit
turned out to be quadratic. The rest of this section is the finding;
the numbers above are what it looked like from outside.

### The commit was quadratic in the accounts a block touched

`NodeRef::Node` held a `Box<TrieNode>`. A `Box` is a unique owner, so
cloning one must clone what it points at — and a `TrieNode` holds two
more `NodeRef`s, so the copy recursed through the whole in-memory
subtree. `internal_put` paid that twice at every level of its descent:

```rust
let child_ref = if bit == 0 { &self.left } else { &self.right };
let child = child_ref.resolve(store).unwrap_or_else(TrieNode::empty);
...
let (new_left, new_right) = if bit == 0 {
    (new_ref, self.right.clone())      // the sibling
} else {
    (self.left.clone(), new_ref)
};
```

and `resolve` was the worse of the two, because it copied even when
there was nothing to read:

```rust
NodeRef::Node(n) => Some(*n.clone()),   // deep copy, no store access
```

A block begins with every child a `NodeRef::Hash` — 32 bytes, free to
clone — and each put leaves a `NodeRef::Node` behind it. So put number
*k* copied what puts 1..k-1 had built. Measured directly against a
100,000-key trie reloaded from the store, three interleaved repetitions
of the whole ladder per run, mainnet node stopped and the machine
otherwise idle:

| puts in the block | µs per put, `Box` | µs per put, `Arc` |
|---:|---:|---:|
| 50 | 72.1 | **14.7** |
| 100 | 119.4 | **11.9** |
| 200 | 210.7 | **12.2** |
| 400 | 390.1 | **11.8** |
| 800 | 663.0 | **11.3** |
| 1600 | 1,215.8 | **11.8** |

The left column doubles every time the block does. The right column
does not move. `crates/trie/examples/put_scaling.rs` is that table; a
second round reproduced both arms within 5%.

The fix is `Arc<TrieNode>`, whose `clone` is an atomic increment.
Sharing is sound because a `TrieNode` is immutable once constructed,
which the code already depended on for `hash_cache`. The one obstacle
was `saved`, set through `&mut self` by `collect_for_save`, which an
`Arc` cannot hand out; it is an `AtomicBool` now, and since the flag is
only ever set and never cleared, two owners of a shared node cannot
disagree about it.

### What it bought

40 blocks of 200 transfers on FlatDB, 401 accounts changed per block,
three rounds alternating the two arms, mainnet node stopped and no
other load on the machine. Medians:

| | wall | tx/s | commit, ms/block |
|---|---:|---:|---:|
| `Box`, one thread | 21.1 s | 401 | 302.4 |
| `Box`, four threads | 21.6 s | 392 | 422.7 |
| **`Arc`, one thread** | 9.8 s | 926 | **19.9** |
| **`Arc`, four threads** | **7.1 s** | **1,357** | 46.6 |

The commit is **15.2× cheaper** on one thread and 9.1× on four. A block
is 2.15× faster on one thread and 3.04× on four.

**All twenty-four runs across both experiments ended on the same state
root**, `0x9efbc466…` — two representations of the node graph, two
thread counts, with and without prefetch. That is a stronger check than
the 2,383 unit tests, and it is the reason to believe the change is
purely one of representation.

**The sign of parallelism flipped.** With `Box`, four threads returned
0.98× of one thread: the 55 ms they saved executing cost 120 ms more to
commit. With `Arc` they return 1.47×. Parallel execution did not get
better — it stopped being buried under a sequential phase that cost
five times what it saved.

Three of the four phases that were listed as unprofiled were negligible
all along — maintenance, root hashing and reload total 5.5 ms. The
fourth was a defect.

### Why these numbers are trustworthy where the wall clock is not

The wall clock for one thread came out 86.8 / 41.0 / 37.8 s across three
runs of identical code: a 2.3× spread. The phase figures over the same
runs repeated within 2% — sublists 83.9 / 84.2 / 108.8, commit 430.4 /
439.3 / 431.7.

That is the expected split. Wall clock includes cold-start I/O, which is
what varies wildly on this host. The phases measure CPU over structures
already in memory.

### How it looked before the cause was found

Instrumenting the single-threaded path too — it applies the changes in
the caller rather than inside the executor, so it had been reporting
nothing — makes the cost explicit. Identical work, 401 accounts changed
per block, three interleaved repetitions:

| | commit, ms/block | |
|---|---|---|
| one thread | 303.8 / 320.2 / 308.6 | median **308.6** |
| four threads | 459.3 / 479.0 / 614.8 | median **479.0** |

**The four-thread row is not comparable to the one-thread row.** It
carries the sequential sublist's execution, which the phase timer
included in the commit; the one-thread path does not go through that
code at all. The comparison below was drawn before that was noticed.

~~**Four sublists cost 55% more to commit than one**, for the same
changes.~~ **Withdrawn.** The four-thread figure includes the
sequential sublist's execution and the one-thread figure does not,
because the phase timer spanned both. Measured again with them
separated, five applies cost 1.04× one apply — nothing. See "Splitting
the commit across sublists costs nothing".

So the trade parallel execution appeared to make was to save about
84 ms of execution and spend about 170 ms more on the commit. It lost,
and that is why four threads barely beat one and lost outright once the
store was fast and the access list prefetched.

The explanation offered at the time was that each sublist computes its
changes against the same `base_root`, so `apply_state_changes` runs
once per sublist and each rebuilds the upper trie on top of the
previous one's result, where one thread rebuilds it once. That is true
and it is not the reason the penalty was large. Four rebuilds of a
linear-cost structure would have cost four times very little. They were
expensive because each rebuild was quadratic.

After the fix the same ratio is still there — 46.6 ms against 19.9, so
four sublists cost 2.34× one — and it no longer matters, because it is
26 ms. Worth knowing if someone raises the sublist count; not worth
attacking at four.

### And the per-account cost grew with the block

This was the measurement that gave the cause away, and it was nearly
dismissed. Four threads, medians of three:

| accounts | commit | per account |
|---|---|---|
| 101 | 98.8 ms | 978 µs |
| 201 | 174.9 ms | 870 µs |
| 401 | 479.0 ms | 1,195 µs |

Doubling 201 to 401 accounts multiplied the commit by 2.74; doubling
101 to 201 multiplied it by 1.77. It was written down as **suggestive
of superlinearity and not established**, because the 101-account point
measured 80.1 / 98.8 / 121.7 — a 52% spread, on a machine that was
sharing four cores with the mainnet node.

Refusing to claim it was right. **Three points with that much noise
cannot establish a shape.** What they can do is justify building an
instrument that measures the shape directly, over a wider range and
without the rest of a block in the way, which is what
`put_scaling.rs` does: six points across a 32× range, and the answer
was unambiguous in the first run.

The candidate named here at the time was the right one: *"`TrieNode::put`
returns a new root, and `NodeRef::Node` owns its subtree, so rebuilding
a path deep-copies the siblings along it."* It understated the problem
by half — `resolve` copies the child on the descent path too, not only
the sibling — but it pointed at the line.

Hashing was correctly ruled out: `hash_cache` is a `OnceLock` and
`put_impl` never touches `compute_hash` or `to_message`, so building the
trie hashes nothing. The whole dirty subtree is hashed once at the end,
for 5.1 ms, which is now a third of the commit rather than a hundredth.

### What to attack

The commit is no longer it. At 19.9 ms against 5.5 ms of root hashing
and 57 ms of execution, nothing in the block is dominant enough to be
the obvious target.

What the measurement could not see was the next question, and it was a
larger one than it sounded: **the load test executed transactions and
committed state, and nothing else.** `--validate true` now adds the
rest, and the answer is in the next section.

## The rest of a block: what validation costs

Adding `--validate true` — sender recovery, the full header rule set
with merged-mining proof of work, the body rules, the transaction and
receipt tries, the REMASC transaction every real block carries, and
persisting the block — cost **28% of throughput**. Medians of three
interleaved rounds, mainnet node stopped, 40 blocks of 200 transfers:

| | wall | tx/s | validation | persist |
|---|---:|---:|---:|---:|
| execution only, 4 threads | 5.9 s | 1,356 | — | — |
| **and the rest of a block** | 8.2 s | **976** | 52.6 ms | 0.9 ms |

Of the 53 ms, **50 was recovering senders** — 248 us each, once per
transaction. The header costs 6.6 us, measured separately over 300 real
mainnet headers; the body rules and the two tries round out the rest.

### It was the largest thing in the block, and it was serial

`BlockProcessor::recover_senders` was a plain sequential loop, run
before any execution thread existed:

```rust
for (i, tx) in transactions.iter().enumerate() {
    let sender = tx.recover_sender(chain_id)?;
    senders.push(sender);
}
```

Every item of it is independent — no ordering, no shared state, no
I/O, one signature each — and all of it sat in the part of a block
that no thread count divides. It is not needed up front either: the
senders are zipped into `(Transaction, Address)` pairs and handed to
the executor, and nothing validates nonces before execution.

Two things were wrong with it, and they multiply.

**The library.** Recovery went through `k256`, pure Rust. Measured
against `secp256k1` — the rust-bitcoin bindings to Pieter Wuille's C
library — on three hundred real mainnet transactions, medians of five
rounds, both backends agreeing on every address:

| | median | floor |
|---|---:|---:|
| `k256` | 247.9 us | 230.4 us |
| `secp256k1` | **58.8 us** | 51.7 us |

**The schedule.** Nothing about recovery has to be serial.

### What both were worth

Three interleaved rounds, node stopped, all twelve runs on the same
state root — the backend and the schedule changed, not what a
signature means:

| | wall | validation | us/tx |
|---|---:|---:|---:|
| `k256`, serial, 1 thread | 9.1 s | 53.7 ms | 267 |
| `k256`, serial, 4 threads | 9.5 s | 53.4 ms | 265 |
| `secp256k1`, 1 thread | 5.9 s | **13.6 ms** | 68 |
| **`secp256k1`, parallel, 4 threads** | **5.2 s** | **7.2 ms** | **36** |

- the library alone is **3.95×**, at one thread where the schedule
  cannot help;
- together they are **7.4×** on the phase;
- the block is 1.83× faster at four threads, and validating throughput
  goes from 842 to **1,538 tx/s**.

Each arm's six phase measurements fall within 1.3%, so all of this is
far above the noise.

**Parallel recovery returns 1.89× on four real cores, not 4×.** Of the
13.6 ms at one thread, about 11.8 is recovery and 1.8 is the header,
rules and roots, which stay serial; dividing only the divisible part
by four predicts 4.8 ms against the 7.2 measured, so recovery itself
scaled about 2.2×. Thread creation per block and memory bandwidth are
the obvious candidates and neither has been measured. Worth a look if
someone wants the remaining 1.8×.

**And a trap for whoever optimises this next.** Nothing on the recovery
path takes a lock today: `signing_hash` only reads `cached_rlp`, and
libsecp256k1's verification context is immutable once built. A sender
cache is the obvious next step — rskj keeps one, and most of a block's
transactions already passed through our own pool, which recovered them
there. A `Mutex<HashMap>` consulted per transaction would serialise
exactly this region again, and would do it invisibly: the work still
happens, one thread at a time. Shard it, make it lock-free, or look the
whole block up once before the threads start.

## A full block, with everything on

Everything above runs 200 transfers a block, which is 4.2M gas against
a 25M limit. Here is a block filled to the limit: **1,190 native
transfers, 24,990,000 gas**, with Arc-shared trie nodes, FlatDB, a
prefetched access list and parallel libsecp256k1 recovery. Four
threads, four sublists, medians of three interleaved rounds, mainnet
node stopped. Each arm ended on one state root across its three runs.

> **The 4-thread commit figures below and everywhere above are
> overstated.** The phase timer started before the ordered commit and
> stopped after the *sequential sublist had executed* — a fifth of the
> block's transactions, which is not trie work. `sequential_us` now
> counts that separately. Re-measured at 1,190 transactions, the commit
> is 104.5 ms and the sequential sublist 35.3; the tables here still
> show the two added together. The single-thread figures are unaffected:
> that path does not go through `execute_block_in_parallel`.

| | execution only | **whole block** |
|---|---:|---:|
| **sum of phases** | **388.0 ms** | **420.1 ms** |
| wall ÷ blocks | 555 ms | 595 ms |
| transactions/s | 3,067 | **2,833** |
| gas/s | 64.4 M | **59.5 M** |

Both time figures are real and measure different things. The sum of
phases is what processing a block costs in steady state. The wall
figure also carries opening the fixture and the cold first block,
spread over only twenty blocks, which does not amortise. Quote the
first against a benchmark that measures blocks, the second against one
that starts a node.

| phase, ms/block | execution only | whole block |
|---|---:|---:|
| access-list prefetch | 46.2 | 42.3 |
| **validation** (recovery, header + PoW, rules, tries) | — | **40.7** |
| execution | 222.0 | 213.8 |
| ├ sublists — the only parallel part | 76.7 | 70.8 |
| └ ordered commit | 141.6 | 141.8 |
| write (root hash, save, flush) | 112.2 | 116.0 |
| └ root hash | 30.3 | 29.9 |
| lazy reload | 7.6 | 7.3 |

**The commit is the largest phase again**, though by less than this
table first said. Separating the sequential sublist's execution out of
it gives, medians of three at 1,190 transactions: sublists 79.1 ms for
952 transactions across four threads, **sequential 35.3 ms** for the
other 238 on one, and **commit 104.5 ms** for the five
`apply_state_changes` calls. It is linear now rather than quadratic,
but it grows with the accounts a block touches — 2,381 here — and no
sublist divides it.

### Splitting the commit across sublists costs nothing

Worth recording, because the obvious fix for a large ordered commit is
to merge the five `apply_state_changes` calls into one, and the
measurement says there is nothing there. Medians of three interleaved
rounds, 1,190 transactions, node stopped:

| | commit, ms/block |
|---|---:|
| one thread — **one** apply over all 2,381 accounts | 100.4 |
| four threads — **five** applies, chained | 104.5 |

1.04×, against an 8% spread within the one-thread column. **The cost is
per put, not per call.** The same 2,381 descents happen either way, and
each rebuilds its own path; five calls add five root clones and five
map iterations, which an `Arc` clone makes invisible.

This also withdraws the "four sublists cost 55% more to commit than
one" result recorded earlier in this document. That was the sequential
sublist's execution being counted inside the commit timer, not applies.

What *would* reduce it is algorithmic. Those 100 ms are 2,381 descents
of about 23 levels, some 55,000 node allocations a block, and every
shared ancestor is rebuilt once per descendant that passes through it.
Sorting the keys and building the affected subtrees bottom-up touches
each ancestor once. `crates/trie/src/bulk.rs` already does this — it is
what made the fixture build linear — and the commit does not use it.

**Validation is no longer the problem**: 40.7 ms of 420, under 10%.
With serial `k256` it would have been 1,190 × 248 us = 295 ms, and the
block would take about 675 ms instead of 420.

**Read the workload before comparing this to anything.** 1,190 native
transfers is the cheapest possible block in computation and the most
expensive in I/O per unit of gas. A real 25M-gas block of contract
calls does far more EVM work per state access. These 420 ms are not
"a 25M-gas block" in general; they are a 25M-gas block of transfers.
A comparison against another client on a different mix measures the
two mixes as much as the two clients.

## What is left

Amdahl's arithmetic has to be redone. The old reasoning — at most a
third of a block is parallelisable, because the maintenance run, the
ordered commit, the root hashing and the lazy reload are all sequential
— was correct about which phases are sequential and wrong about what
they weigh. With the copying gone, on four threads with the access list
prefetched, a block is about 39 ms of sublist execution against 39 ms
of commit, 6 ms of root hashing and 1 ms of reload. The sequential
share fell from 80–84% to roughly half, and most of what is left is a
commit that now costs 26 ms more on four sublists than on one — an
overhead of parallelism rather than a floor under it.

That is why the one-thread recommendation is withdrawn rather than
reversed. The structural ceiling moved; what is above it has not been
measured on a workload that includes the rest of block validation.

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

**Ship the `Arc` change first.** It is the largest single improvement
measured in any of this work, it needs no consensus change, no wire
format and no operational plan, and it is a strictly local change to
how the trie holds its children. 3.4× end to end, 15× on the commit,
and twenty-four runs on the same state root.

**Then the recovery work**, for the same reasons: local, no consensus
exposure, 7.4× on the phase and 1.83× on a validating block, twelve
runs on one state root. The library swap is the larger half and the
simpler one.

**Deploy FlatDB.** 1.76× on the read pattern execution actually uses, a
thirtyfold smaller memory demand, no compaction, and a simpler
durability story. Running on the mainnet node now. The remaining
operational items are in
[`flatdb-rollout.md`](flatdb-rollout.md).

**Pursue the access list, as a peer-supplied hint.** The cheapest
intervention measured, with no consensus exposure, and worth more now
than when it was measured: 1.65× on one thread where it used to be
1.17×, because the acceleration is no longer swallowed by the commit.
Measure it against contract traffic before designing the wire format,
because that is where its coverage assumption is weakest.

**Keep parallel execution; the case for it is open again, and still
unproven.** It was worth 0.98× before the fix — a loss — and is worth
1.47× after it without the access list, and nothing measurable with it
(1,530 against 1,582 tx/s, medians of three, 15% spread on each). It
remains consensus-capped at two threads today. Do not raise the thread
count on the strength of these numbers; do re-measure once validation
is in the workload, because that is all parallelisable work and all of
it is currently missing.

**The whole block is measured now.** `--validate true` covers sender
recovery, header and body rules with proof of work, the transaction and
receipt tries, REMASC and block persistence. What is still outside
every figure here is the workload itself: native transfers between
accounts that already exist, so no run has executed a contract call or
inserted a key. `parallel_workload` builds ERC-20 traffic and
`--new-accounts` creates accounts the fixture does not hold; neither
has been run.

---

## How these numbers were produced, and how they were produced wrongly first

Five methodological faults, recorded because each produced a confident
number that was wrong, and three of them survived into a written
conclusion before being caught.

**Time-boxed runs.** Giving each configuration the same wall clock let
the faster one execute more blocks, sampling a different part of a decay
curve that falls steeply for the first eighty blocks. Fixed by running a
fixed block count.

**And one thing that went right, recorded for the same reason.** The
superlinear commit was visible in three noisy points long before it was
established, and the honest reading at the time was "suggestive and not
established" — the smallest point had a 52% spread. That refusal was
correct: three points with that much noise cannot establish a shape. But
declining to claim a result is not the same as dropping the question.
What closed it was building an instrument that measured the shape
directly — six points over a 32× range, in-memory store, no block around
it — and getting an unambiguous answer in one run. **When a measurement
is too noisy to support a claim, the move is a better measurement, not a
weaker claim.**

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
