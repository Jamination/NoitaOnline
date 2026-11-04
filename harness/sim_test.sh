#!/usr/bin/env bash
# Server + headless simulated clients (no Noita): exercises lockstep relay, verification,
# bandwidth and CPU accounting. Usage: sim_test.sh [bots] [frames] [netsim-spec]
# RUNS=N: N runs; every run but the last ends with each bot leaving it (NextRun) at frame
# frames + 60 * bot index, as after an all-dead game over.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BOTS="${1:-4}"; FRAMES="${2:-1800}"; NETSIM="${3:-}"
OUT="$ROOT/run/simtest"
PORT="${PORT:-41800}"
rm -rf "$OUT"; mkdir -p "$OUT"
SRV_ARGS=(--bind "127.0.0.1:$PORT" --players "$BOTS" --log-dir "$OUT" --exit-when-empty)
[[ -n "$NETSIM" ]] && SRV_ARGS+=(--netsim "$NETSIM")
"$ROOT/target/release/noita-online-server" "${SRV_ARGS[@]}" >"$OUT/server.out" 2>&1 &
SRV=$!
trap 'kill $SRV 2>/dev/null || true' EXIT
sleep 0.3
"$ROOT/target/release/simclient" --server "127.0.0.1:$PORT" --bots "$BOTS" --frames "$FRAMES" --runs "${RUNS:-1}" | tee "$OUT/simclient.out"
for _ in $(seq 50); do kill -0 $SRV 2>/dev/null || break; sleep 0.2; done
cat "$OUT/summary.json"
