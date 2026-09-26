# Snapshot sync

A node joining the network has two ways to get the state at a block. It can
execute every transaction from genesis — correct by construction, and days of
work. Or it can download the state trie directly and check it against a state
root it trusts for other reasons. This is the second.

What is given up is the *re-execution* of history, not the *verification* of
it. The header chain to the snapshot point is verified under the same
proof-of-work rules a full sync applies, and every chunk of state is proved
against that chain's state root as it arrives.

## What the client ends up trusting

Proof-of-work, and nothing else — the same thing a full sync trusts.

1. A peer offers a **checkpoint block**. Nothing about it is believed yet.
2. The client **walks the header chain back** from that block to one it
   already has, verifying each header: merged-mining PoW, the consensus rules,
   and a hash link to its child. A peer that invents a checkpoint has to have
   mined it. If the walk reaches block zero and it is not *our* genesis, the
   session fails — a well-formed chain back to a genesis this node has never
   seen is a different network, and the work behind it says nothing about
   ours.
3. Only then does the client **download state** under that header's
   `state_root`, proving every chunk on arrival.
4. It also downloads the **bodies of the 6000 blocks** behind the checkpoint,
   because contracts can read recent block data, so the state alone is not
   enough to execute what comes next.

A dishonest peer can refuse to serve, serve slowly, or serve something that
fails verification. All three end the same way: the range goes back in the
queue and someone else is asked. Nothing a peer sends is written to the store
before it is checked.

### What it costs the peer

Refusing bad data is not enough on its own — a peer that pays nothing for
serving garbage can serve it again immediately. Every failure is therefore
charged to whoever *sent* the answer, not to whoever was asked for it:

| what the peer did | charged as |
|---|---|
| chunk failed its proof | `InvalidMessage` |
| answered far larger than the request | `InvalidMessage` |
| answered a question nobody asked | `UnexpectedMessage` |
| header failed validation | `InvalidHeader` |
| offered a chain that does not link, or an invented genesis | `InvalidMessage` |
| body that is not the one its header commits to | `InvalidBlock` |
| never answered | `TimeoutMessage` |

Taking a chunk from whichever peer sends it is deliberate — a proved chunk is
good whatever its route — and it only works because blame follows the sender.

Two things are **not** misbehaviour and are never charged. A peer that declines
to serve a range is behaving correctly: it may have pruned that state, or be on
another chain. An rskj peer answering in the older chunk format is speaking the
protocol it knows. Punishing either would teach the network to stop offering;
the right response is to stop asking, so both are dropped from the rotation for
the rest of the download.

### One peer cannot end the sync

A bad status or a header that fails proof of work fails that *session*. It used
to end snapshot sync for the life of the process, which handed any single peer
a free denial of service. Now the peer is charged, the session is abandoned,
and another starts against a different peer — up to `MAX_SNAP_ATTEMPTS` (5),
carrying forward the list of peers already found unhelpful.

### What the header walk costs

For a node that starts with nothing, step 2 walks from the checkpoint to
genesis: about 9.2 million headers on mainnet today. The protocol serves 192
per request on both sides (rskj caps at `syncConfiguration.chunkSize`, and so
does rustock), and each request needs the parent hash from the answer before
it — so the walk is **sequential, around 48,000 round trips**.

Two things are worth saying plainly about that.

It is not extra work. A node needs the header chain regardless; a full sync
downloads exactly the same headers. What snapshot sync skips is executing the
transactions under them, and downloading the bodies.

But it is *serial* work, where the rest of sync is not. rustock's ordinary
forward sync pipelines headers with a skeleton — ask for block identifiers
every 192 blocks, then fetch the chunks between them in parallel — and the
backward walk could do the same, linking the chunks by hash at the end with
exactly the strictness it has now. That is the obvious next improvement, and
it is deliberately not in this version: it duplicates machinery that already
exists for the forward direction, and the walk is the part where a mistake
costs the most. rskj's client walks sequentially too. Tracked as **issue
#131**.

A node that has already imported a header chain — from the rskj database
import, say — anchors on its first answer and skips the walk entirely.

### What a snap-synced node has afterwards

