#!/bin/bash
# The whole sweep: 1..N servers, R repeats each, interleaved.
#
#   ./run-all.sh [max-servers] [repeats]
#
# Interleaved on purpose: any drift in machine load then falls on every group
# rather than on whichever ran last.
set -euo pipefail
MAX="${1:-4}"; REPS="${2:-3}"
HERE="$(cd "$(dirname "$0")" && pwd)"
LOGDIR="${SCALE_WORK:-/mnt/import/scale}"

for i in $(seq 1 "$MAX"); do
  if ! pgrep -f "simulate-height.*--port $((30300 + 10 * i)) " >/dev/null 2>&1; then
    setsid nohup "$HERE/server.sh" "$i" > "$LOGDIR/srv$i.log" 2>&1 < /dev/null &
  fi
done
echo "waiting for servers to open the database..."; sleep 90

for r in $(seq 1 "$REPS"); do
  for n in $(seq 1 "$MAX"); do
    "$HERE/measure.sh" "$n"
  done
done
