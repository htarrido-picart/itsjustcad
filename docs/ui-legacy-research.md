# Legacy CAD UI Conventions Research

Research date: 2026-08-24. Drives mydrafter theme presets and alias tables.
All hex values given as `#RRGGBB` sRGB.

---

## AutoCAD

### Model-Space Background

Default since AutoCAD 2011: **RGB(33, 40, 48)** = `#212830` (dark blue-gray).
Earlier releases defaulted to pure black `#000000`.
Paper space / layout default: white `#FFFFFF`.

Source: Autodesk community thread "What are default color specs (x,y,z) for model space background?" — confirmed RGB 33,40,48.

### Crosshair Cursor

- Default size: **5% of viewport** (CURSORSIZE system variable, range 1–100).
- At 100 the lines extend full-screen (popular with experienced users).
- Crosshair color: white (`#FFFFFF`) against dark background by default.
- Square pickbox at intersection (3 px default, PICKBOX system variable).

Source: Autodesk help CURSORSIZE; "Expand Your AutoCAD Crosshairs" (thecadgeek.com).

### Command Line

- **Position**: docked to bottom of viewport by default (can float or dock top).
- **Default height**: ~3 input rows visible (no official row-count spec; the splitter is drag-resizable).
- **Font**: Courier New, 10 pt is the historical default; modern installs often ship Consolas 10 pt (confirmed by community posts — exact out-of-box font varies by Windows locale, but always a monospace fixed-pitch font).
- Background: dark (inherits model-space dark theme in recent releases); command prompt area uses charcoal `#2D2D2D` approximately.
- Autocomplete / command suggestions appear as a popup list directly above the input line (introduced AutoCAD 2014).

Source: Autodesk ACD 2025 Help "Command Line Window Font Dialog Box"; community forums.

### Ribbon / UI Panels

- Ribbon sits at top, below menu bar (since AutoCAD 2009).
- UI font: system UI font, **Segoe UI 9 pt** on Windows (follows Windows shell).
- Palette/tool-panel titles: bold Segoe UI 9 pt.
- Toolbar icons (classic mode): 16×16 px at 96 DPI; 24×24 at 150%; 32×32 at 200% (4K).

Source: Autodesk support "How to change text size on the ribbon and toolbars in AutoCAD products".

### Mouse Bindings (MBUTTONPAN=1 default)

| Action | Binding |
|---|---|
| Zoom in/out | Scroll wheel |
| Pan | Middle-button drag |
| Zoom extents | Double-click middle button |
| 3D Orbit (transparent) | Shift + middle-button drag |
| Context menu / Osnap menu | Shift + right-click |
| Repeat last command | Enter or Space or right-click (if no drag) |

System variable `MBUTTONPAN=1` enables middle-button pan; set to 0 for legacy Osnap pop-up.

Source: Autodesk help "To Start the Pan Tool with the Middle Mouse Button"; CADforum tip.

### Command Aliases (ACAD.PGP defaults)

Complete standard alias table from the shipped `acad.pgp` file:

| Alias | Command | Alias | Command |
|---|---|---|---|
| A | ARC | AA | AREA |
| ADC | ADCENTER | AL | ALIGN |
| AP | APPLOAD | AR | ARRAY |
| ATT | ATTDEF | B | BLOCK |
| BH | HATCH | BO | BOUNDARY |
| BR | BREAK | C | CIRCLE |
| CH | PROPERTIES | CHA | CHAMFER |
| CO / CP | COPY | D | DIMSTYLE |
| DAL | DIMALIGNED | DAN | DIMANGULAR |
| DDI | DIMDIAMETER | DI | DIST |
| DIV | DIVIDE | DLI | DIMLINEAR |
| DO | DONUT | DOR | DIMORDINATE |
| DR | DRAWORDER | DRA | DIMRADIUS |
| DT | TEXT | E | ERASE |
| ED | DDEDIT | EL | ELLIPSE |
| EX | EXTEND | EXT | EXTRUDE |
| F | FILLET | G | GROUP |
| H | HATCH | HE | HATCHEDIT |
| I | INSERT | IN | INTERSECT |
| J | JOIN | L | LINE |
| LA | LAYER | LE | QLEADER |
| LEN | LENGTHEN | LI / LS | LIST |
| LO | -LAYOUT | LT | LINETYPE |
| LTS | LTSCALE | LW | LWEIGHT |
| M | MOVE | MA | MATCHPROP |
| ME | MEASURE | MI | MIRROR |
| ML | MLINE | MO | PROPERTIES |
| MS | MSPACE | MT / T | MTEXT |
| MV | MVIEW | O | OFFSET |
| OP | OPTIONS | OS | OSNAP |
| P | PAN | PE | PEDIT |
| PL | PLINE | PO | POINT |
| POL | POLYGON | PS | PSPACE |
| PU | PURGE | QC | QUICKCALC |
| R | REDRAW | RA | REDRAWALL |
| RE | REGEN | REC | RECTANG |
| REG | REGION | REV | REVOLVE |
| RO | ROTATE | RR | RENDER |
| S | STRETCH | SC | SCALE |
| SE | DSETTINGS | SEC | SECTION |
| SL | SLICE | SN | SNAP |
| SO | SOLID | SP | SPELL |
| SPL | SPLINE | ST | STYLE |
| SU | SUBTRACT | TB | TABLE |
| TOL | TOLERANCE | TOR | TORUS |
| TP | TOOLPALETTES | TR | TRIM |
| UN | UNITS | UNI | UNION |
| V | VIEW | VP | VPOINT |
| W | WBLOCK | WE | WEDGE |
| X | EXPLODE | XA | XATTACH |
| XB | XBIND | XC | XCLIP |
| XL | XLINE | XR | XREF |
| Z | ZOOM | 3DO | 3DORBIT |

