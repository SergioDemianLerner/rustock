# Epoch garbage collection for the trie store: experimental report

**Date:** 2026-09-13
**Branch:** `feat/trie-gc`, based on `main` at `487c11a`
**Design under test:** [`trie-gc-design.md`](./trie-gc-design.md)

---

## 1. Summary

An epoch-based collector was implemented for rustock's trie store and measured
against the single-database store on a synthetic high-churn workload.

Two experiments were run. **Experiment 1 was confounded** and its throughput
result is withdrawn; it is retained in §7.1 because its size and cycle-cost
results remain valid and it covers a longer run. **Experiment 2 is the
corrected comparison** and is the basis for the conclusions.

Experiment 2, 800 blocks, both backends identically configured:

| Metric | Single database | Epoch collector | Ratio |
|---|---|---|---|
| Final store size | 659.3 MB | 527.7 MB | **0.80x** |
| Wall-clock time | 2,693.6 s | 2,633.0 s | **0.98x** |
| Random reads, warm | 83,609 /s | 159,580 /s | **1.91x** |
| Random reads, cold | 20,287 /s | 21,748 /s | 1.07x |
| Space reclaimed | 0 MB | 146.9 MB | — |
| Time in collection | — | 5.0 s (0.19%) | — |
| State verification | pass | pass | — |

The collector reduced store size while being marginally *faster* overall and
substantially faster on reads. Experiment 1's finding of a 31% throughput
penalty was caused by four defects in the benchmark and the implementation,
all since corrected (§7.6).

Over the longer Experiment 1 run (2,000 blocks) the size advantage reached
**4.1x** — 457.7 MB against 1,889.9 MB — and that result stands.

## 2. Objective

Determine whether generational, epoch-based collection can bound the size of a
content-addressed trie store under sustained write churn, and at what cost to
throughput.

Two specific questions:

- **Q1.** Does the store size remain bounded while blocks continue to be applied?
- **Q2.** Is the mark phase fast enough to be practical when the store is small?
- **Q3.** What does collection cost in throughput?

Q2 arises from a prior measurement: a full traversal of rustock's 128.9 GB RSK
mainnet trie store took 2,992 s, because the live set is 0.87% of stored entries
and the reads are therefore scattered.

---

## 3. System under test

| | |
|---|---|
| CPU | 4 vCPU, AMD EPYC-Rome |
| Memory | 7.6 GB |
| Storage | Network-attached SSD, ext4 |
| Kernel | Linux 7.0.0-29-generic |
| Toolchain | rustc 1.93.1 |
| RocksDB binding | rust-rocksdb 0.25.0 (RocksDB 11.8.1) |
| Build profile | `release` |

Runs were executed sequentially, not concurrently, to avoid I/O contention
between them. No other significant workload was active on the machine.

---

## 4. Implementation under test

`crates/storage/src/epoch_store.rs` implements the design as specified, with two
deviations documented in `trie-gc-design.md` §11a:

1. **Mark** uses a batched breadth-first frontier rather than a recursive
   descent, so that keys discovered at the same level are read together.
2. **Drain** performs a sequential scan of the oldest epoch and tests set
   membership, rather than performing a lookup per live key.

Selection is by backend, not by mode: the node holds an `Arc<dyn TrieStore>`,
and `--trie-backend single|epoch` determines which implementation is
constructed. The default is `single`.

Relevant properties of the epoch backend:

- Writes go to the newest epoch unconditionally, with no existence check against
  older epochs (design invariant I4).
- Reads probe epochs newest-first; each epoch's column family carries a Bloom
  filter (10 bits/key).
- Each epoch is an independent RocksDB instance in its own directory, named by a
  monotonic sequence number.
- The sweep is a recursive directory removal.

---

## 5. Method

### 5.1 Workload

`crates/cli/examples/gc_bench.rs` simulates a chain. Each block:

1. selects a window of 1,000 storage slots of a single contract;
2. for each slot, reads the current value, increments it, and writes it back;
3. saves the resulting trie root;
4. reloads the root from its hash before the next block.

Step 4 is required for the measurement to be meaningful. `TrieNode::save` leaves
the in-memory tree materialised, so without an explicit reload every subsequent
block traverses memory and the store is never read. An earlier version of the
harness omitted this and reported 0.00 store reads.

Slot addresses are derived with `key_mapper::storage_key`, which hashes the slot
number (`keccak256(slot)[0:10]`). Sequential slot numbers therefore do **not**
occupy adjacent trie paths; they scatter.

Two workload modes exist. This report covers `growing` only:

- **`growing`** — the window advances by 250 slots per block, so 250 of each
  block's 1,000 slots are new. The live set grows without bound.
- **`bounded`** — the window cycles over a fixed range, so the live set is
  constant. Implemented but **not run**; see §10.

### 5.2 Configurations

Two experiments.

**Experiment 1** (2,000 blocks) used the configurations as originally written.
These were *not* comparable, which is why its throughput result is withdrawn:

| Option | Single | Epoch |
|---|---|---|
| Bloom filters | none (library default) | 10 bits/key |
| `cache_index_and_filter_blocks` | false | true |
| Block cache | one, library default (~8 MB) | four, library default each |
| Background jobs | 2 | 2 per epoch (8 total) |
| Rotation trigger | — | recursive directory walk, once per block |

**Experiment 2** (800 blocks) made them comparable and corrected two
implementation defects:

| Option | Single | Epoch |
|---|---|---|
| Bloom filters | 10 bits/key | 10 bits/key |
| `cache_index_and_filter_blocks` | false | false |
| Block cache | 256 MB | 256 MB, **shared across epochs** |
| Background jobs | 2 | 1 per epoch (4 total) |
| Rotation trigger | — | incrementally maintained byte counter |

Both experiments: 1,000 slots per block, `N = 4`, 128 MB epochs, burial depth
100 blocks, no write cache on either backend.

Neither configuration corresponds to a production node, which runs the single
backend behind `CachedTrieStore`.

### 5.2.1 Read benchmark

`crates/cli/examples/read_bench.rs` isolates read cost from writes, compaction
and collection. It samples keys from what a store holds, shuffles them with a
fixed seed, and fetches them, reporting throughput warm (page cache primed) and
cold (`/proc/sys/vm/drop_caches` written between runs).

### 5.3 Metrics

Recorded every 200 blocks: cumulative store size on disk (recursive directory
size), cumulative throughput, collection count, bytes reclaimed.

Recorded per collection cycle: live keys marked, entries scanned in the oldest
epoch, entries drained, bytes drained, bytes reclaimed, and the duration of
mark, drain and sweep separately.

Recorded for the whole run: wall-clock time, final size, total reclaimed, total
seconds spent in collection, and — for the epoch backend — epoch probes per
logical read.

### 5.4 Verification

Each run terminates with a verification phase, which must pass for the run to be
considered valid:

1. the head state root is re-read from the store and re-hashed; the hash must
   match;
2. the entire state reachable from the head root is walked; every referenced
   node and every long value must resolve;
3. a sample of 20 storage slots is read from the head state; each must hold at
   least its expected counter value.

---

## 6. Reproduction

```bash
git clone https://github.com/SergioDemianLerner/rustock
cd rustock
git checkout feat/trie-gc          # commit a5dbcfe
cargo build --release -p rustock-cli --example gc_bench

# Baseline: no collector
./target/release/examples/gc_bench \
    --backend single \
    --dir /tmp/gc-single \
    --blocks 2000 --slots 1000 --report-every 200

# Collector enabled
./target/release/examples/gc_bench \
    --backend epoch \
    --dir /tmp/gc-epoch \
    --blocks 2000 --slots 1000 --report-every 200 \
    --epochs 4 --rotate-mb 128 --burial 100
```

