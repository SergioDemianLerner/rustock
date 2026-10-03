# The freezer

Flat-file storage for settled chain data: canonical headers, and the uncle
headers they reference. Below a threshold the node stops expecting a block to
change, its header leaves RocksDB's B-tree and lands in an append-only file
addressed by arithmetic.

[`freezer-estimates.md`](./freezer-estimates.md) is the analysis that argued for
building it, written before it existed. This document describes what was built.

## 1. Why

### The request this is about

A node catching up does not ask for headers one at a time. It asks for a
**skeleton** — every 192nd block, up to 20 of them in an answer — and then
fills the gaps between those points. Both numbers come from rskj
(`chunkSize = 192`, `maxSkeletonChunks = 20`) and are matched here as
`SKELETON_STEP` and `MAX_SKELETON_ENTRIES`; `MAX_HEADERS_SERVE` caps a single
answer at the same 192.

So the unit of work on the serving side is: **"give me the 192 headers below
this hash"**, and a catching-up peer issues many such requests at once, each
answered independently. This is the ordinary forward sync — the same shape a
snapshot sync's header walk uses, which is why the walk is 92% of a snapshot
sync (see [`cross-client-snap-sync.md`](./cross-client-snap-sync.md)).

Serving nine million headers therefore means answering roughly **48,000 of
these requests**, each one a 192-header run.

### Why that is expensive

A header lives in RocksDB under its **hash**. Hashes are uniformly distributed,
so 192 consecutive blocks live in 192 unrelated places. One request is
192 independent point lookups, each reading a whole SST block — and often an
index block above it — to return 1.1 KB.

Worse, they were originally issued *serially*: each lookup waited for the one
before it, so a single request cost 192 round trips to the device, one at a
time. `headers_by_hash` now issues them through a thread pool
(`--read-threads`, default 16) so they overlap.

Measured while serving headers to one syncing client:

| | dependent reads | concurrent reads |
|---|---|---|
| throughput | 934 KB/s | 5,600–7,100 KB/s |
| server disk read | 6.5 MB/s | 37.4 MB/s |
| **amplification** | **7.0×** | **6.5×** |

**~7.1 KB of disk read to deliver 1.1 KB of header.**

The two columns are the design argument. Issuing those 192 lookups through a
16-deep queue instead of one after another raised throughput sevenfold and
left amplification where it was. That is the general shape of the thing:

> **Concurrency hides latency. It does not reduce the bytes read.**
> Only the layout does.

A node can keep buying throughput with queue depth until the device saturates,
and still be reading seven times what it delivers. The freezer attacks the
other half — not how fast the reads are issued, but how many bytes they have to
touch.

Headers are the right place to attack it:

- they are what a syncing peer asks for most;
- they are immutable once settled, so a format that cannot be updated in place
  costs nothing;
- they are read in **runs** of exactly the shape above, and a run of
  consecutive blocks can be made a run of adjacent bytes.

The last point is the one that matters, and it is what makes the fix
disproportionate to the effort. The request is already "192 consecutive
blocks"; it is only the storage that scatters them. Key the same data by
*number* in a flat file and that request becomes **one index read of 1,920
bytes followed by one data read of about 203 KB** — two reads where there were
192, and no amplification beyond the page granularity of the device.

Everything in §2 follows from wanting that, and nothing in §2 would be worth
doing for a workload that read headers one at a time in random order.

## 2. How it works

Two parallel stores, same format, same rules:

```
headers.cidx    headers-0000.cdat  headers-0001.cdat  …
uncles.cidx     uncles-0000.cdat   uncles-0001.cdat   …
```

**The index is fixed-width.** Ten bytes per block — `{file: u16, start: u32,
end: u32}` — so entry `N` sits at byte `N × 10`. Finding a block is arithmetic,
not a search, and finding a *run* is one read of `count × 10` bytes.

**Data files are capped** at `MAX_DATA_FILE_BYTES` (2 GiB), so no single file
grows without bound. A block is never split across files: the writer rolls
before writing, never across.

**Reads are runs.** `Freezer::run(from, count)` does one index read, then for
each data file the run touches — normally exactly one — a single read spanning
the lowest to the highest offset, slicing each header out of the buffer. This
holds whichever direction the run was written in: a descending walk puts block
`N+1` before block `N` in the file, but they are still adjacent.

