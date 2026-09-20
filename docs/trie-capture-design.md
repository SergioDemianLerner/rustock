# Capturing a trie working set: what went wrong, and how to build it right

## 0. What this document is

In September 2026 rustock produced `segbuild`: 97 range-scoped copies of the RSK
state trie, together covering the whole chain. Each copy was supposed to be
everything you need to re-execute its own slice of blocks. They were built by
**intercepting the node store** and copying every node that passed through.

They were incomplete, and — worse — they did not *say* they were incomplete.
They returned wrong answers with the same confidence as right ones. Three
distinct defects, all from the same root, all invisible to the test that was
supposed to catch them.

This document explains what happened from first principles, and then states the
rules for designing an interception-based capture system so it does not happen
again. Sections 1–3 define the concepts; skip them if you already know what a
Merkle trie node store is. Section 4 states the invariant we thought we had.
Section 5 is the failures. Section 6 is why we did not see them. Section 7 is
the design guidance, which is the point of the document.

No prior knowledge of rustock or segbuild is assumed.

---

## 1. The concepts, from the bottom up

### 1.1 A state trie is a tree of content-addressed nodes

RSK keeps all account state — balances, nonces, contract code, contract storage
— in one tree called the **unitrie**. It is a binary radix tree: every key is a
bit string, and you find a key's value by walking from the root, taking the left
or right child at each step according to the next bit of the key.

Each **node** is a small byte string (a "node message") holding its own bit of
shared path, optionally a value, and references to its left and right children.
A child reference is **the 32-byte hash of the child node**. That is the whole
of the addressing scheme: a node names its children by what they contain.

The tree's root is likewise identified by a hash, the **state root**, and each
block header carries the state root of the state after that block. Because every
node names its children by hash, the root hash commits to the entire tree: change
one byte anywhere and every hash from there to the root changes.

### 1.2 A node store is a hash → bytes map

To walk the trie you need to turn a 32-byte child hash into the child's bytes.
That is all a node store does:

```
get(node_hash) -> Option<bytes>
```

In rustock this is a RocksDB column family. The key is the node hash; the value
is the node message. (Values longer than 44 bytes are held out of line under
their own hash in the same map, so "node" below covers those too.)

Two properties of this design matter enormously later:

- **Content addressing makes insertion idempotent and safe.** A given hash always
  maps to the same bytes. Adding a node you did not have cannot corrupt
  anything, and adding it twice is a no-op. This is what later made repair
  possible at all.
- **The store has no idea what a key "means".** It cannot tell you whether a
  hash you asked for is a node that does not exist, or a node that exists in the
  world but not in this store. Both are `None`.

### 1.3 Walking the trie, and the two kinds of "nothing"

To look up a key you descend from the root, fetching each node by hash:

```
node = store.get(root_hash)
loop:
    if the key ends here            -> return node.value
    child_hash = node.left or node.right, by the next key bit
    if there is no such child       -> the key is NOT IN THE TRIE      (A)
    node = store.get(child_hash)
    if store returned None          -> the store CANNOT ANSWER          (B)
```

**(A) and (B) are completely different facts.** (A) is a proof: the trie itself
says this key does not exist, and that proof is part of the state root's
commitment. (B) is an admission of ignorance: the store is missing a node, so
the walk cannot reach a conclusion.

The trap is that the obvious API returns the same thing for both:

```rust
fn get(&self, key: &TrieKeySlice, store: &dyn TrieStore) -> Option<Vec<u8>>
```

`None` for "proven absent". `None` for "I lost a node". **Remember this. It is
the single most important sentence in this document.**

### 1.4 What we were building, and why

An archival node keeps every node of every historical state — hundreds of
gigabytes. We wanted something cheaper with a specific purpose: **independently
re-verify the chain in parallel**.

Split the chain into 97 block ranges. For range *i*, build a store containing
just the nodes that range needs. Then 97 machines can each verify one range
against one small store, sharing nothing. Each such store we call a **chunk**.

