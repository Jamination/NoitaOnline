#!/usr/bin/env bash
# Stop Noita instances running in project-owned prefixes (never touches the user's prefix).
# Usage: kill_all.sh            # every run/prefix*
#        kill_all.sh 10 11      # only the given instance ids
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
WS="${STEAM_ROOT:-/home/james/.local/share/Steam}/steamapps/common/Proton - Experimental/files/bin/wineserver"
if [[ $# -gt 0 ]]; then
  prefixes=()
  for id in "$@"; do prefixes+=("$ROOT/run/prefix$id/pfx"); done
else
  prefixes=("$ROOT"/run/prefix*/pfx)
fi
for p in "${prefixes[@]}"; do
  [[ -d "$p" ]] && WINEPREFIX="$p" "$WS" -k 2>/dev/null
done
exit 0
