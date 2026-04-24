#!/usr/bin/env bash
# Build release and copy navigator.exe to the personal bin dir as x.exe.
# Usage: scripts/release.sh
set -euo pipefail

root=$(cd "$(dirname "$0")" && pwd)
cd "$root"

cargo build --release

src="target/release/navigator.exe"
dst="C:/Users/Nitropc/stuff/bin/x.exe"

mkdir -p "$(dirname "$dst")"
cp -f "$src" "$dst"
echo "copied $src -> $dst"
