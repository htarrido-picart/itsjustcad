# UI icons — Lucide

The line icons used by the ItsJustCAD interface (menu bar, tab strip, toolbars)
are from [Lucide](https://lucide.dev), a permissively-licensed open-source icon
set (ISC License; some icons derived from Feather under MIT). See
`LUCIDE-LICENSE.txt` in this directory for the full text.

We deliberately do **not** use Apple's SF Symbols: their license restricts use to
Apple platforms and forbids redistribution, which is incompatible with this
project's AGPL-3.0 FOSS distribution. Lucide gives us the same clean,
consistent-stroke line-icon language with no such restriction.

## Layout

* `*.svg` — the original Lucide sources (24×24, `stroke="currentColor"`,
  stroke-width 2, round caps/joins).
* `png/*.png` — each icon rasterized to a 48×48 **white-on-transparent** raster
  (`currentColor` → `#FFFFFF`). White so the app can tint each icon to the active
  theme's foreground color at draw time (`egui::Image::tint`), keeping one raster
  per icon for both light and dark skins.

## Regenerating the PNGs

```sh
for f in *.svg; do
  n="${f%.svg}"
  sed 's/currentColor/#FFFFFF/g' "$f" > "/tmp/_ic_$n.svg"
  cairosvg "/tmp/_ic_$n.svg" -o "png/$n.png" --output-width 48 --output-height 48
done
```
