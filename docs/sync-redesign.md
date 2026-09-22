# A sync design in which these stalls cannot happen

**Status:** proposal, for discussion.
**Written:** 2026-09-22, after the fifth stall of the same family.

---

## 1. What this document is for

Rustock's sync has now wedged on mainnet five times. Each time the fix was
correct, narrow, and did not prevent the next one. This document argues that
is not a coincidence or a run of bad luck: the *shape* of the current design
makes this class of bug arrive at a steady rate, and no amount of careful
fixing changes the rate.

It then proposes a design where the class is closed — not by being more
careful, but by making the failure structurally impossible and mechanically
checkable.

It is written to be read start to finish by someone who has not been staring
at this code. Sections 2–4 define the vocabulary and diagnose; 5–8 are the
proposal; 9 is what "prove" means here concretely; 10 is migration.

---

## 2. The vocabulary

Four terms, because the bugs live in the gaps between them.

**Header.** A block's metadata, including `number`, its own hash, and
`parent_hash`. Headers arrive before bodies.

**The height index** (`hashes_at_height`). Every hash we hold at a given
height, canonical or not. Forks mean a height can hold several.

**The canonical index** (`CF_NUMBERS`, `height → hash`). Which block at each
height the node believes is *on the chain*. This is a claim, not a fact: it is
written by the node and can be wrong.

**A head.** A claim about how far the node has got. Rustock has three of them,
stored separately:

| pointer | meaning | read by |
|---|---|---|
| `KEY_HEAD` | highest header downloaded | `our_head_number()`, which chooses where the next skeleton starts |
| `KEY_EXEC_HEAD` | highest block executed, plus its state root | execution, RPC `eth_syncing` |
| `CF_NUMBERS` top entry | highest canonical claim | lineage walks, reorg detection |

Plus in-memory: `last_body_height`, `current_state_root`, `follow_buffer`, the
`SyncState` enum (six variants, four carrying embedded data), and
`PeerChunkTracker` (`next_to_assign`, `next_to_process`, `buffered`,
`in_flight`, `waiting_since`).

That is roughly **twelve mutable pieces of "where am I"**, written from many
places.

---

## 3. The five stalls, and the one sentence they share

| # | Date | Symptom | Fix |
|---|---|---|---|
| 1 | — | Sync cursor skipped unexecuted ranges | `c3c3888` verify block lineage |
| 2 | — | Canonical index hole wedged execution permanently | `62f905f` |
| 3 | — | Follow mode stalled forever behind a small gap | `a8c23f3` re-fetch the gap |
| 4 | 2026-09-18 | Executed head orphaned; wedged 3 days | `e078115` detect via height index |
| 5 | 2026-09-22 | Canonical pointer named a block never downloaded; wedged 15 min | PR #50 |

Every one of them is the same sentence:

> **Two of the node's notions of "where am I" disagreed, and some code path
> read the one that was wrong.**

Not one was a wrong algorithm. Each was a *consistency failure between
redundant copies of the same fact*.

Two details make this vivid.

**Stall 5 had a test named after it that passed.**
`test_reconcile_when_the_forks_parent_was_never_downloaded` was written for
stall 4, reproduces stall 5's exact topology, and asserted the *exec head*
retreats — which it did. It never asserted `KEY_HEAD` or the canonical
pointer. Three pointers, the test checked one, the bug was in the other two.

**Stall 5's repair was a silent no-op.** The reorg path *did* call
`ensure_canonical_lineage(true_parent)`. That function walks down from the
hash it is given and `break`s at the first header the store lacks — so in
precisely the case needing repair, it wrote nothing and returned `Ok`.

