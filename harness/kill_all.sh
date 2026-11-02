#!/usr/bin/env bash
# Stop Noita instances running in project-owned prefixes (never touches the user's prefix).
# Usage: kill_all.sh 10 11      # only the given instance ids
#        kill_all.sh --all      # every run/prefix* (also other agents' / runs' instances)
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
WS="${STEAM_ROOT:-/home/james/.local/share/Steam}/steamapps/common/Proton - Experimental/files/bin/wineserver"
if [[ $# -eq 0 ]]; then
  echo "usage: kill_all.sh <id>... | --all" >&2
  exit 2
elif [[ "$1" == --all ]]; then
  prefixes=("$ROOT"/run/prefix*/pfx)
else
  prefixes=()
  for id in "$@"; do prefixes+=("$ROOT/run/prefix$id/pfx"); done
fi
for p in "${prefixes[@]}"; do
  [[ -d "$p" ]] && WINEPREFIX="$p" "$WS" -k 2>/dev/null
done
exit 0
