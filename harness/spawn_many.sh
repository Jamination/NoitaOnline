#!/usr/bin/env bash
# Launch N isolated Noita instances (ids FIRST..FIRST+N-1, FIRST from $NOITA_FIRST_ID or 0)
# in the current desktop, staggered.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
N="${1:?count}"; shift
ARGS=("$@")
[[ ${#ARGS[@]} -gt 0 ]] || ARGS=(-no_logo_splashes -gamemode 0)
FIRST="${NOITA_FIRST_ID:-0}"
for ((i = FIRST; i < FIRST + N; i++)); do
  setsid "$ROOT/harness/launch_noita.sh" "$i" "${ARGS[@]}" </dev/null >/dev/null 2>&1 &
  sleep "${NOITA_STAGGER:-4}"
done
