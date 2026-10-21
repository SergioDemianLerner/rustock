# The lock that is holding back parallel execution

A guide to one specific bottleneck in FlatDB, written for someone who
did not write the code and wants to fix it.

No prior knowledge of the store is assumed. Everything you need is
quoted here.

Related: [#319](https://github.com/SergioDemianLerner/rustock/issues/319)
for the issue, [`execution-performance.md`](execution-performance.md)
for where this sits among the other bottlenecks, and `crates/flatdb/`
for the code.

---

## 1. The symptom

Execute 100 blocks on a four-core machine, varying only the thread
count:

| threads | wall | CPU |
|---|---|---|
| 1 | 64.1 s | 67% |
| 2 | 67.2 s | 89% |
| 3 | 62.8 s | 96% |
| 4 | 58.7 s | **99%** |

Four threads buy 9%.

The CPU column is the tell. On four cores, work that genuinely ran on
four threads would approach **400%**. It reaches 99% — one core's worth.
Three of the four threads are, on average, doing nothing.

They are not waiting on the disk. That was the previous bottleneck and
it was fixed; the same test on the old store sat at 46% CPU with one
thread, meaning it spent half its time waiting for reads. FlatDB made
reads cheap. The threads now wait on **each other**.

---

## 2. Where the lock is

One line, in `crates/flatdb/src/lib.rs`:

```rust
pub fn put(&self, key: &[u8], value: &[u8]) -> io::Result<()> {
    self.check_writable()?;
    {
        let _guard = self.writer.lock().expect("flatdb writer");   // <-- here
        self.index.put(&self.heap, &self.keys, key, value)?;
        self.puts.fetch_add(1, Ordering::Relaxed);
        self.committed_heap_size.store(self.heap.size(), Ordering::Release);
    }
    ...
}
```

Every write to the store passes through that mutex, one at a time.

A block of 200 transactions rewrites roughly **6,900 trie nodes** —
every node on the path from each touched account up to the root. All
6,900 go through this lock, however many threads are executing.

Reads do not. `Index::get` takes no exclusive lock at all and several
threads can be inside it at once. The asymmetry is deliberate and it is
why reads scale and writes do not.

---

## 3. What the lock is actually protecting

`index.put` does three things while holding it. Only one of them needs
protection.

### (a) Probing — does not need the lock

The index is a hash table with linear probing. To insert, you walk
forward from `hash(key)` until you find an empty slot, checking each
occupied one in case your key is already there.

The check is expensive, and for a reason specific to this design: **the
index stores no key bytes.** A slot holds nothing but a heap offset. So
to ask "is this slot my key?", you have to fetch the record it points at
and recompute `H(value)`:

```rust
fn key_of(keys: &dyn Keys, record: &Record<'_>) -> Vec<u8> {
    match record.key {
        Some(stored) => stored.to_vec(),
        None => keys.compute_key(record.data),     // a Keccak-256, ~483 ns
    }
}
```

That is a hash per probe step, roughly 1.5 steps per insert, plus a
possible page fault to fetch the record. **None of it mutates
anything.** Two threads probing at the same time cannot interfere.

### (b) Publishing a slot — does not need the lock

Once a free slot is found, the record's location is written into it:

```rust
unsafe { table.at(target) }.store(offset + 1, Ordering::Release);
```

`table.at(i)` returns an `&AtomicU64`. This is already an atomic
operation with release ordering — it is *designed* to be safe without a
lock. The matching `Ordering::Acquire` is in `get`.

### (c) Appending to the heap — **this one does need it**

```rust
pub fn append(&self, key: &[u8], data: &[u8], store_key: bool) -> io::Result<u64> {
    let total = Self::record_size(data.len(), store_key, key.len());

    let mut size = self.size.load(Ordering::Relaxed);        // 1. read the frontier
    let mut segment = (size / self.segment_size) as usize;
    let mut offset = (size % self.segment_size) as usize;
    if offset + total > self.segment_size as usize {          // 2. maybe skip a segment
        segment += 1;
        offset = 0;
        size = segment as u64 * self.segment_size;
    }
    ...
    unsafe {
        /* 3. write the record's bytes at `offset` */
    }
    self.size.store(size + total as u64, Ordering::Release);  // 4. publish
    Ok(size)
}
```

Step 1 reads `size`, step 4 writes it back. That is a **read-modify-write
that is not atomic**. Two threads running this concurrently both read the
same `size`, both decide to write at the same offset, and one record
lands on top of the other.

So the lock is genuinely load-bearing — but only for this, and this is
the cheap part. The expensive parts, the probe and its hashes, are
inside the lock for no reason.

---

## 4. The obvious fix is wrong, and here is why

The natural idea: make the reservation atomic and move the byte-writing
outside.

```rust
let start = self.size.fetch_add(total, Ordering::AcqRel);   // reserve
/* write the bytes at `start`, no lock held */               // then write
```

Each thread gets its own range, so no two writes overlap. It looks
right. **It is not**, and the reason is the single most important thing
in this document.

`size` has **two jobs at once**. It is the allocation cursor, and it is
also the signal that says "everything below here is written and safe to
read". Here is the reader:

```rust
pub fn with_record<R>(&self, offset: u64, key_length: usize, f: ...) -> Option<R> {
    let size = self.size.load(Ordering::Acquire);
    if offset >= size {
        return None;              // beyond the frontier: not a record
    }
    ...                           // otherwise, read the bytes
}
```

The pairing is what makes lock-free reading safe:

- the writer writes the bytes, **then** stores `size` with `Release`;
- the reader loads `size` with `Acquire`, **then** reads the bytes.

A reader therefore either sees the old `size` — and rejects the offset
as beyond the frontier — or sees the new one, in which case the
release/acquire edge guarantees the bytes it is about to read are
already there.

`fetch_add` before writing **moves the frontier in front of the bytes**.
There is now a window where `size` says a range is readable and the
range contains whatever was there before, while a thread is overwriting
it.

Can a reader actually land in that window? Yes. Slots are never cleared,
so the table can hold slots left behind by an interrupted flush, pointing
at offsets that were beyond the frontier at the time. As the frontier
advances past them they start resolving again (the store calls these
*resurrected* slots and handles them deliberately). A reader following
one lands exactly in the range being written.

A common objection: *the content is address-checked, so a bad read is
rejected anyway.* That argument is about which **values** come back. A
concurrent read and write of the same memory with no synchronisation
between them is a data race, and a data race is undefined behaviour
regardless of what values it would have produced. The compiler is
entitled to assume it cannot happen.

---

## 5. Two fixes that do work

### Option A — narrow the lock (simpler)

Keep the mutex, but hold it only for the append and the slot store.
Move the probe and its hashing outside.

```
outside the lock:  probe from hash(key), find a candidate slot
                   (or discover the key is already present — then stop)
inside the lock:   resume probing from the candidate, append, publish
```

Why resuming is correct: a slot that was occupied when you probed it
stays occupied. The only thing that empties slots is `grow`, which takes
the table's *write* lock and would make your candidate stale — detect a
capacity change and start over.

What you get: the hashing and the record fetches, which are the bulk of
the time, now overlap across threads. The remaining critical section is
a memcpy of about 75 bytes plus an atomic store.

Watch for: two threads may insert the same key concurrently, since
neither sees the other's unpublished slot. That is already a handled
condition — content addressing means both records are identical, and
`grow` drops duplicates — but it does mean the entry count can
temporarily exceed the live key count.

### Option B — split the two jobs of `size` (faster, more work)

Give the heap two frontiers:

- `alloc` — the allocation cursor. Reserved with a CAS loop, which also
  handles the segment skip atomically.
- `size` — the publication frontier. Advanced **only in order**.

```
let (start, end) = reserve();              // CAS on `alloc`
write the bytes into [start, end);         // no lock held, fully concurrent
while size.load(Acquire) != start { yield }   // wait your turn
size.store(end, Release);                  // publish
```

The invariant survives exactly: `size` still only advances when
everything below it is written, so `with_record` is unchanged and reads
stay safe.

The expensive part — the memcpy and the page faults it causes on fresh
heap pages — now genuinely overlaps. Only the publication is ordered,
and that is a single atomic store.

Cost: a thread descheduled after reserving blocks later publishers until
it finishes writing one record.

---

## 6. How to tell whether you fixed it

Two measurements, in this order.

**Does the CPU go up?** This is the direct test. Four threads at 99% on
four cores is the symptom. If your change does not move that number, it
did not remove the serialisation, whatever the wall clock says.

```sh
/usr/bin/time -f "%P %e" cargo run --release --example parallel_bench -- <fixture> \
    --threads 4 --sublists 4 --txs 200 --blocks 100 --flatdb true --flush-every 100
```

**Does the scaling track the write volume?** This is how the lock was
located in the first place, and it is the sharper test.

| tx/block | 1 thread | 4 threads | scaling |
|---|---|---|---|
| 200 | 43.4 s | 38.0 s | 1.14× |
| 40 | 18.2 s | 13.9 s | 1.31× |

Fewer writes, better scaling — the parallel region shrinks as writes
grow, which is what a lock inside it looks like. **After a correct fix,
those two scaling numbers should converge.** If 200 tx/block still
scales noticeably worse than 40, something is still serialised.

Run it with `--txs 200` and `--txs 40`, one and four threads, 60 blocks.

And the correctness gate, which costs nothing: `parallel_bench` prints
the final state root. **It must not change.** Every run in the
measurements above ends on the same root; a change means the fix altered
the chain, not its speed.

---

## 7. What it is worth, so you can decide whether to start

Be realistic about the ceiling before spending a week on it.

Applying Amdahl's law to the measurements, the parallel fraction of a
block is **16%** at 200 transactions and **32%** at 40. Perfect removal
of the lock recovers what the writes are stealing — roughly from 16%
back towards 32% — which is about **10% of wall clock**.

It is not 4×, and the reason is that most of a block is sequential by
construction and has nothing to do with this lock:

- the block-level maintenance run;
- the ordered commit of each sublist;
- the root hashing, which walks every dirty node;
- the lazy reload of the root between blocks.

At four threads with *few* writes the CPU is still only 41%, which says
the lock is not the only thing in the way.

So: a real bottleneck, a bounded prize, and a well-defined fix. Option A
is a day's careful work. Option B is the one that actually removes the
serialisation and deserves tests written before the code.