Every header of the chain, on disk and verified, keyed by hash. State at the
checkpoint. Bodies for the 6000 blocks behind it. And a canonical
`number → hash` index covering only that 6400-block window.

`eth_getBlockByNumber` for an older height therefore finds nothing, even
though the header is right there. Indexing the whole walk where it finishes
would mean one write batch of nine million entries in the middle of the sync
loop; it belongs in a background pass over data already on disk. Tracked as
**issue #133**.

## How a chunk is proved

The unitrie gives every node an offset in the in-order traversal, because each
node's message carries the size of its subtree (`children_size`, varint-encoded
into the message and therefore fixed by the node's hash, per RSKIP107). So "the
state" is a linear address space `0..total`, and a chunk is a contiguous run of
it.

Two things must hold before a client may keep a chunk:

- **Inclusion** — every node really is in the trie under the expected root.
- **Completeness** — the peer did not quietly skip a node inside the range it
  claims to have sent.

Completeness is the one inclusion proofs alone do not give, and the one that
matters. A peer can send a truthful *subset*: every node genuine, every node
proving its own membership, and the client builds state with a hole in it —
worse than an outright failure, because it looks like success and breaks later,
somewhere else.

Both are proved at once by **re-running the server's traversal over nothing but
the bytes the peer supplied**, and requiring it to reproduce the chunk exactly.
That works because the traversal is a pure function of hash-committed data:
offsets come from `children_size`, node identity is the keccak of the message.
If the replay starts from the expected root, uses only messages that hash to
what their parents say, and yields the same sequence, then that sequence *is*
the trie's in-order run at that offset. Omitting a node changes the replay;
changing a node changes a hash.

### The two sizes everything rests on

Both are built from `children_size`, which RSKIP107 varint-encodes into the
node's message — and the message is what the hash is taken over. So both are
**committed to by the node's hash**: a peer cannot move a node by misreporting
a size, it would have to find a collision.

They are rskj's `TrieDTO.getTotalSize()` and `TrieDTO.getSize()` respectively,
under names that say what they are for here.

For a node `n`, writing `external(n)` for the length of a value stored outside
the node (non-zero only when `n` has a long value) and `message_len(n)` for its
consensus serialization:

```
total_size(n)  = children_size(n) + external(n) + message_len(n)
stream_size(n) = external(n) + message_len(n)
               + total_size(c) for each embedded child c of n
```

`total_size` is the footprint of `n`'s whole subtree. `stream_size` is the
footprint of `n`'s own entry — itself plus any children small enough to travel
inside it, which are therefore not separate entries. It is `stream_size` that
the traversal advances by, and this is the one place the two must not be
confused: advancing by wire bytes instead never reaches the total, and the
download never terminates.

**The offset of a node is the sum of `stream_size` over every node before it in
the in-order traversal**, and `total_size(root)` is the size of the whole trie.

### The verification algorithm

Given the state root `R` the client trusts, the offset `F` the client asked
for, and a chunk of `entries` (each a node message plus its long values) and a
`witness` (bare node messages):

1. **Parse every entry, refusing malformed ones.** Every read is bounds-checked;
   a message that does not describe a node is rejected here rather than
   panicking later.

2. **Check each entry's long values against what its node commits to** — the
   exact multiset, no forgeries, no omissions, no extras. This comes *first*
   because a node that commits to a long value cannot even be measured without
   it: the value's length is part of `message_len`, hence of `total_size`, hence
   of every offset downstream. A missing value silently turns a node into a
   different node.

3. **Index everything the peer sent by its own keccak** — entries, witness, and
   long values into one map. A message filed under a key it does not hash to is
   simply never found, so there is nothing to check separately.

4. **Look up `R` in that map.** If no message the peer sent hashes to the root
   the client trusts, stop. This is the anchor: everything after it descends
   from a node whose identity the client already knew.

5. **Replay the traversal** from `F`, reading *only* that map, stopping after
   `entries.len()` nodes. Resolving a child means looking up the hash its
   parent's message gives; a hash not in the map is unresolvable and the
   traversal ends there.

6. **Require the replay to reproduce the chunk**: same number of nodes, same
   messages, in the same order.