**Writes are buffered** at `WRITE_BUFFER_BYTES` (8 MiB) and are not readable
until flushed — a buffered block reports absent, because saying otherwise
would promise a read that would fail.

### Crash safety

Data is written and `fsync`ed **before** the index entries that name it. The
ordering is the whole story:

- data with no index entry is unreferenced, and `open` truncates it away;
- an index entry with no data behind it would be an unreadable header that
  nothing detects until a peer asks.

On open, the index is the authority: anything in the data file past the
furthest point any entry accounts for is a partial write from a process that
died, and is discarded. A torn index write leaves a partial entry, which is
also dropped. Losing a buffered batch is never a correctness problem — those
blocks were never published, so the freezer simply does not have them, and
whatever fills it writes them again.

### Staging

A snapshot sync walks down from a checkpoint a *peer* offered. Each header is
verified and linked, but the chain is only known to be this network's once the
walk reaches ground the node already accepted. Those headers go to a **staging**
freezer named for the checkpoint they rest on — `staging-<number>-<hash>-*` —
so a walk that turns out to be wrong costs only those files. `promote()`
renames them into place; `discard_staging()` removes them, and also cleans up
after a process that died mid-walk.

The alternative — writing into the real files behind a marker — would mean
discarding the *whole* freezer on failure, including millions of headers frozen
legitimately before the walk began.

## 3. Why it stores uncles

Uncle headers live in exactly one place: the block body.

In RSK, total difficulty advances by the trunk block's difficulty **plus every
uncle's** (rskj `Block.getCumulativeDifficulty`). A header commits to its uncle
list through `ommers_hash` and records `uncle_count`, but carries neither the
uncles nor their difficulties. And an uncle is by definition *not* canonical,
so it has no height slot of its own and can never appear in a store keyed by
block number alongside the canonical header.

So before the uncle store existed, a node that froze its headers and pruned its
bodies could no longer:

- recompute its own chain's total difficulty;
- prove the uncles its chain absorbed ever existed;
- serve those uncles to a peer.

Permanently — the inputs were gone. That is what made the total-difficulty
defect expensive to repair rather than merely wrong: the fix was four
lines, and rebuilding mainnet's stored totals took 7 h 31 m because every block
body had to be read back for its uncle difficulties.

The uncle list is stored as the RLP list that `ommers_hash` commits to, so a
reader can recompute the commitment and a peer can be served the bytes
directly. A block with no uncles stores the **empty list, `0xc0`**, not nothing:
a zero-length run would be indistinguishable from an index entry that was never
written, and "this block has no uncles" and "this block is not frozen" must not
be the same answer to a caller computing total difficulty. `uncles()` returns
`Some(vec![])` against `None`.

### The ordering this creates

The pruner must not outrun the freezer. The freezer works below
`head − FREEZE_DEPTH` (20,000) while the pruner works below
`head − keep_depth` (8,000 floor) — the pruner's range is the *shallower*, so
left alone it reaches every block first and deletes the body before the uncles
have been copied out of it. `plan_prune` therefore clamps to
`freezer.uncles_end_number()` and plans nothing while that is zero. See
[`block-pruning.md`](./block-pruning.md).

A node run with `--no-freezer` and `--prune-blocks` is unguarded, correctly:
there is no second copy to wait for. It also gives up the ability to recompute
its chain's work below the floor.

## 4. Choosing the freeze depth

`FREEZE_DEPTH = 20_000`.

One week at Rootstock's measured 30.3-second block time (19,964 blocks over the
100,000 blocks to 2026-09-28), rounded.

There is no finality gadget here, so this cannot key off finality the way geth
and reth do. It is anchored instead against what the rest of the node already
treats as settled:

| constant | value | meaning |
|---|---|---|
| `MAX_REORG_DEPTH` | 1,000 | deepest reorg the canonical index will follow |
| `MIN_KEEP_DEPTH` | 8,000 | the floor block pruning refuses to cross |

20,000 is 20× the first and 2.5× the second, so a frozen block is one the rest
of the node has already stopped expecting to change. A test asserts the value
stays within 1,000 blocks of a week, so that a change to block time is caught
rather than silently drifting.

