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

    require not already collecting     # I10
    require root(H) is readable        # precondition P1
    require last_block(E₀) <= number(H) # precondition P2, see I8

    L    := mark(root(H))              # §4.4; fails if L is incomplete
    drain(E₀, L)                       # §4.5
    sweep_and_rotate()                 # §4.6
```

The cycle may run concurrently with block processing. Mark and drain only read
`E₀` and write `E_{N-1}`; block processing also writes `E_{N-1}`. No coordination
is needed beyond the sweep — see [§4.6](#46-sweep-and-rotate).

**The three preconditions are not defensive programming; each one, omitted,
deletes live state.**

**P1 — the collection root must be readable.** If `root(H)` is absent, the mark
returns a live set containing almost nothing, the drain copies almost nothing
forward, and the sweep deletes an epoch that was entirely reachable. A root can
legitimately be unreadable: a store seeded from a snapshot holds no state below
the snapshot's block, so `H` is out of range until the chain advances past it.
The correct response is to decline and wait, not to collect.

**P2 — `E₀` must not hold writes for blocks above `H`.** See
[I8](#5-invariants). This is the precondition whose absence caused the failure
recorded in [§8](#8-failure-and-recovery).

**The mark must be complete.** If any referenced entry is missing while marking,
`L` is a subset of the true live set and collecting on it deletes reachable
state. A missing entry is a hard error, not a warning: the store is already
damaged, and proceeding compounds it.

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

It is also the only irreversible step in the cycle, and the only one that can
destroy data. Mark and drain may be abandoned or repeated at will; a sweep
cannot be undone. Every precondition in [§4.3](#43-collection-cycle) exists to
be checked before this point.

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

**I8 — Write separation.** `E₀` may be swept only if every block whose writes
went into `E₀` is at or below `H`.

*Why it is needed.* I4 keeps later state alive by writing it unconditionally to
the newest epoch — which protects nothing if the newest epoch **is** the epoch
being swept. The correctness argument in [§6](#6-correctness-argument) says "a
copy of `k` exists outside `E₀`", and that step is only true when the writes for
blocks above `H` landed somewhere other than `E₀`.

*How it is satisfied.* Each epoch records the highest block whose writes went
into it, fixed at the moment it stops being the newest. A sweep is refused
unless that block is at or below `number(H)`. An epoch with no such record can
never be swept: unknown is treated as unsafe.

*Corollary — a sizing constraint, not just a check.* `E₀` stops receiving writes
`N−1` rotations before it is swept, so I8 holds automatically when

```
    (N − 1) × blocks_per_rotation   >   D
```

i.e. **the retention window must exceed the burial depth**. With epochs that
fill in days and a burial depth of hours this is satisfied by a wide margin and
the check never fires. It is worth stating because the failure mode is silent:
nothing about a store that violates it looks wrong until state is already gone.

**I9 — Seed immutability.** An epoch populated from outside the store — a trie
snapshot used to seed a new one — never receives writes.

*Why.* A seeded store begins with a single epoch, which makes that epoch both
the newest (the write target) and the oldest (the first sweep candidate). Those
are the two roles `N ≥ 3` exists to keep apart. Rotating immediately on open,
before any write can land, restores the separation and makes the seed's highest
block exactly the snapshot's block — which is what I8 needs in order to ever
permit a sweep.

**I10 — One cycle at a time.** At most one collection cycle runs against a store.

*Why.* Two concurrent cycles each drain and sweep `E₀`. The second deletes an
epoch the first has not finished copying out of.

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
  in `E₀`. By **I8**, `E_{N-1}` at that moment was not `E₀` — the block was
  above `H`, and `E₀` holds no writes for blocks above `H`. So a copy of `k`
  exists outside `E₀`, and it survives.

  *This is the step that fails without I8.* "A copy exists outside `E₀`" is not
  a consequence of I4 alone: if `E₀` was still the newest epoch when that block
  executed, the unconditional write put the only copy of `k` **into `E₀`**, and
  the sweep deletes it. I4 and I8 are both required, and neither implies the
  other.

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

`N` and the rotation trigger are not independent of `D`. Together they set the
retention window, and **I8** requires

```
    (N − 1) × blocks_per_rotation   >   D
