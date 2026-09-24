# Trie GC benchmark drivers

The two scripts that produced the measurements in
[`docs/trie-gc-results.md`](../../docs/trie-gc-results.md). They are kept here
because the 3.5 GB of RocksDB stores they generated were deleted on 2026-09-23
and the scripts are 1.5 KB.

| script | what it ran |
|---|---|
| `run.sh` | **Experiment 1** — 2,000 blocks, both backends, default config. Its throughput result is *withdrawn* (four defects, see results §7.6); its size and cycle-cost results stand. |
| `run2.sh` | **Experiment 2** — 800 blocks, both backends configured identically (bloom filters, 256 MB block cache, 64 MB write buffers), plus read benchmarks. This is the basis for the conclusions. |

## Rebuilding

Both invoke `gc_bench` and `read_bench`, which live in this repository as
`crates/cli/examples/gc_bench.rs` and `read_bench.rs`:

```sh
cargo build --release --example gc_bench --example read_bench -p rustock-cli
```

The scripts point at `/srv/rustock-gc/target/...`, a separate worktree that may
no longer exist. Repoint `$B` at `target/release/examples` in this tree.

Each run writes several hundred MB to `/srv/gc-bench`; that path is not created
by the scripts beyond `rm -rf` of the previous run's output.

## The result they produced

Experiment 2, 800 blocks, both backends identically configured:

| Metric | Single database | Epoch collector | Ratio |
|---|---|---|---|
| Final store size | 659.3 MB | 527.7 MB | **0.80x** |
| Wall-clock time | 2,693.6 s | 2,633.0 s | 0.98x |
| Random reads, warm | 83,609 /s | 159,580 /s | **1.91x** |
| Random reads, cold | 20,287 /s | 21,748 /s | 1.07x |
| Space reclaimed | 0 MB | 146.9 MB | — |

Over Experiment 1's longer 2,000-block run the size advantage reached **4.1x**
(457.7 MB against 1,889.9 MB).

Full analysis, including the four defects that produced Experiment 1's
withdrawn 31% throughput penalty, is in `docs/trie-gc-results.md`.