## 5. What happens on a reorg that reaches the freezer

`truncate_head(number)` drops the index above `number` — in **both** stores, or
the uncle store would answer for blocks the freezer has disowned. The data
files keep the orphaned bytes, which `open` later truncates; the index is what
makes a block findable, so shortening it is what removes the block.

In practice this should never run. A reorg deep enough to reach the freezer is
20× deeper than the deepest the canonical index will follow, so by the time a
reorg could invalidate a frozen header the node has already failed to follow it
by other means. The method exists because "should never happen" is not a
storage guarantee, and a freezer that cannot be rolled back would be a
correctness problem rather than a lost optimisation.

Everything below the threshold is canonical and settled by construction: only
canonical headers are frozen, and only once they are 20,000 deep.

## 6. Measured on mainnet

At head #9,288,884, the production node:

| | |
|---|---|
| frozen headers | 9,268,884 — exactly `head − FREEZE_DEPTH` |
| coverage | 99.78% of the chain |
| header data | 10.1 GB, **1,085 bytes/header average** |
| uncle data | 10.5 GB (~1.04 uncles per block) |
| index | 92.7 MB (10 bytes × 9.27 M) |
| **total** | **~20.6 GB** |

Backfilling the uncles for the existing 9.27 M frozen headers took **7 h 31 m**
at ~343 blocks/s, because it reads every block body. A fresh node pays this
incrementally as it freezes.

The uncle store roughly doubles the freezer's size. That is the price of being
able to recompute the chain's work from data the node holds, rather than
trusting a figure it can no longer check.

## 7. How other implementations do this

**geth — the freezer / "ancients"** (`core/rawdb/ancient_scheme.go`). Five
tables, each its own file series: `headers`, `hashes` (canonical), `bodies`,
`receipts`, `bals`. Every one snappy-compressed except `hashes`, stored raw.
Separate per kind rather than interleaved — geth reached that conclusion
independently. Freezing keys off finality.

**reth — static files**. Segments for `Headers` (carrying canonical hashes and
terminal difficulties), `Transactions`, `Receipts`, `TransactionSenders`,
`AccountChangeSets`. Again separate per kind.
`DEFAULT_BLOCKS_PER_STATIC_FILE = 500_000` in the source, though published
summaries say 8,192.

**Where this differs from both.** Neither has an uncle problem: in Ethereum,
total difficulty advances by the trunk header's difficulty alone, so uncles
affect rewards and not work, and a header chain is sufficient to compute and
compare cumulative work. RSK's `getCumulativeDifficulty` diverged, which is why
the freezer here needs a second store that geth and reth have no reason to
keep. See rsksmart/RSKIPs#698.

Neither keys the freeze depth off a block count the way this does, because both
have finality to key off instead.

## 8. Operating it

| Flag | Default | Meaning |
|---|---|---|
| `--no-freezer` | off | Disable it; old headers stay in the block database |
| `--prune-frozen-headers` | off | Delete headers from RocksDB once the freezer holds them |

The freezer fills in the background from the block database, batching and
yielding so it competes with serving rather than starving it. Nothing depends
on it having finished: until a run is frozen, the serving path answers it the
old way.

A freezer that will not open is a **performance** loss, not a correctness one —
every header it would have held is still in RocksDB — so the node says so
loudly and carries on rather than refusing to start.

That failure mode has a sharp edge worth knowing: if the files exist but cannot
be opened — wrong ownership after an offline repair run as `root`, say — the
node logs a warning and runs *without* the freezer, silently abandoning tens of
gigabytes it would otherwise use. Check the startup line reports the size you
expect.

## 9. Tests

`crates/storage/src/freezer.rs` — 27 tests. The ones that pin the design
rather than the mechanics:

| Test | Pins |
|---|---|
| `no_uncles_is_not_the_same_as_not_frozen` | `Some(vec![])` against `None` (§3) |
| `the_empty_list_occupies_a_byte_at_offset_zero` | the `0xc0` sentinel collision (§3) |
| `truncating_the_head_drops_uncles_too` | both stores roll back together (§5) |
| `the_stores_are_addressed_separately` | freezing a header claims nothing about its uncles |
| `uncles_survive_a_reopen` | the crash-safety path covers both stores |
