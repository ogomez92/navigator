#!/usr/bin/env bash
# Build release and copy navigator.exe to a personal bin dir as x.exe, then
# sync navigator_sounds/ next to it (the app reads <exe_dir>/navigator_sounds).
# Override the destination with NAVIGATOR_INSTALL; defaults to ~/stuff/bin/x.exe.
# Usage: ./r.sh from Git Bash/WSL, or .\r.cmd from PowerShell/cmd.
set -euo pipefail

root=$(cd "$(dirname "$0")" && pwd)
cd "$root"

cargo build --release

src="target/release/navigator.exe"
dst="${NAVIGATOR_INSTALL:-$HOME/stuff/bin/x.exe}"

mkdir -p "$(dirname "$dst")"
cp -f "$src" "$dst"
echo "copied $src -> $dst"

# Event sounds live in <exe_dir>/navigator_sounds. sync (not copy) so a file
# removed from the repo folder also disappears from the install.
sounds_dst="$(dirname "$dst")/navigator_sounds"
rclone sync navigator_sounds "$sounds_dst"
echo "synced navigator_sounds -> $sounds_dst"
