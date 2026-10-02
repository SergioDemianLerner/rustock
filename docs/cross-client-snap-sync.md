# Cross-client snapshot sync: rskj and rustock

Notes for rskj developers from a cross-client snapshot sync run against RSK
mainnet at head #9,288,884, on 2026-10-01/02. rustock is an independent RSK
node implementation in Rust; this records what interoperating with rskj's
snapshot sync turned up, and what the runs measured.

**Snapshot sync between the two implementations works.** rustock served rskj
the complete 918 MB state and rskj accepted the status, the cumulative
difficulties, the 6,000 blocks below the checkpoint and every state chunk. rskj
then ran out of heap rebuilding that state on a 7.7 GB host, which is the one
finding here that is about rskj rather than about the wire.

## Result

| | rustock client | rskj client |
|---|---|---|
| snap status accepted | yes | yes |
| blocks below the checkpoint | yes | yes, 6,000 |
| state transferred | 875 MB in **158 s** | 918 MB in **74 m 56 s** |
| state rebuilt | yes | **no — `OutOfMemoryError`** |
| reached the head | yes, #9,288,884 | no |
| peak client memory | ~300–400 MB | >3 GB, died at `-Xmx3g` |

The rustock client completed end to end in **2 h 05 m 45 s**, arriving at state
root `0x9dcfe963…`, byte-identical to the server's at that height.

## Difficulties are not canonical RLP, and that is a trap

This is the one wire-format issue worth raising with anyone implementing
against rskj, because nothing in the message layout reveals it.

rskj keeps difficulties as `BigInteger` and writes them with
`BigInteger.toByteArray()`, which is **two's complement**: a value whose top
bit would otherwise be set carries a leading `0x00` sign byte. It reads them
back with `new BigInteger(bytes)` and refuses a negative outright —
`RLP.parseBlockDifficulty` throws *"A block difficulty must be positive or
zero"*, netty wraps it in a `DecoderException`, and the connection is dropped
mid-decode with no protocol-level error.

Canonical RLP strips every leading zero, including that sign byte. An
implementation that encodes difficulty as a canonical RLP integer is therefore
correct by the RLP spec and unreadable to rskj — but **only once the value's
top bit is set**:

| total difficulty | first byte | `new BigInteger(bytes)` |
|---|---|---|
| 34,690,046,850,504,217,865,101,415,175 | `0x70` | positive |
| 61,049,215,127,746,862,789,732,304,263 | `0xc5` | **negative** |

Both are twelve bytes. Nothing distinguishes them except the high bit, so an
implementation can interoperate for years and then stop, with no code change on
either side, the first time the chain's cumulative difficulty crosses a power
of two that sets bit 7 of the leading byte. Mainnet's total difficulty is in
that range now.

The same applies anywhere a difficulty crosses the wire: the status message and
the `SNAP_STATUS_RESPONSE` / `SNAP_BLOCKS_RESPONSE` difficulty lists.

It is not an isolated case. rskj also writes a zero header field as a literal
`0x00` where canonical RLP writes `0x80`, which means a block body cannot be
re-encoded canonically without changing the transaction root it commits to.
Both stem from rskj's encoding not being canonical RLP, and neither is
discoverable from a message-format description.

**Suggestion.** Either state in the protocol description that difficulties are
signed two's-complement magnitudes rather than RLP integers, or have
`parseBlockDifficulty` accept an unsigned magnitude — `new BigInteger(1, bytes)`
— which would make both encodings readable and cost nothing, since a negative
difficulty is rejected anyway.

## Rebuilding the state needs a heap several times its size

rskj accumulates every node and rebuilds afterwards (`state.getAllNodes()` then
`rebuildStateAndSave`), so client memory scales with the state:

```
java.lang.OutOfMemoryError: Java heap space
  at co.rsk.trie.TrieDTO.toMessage(TrieDTO.java:427)
  at co.rsk.trie.TrieStoreImpl.saveDTO(TrieStoreImpl.java:184)
  at co.rsk.trie.TrieDTOInOrderRecoverer.recoverSubtree(...)   ← deeply recursive
```

918 MB of state did not fit in `-Xmx3g`; the process reached 3.4 GB resident
and died during the rebuild, after every byte had arrived and validated.

For comparison, rustock writes each node through to its trie store as the chunk
arrives and never holds the state in memory, so the same 918 MB cost it
~300–400 MB. On a larger host rskj would likely finish — the point is that one
client's memory tracks the size of the state and the other's does not, which
decides what hardware a snapshot sync needs.

