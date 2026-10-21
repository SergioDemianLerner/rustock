# FlatDB rollout log

A dated record of what was changed on the mainnet node and when, so the
Prometheus series can be cut at the right places. Prometheus retention is
365 days, so every boundary below is queryable.

Scrape target `127.0.0.1:9898`, job `rustock`, 30 s interval. The series
worth comparing:

| metric | what it answers |
|---|---|
| `rustock_execution_seconds_total` / `rustock_blocks_executed_total` | mean seconds to execute a block |
| `rustock_gas_used_total` | the work those blocks actually contained, so a rate change can be separated from a load change |
| `rustock_sync_height` | catch-up rate |
| `rustock_peers` | whether a slowdown was the network rather than the node |

`rate(rustock_execution_seconds_total[5m]) / rate(rustock_blocks_executed_total[5m])`
is the comparison; dividing the totals would average over the whole
period either side and hide the shape.

## Why this is staged

The engine change and the code change have to be separated, or the
comparison measures both. The binary that ran before phase 0 was built
**2026-10-18**, which predates eleven commits — among them a trie-cache
lock fix measured at 1.34×, batched reads in the epoch store worth 7.9×
on a sweep, and the FlatDB work itself. Switching binary and engine at
once would credit all of it to FlatDB.

So: install the new binary on the **unchanged** RocksDB configuration
first, let it settle, and only then change the engine.

---

## Phase 0 — new binary, RocksDB unchanged

**Started 2026-10-20 21:22:50 UTC** (18:22:50 -03), commit `b1f4944`.

A first start at 21:17:13 was replaced five minutes later and should be
excluded: that binary was built without the `smtp` feature and came up
with peg-out alerting disabled. Cut the series at **21:22:50**.

Everything about the node's configuration is as it was: `--trie-backend
epoch`, `--trie-dir /var/lib/rustock/trie-epochs`, four epochs, rotate at
1024 MB, burial 4000. Only the binary changed.

What this phase is for:

- a baseline for phase 1 taken with the **same code**, so the engine
  comparison is about the engine;
- validating eleven commits against a real chain before a new storage
  engine is added on top of them.

What materially changes for *this* configuration, out of those commits:

- **Batched reads in the epoch store.** `EpochTrieStore::get_many` used
  to map `get` over the keys, discarding the parallel implementation
  `RocksDbTrieStore` already had. The sweep's marking phase therefore ran
  at queue depth 1. Measured at 7.9× on the regtest fixture, so GC sweeps
  should get visibly shorter.
- **Durability ordering is now structural** rather than a convention each
  call site followed. The cadence is unchanged at 100 blocks.

What does not change: the node uses the epoch backend, so the
`CachedTrieStore` lock fix does not apply to it; RSKIP-144 is unscheduled
on mainnet, so the parallel scheduler is not reached; all six RocksDB
options are identical to before (one was dropped in the refactor and put
back in `b1f4944` before this phase started).

### Two things the deployment itself turned up

**A dropped RocksDB option.** Moving the setup behind the backend seam
left `write_buffer_size`, `max_write_buffer_number` and
`max_background_jobs` at RocksDB's defaults. The last matters most:
every epoch is a separate instance, so the default of 2 across four
epochs is eight background compaction threads on a four-core machine.
Caught by a dead-code warning on the constant whose comment explained
it. Fixed in `b1f4944`, before the node was started, and all six options
were then checked against the pre-refactor set one by one.

**A missing build feature.** `cargo build --release --bin rustock-cli`
does not enable `smtp`, which the installed binary had. The node came up
at 21:17 with peg-out alerting disabled and said so in its log. Rebuilt
with `--features smtp` and restarted at 21:22:50; the log now shows the
watcher started. CI builds with the feature; a plain local build does
not, and `rustock-deployctl` copies whatever is in `target/release`.

### First hours

Caught up 2,086 blocks in about 20 seconds and entered follow mode, so
the node was roughly 17 hours behind — one block every 30 s on mainnet.
There was no long catch-up to observe; the batch path was exercised for
seconds, not minutes.

### Before phase 0

The node was stopped **2026-10-20 03:39:25 -03** for the parallel
execution benchmarks, after 17 min 25 s of uptime. Series before that
point are the old binary on RocksDB.

---

## Phase 1 — FlatDB — NOT STARTED

Blocked on two things this account cannot do:

1. **The systemd unit must change.** `--trie-engine flatdb` and the new
   `--trie-dir` are `ExecStart` arguments and the unit is root-owned;
   available sudo covers `rustockctl {start|stop|restart}` and
   `rustock-deployctl install` only.
2. **The converted store should move to the same disk.** It is currently
   at `/mnt/import/paseo/flatdb-mainnet-trial/trie-epochs` on `sdb`,
   while the live store is on `sdc`. Comparing across different physical
   disks would confound the result. `/var/lib/rustock` has 87 GB free and
   the store needs about 3.4 GB.

The store itself is ready: converted and verified on 2026-10-20 — 3
epochs, 25,371,307 nodes copied with none skipped, 10,519,952 resolved by
walking from the executed state root, and every long record read back
through the verifying path. See `docs/flatdb.md`.

An untouched copy of the original is at
`/mnt/import/paseo/flatdb-mainnet-trial/trie-epochs.backup`, on a
different physical disk from the live store.