The result carries the offsets the replay derived and `total_size(root)` — both
computed, neither claimed.

### What each step stops

| attack | caught by |
|---|---|
| a node altered | 5 — it no longer hashes to what its parent says, so the replay cannot reach it |
| a node dropped from the middle | 6 — the replay produces the real node at that position, the messages differ |
| entries reordered | 6 — the traversal order is fixed by the trie |
| a genuine node spliced in from elsewhere | 6 — genuine, but not the node at *that* point of the run |
| a chunk for a different offset | 5 — the replay starts at `F`, which the client supplied |
| a chunk from a different trie | 4 — nothing hashes to `R` |
| a long value forged, dropped, or added | 2 |
| the witness withheld | 5 — the traversal cannot descend and stops short, failing 6 |
| a malformed message | 1 |

The property that makes the table short: **the client never reads a number the
peer chose.** The offset comes from its own request, the sizes from
hash-committed fields, the order from the trie's own shape.

### Where the witness comes from

The replay has to descend from `R` to `F`, and to skip subtrees on the way
without reading them — which it can only do if it knows their sizes, which
means having their root messages. That is the witness: the ancestors between
the root and the chunk, plus the roots of the subtrees passed over. O(depth)
nodes.

It is derived rather than reasoned about. The server traverses its own trie
through a recording store, then keeps every message the traversal reached by
*following a child hash* — starting from the root and walking forward through
what it read, including through the chunk's own nodes, since the traversal
descends past them too. Long values are excluded: a node knows its value's
length from its own message, so navigation never needs the bytes.

Deriving it this way means the witness is exactly what the replay will ask for,
because it is the same traversal. What it costs is measured below.

### Nothing is claimed that could be checked instead

Because the replay computes the offsets, a chunk entry does not state where it
sits: a `SnapEntry` is a node message and its long values, and nothing else.

The response envelope still carries `from`, `to` and `complete` — rskj's shape,
kept — but the client never reads them. It matches an answer to a question by
request id, the way every other message in the protocol is matched, and takes
the offset from its own record of what it asked for. That is deliberate: a
client that trusted the echoed `from` could be answered a question nobody
asked, served the cheap start of the trie over and over while a range it had
actually assigned went unfilled.

The same goes for the size of the trie. Snap status offers one, but every proof
carries the root node, which commits to its own subtree size — so the first
chunk to verify, from any offset, from any peer, tells the client how much
there is. The advertised figure is used only to fan out the initial slices and
is discarded the moment a chunk lands.

### Chunks sit on a fixed grid

Cell `i` is the run of nodes covering `[i*G, (i+1)*G)` in offset space. It is a
function of the trie and those two numbers and nothing else, so every client
asking for cell `i` of a given state gets the same bytes.

That is what makes a server's work reusable. The checkpoint moves only every
5000 blocks — about **1.7 days** — so every client syncing in that window wants
the same ~0.92 GB of state. Off a grid they would each ask at different
boundaries and the server would recompute everything for every one of them.

rskj does the same thing: its client steps `from` by a fixed
`snapshotChunkSize * 1024`, which is **51,200 bytes** —
`RskSystemProperties.getSnapshotChunkSize()` returns a hard-coded 50, so both
sides of an rskj pair always agree.

rustock's grid is 100 KB rather than 51,200, because the witness costs the
same whatever the cell size and a bigger cell spreads it further (3.4% against
roughly 7%). Two implementations with different constants have to negotiate,
so the server advertises its grid in the snap status and the client adopts it.
And because the client still derives its next offset from the nodes it
actually received, a gap cannot open even if the two disagree about anything.

A node straddling a cell boundary is sent whole in both neighbouring cells.
That is the only duplication, it is at most one node per boundary, and it is
what makes every node land in some cell.

### What the witness costs

Measured against mainnet state at #9272510 (0.92 GB of trie):

| chunk size | witness as % of payload |
|-----------:|------------------------:|
|      25 KB |                   13.4% |
|      50 KB |                    7.4% |
|     100 KB |                    3.4% |
|     250 KB |                    1.3% |

The witness costs the same whatever the chunk size, so a bigger chunk spreads
it further. 100 KB is the default: past it the saving is under two points and
the cost is a slow peer holding a larger piece of the download for longer.

