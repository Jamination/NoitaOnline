#!/usr/bin/env bash
# Open the analysed noita.exe project in Ghidra (with the user's GhidraMCP extension) using a
# throwaway copy of the Ghidra settings, so the user's own Ghidra config is never modified.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
GH="$ROOT/run/ghidra-home"
mkdir -p "$GH/.config"
rm -rf "$GH/.config/ghidra"
cp -r /home/james/.config/ghidra "$GH/.config/ghidra"
export HOME="$GH"
exec /home/james/.local/bin/ghidra "$ROOT/run/ghidra/NoitaProj.gpr" >"$ROOT/run/logs/ghidra_gui.log" 2>&1
