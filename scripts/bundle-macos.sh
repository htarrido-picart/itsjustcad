#!/usr/bin/env bash
# bundle-macos.sh — Build a macOS .app bundle for ItsJustCAD (ad-hoc signed).
#
# Usage:  ./scripts/bundle-macos.sh [--release]
#   --release   build with `cargo build --release` (default: debug)
#
# Output: dist/ItsJustCAD.app
#
# NOTE: The bundle is ad-hoc signed (codesign --sign -) so Launch Services
#       registers it (and its icon) correctly on the build machine.
#       To distribute outside the developer machine you still need:
#         codesign --deep --force --sign "Developer ID Application: ..." dist/ItsJustCAD.app
#         xcrun notarytool submit ...
#       Those steps require Apple developer credentials not included here.
#
# Test-only environment overrides (used by scripts/tests/test-bundle-macos.sh):
#   ITSJUSTCAD_BUNDLE_BINARY  path to a prebuilt executable; skips `cargo build`
#   ITSJUSTCAD_DIST_DIR       output directory (default: <repo>/dist)
#   ITSJUSTCAD_ACTOOL         actool binary to use (set to a bogus path to
#                             simulate a machine without Xcode's actool)

set -eo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
DIST="${ITSJUSTCAD_DIST_DIR:-$REPO_ROOT/dist}"
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

if [[ -n "${ITSJUSTCAD_BUNDLE_BINARY:-}" ]]; then
  BINARY="$ITSJUSTCAD_BUNDLE_BINARY"
  echo "==> Using prebuilt binary: $BINARY"
else
  echo "==> Building ItsJustCAD ($PROFILE)…"
  cd "$REPO_ROOT"
  # shellcheck disable=SC2086
  cargo build -p itsjustcad $RELEASE_FLAG
  BINARY="$REPO_ROOT/target/$PROFILE/itsjustcad"
fi

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
VERSION="0.8.0"
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

# Icon (icns) — legacy CFBundleIconFile path, works everywhere.
ICON_DIR="$REPO_ROOT/assets/icon"
ICNS="$ICON_DIR/icon.icns"
if [[ -f "$ICNS" ]]; then
  cp "$ICNS" "$RESOURCES/icon.icns"
else
  echo "WARN: $ICNS not found — bundle will have no icon."
  echo "      Run: cargo run -p itsjustcad --example gen_icon"
  echo "      Then re-run this script."
fi

