#!/usr/bin/env bash
# Configure a project-owned Noita save dir for automated multiplayer runs.
# Usage: prep_prefix.sh <Nolla_Games_Noita dir>
set -euo pipefail
SAVE="${1:?save dir}"
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
case "$(realpath -m "$SAVE")" in
  "$ROOT"/run/*) ;;
  *) echo "refusing to touch non-project save dir: $SAVE" >&2; exit 1 ;;
esac
CFG="$SAVE/save_shared/config.xml"
set_attr() { # file attr value
  if grep -q "$2=\"" "$1"; then
    sed -i -E "s/([[:space:]])$2=\"[^\"]*\"/\1$2=\"$3\"/" "$1"
  else
    sed -i -E "0,/<Config/s//<Config $2=\"$3\"/" "$1"
  fi
}
mkdir -p "$SAVE/save_shared"
[[ -f "$CFG" ]] || cp "$(dirname "${BASH_SOURCE[0]}")/config_template.xml" "$CFG"
NOITA_DIR="${NOITA_DIR:-/home/james/.local/share/Steam/steamapps/common/Noita}"
{
  set_attr "$CFG" last_started_game_version_hash "$(cat "$NOITA_DIR/_version_hash.txt")"
  set_attr "$CFG" application_pause_when_unfocused 0
  set_attr "$CFG" mods_sandbox_enabled 0
  set_attr "$CFG" mods_sandbox_warning_done 1
  set_attr "$CFG" mods_disclaimer_accepted 1
  set_attr "$CFG" fullscreen 0
  set_attr "$CFG" window_w "${NOITA_WINDOW_W:-640}"
  set_attr "$CFG" window_h "${NOITA_WINDOW_H:-360}"
  set_attr "$CFG" backbuffer_width "${NOITA_WINDOW_W:-640}"
  set_attr "$CFG" backbuffer_height "${NOITA_WINDOW_H:-360}"
  set_attr "$CFG" check_for_updates 0
  set_attr "$CFG" vsync 0
  set_attr "$CFG" audio_fmod 0
  set_attr "$CFG" report_fps 1
  set_attr "$CFG" single_threaded_loading 1
}
# Every run starts from an identical profile (project-owned prefix only): blank, or a copy
# of NOITA_SAVE_TEMPLATE (a save00 directory) when set.
rm -rf "$SAVE/save00"
if [[ -n "${NOITA_SAVE_TEMPLATE:-}" ]]; then
  cp -a "$NOITA_SAVE_TEMPLATE" "$SAVE/save00"
else
  mkdir -p "$SAVE/save00"
fi
# NOITA_MODS: space-separated mod folder names to enable (default: noita_online).
{
  echo "<Mods>"
  for m in ${NOITA_MODS:-noita_online}; do
    echo "  <Mod enabled=\"1\" name=\"$m\" settings_fold_open=\"0\" workshop_item_id=\"0\" >"
    echo "  </Mod>"
  done
  echo "</Mods>"
} >"$SAVE/save00/mod_config.xml"
