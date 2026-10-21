# FlatDB in rustock

> For how this fits with the other two levers — the trie store and
> prefetched access lists — and which of them is worth what, see
> [`execution-performance.md`](execution-performance.md). This document
> is the detail behind one of them.

A port of the `flatdb-simplified` branch of rskj (`co.rsk.flatdb`, commit
`8b4f94a4c`) to Rust, written against `FlatDB-Rust-Port.md`, plus the
measurements comparing it with RocksDB as a trie store.

Status: **working prototype, measured, not wired into the node.** What
would have to change for that is at the end.

## Why it is allowed to be this simple

FlatDB is not a general key/value store. It stores data whose key *is* the
hash of its value, and every structural decision follows from that plus
three facts about the workload: no deletions, no range queries, no
mutation.

The precondition was checked rather than assumed. rustock's trie store
makes exactly two kinds of write — `TrieNode::save` stores a node under
`keccak(message)` and a long value under `keccak(value)` — and converting
the 1 GB benchmark fixture copied **9,299,999 of 9,299,999 nodes with none
skipped**, where the converter skips any entry whose key is not its
value's digest. The invariant holds in fact, not just in design.

What that buys: the index stores **no key bytes at all**. A probe resolves
a collision by fetching the candidate record and recomputing `H(value)`,
which also makes every successful short-record read a verification of the
bytes against their own address.

## Layout

| | |
|---|---|
| `checkpoint.rs` | the one metadata record, double-buffered and CRC'd |
| `heap.rs` | append-only mmap'd record store |
| `index.rs` | open-addressed table of 8-byte slots, mmap'd |
| `lock.rs` | `flock` on the directory |
| `lib.rs` | public API, write serialisation, flush ordering |

A flush is three ordered steps — heap, index, checkpoint — and the
checkpoint is the only thing that makes a batch real. A crash before step
three leaves heap bytes and index slots that the next startup ignores,
because the restored checkpoint puts the frontier below them. There is no
log, no undo and no recovery scan.

## Where the port departs from the Java original

Several are corrections rather than translation choices. Written up with
reproductions for the branch's maintainers in
`/home/paseo/flatdb/FlatDB-Defects-From-Rust-Port.md`.

**8-byte slots, not 5.** Java has no 40-bit integer, so a five-byte slot
meant assembling an offset byte by byte on every probe. Eight costs 60%
more index RAM and buys a single aligned load, exact sector packing — so
the alignment padding disappears entirely — and `AtomicU64`, which is what
makes the lock-free read path expressible.

**Readers take no lock.** A writer publishes a record with a release store
to its slot and a reader acquires it; the append-only invariant means the
bytes can never be rewritten afterwards. That pairing is precisely what
`experimentalConcurrentRW` lacked and was deleted for. Writers serialise
against each other on one mutex; readers never touch it.

**Zero-copy probing.** `Heap::with_record` hands a `&[u8]` straight out of
the mapping, so a probe step that rejects a candidate costs a hash and a
compare rather than two allocations. Java allocated on every probe step,
including the ones that miss. Only the record that matches is copied.

**The index file is named after its capacity.** This is a defect fix. The
original rehashes into a larger table and rewrites `index.dat` in place,
then writes the checkpoint; a crash between the two leaves the file
holding the **new** table while the newest valid checkpoint still names
the **old** capacity. Reading the first half of a doubled table with the
smaller mask silently loses every entry that rehashed into the second
half — about half of them, with no error, because an empty slot means
"absent".

It is silent rather than loud for a reason worth knowing: a key whose new
position is `i < N` satisfies `hash & (2N-1) == i ⟹ hash & (N-1) == i`, so
probing with the smaller mask still starts at the right slot and finds it.
The surviving half keeps working perfectly.

Here a grow writes `index.<capacity>.dat` and the previous file is removed
only after the checkpoint naming the new capacity is durable, which makes
the half-adopted state unrepresentable rather than merely detectable.
Covered by `a_grow_that_never_reached_a_checkpoint_is_not_adopted`.

