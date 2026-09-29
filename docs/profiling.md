# Profiling a running node

Three one-shot modes that answer questions about a node's state without
syncing, without stopping it, and without writing to it.

All three open both databases **read-only**. RocksDB's read-only open does not
take the lock, so they can be pointed at a node that is serving traffic. The
measurement below was taken against the production node while it followed the
chain; it stayed `active` throughout and its head kept advancing.

Anything that executes blocks also wraps the trie store so every write stays in
memory. Execution produces state; that state has to be readable back within the
block, and it must not reach the disk of a database another process owns.

---

## `--verify-state [ROOT]`

Walks the state under a root and confirms every node is present and readable.
The same check a snapshot sync runs before accepting a downloaded state, on
demand. With no argument it uses the executed head's root.

```
rustock --data-dir /var/lib/rustock \
        --trie-backend epoch --trie-dir /var/lib/rustock/trie-epochs \
        --no-rpc --verify-state
```

```
Verifying the state under 0x278e0e82... with 4 thread(s)
State verified: 12514078 nodes in 39.7s (315373 nodes/s)
```

It answers one question: **is this node's state actually all there?** A missing
node is reported with its hash, distinguishing a missing *node* from a missing
*long value* — a node can be present while the bytes its value stands for are
not, because long values live under their own hash.

This is not a check on the peers that supplied the state. Every chunk was
proved against the root as it arrived. This checks the node itself: that what
was verified reached the disk and can be read back.

### Thread count

Verification is CPU-bound and follows the core count, bounded by
`--read-threads`. Measured on four cores, on a 12,514,078-node state:

| threads | time | nodes/s | vs 1 thread |
|---|---|---|---|
| 1 | 116.8 s | 107,172 | 1.00× |
| 2 | 69.9 s | 178,904 | 1.67× |
| **4** | **39.0 s** | **320,649** | **2.99×** |
| 8 | 38.7 s | 323,332 | 3.02× |
| 16 | 69.3 s | 180,614 | 1.69× |

Sixteen threads is **1.8× slower than four**. This is the opposite of the
header and trie point lookups `--read-threads` was sized for, which wait on
RocksDB and want a deep queue. Two different questions; the default follows
cores for this one.

These figures are on a warm page cache (875 MB of state, 7 GB of RAM). A cold
run is slower and more I/O-bound, which would shift the optimum back toward
more threads.

---

## `--measure-block-reads N`

Re-executes the last `N` blocks and reports what each one had to be given.

```
rustock --data-dir /var/lib/rustock \
        --trie-backend epoch --trie-dir /var/lib/rustock/trie-epochs \
        --no-rpc --measure-block-reads 1000
```

```
Trie reads over 1001 block(s), 2164 transaction(s), 0 skipped:
  distinct nodes / block   277.2
  get calls / block      1535.7
  node bytes / block      47304
  distinct nodes / tx      128.2
  busiest block         #9280477 with 2690 nodes
  totals                 277522 nodes, 47351556 bytes
```

### What the numbers mean

**Distinct** counts each node once per block. A node read twice within a block
travels once, so this is the witness, not the workload.

**Calls** counts every `get`. The gap — 1,535.7 against 277.2, a **5.5× repeat
rate** — is what the in-memory cache absorbs during execution. It is the
difference between "reads issued" and "data that must be carried".

**Bytes** is the encoded size of exactly those distinct nodes: what shipping
them would cost. Long values are included, since they are fetched through the
same `get`.

**Skipped** blocks are ones whose pre-state is no longer in the trie store.
This only works on recent blocks: the epoch collector keeps `--gc-burial`
blocks of history (4,000 by default), so a range older than that will report
skips rather than quietly averaging over fewer blocks.

### Sizing a stateless witness

This is the measurement the question "how much data does a block need to be
verified statelessly" actually wants, and it is not derivable from the trie's
shape — which nodes a block needs depends on the accounts and storage slots its
transactions reach, and on how much their paths overlap.

Two things make the answer smaller than a first estimate suggests:

**The paths are already merged.** The executor resolves every interior node on
the way down to a value, so the distinct count *is* the union of all the
block's Merkle paths. Sharing near the root is counted once, not once per leaf.
Multiplying a leaf count by an average depth would double-count heavily.

**The sibling hashes are already inside the nodes.** A unitrie node's encoded
message carries its children's hashes. Shipping the node messages ships the
proof; there is no separate 32 bytes per level to add.

So, at the measured rate:

| | |
|---|---|
| witness per block | **~46 KB** |
| per transaction | 128 nodes, ~21 KB |
| busiest block observed | 2,690 nodes, ~455 KB — **9.7× the mean** |
| per day (2,880 blocks) | ~130 MB |
| per year | **~48 GB** |

The tail matters more than the mean. A design sized on 46 KB stalls on a block
needing half a megabyte, and the ratio above is from a thousand blocks — a
longer sample would very likely find worse. Percentiles are the thing to
measure next, not a larger average.

### What it is not

It counts every node the executor resolved, including the interior nodes on
each path. That is the right number for a witness that ships the subtrie, and
an over-count for one that ships only the values plus sibling hashes. Both
framings are available from the same run — `distinct` for the first, and the
difference between `distinct` and the value-bearing nodes for the second — but
only the first is reported today.

---

## `--verify-state ROOT --trie-shape`

Reports the trie's shape instead of verifying it: node count, value-bearing
nodes, mean and maximum value depth, how many children are reached by hash
versus embedded in their parent, and value bytes held inline versus
separately.

Useful for reasoning about proof sizes in the abstract. For an actual witness
size, `--measure-block-reads` is the better instrument, because the shape does
not say which nodes a block reaches.

---

## Safety

| | |
|---|---|
| databases | opened read-only; RocksDB's read-only open takes no lock |
| writes during execution | buffered in memory, never forwarded |
| effect on a running node | none observed; it stayed `active` and kept following the chain |
| flush | a no-op in these modes |

The one cost is page cache: a full walk pulls the state through it and may
evict what the running node was using. On a machine where that matters, run it
when the node is not under load.

## Related

- `docs/freezer-estimates.md` — the read-amplification measurements that
  motivated the header freezer
- `docs/snapshot-sync-estimates.md` — phase timings for a full snapshot sync
