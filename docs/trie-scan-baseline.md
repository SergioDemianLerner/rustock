# Unitrie scan: recursive-DFS baseline

Partial results from `--trie-stats` using the original depth-first traversal,
kept as the comparison point for the sorted-batch optimisation described in
[`trie-gc-design.md` §10](./trie-gc-design.md#10-suggested-refinement-sequential-mark).

**The run was stopped deliberately at 49.29%**, not completed. Absolute counts
below are therefore roughly half of a full scan. The *rate* figures are the
point of the record; the *ratios* between categories are already meaningful.

## Run

| | |
|---|---|
| Database | rustock, RSK mainnet, imported from rskj snapshot at #9,230,000 |
| State root | last executed block (node was syncing concurrently) |
| Expanded trie size | 916.00 MB (from the root's `children_size`) |
| Machine | 4 vCPU, 7.6 GB RAM, network-attached SSD, node running concurrently |
| Traversal | recursive DFS, memoised, one dependent random read per node |

## Result at the stopping point

| | |
|---|---|
| Progress | 49.29% of expanded bytes |
| Distinct nodes | 5,310,449 |
| Elapsed | 1h 52m |
| Deduplicated so far | 339.00 MB of 916.00 MB |

### Rate decay — the reason for the optimisation

| Point in scan | Rate |
|---|---|
| ~0.8% | 2,658 nodes/s |
| ~1.35% | 2,304 nodes/s |
| ~4% | 1,119 nodes/s |
| 49.29% | **763 nodes/s** |

A 3.5x decay over the run. This is the signature of dependent random reads:
early on the shallow nodes sit in the page cache, and as the working set
outgrows it nearly every lookup becomes a seek. Each address is unknown until
the previous read returns, so queue depth stays at one and nothing prefetches.

Extrapolating the tail rate, the full scan would have taken roughly 4 hours.

### Structure at 49.29%

| | Count |
|---|---|
| Leaves | 2,626,460 |
| Branches | 2,683,989 |
| Embedded in parent | 926,206 |

### Leaves by type

| Type | Count |
|---|---|
| storage cells | 2,320,844 |
| accounts | 301,074 |
| contract code | 4,541 |
| storage roots | 1 |

### Sharing — repeat references, by what repeats

| What repeats | Count | Bytes |
|---|---|---|
| leaf: storage cells | 23,414 | 831.50 KB |
| branch: storage region | 21,806 | 4.69 MB |
| leaf: contract code | 9,412 | **21.04 MB** |

Total: 56,839 repeat references, 26.57 MB.

**Contract code dominates the bytes while storage cells dominate the count.**
Roughly 2.5x more storage cells repeat than bytecode, yet bytecode accounts for
79% of the bytes that sharing saves — a duplicated contract is kilobytes where a
duplicated storage cell is tens of bytes. Any policy tuned on repetition *counts*
would weight these the wrong way round.

No `branch: account region` sharing was observed: no two accounts had
structurally identical sub-tries.

## What to compare after the optimisation

1. **Rate and its decay.** The sorted-batch variant should both start faster and
   decay less, since it converts scattered seeks into ordered reads.
2. **Total wall time** to 100%.
3. **Identical statistics.** Counts, sizes and classifications must match: the
   change is to read *order*, not to what is visited. Any difference is a bug.
