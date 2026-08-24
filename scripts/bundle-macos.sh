#!/usr/bin/env bash
# bundle-macos.sh — Build a macOS .app bundle for ItsJustCAD (unsigned).
#
# Usage:  ./scripts/bundle-macos.sh [--release]
#   --release   build with `cargo build --release` (default: debug)
#
# Output: dist/ItsJustCAD.app
#
# NOTE: The resulting bundle is unsigned and unnotarised.
#       To distribute outside the developer machine you need:
#         codesign --deep --force --sign "Developer ID Application: ..." dist/ItsJustCAD.app
#         xcrun notarytool submit ...
#       Those steps require Apple developer credentials not included here.

set -eo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
DIST="$REPO_ROOT/dist"
APP="$DIST/ItsJustCAD.app"
CONTENTS="$APP/Contents"
MACOS="$CONTENTS/MacOS"
RESOURCES="$CONTENTS/Resources"

# ── Parse flags ─────────────────────────────────────────────────────────────
PROFILE="debug"
RELEASE_FLAG=""
for arg in "$@"; do
  case "$arg" in
    --release)
      PROFILE="release"
      RELEASE_FLAG="--release"
      ;;
    *)
      echo "Unknown flag: $arg" >&2
      exit 1
      ;;
  esac
done

echo "==> Building ItsJustCAD ($PROFILE)…"
cd "$REPO_ROOT"
# shellcheck disable=SC2086
cargo build -p itsjustcad $RELEASE_FLAG

BINARY="$REPO_ROOT/target/$PROFILE/itsjustcad"
if [[ ! -f "$BINARY" ]]; then
  echo "ERROR: binary not found at $BINARY" >&2
  exit 1
fi

# ── Assemble .app skeleton ───────────────────────────────────────────────────
echo "==> Assembling $APP"
rm -rf "$APP"
mkdir -p "$MACOS" "$RESOURCES"

# Info.plist
BUNDLE_ID="com.itsjustcad.app"
VERSION="0.1.0"
cat > "$CONTENTS/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN"
    "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key>
    <string>ItsJustCAD</string>
    <key>CFBundleDisplayName</key>
    <string>ItsJustCAD</string>
    <key>CFBundleIdentifier</key>
    <string>${BUNDLE_ID}</string>
    <key>CFBundleVersion</key>
    <string>${VERSION}</string>
    <key>CFBundleShortVersionString</key>
    <string>${VERSION}</string>
    <key>CFBundleExecutable</key>
    <string>ItsJustCAD</string>
    <key>CFBundleIconFile</key>
    <string>icon</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>CFBundleSignature</key>
    <string>????</string>
    <key>NSHighResolutionCapable</key>
    <true/>
    <key>NSSupportsAutomaticGraphicsSwitching</key>
    <true/>
    <key>LSMinimumSystemVersion</key>
    <string>12.0</string>
</dict>
</plist>
PLIST

# Binary
cp "$BINARY" "$MACOS/ItsJustCAD"
chmod +x "$MACOS/ItsJustCAD"

# Icon (icns)
ICNS="$REPO_ROOT/assets/icon/icon.icns"
if [[ -f "$ICNS" ]]; then
  cp "$ICNS" "$RESOURCES/icon.icns"
else
  echo "WARN: $ICNS not found — bundle will have no icon."
  echo "      Run: cargo run -p itsjustcad --example gen_icon"
  echo "      Then re-run this script."
fi

echo ""
echo "==> Done: $APP"
echo ""
echo "Launch:  open $APP"
echo "NOTE:    bundle is unsigned — Gatekeeper will block it on other machines."
echo "         See scripts/bundle-macos.sh header for signing instructions."