### 1.5 Capture by interception, and copy-on-read

How do you know which nodes a range needs? You do not compute it in advance. You
**re-execute the range and watch**.

The builder runs the real block processor over the range, but hands it a wrapper
around the node store instead of the store itself. The wrapper has an archival
store behind it, and it remembers everything that passes through:

```
Wrapper::get(hash):
    if the node is in the chunk        -> return it
    otherwise fetch it from the archive
        WRITE IT INTO THE CHUNK        <- "copy-on-read"
        return it
```

Writes are captured the same way. When the range finishes, the chunk holds every
node that went through the wrapper. Copying on *read* — not just on write — is
the essential part: execution reads far more of the trie than it modifies, and a
verifier needs every node it reads.

This is a sound and rather elegant technique. The failures below are not failures
of the technique. They are failures of how we bounded it, stated it, and checked
it.

---

## 2. Two invariants that sound identical and are not

Here is what we wrote down and believed:

> **(I1)** Each chunk contains every node needed to execute its block range.

Here is what interception actually gives you:

> **(I2)** Each chunk contains every node **that rustock read** while executing
> its block range.

I2 is a fact about the mechanism; you get it for free. I1 is a much stronger
claim about the *chain*. The distance between them is exactly this:

> A read set is a property of an implementation, not of the blockchain.

Two correct clients can execute the same block, agree on every state root, and
still touch different sets of trie nodes — because one checks something the
other knows statically, or caches something the other re-reads, or implements a
concept the other does not have at all.

So I2 only implies I1 for **one** client: the one that produced the chunk. For
anybody else, I2 guarantees nothing. And a chunk's whole purpose was to be
handed to somebody else — specifically, to rskj, for independent verification.
The single most important consumer was the one the invariant excluded.

---

## 3. What the gap cost: three defects

All three are the same root cause seen from different angles. Measured
2026-09-20 against chunk 075 (blocks 7,237,813–7,306,518).

### 3.1 A read that went around the interceptor

To start executing at block *N*, the builder needs the state root node of block
*N−1*. It fetched it like this:

```rust
let data = fallback.get(p.state_root.as_slice())?;     // straight to the archive
TrieNode::from_message(&data, fallback.as_ref())       // and again
```

`fallback` is the archival store. Not the wrapper. This is the one read in the
entire build that bypassed interception, so that one node was never copied into
the chunk.

The consequence is almost comic: **a chunk could not execute its own first
block**, because step one is to load the state root it seeds from. This was true
in **96 of the 97 chunks** — all but the one that starts at genesis and so has no
parent to load.

It went unnoticed from the day the chunks were sealed until they were first
probed by something other than rustock, because every test replayed from the
*middle* of a range, where the seeding root is one the builder itself computed
and saved.

**The lesson is not "we forgot one call site."** It is that the design permitted
a call site to exist at all: the raw store was in scope, so it could be used, so
eventually it was.

### 3.2 Reads that never happened

rustock loads a precompiled contract's account only when that contract is
actually *called*. Over a 70,000-block range, contracts at `0x…01000009`,
`0x…01000010` and `0x…01000011` are never called, so nothing ever walked to their
accounts, so the nodes on those paths were never copied.

Those accounts **exist**. Ask the archive and it says so. Ask the chunk and it
says *no such account* — a wrong answer delivered without any error.

`0x…01000006` (the Bridge) and `0x…01000008` (REMASC) were fine, because they are
touched constantly. That contrast is what makes the rule visible: the chunk's
contents track *usage*, not *existence*.

### 3.3 Absences that could not be proved

This is the deepest one and worth slowing down for.

rskj reads a "storage version" cell before decoding certain contract state.
rustock has no notion of storage versions at all — zero occurrences in the
codebase — so it never issues that read. The cell is usually *absent* from the
trie, and rskj's read is an **absence proof**: the walk descends until the trie
structurally shows the key is not there.

