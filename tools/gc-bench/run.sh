#!/bin/bash
# Both backends, same workload, sequentially so they do not compete for I/O.
B=/srv/rustock-gc/target/release/examples/gc_bench
BLOCKS=2000
SLOTS=1000
rm -rf /srv/gc-bench/single /srv/gc-bench/epoch
echo "=================== SINGLE (no collector) ==================="
$B --backend single --dir /srv/gc-bench/single --blocks $BLOCKS --slots $SLOTS --report-every 200
echo
echo "=================== EPOCH (collector on) ===================="
$B --backend epoch --dir /srv/gc-bench/epoch --blocks $BLOCKS --slots $SLOTS \
   --epochs 4 --rotate-mb 128 --burial 100 --report-every 200
echo "BENCH_DONE"