Experiment 2 (the corrected comparison, 800 blocks):

```bash
./target/release/examples/gc_bench --backend single --dir /tmp/s2 \
    --blocks 800 --slots 1000 --report-every 200
./target/release/examples/gc_bench --backend epoch  --dir /tmp/e2 \
    --blocks 800 --slots 1000 --report-every 200 \
    --epochs 4 --rotate-mb 128 --burial 100

# Read-only comparison against the resulting stores
./target/release/examples/read_bench single /tmp/s2 200000
./target/release/examples/read_bench epoch  /tmp/e2 200000
```

Run sequentially. Experiment 1 takes 1.5–2 hours per backend and needs ~2 GB of
free disk; Experiment 2 takes ~45 minutes per backend. `read_bench` writes to
`/proc/sys/vm/drop_caches` and therefore needs root for its cold measurement.

The harness deletes and recreates `--dir` on startup.

Unit tests covering the collector's invariants:

```bash
cargo test --release -p rustock-storage epoch_store
```

---

## 7. Results

### 7.1 Aggregate

**Experiment 2 — corrected comparison, 800 blocks.** This is the valid
throughput comparison.

| | Single | Epoch |
|---|---|---|
| Wall-clock | 2,693.6 s | **2,633.0 s** |
| Throughput | 0.297 blocks/s | **0.304 blocks/s** |
| Final size | 659.3 MB | **527.7 MB** |
| `collect()` invocations | 0 | 4 (1 swept) |
| Reclaimed | 0 MB | 146.9 MB |
| Seconds in collection | 0 | 5.0 (0.19%) |
| Epoch probes per read | — | 1.09 |
| Verification | pass | pass |

**Experiment 1 — confounded, 2,000 blocks.** Size and cycle results valid;
throughput result withdrawn (see §7.6).

| | Single | Epoch |
|---|---|---|
| Wall-clock | 4,900.2 s | 6,440.1 s (withdrawn) |
| Final size | 1,889.9 MB | **457.7 MB** |
| `collect()` invocations | 0 | 15 (12 swept) |
| Reclaimed | 0 MB | 1,624.6 MB |
| Seconds in collection | 0 | 112.4 (1.7%) |
| Epoch probes per read | — | 1.32 |
| Verification | pass | pass (997,329 nodes walked) |

### 7.2 Store size over time (Experiment 1)

| Block | Single (MB) | Epoch (MB) |
|---:|---:|---:|
| 200 | 123.5 | 124.8 |
| 400 | 283.9 | 293.4 |
| 600 | 462.1 | 474.1 |
| 800 | 652.2 | 416.2 |
| 1,000 | 844.3 | 493.8 |
| 1,200 | 1,042.3 | 454.6 |
| 1,400 | 1,252.3 | 538.6 |
| 1,600 | 1,461.0 | 509.7 |
| 1,800 | 1,673.6 | 484.7 |
| 2,000 | 1,889.9 | 457.7 |

The epoch store is larger than the baseline through block 600 — the cost of
unconditional duplicate writes — and smaller from block 800 onward. After block
800 it remains within 416–539 MB while the baseline grows by approximately
210 MB per 200 blocks with no observed inflection.

### 7.3 Collection cycles (Experiment 1)

All twelve sweeping cycles:

| # | Marked live | Scanned | Drained | Drained MB | Reclaimed MB | Mark s | Drain s | Sweep s |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 1 | 272,167 | 1,239,010 | 28,058 | 1 | 128 | 3.9 | 0.5 | 0.04 |
| 2 | 338,418 | 1,228,568 | 49,883 | 2 | 128 | 4.8 | 0.6 | 0.04 |
| 3 | 403,156 | 1,226,804 | 62,786 | 3 | 128 | 5.8 | 0.6 | 0.04 |
| 4 | 466,416 | 1,251,719 | 87,310 | 4 | 130 | 6.6 | 0.6 | 0.04 |
| 5 | 528,686 | 1,270,872 | 108,030 | 5 | 132 | 7.5 | 0.7 | 0.04 |
| 6 | 589,926 | 1,286,775 | 123,860 | 6 | 134 | 8.4 | 0.7 | 0.04 |
| 7 | 650,175 | 1,309,788 | 146,139 | 7 | 136 | 9.1 | 0.7 | 0.04 |
| 8 | 709,947 | 1,330,388 | 166,086 | 8 | 137 | 10.0 | 0.7 | 0.04 |
| 9 | 768,712 | 1,346,906 | 182,515 | 9 | 139 | 10.5 | 0.8 | 0.04 |
| 10 | 826,985 | 1,367,528 | 203,785 | 10 | 141 | 11.5 | 0.8 | 0.06 |
| 11 | 884,741 | 1,391,009 | 224,088 | 11 | 143 | 12.2 | 0.8 | 0.04 |
| 12 | 942,035 | 1,399,397 | 239,348 | 12 | 144 | 13.2 | 0.9 | 0.04 |

Derived:

- **Dead fraction of the swept epoch:** 97.7% (cycle 1) falling to 82.9%
  (cycle 12), as the growing live set occupies a larger share of each epoch.
- **Reclaim ratio:** 128 MB reclaimed per 1 MB copied (cycle 1); 12 MB per 1 MB
  (cycle 12).
- **Sweep duration:** 0.04 s in eleven of twelve cycles, 0.06 s in one,
  independent of the 128–144 MB reclaimed.
- **Mark throughput:** 69,800 keys/s (cycle 1) to 71,400 keys/s (cycle 12);
  effectively constant.

Experiment 2's single sweeping cycle, with the corrected configuration:

| Marked live | Scanned | Drained | Drained MB | Reclaimed MB | Mark s | Drain s | Sweep s |
|---:|---:|---:|---:|---:|---:|---:|---:|
| 308,027 | 1,403,416 | 30,414 | 1 | 146 | 4.2 | 0.6 | 0.03 |

97.8% of the swept epoch was dead; 146 MB was reclaimed for 1 MB copied.

Experiment 2 size over time:

| Block | Single (MB) | Epoch (MB) |
|---:|---:|---:|
| 200 | 124.8 | 124.8 |
| 400 | 287.1 | 293.3 |
| 600 | 467.2 | 480.0 |
| 800 | 659.3 | 527.7 |

The divergence is smaller than Experiment 1's at the same block because the
corrected rotation trigger counts logical bytes written rather than directory
size, and reached the 128 MB threshold less often over this interval — four
`collect()` calls against five, one sweep against two.

### 7.4 Mark throughput against store density

The same traversal algorithm, measured on three stores of differing density
(the first and third figures are from prior work on this machine, recorded in
`trie-tool.md`):

| Store | Live entries / total | Size | Rate |
|---|---|---|---|
| RSK mainnet trie store | 12.2M / 1,246.7M (0.87%) | 128.9 GB | 4,092 keys/s |
| Epoch store, this experiment | ~1.0M / ~5M (~20%) | ~460 MB | ~71,000 keys/s |
| Extracted mainnet snapshot | 12.2M / 12.2M (100%) | 1.2 GB | 118,893 keys/s |

### 7.5 Read cost, measured directly

`read_bench`, 200,000 random keys, against the stores produced by each
experiment:

| Store | Size | Warm | Cold |
|---|---|---|---|
| Experiment 1, single | 1.9 GB | 89,748 /s | 18,928 /s |
| Experiment 1, epoch | 458 MB | 153,580 /s | 20,896 /s |
| Experiment 2, single | 659 MB | 83,609 /s | 20,287 /s |
| Experiment 2, epoch | 528 MB | **159,580 /s** | 21,748 /s |

The epoch store reads **1.71x–1.91x faster warm** and 1.07x–1.10x faster cold,
despite requiring 1.09–1.32 lookups per logical read. Bloom filters make the
additional probes cheap, and the smaller per-database working set more than
compensates for them.

