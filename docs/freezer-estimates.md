# What freezing old blocks would buy

**A prediction, written before the work, so that it can be wrong.**

Written 2026-09-28, from measurements taken while serving the first real
snapshot sync this node has ever performed (#184). The point of writing it now
is that afterwards nobody can tell a good estimate from a lucky one.

Companion to [snapshot-sync-estimates.md](snapshot-sync-estimates.md), and
written for the same reason.

---

## 1. What is measured, and what is inferred

Everything in §2 was measured on this machine, server and client on separate
physical disks, over loopback. Everything in §4 and §5 is an estimate built on
those measurements, and each one names the assumption it rests on.

One correction carried forward: an earlier note in #185 and #166 put the read
amplification at **20×**. That was wrong — it compared disk reads over an 860 s
window against bytes delivered over a 360 s window. Measured over a single
window it is **6.5×**. Everything below uses the corrected figure.

## 2. The measured baseline

Serving headers to one syncing client:

| | before #185 | after #185 |
|---|---|---|
| throughput | 934 KB/s | **5,600–7,100 KB/s** |
| server CPU | 12% of one core | **54%** |
| client CPU | ~0% | **38%** |
| server disk read | 6.5 MB/s | **37.4 MB/s** |
| amplification | 7.0× | **6.5×** |
| queue depth | **1** | 16 |

#185 changed *when* the reads happen, not *what* is read: amplification is
unchanged, throughput is 7× higher. That is what raising queue depth does, and
it is worth stating plainly because the freezer attacks the other half.

**~7.1 KB of disk is read to deliver 1.1 KB of header.**

## 3. Why it amplifies

Headers and bodies are keyed by **hash** (`CF_HEADERS`, `CF_BODIES`). Hashes
are uniformly distributed, so two consecutive blocks share neither a data block
nor usually a file. Every lookup is an independent descent: consult the table
index, test the bloom filter, fetch and decompress a whole data block, return
1.1 KB from it.

Storing by **number** in an append-only file removes the cause rather than
working around it. Consecutive blocks become consecutive bytes.

## 4. Header sync — the strong case

A header request carries **192 hashes' worth of chain** (`BlockHeadersRequest`
walks 192 back from one hash). Under a freezer that is **one contiguous ~211 KB
read** instead of 192 scattered ones.

| | now | predicted |
|---|---|---|
| amplification | 6.5× | **~1.05×** |
| disk read at today's output | 37.4 MB/s | **~6 MB/s** |
| random reads/s | ~5,300 | **~40** |

**Prediction: the disk stops being the constraint, and throughput rises until
CPU binds.**

Server CPU is 54% of one core for 5,643 KB/s, on two pinned cores → **~20 MB/s**.
Client CPU is 38% → ~29 MB/s. So the server binds first:

> **~20 MB/s, about 3.7× beyond today, ~22× over the original.**
> The header phase of a snapshot sync: 3 hours → 25 minutes → **~8 minutes**.

**The assumption this rests on**, stated so it can be checked: that the disk is
what binds today. That is inferred from *CPU headroom on both sides* while 37
MB/s of random reads are in flight — not from benchmarking the device. If the
real limit is the 16-thread pool or lock contention, the freezer buys less.
**Measuring the device's random-vs-sequential capability would settle it, and
has not been done.**

## 5. Body sync — the weaker case, and why

Bodies have the same storage problem and **will not get the same benefit**,
because the protocol asks for them differently:

```rust
pub struct BodyRequest { pub id: u64, pub hash: B256 }   // ONE hash
```

One body per request, against 192 headers per request. So a freezer cannot
collapse a body range into one read the way it can for headers — each body is
still a separate request, a separate lookup, a separate round trip.

What is still gained:

- **No LSM overhead per lookup.** No bloom test, no index descent, no block
  decompression to extract one value. A direct offset read.
- **Readahead works.** Consecutive bodies are adjacent in the file, so fetching
  block *N* pulls *N+1…N+k* into page cache for free. During a sequential sync
  that is most of the next requests already resident.

**Prediction: 2–3× on body serving, against ~6.5× for headers.** Lower
confidence than the header figure — body sizes vary with transaction count in a
way header sizes do not, and the readahead benefit depends on the client
requesting in order, which it does during a sync and may not otherwise.

### The observation worth acting on

