#!/usr/bin/env bash
# Launch one isolated Noita instance under Proton with a project-owned Wine prefix.
# Usage: launch_noita.sh <instance-id> [noita args...]
# Each instance gets run/prefix<id>, so the user's Steam prefix/saves are never touched.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ID="${1:?instance id}"; shift
STEAM="${STEAM_ROOT:-/home/james/.local/share/Steam}"
NOITA="$STEAM/steamapps/common/Noita"
PROTON="$STEAM/steamapps/common/Proton - Experimental/proton"
PREFIX="$ROOT/run/prefix$ID"
LOG="$ROOT/run/logs"
if [[ ! -e "$PREFIX/version" && -e "$ROOT/run/prefix0/version" && "$ID" != 0 ]]; then
  rm -rf "$PREFIX"
  cp -a --reflink=auto "$ROOT/run/prefix0" "$PREFIX"
fi
mkdir -p "$PREFIX" "$LOG"

export STEAM_COMPAT_DATA_PATH="$PREFIX"
export STEAM_COMPAT_CLIENT_INSTALL_PATH="$STEAM"
export SteamAppId=881100 SteamGameId=881100
export WINEDEBUG="${WINEDEBUG:--all}"
# NOITA_STEAM=1: go through Proton's steam.exe wrapper and the running Steam client (needs
# the real ~/.steam). Default: Steam-free — run noita.exe directly in the prefix with a
# project-owned HOME, so Steam's single-session bookkeeping cannot interfere with many
# concurrent test instances.
if [[ "${NOITA_STEAM:-0}" == 1 ]]; then
  export HOME="${NOITA_REAL_HOME:-/home/james}"
  VERB=run
else
  export HOME="$ROOT/run/home$ID"
  mkdir -p "$HOME"
  VERB=runinprefix
  export WINEDLLOVERRIDES="lsteamclient=d;steamclient=d;steamclient64=d${WINEDLLOVERRIDES:+;$WINEDLLOVERRIDES}"
fi
export NOITA_ONLINE_INSTANCE="$ID"
export NOITA_ONLINE_ROOT="$ROOT"

SAVE="$PREFIX/pfx/drive_c/users/steamuser/AppData/LocalLow/Nolla_Games_Noita"
if [[ -d "$SAVE" ]]; then
  "$ROOT/harness/prep_prefix.sh" "$SAVE"
fi

cd "$NOITA"
# NOITA_CPUSET (e.g. "2,8"): pin this instance (and its wineserver) to those CPUs, so
# concurrent test instances behave like separate machines instead of fighting for cores.
PIN=()
[[ -n "${NOITA_CPUSET:-}" ]] && PIN=(taskset -c "$NOITA_CPUSET")
exec "${PIN[@]}" "$PROTON" "$VERB" "$NOITA/noita.exe" "$@" >"$LOG/noita$ID.out" 2>&1
