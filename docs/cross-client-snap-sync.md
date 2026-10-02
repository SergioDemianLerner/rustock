# Cross-client snapshot sync: rskj and rustock

Can an rskj node snapshot sync from a rustock server, and a rustock node from
the same server? Run against RSK mainnet at head #9,288,884 on 2026-10-01/02.

**Yes for the wire protocol, in both directions.** rustock served rskj the
complete 918 MB state and rskj accepted every part of it. rskj then failed to
*rebuild* that state in memory on this 7.7 GB host, which is a limitation of
its own design rather than anything exchanged between the two.

Getting there required finding and fixing three interop defects, all in
rustock, each of which stopped the sync dead.

## Result

| | rustock client | rskj client |
|---|---|---|
| snap status accepted | yes | yes |
| blocks below the checkpoint | yes | yes, 6,000 |
| state transferred | 875 MB in **158 s** | 918 MB in **74 m 56 s** |
| state rebuilt | yes | **no — `OutOfMemoryError`** |
| reached the head | yes, #9,288,884 | no |
| peak client memory | ~300–400 MB | >3 GB, died at `-Xmx3g` |

The rustock client completed end to end in **2 h 05 m 45 s** and arrived at
`0x9dcfe963…`, byte-identical to the server's state root at that height.

## The three defects

Each was found by a peer refusing to proceed, not by review.

### 1. Snap message framing

rustock wrote a string header inside the message body where rskj writes the
request id followed by the parameters. rskj could not parse the request at all.
Fixed in both coder and decoder, accepting either framing on the way in.

### 2. Total difficulty omitted uncle difficulty

rskj advances total difficulty by `header.difficulty + sum(uncle difficulties)`
— `Block.getCumulativeDifficulty()`. rustock advanced it by the header
difficulty alone, so our stored totals drifted below rskj's by whatever the
chain had absorbed in uncles.

rskj checks, for each adjacent pair in a snap status response,

```
td(parent) == td(child) - child.getCumulativeDifficulty()
```

and rejected ours with a bare `INVALID_BLOCK` at #9,274,999. That check is one
of only two in `SnapshotProcessor.areBlockPairsValid` that fail *silently* —
every rule in both composite rule sets logs a warning on failure, and none did,
which is what localised it.

Measured at #9,275,000, which has two uncles:

| | |
|---|---|
| difficulty | 6,441,362,214,782,067,376,988 |
| uncle difficulties | 6,409,275,777,918,861,078,714 + 6,425,298,967,363,658,231,410 |
| rskj requires | 19,275,936,960,064,586,687,112 |
| rustock had stored | 6,441,362,214,782,067,376,988 |

Fixing the rule was four lines. Fixing the *data* was not: total difficulty is
stored, so every rustock database held understated values from genesis. The
repair reads each block's body for its uncle difficulties and rewrote the chain
— 9,288,885 blocks in **7 h 31 m** at ~343 blocks/s. The uncle headers exist
only in block bodies, so a node that has pruned them cannot do this at all; the
freezer now keeps them in parallel files for that reason.

### 3. Difficulties were not two's complement

rskj keeps difficulties as `BigInteger` and writes them with
`BigInteger.toByteArray()`, which is two's complement: a value whose top bit
would otherwise be set carries a leading `0x00`. It reads them back with
`new BigInteger(bytes)` and refuses a negative outright —
`RLP.parseBlockDifficulty` throws *"A block difficulty must be positive or
zero"*, and the connection is dropped mid-decode.

Canonical RLP strips every leading zero, including that sign byte. Our status
message therefore became unreadable to rskj the moment total difficulty passed
2^95:

| | first byte | Java reads |
|---|---|---|
| before the uncle fix | `0x70` | positive |
| after | `0xc5` | **negative** |

This defect was **latent, not introduced**. Counting uncle difficulty roughly
doubled mainnet's total and brought the date forward; the chain would have
reached 2^95 unaided and broken rskj interop with no code change at all.

It is the second time rskj's non-canonical RLP has cost us, after the known
case of rskj writing a zero field as a literal `0x00` where canonical RLP
writes `0x80`. Assuming rskj speaks canonical RLP keeps generating these.

