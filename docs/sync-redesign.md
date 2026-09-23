# A sync design in which these stalls cannot happen

**Written:** 2026-09-22, after the fifth mainnet stall of the same family
**Audience:** anyone who knows what a blockchain is. No familiarity with
rustock's sync code is assumed; everything it needs is built up in Part I.

**Status:** stages 1–5 implemented and running on mainnet since 2026-09-23.
Stage 6 not started.

| Stage | State | Where |
|---|---|---|
| 1 — Φ watchdog | **done** | `crates/sync/src/watchdog.rs` |
| 2 — invariant after every commit | **done** | `crates/sync/src/invariant.rs` |
| 3 — Cursor | **done**, as a read-through rather than a derivation (§10) | `crates/storage/src/position.rs` |
| 4 — `Validated` | **done** | `crates/storage/src/position.rs` |
| 5 — simulator | **done, partially** — see §18a for what it does not do | `crates/storage/tests/` |
| 6 — `step()` | not started | — |

> **Read §18a, §17(c) and §23 before citing this document as evidence.** The
> design sections describe what was intended; those three describe the gap
> between that and what exists. In particular **liveness is not established**,
> and the simulator does not exercise the protocol at all.
>
> Two stalls (6 and 7) happened *after* this was written, and one of them was
> caused by the fix for stall 5. Appendix C records two further defects the
> simulator found in the implementation of this very document. The corrections
> are inline, marked as blockquotes, rather than rewritten into the original
> text — the way the design was wrong is more useful than a clean draft.

---

## How to read this

The document has four parts. They can be read independently, but they build.

| Part | What it does | Read it if |
|---|---|---|
| **I** (§1–§7) | Explains what syncing is, how rustock does it today, and walks through the first five stalls with diagrams (stalls 6 and 7 are in Appendix A) | You want to understand *why* this keeps happening |
| **II** (§8–§15) | The proposed design, one principle at a time | You want to evaluate the proposal |
| **III** (§16–§20) | Replays each historical stall against the new design; defines what "prove" means and how | You want to know whether it actually closes the class |
| **IV** (§21–§23) | Migration, staged by cost and risk | You want to decide what to do on Monday |
| **Appendices** | The stall table (A), the invariant in full (B), what the simulator found (C), and how all of it was tested (D) | You want the evidence rather than the argument |

If you read only one section, read **§20** — it argues for the Φ watchdog,
thirty lines of code that would have caught every stall in this document and
that can ship without any of the rest. If you read two, read **Appendix D**,
which is the only place that says what the evidence for all of this is
actually worth.

---

# Part I — Understanding the problem

## 1. What a syncing node has to do

A blockchain is a linked list built backwards. Each **block** contains a
**header**, and each header contains the hash of its parent's header:

```
   #100          #101          #102          #103
  ┌──────┐      ┌──────┐      ┌──────┐      ┌──────┐
  │ hdr  │◄─────│ hdr  │◄─────│ hdr  │◄─────│ hdr  │
  │ ...  │parent│ ...  │parent│ ...  │parent│ ...  │
  └──────┘      └──────┘      └──────┘      └──────┘
   0xAA…         0xBB…         0xCC…         0xDD…
```

Because a header names its parent by *hash*, and a hash is computed from the
content, the link cannot be forged or accidentally rewired. If you hold
block #103 and it says its parent is `0xCC…`, then *any* block you can find
whose hash is `0xCC…` is unambiguously its parent. This is the single most
useful property in the whole system, and the design in Part II leans on it
heavily.

A node that starts with only the genesis block and wants to reach the current
tip must:

1. **discover** what the tip is, by asking peers;
2. **download** every header between where it is and there;
3. **download the bodies** (the transactions) for those headers;
4. **execute** each block's transactions in order, computing the resulting
   state and checking it against the `state_root` in the header.

Steps 2 and 3 are separated because headers are small (~500 bytes) and bodies
are large. Downloading all the headers first lets the node verify the shape
of the chain cheaply, and fetch bodies in parallel from many peers afterwards.

## 2. The complication: the chain is not a line

At any moment, two miners may produce a block at the same height. The network
briefly holds two candidate chains, and then one wins — the one with more
accumulated proof-of-work. This is a **reorganisation**, or **reorg**:

```
                              ┌──────┐
                       ┌──────│ #102a│   ← our node downloaded this one
                       │      └──────┘
   #100      #101      │
  ┌──────┐  ┌──────┐◄──┤
  │      │◄─│      │   │      ┌──────┐      ┌──────┐
  └──────┘  └──────┘   └──────│ #102b│◄─────│ #103 │   ← the network kept
                              └──────┘      └──────┘      this one
```

Here `#102a` is **orphaned**: it is a real, valid block that is no longer on
the chain. `#102b` is canonical.

Reorgs are normal and frequent — on Rootstock, typically one block deep, many
times a day. **A node must handle them as routine, not as an error.** Every
stall in this document begins with a reorg.

The vocabulary that follows from this:

- **canonical** — on the winning chain, as the node currently believes
- **orphaned** — a block we hold that turned out not to be canonical
- **fork point** — the last block both branches agree on (`#101` above)
- **sibling** — another block at the same height (`#102a` and `#102b`)

## 3. The four things rustock stores

Rustock's database holds four distinct kinds of information about position.
The distinction matters enormously, because the bugs live in the gaps between
them.

**(a) Headers, keyed by hash.** `hash → header`. This is a pure content-
addressed map. A header stored under hash `h` is immutable and self-verifying:
recompute the hash of the content and you get `h` back. **This data cannot
become internally inconsistent.** Remember that; it becomes the foundation of
the proposal.

**(b) The height index.** `height → [all hashes we hold at that height]`.
After the reorg above, this holds `102 → [102a, 102b]`. It describes what we
*have*, making no claim about what is canonical.

```
  height index                canonical index
  ┌────┬────────────┐         ┌────┬───────┐
  │101 │ [101]      │         │101 │ 101   │
  │102 │ [102a,102b]│         │102 │ 102a  │  ← a CLAIM, and here a wrong one
  │103 │ [103]      │         │103 │  —    │
  └────┴────────────┘         └────┴───────┘
   what we HAVE                what we BELIEVE
```

**(c) The canonical index.** `height → the one hash we believe is on the
chain`. Unlike (a) and (b), **this is an opinion, written by the node, and it
can be wrong.** Keeping it right through reorgs is where the difficulty lives.

**(d) The heads.** Pointers saying how far we have got. Rustock has *three*,
stored separately:

| pointer | key | meaning | who reads it |
|---|---|---|---|
| download head | `KEY_HEAD` | highest header downloaded | `our_head_number()` → decides where the next download starts |
| executed head | `KEY_EXEC_HEAD` | highest block executed, + its state root | the executor, `eth_syncing` |
| canonical top | top of `CF_NUMBERS` | highest canonical claim | lineage walks, reorg detection |

On top of those three *persisted* heads, the running process holds more
position state in memory:

```
  in-memory position state
  ├─ last_body_height          how far bodies have been requested
  ├─ current_state_root        the trie node execution continues from
  ├─ follow_buffer             blocks arrived but not yet executed
  └─ SyncState                 a 6-variant enum, 4 variants carrying data:
       ├─ FindingConnectionPoint { peer, peer_best, start, end }
       ├─ DownloadingSkeleton   { peer, peer_best, connection_point }
       ├─ DownloadingHeaders    { peer_best, skeleton, connection_point,
       │                          tracker, pending_next_skeleton }
       │    └─ tracker: PeerChunkTracker
       │         ├─ next_to_assign     next chunk to hand to a peer
       │         ├─ next_to_process    next chunk to consume, in order
       │         ├─ in_flight          per-peer outstanding chunk indices
       │         ├─ buffered           chunks that arrived out of order
       │         └─ waiting_since      per-peer wait clocks
       └─ DownloadingBodies     { peer_best, pending_headers, next_request,
                                  in_flight, id_index }
```

Counting generously, that is **about twelve mutable facts that all encode
"where am I"**, written from many different code paths.

**They are supposed to agree.** Nothing checks that they do.

## 4. How a sync round works today

Rustock uses *skeleton sync*, the same approach as most Ethereum clients.
Rather than requesting headers one by one, the node asks a peer for a sparse
"skeleton" — every 192nd header, say — and then fills the gaps in parallel
from many peers.