Source: `acad.pgp` alias listing at stress-free.co.nz (AutoCAD 2002 baseline; aliases stable through 2024).

---

## Rhino (Rhinoceros 3D)

### Viewport Backgrounds

Rhino ships with four default viewports (Top, Front, Right, Perspective).

- **Wireframe background**: medium gray — community consensus and source-reviewed Rhino default display mode files place this around **RGB(212, 212, 212)** (`#D4D4D4`) for Perspective and similar viewport backgrounds. Some versions show a subtle gradient (lighter at top, slightly darker at bottom). Exact value confirmed as approximately `#D4D4D4` by Rhino user reports on discourse.mcneel.com.
- **Grid**: major lines slightly darker gray (`#A0A0A0` approx); minor lines even lighter.
- **World axes icon**: colored X(red), Y(green), Z(blue) in lower-left of each viewport.
- **Shaded / Rendered modes**: backgrounds can differ per display mode.

Note: Rhino 8 introduced dark UI panels, but default viewport background remains light gray for Wireframe.

Source: McNeel discourse threads on viewport background color; shapemachine.design.gatech.edu Rhino setup guide (recommends white for architectural work).

### Command Line / Prompt Bar

- **Position**: top of window, directly below the menu bar — above the toolbar row.
- Input field is persistent at top; command history scrolls upward above it.
- Font: follows Windows system UI font (Segoe UI 9 pt on Windows, SF UI on Mac).
- Background: light (gray-white, matches panel chrome).
- Command options appear inline as clickable hyperlinks in the command history area.

Source: Rhino 8 window layout documentation at docs.mcneel.com/rhino/8/help/en-us/user_interface/rhino_window.htm.

### Viewport Tab Placement

- Tabs at **bottom** of viewport frame (one tab per named viewport / layout).
- Model tabs and layout (paper space) tabs coexist in the same tab bar.

### Right Mouse Button Convention

- **Right-click in viewport (no drag)**: repeats last command (acts as Enter/Return).
- **Right-click + drag**: no default viewport orbit; orbit is done with middle-drag or Shift+Ctrl+drag depending on mouse settings.
- **Middle-drag**: rotate / orbit in Perspective viewport.
- **Scroll wheel**: zoom.
- Context menu: configurable; default is right-click = repeat last.

This is a critical workflow convention that mydrafter should match: right-click = accept/repeat.

Source: McNeel docs "Repeat | Rhino 3D modeling"; discourse thread "Disable enter repeat command on right mouse button".

### Default Aliases (Rhino ships minimal built-ins; users often import ACAD set)

Rhino's built-in default alias list is intentionally small. McNeel provides an optional ACAD alias pack (`acadaliasesforrhino.zip`). Common defaults observed in community:

| Alias | Command |
|---|---|
| C | Circle |
| L | Line |
| R | Rectangle |
| P | Point |
| PL | Polyline |
| M | Move |
| CP | Copy |
| RO | Rotate |
| SC | Scale |
| MI | Mirror |
| E | Erase (Delete) |
| TR | Trim |
| EX | Extend |
| F | Fillet |
| CH | Chamfer |
| OF | Offset |
| J | Join |
| EXP | Explode |
| BO | BooleanUnion |
| BS | BooleanDifference |
| BI | BooleanIntersection |