**The protocol shape, not the storage, is what limits the body case.** A batched
body request — *n* bodies by range, the way headers already work — would let a
freezer answer a whole range in one contiguous read, and would cut round trips
by the same factor. That is a wire-protocol change and therefore an rskj
compatibility question, but it is where the remaining win is.

Without it, the freezer's body gain is capped by one round trip per block
whatever the disk does.

## 6. Separate files, not interleaved

Headers in one file, bodies in another — **not** header-body-header-body.

### The number that decides it

Measured on mainnet blocks (#4,194,304 through #9,240,576): a whole block is
**1,534–3,561 bytes**, averaging ~2,500. The header is ~1,100 of that, so a
body averages ~1,400.

A 192-header request therefore spans:

| layout | bytes covered | to deliver |
|---|---|---|
| headers only | **211 KB** | 211 KB |
| interleaved | **480 KB** | 211 KB |

Interleaving reads **2.3× more** for the same answer. That is most of the
amplification the freezer exists to remove, put straight back — and it would be
paid on the single most common request in the protocol, since the header walk
never looks at a body at all.

### Three more reasons, in order of how much they matter

**Pruning is a file operation or it is a rewrite.** This node already prunes
bodies and receipts while keeping headers (`docs/block-pruning.md`) — a node
that serves history but has dropped bodies is a supported configuration.
Separate files make that "truncate the body file"; interleaved it means
rewriting every record to remove the middle of each one.

**Body size is data-dependent and header locality should not be.** Bodies range
from near-empty to a few kilobytes with transaction count. Interleaved, how many
headers land in a page depends on how busy the chain was that day, so the
amplification varies with history rather than being a property of the format.

**The two are read in different shapes.** Headers come in runs of 192; bodies
one at a time (§5). They want different record layouts, different readahead, and
plausibly different block sizes. One file cannot be tuned for both.

### What interleaving would buy, for completeness

Fetching a whole block — `eth_getBlockByNumber` with full transactions, or
replay — gets header and body in one read instead of two. That saves one seek
per block, on a path that handles one block at a time. Against 2.3× on the
hottest path in the protocol, it is not close.

## 7. Where this actually pays

**Not in client sync time, on a real network.** 10 GB of headers at 20 MB/s is
160 Mbit sustained, which is more than most peers will give. A real sync becomes
network-bound long before it becomes disk-bound, so the client-side gain is
largely theoretical.

**In server capacity.** Going from 37 MB/s to ~6 MB/s of disk per syncing client
decides how many clients one node can serve at once. At today's rate a server
saturates its disk on a handful of simultaneous syncs; with a freezer the same
disk serves roughly six times as many.

That is the argument for #166: not faster syncs, but a network where serving
them is cheap enough that nodes do it.

## 8. How geth and reth do it

Both were read from source rather than documentation — `/mnt/import/geth-src`
and `/mnt/import/reth-src`. Several published descriptions turned out to be
wrong or silent on the points that matter most, and two of the answers below
contradict what the docs say.

### geth: the freezer

**Tables** (`core/rawdb/ancient_scheme.go`). Five, each its own file series —
`headers`, `hashes` (canonical), `bodies`, `receipts`, `bals` (EIP-7928). Every
one snappy-compressed except `hashes`, which is stored raw.

**Separate per kind, not interleaved.** geth reached the same conclusion §5a
reaches here, independently.

**Index** (`core/rawdb/freezer_table.go:54`):

```go
type indexEntry struct {
    filenum uint32 // stored as uint16 ( 2 bytes )
    offset  uint32 // stored as uint32 ( 4 bytes )
}
const indexEntrySize = 6
```

Six fixed bytes per item. Locating block *N* is `N * 6` into the `.cidx` file —
a multiply and a read, no search. Data files (`.cdat`) cap at **2 GB**
(`freezerTableSize`), which is what the 2-byte file number buys.

**Threshold** — a formula, not a constant (`core/rawdb/chain_freezer.go:132`):

```go
// max(finality, HEAD-params.FullImmutabilityThreshold)
```

with `FullImmutabilityThreshold = 90000`. Under proof of stake finality is
normally well ahead of head−90,000, so **the 90,000 is a floor for when finality
is unavailable, not the usual case** — the opposite of what the documentation
implies.

**Frozen data is deleted, not duplicated** — and the ordering is the safety
argument (`chain_freezer.go:218`):

```go
// Batch of blocks have been frozen, flush them before wiping from key-value store
if err := f.SyncAncient(); err != nil { log.Crit(...) }
// Wipe out all data from the active database
... DeleteBlockWithoutNumber(batch, ...); DeleteCanonicalHash(batch, ...)
```

Sync the flat files first, delete from the key-value store second. A crash
between the two costs duplicated data, never lost data. Genesis is always kept
in the active database, and dangling side chains are deleted separately.

**Not purely append-only.** `truncateHead` and `truncateTail` both exist
(`freezer_table.go:607`, `:718`) — the first for reorgs reaching into frozen
range, the second for pruning from below.

### reth: static files

**Segments**: `Headers` (carrying canonical hashes and terminal difficulties),
`Transactions`, `Receipts`, `TransactionSenders`, `AccountChangeSets`. Again
separate per kind.

**`DEFAULT_BLOCKS_PER_STATIC_FILE = 500_000`** (`crates/static-file/types/src/lib.rs:35`).
Published summaries say 8,192; the source says 500,000.

**Trigger**: the static-file producer works from `last_finalized_block` per
segment rather than a fixed age — the same idea as geth's formula, expressed
differently.

**Format**: NippyJar — columnar, memory-mapped, with separate data, offsets and
config files, compressed with zstd (optionally dictionary-based) or lz4. More
machinery than geth's fixed six bytes, and more flexibility than headers need.

## 9. What to do here, and why

### Copy geth's shape, not reth's

**Fixed-width index, `N * 6`.** The access pattern that matters is "give me the
192 headers below *N*", which is one index read and one contiguous data read.
NippyJar's columnar layout and dictionary compression buy flexibility this does
not need, at the cost of a format nobody here has debugged.

**One file series per kind**, for the reasons in §6 and because both
implementations agree.

**Delete after sync, never duplicate.** geth's ordering exactly: write the flat
files, fsync them, *then* delete from RocksDB. The failure mode is duplicated
data, which is recoverable and detectable; the alternative failure mode is a
hole in the chain.

### Where this must differ from both

**The threshold cannot key off finality, because RSK has none.** Both geth and
reth freeze to the finalized block. There is no finality gadget here, so the
question is what depth is deep enough that freezing is effectively permanent.

Existing constants to anchor to rather than inventing a number:

| constant | value | what it means |
|---|---|---|
| `MAX_REORG_DEPTH` | 1,000 | deepest reorg the canonical index will follow |
| `MIN_KEEP_DEPTH` (pruner) | 8,000 | the floor block pruning refuses to go below |
| snapshot `checkpoint_distance` | 10,000 | how far back the state offered to peers sits |

**Proposal: freeze below `head − 8,000`**, reusing the pruner's floor. It is
already the number this node treats as "old enough to touch", it is eight times
the deepest reorg the index will follow, and reusing it means one concept to
reason about instead of two. geth's 90,000 would be ~31 days on RSK — far more
conservative than our own reorg behaviour justifies.

`truncateHead` still has to exist for the case the threshold is wrong, but at
8,000 it should never run.

### Order of work

1. **Headers first.** They are the hot path — 192 per request, §4 — and the
   whole measured benefit is there.
2. **Receipts second**, if `eth_getLogs` turns out to be read-heavy enough to
   care. Not measured yet; should be before it is built.
3. **Bodies last**, because §5 says the protocol caps the win until body
   requests are batched.

### What not to do yet

**Do not build the general "ancient store" of #166 first.** The measured problem
is header serving, the fix for it is one file series and one index, and the rest
of #166 can follow the number it produces. Building the general thing first
means carrying receipts and bodies through a design decision that headers alone
would have settled.

## 10. How to check this

Re-run the header phase of #184 against a freezer-backed server, on the same
machine, same disks, same loopback, and record:

1. throughput, against the 5,600–7,100 KB/s measured here;
2. server disk read per byte delivered, against 6.5×;
3. server and client CPU, to see which binds once the disk does not;
4. random reads/s, which should collapse from ~5,300 to a few dozen.

Then the same for bodies, which needs an ordinary sync rather than a snapshot
one, since a snapshot sync fetches only 6,000 bodies.

**The prediction most likely to be wrong is §4's 20 MB/s ceiling**, because it
assumes CPU scales cleanly across two pinned cores and that nothing else appears
once the disk stops binding. **The one most likely to hold is §4's amplification
figure**, which is arithmetic over a known request size rather than a
projection.

If §5 comes out at 5× rather than 2–3×, the likely reason is readahead doing
more than expected, and that would be an argument for batched body requests
rather than against them.