### Two more sites, found by grep rather than by failing

Once rustock's totals were correct, rustock's own *client* rejected them: it
required the offered totals to grow by exactly the header difficulty, and
capped a checkpoint's claim by the checkpoint's own difficulty. A snap sync
against a correct server failed five times and fell back to a full sync. Both
now account for uncles. The second was found by searching for the pattern after
missing the class twice.

## Method

One machine, both nodes, isolated from the network.

- **Server**: the production database opened **read-only** with
  `--follow-up false`, its own identity, `--snap-server`, offering the state at
  #9,275,000 (head − 10,000).
- **Clients**: empty databases. rustock with `--snap-sync --exit-when-synced`;
  rskj 9.1.0-SNAPSHOT with `snapshot.client.enabled = true`,
  `checkHistoricalHeaders = true`, `parallel = false`, `-Xmx3g`.
- Isolation is `--closed-network` **plus** `--bootnodes` on both sides.
  `--closed-network` alone is not isolation — without bootnodes a node still
  finds mainnet peers.
- The production node was stopped for every run.

`--bootnodes host:port` names a peer's **discovery** port, which is the listen
port plus one. Pointing it at the listen port produces a node that starts
cleanly, logs nothing unusual, and never connects.

## Where the time goes

For the rustock client, which is the only run that finished:

| phase | duration | share |
|---|---|---|
| status exchange | 11 s | 0.1% |
| **header walk to genesis** | **6,971 s** | **92.4%** |
| state download, 875 MB | 158 s | 2.1% |
| blocks and execution | 405 s | 5.4% |

The state — the thing snapshot sync exists to avoid computing — took under
three minutes. Proving the checkpoint deserves to be trusted took 44 times as
long. That ratio, not the transfer rate, is what governs snapshot sync.

For rskj, state chunks arrived about 1 s apart early and 50 ms apart at the
end: the historical header check competes for the same connection. Its 918 MB
took 74 m 56 s against rustock's 158 s for the same state from the same server.
The difference is rskj's `parallel = false` default — one chunk request in
flight against rustock's eight — not a protocol or server limit.

## Memory

rskj accumulates every node and rebuilds afterwards
(`state.getAllNodes()` → `rebuildStateAndSave`), so its heap scales with the
state:

```
java.lang.OutOfMemoryError: Java heap space
  at co.rsk.trie.TrieDTO.toMessage(TrieDTO.java:427)
  at co.rsk.trie.TrieStoreImpl.saveDTO(TrieStoreImpl.java:184)
  at co.rsk.trie.TrieDTOInOrderRecoverer.recoverSubtree(...)
```

rustock writes each node through to the trie store as the chunk arrives, so the
same 918 MB cost it ~300–400 MB against rskj's >3 GB. On a host with more RAM
rskj would likely finish; the point is that one client's memory tracks the
state and the other's does not.

## For reference: full sync

Measured separately over blocks 0–10,000, same machine, same isolation:

| | cold | warm |
|---|---|---|
| rustock | 80 s | 65 s |
| rskj | 725 s | 658 s |

Run in both orders to rule out page-cache advantage. rustock is between eight
and eleven times faster over that range.

## What this does not establish

- One machine, one run per direction, no competing traffic, no latency or loss,
  and the two processes shared a disk.
- The server read a warm production database; a cold one would be slower.
- rskj's rebuild was never exercised, so nothing here says whether the state it
  received would have reconstructed correctly. Everything up to that point
  validated.
- The full-sync figures cover 10,000 early blocks, which carry few
  transactions. They are not a claim about the whole chain.

## Open items

- rskj's snapshot client needs a heap several times the state size. Worth
  reporting upstream; the recursion in `TrieDTOInOrderRecoverer.recoverSubtree`
  is part of the cost.
- The header walk is 92% of a snapshot sync. Any work on snapshot sync speed
  belongs there, not in the state transfer.
- A rustock client's progress line reports the sequential frontier, which
  barely moves during a pipelined walk; its percentage and ETA are useless.
- The walk writes every header to RocksDB *and* the staging freezer: 19 GB for
  9.275 M headers, about 3,200 bytes for a 1,085-byte header.
