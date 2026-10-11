#!/usr/bin/env sh
# install.sh — per-user install of ItsJustCAD (writes only under $HOME).
#
# Copies, relative to this script's directory:
#   itsjustcad                        -> ~/.local/bin/
#   itsjustcad.desktop                -> ~/.local/share/applications/
#   icons/hicolor/<size>/apps/*.png   -> ~/.local/share/icons/hicolor/<size>/apps/
#
# Run from the unpacked release tarball:  ./install.sh

set -eu

HERE="$(cd "$(dirname "$0")" && pwd)"
BIN_DIR="${HOME}/.local/bin"
APPS_DIR="${HOME}/.local/share/applications"
ICONS_DIR="${HOME}/.local/share/icons"

mkdir -p "$BIN_DIR" "$APPS_DIR"

install -m 755 "$HERE/itsjustcad" "$BIN_DIR/itsjustcad"
install -m 644 "$HERE/itsjustcad.desktop" "$APPS_DIR/itsjustcad.desktop"

for size in 256x256 512x512; do
  src="$HERE/icons/hicolor/$size/apps/itsjustcad.png"
  if [ -f "$src" ]; then
    mkdir -p "$ICONS_DIR/hicolor/$size/apps"
    install -m 644 "$src" "$ICONS_DIR/hicolor/$size/apps/itsjustcad.png"
  fi
done

# Refresh desktop/icon caches so the launcher picks the icon up immediately.
command -v update-desktop-database >/dev/null 2>&1 && update-desktop-database "$APPS_DIR" || true
command -v gtk-update-icon-cache  >/dev/null 2>&1 && gtk-update-icon-cache -q "$ICONS_DIR/hicolor" || true

echo "Installed itsjustcad to $BIN_DIR (make sure it is on your PATH)."
echo "Desktop entry + icons installed under ~/.local/share."
