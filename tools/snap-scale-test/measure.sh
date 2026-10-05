#!/bin/bash
# One steady-state measurement of the ascending header sync.
#
#   ./measure.sh <n-servers> [settle-secs] [window-secs]
#
# Starts a client from a WIPED data directory, waits `settle` for it to connect
# and get past start-up, then measures blocks/s over `window`.
#
# Settle matters. At 90s a run occasionally began before every server had
# connected, and measured one peer while believing it had three -- which is a
# 973 blocks/s reading among 3,549 and 3,796. 150s is the default for that
# reason; the script also refuses a window whose peer count is wrong.
set -euo pipefail
N="${1:?usage: measure.sh <n-servers> [settle] [window]}"
SETTLE="${2:-150}"
WINDOW="${3:-180}"
WORK="${SCALE_WORK:-/mnt/import/scale}"
BIN="${RUSTOCK_CLIENT_BIN:-/srv/rustock/target/release/rustock-cli}"

BOOT=""
for i in $(seq 1 "$N"); do BOOT="$BOOT --bootnodes 127.0.0.1:$((30300 + 10 * i + 1))"; done

rm -rf "$WORK/data" "$WORK/trie"; mkdir -p "$WORK"
setsid nohup "$BIN" \
  --data-dir "$WORK/data" --trie-backend epoch --trie-dir "$WORK/trie" \
  --port 30290 --rpc-port 4439 \
  $BOOT --closed-network --max-peers "$N" \
  --snap-sync --snap-forward-headers \
  --log-to-stdout > "$WORK/run.log" 2>&1 < /dev/null &

sleep "$SETTLE"
A=$(grep "ascending the header chain:" "$WORK/run.log" | tail -1)
sleep "$WINDOW"
B=$(grep "ascending the header chain:" "$WORK/run.log" | tail -1)
PEERS=$(curl -s -m 5 -X POST -H 'Content-Type: application/json' \
  --data '{"jsonrpc":"2.0","id":1,"method":"net_peerCount","params":[]}' \
  http://127.0.0.1:4439 2>/dev/null | grep -o '0x[0-9a-f]*' || echo "0x0")

PID=$(ps -eo pid,cmd | grep "[-]-data-dir $WORK/data" | awk '{print $1}' | head -1)
[ -n "$PID" ] && kill "$PID" 2>/dev/null || true

python3 - "$N" "$A" "$B" "$PEERS" <<'PY'
import sys, re
n, a, b, peers = sys.argv[1], sys.argv[2], sys.argv[3], sys.argv[4]
def at(l):
    m = re.search(r'at #(\d+) of.*?(\d+)s elapsed', l)
    return (int(m.group(1)), int(m.group(2))) if m else None
A, B = at(a), at(b)
if not A or not B:
    print(f"{n} server(s): no progress lines -- did the client connect?"); sys.exit(1)
dh, dt = B[0] - A[0], B[1] - A[1]
want = int(n)
got = int(peers, 16)
flag = "" if got == want else f"  ** peers={got}, expected {want}: DISCARD **"
print(f"{n} server(s)  peers={got}  {dh:>9,} blocks in {dt:>4}s  =  {dh/dt:>7,.0f} blocks/s{flag}")
PY
for _ in $(seq 1 20); do ps -eo cmd | grep -q "[-]-data-dir $WORK/data" || break; sleep 3; done