```
  STEP 1  Ask a peer: "skeleton from #9,258,222"

  STEP 2  Peer replies with 16 evenly spaced points:

     #9258048   #9258240   #9258432   …   #9260916
        │          │          │              │
        ●──────────●──────────●───── … ──────●
        chunk 1    chunk 2    chunk 3      chunk 15

  STEP 3  Hand each chunk to a different peer, in parallel:

     chunk 1 → peer A       chunk 4 → peer D
     chunk 2 → peer B       chunk 5 → peer A   (A finished chunk 1)
     chunk 3 → peer C       …

  STEP 4  Chunks arrive out of order. They must be PROCESSED in order,
          because each chunk's first header must link to the previous
          chunk's last header. Out-of-order arrivals wait in `buffered`.

            next_to_process = 3
                   │
            [1][2][3][ ][ ][6][7][ ][9]…
             ▲  ▲      ▲        ▲  ▲
             done      waiting  buffered (arrived early)

  STEP 5  When every chunk is processed, the headers are stored, the head
          advances, and the round ends. Then: bodies, execution, repeat.
```

This is a reasonable design and it is fast when it works. The problem is not
the algorithm. The problem is what happens to those twelve position facts
when a reorg lands in the middle of step 3.

## 5. The first five stalls

Each of these wedged the node on mainnet. Each was fixed correctly. None of
the fixes prevented the next one.

### Stall 1 — the cursor skipped unexecuted ranges (`c3c3888`)

A batch of blocks failed to execute. The node halted the batch, but
`last_body_height` — the in-memory cursor saying how far bodies had been
requested — kept its *downloaded* position rather than retreating to the
*executed* position.

```
  executed head: #456          last_body_height: #3841
         │                              │
  ───────●──────────────────────────────●──────────►
         │◄──── this range never executed ────►│

  On retry, the node queued bodies from #3841 and executed #3841
  on top of #456's state. Silently.
```

Two position facts — executed head and body cursor — disagreed, and the retry
path read the wrong one.

### Stall 2 — a hole in the canonical index (`62f905f`)

A node was stopped in the middle of a reorg. `ensure_canonical_lineage`
rewrote the canonical index by walking from the new tip down to the fork
point, issuing **one independent database write per height**. Interrupted
half-way:

```
  height  canonical pointer      state after interruption
  ┌─────┬──────────────────┐
  │ 970 │ new-fork hash    │  ← rewritten before the crash
  │ 969 │ new-fork hash    │  ← rewritten before the crash
  │ 968 │     (none)       │  ← ☠ the write never happened
  │ 967 │ OLD-fork hash    │  ← never got rewritten
  │ 966 │ OLD-fork hash    │
  └─────┴──────────────────┘
```

Nothing recorded that a rewrite was in progress, so nothing repaired it on
restart. Worse, the code that should have noticed reads the canonical pointer
at `exec_head + 1` and treats `None` as *"we are at the tip, nothing to do"* —
and a hole is indistinguishable from the tip by that test. The node retried
every 5 seconds forever while the chain moved 193 blocks ahead.

Three position facts disagreed; the detector read the one that could not tell
the difference.

### Stall 3 — a gap in follow mode (`a8c23f3`)

Near the tip the node switches to "follow mode": blocks arrive by
announcement and are buffered until they can be executed in order. The rule
is *execute the lowest buffered block if it builds on the executed head*.
When it did not:

```
  executed head          buffered blocks
       #500      ✗gap✗   #502  #503  #504  …
        ●─────── ? ───────●─────●─────●

  #502 does not build on #500. drain_follow_buffer returns.
  NOTHING fetches #501. Blocks pile up behind the hole forever.
```

Observed on mainnet twice: **twenty hours, 2,332 new tips logged, zero blocks
executed, not one error message.** The resync trigger compares the
*downloaded* head against peers — and the downloaded head was at the tip, so
the node looked perfectly caught up.

The only visible symptom was the *absence* of a log line.

### Stall 4 — an orphaned head invisible to the canonical index (`e078115`)

The chain split at #9,251,057. The node took the losing side. The network
moved **7,000 blocks** ahead on the sibling branch.

```
                     ┌────────┐
              ┌──────│ 051058a│  ← we executed this, and stayed here
              │      └────────┘
   #9251057   │
  ┌────────┐◄─┤
  │        │  │      ┌────────┐     ┌────────┐         7,000 blocks
  └────────┘  └──────│ 051058b│◄────│ 051059 │◄── … ──► and counting
                     └────────┘     └────────┘
                       ▲
                       └─ we had DOWNLOADED these headers,
                          but never adopted them
```

`reconcile_exec_head_with_canonical` exists precisely for this. It was blind,
because **both of its signals read the canonical index**:

```
  canonical_hash(N+1)  →  None       "we never adopted that fork"
  head_number > N+1    →  false      "the head tracks canonical too"
```

A branch the node did not follow never gets a canonical pointer — so *no
question asked of that index can reveal one*. That is not an edge case; it is
the definition of the situation.

The node asked peers for headers after a block they had abandoned. Every peer
accepted the request and never answered. Each was sidelined in turn, the
request timed out, sync reset, repeat. **Three days.**

The fix was to consult the *height* index instead, which names every block at
a height, canonical or not.

### Stall 5 — the canonical pointer named a block we never downloaded (PR #50, today)

```
  what we HOLD                         what the network has

   #9258221                             #9258221
  ┌────────┐                           ┌────────┐
  │0xe62f… │                           │0xe62f… │
  └────────┘                           └────────┘
       ▲                                    ▲
       │      #9258222                      │     #9258222
       └─────┌────────┐                     └────┌────────┐
             │0x1476… │ ← our canonical         │0x37cd… │ ← the real one
             └────────┘                          └────────┘
                                                      ▲
   #9258223                                           │
  ┌────────┐  parent = 0x37cd…  ───────────────────────┘
  │0xc6a2… │  ← we downloaded this…
  └────────┘     …but not its parent
```

The reorg path *noticed* and called `ensure_canonical_lineage(0x37cd…)` to
repoint the index at the real block. That function walks down from the hash
it is given and stops at the first header the store does not hold:

```rust
let header = match self.header(hash)? {
    Some(h) => h,
    None => break,      // ← writes nothing, returns Ok(())
};
```

We do not hold `0x37cd…`. So in **exactly the case that needs repairing**, it
wrote nothing and reported success. The canonical pointer kept naming the
orphan; `our_head_number()` reads that pointer; every round asked peers for
headers after a block their chain had abandoned.

Fifteen minutes, 16 peers, all answering in one second, storing the same 18
headers over and over:

```
  Requesting skeleton from #9258222
  Received skeleton with 16 points (#9258048 -> #9260917)
  Header #9258223: parent 0x37cd50fe… not held; storing without verification
  Stored 18 headers (#9258223 -> #9258240)
  Skeleton round complete (head #9258222 …), requesting next skeleton
  ↻ repeat every 2 seconds, forever
```

## 6. The one sentence

Read the five together:

| # | Fact A | Fact B | Which was read |
|---|---|---|---|
| 1 | executed head | body cursor | body cursor |
| 2 | canonical index | reality (mid-rewrite) | canonical index |
| 3 | executed head | downloaded head | downloaded head |
| 4 | canonical index | height index | canonical index |
| 5 | canonical pointer | what we actually hold | canonical pointer |

> **Every stall is: two of the node's notions of "where am I" disagreed, and
> some code path read the one that was wrong.**

Not one was a wrong algorithm. Not one was a misunderstanding of the protocol.
Every single one was a **consistency failure between redundant copies of the
same fact.**

## 7. Why fixing them one at a time cannot converge

Let *n* be the number of stored position facts. The property we actually need
is a relation over **all of them at once**. The number of ways any two can
disagree grows like *n²*, and the number of code paths that write one without
updating the others grows with the size of the codebase.

```
   Each fix adds ONE check at the ONE site where a disagreement was noticed.

        stall 1        stall 2        stall 3        stall 4      stall 5
           │              │              │              │            │
    ┌──────▼──────┐┌──────▼──────┐┌──────▼──────┐┌──────▼─────┐┌─────▼──────┐
    │ check here  ││ check here  ││ check here  ││ check here ││ check here │
    └─────────────┘└─────────────┘└─────────────┘└────────────┘└────────────┘

   …but the surface is the whole n² grid of possible disagreements,
   and each fix covers one cell of it.
```

Two observations make this concrete and, I think, decisive.