Source: Rhino helpmax aliases page; McNeel Wiki "AutoCAD Aliases for Rhino"; community cheat sheets.

### Mouse Bindings

| Action | Binding |
|---|---|
| Zoom | Scroll wheel |
| Pan (2D views) | Right-drag |
| Orbit (Perspective) | Right-drag |
| Pan (Perspective) | Shift + right-drag |
| Zoom to window | Shift + right-drag in 2D |
| Repeat last command | Right-click (no drag) |
| Osnap menu | Shift + right-click |

Note: Rhino uses right-drag for orbit in 3D — the opposite hemisphere from AutoCAD which uses middle-drag.

### UI Font

- Windows: Segoe UI 9 pt for all panels, toolbars, property sheets.
- Command prompt text: same as UI font (not a fixed-pitch font, unlike AutoCAD).

---

## Revit

### Canvas Background

- 2D views (floor plan, section, elevation, sheet): **white** `#FFFFFF` by default.
- 3D views: **white** by default in older versions; Revit 2024+ added dark canvas option.
- No grid visible on canvas by default (grid only shown explicitly via structural grids).

Source: Autodesk support "How to change the background color in AutoCAD [Revit]"; BIM Chapters blog.

### UI Font

- Ribbon, browser, properties: **Segoe UI 9 pt** (follows Windows system font; cannot be changed inside Revit itself — only via Windows accessibility settings).
- Dialog labels: Segoe UI 9 pt.
- Canvas annotation (dimensions, tags): separate text style system entirely (not UI font).

Source: Autodesk support "How to change the size and typeface in the Revit UI".

### Ribbon Conventions (worth borrowing minimally)

- Contextual ribbon tabs appear when objects are selected (e.g., "Modify | Walls").
- Each tab divided into **panels** (logical groups) with a panel title at the bottom.
- Most-used tools pinned to the first tab (Architecture/Structure/MEP).
- Quick Access Toolbar (QAT) above ribbon for Save, Undo, Redo, etc.
- Ribbon auto-collapses to tab labels only (double-click to expand).

Key insight: Revit users expect **context-sensitive tools** — the active object type changes what's available. mydrafter could surface this via the deck/command-line context rather than a ribbon.

### Mouse Navigation

| Action | Binding |
|---|---|
| Zoom | Scroll wheel |
| Pan | Middle-button drag |
| Orbit (3D views only) | Shift + middle-button drag |
| Zoom to fit | ZA (type) or double-click wheel |
| Select | Left-click |
| Deselect / cancel | Escape |

Source: Mashyo "How to Navigate in Revit: Zoom, Orbit, Pan"; Autodesk navigation docs.

### No Command Line

Revit has no persistent command line. Commands are initiated only via ribbon, keyboard shortcuts, or right-click context menus. This is the sharpest contrast with AutoCAD/Rhino — mydrafter's command-line-first approach will feel more like AutoCAD/Rhino to Revit migrants.

---

## General Desktop CAD Conventions

### UI Font Sizes

| Element | Typical Pt Size | Notes |
|---|---|---|
| Menu bar | 9 pt | Windows Segoe UI default |
| Ribbon tab labels | 9 pt | |
| Ribbon panel labels | 8 pt (smaller) | Often all-caps |
| Toolbar tooltips | 9 pt | |
| Command line / prompt | 10–11 pt | Monospace in AutoCAD; proportional in Rhino/Revit |
| Status bar | 8–9 pt | |
| Property panels | 9 pt | |
| Dialog boxes | 9 pt | |

Minimum recommended: **9 pt Segoe UI** (or equivalent) at 96 DPI. Scale linearly with DPI.
At 120 DPI (125% scaling): ~11 pt equivalent target.
At 144 DPI (150% scaling): ~13 pt equivalent target.

Source: Microsoft typography guidelines (learn.microsoft.com); DesignCAD 2022 icon size docs.

### Toolbar Icon Sizes

| DPI scaling | Small icons | Large icons |
|---|---|---|
| 100% (96 DPI) | 16×16 px | 32×32 px |
| 125% (120 DPI) | 20×20 px | 40×40 px |
| 150% (144 DPI) | 24×24 px | 48×48 px |
| 200% (192 DPI) | 32×32 px | 64×64 px |

AutoCAD ships PNG sprite sheets covering all five sizes. Toolbar icon density: ~28 icons per row at 16 px.

Source: Autodesk support "Ribbon and Palette icons are too small on high-resolution monitor"; AUGI forum "Original size of toolbar command icons".

