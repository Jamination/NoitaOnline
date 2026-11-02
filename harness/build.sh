#!/usr/bin/env bash
# Build everything: Linux server/tools, Windows (x86_64) server cross-build, and the 32-bit
# Windows client DLL that Noita loads (copied into the mod's bin/ folder).
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
cargo build --release -p noita-online-server -p simclient
cargo build --release -p noita-online-server --target x86_64-pc-windows-gnu
cargo build --release -p noita_online_client --target i686-pc-windows-gnu
mkdir -p mod/noita_online/bin
cp target/i686-pc-windows-gnu/release/noita_online.dll mod/noita_online/bin/noita_online.dll
echo "built:"
ls -la target/release/noita-online-server target/x86_64-pc-windows-gnu/release/noita-online-server.exe mod/noita_online/bin/noita_online.dll
