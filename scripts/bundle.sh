#!/usr/bin/env bash
# Builds Orbit in release mode and packages it for the current OS:
#
#   Linux:  dist/orbit-<version>-<target>.tar.gz   (orbit, orbitd, orbit.desktop)
#   macOS:  dist/orbit-<version>-<target>.zip      (Orbit.app, orbitd inside it)
#
# Usage: scripts/bundle.sh [--target <triple>] [--out <dir>] [--no-build]
#
# On macOS the bundle is signed with $MACOS_SIGN_IDENTITY when it is set, and
# ad-hoc otherwise (enough to run locally; not notarized). Windows uses
# scripts/bundle.ps1.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root"

target=""
out="$root/dist"
build=1
while [[ $# -gt 0 ]]; do
  case "$1" in
    --target) target="$2"; shift 2 ;;
    --out) out="$2"; shift 2 ;;
    --no-build) build=0; shift ;;
    -h|--help) sed -n '2,12p' "$0"; exit 0 ;;
    *) echo "bundle.sh: unknown argument $1" >&2; exit 2 ;;
  esac
done

host="$(rustc -vV | sed -n 's/^host: //p')"
target="${target:-$host}"
version="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n1)"
# Honors CARGO_TARGET_DIR and .cargo/config.toml.
target_dir="$(cargo metadata --format-version 1 --no-deps | sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p')"
bin="$target_dir/$target/release"

if [[ $build == 1 ]]; then
  cargo build --release --locked --target "$target" -p orbit-app --bin orbit -p orbitd --bin orbitd
fi

mkdir -p "$out"
stage="$(mktemp -d)"
trap 'rm -rf "$stage"' EXIT
name="orbit-$version-$target"

case "$target" in
  *-apple-darwin)
    app="$stage/Orbit.app"
    mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
    cp "$bin/orbit" "$bin/orbitd" "$app/Contents/MacOS/"
    cat > "$app/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleDevelopmentRegion</key><string>en</string>
  <key>CFBundleDisplayName</key><string>Orbit</string>
  <key>CFBundleExecutable</key><string>orbit</string>
  <key>CFBundleIdentifier</key><string>dev.orbit.Orbit</string>
  <key>CFBundleInfoDictionaryVersion</key><string>6.0</string>
  <key>CFBundleName</key><string>Orbit</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>$version</string>
  <key>CFBundleVersion</key><string>$version</string>
  <key>LSApplicationCategoryType</key><string>public.app-category.developer-tools</string>
  <key>LSMinimumSystemVersion</key><string>11.0</string>
  <key>NSHighResolutionCapable</key><true/>
  <key>NSSupportsAutomaticGraphicsSwitching</key><true/>
</dict>
</plist>
EOF
    printf 'APPL????' > "$app/Contents/PkgInfo"
    if command -v codesign >/dev/null; then
      if [[ -n "${MACOS_SIGN_IDENTITY:-}" ]]; then
        # Hardened runtime and a timestamp: what notarization requires.
        codesign --force --deep --options runtime --timestamp --sign "$MACOS_SIGN_IDENTITY" "$app"
      else
        codesign --force --deep --sign - "$app"
      fi
    fi
    archive="$out/$name.zip"
    rm -f "$archive"
    if command -v ditto >/dev/null; then
      ditto -c -k --keepParent "$app" "$archive"
    else
      (cd "$stage" && zip -qry "$archive" Orbit.app)
    fi
    ;;
  *-linux-*)
    dir="$stage/$name"
    mkdir -p "$dir"
    cp "$bin/orbit" "$bin/orbitd" README.md "$dir/"
    # StartupWMClass matches the window's app_id, so docks group the window.
    cat > "$dir/orbit.desktop" <<EOF
[Desktop Entry]
Type=Application
Name=Orbit
Comment=A native control surface for coding agents
Exec=orbit
Terminal=false
Categories=Development;
StartupWMClass=dev.orbit.Orbit
EOF
    archive="$out/$name.tar.gz"
    tar -C "$stage" -czf "$archive" "$name"
    ;;
  *)
    echo "bundle.sh: unsupported target $target (use scripts/bundle.ps1 on Windows)" >&2
    exit 2
    ;;
esac

echo "$archive"