## The server caches what it computed

A cell is deterministic, so it is stored the first time it is served, keyed by
`state_root || cell_index` in its own column family, and read back for every
client after. Measured at #9272510:

| serving path | per 100 KB chunk |
|---|---|
| cold traversal | 1.73 s |
| warm traversal | 29 ms |
| from cache | 0.46 ms to decode, plus one read |

A cell stores at ~102 KB, so a whole state is ~0.94 GB of cache.

The cells of a state are dropped when the checkpoint rolls past it — they will
never be asked for again, and there is about a gigabyte of them per state.

The cache is never a source of truth. Every entry can be recomputed from the
trie, the lookup happens only *after* the state root has been located in the
store, and a server that has pruned a state refuses rather than serving cells
it can no longer justify.

## When a server will not answer

An empty answer alone says "not this one" without saying why, and the
difference decides what the client should do next. So a refusal is named:

| refusal | what the client does |
|---|---|
| `OffsetNotOnGrid` | realign and ask again — this is about the request |
| `PastTheEnd` | our size was wrong; ask again |
| `StateRootMismatch` | this peer is on another chain; stop asking it |
| `StateNotStored` | it pruned that state; stop asking it |
| `UnknownBlock` | it does not have the block; stop asking it |

None of these is misbehaviour and none is charged. The distinction that
matters is only whether another request to the same peer is worth a round
trip. The reason travels as a sixth element of the response, past the five
rskj reads.

## Serving rskj clients

An rskj client cannot read rustock's chunk format under any circumstances, so
without an encoder for theirs a rustock node can never seed one. It gets one.

The two are told apart by what the client does *not* send: an rskj chunk
request carries three elements, a rustock one carries four (the state root).
A request with no state root is served rskj's format, on rskj's grid of 51,200
bytes — a constant on their side, so not ours to choose — and cached
separately from the proved form, since the same range in two encodings must
never be confused.

The encoder is checked against **vectors generated by running rskj's own
code** (`crates/trie/tests/rskj-vectors/`, generated by the `Chunk.java` and
`Probe.java` beside them). Byte-for-byte on 53 nodes across four tries and on
8 whole chunks including mid-trie ranges, where the witness lists are not
empty. The same vectors establish something worth having on its own: for the
same keys, rustock's trie is identical to rskj's — same roots, same RSKIP107
sizes, same consensus messages, node for node.

Two details of rskj's format that a reading of the spec would not catch, and
that the vectors did:

- an embedded child's length is **three** bytes where the consensus format
  uses one;
- a long value stops being a 32-byte reference and is **inlined**, which is
  why a server that has lost a value cannot serve that node in this format at
  all.

## Reading rskj's chunks

The other direction works too: a rustock client can sync from an rskj server.

It costs more than reading our own format, and the cost is structural. rskj
drops each non-embedded child's 32-byte hash, so its nodes cannot be checked
one at a time — there is nothing to check them against until the subtree is
rebuilt and hashed from the bottom. So an rskj chunk is reconstructed first
and checked at the root, where a proved chunk is replayed and compared node by
node.

**The quadratic step is not inherited.** rskj finds each subtree's root by
scanning its range for the largest `children_size`, recursively. The scan is
unnecessary: the root of a range is the *strict* maximum of `children_size`
over it — a subtree root's children size is everything else in its range,
while any other node's is a proper subset of that, short by at least its own
bytes. A sequence whose tree is "the maximum splits the range" is a Cartesian
tree, and those build with a stack in one pass. Same tree, linear time.

### The rebuild algorithm

An rskj chunk is `[pre, nodes, first_left, last_hashes, post]`: the nodes
themselves, the ancestors the walk turned right at (each carrying the left
hash it skipped), the ancestors still owed a right subtree (each carrying the
right hash to come), and the boundary hashes of the first and last node.

Given the state root `R` the client trusts:

1. **Concatenate** `pre ++ nodes ++ post` into one in-order sequence, attaching
   each stub's given hash to the side it belongs to, and the boundary hashes to
   the first and last of the chunk's own nodes.

