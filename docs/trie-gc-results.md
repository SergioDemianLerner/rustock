# Epoch collector: measured results

Synthetic churn benchmark, `crates/cli/examples/gc_bench.rs`, 2,000 blocks each
incrementing 1,000 sequential storage slots of one contract. Both backends ran
the same workload sequentially on the same machine (4 vCPU, 7.6 GB RAM,
network-attached SSD), neither with a write cache in front of it.

Workload: `growing` -- the slot window advances by 250 per block, so a quarter
of each block's slots are new and **the live set itself grows**. A collector can
only make growth sublinear here; it cannot make it flat. See the note at the end
about the `bounded` workload, which is the case that tests flatness.

## Result

| | Single (no collector) | Epoch (collector) | |
|---|---|---|---|
| Final size | 1,889.9 MB | **457.7 MB** | **4.1x smaller** |
| Wall time | 4,900 s | 6,440 s | 31% slower |
| Throughput | 0.40 blocks/s | 0.31 blocks/s | |
| Collections | 0 | 15 | |
| Reclaimed | 0 | 1,624.6 MB | |
| Time spent collecting | -- | 112.4 s | **1.7% of runtime** |
| Epoch probes per read | -- | 1.32 | |
| Verification | PASS | PASS | |

Both runs ended by re-reading the head state root, re-hashing it, walking the
entire live state for dangling references, and checking the storage counters.
The epoch run walked 997,329 nodes after fifteen collections and found every one
of them.

## Size over time

| Block | Single | Epoch |
|---|---|---|
| 200 | 123.5 MB | 124.8 MB |
| 600 | 462.1 MB | 474.1 MB |
| 800 | 652.2 MB | 416.2 MB |
| 1200 | 1,042.3 MB | 454.6 MB |
| 1600 | 1,461.0 MB | 509.7 MB |
| 2000 | 1,889.9 MB | **457.7 MB** |

The epoch store is *larger* early on -- it pays for the duplication that
unconditional writes require -- and then diverges once collection starts. From
block 800 onward it oscillates in a 420-540 MB band while the uncollected store
grows without bound. It ends smaller than it was at block 600, having ingested
1,400 more blocks in between.

## What a collection cycle costs

Fifteen cycles, first and last:

```
marked 272,167 live | scanned 1,239,010 | drained  28,058 | reclaimed 128 MB
   mark 3.9s   drain 0.5s   sweep 0.04s

marked 942,035 live | scanned 1,399,397 | drained 239,348 | reclaimed 144 MB
   mark 13.2s  drain 0.9s   sweep 0.04s
```

- **The oldest epoch is 83-98% dead.** The first cycle found 28,058 live entries
  among 1,239,010 -- 2.3%. Reclaiming 128 MB cost copying 1 MB.
- **The sweep is O(1).** 0.04 s, every time, independent of how much it drops.
  It is a directory removal. A reference-counted scheme would have had to touch
  1.2 million dead entries to achieve the same thing.
- **Mark is O(live), and cheap here.** 3.9 s growing to 13.2 s as the live set
  went 272k to 942k -- a steady ~71,000 nodes/s.

### Marking is fast because the store is small

This is the claim the design rests on, and it is worth stating with the numbers
side by side. The *same traversal*, over stores of different density:

| Store | Live nodes | Store size | Rate |
|---|---|---|---|
| RSK mainnet trie store | 12.2M of 1.25G entries (0.87%) | 128.9 GB | 4,092 nodes/s |
| This benchmark's epoch store | ~1M of ~5M entries | ~460 MB | ~71,000 nodes/s |
| Extracted mainnet snapshot | 12.2M of 12.2M | 1.2 GB | 118,893 nodes/s |

Keeping the store small is worth more than any amount of work on the traversal.
Three rewrites of the mainnet scan moved it between 479 and 4,096 nodes/s;
changing the *density* of what it read was worth 29x. The collector is a way of
buying that density permanently.

## The cost: 31% slower

Worth being precise about where it goes, because it is not where one might
assume.

**Collection itself is nearly free: 112 s out of 6,440, or 1.7%.** Mark, drain
and sweep together are not the expense.

The other ~1,430 s is the read path. Every read walks epochs newest-first, and
measured 1.32 probes per read -- so roughly a third of reads miss the newest
epoch and pay for a second lookup. On top of that, four separate RocksDB
instances each keep their own block cache and memtables, so a fixed memory
budget is split four ways instead of pooled.

Two concrete improvements follow, neither attempted here:

1. **Share one block cache across epochs.** RocksDB allows a `Cache` to be
   shared between instances. Four fragmented caches on a 7.6 GB machine is
   likely a large part of the gap.
2. **Put a write cache in front.** `CachedTrieStore` wraps a RocksDB handle
   rather than a `TrieStore`, so it cannot currently sit on top of the epoch
   store. The single backend in production runs *with* that cache; this
   benchmark ran both without, which is fair here but means the production
   comparison is still unmeasured.

## Honest limits of this experiment

- **The live set grows**, so this does not demonstrate a flat store -- only
  sublinear growth. `--workload bounded` cycles a fixed slot range and is the
  direct test of "the last N blocks fit in X GB". Not yet run.
- **Synthetic, not the EVM.** No transaction execution, receipts, or consensus
  work. That is deliberate -- it isolates the trie store -- but it means the
  31% figure is a slowdown on the *storage* portion of block processing, not on
  a real node's block time, where storage is one cost among several.
- **One machine, one configuration.** N=4, 128 MB epochs, burial 100. The
  balance between duplication, read amplification and reclamation will move with
  those.