To *prove* an absence you must still load every node down to the branch point
where the path dies. Those nodes are real, and they were never copied, because
rustock never walked there.

So the chunk was asked "is this cell present?" and answered "no" — the correct
answer — by falling off the end of its own data (case B from §1.3) rather than by
reaching the trie's dead end (case A). **Right answer, no supporting evidence, no
way for the caller to tell.** Every Bridge storage cell we probed behaved this
way.

Now put §3.2 and §3.3 together and you have the real hazard:

> An incomplete chunk does not fail. It answers wrongly. And because the wrong
> answer is "this key is absent", the consumer recomputes a state root that
> differs from the header — and concludes it has found a **consensus bug**.

You would spend days looking for a divergence in the EVM that does not exist.

---

## 4. Why we believed it was correct

A claim this wrong survived review because the evidence for it was bad in a
specific, instructive way.

**The validation used the producer as the verifier.** The proof offered was:
replay 30 blocks against one chunk alone, with no fallback, and check every state
root against its header. Every root matched.

That test cannot fail. It replays with rustock, and I2 says the chunk contains
exactly what rustock reads. The test verifies the mechanism against itself. It
would have passed just as cleanly on a chunk missing 90% of what rskj needs —
which, for some key sets, it was.

It also started mid-range, so it never touched the §3.1 seed node either. One
test, blind to all three defects, reported as proof.

**The failure mode is silent.** Per §1.3, a missing node and an absent key are
the same `Option::None`. Nothing logs, nothing throws, no counter moves. There is
no observable event to detect even if you are looking.

**The invariant was stated in absolute terms.** "Self-contained for its block
range" mentions no client and no read set. Written as I2 — "contains what rustock
reads" — the question *"what about a client that reads something else?"* asks
itself. Written as I1, it never comes up.

---

## 5. Root causes, stated generally

Strip out the RSK specifics and four faults remain, each independently
sufficient to cause this:

| | Fault | General form |
|---|---|---|
| **C1** | The builder could reach the source store directly | The interception boundary was a convention, not a constraint |
| **C2** | Capture was bounded by one implementation's behaviour | The invariant was implementation-relative but stated as absolute |
| **C3** | Missing data was indistinguishable from absent data | The store's API conflated *ignorance* with *knowledge* |
| **C4** | Validation replayed with the producer | The oracle and the subject were the same program |

C3 is the one that turns a bounded gap into a correctness disaster. C1 and C2
create holes; C3 hides them; C4 guarantees nobody looks.

---

## 6. How to build this correctly

Rules, in rough order of importance. Each states the rule, why it follows from
above, and what it looks like in practice.

### R1. Make "cannot answer" a distinct, loud result — everywhere

**Fixes C3. Do this one first; it makes every other bug shallow.**

A node store must not return `Option`. It must distinguish the two cases from
§1.3, all the way up through the trie layer:

```rust
enum NodeLookup { Found(Vec<u8>), NotInStore }        // store level

enum TrieLookup  { Value(Vec<u8>), ProvenAbsent, Incomplete { node: B256 } }
```

`ProvenAbsent` means the trie structurally demonstrates the key is not there.
`Incomplete` means a node was needed and could not be loaded, and it carries the
hash so the gap is reportable.

Consumers must be unable to accidentally treat `Incomplete` as absent. Do not
offer a convenience `.unwrap_or_default()`. In a verifier, `Incomplete` should
abort the block, not fall through.

If you take one thing from this document: **the type must make the mistake
unspeakable**, because the runtime behaviour of the two cases is identical and
no test will separate them for you.

### R2. Make the interceptor the only reachable path to the source

**Fixes C1.**

The §3.1 bug was possible because `fallback` was a live handle in the same scope
as the wrapper. Structure it so it cannot be:

- Construct the source store *inside* the wrapper and never expose it. The
  wrapper owns it; nothing else holds a reference.
