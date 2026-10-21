# What serialises FlatDB's writes

A guide to the concurrency of FlatDB's write path, for someone who did
not write the code and wants to make it faster.

It is also the record of a hypothesis that was wrong. An earlier version
of this document argued that a writer mutex was holding back parallel
execution, and said how to remove it. The mutex was removed. **Nothing
got faster.** That result is kept here because the reasoning behind the
hypothesis looked sound and was not, and the next person deserves to
know which arguments already failed.

No prior knowledge of the store is assumed.

Related: [#320](https://github.com/SergioDemianLerner/rustock/issues/320)
for the live problem,
[`execution-performance.md`](execution-performance.md) for where this
sits among the other bottlenecks, and `crates/flatdb/` for the code.

---

## 1. Where things stand

Executing 100 blocks of 200 native transfers on a four-core machine,
before and after the mutex was removed:

| threads | with the mutex | without it |
|---|---|---|
| 1 | 64.1 s | 64.4 s |
| 2 | 67.2 s | 67.2 s |
| 3 | 62.8 s | 63.4 s |
| 4 | 58.7 s (99% CPU) | 57.6 s (101% CPU) |

Four threads on four cores still reach about one core's worth of CPU.
Scaling went from 1.09× to 1.12×, which is inside the run-to-run spread.

**The mutex was not the bottleneck.** It existed, it did serialise
writers, and removing it bought nothing measurable.

### What the same change did buy

The work that replaced the mutex also batched the writes — a block's
whole dirty subtree is collected and written in one call instead of
roughly 6,900 — and *that* paid, on a single thread:

| 60 blocks, one thread | before | after | |
|---|---|---|---|
| 200 tx/block | 43.4 s | **37.3 s** | 1.16× |
| 40 tx/block | 18.2 s | **10.1 s** | **1.80×** |

Which matters, because RSKIP-144 is unscheduled on mainnet and the node
executes on one thread.

### And what it broke

| 60 blocks, 200 tx | before | after |
|---|---|---|
| 1 thread | 43.4 s | 37.3 s |
| 4 threads | 38.0 s | **42.2 s** |

With full blocks, four threads are now **slower than one**. That
regression is the subject of section 4 and is the thing actually worth
fixing.

---

## 2. How the write path works now

Three pieces, and only the middle one still orders anything.

### The heap has two frontiers

```rust
/// One past the last byte **reserved**. Writers move this.
alloc: AtomicU64,
/// One past the last byte **written and published**. Every reader
/// checks against this.
size: AtomicU64,
```

They are separate for a reason that constrains every change here.
`size` is the publication barrier: the writer stores it with `Release`
*after* copying the bytes, and the reader loads it with `Acquire`
*before* reading them.

```rust
// with_record, the read path
let size = self.size.load(Ordering::Acquire);
if offset >= size {
    return None;              // beyond the frontier: not a record
}
```

A reader therefore either sees the old `size` and rejects the offset, or
sees the new one — and the release/acquire edge guarantees the bytes are
already there. **This is the only thing making lock-free reads sound.**
Any scheme that lets `size` run ahead of written bytes is a data race,
whatever the values would have been.

### Space is claimed with a CAS

```rust
fn reserve_run(&self, sizes: &[usize]) -> io::Result<(u64, u64, usize, u64)> {
    loop {
        let from = self.alloc.load(Ordering::Acquire);
        let mut start = from;
        let mut room = segment_size - (start % segment_size);
        if sizes[0] as u64 > room {
            start = (start / segment_size + 1) * segment_size;   // skip
            room = segment_size;
        }
        /* take as many records as fit in `room` */
        if self.alloc.compare_exchange_weak(from, end, AcqRel, Acquire).is_ok() {
            return Ok((from, start, count, end));
        }
    }
}
```

It returns `from` *and* `start` because they differ when a record would
have straddled a segment and the run had to skip one. The reservation
covers the skipped tail, so publication must too — keying publication on
`start` instead makes the barrier wait for a value it never takes, which
deadlocks every later writer. That was a real bug and
`a_batch_that_skips_a_segment_still_publishes` is the test for it.

### Slots are published with a compare-exchange

```rust
match cell.compare_exchange(EMPTY, offset + 1, Release, Acquire) {
    Ok(_) => return true,
    Err(_) => continue,        // someone took it; look at what landed
}
```

A plain store would not do. Two threads probing concurrently can settle
on the same empty slot, and the loser's record would be left in the heap
with nothing pointing at it.

---

## 3. Why the mutex hypothesis failed

Worth understanding, because the evidence for it was real.

Scaling tracked write volume: 1.14× at 200 transactions a block against
1.31× at 40. The parallel region appeared to shrink as writes grew,
which is what a lock inside it looks like. The inference was that the
writes were serialising on the mutex.

They were. It just did not cost anything worth measuring. The mutex held
the probe, the key comparison and the append — about a microsecond each,
6,900 times a block, call it 7 ms against a 611 ms block. Visible in a
profile, invisible in a wall clock.

The gap between 1.14× and 1.31× had a duller explanation: **more
transactions mean more sequential work per block**, not more contention.
More nodes to hash when recomputing the root, more to walk in the
ordered commit, more to write. Work that grows with block size and that
no thread count divides.

The test proposed at the time — "after a correct fix those two scaling
numbers should converge" — was the right test. They were measured again
after the fix and they **diverged**: 0.88× and 1.32×. A good test that
refuted the thing it was built to confirm.

---

## 4. What is actually serialising now

```rust
fn publish(&self, from: u64, to: u64) {
    let mut spins = 0u32;
    while self.size.compare_exchange_weak(from, to, AcqRel, Acquire).is_err() {
        spins += 1;
        if spins < 64 { std::hint::spin_loop(); } else { std::thread::yield_now(); }
    }
}
```

That loop only succeeds once `size` has reached `from` — that is, once
**every writer that reserved earlier has published**. Reservations
complete out of order; publication may not, or `size` would run ahead of
a range still being written and break the invariant in section 2.

Which was affordable when a reservation was one record. A writer waited
for a predecessor to memcpy 115 bytes.

It is not affordable now that a reservation is a whole batch. Each
sublist reserves ~1,380 nodes, so the first to reserve blocks the other
three for its entire copy. The batching that made one thread 1.16–1.80×
faster is the same change that made four threads slower than one.

### The fix ([#320](https://github.com/SergioDemianLerner/rustock/issues/320))

Publish **out of order**. Keep a small record of in-flight ranges, and
when a writer finishes, advance `size` to the start of the lowest range
that is still incomplete rather than waiting for a turn.

A sketch, not a prescription: with at most a handful of batches in
flight — four sublists — a fixed-size array of `(start, end, done)`
under a short lock, or a sorted small-vec, is enough. The writer marks
its own range done, then advances `size` as far as the contiguous
completed prefix allows.

The invariant to preserve is unchanged and is the only hard
requirement: **`size` must never exceed the lowest unwritten byte.**

### What to expect from it

It should recover the parallel path without giving back the serial gain,
so roughly 42.2 s back to 38 s or better at 200 tx and four threads.
It will not deliver 4×, because of section 5.

---

## 5. The ceiling, before you start

At four threads with *few* writes — where neither the old mutex nor the
new publication ordering costs much — the CPU still only reaches 41%.
Most of a block is sequential by construction and has nothing to do with
any of this:

- the block-level maintenance run;
- the ordered commit of each sublist;
- the root hashing, which walks every dirty node;
- the lazy reload of the root between blocks.

By Amdahl the parallel fraction is a third of a block at best. And there
is a prior question: with a fast store and a prefetched access list, the
best measured configuration **uses one thread**, and adding threads made
it slower even before this regression existed. See
[`execution-performance.md`](execution-performance.md).

So fix the regression, because a change should not leave things worse
than it found them. Do not expect throughput from thread count.

---

## 6. How to tell whether you fixed it

**The regression test.** Four threads at 200 tx/block must beat one
thread again.

```sh
for t in 1 4; do
  /usr/bin/time -f "%P %e" cargo run --release --example parallel_bench -- <fixture> \
      --threads $t --sublists 4 --txs 200 --blocks 60 --flatdb true --flush-every 100
done
```

**The convergence test**, which is sharper and which is how the previous
hypothesis was refuted. Scaling at 200 and at 40 transactions a block
should land in the same place once nothing inside the parallel region
depends on batch size:

| tx/block | 1 thread | 4 threads | scaling |
|---|---|---|---|
| 200 | 37.3 s | 42.2 s | 0.88× |
| 40 | 10.1 s | 7.65 s | 1.32× |

**The CPU figure.** Four threads at about one core on four cores is the
symptom. If it does not move, the serialisation did not.

**And the correctness gate, which costs nothing.** `parallel_bench`
prints the final state root; it must not change. Every run quoted in
this document ends on the same one.