# ── Modern icon path: compiled asset catalog (Assets.car) ────────────────────
# macOS prefers CFBundleIconName + a compiled asset catalog; some setups never
# pick up a bare .icns. Build an AppIcon asset catalog from the icon PNGs and
# compile it with actool. Degrades gracefully (warn + skip) if actool or the
# PNGs are unavailable, so the script never hard-fails where it used to pass.
build_asset_catalog() {
  local actool_bin="${ITSJUSTCAD_ACTOOL:-}"
  if [[ -z "$actool_bin" ]]; then
    actool_bin="$(xcrun --find actool 2>/dev/null || command -v actool || true)"
  fi
  if [[ -z "$actool_bin" || ! -x "$actool_bin" ]]; then
    echo "WARN: actool not found — skipping Assets.car (icon falls back to icon.icns)."
    return 0
  fi

  local pngs=(16 32 64 128 256 512 1024)
  for px in "${pngs[@]}"; do
    if [[ ! -f "$ICON_DIR/$px.png" ]]; then
      echo "WARN: $ICON_DIR/$px.png missing — skipping Assets.car (icon falls back to icon.icns)."
      echo "      Run: cargo run -p itsjustcad --example gen_icon"
      return 0
    fi
  done

  local tmp
  tmp="$(mktemp -d)"
  # shellcheck disable=SC2064
  trap "rm -rf '$tmp'" RETURN

  local iconset="$tmp/Assets.xcassets/AppIcon.appiconset"
  mkdir -p "$iconset"
  cp "$ICON_DIR/16.png" "$ICON_DIR/32.png" "$ICON_DIR/64.png" \
     "$ICON_DIR/128.png" "$ICON_DIR/256.png" "$ICON_DIR/512.png" \
     "$ICON_DIR/1024.png" "$iconset/"

  cat > "$tmp/Assets.xcassets/Contents.json" <<'JSON'
{ "info": { "author": "xcode", "version": 1 } }
JSON

  cat > "$iconset/Contents.json" <<'JSON'
{
  "images": [
    { "idiom": "mac", "size": "16x16",   "scale": "1x", "filename": "16.png" },
    { "idiom": "mac", "size": "16x16",   "scale": "2x", "filename": "32.png" },
    { "idiom": "mac", "size": "32x32",   "scale": "1x", "filename": "32.png" },
    { "idiom": "mac", "size": "32x32",   "scale": "2x", "filename": "64.png" },
    { "idiom": "mac", "size": "128x128", "scale": "1x", "filename": "128.png" },
    { "idiom": "mac", "size": "128x128", "scale": "2x", "filename": "256.png" },
    { "idiom": "mac", "size": "256x256", "scale": "1x", "filename": "256.png" },
    { "idiom": "mac", "size": "256x256", "scale": "2x", "filename": "512.png" },
    { "idiom": "mac", "size": "512x512", "scale": "1x", "filename": "512.png" },
    { "idiom": "mac", "size": "512x512", "scale": "2x", "filename": "1024.png" }
  ],
  "info": { "author": "xcode", "version": 1 }
}
JSON

  echo "==> Compiling AppIcon asset catalog with actool"
  if ! "$actool_bin" "$tmp/Assets.xcassets" \
        --compile "$RESOURCES" \
        --platform macosx \
        --minimum-deployment-target 12.0 \
        --app-icon AppIcon \
        --output-partial-info-plist "$tmp/partial.plist" \
        > "$tmp/actool.log" 2>&1; then
    echo "WARN: actool failed — skipping Assets.car (icon falls back to icon.icns)."
    sed 's/^/      actool: /' "$tmp/actool.log" | head -20 || true
    return 0
  fi

  if [[ ! -f "$RESOURCES/Assets.car" ]]; then
    echo "WARN: actool produced no Assets.car — icon falls back to icon.icns."
    return 0
  fi

  # Declare the modern icon name alongside the legacy CFBundleIconFile.
  if ! /usr/libexec/PlistBuddy -c "Add :CFBundleIconName string AppIcon" \
        "$CONTENTS/Info.plist" 2>/dev/null; then
    /usr/libexec/PlistBuddy -c "Set :CFBundleIconName AppIcon" "$CONTENTS/Info.plist"
  fi
  echo "==> Assets.car compiled; CFBundleIconName=AppIcon added to Info.plist"
}
build_asset_catalog

# ── Ad-hoc sign + strip quarantine ───────────────────────────────────────────
# An unsigned/quarantined bundle is not registered by Launch Services, which
# leaves the generic blank icon in Finder and the Dock (issue #19).
if command -v codesign >/dev/null 2>&1; then
  echo "==> Ad-hoc signing $APP"
  codesign --force --deep --sign - "$APP"
else
  echo "WARN: codesign not found — bundle left unsigned."
fi

if command -v xattr >/dev/null 2>&1; then
  xattr -dr com.apple.quarantine "$APP" 2>/dev/null || true
fi

echo ""
echo "==> Done: $APP"
echo ""
echo "Launch:  open $APP"
echo ""
echo "If Finder/Dock still show a generic icon (stale icon cache), reset it:"
echo "  /System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister -f $APP"
echo "  sudo rm -rf /Library/Caches/com.apple.iconservices.store && killall Dock Finder"
echo ""
echo "NOTE:    bundle is ad-hoc signed — Gatekeeper will block it on other machines."
echo "         See scripts/bundle-macos.sh header for Developer ID signing instructions."