**Stall 5 had a passing test named after it.**
`test_reconcile_when_the_forks_parent_was_never_downloaded` was written for
stall 4, reproduces stall 5's exact topology, and **passed**. It asserted that
the *executed head* retreats — which it did. It never asserted the download
head or the canonical pointer, which are the two the sync side actually
reads.

Three pointers. The test checked one. The bug was in the other two.

**Stall 5's repair was a silent no-op.** The code already tried to fix
itself, and the fix was a function that returns `Ok(())` without doing
anything precisely when it is needed.

Both are the same failure at different levels: **a claim that nothing
checked**. "The lineage is repaired." "This bug is covered." Neither was
verified; both were false.

A design that stores the same fact twice will have bugs where the two copies
disagree, at a rate proportional to how many times it is stored and how many
places write it. That is the thing to change.

---

# Part II — The design

## 8. What we are trying to achieve

Three properties, stated so they can be checked rather than hoped for.

> **P1 — Coherence.** The node's picture of the chain is never internally
> contradictory. If it claims block *X* is canonical at height *h*, then it
> holds *X*, and *X*'s parent is what it claims is canonical at *h−1*.

> **P2 — Safety.** The node never claims a chain the network does not have.
> Every canonical claim was produced by a verified walk over blocks we hold.

> **P3 — Liveness.** If peers are reachable and the network has a chain ahead
> of us, the node's position advances. It never repeats a cycle of work that
> leaves it where it started.

Every stall in Part I is a violation of P1 or P3 (never P2 — rustock has
never computed a wrong state root; it has only ever got *stuck*). The
existing code has no way to state any of the three, let alone check them.

The design that follows is six principles. Each is independently useful, and
they are ordered so that the earliest are the cheapest to adopt.

## 9. Principle 1 — one source of truth

Recall §3: headers keyed by hash are **content-addressed and append-only**.
That data has a property nothing else in the system has:

> It cannot become internally inconsistent, because there is nothing to be
> inconsistent *with*. A header stored under hash `h` either is or is not
> present, and if present, `hash(content) == h` verifies it.

So: **let that be the only truth, and derive everything else from it.**

```
  ══════════════ TODAY ══════════════        ═══════════ PROPOSED ═══════════

  ┌───────────────────────────────┐          ┌───────────────────────────────┐
  │ headers by hash  (truth)      │          │ headers by hash  (truth)      │
  │ height index     (truth)      │          │ height index     (truth)      │
  └───────────────────────────────┘          └───────────────────────────────┘
  ┌───────────────────────────────┐                        │
  │ canonical index  (opinion) ◄──┼── written              │ derived, on demand
  │ KEY_HEAD         (opinion) ◄──┼── from                 ▼
  │ KEY_EXEC_HEAD    (opinion) ◄──┼── many     ┌───────────────────────────────┐
  │ last_body_height (opinion) ◄──┼── places   │  Cursor { validated_head,     │
  │ SyncState+tracker(opinion) ◄──┼──          │           executed,           │
  └───────────────────────────────┘            │           state_root }        │
         ▲         ▲        ▲                  └───────────────────────────────┘
         └─────────┴────────┴─ can disagree                  │
                                                  canonical index = a CACHE of
                                                  the walk that produced it
```

The canonical index does not disappear — walking the chain on every query
would be too slow. It changes *status*: from an independent opinion to a
**cache of a computation**, governed by one rule.

> **The cache rule.** The canonical index may only be written by the function
> that has just verified the lineage it records, in the same atomic batch.

A cache that can only be filled by the computation it caches cannot disagree
with that computation. Stall 2 (the half-written rewrite) is excluded by
"same atomic batch". Stall 5 (a pointer to a block we do not hold) is excluded
by "has just verified" — you cannot verify a walk through a block you do not
have.

`KEY_HEAD` is **deleted outright**. `our_head_number()` becomes
`cursor.validated_head.number`, which by construction names a block we hold,
because the cursor is produced by a walk that held every block it passed.

## 10. Principle 2 — position is a derived value, not stored state

```rust
/// The node's entire position. Computed from the store; never stored.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Cursor {
    /// Highest block whose header we hold AND whose lineage back to the
    /// floor is unbroken. This is "how far the chain we believe in reaches".
    validated_head: BlockRef,

    /// Highest block we have executed. Invariant: an ancestor of
    /// validated_head (or equal to it).
    executed: BlockRef,

    /// The post-execution state root at `executed`.
    state_root: B256,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct BlockRef { number: u64, hash: B256 }
```

`BlockRef` carries **both** the number and the hash, always. Most of Part I's
bugs involved a height without a hash (which sibling?) or a hash without a
height (how far along?). Carrying one without the other is the representation
that permits the confusion.

Deriving the cursor is a walk, so it must be bounded and cached — see the
caveats in §23. The point is not that it is recomputed on every access; it is
that **there is no second place where it could be wrong**.

> **What was actually built (2026-09-23), and how it differs.**
>
> `KEY_HEAD` was **not** deleted. All three keys are still stored, and
> `BlockStore::cursor()` is a read-through over them rather than a derived
> value. This principle is therefore only half implemented, and the document
> should not be read as saying otherwise.
>
> The reason is that the atomicity in §11 turned out to do the work this
> principle was reaching for. Once every write of position goes through one
> `Transition` applied as one `WriteBatch`, "a second place where it could be
> wrong" stops being reachable — not because the second place was removed, but
> because nothing can write to it alone. Deriving the cursor instead would have
> meant a bounded, cached walk whose cache invalidation is, by §23's own
> admission, *"the single most dangerous detail in the proposal"*.
>
> Trading a dangerous cache for an atomic batch was the better deal. But it
> means the cursor is a *view*, not a derivation, and if a future change
> reintroduces a path that writes one key alone, this principle will not stop
> it. The coherence check is what would catch that.

## 11. Principle 3 — make the illegal state unrepresentable

Where a rule can be enforced by the type system, enforce it there, so that
violating it is a compile error rather than a runtime surprise.

The sketch this section originally carried is kept below, because the way it
was **wrong** is the most instructive thing in this document:

```rust
// ORIGINAL SKETCH — do not build this.
pub fn advance_head(store: &Store, head: Validated, batch: &mut WriteBatch);
pub fn write_canonical(store: &Store, head: Validated, batch: &mut WriteBatch);
```

Two functions. Both demand the proof, so neither can write an unverified head —
and **both can still be called without the other.** That is stall 6 exactly:
`ensure_canonical_lineage` wrote the canonical entries and nothing wrote the
head. A design whose whole purpose was to stop one key moving without the
others reproduced the defect in its own API sketch, and nobody noticed until
the simulator did.

What shipped merges them, so the question cannot arise:

```rust
/// A BlockRef proved to be held and lineage-linked. No `new`, no `From`,
/// private fields. It also carries the canonical entries adopting it needs,
/// so indexing and adopting cannot come apart.
pub struct Validated { tip: BlockRef, lineage: Vec<(u64, B256)> }

/// The sole constructor.
impl Validated {
    pub fn prove(store: &BlockStore, hash: B256) -> Result<Self, LineageBreak>;
}

/// The only way to change position. Each variant names every key that must
/// move, and `apply` writes them in ONE WriteBatch.
pub enum Transition {
    Adopt    { head: Validated },
    Retreat  { to: Validated },
    Executed { at: Validated, state_root: B256 },
}

impl BlockStore {
    pub fn cursor(&self) -> Result<Option<Cursor>>;
    pub fn apply(&self, t: &Transition) -> Result<Cursor>;
}
```

`crates/storage/src/position.rs`. `ensure_canonical_lineage` — the function
that wrote `CF_NUMBERS` alone — is now crate-private, so the sketch above is
not merely discouraged, it is unreachable from outside the storage crate.

There is no `Validated::new`. There is no `From<BlockRef> for Validated`.
The struct field is private. **Therefore it is not possible to write a head
or a canonical entry that was not just verified** — not because of a
convention or a review, but because the code does not compile.

```
      TODAY                                PROPOSED

  set_head(some_hash)                  let v = verify_lineage(store, tip)?;
     │                                 advance_head(store, v, &mut batch);
     └─ any B256 will do,                 │
        verified or not                   └─ won't compile without the proof
```

The `LineageBreak` error is itself useful — it names *where* the chain is
broken, which is the diagnostic the node has never had:

