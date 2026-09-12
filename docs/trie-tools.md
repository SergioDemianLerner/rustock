# Unitrie inspection tools

Three things live behind the `--trie-*` flags: a statistics scan, a way to
extract one state into a standalone database, and a lookup for a single node.
They share one source argument, so anything the scan reports can be handed back
to the lookup.

## Why snapshot a state

Scanning RSK mainnet's live state takes about 48 minutes. Almost none of that is
the state itself -- 12.2M nodes, under a gigabyte -- and almost all of it is the
shape of the store they sit in: 1.25 billion entries across 129 GB, of which the
live set is **0.87%**. Consecutive live nodes are ~115 stored entries apart, so
every read is a random one.

Copying those nodes into a database that holds nothing else changes both halves
of that. The result is dense and small, so a re-scan is minutes rather than an
hour, and it is *immutable*, so two runs measure the same state -- the live store
moves under you as the node syncs, and successive scans there are not comparable.

The scan already reads every live node, so the copy costs the write and nothing
else. Take it once, then iterate against it.

## Scanning

```bash
rustock --trie-stats                                  # last executed block
rustock --trie-stats --trie-stats-block 9233965       # a specific block
rustock --trie-stats --trie-stats-source ./snap       # a snapshot
rustock --trie-stats --trie-stats-progress 0          # no progress lines
```

| Flag | Default | Meaning |
|------|---------|---------|
| `--trie-stats` | off | Scan and print statistics, then exit |
| `--trie-stats-block N` | last executed | Which block's state root to scan |
| `--trie-stats-progress N` | `30` | Seconds between full counter dumps; `0` disables |
| `--trie-stats-source DIR` | `--data-dir` | Node datadir or snapshot to read |
| `--trie-stats-copy [DIR]` | off | Also write the state to a new database |
| `--trie-stats-copy-overwrite` | off | Replace `DIR` if it exists |

The database is opened **read-only**, so a scan runs against a node that is
syncing. Progress is reported against the root's `children_size` (RSKIP107),
which gives a true percentage without a counting pass first.

Blocks before RSKIP126 (mainnet #1,591,000) are rejected with an explanation:
their header carries the legacy Orchid state root, which is not a Unitrie node
hash and will not resolve.

## Taking a snapshot

```bash
# Name it after the state root
rustock --trie-stats --trie-stats-copy

# Or choose the directory
rustock --trie-stats --trie-stats-copy /srv/snapshots/mainnet-9233965

# Replace an existing one
rustock --trie-stats --trie-stats-copy ./snap --trie-stats-copy-overwrite
```

Overwrite is opt-in on purpose. Every key in a trie database is a hash, so a
snapshot written on top of unrelated data would neither collide nor complain --
it would just quietly carry whatever was there before.

### What a snapshot contains

The same schema the node uses, which is why it opens with no translation:

- column family `trie_nodes`, key = node hash, value = the node's message;
- one entry per value over 32 bytes, keyed by the value's own hash;
- `trie_snapshot.json`, the metadata.

Embedded nodes are deliberately absent: an embedded node is serialised inside
its parent and has no entry of its own in the source either.

### The metadata file

A snapshot without it is unreadable. Every key is a hash, so there is nothing to
distinguish the root from any other node by inspection, and the whole trie is
reachable only from the root.

```json
{
  "root": "0x251ce150...",
  "block": 9233965,
  "source": "/var/lib/rustock",
  "nodes": 12247350,
  "long_values": 745920,
  "bytes": 916010000,
  "created": "2026-09-12T21:14:02Z",
  "schema": "rustock-trie-snapshot-1"
}
```

It is written **last**. A run that dies partway therefore leaves a directory
that is visibly not a snapshot, rather than one that opens and silently serves a
truncated trie.

A snapshot holds one state and no headers, so `--trie-stats-block` against one
is an error rather than a lookup that quietly returns the wrong thing.

## Looking at a single node

```bash
rustock --trie-node ""            --trie-stats-source ./snap   # the root
rustock --trie-node "0000/9"      --trie-stats-source ./snap
rustock --trie-node "a3f0/13"     --trie-stats-source ./snap
```

```
path            0000/9
hash            0x81f77d83...
stored as       its own entry in the store
depth           2 (root = 1)
kind            branch
shared_path     - (0 bits)
children_size   505421428
message_length  70
left            0x6eb69819...
right           0xe98c8b07...
type            n/a (branch nodes sit inside a key, not at the end of one)
value           none
```

Values print as hex, truncated to 80 bytes with a `....` suffix.

### Path notation

`<hex>/<bits>` -- hex digits holding the path bits most-significant first,
padded to a byte boundary, then how many of those bits count.

The suffix is what makes it unambiguous. The trie is binary, so a path is a bit
string of any length, and `0f` alone cannot say whether it means four bits or
eight. Written bare, `<hex>` means all `4 x len(hex)` bits, so `a3f` is twelve.
Output always carries the explicit form, so it can be pasted straight back in.

A path names the node whose accumulated path -- every shared-path bit and every
branch bit from the root, including the node's own shared path -- equals it.
Two consequences are worth stating:

- **No node exists at a path that ends partway through a shared path.** A shared
  path is a run of bits the trie keeps compressed inside one node. Such a path
  addresses nothing; that is not an error.
- The empty path is accepted as "the root", even though the root's own path is
  its shared path (`00/8` on RSK mainnet -- the zero byte every key starts with).

## Reading the statistics

Two sections are easy to misread.

**`LEAVES BY TYPE` undercounts accounts.** An account key is a strict prefix of
that account's storage and code keys, so an account with either is a *branch*
carrying a value, not a terminal node. Leaves-by-type therefore counts only
accounts with neither storage nor code. `VALUE-BEARING NODES BY TYPE` counts
both, and the line beneath it says how many accounts are branches.

**`expanded` is not a disk figure.** It comes from the root's `children_size`,
which counts a shared subtree once per reference -- the size the trie would
occupy if nothing were shared. `total, deduplicated` is what the nodes actually
cost.

`SHARING` and `TOP 3 MOST-REFERENCED NODES` describe repetition. The first says
how much and of what kind; the second names individual nodes, with a path for
each so `--trie-node` can show what is being repeated.

## Measured on RSK mainnet

Block #9,233,965, 4 vCPU, network-attached SSD, node syncing concurrently:

| | |
|---|---|
| Distinct nodes | 12,247,350 |
| Expanded / deduplicated | 916.01 MB / 760.85 MB |
| Saved by sharing | 155.15 MB (16.9%) |
| Leaves / branches | 6,071,806 / 6,175,544 |
| Max depth | 47 |
| Scan time | 2,912 s (4,205 nodes/s) |

Of the 50.33 MB that sharing saves, **contract code is 39.97 MB** from only
16.6% of the repeat references -- identical bytecode deployed many times.
Storage cells are the most numerous repeats (40.7%) and the smallest (1.52 MB).

Traversal strategies compared, same database, same machine, in
`docs/trie-gc-design.md` §10.2: 16 threads of point lookups reached 4,096
nodes/s against 763 for a dependent-read DFS at the same depth, while sorting
the reads and serving them from a cursor made it *worse* -- at 0.87% density the
keys are too sparse to share blocks, and an iterator cannot use the bloom
filters a point lookup gets.