**The segment size is recorded in the checkpoint.** Every offset is
`segment * segment_size + position`, so opening a database under a
different segment size resolves every record to the wrong bytes. Recording
it turns a silent empty read into a refusal to open, and lets the tests use
1 MiB segments and exercise the boundary paths in milliseconds instead of
reserving half a gigabyte each.

**Two guards against resurrected slots.** A stale slot points past the
committed frontier and reads as a miss — but the frontier only moves
forward, so once new records occupy those offsets the slot resolves again,
to a valid record that has nothing to do with the key whose probe chain it
sits on. Lookups stay correct because a probe compares the key it finds,
but the table can then hold more occupied slots than it has live entries,
and growth is driven by the entry count. If occupancy reaches capacity
while the count stays under half, the original's `while (true)` probe loop
never finds an empty slot and hangs. Its rehash also carries duplicates
over rather than collapsing them, which makes that state more reachable
over time.

So here `put` gives up after a full lap and grows rather than spinning,
and a grow drops duplicates instead of carrying them forward.

**`Heap::read` bounds-checks the record's end against the frontier**, not
only its start against the segment, so a length read out of bytes that are
not a record cannot walk past what has been committed.

**`fallocate` before mapping.** Writing into a hole on a full disk faults
at access time and arrives as `SIGBUS`, which no `Result` can carry.
Reserving the blocks turns that into an error at open.

## Measurements

4 threads, 4 sublists, 200 transactions a block, 100 blocks, against the
same 1 GB regtest fixture. Fresh copy and evicted page cache before each
run, two repetitions.

| store | wall | execution | write | CPU |
|---|---|---|---|---|
| RocksDB, 256 MB block cache | 236.5 / 121.6 s | 2298 / 1154 ms | 36 / 34 ms | 65% / 100% |
| FlatDB, flush every block | 170.2 / 190.8 s | 483 / 490 ms | 1163 / 1365 ms | 38% / 34% |
| FlatDB, flush every 25 blocks | **63.3 / 65.5 s** | 481 / 497 ms | 123 / 127 ms | 93% / 86% |

**All six runs end on the same state root**, `0x30d39bcb…37965`. These are
differences in speed and in nothing else.

### Reads are 3.5× faster, and far steadier

Execution — which is almost entirely trie reads — is 488 ms a block
against RocksDB's 1726 on average. The spread matters as much as the mean:
FlatDB measured 483, 490, 481, 497 across four runs, RocksDB 2298 against
1154 across two. A lookup here is a hash, one aligned load from a mapped
page, and a memcmp. There is no block cache to miss, no bloom filter, no
SST level to descend, and no 4 KiB block fetched to deliver a 115-byte
node.

That last point is the one that matters. The earlier RocksDB measurements
in `parallel-execution-benchmark.md` traced the bottleneck to read
amplification: trie keys are hashes, so a 4 KiB SST block holds ~35
unrelated nodes of which a lookup wants one, and 32 MB was being read per
block to serve ~6,000 useful nodes. FlatDB reads the node.

### The flush is the whole cost, and the comparison is not symmetric

`RocksDbTrieStore` does not implement `flush` at all — the trait default
is a no-op — so **the RocksDB column has no durability whatever**. Its
writes sit in the OS page cache; nothing is fsynced. FlatDB's flush is
real: msync the heap, msync the index, fsync the checkpoint.

So even the 25-block row is *more* durable than the RocksDB row, while
being 2.8× faster overall. Comparing like with like would make the gap
wider, not narrower.

Flushing every block costs 1163–1365 ms, which is the index writeback:
~6,900 slots a block, scattered by hash over 33.5 M slots, touch up to
6,900 distinct 4 KiB pages — about 27 MB of random writes to persist 55 KB
of actual slot changes. Dirty-page bookkeeping would not help; the I/O is
inherent to an 8-byte slot in a 4 KiB page.

Flushing every 25 blocks is sound rather than a cheat, and for the same
reason the store is allowed to have no log: **trie nodes are derivable**.
Losing them means re-executing those blocks, provided the node's executed
head is only advanced past what is durable. That is the discipline the
epoch store already follows.