2. **Parse each stripped node** far enough to know its flags, its
   `children_size`, and its embedded children — converting those to consensus
   form as it goes, because their length prefix differs between the two
   encodings and an embedded child that inlines a long value is a different
   length in each.

3. **Build the Cartesian tree** over the `children_size` sequence, maximum at
   the root, with a stack in one pass.

4. **Walk it bottom-up**, and for each node emit its consensus message:
   flags and shared path copied; each non-embedded child's 32-byte hash filled
   in from the child the tree gave it, or from the stub hash the chunk supplied
   for a subtree it did not carry; embedded children re-prefixed with a
   one-byte length; a long value replaced by its keccak and a three-byte
   length, the bytes kept aside to store separately. Its hash is the keccak of
   that message.

5. **Compare the tree root's hash with `R`.** Everything stands or falls here.

### Why the Cartesian tree is the right tree

Step 3 replaces rskj's recursive scan, so it had better find the same tree.

For a range that forms a subtree, its root `r` satisfies
`children_size(r) = total of the range − stream_size(r)`, because the root's
subtree is the whole range. Any other node `k` in that range has
`children_size(k) = total of k's subtree − stream_size(k)`, and `k`'s subtree
is a proper subset of the range that excludes `r`. So

```
children_size(k) < k's subtree total ≤ range total − stream_size(r) = children_size(r)
```

— strictly, since `stream_size` is never zero. The maximum is unique, which is
what makes the Cartesian tree well defined. Had it been merely non-strict, ties
would leave the shape ambiguous, and an ambiguous rebuild is a *wrong* answer
rather than a slow one.

### What this gives up

Completeness is established *at the end* rather than by construction. If the
rebuilt root matches, the nodes and their arrangement are the ones the trie
commits to, since any other set or shape would have to collide with the root
hash. Sound — but the property arrives as a conclusion rather than falling out
of a comparison, and a conclusion is easier to get subtly wrong. It is why
rustock asks for its own format whenever it can, and why this path exists only
for peers that cannot speak it.

## Differences from rskj

rskj has snapshot sync, experimental and off by default on both sides, with no
snap boot nodes configured. It does verify each chunk as it arrives
(`TrieDTOInOrderRecoverer.verifyChunk`). Three things differ here, all in the
direction of trusting the peer less or doing less work.

**Linear replay instead of heuristic reconstruction.** rskj rebuilds the
chunk's subtree by scanning the range for the largest `children_size` to guess
each subtree root, recursively — the step the PoC report measures as quadratic
and lists as future work to replace. Walking forward from an offset is linear
and needs no guessing, because the trie says where its children are rather than
being asked to reveal it.

**Consensus messages instead of stripped nodes.** rskj's serializer strips
child hashes, recovering roughly 40% of the bytes, and rebuilds them
client-side. Keeping the consensus message means a verified node is *already*
what the store holds: the client writes `put(hash, message)` and is done. No
reconstruction pass, no intermediate representation, and every node is
independently checkable the moment it arrives. The trade is bandwidth for CPU,
memory and a class of bugs. Measured overhead against mainnet is 3.4% at the
default chunk size — against the ~40% the stripping would save, which is a real
cost and a deliberate one.

**The request names the state root.** rskj asks for "the state at block N" and
takes whatever comes back, having learned the root from that same peer's
status. Here the client, which has already verified a header chain, says which
root it wants. Peers cannot then disagree about what is being downloaded, which
is also what makes fetching one state from several unrelated peers
straightforward. A server whose chain has a different root at that height
declines rather than answering a different question.

## Wire protocol

rskj's six message types and envelope, unchanged:

| id | message | direction |
|---:|---------|-----------|
| 20 | `SnapStateChunkRequest` | client → server |
| 21 | `SnapStateChunkResponse` | server → client |
| 22 | `SnapStatusRequest` | client → server |
| 23 | `SnapStatusResponse` | server → client |
| 24 | `SnapBlocksRequest` | client → server |
| 25 | `SnapBlocksResponse` | server → client |

Two payload changes:

- The chunk request appends a fourth element, the state root. rskj reads the
  first two and ignores the rest, so the request stays readable by an rskj
  node.
