#!/usr/bin/env bash
# Build release and copy navigator.exe to the personal bin dir as x.exe.
# Usage: ./r.sh from Git Bash/WSL, or .\r.cmd from PowerShell/cmd.
set -euo pipefail

root=$(cd "$(dirname "$0")" && pwd)
cd "$root"

cargo build --release

src="target/release/navigator.exe"
dst="C:/Users/Nitropc/stuff/bin/x.exe"

mkdir -p "$(dirname "$dst")"
cp -f "$src" "$dst"
echo "copied $src -> $dst"
