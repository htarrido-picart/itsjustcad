#!/usr/bin/env bash
# SPDX-License-Identifier: AGPL-3.0-or-later
# Copyright © 2026 Hector Tarrido-Picart
#
# render-examples.sh — regenerate the inline showcase images used in the README.
#
# Each showcase is a small, deterministic command script in
# docs/examples/scenes/*.txt. This script frames each one (ze + a view, and an
# optional display mode) and renders it offscreen to a PNG via the headless
# `--run … --shot` path — the exact path CI and the agents use — then downscales
# the committed PNG to a sane width (default 640 px) in docs/examples/img/.
#
# The seven scenes map one-to-one to the README's showcased sections:
#   formfinding  → Form-finding & expressive structures
#   dieste       → Form-finding (Dieste Gaussian masonry vaults)
#   see_*        → See (camera / display modes)
#   env          → Analyze the environment (sun + shadow study)
#   landscape    → Landscape & site (planting)
#   compliance   → Pre-check code compliance
#   facade_*     → Document (the flagship curtain-wall building)
#   venetian_*   → Document (a Venetian palazzo facade from primitives)
#   blocks       → Blocks & external references
#
# ── Requires a GPU adapter ──────────────────────────────────────────────────
# The headless renderer needs a real wgpu adapter (Metal / Vulkan / D3D12).
# On a headless CI box or a sandbox with no GPU, render fails with "no wgpu
# adapter" and NO images are produced — that is expected. Run this on a machine
# with a GPU (any normal desktop/laptop) to (re)generate them.
#
# ── Usage ───────────────────────────────────────────────────────────────────
#   docs/examples/render-examples.sh                # build (quick) + render all
#   IJC_BIN=/path/to/itsjustcad docs/examples/render-examples.sh   # prebuilt binary
#   IJC_THUMB=720 docs/examples/render-examples.sh  # different committed width
#
# Downscaling uses ImageMagick (`magick`/`convert`) if present, else macOS
# `sips`. If neither exists the full-size 1280×800 PNG is kept as-is.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
SCENES="$ROOT/docs/examples/scenes"
IMG_DIR="$ROOT/docs/examples/img"
TMP_DIR="$(mktemp -d)"
THUMB_W="${IJC_THUMB:-640}"                 # committed image width in px
RENDER_SIZE="${IJC_RENDER_SIZE:-1100x850}"  # offscreen window hint

mkdir -p "$IMG_DIR"
trap 'rm -rf "$TMP_DIR"' EXIT

# ── Locate / build the binary ───────────────────────────────────────────────
BIN="${IJC_BIN:-}"
if [ -z "$BIN" ]; then
  for cand in "$ROOT/target/quick/itsjustcad" "$ROOT/target/release/itsjustcad" "$ROOT/target/debug/itsjustcad"; do
    [ -x "$cand" ] && BIN="$cand" && break
  done
fi
if [ -z "$BIN" ] || [ ! -x "$BIN" ]; then
  echo "no prebuilt binary found — building (cargo build -p itsjustcad --profile quick)…"
  ( cd "$ROOT" && cargo build -p itsjustcad --profile quick )
  BIN="$ROOT/target/quick/itsjustcad"
fi
echo "using binary: $BIN"

# ── Thumbnail downscaler ────────────────────────────────────────────────────
downscale() { # <src.png> <dst.png>
  local src="$1" dst="$2"
  if command -v magick >/dev/null 2>&1; then
    magick "$src" -resize "${THUMB_W}x" "$dst"
  elif command -v convert >/dev/null 2>&1; then
    convert "$src" -resize "${THUMB_W}x" "$dst"
  elif command -v sips >/dev/null 2>&1; then
    cp "$src" "$dst"; sips --resampleWidth "$THUMB_W" "$dst" >/dev/null
  else
    echo "  (no magick/convert/sips — keeping full-size)"; cp "$src" "$dst"
  fi
}

# ── Render one scene: <out-name> <scene-file> <view> [display] ──────────────
render() {
  local name="$1" scene="$2" view="${3:-persp}" display="${4:-}"
  local script="$TMP_DIR/$name.txt" raw="$TMP_DIR/$name.png" out="$IMG_DIR/$name.png"
  cp "$scene" "$script"
  { echo "ze"; echo "$view"; echo "ze"; [ -n "$display" ] && echo "display $display"; } >> "$script"
  if ITSJUSTCAD_WINDOW_SIZE="$RENDER_SIZE" ITSJUSTCAD_THEME=dark \
       "$BIN" --run "$script" --headless --shot "$raw" >/dev/null 2>"$TMP_DIR/$name.err"; then
    downscale "$raw" "$out"
    echo "  ✓ $name → docs/examples/img/$name.png"
  else
    echo "  ✗ $name FAILED:"; sed 's/^/      /' "$TMP_DIR/$name.err"
    FAILED=1
  fi
}

