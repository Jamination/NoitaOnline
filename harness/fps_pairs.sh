#!/usr/bin/env bash
# FPS overhead measurement: N pairs of a 1-player session and the vanilla single-player
# baseline, run one at a time on the same pinned cores, alternating which goes first
# (MP solo, solo MP, MP solo, ...) so drift in machine state (temperature, background load)
# hits both sides alike. Must run inside a desktop session, like e2e.sh.
# Solo runs leave the scripted trajectory on their own (vanilla is not deterministic); a
# session replays one trajectory per seed. So that both sides sample the scenario alike, MP
# run k takes its own deterministic trajectory: the bot gets one extra frame of "Right" at
# frame 400 + 50*(k-1) (NOITA_ONLINE_LADDER=nudge<N>). --same-trajectory turns that off.
#   harness/fps_pairs.sh --pairs 3 --seed 98765432 --frames 3600 --out run/e2e/fps
# Results: <out>/mp_<k>, <out>/solo_<k>; then
#   harness/evaluate.py --fps-only --fps-run <out>/mp_* --baseline <out>/solo_*
set -uo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PAIRS=3
SEED=98765432
FRAMES=3600
OUT=run/e2e/fps
VARY=1
while [[ $# -gt 0 ]]; do
  case "$1" in
    --pairs) PAIRS="$2"; shift 2 ;;
    --seed) SEED="$2"; shift 2 ;;
    --frames) FRAMES="$2"; shift 2 ;;
    --out) OUT="$2"; shift 2 ;;
    --same-trajectory) VARY=0; shift ;;
    *) echo "unknown arg $1" >&2; exit 2 ;;
  esac
done
[[ "$OUT" = /* ]] || OUT="$ROOT/$OUT"
mkdir -p "$OUT"
run() {
  local kind="$1" k="$2"
  local args=(--pin --players 1 --frames "$FRAMES" --seed "$SEED" --out "$OUT/${kind}_$k")
  [[ "$kind" == solo ]] && args+=(--solo)
  echo "[fps_pairs $(date +%T)] ${kind}_$k" | tee -a "$OUT/pairs.log"
  if [[ "$kind" == mp && "$VARY" == 1 ]]; then
    NOITA_ONLINE_LADDER="nudge$((400 + 50 * (k - 1)))" "$ROOT/harness/e2e.sh" "${args[@]}" >/dev/null 2>&1
  else
    "$ROOT/harness/e2e.sh" "${args[@]}" >/dev/null 2>&1
  fi
  sleep 3
}
for ((k = 1; k <= PAIRS; k++)); do
  if (( k % 2 )); then run mp "$k"; run solo "$k"; else run solo "$k"; run mp "$k"; fi
done
echo "[fps_pairs $(date +%T)] done" | tee -a "$OUT/pairs.log"
echo done >"$OUT/done"
