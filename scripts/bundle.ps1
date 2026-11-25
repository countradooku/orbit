<#
.SYNOPSIS
  Builds Orbit in release mode and packages it for Windows:
  dist/orbit-<version>-<target>.zip with orbit.exe, orbitd.exe and README.md.

.EXAMPLE
  ./scripts/bundle.ps1
  ./scripts/bundle.ps1 -Target aarch64-pc-windows-msvc -Out dist
#>
param(
    [string] $Target = "",
    [string] $Out = "",
    [switch] $NoBuild
)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
Set-Location $root

if (-not $Target) {
    $Target = (rustc -vV | Select-String '^host: (.+)$').Matches[0].Groups[1].Value.Trim()
}
if (-not $Out) { $Out = Join-Path $root 'dist' }
$version = (Select-String -Path Cargo.toml -Pattern '^version = "(.+)"' | Select-Object -First 1).Matches[0].Groups[1].Value
# Honors CARGO_TARGET_DIR and .cargo/config.toml.
$targetDir = (cargo metadata --format-version 1 --no-deps | ConvertFrom-Json).target_directory
$bin = Join-Path $targetDir "$Target/release"

if (-not $NoBuild) {
    cargo build --release --locked --target $Target -p orbit-app --bin orbit -p orbitd --bin orbitd
    if ($LASTEXITCODE -ne 0) { throw "cargo build failed ($LASTEXITCODE)" }
}

$name = "orbit-$version-$Target"
$stage = Join-Path ([System.IO.Path]::GetTempPath()) ([System.Guid]::NewGuid().ToString())
$dir = Join-Path $stage $name
New-Item -ItemType Directory -Force $dir | Out-Null
New-Item -ItemType Directory -Force $Out | Out-Null
try {
    Copy-Item (Join-Path $bin 'orbit.exe'), (Join-Path $bin 'orbitd.exe'), README.md $dir
    $archive = Join-Path (Resolve-Path $Out) "$name.zip"
    if (Test-Path $archive) { Remove-Item $archive -Confirm:$false }
    Compress-Archive -Path $dir -DestinationPath $archive
    $archive
} finally {
    Remove-Item -Recurse -Force $stage -Confirm:$false
}
