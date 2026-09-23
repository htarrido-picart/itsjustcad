#!/usr/bin/env bash
# SPDX-License-Identifier: AGPL-3.0-or-later
# Copyright © 2026 Hector Tarrido-Picart
#
# render-examples.sh — regenerate the small thumbnail images used in the README
# "Visual examples" section.
#
# For each showcase example this writes a tiny, deterministic command script,
# renders it offscreen to a PNG via the headless `--run … --shot` path, then
# downscales it to a SMALL thumbnail (default 360 px wide) in docs/examples/img/.
#
# ── Requires a GPU adapter ──────────────────────────────────────────────────
# The headless renderer needs a real wgpu adapter (Metal / Vulkan / D3D12).
# On a headless CI box or a sandbox with no GPU, `render_headless` fails with
# "no wgpu adapter" and NO images are produced — that is expected. Run this on a
# machine with a GPU (any normal desktop/laptop) to (re)generate the thumbnails.
#
# ── Usage ───────────────────────────────────────────────────────────────────
#   docs/examples/render-examples.sh                # build (quick) + render all
#   IJC_BIN=/path/to/itsjustcad docs/examples/render-examples.sh   # use a prebuilt binary
#   IJC_THUMB=320 docs/examples/render-examples.sh  # different thumbnail width
#
# Downscaling uses ImageMagick (`magick`/`convert`) if present, else macOS
# `sips`. If neither exists the full-size 1280×800 PNG is kept as-is.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
IMG_DIR="$ROOT/docs/examples/img"
TMP_DIR="$(mktemp -d)"
THUMB_W="${IJC_THUMB:-360}"          # thumbnail width in px
RENDER_SIZE="${IJC_RENDER_SIZE:-720x720}"  # offscreen window hint (render is fixed 1280x800)

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

# ── One example: name + view + heredoc script on stdin ──────────────────────
# render <name> <view: persp|top|front|…> [display mode]
render() {
  local name="$1" view="${2:-persp}" display="${3:-}"
  local script="$TMP_DIR/$name.txt" raw="$TMP_DIR/$name.png" out="$IMG_DIR/$name.png"
  cat > "$script"
  { echo "ze"; echo "$view"; [ -n "$display" ] && echo "display $display"; } >> "$script"
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
echo "rendering examples → $IMG_DIR (thumbnail width ${THUMB_W}px)"

# ── Drawing / curves ────────────────────────────────────────────────────────
render polyline-arc top <<'EOF'
polyline 0,0,0 6,0,0 6,4,0 2,4,0
arc 2,4,0 2 0 180
EOF

render polygon top <<'EOF'
polygon 0,0,0 4 6
EOF

render circle-tangent top <<'EOF'
line 0,0,0 10,0,0
name last a
line 0,0,0 0,10,0
name last b
circletan a b 2
EOF

# ── Sweeps / surfaces ───────────────────────────────────────────────────────
render sweep1 <<'EOF'
polyline 0,0,0 5,0,0 5,5,0 10,5,3
name last rail
circle 0,0,0 0.5
sweep last rail
EOF

render sweep2 <<'EOF'
polyline 0,0,0 4,0,2 8,0,0
name last raila
polyline 0,6,0 4,6,3 8,6,0
name last railb
circle 0,0,0 0.5
sweep2 last raila railb
EOF

render loft <<'EOF'
circle 0,0,0 3
circle 0,0,6 1.5
loft all
EOF

render revolve <<'EOF'
polyline 1,0,0 3,0,1 2,0,4 1,0,6 0.6,0,6 0.6,0,0 closed
revolve last
EOF

# ── Form-finding / expressive ───────────────────────────────────────────────
render geodesic-dome <<'EOF'
geodesic 3 5 dome
EOF

render hypar <<'EOF'
hypar 5 5 6
EOF

render gridshell <<'EOF'
gridshell vault 8 12 3
EOF

render funicular <<'EOF'
funicular 0,0,6 12,0,6 24 1 0.4 invert
EOF

render tensegrity <<'EOF'
tensegrity 3 3 5 30
EOF

render minsurf <<'EOF'
polyline 0,0,0 6,0,2 6,6,0 0,6,2 closed
minsurf last 16
EOF

# ── Parametric (M-parametric) ───────────────────────────────────────────────
render spaceframe <<'EOF'
spaceframe 6 4 2 1.5
EOF

render gaussvault <<'EOF'
gaussvault 8 14 3 undulate
EOF

# ── Drawings → BIM ──────────────────────────────────────────────────────────
render fromlayer-walls <<'EOF'
layer walls
polyline 0,0,0 10,0,0 10,8,0 0,8,0 closed
fromlayer walls wall thick 0.3 height 3
EOF

render massing <<'EOF'
box 0,0,0 16,12,2
box 2,2,2 12,8,3
box 5,4,5 6,4,8
EOF

# ── Hatch ───────────────────────────────────────────────────────────────────
render hatch-brick top <<'EOF'
rect 0,0,0 8 5
hatch last brick
EOF

render hatch-concrete top <<'EOF'
rect 0,0,0 8 5
hatch last concrete
EOF

echo
if [ "$FAILED" -eq 0 ]; then
  echo "done — all examples rendered."
else
  echo "done — some examples failed (see above). If every one failed with"
  echo "'no wgpu adapter', this machine has no GPU; run on a GPU machine."
fi