```rust
pub enum LineageBreak {
    MissingHeader  { at: u64, hash: B256 },        // stall 5
    ParentMismatch { at: u64, holds: B256, parent_of_child: B256 },  // stall 2
    BelowFloor     { hash: B256, floor: u64 },     // pruned history
    Storage(String),
}
```

(`NoCanonicalEntry` from the original sketch turned out to belong to the
coherence check, not to lineage proof: proving a lineage is about what we
*hold*, and the canonical index is what we *claim*.)

Compare with today, where the equivalent situation produces `Ok(())`.

## 12. Principle 4 — one invariant, checked continuously

Some relations cannot be encoded in types — "executed is an ancestor of
validated_head" is a fact about data, not about shapes. So state them **once**,
in **one** function, and check it.

```rust
/// The complete coherence condition (P1 and P3's precondition).
/// Cheap form: check only the heights this batch touched.
/// Full form (behind a flag, for tests and startup): sweep floor..head.
fn invariant(store: &Store, scope: Scope) -> Result<(), Violation> {
    // I1  Every height in (floor, head] has a canonical entry …
    // I2  … and we hold the header it names.
    // I3  For every height h in (floor, head]:
    //         header(canonical(h)).parent_hash == canonical(h-1)
    // I4  executed.number <= validated_head.number
    // I5  executed is an ancestor of validated_head
    // I6  No canonical entry exists above validated_head
    // I7  The state root recorded at `executed` is present in the trie store
    // I8  validated_head IS the canonical entry at its own height
}
```

> **I8 was added on 2026-09-23, by the node.** The list above originally
> stopped at I7, and asked of the *executed* head (I5) a question it never
> asked of the head itself. On mainnet, `KEY_HEAD` named a block at #9,262,401
> that was not canonical at that height while the canonical index below **and**
> above it was perfectly consistent — so no listed relation broke, and the
> node sawtoothed between the tip and three minutes behind it.
>
> Seven notions of position, and the invariant covered six of them. This is
> worth more than the fix: a list of relations written by hand is exactly as
> complete as its author's imagination, which is the failure mode this whole
> document is about.

Run it **after every commit**, not only in tests. A violation means the
node's picture of the chain is incoherent, and the entire lesson of Part I is
that continuing from an incoherent picture is how you wedge for three days.
It should refuse to proceed and say exactly which relation broke and where.

Here is the payoff, and it is the strongest single argument in this document:

| Stall | Violates | Detected today by |
|---|---|---|
| 1 — cursor skipped range | I4, I5 | nothing |
| 2 — canonical hole | I1, I3 | nothing |
| 3 — follow-mode gap | I4, I5 | nothing |
| 4 — orphaned head | I5 | nothing |
| 5 — pointer to absent block | I2, I3 | nothing |
| 6 — canonical stranded above head | I6 | nothing |
| 7 — head not canonical | I8 | nothing (I8 did not exist) |

**Every stall violates a relation in this list at the moment it begins. Not
one is detectable by any assertion that exists in the code today.** One
function, checked after every commit, turns all five from silent multi-hour
wedges into immediate, located, reproducible failures.

## 13. Principle 5 — a pure step function, not a state machine

Replace the six-variant `SyncState` — and the tracker inside it — with:

```rust
/// Everything we know from the network right now. No history, no position.
struct Inbox {
    peers:   Vec<PeerView>,      // who is connected, and their claimed tip
    headers: Vec<Header>,        // arrived since the last step
    bodies:  Vec<Body>,
    now:     Instant,
}

/// A complete, atomic change. Applying it is ONE WriteBatch plus some sends.
struct Transition {
    append:     Vec<Header>,        // headers to store
    set_head:   Option<Validated>,  // advance … (proof required)
    retreat_to: Option<BlockRef>,   // … or go backwards, explicitly
    request:    Vec<Request>,       // what to ask the network next
}

/// The whole of sync.
fn step(cursor: &Cursor, inbox: &Inbox) -> Transition;
```

Four consequences fall straight out of the shape:

**(a) No stale embedded position.** `step` receives the cursor as an
argument, freshly derived from the store. There is no `skeleton`, no
`connection_point` and no `tracker` living across calls to go stale when a
reorg changes the store underneath.

This is exactly what failed in stall 5. The instrumentation added in PR #50
caught it in its first printed line:

```
  chunk 3/16 (15 assigned, 4 in flight across 4 peers, 9 buffered) …
  chunk 4/16 …
  chunk 3/16 …
     ▲
     └─ next_to_process advancing, then going BACKWARDS —
        a state machine making "progress" against a skeleton
        that no longer described reality
```

**(b) Restart-equivalence is free.** Behaviour depends only on
`(store, inbox)`. Killing the node mid-round and restarting is *by
construction* identical to not killing it. Today, a restart loses
`last_body_height`, `follow_buffer` and the entire tracker — and that
asymmetry between "running" and "just restarted" has produced bugs of its
own. Note that in stall 3, **a restart cleared the wedge** — which is the
signature of exactly this asymmetry.

**(c) Retreat is a first-class outcome.** `retreat_to` sits in the
`Transition` type as a peer of `set_head`. Going backwards is a normal thing
for a syncing node to do, and it should not be an error path bolted onto
reorg handling. PR #50 had to add retreat by hand at one site; here it is
part of the vocabulary.

**(d) It is testable as a pure function.** `step` needs no network, no
threads, and no clock beyond `now`. Every scenario in §19 becomes a table of
inputs and an expected `Transition`.

```
        TODAY                              PROPOSED

   ┌──────────────┐                   cursor ──┐
   │  SyncState   │◄──mutated from            ├──► step() ──► Transition
   │  + tracker   │   12 places       inbox ───┘                  │
   └──────┬───────┘                                               │
          │ read by                                    one WriteBatch,
          ▼                                            all-or-nothing
   the next decision  ← may be stale,
                        nothing recomputes it
```

## 14. Principle 6 — liveness as a measure, with a watchdog

This is the part that would have caught **all five** stalls, and it is the
cheapest thing in the document.

Define a progress measure, ordered lexicographically:

```
  Φ  =  ( peer_best − validated_head.number ,   // how far behind we are
          outstanding_requests ,                 // work in flight
          −age_of_oldest_request )               // and how stale it is
```

Impose an obligation on `step`:

> **Progress obligation.** Every transition must either strictly decrease Φ,
> or be a no-op, or be an explicit `retreat_to`. A transition that commits
> changes and leaves Φ unchanged is a bug.

Now the watchdog. Look again at what stall 5 printed, every two seconds, for
fifteen minutes:

```
  Skeleton round complete (head #9258222, peer #9260913), requesting next skeleton
                                 ▲
                                 └── never changed. Not once. For 450 rounds.
```

**"Round complete" while Φ is unchanged is precisely the bug signature** —
and the node logged it 450 times without treating it as anything at all.

So: track Φ. If Φ fails to reach a strictly better value within a bounded
window, the node is not syncing, whatever it believes about itself. Escalate:

> **Corrected 2026-09-22, after stall 6.** This said *"if k consecutive rounds
> leave Φ **unchanged**"*, which contradicts the progress obligation three
> paragraphs above — that obligation is *strict decrease*. Stall 6 fell exactly
> into the gap between the two: the executed head was frozen while `peer_best`
> kept rising, so Φ's first component **grew** (354, 355, … 364) and a test for
> "unchanged" would have stayed quiet through all 106 rounds of it. Growing is
> not progress. The implemented test is *failed to improve*, and
> `a_growing_gap_escalates_even_though_phi_is_never_unchanged` pins it.

```
   k rounds, Φ unchanged
        │
        ├─ 1st escalation:  retreat one block, re-verify lineage
        ├─ 2nd escalation:  re-run the connection-point search from scratch
        ├─ 3rd escalation:  widen the retreat (exponential, capped)
        └─ throughout:      WARN with Φ's components, so it is never silent
```

This is a **generic liveness net under the whole subsystem**, not a fix for
one topology. It does not need to know why the node is stuck. It only needs
to know that a full cycle of work produced no progress — which is observable
without understanding the cause.

| Stall | Duration | Would the watchdog have fired? |
|---|---|---|
| 1 | — | Yes — Φ's first component unchanged across retries |
| 2 | until restart | Yes — 5-second retry loop, Φ unchanged, 193 blocks behind |
| 3 | 20 hours | Yes — 2,332 tips arrived, executed head never moved |
| 4 | 3 days | Yes — sync reset in a loop, Φ unchanged |
| 5 | 15 minutes | Yes — 450 rounds, Φ unchanged |
| 6 | 3 hours | Yes — 106 rounds, Φ *growing* (see the correction in §14) |

