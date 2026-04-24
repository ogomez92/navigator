# Build a release binary and copy it to the user's personal bin dir so the
# last successful release is always reachable from PATH as `x`.
#
# Invocation:
#   powershell -ExecutionPolicy Bypass -File scripts\release.ps1
#
# Exits non-zero if `cargo build --release` fails (no copy happens).
$ErrorActionPreference = 'Stop'

$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

Write-Host "Building release..."
cargo build --release
if ($LASTEXITCODE -ne 0) {
    throw "cargo build --release failed (exit $LASTEXITCODE)"
}

$src = Join-Path $root 'target\release\navigator.exe'
$dstDir = 'C:\Users\Nitropc\stuff\bin'
$dst = Join-Path $dstDir 'x.exe'

if (-not (Test-Path $dstDir)) {
    New-Item -ItemType Directory -Path $dstDir -Force | Out-Null
}

Copy-Item -Path $src -Destination $dst -Force
Write-Host "Copied $src -> $dst"
