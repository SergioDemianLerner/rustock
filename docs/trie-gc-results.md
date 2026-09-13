# Epoch garbage collection for the trie store: experimental report

**Date:** 2026-09-13
**Branch:** `feat/trie-gc`, commit `a5dbcfe`, based on `main` at `487c11a`
**Design under test:** [`trie-gc-design.md`](./trie-gc-design.md)

---

## 1. Summary

An epoch-based collector was implemented for rustock's trie store and measured
against the existing single-database store on a synthetic high-churn workload.

Over 2,000 blocks each performing 1,000 storage-slot increments:

| Metric | Single database | Epoch collector | Ratio |
|---|---|---|---|
| Final store size | 1,889.9 MB | 457.7 MB | **0.24x** |
| Wall-clock time | 4,900.2 s | 6,440.1 s | 1.31x |
| Throughput | 0.408 blocks/s | 0.311 blocks/s | 0.76x |
| Space reclaimed | 0 MB | 1,624.6 MB | — |
| State verification | pass | pass | — |

The collector reduced store size by a factor of 4.1 and held it within a
420–540 MB band from block 800 onward, at a cost of 31% throughput. Collection
work accounted for 1.7% of run time; the remainder of the slowdown is attributed
to the multi-epoch read path.

---

## 2. Objective

Determine whether generational, epoch-based collection can bound the size of a
content-addressed trie store under sustained write churn, and at what cost to
throughput.

Two specific questions:

- **Q1.** Does the store size remain bounded while blocks continue to be applied?
- **Q2.** Is the mark phase fast enough to be practical when the store is small?

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

Both runs: 2,000 blocks, 1,000 slots per block.

| Parameter | Single | Epoch |
|---|---|---|
| Backend | `RocksDbTrieStore` | `EpochTrieStore` |
| Epochs (`N`) | — | 4 |
| Rotation trigger | — | newest epoch ≥ 128 MB |
| Burial depth (`D`) | — | 100 blocks |
| Write cache | none | none |

Neither configuration used `CachedTrieStore`. This makes the comparison
internally fair but means neither figure corresponds to a production node, which
runs the single backend *with* a write cache.

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

Run sequentially. Each run takes 1.5–2 hours on the system described in §3 and
requires approximately 2 GB of free disk for the baseline.

The harness deletes and recreates `--dir` on startup.

Unit tests covering the collector's invariants:

```bash
cargo test --release -p rustock-storage epoch_store
```

---

## 7. Results

### 7.1 Aggregate

| | Single | Epoch |
|---|---|---|
| Blocks | 2,000 | 2,000 |
| Wall-clock | 4,900.2 s | 6,440.1 s |
| Throughput | 0.408 blocks/s | 0.311 blocks/s |
| Final size | 1,889.9 MB | 457.7 MB |
| `collect()` invocations | 0 | 15 |
| — of which swept | 0 | 12 |
| Total reclaimed | 0 MB | 1,624.6 MB |
| Seconds in collection | 0 | 112.4 |
| Epoch probes per read | — | 1.32 |
| Epochs on disk at end | — | 4 |
| Verification | pass | pass (997,329 nodes walked) |

The first three `collect()` invocations added epochs without sweeping: with
`N = 4`, the store must reach four epochs before the oldest may be removed.

### 7.2 Store size over time

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

### 7.3 Collection cycles

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

### 7.4 Mark throughput against store density

The same traversal algorithm, measured on three stores of differing density
(the first and third figures are from prior work on this machine, recorded in
`trie-tool.md`):

| Store | Live entries / total | Size | Rate |
|---|---|---|---|
| RSK mainnet trie store | 12.2M / 1,246.7M (0.87%) | 128.9 GB | 4,092 keys/s |
| Epoch store, this experiment | ~1.0M / ~5M (~20%) | ~460 MB | ~71,000 keys/s |
| Extracted mainnet snapshot | 12.2M / 12.2M (100%) | 1.2 GB | 118,893 keys/s |

### 7.5 Attribution of the throughput cost

| Component | Seconds | Share of epoch run |
|---|---:|---:|
| Collection (mark + drain + sweep) | 112.4 | 1.7% |
| Remainder (block application) | 6,327.7 | 98.3% |
| **Difference vs baseline** | **+1,539.9** | — |

Collection accounts for 112.4 s of the 1,539.9 s difference. The remaining
1,427.5 s occurs during block application.

Two candidate causes, neither isolated by this experiment:

1. **Read amplification.** 1.32 epoch probes per logical read; approximately
   32% of reads do not hit the newest epoch.