## 15. The whole design, on one page

```
  ┌─────────────────────────────────────────────────────────────────────────┐
  │  STORE (the only truth)                                                 │
  │    headers by hash        content-addressed, append-only, immutable     │
  │    height index           what we hold, canonical or not                │
  │    canonical index        CACHE — writable only by verify_lineage       │
  └───────────────────────────────┬─────────────────────────────────────────┘
                                  │  derive (bounded walk, cached)
                                  ▼
  ┌─────────────────────────────────────────────────────────────────────────┐
  │  Cursor { validated_head: BlockRef, executed: BlockRef, state_root }    │
  └───────────────────────────────┬─────────────────────────────────────────┘
                                  │
             Inbox ───────────────┤
        (peers, headers,          ▼
         bodies, now)      ┌─────────────┐
                           │   step()    │   pure; no hidden state
                           └──────┬──────┘
                                  │
                                  ▼
  ┌─────────────────────────────────────────────────────────────────────────┐
  │  Transition { append, set_head: Option<Validated>, retreat_to, request } │
  └───────────────────────────────┬─────────────────────────────────────────┘
                                  │  apply: ONE WriteBatch, all-or-nothing
                                  ▼
                        ┌───────────────────┐
                        │ invariant(store)  │  ← after EVERY commit
                        └─────────┬─────────┘
                                  │ ok
                                  ▼
                        ┌───────────────────┐
                        │  Φ watchdog       │  ← did this cycle progress?
                        └───────────────────┘
                                  │ no, k times
                                  ▼
                            escalate / retreat / shout
```

Read the diagram against the first five stalls:

- **Stall 2** cannot happen: one `WriteBatch`, all-or-nothing.
- **Stall 5** cannot happen: no `KEY_HEAD`; canonical entries need `Validated`.
- **Stalls 1, 3, 4** are caught within seconds by `invariant()` or the Φ
  watchdog, even if some new mistake introduces them.

---

# Part III — Does it actually close the class?

A design is only worth the migration cost if it *provably* excludes the bugs
that motivated it. So let us replay all five against it, one at a time, and
be specific about which mechanism does the work.

## 16. Replaying the first five stalls

### Stall 1 — cursor skipped unexecuted ranges

*Old mechanism:* `last_body_height` (in-memory) kept its downloaded position
while the executed head retreated. The retry read the body cursor.

*Under the new design:* `last_body_height` does not exist. The body cursor is
`cursor.executed.number`, derived from the store. There is no second place for
it to be wrong.

**Excluded by:** Principle 2 (derived position). Additionally caught by I4/I5
if reintroduced.

```
   OLD:  executed=#456   last_body_height=#3841   ← two facts, disagreeing
   NEW:  cursor.executed=#456                     ← one fact
```

### Stall 2 — a hole in the canonical index

*Old mechanism:* per-height `put_cf` calls, interrupted half-way; nothing
recorded that a rewrite was in progress; the detector could not distinguish a
hole from the tip.

*Under the new design:* the canonical rewrite is part of a `Transition`,
applied as one `WriteBatch`. An interrupted reorg **did not happen** — the
database either has the whole new lineage or the whole old one.

**Excluded by:** Principle 5 (atomic transition). Additionally, I1 and I3
would detect any hole on the next commit, and the `LineageBreak::NoCanonicalEntry`
error names the exact height rather than masquerading as "we're at the tip".

```
   OLD:  put(970) put(969) ✗crash✗ put(968) put(967)   ← 2 of 4 applied
   NEW:  batch{970,969,968,967}.commit()               ← 4 or 0
```

### Stall 3 — a gap in follow mode

*Old mechanism:* the lowest buffered block did not build on the executed head;
`drain_follow_buffer` returned; nothing fetched the missing block; the resync
trigger compared the *downloaded* head (which was at the tip) against peers.

*Under the new design:* `follow_buffer` is part of `Inbox`, not persistent
state, and `step` is called with the cursor derived from the store. A buffered
block that does not extend `cursor.executed` produces a `request` for the gap
— because `step` is a total function that must return *something*, and
returning an empty `Transition` while Φ's first component is non-zero is a
violation of the progress obligation.

**Excluded by:** Principle 6 (progress obligation). The 20-hour silent version
is impossible: 2,332 tips arriving with `cursor.executed` never moving is Φ
unchanged across thousands of cycles, which the watchdog escalates on.

```
   OLD:  "downloaded head is at the tip" → looks caught up → no resync
   NEW:  Φ = (peer_best − validated_head, …) — but the EXECUTED head is
         what the measure is anchored to, so a stalled executor is visible
```

> **Design note.** Φ must be anchored to the *executed* head, not the
> downloaded head. Stall 3 is precisely the failure of measuring the wrong
> frontier. This is the kind of detail worth arguing about before building.

### Stall 4 — an orphaned head invisible to the canonical index

*Old mechanism:* both detection signals read the canonical index, and a branch
the node never adopted has no canonical entry — so the index structurally
cannot reveal it.

*Under the new design:* `verify_lineage` walks from the *height index*, which
names every block at a height regardless of canonical status. The question
"does anything stored above me build on me?" is answerable from truth rather
than from opinion.

**Excluded by:** Principle 1 (the height index is truth; the canonical index
is a cache). Additionally caught by I5 — `executed` ceases to be an ancestor
of the best available lineage — and by the watchdog after *k* rounds, rather
than after three days.

### Stall 5 — canonical pointer named a block we never downloaded

*Old mechanism:* `ensure_canonical_lineage` broke at the first absent header
and returned `Ok(())`; `KEY_HEAD` kept naming the orphan; `our_head_number()`
read `KEY_HEAD`.

*Under the new design:* three independent exclusions, which is a good sign.

1. `KEY_HEAD` does not exist. `our_head_number()` reads
   `cursor.validated_head`, which is produced by a walk over blocks we hold.
2. Writing a canonical entry requires a `Validated`, and `verify_lineage`
   cannot produce one through a missing header — it returns
   `LineageBreak::MissingHeader { at: 9_258_222, expected: 0x37cd… }`.
3. I2 fails on the next commit if it somehow happened anyway.

**Excluded by:** Principles 1, 2 and 3, redundantly.

### Summary

| Stall | Excluded by construction | Also caught by |
|---|---|---|
| 1 | P2 derived position | I4, I5 |
| 2 | P5 atomic transition | I1, I3 |
| 3 | P6 progress obligation | watchdog |
| 4 | P1 truth vs cache | I5, watchdog |
| 5 | P1, P2, P3 | I2, watchdog |

Three of five become *unrepresentable*. The other two become *detected within
seconds* rather than hours or days. And crucially, a **sixth** stall of a
shape nobody has thought of yet still trips either `invariant()` or the
watchdog, because both are stated over the general property, not over the
specific topologies of stalls 1–5.

That last sentence is the whole point of the redesign.

## 17. What "prove" means here

Three obligations of increasing strength. Being precise about which is which
matters, because overclaiming here would be its own kind of bug.

### (a) Invariant preservation — *checked, continuously*

`invariant()` holds after every committed `Transition`. This is not a proof
in the mathematical sense; it is a **runtime-enforced postcondition**. But it
is enforced on every commit in production, not merely in tests, which makes
it strictly stronger than any test suite for the property it covers.

### (b) Safety (P2) — *a one-site argument*

Because only one function writes canonical entries, and it requires a
`Validated` it can only obtain by walking blocks we hold, the argument
"every canonical claim was verified" reduces to inspecting a single function.

This is a genuine proof, in the informal sense engineers use the word: a
short, checkable argument that does not depend on the behaviour of the rest
of the system. It is short *because* of the design — under the current code
the same argument would have to consider every call site of
`put_canonical_hash`.

> **Held, with one correction.** `BlockStore::apply` is that single function,
> and `ensure_canonical_lineage` is crate-private. But the one-site argument
> was made *before* the simulator ran, and it was incomplete in a way reading
> could not reveal: the argument covered what gets written into the canonical
> index, and said nothing about the executed head being dragged off the chain
> by a reorg that index performed. See Appendix C.
>
> A one-site argument is only as good as the set of properties it is made
> about. This one now includes execution because a machine noticed it did not.

### (c) Liveness (P3) — *exhaustive simulation*