### Space

| | RocksDB | FlatDB |
|---|---|---|
| fixture on disk | 1,020 MB | 1,281 MB |
| of which data | — | 704 MB heap |
| of which index | — | 257 MB (33.5 M slots at 27% load) |

The heap holds the same 9.3 M nodes in 704 MB against RocksDB's 1,020 MB,
because there is no LSM overhead and no obsolete version awaiting
compaction. The index is the new cost, and it is the one to watch: 8 bytes
a slot at load factor 0.5 is **16 bytes of index per live key**, and it is
resident. Mainnet's live state is about 10.8 M nodes, so ~170 MB — but the
index must be sized for the peak, not the present.

## What is not done

Honest list, because the gap between this and a production store is where
the risk lives.

Tracked as issues #310–#318.

- **Not wired into the node** (#311). `FlatTrieStore` implements
  `TrieStore`, `FlatEpoch` implements `EpochBackend`, and the benchmark can
  select either, but nothing in `main.rs` can.
- **No `get_many`** (#314). The trait's concurrent-read path falls back to
  serial gets. For a mapped store the right implementation is probably
  different from RocksDB's thread pool — page faults, not syscalls — and it
  is unmeasured.
- **No crash testing under real power loss.** The crash tests simulate by
  rewinding the checkpoint and dropping mappings. The porting notes
  suggest a torn-write harness that drops a configurable subset of
  sectors; that would be the next thing to build.
- **`loom` has not been run** on the lock-free read path. The
  release/acquire argument is written down and exercised by concurrent
  tests, which is not the same as model-checked.
- **Index growth is stop-the-world.** It rehashes every entry under the
  write lock, reading every record to recover its key. At 16 M entries
  that is minutes. Sizing from the live key count at open would make it
  rare; it would not make it fast.
- **Long records are not verified on read.** Short records recompute
  `H(value)` and so self-verify; records over 1,024 bytes read their
  stored key back and do not. That asymmetry is inherited from the
  original. Closing it would cost one hash per long read and is worth
  measuring — contract code is exactly the data it applies to.
- **The 1,024-byte threshold was never measured**, in Java or here.

## As an epoch backend

Done (#310). `epoch_backend.rs` defines what the epoch store needs from one
epoch's storage — get, put, put_many, delete, flush, is_empty, for_each,
size_bytes — with `RocksFactory`/`RocksEpoch` and `FlatFactory`/`FlatEpoch`
behind it. `epoch_store.rs` no longer mentions RocksDB.

Three things the seam had to get right, and each is a test:

**Iteration is a heap scan, not an index walk.** The sweep reads the oldest
epoch end to end, and for an append-only store the heap *is* the sequential
order: one pass, each page read once, no probing. It can yield records the
index no longer reaches — a rolled-back batch leaves its bytes behind —
which is harmless to a consumer that filters by key, and the sweep does.

**The rotation size comes from the backend.** FlatDB reserves heap segments
whole and sizes its index for a key count it has not reached, so its
directory is over a gigabyte before a single node is stored. A policy
reading the filesystem would rotate an empty epoch. `size_bytes` reports
the heap frontier plus the live index; `flatdb_reports_what_it_holds_rather_than_what_it_reserved`
asserts the directory overstates it by more than 4×.

**`delete` fails loudly.** An append-only heap has nowhere to put a
tombstone, and the probe chains assume no slot is ever vacated. Nothing in
the production path deletes — reclamation is discarding a whole epoch — but
a backend that appeared to delete would let the epoch store believe it had
reclaimed something.

## Verifying what comes back

A short record stores no key: the probe recomputes `H(value)` to compare,
so every successful read of one checks the bytes against their own
address. A record over 1,024 bytes carries its key and was compared
against what it *claims* rather than against what it *is* — and that is
exactly the data contract code lives in. Closed (#316), and the shape of
the fix came out of the measurement rather than the other way round.

Keccak-256 on this machine, `sha3` 0.10:

| bytes | ns/hash |
|---|---|
| 50 | 483 |
| 1,024 | 3,623 |
| 2,048 | 7,439 |
| 4,096 | 14,101 |
| 65,536 | 217,080 |

About 283 MB/s, with ~480 ns of fixed cost.

That rules out the obvious implementation. Verifying during the
comparison would mean hashing a 24 KB contract body in order to
**reject** it on a collision — some 80 µs to discover the record is not
the one being looked for. The stored key exists precisely to make that
rejection cost 32 bytes of memcmp.

So verification runs **on the hit, never on a rejection**: once the
stored key has matched, `H(data)` is recomputed once and compared. One
hash per successful long read, zero on a miss. A record whose bytes no
longer hash to their address is reported as absent rather than returned,
because a missing trie node can be fetched from a peer and a wrong one
cannot.

`verify_long_records` defaults to on. `without_verification_a_corrupt_long_record_is_returned`
exists to document what the other setting means.

### And the threshold is no longer a correctness boundary

That was the second half of #316, and the measurement reframes it. With
verification on the hit, 1,024 bytes stops deciding which values are
content-checked — all of them are — and becomes a pure space-versus-time
knob:

- below it, 32 bytes saved per record, and every probe step that touches
  the record pays a hash (483 ns at 50 bytes);
- above it, 32 bytes spent, rejections cost a memcmp, and one hash is
  paid on the hit.

At the threshold itself a hash is 3.6 µs and the key is 3% of the
record, which makes storing it clearly right there. At 128 bytes a hash
is 489 ns and the key is 25% overhead, which makes it clearly wrong. The
crossover is somewhere in between and depends on the read-to-space
tradeoff of the deployment, so 1,024 stays — now as a defensible default
rather than an unexamined one.

**Not measured end to end**: the 1 GB fixture is accounts, so almost
every record is short and the benchmark cannot see this cost. Measuring
it needs state with contract code in it.

## Parallel execution: the bottleneck moved, it did not go away

The parallel execution benchmark found the workload I/O bound — four
threads reaching 86% CPU on four cores, buying 1.33×. The obvious
follow-up was whether FlatDB, by making reads cheaper, would let the
threads finally show something. Run against both engines with the same
binary, 100 blocks, 200 tx/block, 4 sublists:

| engine | threads | wall | CPU | total ms/block |
|---|---|---|---|---|
| RocksDB | 1 | 220.9 s | 46% | 2,179 |
| RocksDB | 4 | 159.9 s | 80% | 1,566 |
| FlatDB | 1 | **64.1 s** | 67% | **611** |
| FlatDB | 4 | 58.7 s | **99%** | 558 |

All eight runs end on the same state root.

**FlatDB is 3.6× faster on a single thread** — which is the number that
matters, because block execution issues dependent reads and cannot batch
them. **And it scales 1.09×**, against RocksDB's 1.39×.

99% CPU on four cores is one core busy. The bottleneck stopped being the
disk and became a lock.

### Write volume governs the scaling, but not for the reason it looked like

60 blocks on FlatDB, varying only the transactions:

| tx/block | 1 thread | 4 threads | scaling | CPU at 4 |
|---|---|---|---|---|
| 200 | 43.4 s | 38.0 s | 1.14× | 93% |
| 40 | 18.2 s | 13.9 s | 1.31× | 41% |

Cutting writes five-fold raises the scaling, and the parallel fraction
by Amdahl goes from 16% to 32%. That was read as a lock inside the
parallel region — `FlatDb`'s writer mutex, held across the probe, the
key comparison, the heap append and the slot store some 6,900 times a
block.

**It was removed, and nothing got faster.** 1/2/3/4 threads went from
64.1 / 67.2 / 62.8 / 58.7 s to 64.4 / 67.2 / 63.4 / 57.6 s. The mutex
was real and cost about 7 ms against a 611 ms block: visible in a
profile, invisible in a wall clock.

Every figure in this section is a single run, and single runs on this
host have repeatedly failed to replicate — the spread reaches 5× for
identical configurations. Treat them as indicative and see
[`flatdb-writer-lock.md`](flatdb-writer-lock.md) §7.

What the gap actually reflects is that more transactions mean more
**sequential** work per block — more nodes to hash recomputing the root,
more to walk in the ordered commit, more to write — and no thread count
divides that.

[`flatdb-writer-lock.md`](flatdb-writer-lock.md) records the disproof
and the concurrency of the write path as it stands, for someone picking
it up cold. It also covers what *is* serialising now: batching the
writes made one thread 1.16–1.80× faster and made four threads slower
than one, because publication is ordered and a reservation is now a
whole sublist rather than a single record.

### Why the obvious fix is unsound

The first proposal was to reserve the frontier with a `fetch_add` and
write outside the lock, since no other thread can be handed the same
range. **That is wrong**, and the reason is worth keeping.

`size` has two jobs. It is the allocation cursor, and it is the
publication barrier:

```rust
// append: bytes first, publication second
std::ptr::copy_nonoverlapping(data.as_ptr(), p, data.len());
self.size.store(size + total, Ordering::Release);

// with_record: acquire, then reject anything beyond
let size = self.size.load(Ordering::Acquire);
if offset >= size { return None; }
```

That pair is the *only* thing making the lock-free read path sound: a
reader either sees the old `size` and rejects the offset, or sees the
new one, and the edge guarantees the bytes are there. Reserving ahead of
writing moves the barrier in front of the bytes, and a reader following
a resurrected stale slot into the reserved range would read memory while
it is being written. That is a data race and therefore undefined
behaviour — the content-addressing argument does not rescue it, because
that argument is about which *values* would come back and a race is
undefined regardless.

Two alternatives that do hold:

**Narrow the lock.** The probe and the key comparison mutate nothing,
and the comparison hashes every candidate record — 483 ns each, about
1.5 times per put. Both can move outside, leaving the append and the
slot publish inside. The pass under the lock has to resume from the
probe's candidate rather than trust it, which is sound because an
occupied slot never empties except under `grow`'s write lock.

**Ordered publication.** Split the two roles: an `alloc` cursor reserved
by CAS, and `size` advanced only in order. A writer reserves, writes its
bytes with no lock held, then waits for `size` to reach its own start
before storing its end. The invariant survives exactly, and the
expensive part — the memcpy and its page faults — genuinely overlaps.

But 41% CPU at four threads with *few* writes says the lock is not the
only limit. A third of a block's work is parallel at best; the
maintenance run, the ordered commit, the root hashing and the lazy
reload are all sequential. Removing the mutex should recover what writes
are stealing and no more.

## Memory

The worry was that 8 bytes a slot at load factor 0.5 is 16 bytes of
**resident** index per key, sized for the peak rather than the present
(#318). Measured, the premise is wrong, and the direction is the
opposite of what it assumed.

Peak resident memory over 60 blocks at 4 threads, sampled from
`/proc/<pid>/status`:

| | RssAnon | RssFile | total |
|---|---|---|---|
| RocksDB, 256 MB block cache | **334 MB** | 11 MB | 345 MB |
| FlatDB | **10 MB** | 962 MB | 972 MB |

FlatDB's total looks three times worse and its *demand* is thirty times
smaller. Almost all of it is file-backed: the heap and index mappings,
which are page cache the kernel owns and can reclaim at will. The
irreducible anonymous footprint is 10 MB.

RocksDB's 334 MB is anonymous. The block cache is the application's
memory; the kernel cannot take it back under pressure, it can only swap
it.

That is an operational difference, not a bookkeeping one. On a machine
where something else needs memory, FlatDB degrades — pages are evicted,
the next probe faults, reads get slower — and RocksDB does not degrade,
it competes. On this 7 GB host, where a 2 GB cache has previously got a
long-running job OOM-killed, graceful degradation is the behaviour worth
having.

It also explains the earlier cache results that looked paradoxical: a
1,536 MB RocksDB block cache made the benchmark *slower* than a 256 MB
one, and a 500k-entry node LRU cost 38%. Anonymous caches take memory
away from the page cache that was doing the real work. A mapped store
has only one pool and cannot make that mistake.

### Sizing designs the rehash out (#313)

A grow is stop-the-world: it holds the write lock and reads every record
out of the heap to recover its key, because the index stores none. At
sixteen million entries that is minutes with no read proceeding.

Making it *fast* was the obvious fix and is the wrong one. An incremental
rehash would keep both tables and migrate a bounded number of slots per
operation, which removes the stall by complicating every read — the path
that has to stay fast. And the memory result above says the alternative
is nearly free: `expected_keys` buys reclaimable address space, not
committed memory, so over-sizing and under-sizing are not symmetric
errors at all.

So the size is **computed, not configured**. An epoch is not open-ended:
it rotates at `rotate_bytes`, so the records it will ever hold are
bounded.

```
expected_keys = rotate_bytes / AVERAGE_RECORD_BYTES        (80 bytes)
```

At 1 GB that is 13.4 M keys, 2^25 slots, 268 MB of mapping, and a full
epoch sits at 40% load — under the 0.5 factor, so it never grows. A test
asserts exactly that, and another pins `AVERAGE_RECORD_BYTES` against
the real record encoding so a layout change trips it instead of silently
shrinking every index. The 80 comes from the fixture: 704 MB of heap
over 9,299,999 records is 75.7 bytes each, rounded up because
over-estimating the divisor under-sizes the index.

`--trie-expected-keys 0` (the default) derives it. A non-zero value
overrides.

One consequence worth stating, because it was nearly a silent
behavioural change. The rotation policy asks how much an epoch has
*accumulated*, and FlatDB allocates its whole index up front — so
reporting heap plus index would add a constant of 256 MB to every
reading from the first, and an epoch configured to rotate at 1 GB would
rotate after 768 MB of records, a quarter early, for the life of the
store. `size_bytes` is the heap frontier alone; `disk_bytes` answers the
other question.

The grow stays as the safety net it now is, and says so at both ends
with how long it stopped the world — reaching it means the sizing was
wrong, which is something an operator needs told rather than left to
infer from a latency graph.

**Revisiting the 5-byte slot is not warranted.** It would trade the
single aligned load and, more importantly, `AtomicU64` and the lock-free
read path for 10 bytes a key instead of 16 — a saving in a quantity the
measurement shows is not the binding constraint.

## What real data did to the headline

The benchmark sections below were measured on the synthetic fixture.
Re-run against the **real mainnet epoch store** — same data on both
sides, same state root, same disk, page cache dropped by name with
`posix_fadvise` before each run — the conclusion changes.

65,535 nodes, walking the trie level by level as the sweep's marking
phase does. Medians of three:

| | serial | batched (16 threads) |
|---|---|---|
| RocksDB | 74,450 ms | **8,166 ms** |
| FlatDB | **42,425 ms** | 8,372 ms |

**Batched, they are a dead heat** — 2.5% apart, which is inside the
run-to-run spread. The 3.5× that `parallel_bench` measured on the
fixture does not survive contact with real state.

The fixture flattered FlatDB and this document said so at the time
without knowing how much: it was built bottom-up by `bulk.rs`, so the
top of the trie sits contiguously at the end of the heap and a walk is
nearly a sequential read. A real store is written in traversal order
over months and spread across three epochs, and the locality is gone.

**Serially, FlatDB is still 1.76× faster**, and that is not a
consolation prize: block execution issues *dependent* reads and cannot
batch them, so the serial column is the one execution lives in. But
1.76× is the number to carry, not 3.5×.

### How not to measure this, part two

The first run of this comparison produced 376 ms and 66,870 ms for the
same configuration — a 178× spread. The eviction was at fault: filling
the page cache by reading 6 GB of unrelated data does not reliably evict
anything, because Linux favours dropping pages it has seen once
sequentially over pages recently touched, which is exactly backwards for
a benchmark. `posix_fadvise(POSIX_FADV_DONTNEED)` on the store's own
files names the pages to drop and takes a second.

## Batched reads

`get_many` exists because a trie traversal issues *dependent* reads — each
address known only once the previous one returns — so the device sees one
request at a time. Keys at the same level are independent, and the sweep's
marking phase is the one place in the store that holds a whole level at
once.

Walking the top 13 levels of the 1 GB fixture (8,191 nodes), each mode in
its own process with the page cache evicted first:

| | serial | batched | |
|---|---|---|---|
| RocksDB | 18,150 ms | **2,283 ms** | 7.9× |
| FlatDB | 54 ms | 42 ms | 1.3× |

Two separate findings, and the first is not about FlatDB at all.

**`EpochTrieStore::get_many` was throwing the batch away.** It mapped
`get` over the keys, discarding the parallel implementation
`RocksDbTrieStore` already had — so the expensive half of a collection
ran at queue depth 1 against a device that wanted sixteen. That is a
production path on the current backend, and 7.9× is what it was leaving
behind. Fixed by batching per epoch, newest first, carrying forward only
what is still unresolved.

**FlatDB was prefetching, and that was wrong.** The first implementation
used `MADV_WILLNEED` over the index slots and the records they point at,
on the reasoning that a mapped read is a page fault rather than a
syscall, so the kernel could overlap the batch with no threads at all.
Measured on the real store over 65,535 nodes:

| | ms |
|---|---|
| fan out over 16 threads | **8,342** |
| prefetch only | 37,026 |
| both | 26,309 |

Prefetching alone is 4.5× slower than fanning out, and adding it *on top*
of the threads is 3.2× slower than the threads alone — the advice
competes with the threads' own faults and spends bandwidth fetching
pages in an order nothing is waiting for. The reasoning was clean and the
kernel does not behave that way. The prefetch path is gone; FlatDB fans
out exactly as RocksDB does.

The 336× gap between the two serial columns is **not** a fair claim about
the engines. The fixture was built bottom-up by `bulk.rs`, so the top of
the trie sits contiguously at the end of the heap and the first 13 levels
of a walk are very nearly a sequential read. A node writing in traversal
order per block would not have that layout. It is an accidental
demonstration of the path-locality idea in
`FlatDB-StateOfTheArt.md` rather than a measurement of normal operation.

### How not to measure this

Both of the first two attempts were wrong in the same way, from opposite
ends. Serving each level serially *and* batched inside one process made
whichever pass ran first pay for the cold cache: serial-first reported
419× for batching, batch-first reported 0.05×. Neither number was about
batching. The tool now takes `--mode serial|batch` and refuses to run
without it.

## Crash consistency

Tested against torn writes, not only simulated (#315).
`tests/torn_writes.rs` snapshots a database at a committed point, writes
more, kills the process before the checkpoint, and then rebuilds the
directory with an arbitrary subset of 512-byte sectors reverted to their
earlier contents. For any such subset: the store opens, every key durable
at the old checkpoint reads back, nothing uncommitted appears, and the
store is still writable.

The reason that can hold is the argument for sector granularity: **a slot
write never modifies another slot.** Eight-byte slots divide a 512-byte
sector exactly, so reverting a sector loses precisely the slots written
into it since the snapshot and never damages one already there — and
those lost slots point at or beyond the old frontier, so they read as a
miss anyway.

One control test asserts the harness destroys more than twenty sectors,
because a tear that silently reverted nothing would make every other test
in the file pass for the wrong reason.

The harness's first run reported a lost key, and the defect was in the
harness. It staged the crash by completing the flush and *then* reverting
the checkpoint — but a completed flush also removes the index file the
old capacity names, so that pairing is one no crash can reach. Staging it
faithfully means never writing the second checkpoint at all.

### Why `loom` is not here

The porting notes suggest model-checking the lock-free read path, and it
was tried and rejected rather than skipped.

`loom` replaces `std` atomics with instrumented ones that own their
storage. The index's slots live inside a memory mapping, so they cannot
be loom atomics without abstracting the slot array over its backing — and
even then, loom would verify the slot protocol while the part that is
actually subtle stays outside its view: whether a reader that acquires a
slot sees the heap bytes the writer wrote through a raw pointer. That
rests on the release/acquire pair plus append-only immutability, which is
an argument, not something loom checks.

A loom harness over a `Vec`-backed slot array would therefore model the
easy half and produce confidence about the hard half that it had not
earned. The concurrent tests that are here —
`readers_and_a_writer_can_run_at_once`,
`a_grow_under_concurrent_readers_loses_nothing` — exercise the real
mapping under real threads, which is weaker as proof and honest about it.

## Migrating a node

Done (#317). `flat_migrate` converts an epoch store in place, with the
node stopped:

```sh
cargo run --release --example flatdb_migrate -- \
    /var/lib/rustock/trie-epochs --blocks /var/lib/rustock/blocks \
    --expected-keys 24m --keep-original
```

Each epoch is converted into a sibling directory and marked done, so an
interrupted run resumes where it stopped rather than starting over, and
never half-writes the live store. A directory present without its marker
is wreckage and is redone rather than trusted.

**It verifies by the root, not by a count.** A node count can match the
source while the trie is unwalkable. The migration resolves the executed
head's state root through the converted epochs and descends the whole
trie; only then are the originals moved aside. A node referenced but
absent stops it with the originals untouched, and the message names
issue #293 — an epoch store collected before `b0eff4b` may be missing
long values, which is a precondition the migration cannot repair.

`--segment-mb` is not a tuning knob. The segment size is recorded in each
store's checkpoint and the node refuses to open one written with a
different value, so a migration that guessed would convert cleanly and
then fail to start. It defaults to the node's own 512 MB.

Without `--blocks` nothing is verified and nothing is swapped, which is a
legitimate way to do the slow half in advance.

## The mainnet trial

October 2026, node stopped, working on a copy. The live store at
`/var/lib/rustock/trie-epochs` was not touched and a backup was taken to
a different physical disk first.

```
3 epochs, 25,371,307 nodes copied, 0 skipped, 120 s
10,519,952 nodes resolved walking from the executed state root
```

**Nothing was skipped**, which settles on production data what the
converter only checks: the trie store is content-addressed throughout.

**No epoch grew.** All three came out at 2^25 slots, the figure derived
from the rotation threshold, which is the first real evidence for #313's
approach.

### What it found

| | |
|---|---|
| records | 11.2 M / 8.4 M / 5.7 M across the three epochs |
| 32–63 bytes | 30.4% |
| 64–127 bytes | 69.5% |
| over 1,024 bytes | 0.05% — 5,589 in the oldest epoch |
| longest | 24,573 bytes |
| mean on disk | 73.8, 73.2, 73.4 bytes |

**The fixture has no long records at all**, so until this ran, the
stored-key path and the verification on top of it (#316) had never seen
anything real. A trie walk does not reach them either — a long value
hangs off a node's value hash rather than being one of its children — so
they were read back explicitly: **5,589, 450 and 475, all verified, none
failed.** Contract code, up to 24 KB, through the path that recomputes
`H(value)` and compares.

**And it corrected `AVERAGE_RECORD_BYTES`.** The 80 came from the
synthetic fixture. The real mean is 73.5, so the real key count sat 9%
above the estimate — the same capacity either way, but less margin than
intended. It is now 72, rounded *down* because the figure is a divisor
and under-estimating it over-sizes the index, which is the cheap error.
A full 1 GB epoch is then 14.7 M records against a 2^25 table: 44% load.

## Running it

```sh
# convert an existing RocksDB trie store
cargo run --release --example flatdb_convert -- <rocksdb-dir> <flatdb-dir> \
    --expected-keys 10m

# benchmark against it
cargo run --release --example parallel_bench -- <flatdb-dir> \
    --flatdb true --threads 4 --sublists 4 --txs 200 --blocks 100 \
    --flush-every 25
```

`--expected-keys` sizes the index at open; it is a hint, and exceeding it
costs a rehash rather than a failure.