```

Worked example, RSK mainnet: measured churn is ~40 KB of trie per block, so a
1 GB epoch fills in ~26,000 blocks. With `N = 4` the retention window is ~78,000
blocks against a burial depth of 4,000 — a margin of ~20x. Shrinking epochs to
force more frequent collection shrinks that margin proportionally; at 128 MB
epochs the window falls to ~9,800 blocks, still clear of `D` but no longer
comfortably.

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

### 8.1 Recorded failure: sweeping the write target (2026-09-14)

A production migration violated **I8** and destroyed state. It is recorded here
because the sequence looks reasonable at every individual step.

```
seed a new store with one epoch, from a snapshot at block 9,234,226
start the node; it executes 9,234,227 → 9,238,014
  ⚠ one epoch exists, so it is both newest and oldest:
    every one of those writes lands in E₀
force rotations to reach N = 4
force a cycle against root(9,234,400)
  ⚠ marks only what block 9,234,400 reaches; sweeps E₀
    → deletes every node written for blocks 9,234,400 → 9,238,014
      that existed only there
```

The node did not crash. It executed on, reading zeros where state had been,
taking cheaper code paths, and computed **176,127 gas for a block whose header
said 328,664**. The damage surfaced as a consensus mismatch roughly twenty
minutes after the sweep, with nothing in between to indicate a problem.

Three observations worth carrying:

- **The design was not at fault; the procedure was.** `N ≥ 3` exists to keep the
  write target and the sweep candidate apart. Seeding a single epoch and writing
  into it collapsed them by hand.
- **Silent until far downstream.** No error at sweep time, none at the next
  block, none until execution happened to read a deleted subtree. A destroyed
  trie announces itself only when something needs the part that is gone.
- **Recovery depended entirely on an outside copy.** The store itself held no
  redundancy — content addressing means one copy per epoch and no more. The node
  was restored from a separate archival trie in minutes; without it, the only
  route back would have been a full re-import.

I8, I9 and P1/P2 in [§4.3](#43-collection-cycle) are the response. A store that
cannot prove `E₀` safe to delete now refuses to delete it.

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

| Traversal | Queue depth | Read order | Rate |
|---|---|---|---|
| Recursive DFS, dependent single reads | 1 | discovery | 2,658 nodes/s (early) → 763 (at 49 %) |
| Frontier batched, sorted, one cursor per batch | 1 | sorted | 479 nodes/s |
| Frontier batched, `multi_get` | 1 | sorted | 517 nodes/s |
| Frontier batched, 16 threads of point `get` | 16 | discovery | **4,096 nodes/s** |

Sorting bought nothing at `d = 0.87 %`, and replacing point lookups with a
cursor halved throughput, exactly as the bloom-filter argument predicts.
`multi_get` was no better: it reuses one pinned superversion but still performs
the lookups one after another, so the queue depth stays at 1 — batching an API
call is not the same as batching the I/O.

What actually paid was **concurrency at unchanged read order**: the same point
lookups, in discovery order, issued by 16 threads. That is the one variable of
the three that changes how many requests the device has outstanding.

The density is a property of **which epoch is being marked**, not of the
algorithm. This is the case the epoch design is built to improve: marking `E₀`
right after a rotation reads a young, compact epoch where `d` is high and
sorting pays, rather than scanning a decade of accumulated history where it does
not. When implementing the mark, batch for concurrency first — that gain does not
depend on density and is worth roughly 5× here. Then sort *additionally* only if
the epoch's measured density clears the `d × b ≳ 1` bar; on a freshly rotated
`E₀` it should, and the two compose. On a decade of accumulated history it does
not, and sorting is a pessimisation.

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

## 11a. Implementation

Implemented in `crates/storage/src/epoch_store.rs`, selected with
`--trie-backend epoch`.

### Choosing it

The node only ever holds an `Arc<dyn TrieStore>`, so the collector is not a mode
of the existing store -- it is a different store satisfying the same trait, and
nothing downstream knows which one it got.

| Flag | Default | Meaning |
|---|---|---|
| `--trie-backend` | `single` | `single` keeps everything in the node's own database; `external` keeps everything in a database of its own; `epoch` collects |
| `--trie-dir` | `<data-dir>/trie-epochs` | Where a detached or epoch store lives |
| `--rpc-admin` | off | Enables `rsk_collectTrie` and `rsk_collectTrieStatus` |
| `--gc-epochs` | 4 | `N`, at least 3 |
| `--gc-rotate-mb` | 1024 | rotate once the newest epoch reaches this size |
| `--gc-burial` | 4000 | `D`, confirmations before a root may be collected against |
| `--gc-check-secs` | 60 | how often to test the rotation trigger |

The default is `single`, deliberately. Collecting means historical state below
the retention window stops being queryable, which is a real trade and not one to
make on an operator's behalf silently.

### Structure on disk

```
<data-dir>/trie-epochs/
  epoch-000000000007/     <- oldest, drained and deleted next
  epoch-000000000008/
  epoch-000000000009/
  epoch-000000000010/     <- newest, receives all writes
