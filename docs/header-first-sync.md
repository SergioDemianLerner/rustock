# Header-first sync: a design note

**This describes a shape the node does not have.** It is a proposal, written
down because the sampling gate, the shipped checkpoint and three of the bugs
found around them (#256, #258, #259) all exist to solve a problem this shape
does not have. Nothing here is implemented.

## The shape

Three phases, in order:

1. **Header pass.** Download every header from genesis to the tip, with the
   uncle headers each references, verifying merged-mining proof of work and
   every consensus rule that does not need a body. No bodies, no execution.
   This establishes the canonical chain and its **exact** cumulative
   difficulty.
2. **State.** Ask a peer for the trie at a checkpoint. Its offer is now checked
   against the chain from phase 1 by hash, which is an index lookup.
3. **Bodies.** The last `blocks_required` blocks before the checkpoint, because
   contracts can read recent block data.

Phase 1 is ordinary forward sync with execution switched off. Phase 2 is what
snapshot sync already does. The change is the order and what phase 2 is allowed
to assume.

## Why this is worth considering

**Snapshot sync already downloads every header.** `HeaderWalk` descends from
the peer's offered checkpoint to "the first block this node already holds,
which is usually but not always genesis" -- and on a fresh node that is
genesis. The walk is about 92% of a snapshot sync (#244) and takes roughly
twenty-five minutes on mainnet. What snapshot sync saves against a full sync is
**bodies and execution**, not headers.

So the difficulty gate is a filter protecting a cost that is paid anyway. It
spends ~585 messages bounding a claim, to decide whether to spend twenty-five
minutes establishing the same quantity exactly. In the shape above there is
nothing to pre-filter: the header pass is unconditional, and the exact total
difficulty falls out of it.

## Why the current walk goes backwards

Worth stating, because it is not a free choice.

`BlockHeadersQuery` is `{ hash, count }`. There is no direction field -- unlike
eth's `GetBlockHeaders`, which has `reverse` -- and `serve_headers_request`
walks backward from the hash, as rskj does. **A forward request is not
expressible on this wire.**

So "forward sync" in RSK does not mean forward requests. It means asking for
block identifiers *by height* (the skeleton, which is height-addressed) and
then pulling a descending run from each. The snapshot walk does exactly that
too. Both shapes fetch descending chunks in parallel and order them by height.

The real difference is where trust is anchored:

| | anchored at | answers |
|---|---|---|
| snapshot walk | the peer's offered checkpoint, descending | "does this `state_root` sit atop real work?" |
| header-first | what this node already holds, ascending | "is there more chain, and how much work is in it?" |

Descent is self-authenticating: `parentHash` determines the ancestor chain and
every link is checkable. Ascent is not -- a header does not name its children,
so going up needs a peer to choose the next block, and the links are what catch
a wrong choice rather than preventing it.

## Parallelism

Both shapes can pipeline, and the skeleton trick that makes the current walk
parallel works just as well here. The difference is not how many requests can
be in flight but **what a failure costs**.

The snapshot walk is one bet on one peer's offered checkpoint. Headers go to a
staging freezer as they are verified and are promoted by rename only when an
unbroken run of links reaches the anchor. If the offer was bogus, or the walk
stalls, the staging freezer is discarded -- every chunk fetched was conditional
on a claim that turned out to be false.

A header-first pass is built upward from something already trusted, so a
segment can be promoted as soon as it links to the prefix below it. Chunks are
permanently valuable the moment they link. That makes aggressive fan-out across
many peers cheap to attempt: a peer that answers wrongly costs its own chunks,
not the sync.

The ceiling is then bandwidth and the per-peer message limit rather than
risk appetite:

```
  9,276,564 headers / 192 per response   =  48,316 chunk requests
  rskj's inbound limit, one peer          =  1,000 messages/minute -> 48 minutes
  the same across eight peers             =  about 6 minutes
  20.6 GB at 100 MB/s aggregate           =  about 3.5 minutes
```

against the twenty-five minutes the current walk takes. #244 already tracks
that headroom; this shape makes it safer to use.

## What it costs

Measured on this chain at #9,276,564, from the freezer index and the node's own
RPC:

| | size |
|---|---|
| trunk headers, genesis to tip | 10.1 GB |
| uncle headers they reference | 10.5 GB |
| **header pass total** | **20.6 GB** |
| state at the checkpoint | ~0.9 GB |
| 6,000 bodies | ~8 MB |
| *(for comparison: every block, full sync)* | *22.8 GB* |

The uncle headers are the whole of the extra cost, and they are needed for one
reason: cumulative difficulty in RSK counts uncle difficulty, and uncle headers
travel only in bodies. That is the same 10.5 GB the current walk must also pay
to compute an exact total -- it does not pay it today, which is #258, and the
consequence is that `walked_difficulty` understates the chain.

**With RSKIP-699 the uncle download disappears**: the header states its own
cumulative difficulty, so the pass is 10.1 GB and about 130 MB of that is the
new field. That is the combination this shape is really aiming at.

## What disappears

Everything that exists to approximate a number this shape simply has:

- `ceiling_for`, `max_work_between`, `uncle_allowance_per_mille`, `ChainSampler`,
  `SamplingGate`, `Phase::SamplingClaim`;
- the 17% slack, and the argument for why it is acceptable;
- `MAINNET_CHECKPOINT` as a shipped constant, and with it the governance
  question of asking whoever builds the node to declare a canonical fork;
- `CheckpointVerdict::SelfExceeds`, which exists because a shipped checkpoint
  can be wrong;
- issues #256, #258, #259, and most of RSKIP-695's sampling sections.

**The checkpoint check becomes an index lookup.** A server still dictates which
checkpoint it serves -- `SnapStatusResponse` carries the offered blocks, and a
client cannot request an arbitrary state root. That stops mattering: with the
whole header chain in hand, the client checks whether the offered block is the
one its own canonical index holds at that height. If the hash matches, the
`state_root` is one it derived itself and every chunk proves against it. If it
does not, the peer is on another chain and is dropped. No sampling, no ceiling,
no bound.

**Peer selection stops needing a bound too.** A peer claiming more work must
produce headers carrying it. If it can, the work is real; if it cannot, it
stalls and is dropped. The cost of a liar is bounded by how fast a stall is
noticed, not by a twenty-five minute commitment made in advance. That is what
#253 is really asking for, and this shape answers it by construction.

## What is harder, or unresolved

**Time to first state byte is not better, and may be worse.** The header pass
must finish before the trie download starts, exactly as the walk does today,
and carrying uncles roughly doubles its bytes. Without RSKIP-699 this shape is
slower end to end unless the parallelism above is actually realised.

**The tip moves during the pass.** Twenty-five minutes is about 3,000 blocks.
The pass ends at whatever tip it reached, and the checkpoint a server offers is
derived from *that server's* head rounded down. Those will not generally be the
same block, but they do not need to be -- the client only has to recognise the
offered block in its own index, and a server's checkpoint sits 10,000 blocks
below its head, well inside what the pass covered.

**Two header pipelines become one, but not for free.** `snap/headers.rs` and
the ordinary skeleton sync in `service.rs` duplicate validation today. Merging
them is the bulk of the work and is where regressions would come from.

**It still walks from genesis.** This shape does not enable syncing from a
recent checkpoint without touching old history -- neither does the current one,
so it is not a regression, but it is not the thing an MMR would buy either. See
RSKIP-699's rationale for why an MMR is not proposed.

**Storage before any state.** 20.6 GB of headers and uncles land before the
trie is requested. The freezer already holds both in parallel stores, so this
is served by existing machinery rather than needing new, but it is a disk
requirement that arrives earlier in the sync than it does today.

## Relationship to the wire proposals

- **RSKIP-698** (headers with uncles) is a prerequisite for the exact total,
  and is implemented. Against an `rsk/62` peer the pass can still run, but it
  computes a lower bound and the checkpoint check falls back to what exists
  today.
- **RSKIP-699** (cumulative difficulty in the header) removes the uncle
  download, which is what makes this shape cheaper than the current one rather
  than merely sounder.

Neither is required to start. The pass can be built against RSKIP-698 alone and
improves when 699 lands.
