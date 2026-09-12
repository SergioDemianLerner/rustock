# Epoch-based garbage collection for a content-addressed trie store

A design for reclaiming space in a node database that stores every historical
version of the state trie. Originally designed for rskj; written up here as a
specification, with measurements from rustock's RSK mainnet database and
suggested refinements.

- [1. Problem](#1-problem)
- [2. Terminology](#2-terminology)
- [3. Data model](#3-data-model)
- [4. Algorithm](#4-algorithm)
- [5. Invariants](#5-invariants)
- [6. Correctness argument](#6-correctness-argument)
- [7. Parameters](#7-parameters)
- [8. Failure and recovery](#8-failure-and-recovery)
- [9. Costs](#9-costs)
- [10. Suggested refinement: sequential mark](#10-suggested-refinement-sequential-mark)
- [11. Other suggestions](#11-other-suggestions)
- [12. Open questions](#12-open-questions)

---

## 1. Problem

A node that executes every block writes a new version of each trie node it
touches. Nodes are never updated in place — the trie is persistent — so the
store accumulates every historical state, while only a small part is reachable
from the current head.

Measured on rustock's RSK mainnet database at block 9,230,008:

| | |
|---|---|
| Trie nodes stored | 1,246,651,400 |
| Trie store on disk | 128.9 GB |
| Reachable from the head's state root | **916 MB** |
| Live fraction | **~0.7%** |

So roughly 99% of the trie store is historical versions. The problem is
reclaiming that without a reference count on every node and without a
deletion pass proportional to the number of dead entries.

---

## 2. Terminology

**Trie node.** A single serialised Unitrie node. Identified by the hash of its
own serialised bytes.

**Entry.** A key-value pair in the node database: `key = hash(value)`. Trie
nodes are entries; so are values longer than 32 bytes, which are stored
separately under the hash of the value.

**Content addressing.** The property that an entry's key is a cryptographic
hash of its value. Two entries with the same key therefore have identical
values, and an entry can be copied between databases without any referrer
changing.

**Epoch.** One physical database holding a contiguous slice of write history.
The store is a fixed-size, ordered list of epochs, written `E₀ … E_{N-1}`,
where `E₀` is the **oldest** and `E_{N-1}` the **newest**. `N ≥ 3`.

**Newest epoch (`E_{N-1}`).** The only epoch that receives writes.

**Oldest epoch (`E₀`).** The epoch that the next collection cycle will drain
and delete.

**Head.** The block at the tip of the canonical chain.

**Burial depth (`D`).** The minimum number of confirmations a block must have
before its state root may be used as a collection root. A configuration
parameter. Chosen so that `D` exceeds the deepest reorganisation the operator
is willing to survive.

**Collection root block (`H`).** The block whose state root is used as the
reachability root for one collection cycle. Chosen such that
`number(head) − number(H) ≥ D`. `H` is *not* the head: collecting against the
head would leave nothing recoverable if the chain reorganised.

**Collection root (`root(H)`).** The state root hash recorded in `H`'s header.
The entry point for the mark phase.

**Live set (`L`).** The set of entry keys reachable from `root(H)` by following
node references transitively, including hash references to long values. Formally
the least set satisfying:

```
root(H) ∈ L
k ∈ L  ∧  k' is referenced by the entry stored under k   ⟹  k' ∈ L
```

**Mark.** Computing `L`.

**Drain.** Copying every entry of `E₀` whose key is in `L` into `E_{N-1}`.

**Sweep.** Deleting `E₀` from disk.

**Rotation.** Deleting `E₀`, renumbering `E₁…E_{N-1}` down to `E₀…E_{N-2}`, and
creating a new empty `E_{N-1}`.

**Collection cycle.** Mark, drain, sweep, rotate.

---

## 3. Data model

The store is an ordered list of `N` independent databases:

```
   E₀            E₁            E₂   …   E_{N-1}
 oldest                                 newest
 (drained and                          (all writes
  deleted next)                          land here)
```

Every epoch holds content-addressed entries. **References cross epochs
freely and in both directions**: an entry in `E_{N-1}` may reference an entry in
`E₀` (an unchanged subtree from long ago), and an entry in `E₀` may reference an
entry in `E₁` only if that entry was later re-written there — which is possible,
because duplication is permitted. There is no ordering constraint on references.

This is exactly why an epoch cannot simply be deleted when it "ages out": recent
state routinely references old entries.

---

## 4. Algorithm

### 4.1 Write path

```
write(key, value):
    E_{N-1}.put(key, value)          # unconditional
```

Writes go only to the newest epoch, and **no existence check is performed**
against older epochs. If the entry already exists in `E₀`, it is written again.
This duplication is deliberate — see [invariant I4](#5-invariants) and
[§6](#6-correctness-argument). It is not merely a performance shortcut; the
correctness of collection depends on it.

### 4.2 Read path

```
read(key):
    for i from N-1 down to 0:
        if E_i contains key: return E_i.get(key)
    return NOT_FOUND
```

Newest first, since recently written entries are the most frequently read. A
miss costs one lookup per epoch. Each epoch should carry Bloom filters so a
miss is usually answered without touching disk.

### 4.3 Collection cycle

```
collect():
    H    := choose_block(head, D)      # number(head) - number(H) >= D
    L    := mark(root(H))              # §4.4
    drain(E₀, L)                       # §4.5
    sweep_and_rotate()                 # §4.6
```

The cycle may run concurrently with block processing. Mark and drain only read
`E₀` and write `E_{N-1}`; block processing also writes `E_{N-1}`. No coordination
is needed beyond the sweep — see [§4.6](#46-sweep-and-rotate).

### 4.4 Mark

Computes `L`, the set of keys reachable from `root(H)`.

The straightforward implementation is a recursive (or explicit-stack) traversal
following references. This is correct but has poor I/O behaviour; see
[§10](#10-suggested-refinement-sequential-mark) for an alternative with the same
result and much better locality.

### 4.5 Drain

```
drain(E₀, L):
    for (key, value) in E₀:            # sequential scan
        if key ∈ L:
            E_{N-1}.put(key, value)
```

Idempotent: re-running it writes identical bytes, because entries are
content-addressed. An interrupted drain may therefore be restarted from the
beginning without any undo.

Entries of `E₀` not in `L` are simply not copied. They are reclaimed by the
sweep.

### 4.6 Sweep and rotate

```
sweep_and_rotate():
    close E₀
    delete E₀ from disk                # one filesystem operation
    renumber E₁…E_{N-1} → E₀…E_{N-2}
    create empty E_{N-1}
```

The sweep is **O(1) in the number of dead entries** — it is a directory removal,
not a per-entry deletion.

The only coordination required is that no reader may be mid-lookup in `E₀` when
it is closed. A read-write lock over the epoch list, held briefly for the
renumbering, is sufficient.

---

## 5. Invariants

**I1 — Content addressing.** For every entry, `key = hash(value)`. Two entries
with the same key have identical values.

*Consequence:* copying an entry between epochs is invisible to anything holding
a reference to it. This is what makes relocation free, and it is the property
that distinguishes this design from a conventional copying collector, which must
rewrite referrers.

**I2 — Single writer epoch.** All writes from block processing, and all writes
from a drain, go to `E_{N-1}`.

**I3 — Read totality.** For any key referenced by any entry reachable from the
state root of any block in `[H, head]`, `read(key)` succeeds.

**I4 — Unconditional write.** The write path never skips a write because the
key exists in an older epoch.

*This invariant is load-bearing for correctness, not performance.* See
[§6](#6-correctness-argument).

**I5 — Burial.** `number(head) − number(H) ≥ D` at the moment `H` is chosen.

**I6 — Post-sweep reachability.** After a cycle completes, every key in `L` is
present in some epoch. Equivalently: deleting `E₀` removes no entry that is in
`L`, because the drain copied all of them into `E_{N-1}` first.

**I7 — Drain idempotence.** Running a drain twice produces the same store state
as running it once. Follows from I1.

---

## 6. Correctness argument

**Claim.** After a collection cycle, every node reachable from the state root of
any block in `[H, head]` is still readable.

Take any such node `k`, reachable from `root(B)` for some `B ∈ [H, head]`. At the
moment the sweep runs, `k`'s bytes are in at least one epoch. Two cases:

**Case 1 — `k` is not in `E₀`.** The sweep deletes only `E₀`, so `k` survives.

**Case 2 — `k` is in `E₀`.** Two sub-cases:

- **`k ∈ L`.** The drain copied it to `E_{N-1}` before the sweep. Survives (I6).

- **`k ∉ L`.** Then `k` is not reachable from `root(H)`, yet by assumption it is
  reachable from `root(B)` for some `B ∈ [H, head]`. Since `B ≥ H` and `k` is not
  reachable from `root(H)`, the reference to `k` must have been created by
  executing a block after `H`. Creating that reference required writing the
  referring node, and by **I4** the write path also wrote `k` itself to
  `E_{N-1}`, unconditionally, rather than skipping it because it already existed
  in `E₀`. So a copy of `k` exists outside `E₀`, and it survives.

∎

The second sub-case is the one that fails if the write path is "optimised" to
skip writes whose key already exists. Concretely: a contract's storage returns to
a value it held long ago, so block processing reconstructs a subtree
byte-identical to one living only in `E₀`. With a conditional write, the new
state would reference entries in a database about to be deleted. With I4, the
bytes are rewritten and the reference stays valid.

**What the design deliberately does *not* preserve:** entries in `E₀` that are
reachable only from roots strictly between `E₀`'s era and `H`. Those are dropped.
This is pruning by design — the node retains state for `H` and later, not for
every historical block. A node that must answer historical state queries for
arbitrary blocks cannot use this scheme.

---

## 7. Parameters

| Parameter | Meaning | Considerations |
|---|---|---|
| `N` | Number of epochs | ≥ 3. Larger `N` means smaller, more frequent sweeps and lower peak duplication, at the cost of more lookups on a read miss. |
| `D` | Burial depth | Must exceed the deepest survivable reorganisation. On a merged-mined chain, be conservative. |
| Rotation trigger | When a cycle begins | Size-based is more predictable than time-based; see [§11](#11-other-suggestions). |

---

## 8. Failure and recovery

**Crash during mark.** No writes have occurred. Restart the cycle.

**Crash during drain.** Some entries are duplicated into `E_{N-1}`. Harmless by
I1 and I7: restart the drain from the beginning.

**Crash during sweep.** Either `E₀` exists or it does not; a directory removal is
atomic at the filesystem level. If it is gone but renumbering did not complete,
the epoch list must be rebuilt from what is on disk at startup. Keeping the epoch
list derivable from directory names — rather than in a separate manifest — makes
this trivial and removes a consistency risk.

**Crash after sweep, before creating the new `E_{N-1}`.** Create it at startup.

---

## 9. Costs

Measured on rustock, RSK mainnet block 9,230,008, on a 4 vCPU / 7.6 GB machine
with network-attached SSD. rskj's own figures will differ, but the *shape* of
the costs is a property of the algorithm.

| Phase | Complexity | Measured |
|---|---|---|
| Mark (recursive) | O(live entries), **dependent random reads** | ~2,300 nodes/s |
| Drain | O(size of `E₀`), sequential read + write | disk-bound |
| Sweep | **O(1)** | one directory removal |

The asymmetry is the point. Deleting *n* entries from an LSM-tree store costs
*n* tombstones plus the compaction to merge them away — often more I/O than
writing them was. Here it is one syscall.

The cost that remains is the mark. Its access pattern — each lookup's address
known only after the previous lookup returns — is the slowest possible for a
block device: no prefetch, no batching, no parallelism within a single chain of
references.

For scale: a live set of 916 MB is on the order of 10⁷ nodes, so a recursive
mark at ~2,300 nodes/s is roughly an hour, competing with block processing for
I/O throughout.

---

## 10. Suggested refinement: sequential mark

The recursive mark is the expensive half of the cycle, and its cost is entirely
about **access pattern**, not data volume. The same set `L` can be computed with
sequential reads.

### 10.1 Why the recursive form is slow

```
mark(root):
    stack := [root]
    while stack not empty:
        k := stack.pop()
        v := read(k)              # random read; address unknown until now
        for each child in refs(v):
            stack.push(child)
```

Each `read(k)` must complete before the next address is known. The device sees
one outstanding random read at a time. Queue depth is 1, and the trie's node
keys are hashes, so successive reads land in unrelated parts of the keyspace.

An independent measurement in rustock makes the size of this effect concrete: a
chain walk following `parentHash` — the same dependent-read shape — ran at 1,065
blocks/s, while reading the same data with a **sequential** scan and walking it
in memory ran at 2,089,745 blocks/s. Identical logic; a factor of ~1,960 from
access pattern alone.

### 10.2 Frontier-batched mark (recommended)

Process the trie **level by level**. The gain is not that the reads become
sequential — it is that they become **independent**. Every key in one level was
discovered by the previous level, so the whole level can be in flight at once
instead of one request at a time. Depth of the device queue, not order on disk,
is what a dependent-read traversal is missing.

```
mark(root):
    L        := {root}
    frontier := [root]
    while frontier not empty:
        next := []
        for k, v in read_many(frontier):      # one batch, reads overlap
            for c in refs(v):
                if c ∉ L:
                    L.insert(c)
                    next.append(c)
        frontier := next
    return L
```

Properties:

- **Passes** equal the depth of the trie in nodes — tens, not thousands.
- **Reads within a pass are independent**, so they can be issued together
  (`multi_get`, an async batch, or a thread pool) and the device works at a
  queue depth equal to the batch size instead of 1.
- **Parallelisable.** Split each frontier into ranges and give each range to a
  thread. Threads share only the live set, which needs a concurrent
  set or per-thread sets merged between passes. In rustock, the same keyspace
  split applied to a trie copy gave 2.5× on 4 cores, limited by the shared write
  path rather than by reads — a mark has no write path, so it should scale
  better.
- **Memory** is `|L| × (32 bytes + set overhead)`, plus the frontier. For 10⁷
  nodes, a few hundred MB.

#### Sorting each frontier: only when the live set is dense

It is tempting to also sort each frontier so that a single forward-moving
iterator can serve the pass. That is a real optimisation *only when the live set
is dense enough in the store that consecutive sorted keys share data blocks*.
It is not free, and it can lose outright.

Let **d** be the density of the live set within the epoch being read — the
fraction of stored entries that are live — and let **b** be the number of
entries per data block. Sorted iteration helps when `d × b ≳ 1`, i.e. when a
block fetched for one live key also contains the next one. Below that, each
sorted key still lands in its own block and usually its own file, so the walk is
random anyway — and it is now a *worse* kind of random:

- A **point lookup** consults the bloom filter of each candidate file and skips
  the ones that cannot hold the key. With many files, most are skipped without
  any I/O.
- An **iterator seek** must position a cursor in *every* candidate file to
  establish the merge order. Bloom filters do not apply. The cost is paid per
  file, per seek.

Measured on rustock's mainnet store, marking a single block's state against the
full historical node set:

| | |
|---|---|
| Live nodes reachable from the root | ~10.8 × 10⁶ |
| Entries stored in the node column | 1.247 × 10⁹ |
| Density `d` | **0.87 %** |
| Mean gap between consecutive live keys | ~115 stored entries |
| Files in the column | 772 |

Three traversals over that store, same logic, same machine:

| Traversal | Rate |
|---|---|
| Recursive DFS, dependent single reads | 2,658 nodes/s (early) → 763 (at 49 %) |
| Frontier batched, sorted, per-key `get` | 956 nodes/s |
| Frontier batched, sorted, one cursor per batch | **479 nodes/s** |

Sorting bought nothing at `d = 0.87 %`, and replacing point lookups with a
cursor halved throughput, exactly as the bloom-filter argument predicts.

The density is a property of **which epoch is being marked**, not of the
algorithm. This is the case the epoch design is built to improve: marking `E₀`
right after a rotation reads a young, compact epoch where `d` is high and
sorting pays, rather than scanning a decade of accumulated history where it does
not. When implementing the mark, sort only if the epoch's measured density
clears the `d × b ≳ 1` bar; otherwise batch for concurrency and leave the keys
in discovery order.

### 10.3 Alternative: multi-pass sequential scan

If memory for the frontier is a concern, reachability can instead be computed by
repeated sequential scans over `E₀` itself:

```
mark_by_scan(root):
    L := {root}
    repeat:
        changed := false
        for (key, value) in E₀:              # fully sequential
            if key ∈ L:
                for c in refs(value):
                    if c ∉ L: L.insert(c); changed := true
    until not changed
    return L
```

Each pass is a pure sequential scan. The number of passes depends on how
adversarially the trie's depth is ordered relative to key order — worst case the
node depth, in practice fewer. This trades total bytes read for perfect locality,
and is preferable only when `|E₀|` is small relative to `|L|`, or when the
frontier will not fit in memory.

### 10.4 What not to do

Do not compute `L` by walking the trie while *also* following every reference
without memoising visited keys. On a content-addressed trie, shared subtrees are
re-walked once per reference and the traversal degenerates badly — in rustock
this produced a scan that made no measurable progress in 22 minutes. Always
memoise: on reaching a key already in `L`, stop descending.

---

## 11. Other suggestions

**Rotate by size, not by time.** `E_{N-1}` receives both new writes *and* the
drained live set, so it grows faster than a freshly rotated epoch. Time-based
rotation therefore yields uneven epoch spans and unpredictable peak disk usage.
A size threshold keeps behaviour stable.

**Measure read amplification.** A miss costs one lookup per epoch. Bloom filters
make this cheap in the common case, but it is a permanent tax paid in exchange
for a one-off sweep saving. Workloads that touch old state — `eth_call` against
contracts with cold storage — pay it most. Worth measuring rather than assuming,
particularly as `N` grows.

**Consider draining into `E_{N-2}` rather than `E_{N-1}`.** Drained entries are
by definition old — they survived long enough to be scanned. Placing them in the
newest epoch means they will be scanned again on every subsequent cycle. Putting
them one epoch back lets them age out on schedule. This complicates I2 and needs
care, but it avoids repeatedly copying a stable live set.

**Make the epoch list derivable from disk.** If epochs are directories named by
index or sequence number, startup can reconstruct the list by reading the
directory. A separate manifest is one more thing to keep consistent across a
crash, for no benefit.

**Run the mark concurrently with block processing.** It only reads, and the
drain is idempotent, so neither needs to stop the node. Only the sweep needs a
brief exclusive moment.

---

## 12. Open questions

1. **How much does the live set actually change between cycles?** If most of `L`
   is stable, each cycle re-copies the same entries. Measuring the overlap
   between consecutive live sets would say whether the drain-into-`E_{N-2}`
   refinement is worth its complexity.

2. **What is the real distribution of reads across epochs?** This determines the
   read-amplification cost and the right value of `N`.

3. **Should long values be collected on the same schedule?** They are entries
   like any other, but their size distribution is very different — a repeated
   contract bytecode is kilobytes where a trie node is ~100 bytes. Measurements
   on rustock show sharing is dominated by bytecode in *bytes* while storage
   cells dominate in *count*, so the two may warrant different policies.

4. **Is `D` sufficient under adversarial conditions?** The scheme is
   unrecoverable for a reorganisation deeper than `D`. Worth stating the assumed
   bound explicitly and deciding what a node should do if it observes one.