`TrieDTOInOrderRecoverer.recoverSubtree` recursing per subtree is part of that
cost and bounds how deep a trie can be recovered independently of heap size.

## The sequential chunk default dominates transfer time

rskj's `snapshot.client.parallel` defaults to false, so one
`SNAP_STATE_CHUNK_REQUEST` is in flight at a time. Chunks arrived about 1 s
apart while the historical header check shared the connection, and about 50 ms
apart once it finished.

The same 918 MB from the same server took rustock **158 s** against rskj's
**74 m 56 s**; rustock keeps eight requests of 100,000 bytes in flight. This is
a client default, not a protocol or server limit — the server was not the
bottleneck in either run.

## Where the time actually goes

The two clients spend their time in almost opposite places, because they order
the work differently.

**rskj**, from process start to the `OutOfMemoryError`:

| phase | duration | share |
|---|---|---|
| startup | 4 s | 0.1% |
| status, then 6,000 blocks | 20 s | 0.4% |
| **state download, 918 MB** | **4,495 s** | **97.7%** |
| trie rebuild | 82 s, then died | 1.8% |
| *(historical header check, concurrent with the state)* | *2,544 s* | *overlapped* |
| total | **4,602 s** | |

The header check is not additive: rskj requests state chunks and historical
headers together, so its 42 minutes sit inside the 75 minutes of state
download. They do compete, though — chunks arrived about 1 s apart while the
header check was running and about 50 ms apart in the 22 minutes after it
finished.

**rustock**, from the run that completed end to end:

| phase | duration | share |
|---|---|---|
| status exchange | 11 s | 0.1% |
| **header walk to genesis** | **6,971 s** | **92.4%** |
| state download, 875 MB | 158 s | 2.1% |
| blocks and execution | 405 s | 5.4% |
| total | **7,545 s** | |

rustock verifies the header chain to genesis *before* asking for any state, so
the two phases are sequential and the walk dominates. rskj overlaps them, so
its wall clock is governed by whichever is slower — here the state transfer,
because of the sequential chunk default.

The totals are within a factor of 1.6 of each other, but for opposite reasons:
rustock spends 92% of its time proving the checkpoint and 2% moving the state;
rskj spends 98% moving the state and overlaps the proving inside it. Neither is
bound by the server, which served the same 918 MB in 158 s in the other run.

The implication for either implementation is the same: the state transfer is
not the expensive part of a snapshot sync unless a client makes it so. The
expensive part is establishing that the checkpoint deserves to be trusted, and
it is worth being explicit about that, because it is also the part a client is
tempted to skip. A client that sets `checkHistoricalHeaders = false` downloads
the entire state before — in fact instead of — establishing the work behind the
checkpoint it is trusting.

## A diagnostic gap

`SnapshotProcessor.areBlockPairsValid` has two checks that return false without
logging: `isParentOf`, and the cumulative-difficulty identity

```java
blockPair.getRight().equals(childBlockPair.getRight()
        .subtract(childBlockPair.getLeft().getCumulativeDifficulty()))
```

Every rule in both composite rule sets logs a warning on failure, so when one
of those two fails the result is a bare `INVALID_BLOCK` with nothing saying
which condition was not met. A log line on each would make the difference
between a minute and a day when diagnosing a peer that is almost right.

## Method

One machine, both nodes, isolated from the network.

- **Server**: rustock, production database opened read-only, `--snap-server`,
  offering the state at #9,275,000 (head − 10,000), 918 MB.
- **Clients**: empty databases. rskj 9.1.0-SNAPSHOT with
  `snapshot.client.enabled = true`, `checkHistoricalHeaders = true`,
  `parallel = false`, `-Xmx3g`, discovery disabled and one active peer.
- Host: 7.7 GB RAM, both processes sharing one disk.

## For reference: full sync

Measured separately over blocks 0–10,000, same machine, same isolation, run in
both orders to rule out a page-cache advantage:

| | cold | warm |
|---|---|---|
| rustock | 80 s | 65 s |
| rskj | 725 s | 658 s |

Those blocks carry few transactions, so this is not a claim about the whole
chain.

## What this does not establish

- One machine, one run per direction, no competing traffic, no latency or loss,
  and the two processes shared a disk.
- The server read a warm database; a cold one would be slower.
- rskj's rebuild never completed, so nothing here says whether the state it
  received would have reconstructed correctly. Everything up to that point
  validated.