This refutes the hypothesis offered in the first version of this report, which
attributed Experiment 1's throughput penalty to read amplification. That
hypothesis was inferred from the probes-per-read figure and had not been
tested. When tested, it was false in the opposite direction.

### 7.6 Defects found, and the withdrawal of Experiment 1's throughput result

Experiment 2 differs from Experiment 1 by four changes. Together they removed
the entire 31% penalty and left the epoch backend 2.3% faster.

1. **Unequal bloom filters (benchmark defect).** The single backend was
   constructed through `RocksDbTrieStore::open`, which uses library defaults and
   configures no bloom filter. The epoch backend configured one. Experiment 1
   therefore compared two RocksDB configurations as much as two designs.
2. **`cache_index_and_filter_blocks` against a default cache (implementation
   defect).** The epoch store enabled it while each epoch had only a
   library-default block cache, so index and filter blocks competed with data
   blocks for a few megabytes and were repeatedly evicted and re-read.
3. **Fragmented block caches (implementation defect).** Four epochs each held an
   independent cache, dividing a fixed memory budget four ways. Reads
   concentrate on the newest epoch, so most of that memory served the epochs
   read least. Experiment 2 shares one 256 MB cache across all epochs.
4. **Rotation trigger walked the filesystem (implementation defect).**
   `should_collect()` computed the newest epoch's size by recursive directory
   traversal, with a `stat` per SST file — once per block. It now reads an
   incrementally maintained counter.

A fifth difference is incidental: Experiment 2 caps background compaction at one
job per epoch rather than two, so four epochs use four background threads rather
than eight on a four-core machine.

Because these were not separated from one another, the experiment does not
apportion the 31% among them. It establishes only that the penalty was an
artifact of configuration and implementation, not a property of the design.

## 8. Analysis

**Q1 — is store size bounded?** Under this workload, partially. In Experiment 1
the store was held within a 420–540 MB band from block 800 to block 2,000 while
the baseline grew monotonically to 1,889.9 MB. However, the live set in this
workload grows by design (§5.1), so the result demonstrates that *dead* state is
bounded, not that total size is constant. Over a longer run the band would drift
upward with the live set. The `bounded` workload is required to test constancy
and has not been run.

**Q2 — is mark fast enough?** Yes at the sizes tested. Marking 942,035 live keys
took 13.2 s, and mark throughput held at ~70,000 keys/s while the live set grew
3.5x, so cost is linear in live-set size rather than in store size over this
range.

The comparison in §7.4 indicates mark throughput is governed principally by the
density of the live set within the store rather than by the traversal algorithm:
the same algorithm varies by a factor of 29 across stores of differing density.
This supports the design's central premise — a store kept small is a store that
can be marked cheaply — and implies mark cost should be projected from live-set
density rather than from absolute store size.

**Q3 — what does collection cost in throughput?** Under 5% on this workload,
and within measurement noise of zero.

Experiment 2 measured the epoch backend at 2,633.0 s against the single
backend's 2,693.6 s: 2.3% *faster*, with collection accounting for 5.0 s (0.19%)
of its run. Reads were 1.91x faster warm.

The first version of this report concluded the opposite — a 31% penalty — and
attributed it to read amplification and cache fragmentation. That conclusion was
wrong, and the reasoning behind it was not tested before publication. The
throughput difference was produced by the four defects in §7.6, of which one was
a benchmark error that disadvantaged nothing but the comparison, and three were
implementation errors in the epoch store itself.

The direction of the corrected result is what the structure predicts: `N` small
databases with bloom filters are cheaper to read than one large one, because a
probe that misses is answered by a filter rather than by disk, and because each
database's working set is smaller. That the first experiment measured the
reverse should have prompted a test rather than an explanation.