```

Each epoch directory also holds `epoch-last-block`: the highest block whose
writes went into it, written once as it stops being the newest. **I8** is checked
against this file. A seeded epoch has no such file and instead carries the
snapshot's own `trie_snapshot.json`, whose `block` field serves the same purpose
— which is why seeding must leave that file in place.

Epochs are named by a monotonic sequence rather than by position, so a rotation
never renames a directory. The ordered list is recovered on startup by sorting
the directory names, which means a crash mid-cycle needs no recovery beyond
restarting the cycle -- there is no partial state to repair, by I7.

### Guards

Each precondition in [§4.3](#43-collection-cycle) and each of I8–I10 is enforced
in code, and every one of them refuses rather than proceeding:

| Guard | Enforces | Behaviour |
|---|---|---|
| Collection root readable | P1 | `collect` returns an error naming the root; the background collector treats it as "not yet" and waits, at debug level, since it is expected while a seeded store waits for the chain to pass its snapshot block |
| Mark completeness | §4.3 | Any referenced entry missing during the mark aborts the cycle; the live set is incomplete and cannot be used to decide deletions |
| `E₀` write separation | P2, I8 | Refused unless `epoch-last-block` ≤ `number(H)`; an epoch with no record is never swept |
| Seed rotation on open | I9 | A store opened with a single non-empty epoch rotates before any write can reach it |
| Single cycle | I10 | Compare-and-swap; a second caller is refused, not queued |

Two further checks sit at node startup rather than in the collector, and exist
because the failure they prevent is silent rather than loud:

- `--trie-backend single` against a database whose trie column family is empty.
  RocksDB is opened with `create_missing_column_families`, so a database whose
  trie was detached has the family recreated **empty** on the next open, and the
  node would run against a blank trie with no error at all. It refuses, and
  names the backends that can read the detached store.
- `--trie-backend external` or `epoch` pointing at an empty directory while the
  database still holds its own trie. It refuses and prints the command that
  moves the trie out.

Neither fires on a fresh node or on an unmigrated one, and an unmigrated
database is deliberately not warned about: `single` remains the default and
keeps working, and a warning on a working configuration teaches operators to
ignore warnings.

### Forcing a cycle

Collection is triggered by size, which on a chain writing ~40 KB of trie per
block means an epoch fills in days. `rsk_collectTrie` starts a cycle on demand
without stopping the node, and `rsk_collectTrieStatus` reports progress and the
last cycle's figures. Both require `--rpc-admin`; without it they report as
unknown methods rather than as forbidden ones, so a node without them is
indistinguishable from one that never had them.

The call returns as soon as the cycle starts. Marking a mainnet live set takes
minutes — measured at 266 s for 11.2M nodes — which is far longer than an RPC
should hold a connection open.

The same preconditions apply to a forced cycle as to an automatic one. In
particular, forcing does not override I8: a cycle whose root sits below `E₀`'s
highest written block is refused, with the offending block named.

### Migrating an existing node

The procedure that produced the failure in [§8.1](#81-recorded-failure-sweeping-the-write-target-2026-09-14),
corrected:

1. Detach the trie into a database of its own and verify the node runs against
   it (`--trie-backend external`). This copy is the fallback for everything
   that follows.
2. Seed a new epoch store from a trie snapshot, **keeping `trie_snapshot.json`**.
3. Point `exec_head` at the snapshot's block, so the node re-executes forward
   from state the store actually holds.
4. Start with `--trie-backend epoch`. I9 rotates the seed away from the write
   path before the first write.
5. Leave the archival copy in place until the collected store has run through
   several cycles.

Step 1 is not optional. The store holds one copy of each entry by construction,
so it contains no redundancy of its own; recovery from a bad cycle depends
entirely on a copy kept outside it.

### Deviations from the specification above

Two, both deliberate:

**Mark is batched, not recursive.** §4.4 describes a traversal following
references. That is a chain of *dependent* reads -- each address is known only
once the previous read returns -- which leaves the device at queue depth 1.
Keys discovered at the same level are independent of one another and are read
together. §10 argues for this; the implementation does it from the start,
because the measurement in §10.2 showed it is worth ~5x on a real store.

**Drain scans, it does not seek.** §4.5 already specifies a sequential scan; it
is worth saying why the obvious alternative is wrong. Looking up each live key
in the oldest epoch would be a seek per survivor, and survivors are scattered.
Reading the epoch in key order and testing membership costs one pass regardless
of how many survive.

### Testing

Unit tests in `crates/storage/src/epoch_store.rs` pin one invariant each:

| Test | Pins |
|---|---|
| `unconditional_writes_keep_a_revived_subtree_alive` | I4 — a state reverting to an earlier value rebuilds a subtree byte-identical to one living only in `E₀`, and must survive |
| `refuses_to_sweep_an_epoch_holding_writes_above_the_root` | I8 — reproduces [§8.1](#81-recorded-failure-sweeping-the-write-target-2026-09-14) |
| `a_seeded_epoch_is_rotated_before_it_can_be_written_to` | I9 — the seed is byte-identical after a write lands |
| `refuses_to_collect_against_an_unresolvable_root` | P1 — and asserts nothing was swept |
| `collection_keeps_the_state_it_collected_against` | I6 |
| `collection_drops_state_older_than_the_collection_root` | that collection actually reclaims, rather than quietly keeping everything |
| `reads_find_entries_in_older_epochs` | the read path across epochs |
| `rejects_fewer_than_three_epochs` | `N ≥ 3` |

Two notes on writing these, both learned the hard way:

- **A test for a guard must fail for the right reason.** The first version of
  the I8 test used `N = 3`, which made its third call a *real* cycle that
  legitimately swept the old state — so the P1 guard fired instead of the guard
  under test, and the test passed while proving nothing. It uses `N = 4` so all
  three preparatory calls are growth rotations.
- **Verify a regression test fails without the fix.** Checking this is itself
  easy to get wrong: `git stash push <path>` on a clean tree exits 0 without
  stashing, so a "revert, re-run, confirm failure" step can silently test the
  fixed code twice and report green.

`crates/cli/examples/gc_bench.rs` drives a synthetic chain whose every block
increments 1000 sequential storage slots of one contract. Two workloads:

- `--workload growing` advances the slot window, so the live set itself grows.
  A collector can only make growth *sublinear* here -- it cannot make it flat,
  because the live state is expanding. This is what a real chain does.
- `--workload bounded` cycles over a fixed range, so the live set is constant
  and every write supersedes an earlier version. This is the case the design is
  pitched at, and the one where a collector should hold size flat indefinitely
  while an uncollected store grows forever.

Note that RSK storage keys hash the slot (`keccak256(slot)[0:10]`), so
"sequential" slots do not occupy adjacent trie paths. They scatter, which makes
this harder than a clustered workload and more representative.

The harness verifies as well as measures. Each run ends by re-reading the head
state root, re-hashing it, walking the whole state looking for a dangling
reference, and checking that the storage counters hold the values they should. A
collector that is fast and wrong is worse than no collector.

One trap worth recording: the first version of the harness reported *zero* store
reads. `save` leaves the in-memory tree materialised, so every later block walked
RAM and never touched the store -- it was benchmarking an in-memory trie. The
harness now reloads the root from its hash each block, which is what a node
actually does between blocks.

### Known gap

`CachedTrieStore` wraps a RocksDB handle rather than a `TrieStore`, so it cannot
sit in front of the epoch store without a refactor. Execution with the collector
enabled therefore pays a real read and write per node. The synthetic comparison
is fair because neither side is cached, but a node-level comparison against a
cached single backend would not be, and has not been made.


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
