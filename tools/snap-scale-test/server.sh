#!/bin/bash
# One frozen snapshot-sync server, for scaling measurements.
#
#   ./server.sh <index> [data-dir] [simulate-height]
#
# Several of these run against the SAME database at once. Two things make that
# work and both are easy to get wrong:
#
#   --read-only      several processes may open one database; none may write
#                    it. Also what makes the server a fixed fixture -- see
#                    docs/snap-sync-test-method.md.
#   --secret-key     the node identity lives in <data-dir>/node.key, so servers
#                    sharing a database otherwise present the SAME node id and
#                    a client treats them all as one peer. The key is derived
#                    from the index so a rerun reproduces the same identities.
#
# Ports: listen 30300+10*index, discovery is listen+1, RPC 4440+index.
set -euo pipefail
IDX="${1:?usage: server.sh <index> [data-dir] [simulate-height]}"
DATA="${2:-/var/lib/rustock}"
HEIGHT="${3:-9296448}"
BIN="${RUSTOCK_BIN:-/srv/rustock/target/release/rustock-cli}"

PORT=$((30300 + 10 * IDX))
RPC=$((4440 + IDX))
# Deterministic per-index key: reruns get the same identities.
KEY=$(printf 'rustock-scale-test-server-%02d' "$IDX" | sha256sum | cut -d' ' -f1)

echo "server $IDX: port $PORT (discovery $((PORT+1))), rpc $RPC, height $HEIGHT"
exec "$BIN" \
  --data-dir "$DATA" \
  --trie-backend epoch --trie-dir "$DATA/trie-epochs" \
  --read-only --follow-up false \
  --simulate-height "$HEIGHT" \
  --port "$PORT" --rpc-port "$RPC" \
  --secret-key "$KEY" \
  --snap-server \
  --log-to-stdout