### DPI Handling Patterns

1. **AutoCAD**: stores icons at multiple resolutions in CUI; scales ribbon via Windows DPI awareness.
2. **Rhino**: uses OS-level scaling (HiDPI retina on Mac; DPI awareness on Windows).
3. **Revit**: fully DPI-aware via WPF; all UI elements scale with Windows display settings.
4. **Pattern for mydrafter**: declare DPI awareness, scale viewport content independently from UI chrome, use egui's built-in pixels_per_point for crisp rendering.

---

## Preset Table

This table is the direct source for implementing mydrafter theme presets.

### AutoCAD Preset

| Key | Value |
|---|---|
| `bg_color` | `#212830` (RGB 33,40,48) |
| `grid_major_color` | `#3A4550` (approx; slightly lighter than bg) |
| `grid_minor_color` | `#2A3038` (approx; barely visible) |
| `crosshair_color` | `#FFFFFF` |
| `ui_font` | Segoe UI 9 pt |
| `cmd_font` | Consolas 10 pt (monospace) |
| `cmd_position` | Bottom, docked |
| `cmd_rows_visible` | 3 |
| `accent_color` | `#00A1F1` (AutoCAD blue, approx) |
| `toolbar_icon_px` | 16 (96 DPI), scales with DPI |
| Mouse pan | Middle-drag |
| Mouse zoom | Scroll wheel |
| Mouse orbit | Shift + middle-drag |
| Right-click | Context menu (no repeat-last) |
| **Core aliases** | See AutoCAD alias table above |

### Rhino Preset

| Key | Value |
|---|---|
| `bg_color` | `#D4D4D4` (RGB 212,212,212 — light gray) |
| `grid_major_color` | `#A0A0A0` |
| `grid_minor_color` | `#C8C8C8` |
| `crosshair_color` | `#404040` (dark on light bg) |
| `ui_font` | Segoe UI 9 pt |
| `cmd_font` | Segoe UI 9 pt (proportional) |
| `cmd_position` | Top, below menu bar |
| `cmd_rows_visible` | 1–2 (history above, input below) |
| `accent_color` | `#E8E8E8` panel chrome (neutral) |
| `toolbar_icon_px` | 24 (Rhino uses slightly larger default icons) |
| Mouse pan | Right-drag (2D) / Shift+right-drag (3D) |
| Mouse zoom | Scroll wheel |
| Mouse orbit | Right-drag (Perspective viewport) |
| Right-click | Repeat last command |
| **Core aliases** | See Rhino alias table above |

### Revit Preset

| Key | Value |
|---|---|
| `bg_color` | `#FFFFFF` (pure white) |
| `grid_major_color` | n/a (no automatic grid) |
| `grid_minor_color` | n/a |
| `crosshair_color` | `#000000` (black on white) |
| `ui_font` | Segoe UI 9 pt |
| `cmd_font` | n/a (no command line) |
| `cmd_position` | n/a |
| `accent_color` | `#0070C0` (Revit blue ribbon accent) |
| `toolbar_icon_px` | 16 (96 DPI) |
| Mouse pan | Middle-drag |
| Mouse zoom | Scroll wheel |
| Mouse orbit | Shift + middle-drag (3D only) |
| Right-click | Context menu |
| **Aliases** | n/a (keyboard shortcuts, not aliases) |

---

## Implementation Notes for mydrafter

1. **Theme switcher**: three named presets ("AutoCAD", "Rhino", "Revit") loadable at runtime. Store in `~/.config/mydrafter/ui.json` under a `theme` key.

2. **Command-line position**: make it configurable top vs bottom. AutoCAD veterans expect bottom; Rhino users expect top. Default mydrafter to bottom (AutoCAD convention is larger user base).

3. **Right-click behavior**: expose as a preference — "Repeat last command" (Rhino) vs "Context menu" (AutoCAD/Revit). Rhino convention strongly preferred by power users.

4. **Alias registration**: the commands crate registry already feeds aliases. Pre-populate with the ACAD.PGP core set (`l`, `c`, `pl`, `o`, `tr`, `co`, `mi`, `ro`, `ar`, `x`, `e`, `m`, `z`, etc.) since those are muscle memory for the largest CAD user base.

5. **Crosshair cursor**: store as a percentage (like AutoCAD CURSORSIZE), default 5%. Expose in prefs.

6. **Grid colors**: compute from background using luminance offset — don't store absolute colors for grid, derive them. Keeps all three presets consistent.