> **NOT ESTABLISHED.** This is the largest gap between this document and what
> exists, and it should not be read as done.
>
> The simulator that shipped (§18) asserts **coherence**, not liveness. It has
> no model of peers, so it cannot ask "does the node converge on the true
> chain?" — only "is the node's picture of the chain self-consistent after
> every write?". Those are different questions, and only the second is
> answered.
>
> Liveness is what stage 6 buys, because asking it requires a `step()` the
> simulator can drive against simulated peers. Until then, liveness rests on
> the Φ watchdog (§14) — a *detector*, not a guarantee.

## 18. The simulator, in detail

The first five stalls are all **scheduling phenomena**: a reorg arriving while a
chunk is in flight; a header whose parent is replaced between request and
delivery; a crash between two writes. Unit tests cannot find these reliably,
because the author must first imagine the interleaving.

So build a machine that imagines them for us.

```
  ┌────────────────────────────────────────────────────────────────────┐
  │  MODEL CHAIN                                                       │
  │    grows on a seeded schedule; reorgs of seeded depth and timing;  │
  │    knows the "true" canonical chain at all times                   │
  └───────────────────────────┬────────────────────────────────────────┘
                              │ serves
  ┌───────────────────────────▼────────────────────────────────────────┐
  │  SIMULATED PEERS  (each with its own seeded misbehaviour)          │
  │    • delay responses by a random number of ticks                   │
  │    • drop responses entirely                                       │
  │    • reorder responses                                             │
  │    • disconnect and reconnect                                      │
  │    • serve a STALE view of the chain (lagging peers)               │
  │    • serve a DIFFERENT fork (honest disagreement)                  │
  │    • lie: headers that do not link, wrong heights, bad hashes      │
  └───────────────────────────┬────────────────────────────────────────┘
                              │ Inbox
  ┌───────────────────────────▼────────────────────────────────────────┐
  │  NODE UNDER TEST                                                   │
  │    cursor = derive(store)                                          │
  │    transition = step(cursor, inbox)      ← pure, deterministic     │
  │    apply(transition)                     ← one batch               │
  │    assert!(invariant(store).is_ok())     ← AFTER EVERY STEP        │
  └───────────────────────────┬────────────────────────────────────────┘
                              │
                              ▼
        after N ticks:  assert_eq!(cursor.validated_head, model.head)
```

No threads. No wall clock. No sockets. The whole simulation is a loop over
ticks, and a seed determines everything — so a failure is a **seed**, and
replaying that seed reproduces it exactly, every time.

**What we assert:**

| Property | Assertion | When |
|---|---|---|
| Coherence (P1) | `invariant(store).is_ok()` | after every step |
| Safety (P2) | every canonical entry matches the model's chain | after every step |
| Liveness (P3) | `validated_head == model.head` | within a bounded number of ticks after the model stops moving |
| No silent loops | Φ strictly decreases at least once per *k* ticks | throughout |

**Shrinking.** When a seed fails, bisect it automatically: halve the number
of injected faults, re-run, keep halving while it still fails. A three-day
mainnet wedge becomes a twelve-line reproduction.

**Scale.** Each tick is a few microseconds — no I/O, no threads. A million
seeded schedules is minutes of CPU, i.e. affordable in CI on every commit.

**The corpus.** Alongside random schedules, replay a fixed set of *recorded
real reorgs*, including:

- #9,258,222 — stall 5's topology (canonical parent never downloaded)
- #9,251,057 — stall 4's three-day split
- the #9,233,966 interrupted-rewrite database from stall 2
- the follow-mode gap from stall 3

These become permanent regression tests expressed in the same framework, so
"does the new design handle the old bugs?" is answered by running the suite
rather than by reasoning.

### 18a. What was actually built, 2026-09-23

The design above is the simulator worth having. What shipped is **smaller**, and
the difference matters enough to state plainly before anyone relies on it.

`crates/storage/tests/position_simulator.rs` — about 420 lines.

```
  ┌─────────────────────────────────────────────────────────────────┐
  │  WORLD            every block that exists anywhere              │
  │                   grows on a seeded schedule; forks at seeded   │
  │                   depth; no notion of "true" canonical chain    │
  └────────────────────────────┬────────────────────────────────────┘
                               │  events, not messages
  ┌────────────────────────────▼────────────────────────────────────┐
  │  NODE             a real BlockStore on a tempdir                │
  │    Mine / Download / Adopt / Execute / Retreat / Restart        │
  │    each applied through the real Transition API                 │
  │    assert!(coherent(store))        ← AFTER EVERY EVENT          │
  └─────────────────────────────────────────────────────────────────┘
```

**What it does.** Seeded xorshift picks one of six events per step. Downloads
arrive in arbitrary order, including blocks the node will never adopt. Forks
are mined at random depth, so siblings pile up at every height. Adoption and
execution are attempted on blocks that may not be provable or canonical, and a
refusal is a legitimate outcome that must leave the store untouched. After
every event the **full** coherence sweep runs — every height from the floor to
the head, plus I4, I5, I6, I8 — and a failure prints the seed, the step, the
broken relation, and the last twelve events.

**Coverage as it stands:**

| Suite | Seeds | Steps | Events |
|---|---|---|---|
| `random_schedules_never_leave_the_store_incoherent` | 400 | 400 | 160,000 |
| `long_runs_stay_coherent` | 5 | 5,000 | 25,000 |
| `a_storm_of_reorgs_stays_coherent` | 60 | 600 | 36,000 |
| | | | **221,000** |

Roughly a minute of CPU. (An earlier note said ~245,000; the arithmetic above
is the correct figure.)

**What it does NOT do**, each of which §18 above promises:

- **No simulated peers.** No delayed, dropped, reordered, disconnecting,
  stale-view or lying peers. Events are applied directly to the store, so the
  *protocol* is not exercised at all — only the position layer underneath it.
- **No `step()` under test.** There is nothing to drive, because stage 6 has
  not been done. The node's actual sync loop is not in the simulation.
- **No liveness assertion.** There is no model of the true canonical chain, so
  `validated_head == model.head` is not checked and cannot be. See §17(c).
- **No safety-against-a-model assertion**, for the same reason.
- **No shrinking.** A failing seed is reproducible but not minimised; the
  twelve-event tail does the job by hand, which was enough for the two defects
  found but will not scale.
- **No run against the old implementation.** §21 says to validate the
  simulator by reproducing stalls 1–5 against the pre-redesign code. That was
  not done. Instead the nine replays in `stall_replays.rs` assert the shapes
  directly — weaker evidence that the simulator *works*, since it never
  demonstrated it can catch a bug it was not pointed at.

That last one deserves emphasis: the simulator **did** find two real defects it
was not pointed at (Appendix C), which is the evidence that matters most. But
it found them in the new code, not by reproducing old bugs, so the
self-validation §21 asked for has not happened.

**The corpus.** `crates/storage/tests/stall_replays.rs` — nine tests building
the recorded on-disk shape of each stall at its real heights, each asserting
the shape is *unreachable through the public API* rather than merely detected.
This is the part of §18's "corpus" that was delivered.

## 19. Why the current tests could not have done this

It is worth being clear about this, because the natural objection to the whole
document is *"why not just write more tests?"*

`step`'s behaviour today depends on in-memory state that tests do not
construct and cannot easily reach: `SyncState`'s embedded skeleton and
tracker, `follow_buffer`, `last_body_height`. A test can drive the coarse
entry points and assert on whichever pointer the author happened to think of.

That is not a hypothesis. It is what happened:

> `test_reconcile_when_the_forks_parent_was_never_downloaded` reproduces
> stall 5's topology exactly, and passed throughout the fifteen-minute
> mainnet stall, because it asserted the executed head and not the other two
> pointers.

Under the new design that test would assert `invariant(store)`, which covers
all three pointers and four relations between them, without the author having
to know which one this particular bug would corrupt.

**The difference is not test count. It is that the assertion is stated over
the general property rather than over the specific symptom.**

## 20. If you only do one thing

Ship the Φ watchdog (§14). It is roughly thirty lines. It requires no
redesign, no type changes, and no migration. It would have caught **all five**
stalls, turning a three-day wedge and a twenty-hour wedge into seconds.

Second-cheapest: `invariant()` (§12) run after every commit. Perhaps a
hundred lines. It converts silent incoherence into a loud, located error.

Both are independently valuable even if the rest of this document is
rejected outright.

---

# Part IV — Getting there