Both details are the same failure at different levels: a claim ("the lineage
is repaired", "this bug is covered") that nothing checked.

---

## 4. Why fixing them one at a time cannot converge

Let *n* be the number of stored position facts. The invariant we actually
need is a relation over all of them. The number of ways any two can disagree
grows like *n²*; the number of code paths that write one without the others
grows with the codebase.

Each fix adds a check at *one* site — the site where that disagreement
happened to be noticed. The next disagreement surfaces at a different site.
This is why five correct fixes produced a sixth stall.

The current `SyncState` machine makes it worse in a specific way: **position
is duplicated into the state value**. `DownloadingHeaders` carries
`connection_point`, a `skeleton`, and a `tracker` whose `next_to_process`
must stay consistent with what the store holds. When a reorg changes the
store underneath, the in-memory state is stale and nothing recomputes it. In
stall 5 the tracker oscillated `3 → 4 → 3` for fifteen minutes: the state
machine was making "progress" against a skeleton that no longer described
reality.

**A design that stores the same fact twice will have bugs where they
disagree, in proportion to how many times it is stored.** That is the thing
to fix.

---

## 5. Principle: one source of truth, everything else derived

**The store's block data — headers keyed by hash, and the height index — is
the only persisted truth.** It is append-only and content-addressed: a header
under hash *h* is immutable, because *h* is its hash. Append-only
content-addressed data cannot become internally inconsistent.

Everything else becomes a **pure function of it**, computed on demand:

```rust
/// The node's entire position. Not stored; derived.
struct Cursor {
    /// Highest block whose header we hold and whose lineage back to the
    /// floor is unbroken.
    validated_head: BlockRef,
    /// Highest block we have executed. Always an ancestor of validated_head.
    executed: BlockRef,
    state_root: B256,
}
```

The canonical index stops being a separate claim and becomes a **cache of the
lineage walk**, with one rule: *it may only be written by the function that
just verified the lineage it records, in the same atomic batch.* A cache that
can only be filled by the computation it caches cannot disagree with it.

`KEY_HEAD` is deleted. `our_head_number()` becomes
`cursor().validated_head.number`, which by construction cannot name a block
we do not hold, because `validated_head` is produced only by a walk that
holds every block it passes.

**Stall 5 is unrepresentable under this rule.** Its precondition was
"`KEY_HEAD` names a block the canonical chain has abandoned". There is no
`KEY_HEAD` to be stale, and no way to name a block whose lineage was not just
verified.

---

## 6. Principle: make the illegal state unrepresentable, then check what is left

Two mechanisms, in order of preference.

**Types first.** A head is not a `B256`. It is

```rust
struct Validated<T>(T);   // constructible ONLY by `verify_lineage`
fn verify_lineage(store: &Store, tip: BlockRef) -> Result<Validated<BlockRef>, Break>;
```

Every API that advances position takes `Validated<BlockRef>`. You cannot
write a head you did not verify, because you cannot *construct* the argument.
This removes an entire class by making it a compile error.

**Then one invariant function, checked always.** Some relations cannot be
typed away — particularly "executed is an ancestor of validated_head". So
state one predicate, in one place, and run it after every commit:

```rust
fn invariant(store: &Store) -> Result<(), Violation> {
    // I1  For all h in (floor, head]: canonical(h) exists and we hold it.
    // I2  For all h in (floor, head]: header(canonical(h)).parent == canonical(h-1).
    // I3  executed.number <= head.number, and executed is an ancestor of head.
    // I4  No canonical entry above head.
    // I5  The state root recorded for `executed` is present in the trie store.
}
```

Cost is O(1) per commit if checked only over the delta just written, with a
full sweep available behind a flag. It is not a debug assertion: a violation
means the node's picture of the chain is incoherent, and continuing from an
incoherent picture is how you wedge for three days. It should refuse to
proceed and say exactly which relation broke.

Every one of the five stalls violates I1, I2 or I3 at the moment it begins.
**Not one of them is detectable by any assertion that exists today.**

---

## 7. Principle: no state machine — a total, idempotent step function

Replace the six-variant `SyncState` with:

```rust
/// What we know from the network right now. No history.
struct Inbox { peers: Vec<PeerView>, arrived: Vec<Header>, bodies: Vec<Body>, now: Instant }

/// A complete, atomic change. Applying it is one WriteBatch plus some sends.
struct Transition { append: Vec<Header>, retreat_to: Option<BlockRef>, request: Vec<Request> }

fn step(cursor: &Cursor, inbox: &Inbox) -> Transition;
```

Three properties follow immediately from the shape:

- **No stale embedded position.** `step` takes the cursor as an argument,
  derived fresh from the store. There is no `skeleton` or `tracker` to go
  stale under a reorg — the thing that oscillated `3 → 4 → 3`.
- **Restart-equivalence is free.** Behaviour depends only on
  `(store, inbox)`. Killing the node mid-round and restarting is by
  construction identical to not killing it. Today, restart loses
  `last_body_height`, `follow_buffer` and the tracker, and that asymmetry has
  produced bugs of its own.
- **Testable as a pure function.** `step` needs no network, no threads and no
  clock beyond `now`. Every scenario in section 9 is a table of inputs.

Retreat becomes a first-class outcome rather than an error path bolted onto
reorg handling. `retreat_to` is exactly what PR #50 had to add by hand.

---

## 8. Principle: liveness is a measure, and a watchdog on it

This is the part that would have caught **all five** stalls, and it is cheap.

Define a progress measure, lexicographically ordered:

```
Φ = ( peer_best − validated_head.number ,   // how far behind
      outstanding_requests ,                 // work in flight
      −age_of_oldest_request )               // and how stale it is
```

Require of `step`:

> **Progress obligation.** Every transition either strictly decreases Φ, or
> is a no-op, or is an explicit `retreat_to`. A transition that commits
> changes and leaves Φ equal is a bug.

Now the watchdog. Rustock already has the loop that would have fired:

```
Skeleton round complete (head #9258222, peer #9260913), requesting next skeleton
```

printed every ~2 seconds for fifteen minutes with `head` never changing.
**"Round complete" while Φ is unchanged is precisely the bug signature**, and
the node logged it hundreds of times without treating it as anything.

So: track Φ across rounds. If *k* consecutive completed rounds leave Φ
unchanged, the node is not syncing, whatever it believes. Escalate —
retreat one step, then re-run the connection-point search, then widen — and
say so loudly. This is a generic liveness net under the whole subsystem, not
a fix for one topology.

Stall 4 (three days) and stall 5 (fifteen minutes) would both have self-healed
in seconds, before any human looked.

---

## 9. What "prove" means here

Three obligations, in increasing strength. Only the third is new work.

**(a) Invariant preservation.** `invariant()` holds after every `step`.
Mechanised as: assert it after each commit in production, and after every
step of every test.

**(b) Safety.** The node never claims a chain the network does not have. With
the store append-only and content-addressed, and heads constructible only via
`verify_lineage`, this reduces to: *every canonical entry was written by a
verified walk.* That is a one-site argument, because only one function writes
canonical entries.

**(c) Liveness, by exhaustive simulation.** The real proof, and the part
worth building:

> A deterministic discrete-event simulator. A model chain that reorgs on a
> seeded schedule. Simulated peers that delay, drop, reorder, disconnect,
> serve stale views, and lie. The node stepped by `step`, with no threads and
> no wall clock.
>
> After **every** step: assert `invariant()`.
> Over a bounded horizon: assert `validated_head` reaches the model's head.
>
> Run it over millions of seeded schedules in CI.

This is a property test, not a theorem prover, and the distinction matters —
but the coverage is of the right shape. The five stalls are all *scheduling*
phenomena: a reorg arriving while a chunk is in flight, a header whose parent
is replaced between request and delivery. A simulator that reorders and
reorgs adversarially explores exactly that space, and the invariant check
turns a subtle wedge into an immediate, minimal, reproducible failure with a
seed.

The current test suite cannot do this, for a structural reason: `step`'s
behaviour today depends on in-memory state that tests do not control, so a
test can only drive the coarse entry points and assert on whichever pointer
the author thought of. That is how a test named after stall 5 passed while
stall 5 ran.

**Acceptance for the redesign:** replay a fixed corpus of recorded mainnet
reorgs — including #9,258,222 and the 2026-09-18 three-day wedge — plus 10⁶
seeded random schedules, with the invariant asserted throughout and liveness
required in every one.

---

## 10. Migration

Not a rewrite. The store, the wire protocol, the header verifier and the
executor are all unchanged; this is a change to how position is represented
and advanced.

1. **Write `invariant()` and run it on the existing code**, after every
   commit, behind a flag defaulting to on. Expect it to fire on the recorded
   reorg corpus. This is worth doing on its own even if nothing else here is
   adopted: it converts silent wedges into loud, located failures.
2. **Add the Φ watchdog to the existing loop.** Perhaps thirty lines. It
   subsumes the ad-hoc "sideline this peer", "re-fetch the gap" and "debounce
   the buffer" heuristics, which are all local guesses at the same thing.
   Independently deployable, and would have fixed stalls 3, 4 and 5.
3. **Introduce `Cursor` as a derived value.** Delete `KEY_HEAD`; make
   `our_head_number()` read the cursor. Canonical entries writable only by
   `verify_lineage`.
4. **Introduce `Validated<BlockRef>`** and thread it through the advance
   paths, turning "wrote an unverified head" into a compile error.
5. **Extract `step`** and move the tracker's state into the store or into
   `Inbox`. This is the largest piece and should be last, once 1–2 are
   protecting the live node.
6. **Build the simulator** and let it run against the old and new
   implementations side by side, as a differential oracle during the
   transition.

Steps 1 and 2 are small, independently valuable, and reduce the damage of the
*next* stall from days to seconds. I would ship them before touching anything
structural.

---

## 11. The honest caveats

- Deriving the cursor costs a lineage walk. It must be bounded (walk to the
  last confirmed checkpoint, not to genesis) and cached — with the cache
  invalidated by the same batch that writes blocks, or we reintroduce exactly
  the problem this document is about.
- A property test is not a proof. It is the strongest thing available at
  reasonable cost, and it is a large improvement on the status quo, but it
  should not be described as more than it is.
- The Φ watchdog must not fire when the node is legitimately idle at the tip,
  or during a long body download where the header head does not move. The
  measure is lexicographic for that reason, and the no-op case must be
  distinguished from the stalled case by whether any request is outstanding.
- Retreat must be bounded. A node that retreats one block per round in
  response to a persistent disagreement walks backwards to genesis. Retreat
  should be exponential and capped, and a retreat that does not help within a
  bound is a different failure that deserves its own alarm.
