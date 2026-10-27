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

# The exe loads prism.dll from its own folder, and that folder is shared
# with other prism users. Ship the one this build was linked against: a
# different PrismConfig layout corrupts memory rather than failing.
# Another prism user (noty) may be running and holding the DLL open, in
# which case the copy fails — fine when what is there is already identical.
prism_dst="$(dirname "$dst")/prism.dll"
if cp -f target/release/prism.dll "$prism_dst" 2>/dev/null; then
  echo "copied prism.dll -> $prism_dst"
elif cmp -s target/release/prism.dll "$prism_dst"; then
  echo "prism.dll in use but already current"
else
  echo "prism.dll is in use and differs from the one this build needs; close the program using it and rerun" >&2
  exit 1
fi

# Event sounds live in <exe_dir>/navigator_sounds. sync (not copy) so a file
# removed from the repo folder also disappears from the install.
sounds_dst="$(dirname "$dst")/navigator_sounds"
rclone sync navigator_sounds "$sounds_dst"
echo "synced navigator_sounds -> $sounds_dst"