**Correctness.** All four runs passed verification. The Experiment 1 epoch run
walked all 997,329 nodes reachable from the head state root after twelve sweeps
and found no dangling reference. Six unit tests cover the collector's
invariants, including the case in design §6 where a state reverts to a
previously-held value and the resulting subtree is byte-identical to one
residing only in the epoch about to be deleted.

## 9. Threats to validity

1. **The live set grows.** The `growing` workload cannot demonstrate a constant
   store size. Conclusions about boundedness are limited to dead state.
2. **Both experiments fit in RAM.** Stores of 528 MB–1.9 GB against ~5 GB of
   available page cache. The locality advantage of a smaller store is therefore
   understated: the baseline was never large enough to be penalised for its
   size. At mainnet scale (128.9 GB) the difference should be considerably
   larger, and §7.4 indicates by how much.
3. **Synthetic workload.** No EVM execution, receipts, consensus validation or
   networking. Measured costs are for the trie-storage component only.
4. **Neither configuration is production-representative.** Both ran without a
   write cache; production runs the single backend with one.
5. **Single configuration.** `N = 4`, 128 MB epochs, `D = 100`, not swept.
6. **Single run per configuration.** No repetitions and no confidence intervals.
   Size and cycle figures are deterministic given the workload; timing figures
   are not repeated measurements, and the 2.3% difference in Experiment 2 is
   within plausible run-to-run variation. The claim supported is that the
   backends are comparable in throughput, not that the epoch backend is faster.
7. **Experiment 2 is shorter.** 800 blocks against 2,000, with one sweep rather
   than twelve. It establishes the throughput comparison under a corrected
   configuration; it does not re-establish the 4.1x size result, which comes
   from Experiment 1.
8. **The four corrections in §7.6 were applied together** and are not
   individually attributed.
9. **Burial depth is unrealistically small.** `D = 100` was chosen to trigger
   collection within a short run. A production value must exceed the deepest
   survivable reorganisation.

## 10. Conclusions

1. The epoch collector functions as specified and preserves correctness: all
   state reachable from the collection root survived collection in every run.
2. It bounds the accumulation of dead state. Over 2,000 blocks it held the store
   4.1x smaller than the uncollected baseline (457.7 MB against 1,889.9 MB).
3. **It is not slower.** With both backends identically configured, the epoch
   backend completed 800 blocks 2.3% faster than the single backend and served
   random reads 1.91x faster warm. Collection work was 0.19% of run time.
4. Reclamation cost is proportional to surviving data, not to reclaimed data:
   the sweep took 0.03–0.06 s regardless of the 128–146 MB it released, and
   82.9–97.8% of each swept epoch was dead.
5. Mark cost is linear in live-set size and, at ~70,000 keys/s, is not a
   limiting factor at the sizes tested.
6. An earlier version of this report concluded that collection cost 31%
   throughput. That conclusion is withdrawn. It was an artifact of four defects
   (§7.6) and of an untested explanation offered alongside measured data.

## 11. Further work

In order of expected value:

1. **Run the `bounded` workload** to test whether store size is constant when the
   live set is constant. This is the claim the design is pitched at and the only
   one this report cannot address.
2. **Separate the four corrections in §7.6** to establish which mattered. The
   rotation trigger's per-block directory walk is the leading suspect, since it
   scaled with the number of SST files and ran on every block.
3. **Make `CachedTrieStore` wrap a `TrieStore`** so a write cache can front
   either backend, enabling a production-representative comparison.
4. **Sweep `N` and epoch size** to characterise the trade between duplication,
   read amplification and reclamation frequency.
5. **Measure at a scale exceeding RAM.** Both experiments used stores that fit
   in the machine's page cache (659 MB–1.9 GB against 5 GB available), so the
   locality advantage of a smaller store was understated. Mainnet's 128.9 GB
   store does not fit, and §7.4 suggests the advantage there is much larger.
6. **Measure on a real chain** by enabling `--trie-backend epoch` on a syncing
   node, where block application includes EVM execution.
