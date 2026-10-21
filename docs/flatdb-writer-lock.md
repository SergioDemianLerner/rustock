# What serialises FlatDB's writes

A guide to the concurrency of FlatDB's write path, for someone who did
not write the code and wants to make it faster.

It is also the record of three claims that did not survive being
measured again, kept because each looked sound and the next person
deserves to know which arguments already failed.

1. That a writer mutex was holding back parallel execution. The mutex
   was removed. **Nothing got faster.**
2. That batching the writes was then worth 1.16–1.80× on a single
   thread. **It is not measurable**, in either direction.
3. That batching had made four threads slower than one. **It had not** —
   and the fix written for that measured slightly worse and was
   reverted.

All three came from single runs on a machine whose spread is severe.
See §7, which is the most useful section here.

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

### What the same change did buy: nothing measurable either

The work that replaced the mutex also batched the writes — a block's
whole dirty subtree is collected and written in one call instead of
roughly 6,900. Single runs put that at 1.16× and 1.80×, and the commit
that landed it says so. **Repeated three times, it does not replicate.**

Medians of three, 60 blocks:

| | without batching | with it |
|---|---|---|
| 40 tx/block, 1 thread | 20.7 s | 20.1 s |
| 40 tx/block, 4 threads | 18.1 s | 15.4 s |
| 200 tx/block, 1 thread | 42.6 s | **46.6 s** |
| 200 tx/block, 4 threads | 40.1 s | 37.3 s |

Three to fifteen per cent, in both directions, drawn from samples that
include 28.2 s, 40.3 s and 189.5 s for identical configurations. Nothing
here is resolvable.

The change is kept: one reservation a block instead of 6,900 is better
structure whatever the clock says, the tests are stronger for it, and it
measures no worse. But **the performance claim in `8336b24` is
withdrawn** — see §7 for why it was made.

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

## 4. The regression that was not there either

An earlier version of this section said that batching had made four
threads slower than one on full blocks, blamed `publish`'s ordered wait,
and specified a fix. The fix was written. **Neither held up.**

Three runs each, 60 blocks:

| 200 tx/block | 1 thread | 4 threads | median |
|---|---|---|---|
| as it stands | 189.5 / 43.2 / 46.6 | 39.8 / 37.3 / 36.7 | 46.6 / **37.3** |
| with out-of-order publication | 40.9 / 43.2 / 46.2 | 38.8 / 42.5 / 41.9 | 43.2 / 41.9 |

Four threads are 1.25× faster than one, as it stands. The 42.2 s that
started it was a single run and does not reproduce. The fix measured
slightly worse and was reverted.

### Why the ordered wait costs less than it looks like it should

`publish` does advance the frontier strictly in reservation order, and a
writer does wait for every earlier reservation to complete:

```rust
fn publish(&self, from: u64, to: u64) {
    while self.size.compare_exchange_weak(from, to, AcqRel, Acquire).is_err() {
        /* spin, then yield */
    }
}
```

The reason it is affordable came out of instrumenting it. Over 20
blocks, **27,879 puts were served by 20 reservations, and the
publication queue never exceeded depth one.** Batching collapses a
block's writes into a single reservation per writer, so in practice
there is rarely anything outstanding to wait behind.

The pathology is real in principle — a slow writer *would* hold up later
publishers — and it does not arise at this workload's shape. If a future
change puts many reservations in flight at once, revisit it; the
out-of-order design is in the git history at the commit that closed
#320.

## 5. The ceiling, before you start

At four threads with *few* writes — where neither the old mutex nor the
new publication ordering costs much — the CPU still only reaches 41%.
Most of a block is sequential by construction and has nothing to do with
any of this:

- the block-level maintenance run;
- the ordered commit of each sublist;
- the root hashing, which walks every dirty node;
- the lazy reload of the root between blocks.

**Both of those claims have since been overtaken**, and the correction
belongs here because it is the same kind of mistake this document is
about. The ordered commit in that list was not merely sequential, it
was *quadratic* in the accounts a block touched —
`NodeRef::Node(Box<TrieNode>)` could not share a subtree, so every
descent deep-copied the ones it passed. With an `Arc` the commit is 15×
cheaper, the parallel fraction is about half a block rather than a
third, and four threads went from 0.98× of one thread to 1.47×. The
"best configuration uses one thread" result was an artefact of a cost
four sublists paid four times.

So: the ceiling was real, it was mostly one defect, and the arithmetic
above should not be reused. See
[`execution-performance.md`](execution-performance.md) for the
replacement.

---

## 6. How to tell whether you fixed it

**The regression test.** Four threads at 200 tx/block must beat one
thread again.

```sh
for t in 1 4; do
  /usr/bin/time -f "%P %e" cargo run --release --example loadtest -- <fixture> \
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

**And the correctness gate, which costs nothing.** `loadtest`
prints the final state root; it must not change. Every run quoted in
this document ends on the same one.

---

## 7. Measure three times

Every quantitative conclusion in this area was drawn from single runs,
and repetition dissolved all of them. They shared a direction: each made
the story more interesting than the truth.

The spread is the reason. On this host the same configuration on the
same binary has produced **189.5 s and 36.7 s**, and the mainnet node
runs on the same four cores. A 10% difference between two single runs
carries no information at all.

So: three runs minimum, report medians rather than means, and make no
claim about a difference smaller than the spread you just measured. If
that sounds slow, it is faster than writing a fix for a problem that is
not there.