## 21. Migration, staged

This is **not a rewrite**. The store, the wire protocol, the header verifier
and the block executor are all unchanged. What changes is how *position* is
represented and advanced — perhaps 2,500 lines of the sync crate, and none of
the 665 execution tests.

The stages are ordered so the cheapest, highest-value work ships first and
protects the live node while the structural work proceeds.

```
  ┌────────────────────────────────────────────────────────────────────────┐
  │ STAGE 1   Φ watchdog                  DONE    ~30 lines    ~half a day │
  │           Detects any stall of this family. No design change.          │
  │           ▸ ships alone, immediately                                   │
  └────────────────────────────────────────────────────────────────────────┘
                                   │
  ┌────────────────────────────────▼───────────────────────────────────────┐
  │ STAGE 2   invariant() after every commit  DONE  ~150 lines   ~2 days   │
  │           Run it against the recorded reorg corpus first — EXPECT it   │
  │           to fire on the existing code. That is the point.             │
  │           ▸ ships alone; gate behind a flag defaulting to on           │
  └────────────────────────────────────────────────────────────────────────┘
                                   │
  ┌────────────────────────────────▼───────────────────────────────────────┐
  │ STAGE 3   Cursor (as a read-through)      DONE  ~300 lines   ~3 days   │
  │           Delete KEY_HEAD. our_head_number() reads the cursor.         │
  │           Canonical entries written only alongside the walk.           │
  │           ▸ invariant() from stage 2 guards this change                │
  └────────────────────────────────────────────────────────────────────────┘
                                   │
  ┌────────────────────────────────▼───────────────────────────────────────┐
  │ STAGE 4   Validated<BlockRef>             DONE  ~100 lines   ~1 day    │
  │           Mechanical once stage 3 lands: make the proof a parameter.   │
  │           Turns "wrote an unverified head" into a compile error.       │
  └────────────────────────────────────────────────────────────────────────┘
                                   │
  ┌────────────────────────────────▼───────────────────────────────────────┐
  │ STAGE 5   The simulator (partial, §18a)   DONE  ~420 lines   ~1 week   │
  │           Build it BEFORE stage 6, and run it against the OLD          │
  │           implementation — it should reproduce stalls 1–5 from seeds.  │
  │           That validates the simulator itself.                         │
  └────────────────────────────────────────────────────────────────────────┘
                                   │
  ┌────────────────────────────────▼───────────────────────────────────────┐
  │ STAGE 6   Extract step()                  TODO  ~1,200 lines ~2 weeks  │
  │           The largest piece. Move tracker state into store or Inbox.   │
  │           Run old and new side by side under the simulator as a        │
  │           differential oracle during the transition.                   │
  └────────────────────────────────────────────────────────────────────────┘
```

**Why this order.** Stages 1 and 2 are pure additions — they cannot break
anything, because they only observe. They make the live node safe while the
risky work happens. Stage 5 before stage 6 is deliberate: a simulator that
cannot reproduce the *known* bugs against the *old* code is not trustworthy
enough to certify the new code.

**Decision point after stage 2.** Once `invariant()` runs against the recorded
corpus, we will know how often the existing code actually violates coherence.
If the answer is "rarely, and only in the five known shapes", stages 3–6 are a
judgement call about long-term cost. If the answer is "constantly, in shapes
we have not seen", that settles it.

> **The decision point resolved itself, 2026-09-22.** `invariant()` was
> deployed at 20:29:46 and fired 1.5 ms later on a shape nobody had listed, on
> a node that had been silently wedged for three hours. Within twenty-four
> hours it had also produced I8 — an eighth relation, found because the node
> broke a rule the list did not contain.
>
> That answered the question in the second form: not "rarely, in known
> shapes", but "immediately, in shapes we had not seen". Stages 3, 4 and 5
> followed on 2026-09-23.
>
> **Stage 5 did not run against the old implementation**, which §21 asks for
> and §18a records as an outstanding gap in the evidence.

## 22. Effort and risk summary

| Stage | Lines | Time | Risk | Value if stopped here |
|---|---:|---:|---|---|
| 1 Φ watchdog | ~30 | ½ day | none — observes only | **Every stall in this document self-heals** — *shipped 2026-09-22, `crates/sync/src/watchdog.rs`* |
| 2 invariant() | ~150 | 2 days | none — observes only | Silent incoherence becomes loud and located |
| 3 Cursor | ~300 | 3 days | medium — changes write paths | Stalls 1, 3, 5 become unrepresentable |
| 4 Validated | ~100 | 1 day | low — mechanical | Unverified heads become a compile error |
| 5 Simulator | ~600 | 1 week | none — test-only | Liveness becomes testable at all |
| 6 step() | ~1,200 | 2 weeks | high — the real surgery | Restart-equivalence; no stale embedded state |

Stages 1+2 are **two and a half days for the bulk of the safety benefit**.
That is the recommendation regardless of what is decided about the rest.

## 23. Honest limits and open questions

I would rather name these than have them found in review.

**Deriving the cursor costs a walk.** It must be bounded — walk to the last
confirmed checkpoint, not to genesis — and cached. And the cache must be
invalidated by the *same batch* that writes blocks, or we have reintroduced
exactly the problem this document is about, one level up. This is the single
most dangerous detail in the proposal and deserves its own review.

**Coherence is not liveness.** The simulator answers "is the node's picture of
the chain self-consistent?" and does not answer "does the node get to the right
chain?". Nothing in what shipped establishes the second. See §17(c) and §18a.

**The simulator does not exercise the protocol.** There are no simulated peers
and no `step()`, so every scheduling phenomenon §18 was written to catch — a
reorg arriving while a chunk is in flight, a header whose parent is replaced
between request and delivery — is *not* covered. The position layer beneath
those phenomena is covered thoroughly; the phenomena themselves are not.

**The simulator was never validated against a known bug.** §21 asks that it
reproduce stalls 1–5 against the pre-redesign code before being trusted to
certify the new code. It did not. It found two genuine defects it was not
pointed at, which is good evidence but not the evidence that was asked for.

**Position is stored, not derived.** §10's principle is half implemented. A
future path that writes one key alone would not be prevented — only detected.

**A property test is not a proof.** §18 gives high confidence over an
enormous number of schedules; it does not give certainty. It is the strongest
tool available at reasonable cost, and it is a large improvement on the status
quo, but it should not be described as more than it is. If we wanted a real
proof we would be writing TLA+ — which is a legitimate option for `step`
alone, and perhaps worth it later, but not where I would start.

**The watchdog must not fire spuriously.** Two cases need care:
- the node is legitimately idle at the tip (Φ's first component is zero — do
  not escalate);
- a long body download where the header head does not move but bodies are
  arriving (that is why Φ is lexicographic, with outstanding requests as the
  second component).

The no-op case must be distinguishable from the stalled case by whether any
request is outstanding and whether responses are arriving.

**Retreat must be bounded.** A node that retreats one block per round in
response to a persistent disagreement walks backwards to genesis. Retreat
should be exponential and capped, and a retreat that does not restore progress
within a bound is a *different* failure that deserves its own alarm, not more
retreating.

**Φ anchored to the executed head** — see the design note in §16. Stall 3 is
exactly the failure of measuring the downloaded frontier instead. But
anchoring to execution means Φ does not move during a long pure-header phase,
so the second and third components have to carry the progress signal there.
This needs to be got right; it is the sort of detail that produces stall
number six.

> It did. Stall 6 arrived on 2026-09-22, before either stage was implemented,
> and through a different door than this paragraph anticipated — not a false
> positive during a header phase, but a false *negative* from the watchdog's
> wording. Both are now covered by tests:
> `a_working_header_round_banks_progress_through_outstanding_requests` for the
> case feared here, and `a_churning_round_does_not_bank_progress_forever` for
> its mirror image.

**What this does not address.** Peer selection and reputation, bandwidth
scheduling, and snap/state sync are all out of scope. This document is about
one thing: the node's own picture of where it is.

---

## Appendix A — the stalls, in one table

| # | Date | Duration | Fact A | Fact B | Read | Fix |
|---|---|---|---|---|---|---|
| 1 | — | — | executed head | `last_body_height` | B | `c3c3888` |
| 2 | — | until restart | canonical index | reality mid-rewrite | A | `62f905f` |
| 3 | — | 20 hours | executed head | downloaded head | B | `a8c23f3` |
| 4 | 2026-09-18 | 3 days | canonical index | height index | A | `e078115` |
| 5 | 2026-09-22 | 15 min | canonical pointer | what we hold | A | PR #50 |
| 6 | 2026-09-22 | 3 hours | canonical index | KEY_HEAD | A | PR #69 — not expressible |
| 7 | 2026-09-23 | sawtooth | KEY_HEAD | canonical index | A | PR #69 — not expressible |

