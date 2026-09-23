#!/bin/bash
# Corrected experiment: both backends configured identically (bloom filters,
# 256 MB block cache, 64 MB write buffers). Epoch additionally shares one cache
# across epochs, caps background jobs at 1 per epoch, and no longer walks the
# directory to test the rotation trigger.
B=/srv/rustock-gc/target/release/examples
BLOCKS=800
SLOTS=1000
rm -rf /srv/gc-bench/s2 /srv/gc-bench/e2
echo "### E1 SINGLE fixed-config ###"
$B/gc_bench --backend single --dir /srv/gc-bench/s2 --blocks $BLOCKS --slots $SLOTS --report-every 200
echo
echo "### E2 EPOCH fixed-config ###"
$B/gc_bench --backend epoch --dir /srv/gc-bench/e2 --blocks $BLOCKS --slots $SLOTS \
   --epochs 4 --rotate-mb 128 --burial 100 --report-every 200
echo
echo "### E3 READ single ###"
$B/read_bench single /srv/gc-bench/s2 200000
echo "### E4 READ epoch ###"
$B/read_bench epoch /srv/gc-bench/e2 200000
echo "RUN2_DONE"