2. **Cache fragmentation.** Four independent RocksDB instances each maintain a
   separate block cache and memtable set, dividing a fixed memory budget rather
   than pooling it.

---

## 8. Analysis

**Q1 — is store size bounded?** Under this workload, partially. The store is
held within a 420–540 MB band from block 800 to block 2,000 while the baseline
grows monotonically to 1,889.9 MB. However, the live set in this workload grows
by design (§5.1), so the result demonstrates that *dead* state is bounded, not
that total size is constant. The band is stable because live-set growth
(250 slots/block) is small relative to epoch size over this interval; over a
longer run the band would drift upward. The `bounded` workload is required to
test constancy and was not run.

**Q2 — is mark fast enough?** Yes, under the conditions tested. Marking
942,035 live keys took 13.2 s. Mark throughput was ~70,000 keys/s and did not
degrade as the live set grew by 3.5x, indicating the cost is linear in live-set
size over this range rather than in store size.

The comparison in §7.4 indicates that mark throughput is governed principally by
the density of the live set within the store rather than by the traversal
algorithm: the same algorithm varies by a factor of 29 across stores of
differing density. This supports the design's central premise — that a store
kept small is a store that can be marked cheaply — and suggests that mark cost
should be projected from live-set density, not from absolute store size.

**Cost.** The 31% throughput reduction is not dominated by collection work,
which is 1.7% of run time. It is dominated by block application, which is 29%
slower under the epoch backend. Both candidate causes in §7.5 are addressable
and untested:

- RocksDB supports sharing one `Cache` instance across databases, which would
  remove cache fragmentation.
- `CachedTrieStore` currently wraps a RocksDB handle rather than a `TrieStore`
  and so cannot be composed with the epoch store; changing that would allow a
  write cache in front of either backend.

**Correctness.** Both runs passed verification. The epoch run walked all 997,329
nodes reachable from the head state root after twelve sweeps and found no
dangling reference, and the sampled storage counters held their expected values.
Six unit tests cover the collector's invariants, including the case in design §6
where a state reverts to a previously-held value and the resulting subtree is
byte-identical to one residing only in the epoch about to be deleted.

---

## 9. Threats to validity

1. **The live set grows.** The `growing` workload cannot demonstrate a constant
   store size. Conclusions about boundedness are limited to dead state.
2. **Synthetic workload.** No EVM execution, receipts, consensus validation or
   networking. Measured costs are for the trie-storage component only; on a real
   node this is one cost among several, so the 31% figure does not translate
   directly to block time.
3. **Neither configuration is production-representative.** Both ran without a
   write cache; production runs the single backend with one.
4. **Single configuration.** `N = 4`, 128 MB epochs, `D = 100`. The balance
   between duplication, read amplification and reclamation frequency depends on
   these and was not swept.
5. **Single machine, single run.** No repetitions; no confidence intervals. The
   size and cycle figures are deterministic given the workload, but the timing
   figures are not repeated measurements.
6. **Burial depth is unrealistically small.** `D = 100` was used to trigger
   collection within a short run. A production value must exceed the deepest
   survivable reorganisation.

---

## 10. Conclusions

1. The epoch collector functions as specified and preserves correctness: all
   state reachable from the collection root survived twelve collection cycles.
2. It reduced store size by a factor of 4.1 on this workload and prevented the
   unbounded growth exhibited by the baseline.
3. Reclamation cost is proportional to surviving data, not to reclaimed data:
   the sweep took 0.04 s regardless of the 128–144 MB it released, and 82.9–97.7%
   of each swept epoch was dead.
4. Mark cost is linear in live-set size and, at ~70,000 keys/s, is not a
   limiting factor at the store sizes tested.
5. The throughput cost is 31%, of which collection itself is 1.7 percentage
   points. The remainder arises in block application and has two identified,
   untested candidate causes.

---

## 11. Further work

In order of expected value:

1. **Run the `bounded` workload** to test whether store size is constant when the
   live set is constant. This is the claim the design is pitched at and the only
   one this report cannot address.
2. **Share a single RocksDB block cache across epochs** and re-measure. This is a
   small change and is the leading candidate for the unexplained 29% slowdown in
   block application.
3. **Make `CachedTrieStore` wrap a `TrieStore`** so a write cache can front
   either backend, enabling a production-representative comparison.
4. **Sweep `N` and epoch size** to characterise the trade between duplication,
   read amplification and reclamation frequency.
5. **Measure on a real chain** by enabling `--trie-backend epoch` on a syncing
   node, where block application includes EVM execution.
