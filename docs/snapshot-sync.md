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

### Nothing is claimed that could be checked instead

Because the replay computes the offsets, a chunk entry does not state where it
sits. rskj's chunk response carries `from` and `to` as claims the client must
then check; here they are not on the wire at all, and the client takes the
offset it asked for from its own record of the request.

The same goes for the size of the trie. Snap status offers one, but every proof
carries the root node, which commits to its own subtree size — so the first
chunk to verify, from any offset, from any peer, tells the client how much
there is. The advertised figure is used only to fan out the initial slices and
is discarded the moment a chunk lands.

### The witness

For the replay to get off the ground the peer must send the nodes it would
otherwise be missing: the ancestors between the root and the chunk, and the
roots of the sibling subtrees the traversal skips on the way down. This is the
**witness** — O(depth) nodes, derived rather than guessed: the server records
what its own traversal read, then keeps what is reachable by following child
hashes.

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
| `--snap-parallel` | 8 | chunk requests in flight, across all peers |

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
