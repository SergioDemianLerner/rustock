# FlatDB in rustock

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