## Appendix B — the invariant, in full

> **Implemented 2026-09-22** as `crates/sync/src/invariant.rs`, with the
> structural half following on 2026-09-23 as `crates/storage/src/position.rs`
> (see Appendix C), checked on every
> tick with `Scope::Delta` over the window between the executed head and the
> validated head. One test per relation, each named for the stall it would have
> caught. `derive_cursor` reads the three position keys directly rather than
> deriving a coherent cursor — that is stage 3, and the point of stage 2 is to
> check the representation the node actually has today.

```rust
/// Relations that must hold over the node's picture of the chain.
/// `floor` is the pruning floor (0 on an archive node).
fn invariant(store: &Store, scope: Scope) -> Result<(), Violation> {
    let cursor = derive_cursor(store)?;
    let floor = store.prune_floor()?.map_or(0, |f| f.number);

    for h in scope.heights(floor, cursor.validated_head.number) {
        // I1 — a canonical entry exists
        let c = store.canonical_hash(h)?
            .ok_or(Violation::NoCanonicalEntry { at: h })?;

        // I2 — and we hold the header it names
        let hdr = store.header(c)?
            .ok_or(Violation::CanonicalHeaderMissing { at: h, hash: c })?;

        // I3 — and it links to the canonical entry below
        if h > floor {
            let below = store.canonical_hash(h - 1)?
                .ok_or(Violation::NoCanonicalEntry { at: h - 1 })?;
            if hdr.parent_hash != below {
                return Err(Violation::ParentMismatch {
                    at: h, parent: hdr.parent_hash, canonical_below: below,
                });
            }
        }
    }

    // I4 — execution never runs ahead of validated headers
    if cursor.executed.number > cursor.validated_head.number {
        return Err(Violation::ExecutedAboveHead { .. });
    }

    // I5 — and is on the same chain
    if store.canonical_hash(cursor.executed.number)? != Some(cursor.executed.hash) {
        return Err(Violation::ExecutedOffChain { .. });
    }

    // I6 — nothing canonical above the head
    if store.canonical_hash(cursor.validated_head.number + 1)?.is_some() {
        return Err(Violation::CanonicalAboveHead { .. });
    }

    // I8 — the head is itself the canonical block at its height
    if store.canonical_hash(cursor.validated_head.number)?
        != Some(cursor.validated_head.hash) {
        return Err(Violation::HeadNotCanonical { .. });
    }

    // I7 — the state we would continue from actually exists
    if !store.trie_has(cursor.state_root) {
        return Err(Violation::StateRootMissing { .. });
    }

    Ok(())
}
```

`Scope::Delta` checks only the heights a batch touched — O(1) per commit, for
production. `Scope::Full` sweeps `floor..head` — for startup, tests and the
simulator.

## Appendix C — what the simulator found, 2026-09-23

Stages 3, 4 and 5 were implemented together. The simulator found two holes in
stages 3 and 4 **on its first run**, both within 34 steps, and neither was in
any hand-written test.

They are recorded here because they are the argument for stage 5 existing at
all. Both are in code that had just been written to be correct by construction,
reviewed, and believed.

### 1. `Adopt` reorged the chain and left execution on the old branch

```
  execute #1 on branch A
  adopt a tip on branch B that rewrites #1
  -> canonical(1) = B,  executed = A   ... I5 broken
```

This is stall 4's shape arriving straight through the new API. The type system
guaranteed the head and the canonical index moved together, and said nothing
about the third key.

**Fix:** `apply()` rolls the executed head back to the fork point in the same
`WriteBatch`. The target is the height just below the lowest one the transition
rewrites, clamped to the new head — conservative, and always a block common to
both branches.

### 2. `Validated` proves lineage, not canonicity

`Transition::Executed` accepted any provable block, and a fork block is
perfectly provable. Recording one as the executed head breaks I5 immediately,
and the node then executes forward from a branch it is not on.

**Fix:** `apply()` refuses to record execution of a block that is not the
canonical block at its height, and refuses one above the head.

### Why this matters more than the fixes

The claim in §12 was that the seven relations would be maintained *by
construction*. Stages 3 and 4 made six of them structural and left the seventh
reachable, and no amount of reading the code found it. A property test over
~245,000 adversarial events found it twice in under a second.

The honest conclusion: **"correct by construction" is a claim that needs the
same evidence as any other.** The previous two attempts at this subsystem were
asserted safe on exactly the reasoning that failed here.

---

## Appendix D — how this was tested, and what the evidence is worth

Four layers, each answering a different question, and each with a different
weight. They are listed weakest first, because the order in which they are
*convincing* is the reverse of the order in which they are usually cited.

### Layer 1 — unit tests (weakest)

`crates/storage/src/position.rs`, 14 tests. One per behaviour of `prove`,
`Adopt`, `Retreat`, `Executed` and `cursor`.

**Answers:** does each piece do what its author thought?
**Cannot answer:** anything its author did not think of. Both defects in
Appendix C passed every unit test in this file, because the author wrote both
the code and the tests from the same misunderstanding. This is the same
structural blindness the September audit response describes at length: *a test
authored alongside the implementation inherits the implementation's
misreadings.*

### Layer 2 — the recorded corpus

`crates/storage/tests/stall_replays.rs`, 9 tests. Each builds the on-disk shape
the live node was actually found in, at its real heights, and asserts the shape
is **unreachable** through the public API rather than merely detected.

**Answers:** would the old bugs still be possible?
**Cannot answer:** whether new ones are. A regression corpus is a record of
defeats, not a prediction.

The distinction between *unreachable* and *detected* is the point of these
tests. A shape that is merely detected is a shape a future caller can still
create.

### Layer 3 — the property simulator (strongest of the three offline layers)

`crates/storage/tests/position_simulator.rs`. 221,000 seeded adversarial events
(§18a), full coherence checked after **every** write, failures reproducible
from a printed seed.

**Answers:** across an enormous number of orderings nobody imagined, does the
node's picture of the chain stay self-consistent?
**Cannot answer:** liveness, protocol behaviour, or anything involving peers
(§18a's omissions; §17(c)).

This is the layer that earned its cost. It found two real defects within 34
steps of its first run, neither of which was in any hand-written test, both in
code that had been written to be correct by construction, reviewed, and
believed. Without it, this document's central claim would have shipped false
for the third time.

### Layer 4 — production (the only layer that settles anything)

Deployed to the mainnet node at 02:50 on 2026-09-23, replacing a build that
was visibly sawtoothing.

| | before (per hour) | after (first 30 min) |
|---|---|---|
| coherence violations | 13 | **0** |
| watchdog escalations | 6 | **0** |
| head re-verifications | 4 | **0** |
| ERROR lines | 0 | **0** |
| blocks executed | — | 60, at the tip |

Three tip reorgs occurred in that window — the events that previously wedged
the node for three minutes each. Each was handled in a single tick:

```
Orphan recovery: cannot adopt 0xe…c9 (no header stored for 0x1d…2e at #9263608);
                 retreating to #9263607 so the height is downloaded again
Orphan recovery: head and execution retreated to #9263607
```

Head and execution moving together, in one write.

**Answers:** does it work?
**Cannot answer:** does it keep working? Thirty minutes is thirty minutes.
Stall 4 took three days to appear and stall 3 ran for twenty hours without a
single error line. A quiet half-hour is consistent with a correct node and also
with a node that has not yet met the schedule that breaks it.

### What the four layers do not cover

Stated together, so it is not necessary to reassemble them from four sections:

1. **Liveness.** Nothing here shows the node converges on the correct chain,
   only that it stays self-consistent while trying. (§17c)
2. **The protocol.** No simulated peers, no `step()` — so reorg-during-chunk,
   parent-replaced-in-flight and every other scheduling phenomenon §18 was
   written for remain untested. (§18a)
3. **The simulator itself.** It was never run against the old code to prove it
   can catch a known bug. (§21)
4. **Duration.** See above.

Items 1 and 2 are what stage 6 buys. Item 3 is a day's work and should be done
before the simulator is cited as certification rather than as a bug-finder.
