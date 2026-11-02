#!/usr/bin/env bash
# End-to-end session: headless server + N real Noita clients (isolated Proton prefixes)
# running the scripted bot scenario. Must be started inside the agent's cua desktop
# (it launches GUI processes), e.g. via launch_app:
#   harness/e2e.sh --players 4 --frames 36000 --out run/e2e/full
# Solo baseline (same scenario, no server, each client single-player):
#   harness/e2e.sh --solo --players 4 --frames 36000 --out run/e2e/baseline
# Writes <out>/done when finished; results: <out>/summary.json, frames_<id>.txt, client_<id>.log.
set -uo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PLAYERS=4
FRAMES=36000
NETSIM=""
OUT=""
SOLO=0
FIRST="${NOITA_FIRST_ID:-0}"
PORT=41773
TIMEOUT_S=""
SEED=""
PIN=0
while [[ $# -gt 0 ]]; do
  case "$1" in
    --players) PLAYERS="$2"; shift 2 ;;
    --frames) FRAMES="$2"; shift 2 ;;
    --netsim) NETSIM="$2"; shift 2 ;;
    --out) OUT="$2"; shift 2 ;;
    --solo) SOLO=1; shift ;;
    --first-id) FIRST="$2"; shift 2 ;;
    --port) PORT="$2"; shift 2 ;;
    --timeout) TIMEOUT_S="$2"; shift 2 ;;
    --seed) SEED="$2"; shift 2 ;;
    --pin) PIN=1; shift ;;
    --human) HUMAN=1; shift ;;
    --scenario) SCENARIO_ARG="$2"; shift 2 ;;
    --fps-cap) FPS_CAP="$2"; shift 2 ;;
    *) echo "unknown arg $1" >&2; exit 2 ;;
  esac
done
[[ -n "$OUT" ]] || OUT="run/e2e/$(date +%Y%m%d-%H%M%S)"
[[ "$OUT" = /* ]] || OUT="$ROOT/$OUT"
rm -rf "$OUT"; mkdir -p "$OUT"
# Game frames run slower than real time under load; allow generous wall time.
[[ -n "$TIMEOUT_S" ]] || TIMEOUT_S=$(( FRAMES / 10 + 600 ))
IDS=()
for ((i = FIRST; i < FIRST + PLAYERS; i++)); do IDS+=("$i"); done
log() { echo "[e2e $(date +%T)] $*" | tee -a "$OUT/e2e.log"; }

cleanup() {
  "$ROOT/harness/kill_all.sh" "${IDS[@]}"
  [[ -n "${SRV:-}" ]] && kill "$SRV" 2>/dev/null
}
trap cleanup EXIT

export NOITA_MODS=noita_online
SCENARIO=bot
if [[ "${HUMAN:-0}" == 1 ]]; then
  # Manual play: real input via the proxy, no scripted events (unless --scenario, e.g.
  # "items"), no auto-exit.
  SCENARIO="${SCENARIO_ARG:-}"
  export NOITA_ONLINE_BOT=0 NOITA_ONLINE_DEBUG=1
else
  export NOITA_ONLINE_BOT=1
  export NOITA_ONLINE_DURATION="$FRAMES"
  export NOITA_ONLINE_EXIT=1
fi
export NOITA_ONLINE_OUT="$OUT"
# Same frame cap (vsync stand-in) for baseline and multiplayer runs; 0 = uncapped.
[[ "${FPS_CAP:-60}" != 0 ]] && export NOITA_ONLINE_FPS_CAP="${FPS_CAP:-60}"
[[ -n "$SEED" ]] && export NOITA_ONLINE_SOLO_SEED="$SEED"

if [[ "$SOLO" == 1 ]]; then
  export NOITA_ONLINE_SOLO=1
  log "solo baseline: ${PLAYERS} instances x ${FRAMES} frames"
else
  SRV_ARGS=(--bind "127.0.0.1:$PORT" --players "$PLAYERS" --log-dir "$OUT" --exit-when-empty --scenario "$SCENARIO"
            --game-build "noita-2025-01-25-6794ee3c")
  [[ -n "$NETSIM" ]] && SRV_ARGS+=(--netsim "$NETSIM")
  [[ -n "$SEED" ]] && SRV_ARGS+=(--seed "$SEED")
  "$ROOT/target/release/noita-online-server" "${SRV_ARGS[@]}" >"$OUT/server.out" 2>&1 &
  SRV=$!
  export NOITA_ONLINE_SERVER="127.0.0.1:$PORT"
  log "server pid $SRV; ${PLAYERS} clients x ${FRAMES} frames; netsim='${NETSIM}'"
fi

k=0
for id in "${IDS[@]}"; do
  # Without --pin, NOITA_CPUSET (if set) confines every client to those CPUs.
  CPUSET="${NOITA_CPUSET:-}"
  # --pin: client k gets physical core PIN_BASE+k (both SMT threads; default base 2); cores
  # 0-1 stay free for the X server, wineservers and the game server. Concurrent harness
  # runs use different NOITA_PIN_BASE values so they never share cores.
  BASE="${NOITA_PIN_BASE:-2}"
  if [[ "$PIN" == 1 && $((BASE + k)) -lt 6 ]]; then CPUSET="$((BASE + k)),$((BASE + 6 + k))"; fi
  NOITA_CPUSET="$CPUSET" NOITA_ONLINE_NAME="p$id" setsid "$ROOT/harness/launch_noita.sh" "$id" -no_logo_splashes -gamemode 0 </dev/null >/dev/null 2>&1 &
  k=$((k + 1))
  sleep "${NOITA_STAGGER:-4}"
done
log "clients launched: ${IDS[*]}"

deadline=$(( $(date +%s) + TIMEOUT_S ))
status="timeout"
while (( $(date +%s) < deadline )); do
  if [[ "$SOLO" == 0 ]]; then
    if ! kill -0 "$SRV" 2>/dev/null; then status="server-exited"; break; fi
  else
    done_count=0
    for id in "${IDS[@]}"; do
      grep -q "finishing:" "$OUT/client_$id.log" 2>/dev/null && done_count=$((done_count + 1))
    done
    if (( done_count == PLAYERS )); then status="all-clients-finished"; break; fi
  fi
  sleep 5
done
log "end: $status"
sleep 3
echo "$status" >"$OUT/done"