- The chunk response's first element is an RLP **list** of consensus-form nodes
  plus the witness, where rskj's is an RLP **string** holding its stripped
  blob. The RLP header alone tells them apart, so a rustock client that meets
  an rskj snap server reports "this peer speaks the older chunk format" rather
  than misreading it, and that format can be added later as a decode path
  without another protocol change.

## Running it

Both sides are off by default. Snapshot sync changes how a node comes to trust
its state; that is a decision an operator makes, not one they discover.

```
# Serve snapshots to peers that ask
rustock --snap-server

# Catch up by downloading a state
rustock --snap-sync --snap-parallel 8 --snap-chunk-bytes 100000
```

| flag | default | meaning |
|------|--------:|---------|
| `--snap-server` | off | serve snapshots of this node's state |
| `--snap-sync` | off | catch up by downloading a state |
| `--snap-chunk-bytes` | 100000 | bytes of state per chunk |
| `--snap-chunk-grid` | 100000 | the offset grid cells sit on |
| `--snap-parallel` | 8 | chunk requests in flight, across all peers |

All five are settable from the config file too, under `[snapshot]`:

```toml
[snapshot]
server = true
sync = false
chunk_grid = 100000
parallel = 8
```

A cached cell is keyed by `state_root || grid || format || index`, so changing
the grid or serving a second dialect cannot hand a client bytes for a range it
did not ask about. The old cells simply go unread until the checkpoint rolls
and everything under that state root is dropped.

The checkpoint sits 10000 blocks behind the tip, rounded down to a multiple of
5000 so that independent servers converge on the same block and their chunks
are interchangeable. Both match rskj.

A node that has pruned the state at its checkpoint serves nothing rather than
serving a state it cannot complete.

## What bounds a request

Serving state is the one place a peer chooses how much work this node does:

- The chunk size is clamped to 1 MiB, so a peer asking for the whole trie in
  one message gets a chunk.
- A chunk request must name a root this node would have offered anyway, so the
  server is not an oracle for arbitrary historical states.
- Snap status is cached per checkpoint, because every client asks for the same
  one and computing it walks 400 blocks.
- Three requests per peer at a time (rskj's `maxSenderRequests`). A snapshot
  request is the most expensive thing a stranger can ask this node to do, and
  a peer that pipelines them would otherwise keep every worker busy on its own
  behalf.

**Serving happens off the network thread.** A chunk is disk-bound — most of a
second on a cold store — and the handler is called from inside the peer's
async task, so doing the work there would block every other task sharing that
runtime worker: a node that serves snapshots would stop following the chain
while it did. The work goes to a blocking thread and the answer is sent when
it is ready. This is the PoC report's V5 "dedicated thread", and what rskj
does with `scheduleJob`.

A client is likewise bounded by what it will read: a chunk more than four
times the budget it asked for is thrown away unverified, since the transport
allows 16 MB and verifying that much costs real CPU. A chunk of a single node
is exempt — a server always sends at least one, and a node larger than the
budget (a contract's code, say) would otherwise be undownloadable.

## Checking it against a real state

`crates/cli/examples/snap_serve_check.rs` serves chunks from a live mainnet
state and verifies each the way a client would — with nothing but the bytes the
chunk carries — then reports what it cost. The trie is opened read-only, so it
is safe to run beside a syncing node.

```
cargo build --release --example snap_serve_check
./target/release/examples/snap_serve_check /var/lib/rustock 9272510 100000 30
```

Against mainnet #9272510 it serves at 2.2 disk reads per node, verifies a
100 KB chunk in ~13 ms, and every node it returns matches the one the store
holds under that hash.

## Where the code is

| part | file |
|------|------|
| offset addressing, chunking | `crates/trie/src/snapshot.rs` |
| proving and verifying a chunk | `crates/trie/src/snapshot_proof.rs` |
| wire messages | `crates/networking/src/protocol/snap.rs` |
| serving | `crates/sync/src/snap/server.rs` |
| covering the offset space | `crates/sync/src/snap/client.rs` |
| sequencing a whole sync | `crates/sync/src/snap/session.rs` |
| peers, request ids, timeouts | `crates/sync/src/snap/driver.rs` |
