#!/usr/bin/env bash
# test-bundle-macos.sh — hermetic tests for scripts/bundle-macos.sh (issue #19).
#
# Covers:
#   1. Info.plist contains BOTH CFBundleIconFile and CFBundleIconName, and
#      Assets.car is compiled, when actool is available.
#   2. Bundle assembly still succeeds (graceful degrade) when actool is
#      unavailable: no Assets.car, no CFBundleIconName, exit 0.
#   3. The bundle is ad-hoc codesigned and carries no quarantine attribute.
#
# Hermetic: uses a stub binary (no cargo build), a temp dist dir, no network,
# no sudo, never touches /Applications.

set -eo pipefail

if [[ "$(uname)" != "Darwin" ]]; then
  echo "SKIP: macOS-only test."
  exit 0
fi

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
BUNDLE_SH="$REPO_ROOT/scripts/bundle-macos.sh"

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

FAILURES=0
pass() { echo "ok   - $1"; }
fail() { echo "FAIL - $1"; FAILURES=$((FAILURES + 1)); }
check() { # check <description> <command...>
  local desc="$1"; shift
  if "$@" >/dev/null 2>&1; then pass "$desc"; else fail "$desc"; fi
}

# Stub executable so the script skips `cargo build`.
STUB="$TMP/stub-itsjustcad"
cp /usr/bin/true "$STUB"
chmod +x "$STUB"

plist_get() { /usr/libexec/PlistBuddy -c "Print :$2" "$1" 2>/dev/null; }

# ── Case 1: normal run (actool available on this machine or not) ─────────────
DIST1="$TMP/dist1"
echo "==> Case 1: default run"
ITSJUSTCAD_BUNDLE_BINARY="$STUB" ITSJUSTCAD_DIST_DIR="$DIST1" \
  bash "$BUNDLE_SH" > "$TMP/run1.log" 2>&1 \
  || { fail "bundle script exits 0 (default run)"; sed 's/^/     /' "$TMP/run1.log"; }

APP1="$DIST1/ItsJustCAD.app"
PLIST1="$APP1/Contents/Info.plist"
check "bundle exists" test -d "$APP1"
check "Info.plist exists" test -f "$PLIST1"
check "CFBundleIconFile=icon" test "$(plist_get "$PLIST1" CFBundleIconFile)" = "icon"
check "icon.icns copied" test -f "$APP1/Contents/Resources/icon.icns"

HAVE_ACTOOL="$(xcrun --find actool 2>/dev/null || command -v actool || true)"
if [[ -n "$HAVE_ACTOOL" ]]; then
  check "CFBundleIconName=AppIcon (modern icon declared)" \
    test "$(plist_get "$PLIST1" CFBundleIconName)" = "AppIcon"
  check "Assets.car compiled" test -f "$APP1/Contents/Resources/Assets.car"
else
  echo "note - actool not on this machine; modern-icon assertions covered by Case 2 shape only"
fi

check "bundle is codesigned (ad-hoc)" codesign -dv "$APP1"
check "codesign verifies" codesign --verify --deep "$APP1"
if xattr -l "$APP1" 2>/dev/null | grep -q com.apple.quarantine; then
  fail "no quarantine attribute on bundle"
else
  pass "no quarantine attribute on bundle"
fi

# ── Case 2: actool unavailable → graceful degrade ────────────────────────────
DIST2="$TMP/dist2"
echo "==> Case 2: actool unavailable (graceful degrade)"
ITSJUSTCAD_BUNDLE_BINARY="$STUB" ITSJUSTCAD_DIST_DIR="$DIST2" \
  ITSJUSTCAD_ACTOOL="$TMP/no-such-actool" \
  bash "$BUNDLE_SH" > "$TMP/run2.log" 2>&1 \
  || { fail "bundle script exits 0 without actool"; sed 's/^/     /' "$TMP/run2.log"; }

APP2="$DIST2/ItsJustCAD.app"
PLIST2="$APP2/Contents/Info.plist"
check "bundle exists without actool" test -d "$APP2"
check "CFBundleIconFile kept (icns fallback)" \
  test "$(plist_get "$PLIST2" CFBundleIconFile)" = "icon"
if plist_get "$PLIST2" CFBundleIconName >/dev/null; then
  fail "CFBundleIconName omitted when Assets.car is skipped"
else
  pass "CFBundleIconName omitted when Assets.car is skipped"
fi
check "no Assets.car without actool" test ! -f "$APP2/Contents/Resources/Assets.car"
check "actool skip is warned" grep -q "actool" "$TMP/run2.log"
check "bundle still codesigned without actool" codesign -dv "$APP2"

echo ""
if [[ "$FAILURES" -eq 0 ]]; then
  echo "All bundle-macos.sh tests passed."
else
  echo "$FAILURES test(s) FAILED."
  exit 1
fi