FAILED=0
echo "rendering showcase images → $IMG_DIR (committed width ${THUMB_W}px)"

# The blocks scene attaches an xref; build its source drawing first so the
# reference resolves deterministically on any machine.
PAVILION="$TMP_DIR/pavilion.dxf"
cat > "$TMP_DIR/mkpavilion.txt" <<EOF
box 0,0,0 6,6,0.3
box 0,0,0.3 6,0.3,3
box 0,5.7,0.3 6,0.3,3
export $PAVILION
EOF
"$BIN" --run "$TMP_DIR/mkpavilion.txt" --headless >/dev/null 2>&1 || true

# The committed blocks scene references /tmp/pavilion.dxf; point it at ours.
BLOCKS_SCENE="$TMP_DIR/blocks_scene.txt"
sed "s#/tmp/pavilion.dxf#$PAVILION#g" "$SCENES/blocks.txt" > "$BLOCKS_SCENE"

# ── Form-finding & expressive structures ────────────────────────────────────
render formfinding "$SCENES/formfinding.txt" persp
render dieste      "$SCENES/dieste.txt"      persp

# ── See (camera / display modes): same building, two ways ───────────────────
render see_shaded "$SCENES/facade.txt" persp
render see_pencil "$SCENES/facade.txt" persp pencil

# ── Analyze the environment (sun + shadow study) ────────────────────────────
render env "$SCENES/env.txt" persp

# ── Landscape & site (planting) ─────────────────────────────────────────────
render landscape "$SCENES/landscape.txt" persp

# ── Pre-check code compliance ───────────────────────────────────────────────
render compliance "$SCENES/compliance.txt" persp

# ── Document (the flagship curtain-wall building) ───────────────────────────
render facade_persp "$SCENES/facade.txt" persp
render facade_elev  "$SCENES/facade.txt" front

# ── Document (paper-space dimensioned drawing sheet) ────────────────────────
# Unlike the GPU renders above, this one prints a real sheet to PDF (no GPU
# needed) and rasterizes page 1 to a PNG. The scene ends with
# `print A-101 <pdf>`; we rewrite that path to a temp file, run headless, then
# pdftoppm/magick the first page and downscale it like the others.
render_sheet() { # <out-name> <scene-file> <sheet-name>
  local name="$1" scene="$2" sheet="$3"
  local script="$TMP_DIR/$name.txt" pdf="$TMP_DIR/$name.pdf"
  local raw="$TMP_DIR/${name}_page" out="$IMG_DIR/$name.png"
  # Repoint the scene's `print` line at our temp PDF.
  sed "s#^print .*#print $sheet $pdf#" "$scene" > "$script"
  if "$BIN" --run "$script" --headless >/dev/null 2>"$TMP_DIR/$name.err"; then
    if command -v pdftoppm >/dev/null 2>&1; then
      pdftoppm -png -r 150 "$pdf" "$raw" && downscale "${raw}-1.png" "$out"
    elif command -v magick >/dev/null 2>&1; then
      magick -density 150 "${pdf}[0]" "${raw}-1.png" && downscale "${raw}-1.png" "$out"
    else
      echo "  ✗ $name: need pdftoppm or magick to rasterize the PDF"; FAILED=1; return
    fi
    echo "  ✓ $name → docs/examples/img/$name.png"
  else
    echo "  ✗ $name FAILED:"; sed 's/^/      /' "$TMP_DIR/$name.err"
    FAILED=1
  fi
}
render_sheet document_sheet "$SCENES/document.txt" A-101

# ── Facades (a Venetian palazzo, assembled from primitives) ──────────────────
render venetian_elev  "$SCENES/venetian.txt" front
render venetian_persp "$SCENES/venetian.txt" persp

# ── Blocks & external references ────────────────────────────────────────────
render blocks "$BLOCKS_SCENE" persp

echo
if [ "$FAILED" -eq 0 ]; then
  echo "done — all showcase images rendered."
else
  echo "done — some images failed (see above). If every one failed with"
  echo "'no wgpu adapter', this machine has no GPU; run on a GPU machine."
fi