- Give the build function the wrapper only. It should not be able to name the
  archive's type, let alone call it.
- If you need a bootstrap read before execution (the seed root — you always do),
  make it a method **on the wrapper**, so it is captured like everything else.

Then add a cheap runtime assertion, because scoping discipline erodes:

- The wrapper counts source fetches. **At seal time, re-run the range's first
  block using the chunk alone and assert the source-fetch counter stays at zero.**
  That single assertion catches §3.1 in seconds, and we had every piece needed to
  write it.

### R3. Write the read set down, and capture a deliberate superset

**Fixes C2.**

Accept that you cannot capture "what the range needs" — that is not a
well-defined set. You can only capture "what *some specified reader* needs". So
specify it:

1. **Name the target readers** in the design document. Ours should have said: *rustock, and rskj.*
2. **Enumerate what the other readers touch that you do not.** For us: storage
   version cells, precompile accounts on every call rather than on dispatch,
   existence checks. This is a finite, reviewable list, and it is exactly the
   list nobody wrote.
3. **Amplify reads during the build.** At every block, touch that extra key set
   deliberately so the interceptor copies those paths. The cost is small — the
   nodes are shared across blocks and the walks hit cache — and it is enormously
   cheaper than discovering the gap after the fact.
4. **Capture absence paths on purpose.** For any key your readers *probe for
   absence*, walk to it even though you expect nothing. The walk is the point.

Also capture *shape*, not just specific keys. We patched the Bridge's storage
subtree by walking 128 arbitrary slots whose hashed keys spread across it,
dragging in the upper levels of that subtree. Now most unenumerated keys in that
region can at least reach a genuine dead end. Generalise: for each region a
consumer will probe, pull in its top *k* levels.

### R4. Validate with a different reader, or a deliberately hostile one

**Fixes C4.**

Never certify a capture by replaying with the program that produced it. Options,
best first:

- **A second implementation.** rskj reading a chunk is the real test. If a second
  implementation exists, it is worth the integration cost.
- **A perturbed reader.** Same code, extra reads injected: load every precompile
  account each block, probe keys you expect to be absent, re-read what you cached.
  Cheap to build, and it catches most of C2.
- **A structural audit** that needs no reader at all. For a sample of state roots,
  walk to a fixed key set in the chunk while counting `NotInStore`, and compare
  the answer against the archive. Report three columns: *archive says*, *chunk
  says*, *nodes the walk could not load*. A nonzero third column is a defect even
  when the first two agree — that is precisely §3.3, and it is the column that
  would have caught it.

Our audit tool prints exactly that table. It was written after the defects were
found; it should have been a precondition of starting the build.

### R5. Make each artifact self-describing

A chunk should carry a manifest stating what it promises, so a consumer can check
its assumptions instead of guessing:

```
range            7237813..7306518
seed_state_root  0x839eff…        <- and it must be IN the store
producer         rustock 0.1.0 (git sha)
captured_for     rustock; rskj-compatible key set v2
key_set_digest   sha256:…
sealed_at        2026-09-17T…
```

`captured_for` is the honest form of the invariant. Any consumer not on that list
is on notice. Had this field existed, §3.2 and §3.3 would have been visible as
*design*, not discovered as *bugs*.

### R6. Verify at seal time, while the source is still open

The cheapest moment to notice a gap is before you declare the artifact finished,
because the archive is still attached and repair is one write away. Before
sealing:

1. Re-execute the range's **first** block from the chunk alone — catches §3.1.
2. Re-execute a **sample spread across the range** from the chunk alone — catches
   ordinary omissions.
3. Run the structural audit of R4 at a sample of roots — catches §3.2 and §3.3.
4. Assert the source-fetch counter is zero throughout.

All four together cost a few minutes against build times measured in days.

### R7. Design for post-hoc repair, and exploit content addressing