7. **Font size scaling**: read system DPI at startup and apply a `base_pt * (dpi / 96.0)` scale factor. egui's `pixels_per_point` handles this once set correctly.

---

## Sources

- [AutoCAD 2025 Help — Command Line Window Font Dialog Box](https://help.autodesk.com/view/ACD/2025/ENU/?guid=GUID-F87EBBFC-E3A0-478E-8C8E-F31A86ED7144)
- [AutoCAD 2024 Help — About Positioning the Command Window](https://help.autodesk.com/view/ACD/2025/ENU/?guid=GUID-5A401FD1-C5BD-4AD1-BB75-801A6D3AD09E)
- [AutoCAD 2024 Help — To Change the Font Displayed in the Command Window](https://help.autodesk.com/view/ACD/2024/ENU/?guid=GUID-CC2EE334-2D51-43EB-AC8A-FD4EAE2525CA)
- [Autodesk — How to change the background color in AutoCAD](https://www.autodesk.com/support/technical/article/caas/sfdcarticles/sfdcarticles/How-to-change-the-background-color-of-the-AutoCAD-drawing-window.html)
- [Autodesk Community — What are default color specs for model space background?](https://forums.autodesk.com/t5/autocad-architecture-forum/what-are-default-color-specs-x-y-z-for-model-space-background/td-p/13213889)
- [Autodesk — CURSORSIZE System Variable](https://help.autodesk.com/cloudhelp/2022/ENU/AutoCAD-Core/files/GUID-06BFA068-F97A-453F-8403-75AA778E7C35.htm)
- [Autodesk — AutoCAD Crosshair Tuesday Tips](https://www.autodesk.com/blogs/autocad/autocad-crosshair-tuesday-tips-with-frank/)
- [Autodesk — Middle Mouse Button Pan](https://help.autodesk.com/view/NAVFREE/2022/ENU/?guid=GUID-E615BAFB-E74B-429C-AA3D-397E5D6C2697)
- [CADForum — How to disable 3D orbit on Shift+mouse wheel](https://www.cadforum.cz/en/qaID.asp?tip=6277)
- [AutoCAD ACAD.PGP alias list (2002 baseline)](https://www.stress-free.co.nz/sites/default/files/images/tutorials/autocad.2002/acad_pgp_aliases_06.html)
- [Autodesk — About Command Aliases (AutoCAD 2024)](https://help.autodesk.com/view/ACD/2024/ENU/?guid=GUID-BD3AD667-EBFC-4C0A-A691-C07D4436DAB0)
- [Rhino 8 — Window Layout Documentation](https://docs.mcneel.com/rhino/8/help/en-us/user_interface/rhino_window.htm)
- [Rhino 7 — Appearance Options](https://docs.mcneel.com/rhino/7/help/en-us/options/appearance.htm)
- [McNeel Wiki — AutoCAD Aliases for Rhino](https://wiki.mcneel.com/rhino/acadaliases)
- [McNeel Discourse — Disable enter repeat command on right mouse button](https://discourse.mcneel.com/t/disable-enter-repeat-command-on-right-mouse-button/21829)
- [McNeel Docs — Repeat command](https://docs.mcneel.com/rhino/8/help/en-us/commands/repeat.htm)
- [Rhino 7 — Aliases Options](https://docs.mcneel.com/rhino/7/help/en-us/user_interface/shortcuts.htm)
- [Autodesk — How to change the size and typeface in the Revit UI](https://www.autodesk.com/support/technical/article/caas/sfdcarticles/sfdcarticles/Revit-How-to-change-the-size-and-typeface-in-the-Revit-UI.html)
- [Autodesk — Revit Navigation Tools](https://ukcommunity.arkance.world/hc/en-us/articles/21565674884882-Revit-2019-Navigation-Tools)
- [Mashyo — How to Navigate in Revit: Zoom, Orbit, Pan](https://mashyo.com/navigate-in-revit/)
- [BIM Chapters — Changing the Revit Background Color](https://bimchapters.blogspot.com/2018/01/changing-revit-background-color-use.html)
- [Microsoft — Fonts Win32 UX Guidelines](https://learn.microsoft.com/en-us/windows/win32/uxguide/vis-fonts)
- [Autodesk — Ribbon and Palette icons on high-resolution monitors](https://www.autodesk.com/support/technical/article/caas/sfdcarticles/sfdcarticles/My-icons-and-text-is-small-on-my-new-machine.html)
- [AUGI Forums — Original size of toolbar command icons](https://forums.augi.com/archive/index.php/t-169501.html)