We were lucky: because nodes are keyed by their own hash, adding missing ones
later is **purely additive and idempotent**. No versioning, no invalidation,
safe to interrupt, safe to re-run. Repairing all 97 chunks took 5 hours of
wall-clock and added 209,526 nodes — no re-execution at all.

Design for this deliberately:

- Keep the source archive until consumers have signed off. Do not delete it the
  moment the chunks look finished.
- Ship a repair tool with the artifact, taking a key set as input, so a consumer
  who hits a gap can close it without a rebuild.
- Instruct consumers to **report** `Incomplete{node}` with the state root and the
  key being walked. That triple is enough to patch. A consumer who silently works
  around a gap destroys the only signal you have. (R1 is what makes such a report
  possible to write.)

### R8. Know the cost model before you design the walk

Two measurements changed the repair from infeasible to routine, and both are
general to this kind of tool:

- **A per-key trie lookup restarts at the root.** Looking up 103 keys that share
  most of their prefix cost 3,592 node reads per state root. One combined descent
  carrying the whole key set down and splitting it at each branch cost 1,258 and
  returned identical results. If you walk many related keys, walk them together.
- **Consecutive state roots walk almost the same nodes.** The paths differ only
  near the top. A plain hash map in front of the store gave a 99% hit rate and
  took throughput from 26 to 176 roots per second.

And one that justified sampling rather than exhaustive work: **a missing node
belongs, by construction, to a region the producer never touched, and untouched
regions change rarely.** So each missing version is long-lived and a coarse sweep
finds it. We checked instead of assuming — on one chunk a stride-16 sweep found
all 975 missing nodes, and a subsequent sweep of all 40,192 roots added none.

---

## 7. Checklist

Before building an interception-based capture system:

- [ ] The store API distinguishes *proven absent* from *cannot answer*, and the
      distinction survives to the top of the trie layer (**R1**)
- [ ] Nothing outside the wrapper can reach the source store (**R2**)
- [ ] The target readers are named in writing (**R3**)
- [ ] The extra keys those readers touch are enumerated, and the build touches
      them deliberately (**R3**)
- [ ] Absence-probe paths are walked on purpose (**R3**)
- [ ] The top *k* levels of each probed region are pulled in (**R3**)
- [ ] Validation runs a reader that is *not* the producer (**R4**)
- [ ] The audit reports "nodes the walk could not load", not just answers (**R4**)
- [ ] Each artifact carries a manifest naming who it was captured for (**R5**)
- [ ] Seal-time verification replays the first block, and a sample, from the
      artifact alone, asserting zero source fetches (**R6**)
- [ ] The source archive is retained until consumers sign off (**R7**)
- [ ] A repair tool ships with the artifact (**R7**)
- [ ] Consumers are told to report gaps rather than work around them (**R7**)

---

## 8. What we would change in `build_segments` specifically

1. `WindowStore::get` returns `NodeLookup`, and `TrieNode::get` returns
   `TrieLookup`. The `Option`-returning convenience methods are removed, not
   deprecated.
2. `WindowStore::new` takes the archive *path* and opens it internally.
   `run_chunk` never sees an `Arc<dyn TrieStore>` for the archive.
3. `WindowStore::seed_root(block)` replaces the direct `fallback.get` at
   `build_segments.rs:226`.
4. The block processor is handed an amplified key set and touches it once per
   block.
5. `WindowStore::finish()` refuses to write the completion marker until it has
   replayed the range's first block, and a sample, from the sealed store alone
   with the archive detached.
6. Each chunk gains a `manifest.json` per R5, and `segments.csv` records the
   producer's git sha.

---

## 9. Afterword

None of the three defects was a subtle algorithmic error. The copy-on-read
mechanism worked exactly as designed, on every read it saw. What failed was the
boundary around it — one call site outside the interceptor — and the language we
used to describe what it produced.

"Self-contained for its block range" was a sentence nobody could disprove,
because it never said *for whom*. Adding those two words would have forced all
of this analysis before a single chunk was built, for free.
