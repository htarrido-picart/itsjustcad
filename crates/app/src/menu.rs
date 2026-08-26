// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! Registry-driven menu bar.
//!
//! The menu structure is derived from ONE source — the command registry's
//! [`Category`] on each [`CommandSpec`] — so menus, the (future) toolbar, and
//! the command palette never drift apart. A per-preset [`MenuStyle`] maps the
//! twelve categories onto top-level menu titles (Rhino vs AutoCAD ergonomics);
//! every category lands in exactly one menu, so no verb is orphaned.
//!
//! The pure functions here ([`top_menus`], [`categories_for`], [`menu_action`],
//! [`verbs_in`]) carry no egui state and are unit-tested standalone. The `ui`
//! entry point renders them and returns the action the user picked.

use itsjustcad_commands::{Category, registry};

use crate::icons::{Icon, Icons};
use crate::preset::MenuStyle;

/// What happens when a menu item is chosen. The rule (documented on each
/// variant) is: bare draw verbs start the interactive draw tool; verbs that
/// take no arguments execute immediately; verbs needing arguments are inserted
/// into the command line (with a trailing space) ready for the user to type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MenuAction {
    /// Run the command line immediately (no-arg verbs like undo/redo, or a
    /// wired File/Edit action such as `save`/`open`).
    Execute(String),
    /// Start the interactive draw tool for a bare draw verb (line/rect/…).
    StartDraw(String),
    /// Insert `verb ` into the command line for the user to complete.
    Insert(String),
    /// Print the command reference to the scrollback.
    Help,
    /// Show the About dialog.
    About,
    /// Open the Model Setup panel (download/manage local models). Available any
    /// time from Tools, not just at first run.
    ModelSetup,
    /// Open the Edit history / amend panel as a modal (the command line is the
    /// op-log scrollback; this exposes step-jump + amend on demand).
    EditHistory,
    /// Start a new empty document (fresh model, keeps the current chat session).
    NewDocument,
    /// Start a new file session: fresh document AND a fresh chat/deck session
    /// (drops the provider conversation handle and transcript).
    NewSession,
    /// Pop a native file-open dialog for import, then run `import <path>` through
    /// the substrate. Fired by the File → Import… menu item.
    ImportDialog,
    /// Pop a native file-save dialog for export, then run `export <path>` through
    /// the substrate. Fired by the File → Export… menu item.
    ExportDialog,
    /// Set the app appearance theme from the native View ▸ Appearance items.
    /// `Some(true)` = Dark, `Some(false)` = Light, `None` = follow the OS.
    /// UI/session state (not the op-log): drives the existing theme pin.
    SetTheme(Option<bool>),
    /// Step the app text size (egui zoom factor) one notch, clamped 0.5–3.0.
    /// `true` = larger (⌘+), `false` = smaller (⌘-). UI state, not op-log.
    ZoomStep(bool),
    /// Reset the app text size (egui zoom factor) to the default (⌘0).
    ZoomReset,
    /// Open the ⌘K command palette overlay (fuzzy-search every verb).
    CommandPalette,
}

/// Draw-tool verbs (mirror `draw_tool::try_start`). A menu pick of one of these
/// starts the interactive picker rather than typing text.
const DRAW_VERBS: [&str; 4] = ["line", "polyline", "rect", "circle"];

/// The keyboard shortcut shown next to a menu item, if it has one. This is the
/// SINGLE source of truth for menu accelerators — both the in-window bar and the
/// native (muda) bar read it, and every string here mirrors an actual binding in
/// [`crate::keymap::keymap`] so the two never drift (asserted in tests).
///
/// The display uses the macOS convention (`Cmd`, `Shift`, `Delete`); the native
/// layer parses the same string into a muda [`muda::accelerator::Accelerator`].
/// Bare single-letter draw shortcuts (L/C/P/R, G) are intentionally NOT surfaced
/// in menus — they are modeless tool hotkeys, not menu accelerators, and showing
/// them would clutter every draw item with a lone letter.
pub fn menu_shortcut(verb: &str) -> Option<&'static str> {
    match verb {
        "save" => Some("Cmd+S"),
        "undo" => Some("Cmd+Z"),
        "redo" => Some("Cmd+Shift+Z"),
        "select all" | "selectall" => Some("Cmd+A"),
        "copyselection" | "copy" => Some("Cmd+C"),
        "pasteselection" => Some("Cmd+V"),
        "delete" => Some("Delete"),
        _ => None,
    }
}

/// The accelerator shown for a wired app action (as opposed to a registry verb)
/// keyed by its [`MenuAction`]. Zoom (⌘= / ⌘- / ⌘0) and Settings (⌘,) live here
/// because they are app verbs, not registry verbs. Kept beside [`menu_shortcut`]
/// so all menu accelerators have one home.
pub fn action_shortcut(action: &MenuAction) -> Option<&'static str> {
    match action {
        MenuAction::ZoomStep(true) => Some("Cmd+="),
        MenuAction::ZoomStep(false) => Some("Cmd+-"),
        MenuAction::ZoomReset => Some("Cmd+0"),
        MenuAction::ModelSetup => Some("Cmd+,"),
        MenuAction::CommandPalette => Some("Cmd+K"),
        MenuAction::Execute(v) if v == "undo" => Some("Cmd+Z"),
        MenuAction::Execute(v) if v == "redo" => Some("Cmd+Shift+Z"),
        _ => None,
    }
}

/// Whether a registry verb operates on the current selection, so a menu should
/// DIM (disable) rather than hide it when nothing is selected. This teaches
/// capability: the user sees Move/Rotate/… exist but greyed until they pick
/// something. Curated to the modify/transform/boolean verbs that consume a
/// selection; drawing and file/view verbs stay always-enabled.
pub fn needs_selection(verb: &str) -> bool {
    matches!(
        verb,
        "move"
            | "copy"
            | "cut"
            | "deselect"
            | "rotate"
            | "scale"
            | "mirror"
            | "array"
            | "polararray"
            | "delete"
            | "trim"
            | "extend"
            | "split"
            | "join"
            | "fillet"
            | "offset"
            | "group"
            | "ungroup"
            | "union"
            | "difference"
            | "intersect"
            | "hideobj"
            | "showobj"
    )
}


// ── Menu iconography ─────────────────────────────────────────────────────────
// Lucide (ISC-licensed, FOSS) line icons give the menus a clean, consistent-
// stroke scannable column — one icon per action / per registry category so
// related verbs read as a group. See `crate::icons`.

/// A Lucide [`Icon`] for a registry verb, chosen by its [`Category`] so every
/// verb in a menu group shares a mark. A few high-traffic verbs get a specific
/// icon; the rest fall back to their category mark so the menu stays grouped.
fn verb_icon(verb: &str) -> Icon {
    match verb {
        "line" | "polyline" => return Icon::Line,
        "rect" => return Icon::Rect,
        "circle" => return Icon::CircleShape,
        "box" => return Icon::BoxShape,
        "move" => return Icon::Move,
        "copy" => return Icon::Copy,
        "rotate" => return Icon::Rotate,
        "scale" => return Icon::Scale,
        "mirror" => return Icon::Mirror,
        _ => {}
    }
    registry()
        .iter()
        .find(|s| s.name == verb)
        .map(|s| category_icon(s.category))
        .unwrap_or(Icon::ToolsCat)
}

/// One Lucide [`Icon`] per command [`Category`] — the group mark used when a
/// verb has no specific icon.
fn category_icon(cat: Category) -> Icon {
    match cat {
        Category::File => Icon::Open,
        Category::Edit => Icon::EditCat,
        Category::View => Icon::View,
        Category::Draw2d => Icon::Line,
        Category::Curve => Icon::Curve,
        Category::Solid => Icon::Solid,
        Category::Boolean => Icon::Boolean,
        Category::Transform => Icon::Transform,
        Category::Annotate => Icon::Annotate,
        Category::Dimension => Icon::Dimension,
        Category::Analyze => Icon::Analyze,
        Category::Structure => Icon::Structure,
        Category::Tools => Icon::ToolsCat,
    }
}

/// Classify a registry verb into a [`MenuAction`]. Pure: depends only on the
/// verb name and its registry usage string.
pub fn menu_action(verb: &str) -> MenuAction {
    if DRAW_VERBS.contains(&verb) {
        return MenuAction::StartDraw(verb.to_string());
    }
    // Import/Export need a path; picking them from a menu pops a native file
    // dialog first, then runs the command WITH the chosen path through the
    // substrate (rather than prefilling the command line for typing).
    match verb {
        "import" => return MenuAction::ImportDialog,
        "export" => return MenuAction::ExportDialog,
        _ => {}
    }
    // No angle-bracket placeholder in the usage ⇒ the verb takes no required
    // argument, so it can run straight away.
    let takes_args = registry()
        .iter()
        .find(|s| s.name == verb)
        .map(|s| s.usage.contains('<'))
        .unwrap_or(true);
    if takes_args {
        MenuAction::Insert(format!("{verb} "))
    } else {
        MenuAction::Execute(verb.to_string())
    }
}

/// The minimal menu bar's top-level titles, in order (before the platform Window
/// menu and the synthetic Help). Deliberately SHORT: the command palette (⌘K),
/// the command line, and the LLM deck do the heavy lifting — the menu bar is NOT
/// a command catalog. The geometry category menus (Draw / Curve / Solid /
/// Transform / …) were removed; the registry still drives the palette, deck
/// prompt, and autosuggest.
#[allow(dead_code)] // documented contract; referenced by tests
pub const TOP_TITLES: [&str; 3] = ["File", "Edit", "View"];

// ── Native menu model (muda) ─────────────────────────────────────────────────
// The in-window egui menu bar (`ui` below) and the true native OS menu bar
// (`crate::native_menu`, muda) render from ONE description so they never drift.
// A `NativeMenuModel` is a pure, egui-free tree: top-level menus, each holding
// items (leaf verbs / wired actions) and separators. Each leaf carries a stable
// string `id` and the `MenuAction` it dispatches through the substrate — the
// same actions `apply_menu_action` already routes. This function is unit-tested
// standalone; the muda layer merely walks it.

/// One row in a native menu: a clickable leaf, or a separator between groups.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NativeItem {
    /// A clickable menu entry: stable `id` (used as the muda `MenuId`), the label
    /// shown, and the [`MenuAction`] dispatched when chosen.
    Leaf {
        id: String,
        label: String,
        action: MenuAction,
        /// Keyboard accelerator string (e.g. `"Cmd+S"`), sourced from the keymap
        /// via [`menu_shortcut`] / [`action_shortcut`] so menus stay in sync.
        /// `None` for items with no binding.
        shortcut: Option<String>,
        /// False ⇒ the item renders DIMMED (disabled). Selection-dependent verbs
        /// (see [`needs_selection`]) are disabled when nothing is selected — the
        /// menu still teaches the capability instead of hiding it.
        enabled: bool,
    },
    /// A native predefined item (macOS Minimize / Zoom / Bring All to Front, etc).
    /// Carried in the model so tests can see the Window menu; the muda layer maps
    /// each kind to a [`muda::PredefinedMenuItem`]. Has no [`MenuAction`] — the OS
    /// handles it.
    Predefined(PredefinedKind),
    /// A visual divider between item groups.
    Separator,
}

/// A macOS-standard predefined menu item the OS implements directly (no
/// [`MenuAction`]). Used for the Window menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PredefinedKind {
    /// Minimize the window (⌘M).
    Minimize,
    /// Zoom (macOS green-button behaviour).
    Zoom,
    /// Bring all app windows to the front.
    BringAllToFront,
    /// Enter/Exit Full Screen (⌃⌘F on macOS; label toggles natively).
    Fullscreen,
    /// Quit the application (⌘Q). On macOS the app menu owns Quit; on other
    /// platforms it is surfaced at the foot of the File menu.
    #[cfg_attr(target_os = "macos", allow(dead_code))]
    Quit,
}

/// A top-level native menu (e.g. "File") and its ordered rows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeMenu {
    pub title: String,
    pub items: Vec<NativeItem>,
}

/// The complete native menu bar: the ordered top-level menus.
///
/// Built from the SAME source as the in-window bar — [`top_menus`] grouping plus
/// the wired File/Edit/Tools/Help actions — so the two bars stay identical. The
/// muda layer ([`crate::native_menu`]) walks this to build the OS menu; picking
/// an item emits its [`NativeItem::Leaf`] `action` through the op-log path.
///
/// Note: rich, egui-only rows (Appearance dark/light toggle + text-size stepper,
/// color swatches) are intentionally NOT here — those stay in-window because they
/// cannot be native menu items. Standard verb menus go native.
/// The Appearance rows injected into the native View menu: theme (Light / Dark /
/// System) followed by the text-size stepper (Increase / Decrease / Reset). Each
/// is `(id_suffix, label, MenuAction)` and routes through `apply_menu_action`,
/// the same substrate dispatch every other menu pick uses. These replace the old
/// in-window Appearance strip on macOS/Windows (Linux keeps the strip — no native
/// bar). Kept as a standalone fn so it is unit-testable.
pub fn appearance_native_items() -> Vec<(&'static str, &'static str, MenuAction)> {
    vec![
        ("theme_light", "Appearance: Light", MenuAction::SetTheme(Some(false))),
        ("theme_dark", "Appearance: Dark", MenuAction::SetTheme(Some(true))),
        ("theme_system", "Appearance: System", MenuAction::SetTheme(None)),
        ("text_bigger", "Text Size: Increase", MenuAction::ZoomStep(true)),
        ("text_smaller", "Text Size: Decrease", MenuAction::ZoomStep(false)),
        ("text_reset", "Text Size: Reset", MenuAction::ZoomReset),
    ]
}

/// Build one leaf whose accelerator comes from [`action_shortcut`] for `action`.
fn wired_leaf(title: &str, id: &str, label: &str, action: MenuAction) -> NativeItem {
    NativeItem::Leaf {
        id: format!("{title}/{id}"),
        label: label.to_string(),
        shortcut: action_shortcut(&action).map(str::to_string),
        enabled: true,
        action,
    }
}

/// The minimal menu bar model — the SINGLE source both the native (muda) bar and
/// the in-window fallback render from, so the two never drift.
///
/// Exactly five menus: **File / Edit / View** (built here), the platform
/// **Window** menu (macOS), and the synthetic **Help**. No geometry category
/// menus — the registry still drives the ⌘K palette, the deck prompt, and
/// autosuggest, but the bar is intentionally lean.
///
/// `has_selection` drives disable-don't-hide on the selection-dependent Edit
/// items (Cut/Copy/Paste/Delete/Deselect): they stay visible but dimmed when
/// nothing is selected, teaching the capability. The `_style` argument is kept
/// for signature stability (the minimal bar is identical across presets).
pub fn native_model(_style: MenuStyle, has_selection: bool) -> Vec<NativeMenu> {
    let mut menus: Vec<NativeMenu> = Vec::new();

    // ── File ────────────────────────────────────────────────────────────────
    // New, Open…, Save, Save As…, Import…, Export…, Settings (⌘,), Quit.
    {
        let t = "File";
        #[cfg_attr(target_os = "macos", allow(unused_mut))]
        let mut items = vec![
            wired_leaf(t, "new", "New", MenuAction::NewDocument),
            wired_leaf(t, "new_session", "New file session", MenuAction::NewSession),
            NativeItem::Separator,
            NativeItem::Leaf {
                id: format!("{t}/open"),
                label: "Open…".into(),
                shortcut: menu_shortcut("open").map(str::to_string),
                enabled: true,
                action: menu_action("open"),
            },
            NativeItem::Leaf {
                id: format!("{t}/save"),
                label: "Save".into(),
                shortcut: menu_shortcut("save").map(str::to_string),
                enabled: true,
                action: menu_action("save"),
            },
            NativeItem::Leaf {
                id: format!("{t}/saveas"),
                label: "Save As…".into(),
                shortcut: None,
                enabled: true,
                // `save ` prefilled so the user supplies a path (Save As).
                action: MenuAction::Insert("save ".into()),
            },
            NativeItem::Separator,
            NativeItem::Leaf {
                id: format!("{t}/import"),
                label: "Import…".into(),
                shortcut: None,
                enabled: true,
                action: MenuAction::ImportDialog,
            },
            NativeItem::Leaf {
                id: format!("{t}/export"),
                label: "Export…".into(),
                shortcut: None,
                enabled: true,
                action: MenuAction::ExportDialog,
            },
            NativeItem::Separator,
            wired_leaf(t, "settings", "Settings…", MenuAction::ModelSetup),
        ];
        // macOS puts Quit in the app menu automatically; on other platforms we
        // surface it here. AppKit still offers ⌘Q regardless.
        #[cfg(not(target_os = "macos"))]
        {
            items.push(NativeItem::Separator);
            items.push(NativeItem::Predefined(PredefinedKind::Quit));
        }
        menus.push(NativeMenu { title: t.into(), items });
    }

    // ── Edit ────────────────────────────────────────────────────────────────
    // Undo, Redo, Cut, Copy, Paste, Delete, Select All, Deselect, Edit history…
    {
        let t = "Edit";
        // A selection-dependent Edit leaf: dimmed when nothing is selected.
        let sel_leaf = |id: &str, label: &str, action: MenuAction, shortcut: Option<&str>| {
            NativeItem::Leaf {
                id: format!("{t}/{id}"),
                label: label.into(),
                shortcut: shortcut.map(str::to_string),
                enabled: has_selection,
                action,
            }
        };
        let items = vec![
            wired_leaf(t, "undo", "Undo", MenuAction::Execute("undo".into())),
            wired_leaf(t, "redo", "Redo", MenuAction::Execute("redo".into())),
            NativeItem::Separator,
            // Cut = copy-selection then delete-selection (app clipboard verbs).
            sel_leaf("cut", "Cut", MenuAction::Execute("cut".into()), Some("Cmd+X")),
            sel_leaf("copy", "Copy", MenuAction::Execute("copyselection".into()), Some("Cmd+C")),
            NativeItem::Leaf {
                id: format!("{t}/paste"),
                label: "Paste".into(),
                shortcut: Some("Cmd+V".into()),
                enabled: true, // paste doesn't need a selection
                action: MenuAction::Execute("pasteselection".into()),
            },
            sel_leaf("delete", "Delete", MenuAction::Execute("delete sel".into()), Some("Delete")),
            NativeItem::Separator,
            NativeItem::Leaf {
                id: format!("{t}/selectall"),
                label: "Select All".into(),
                shortcut: menu_shortcut("select all").map(str::to_string),
                enabled: true,
                action: MenuAction::Execute("select all".into()),
            },
            sel_leaf("deselect", "Deselect", MenuAction::Execute("selectnone".into()), None),
            NativeItem::Separator,
            NativeItem::Leaf {
                id: format!("{t}/history"),
                label: "Edit history…".into(),
                shortcut: None,
                enabled: true,
                action: MenuAction::EditHistory,
            },
        ];
        menus.push(NativeMenu { title: t.into(), items });
    }

    // ── View ─────────────────────────────────────────────────────────────────
    // Display mode, lighting mode, viewports 1/2/4, standard views, Zoom
    // Extents, Appearance (Light/Dark/System), Text Size, Command Palette.
    {
        let t = "View";
        let ex = |id: &str, label: &str, line: &str| NativeItem::Leaf {
            id: format!("{t}/{id}"),
            label: label.into(),
            shortcut: None,
            enabled: true,
            action: MenuAction::Execute(line.into()),
        };
        let mut items = vec![
            // Command palette is the primary discoverability surface.
            wired_leaf(t, "palette", "Command Palette…", MenuAction::CommandPalette),
            NativeItem::Separator,
            // Display modes.
            ex("disp_shaded", "Display: Shaded", "display shaded"),
            ex("disp_wire", "Display: Wireframe", "display wireframe"),
            ex("disp_xray", "Display: X-ray", "display xray"),
            ex("disp_pencil", "Display: Pencil", "display pencil"),
            NativeItem::Separator,
            // Lighting modes.
            ex("light_working", "Lighting: Working", "lightmode working"),
            ex("light_sun", "Lighting: Sun", "lightmode sun"),
            ex("light_present", "Lighting: Presentation", "lightmode presentation"),
            NativeItem::Separator,
            // Viewport layout.
            ex("vp1", "Viewports: 1", "viewports 1"),
            ex("vp2", "Viewports: 2", "viewports 2"),
            ex("vp4", "Viewports: 4", "viewports 4"),
            NativeItem::Separator,
            // Standard views.
            ex("v_top", "Top", "top"),
            ex("v_front", "Front", "front"),
            ex("v_right", "Right", "right"),
            ex("v_persp", "Perspective", "persp"),
            NativeItem::Separator,
            ex("ze", "Zoom Extents", "ze"),
            NativeItem::Separator,
        ];
        // Appearance (theme + text size) — egui-only in-window elsewhere, but the
        // native bar carries them as ordinary routed leaves.
        for (id, label, action) in appearance_native_items() {
            items.push(wired_leaf(t, id, label, action));
        }
        menus.push(NativeMenu { title: t.into(), items });
    }

    // ── Window (macOS) ────────────────────────────────────────────────────────
    // Standard OS-handled items: Minimize ⌘M / Zoom / Bring All to Front / Full
    // Screen. No MenuAction — AppKit implements them.
    #[cfg(target_os = "macos")]
    menus.push(NativeMenu {
        title: "Window".to_string(),
        items: vec![
            NativeItem::Predefined(PredefinedKind::Minimize),
            NativeItem::Predefined(PredefinedKind::Zoom),
            NativeItem::Separator,
            NativeItem::Predefined(PredefinedKind::Fullscreen),
            NativeItem::Separator,
            NativeItem::Predefined(PredefinedKind::BringAllToFront),
        ],
    });

    // ── Help ───────────────────────────────────────────────────────────────────
    // Docs, Command reference, About.
    menus.push(NativeMenu {
        title: "Help".to_string(),
        items: vec![
            NativeItem::Leaf {
                id: "Help/docs".into(),
                label: "Docs".into(),
                action: MenuAction::Insert("help ".into()),
                shortcut: None,
                enabled: true,
            },
            NativeItem::Leaf {
                id: "Help/reference".into(),
                label: "Command reference".into(),
                action: MenuAction::Help,
                shortcut: None,
                enabled: true,
            },
            NativeItem::Separator,
            NativeItem::Leaf {
                id: "Help/palette".into(),
                label: "Command Palette…".into(),
                action: MenuAction::CommandPalette,
                shortcut: action_shortcut(&MenuAction::CommandPalette).map(str::to_string),
                enabled: true,
            },
            NativeItem::Separator,
            NativeItem::Leaf {
                id: "Help/about".into(),
                label: "About ItsJustCAD".into(),
                action: MenuAction::About,
                shortcut: None,
                enabled: true,
            },
        ],
    });
    menus
}

/// Registry verbs belonging to any of `cats`, in registry order. Test-only now
/// that the menu bar no longer groups by category (the registry still feeds the
/// palette / deck prompt directly).
#[cfg(test)]
pub fn verbs_in(cats: &[Category]) -> Vec<&'static str> {
    registry()
        .iter()
        .filter(|s| cats.contains(&s.category))
        .map(|s| s.name)
        .collect()
}

/// A Lucide [`Icon`] for a menu leaf, chosen by its label / id so the in-window
/// bar keeps a scannable icon column. Falls back to a neutral mark.
fn leaf_icon(id: &str, label: &str) -> Icon {
    match label {
        "New" => return Icon::New,
        "New file session" => return Icon::NewSession,
        "Open…" => return Icon::Open,
        "Save" | "Save As…" => return Icon::Save,
        "Import…" => return Icon::Import,
        "Export…" => return Icon::Export,
        "Settings…" => return Icon::Model,
        "Undo" => return Icon::Undo,
        "Redo" => return Icon::Redo,
        "Edit history…" => return Icon::History,
        "Command reference" | "Docs" => return Icon::Help,
        "About ItsJustCAD" => return Icon::About,
        _ => {}
    }
    verb_icon(id.rsplit('/').next().unwrap_or(""))
}

/// Draw the minimal menu bar in-window (fallback when no native OS bar). Renders
/// the SAME [`native_model`] the native bar uses, so the two never drift. Returns
/// the action the user picked this frame, if any.
pub fn ui(
    ui: &mut egui::Ui,
    icons: &Icons,
    style: MenuStyle,
    has_selection: bool,
) -> Option<MenuAction> {
    let mut action = None;
    let model = native_model(style, has_selection);
    egui::MenuBar::new().ui(ui, |ui| {
        for menu in &model {
            ui.menu_button(&menu.title, |ui| {
                for it in &menu.items {
                    match it {
                        NativeItem::Separator => {
                            ui.separator();
                        }
                        NativeItem::Predefined(kind) => {
                            let label = match kind {
                                PredefinedKind::Minimize => "Minimize",
                                PredefinedKind::Zoom => "Zoom",
                                PredefinedKind::BringAllToFront => "Bring All to Front",
                                PredefinedKind::Fullscreen => "Toggle Full Screen",
                                PredefinedKind::Quit => "Quit",
                            };
                            let _ = ui.button(label);
                        }
                        NativeItem::Leaf { id, label, action: a, shortcut, enabled } => {
                            if icons
                                .menu_item_ex(ui, leaf_icon(id, label), label, shortcut.as_deref(), *enabled)
                                .clicked()
                            {
                                action = Some(a.clone());
                                ui.close();
                            }
                        }
                    }
                }
            });
        }
        appearance_controls(ui, icons);
    });
    action
}


/// Right-aligned Appearance controls: dark/light toggle + text-size stepper,
/// applied app-wide. Factored out because these are egui-only widgets that
/// CANNOT be native menu items — when the true native OS menu bar (muda) is
/// attached, we still render this slim in-window strip for them (see
/// [`appearance_only`]).
fn appearance_controls(ui: &mut egui::Ui, icons: &Icons) {
    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
        let zoom = ui.ctx().zoom_factor();
        if ui
            .small_button("A+")
            .on_hover_text("bigger text (Cmd =)")
            .clicked()
        {
            ui.ctx().set_zoom_factor((zoom + 0.1).min(3.0));
        }
        ui.label(format!("{:.0}%", zoom * 100.0));
        if ui
            .small_button("A−")
            .on_hover_text("smaller text (Cmd -)")
            .clicked()
        {
            ui.ctx().set_zoom_factor((zoom - 0.1).max(0.5));
        }
        ui.separator();
        egui::widgets::global_theme_preference_switch(ui);
        // Lucide sun-moon mark labelling the light/dark toggle.
        let size = ui.text_style_height(&egui::TextStyle::Body);
        let color = ui.visuals().weak_text_color();
        ui.add(icons.image(ui.ctx(), Icon::Theme, size, color));
    });
}

/// Dev/screenshot hook: render one top-level menu of the minimal bar as an open
/// dropdown-style panel just under the bar, so `ITSJUSTCAD_SHOT` frames can show
/// a menu without a live click. Set `ITSJUSTCAD_MENU_DEMO=<title>` (File / Edit /
/// View / Help). Faithful — it walks the same [`native_model`] the real bar does.
pub fn demo_open(
    ctx: &egui::Context,
    icons: &Icons,
    style: MenuStyle,
    title: &str,
    at: egui::Pos2,
) {
    // Demo with an EMPTY selection so disable-not-hide (dimmed Cut/Copy/Delete
    // with their shortcut hints) is visible in the shot.
    let Some(menu) = native_model(style, false).into_iter().find(|m| m.title == title) else {
        return;
    };
    egui::Area::new(egui::Id::new("menu_demo"))
        .fixed_pos(at)
        .show(ctx, |ui| {
            egui::Frame::menu(ui.style()).show(ui, |ui| {
                ui.set_min_width(220.0);
                ui.label(egui::RichText::new(&menu.title).strong());
                ui.separator();
                for it in &menu.items {
                    match it {
                        NativeItem::Separator => {
                            ui.separator();
                        }
                        NativeItem::Predefined(_) => {}
                        NativeItem::Leaf { id, label, shortcut, enabled, .. } => {
                            let _ = icons.menu_item_ex(
                                ui,
                                leaf_icon(id, label),
                                label,
                                shortcut.as_deref(),
                                *enabled,
                            );
                        }
                    }
                }
            });
        });
}


#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    /// Collect all leaf (id, label, action) triples from a native model.
    fn leaves(menus: &[NativeMenu]) -> Vec<(String, String, MenuAction)> {
        menus
            .iter()
            .flat_map(|m| &m.items)
            .filter_map(|it| match it {
                NativeItem::Leaf { id, label, action, .. } => {
                    Some((id.clone(), label.clone(), action.clone()))
                }
                NativeItem::Separator | NativeItem::Predefined(_) => None,
            })
            .collect()
    }

    // MENU BAR IS MINIMAL: exactly File / Edit / View (+ Window on macOS) + Help.
    // No geometry category menus.

    #[test]
    fn menu_bar_is_exactly_five_menus() {
        // The three geometry-free top titles are the documented contract.
        assert_eq!(TOP_TITLES, ["File", "Edit", "View"]);
        for style in [MenuStyle::Rhino, MenuStyle::AutoCAD] {
            let titles: Vec<String> =
                native_model(style, true).iter().map(|m| m.title.clone()).collect();
            let mut expected = vec!["File".to_string(), "Edit".into(), "View".into()];
            #[cfg(target_os = "macos")]
            expected.push("Window".to_string());
            expected.push("Help".to_string());
            assert_eq!(titles, expected, "menu titles differ for {style:?}");
        }
    }

    #[test]
    fn no_geometry_category_menus_present() {
        // The removed category menus (Draw/Curve/Solid/Transform/Modify/Dimension/
        // Analyze/Structure/Tools/Format) must not appear as top-level menus.
        let banned = [
            "Draw", "Curve", "Solid", "Transform", "Modify", "Dimension", "Annotate",
            "Analyze", "Structure", "Tools", "Format", "Boolean", "Plugins",
        ];
        for style in [MenuStyle::Rhino, MenuStyle::AutoCAD] {
            for m in native_model(style, true) {
                assert!(!banned.contains(&m.title.as_str()), "banned menu {} present", m.title);
            }
        }
    }

    #[test]
    fn menu_bar_identical_across_presets() {
        // The minimal bar ignores the preset style.
        assert_eq!(native_model(MenuStyle::Rhino, true), native_model(MenuStyle::AutoCAD, true));
    }

    #[test]
    fn file_menu_has_curated_items() {
        let file = native_model(MenuStyle::Rhino, true)
            .into_iter()
            .find(|m| m.title == "File")
            .unwrap();
        let ls = leaves(&[file]);
        let has = |label: &str| ls.iter().any(|(_, l, _)| l == label);
        assert!(has("New"));
        assert!(has("Open\u{2026}"));
        assert!(has("Save"));
        assert!(has("Save As\u{2026}"));
        assert!(has("Import\u{2026}"));
        assert!(has("Export\u{2026}"));
        assert!(has("Settings\u{2026}"));
        // Import/Export route to native dialogs.
        assert!(ls.iter().any(|(_, l, a)| l == "Import\u{2026}" && *a == MenuAction::ImportDialog));
        assert!(ls.iter().any(|(_, l, a)| l == "Export\u{2026}" && *a == MenuAction::ExportDialog));
        // Save As prefills `save ` for a path.
        assert!(ls.iter().any(|(_, l, a)| l == "Save As\u{2026}" && *a == MenuAction::Insert("save ".into())));
    }

    #[test]
    fn edit_menu_has_curated_items() {
        let edit = native_model(MenuStyle::Rhino, true)
            .into_iter()
            .find(|m| m.title == "Edit")
            .unwrap();
        let ls = leaves(&[edit]);
        for label in ["Undo", "Redo", "Cut", "Copy", "Paste", "Delete", "Select All", "Deselect", "Edit history\u{2026}"] {
            assert!(ls.iter().any(|(_, l, _)| l == label), "Edit missing {label}");
        }
        assert!(ls.iter().any(|(_, l, a)| l == "Undo" && *a == MenuAction::Execute("undo".into())));
        assert!(ls.iter().any(|(_, l, a)| l == "Copy" && *a == MenuAction::Execute("copyselection".into())));
        assert!(ls.iter().any(|(_, l, a)| l == "Paste" && *a == MenuAction::Execute("pasteselection".into())));
        assert!(ls.iter().any(|(_, l, a)| l == "Delete" && *a == MenuAction::Execute("delete sel".into())));
        assert!(ls.iter().any(|(_, l, a)| l == "Select All" && *a == MenuAction::Execute("select all".into())));
    }

    #[test]
    fn view_menu_has_display_lighting_viewports_views_and_palette() {
        let view = native_model(MenuStyle::Rhino, true)
            .into_iter()
            .find(|m| m.title == "View")
            .unwrap();
        let ls = leaves(&[view]);
        let by_action = |a: &MenuAction| ls.iter().any(|(_, _, act)| act == a);
        assert!(by_action(&MenuAction::Execute("display shaded".into())), "display mode missing");
        assert!(by_action(&MenuAction::Execute("lightmode sun".into())), "lighting missing");
        assert!(by_action(&MenuAction::Execute("viewports 4".into())), "viewport layout missing");
        assert!(by_action(&MenuAction::Execute("top".into())), "standard view missing");
        assert!(by_action(&MenuAction::Execute("ze".into())), "zoom extents missing");
        assert!(by_action(&MenuAction::CommandPalette), "command palette entry missing");
        // Appearance rows still present.
        assert!(by_action(&MenuAction::SetTheme(Some(true))), "dark theme missing");
        assert!(by_action(&MenuAction::ZoomStep(true)), "text size step missing");
    }

    #[test]
    fn help_menu_has_docs_reference_palette_about() {
        let help = native_model(MenuStyle::AutoCAD, true)
            .into_iter()
            .find(|m| m.title == "Help")
            .unwrap();
        let ls = leaves(&[help]);
        assert!(ls.iter().any(|(_, l, _)| l == "Docs"));
        assert!(ls.iter().any(|(_, l, a)| l == "Command reference" && *a == MenuAction::Help));
        assert!(ls.iter().any(|(_, _, a)| *a == MenuAction::CommandPalette));
        assert!(ls.iter().any(|(_, l, a)| l == "About ItsJustCAD" && *a == MenuAction::About));
    }

    #[test]
    fn command_palette_bound_to_cmd_k() {
        assert_eq!(action_shortcut(&MenuAction::CommandPalette), Some("Cmd+K"));
    }

    #[test]
    fn native_leaf_ids_are_unique() {
        for style in [MenuStyle::Rhino, MenuStyle::AutoCAD] {
            let ls = leaves(&native_model(style, true));
            let ids: HashSet<&String> = ls.iter().map(|(id, _, _)| id).collect();
            assert_eq!(ids.len(), ls.len(), "duplicate native menu id for {style:?}");
        }
    }

    #[test]
    fn no_show_tab_bar_item_anywhere() {
        for style in [MenuStyle::Rhino, MenuStyle::AutoCAD] {
            for (_, label, _) in leaves(&native_model(style, true)) {
                let l = label.to_lowercase();
                assert!(!l.contains("tab bar"), "found tab-bar item: {label}");
                assert!(!l.contains("all tabs"), "found all-tabs item: {label}");
            }
        }
    }

    // ── menu_action classification (registry verbs, used by the palette) ──────

    #[test]
    fn draw_verbs_start_the_draw_tool() {
        for v in ["line", "polyline", "rect", "circle"] {
            assert_eq!(menu_action(v), MenuAction::StartDraw(v.to_string()));
        }
    }

    #[test]
    fn no_arg_verbs_execute_immediately() {
        for v in ["undo", "redo", "selectnone", "blocks", "sunoff"] {
            assert_eq!(menu_action(v), MenuAction::Execute(v.to_string()));
        }
    }

    #[test]
    fn arg_verbs_are_inserted_with_trailing_space() {
        assert_eq!(menu_action("box"), MenuAction::Insert("box ".to_string()));
        assert_eq!(menu_action("move"), MenuAction::Insert("move ".to_string()));
    }

    #[test]
    fn import_export_route_to_native_dialog() {
        assert_eq!(menu_action("import"), MenuAction::ImportDialog);
        assert_eq!(menu_action("export"), MenuAction::ExportDialog);
    }

    #[test]
    fn verbs_in_returns_registry_members() {
        let solids = verbs_in(&[Category::Solid]);
        assert!(solids.contains(&"box"));
        assert!(solids.contains(&"extrude"));
        assert!(!solids.contains(&"line"));
    }

    // ── Disable-don't-hide on selection-dependent Edit items ─────────────────

    #[test]
    fn selection_edit_items_disabled_when_empty() {
        let sel_labels = ["Cut", "Copy", "Delete", "Deselect"];
        let empty = native_model(MenuStyle::Rhino, false);
        let filled = native_model(MenuStyle::Rhino, true);
        let enabled_of = |menus: &[NativeMenu], label: &str| -> Option<bool> {
            menus.iter().flat_map(|m| &m.items).find_map(|it| match it {
                NativeItem::Leaf { label: l, enabled, .. } if l == label => Some(*enabled),
                _ => None,
            })
        };
        for l in sel_labels {
            assert_eq!(enabled_of(&empty, l), Some(false), "{l} should be disabled when empty");
            assert_eq!(enabled_of(&filled, l), Some(true), "{l} should be enabled with a selection");
        }
        // Paste and Select All stay enabled regardless.
        assert_eq!(enabled_of(&empty, "Paste"), Some(true));
        assert_eq!(enabled_of(&empty, "Select All"), Some(true));
    }

    // ── Window menu (macOS) ──────────────────────────────────────────────────

    #[cfg(target_os = "macos")]
    #[test]
    fn window_menu_present_with_standard_items() {
        let win = native_model(MenuStyle::Rhino, false)
            .into_iter()
            .find(|m| m.title == "Window")
            .expect("Window menu present");
        let kinds: Vec<PredefinedKind> = win
            .items
            .iter()
            .filter_map(|it| match it {
                NativeItem::Predefined(k) => Some(*k),
                _ => None,
            })
            .collect();
        assert!(kinds.contains(&PredefinedKind::Minimize));
        assert!(kinds.contains(&PredefinedKind::Zoom));
        assert!(kinds.contains(&PredefinedKind::BringAllToFront));
        assert!(kinds.contains(&PredefinedKind::Fullscreen));
    }

    // ── Shortcut strings sourced from the keymap ─────────────────────────────

    #[test]
    fn menu_items_carry_shortcut_strings() {
        assert_eq!(menu_shortcut("save"), Some("Cmd+S"));
        assert_eq!(menu_shortcut("undo"), Some("Cmd+Z"));
        assert_eq!(menu_shortcut("redo"), Some("Cmd+Shift+Z"));
        assert_eq!(menu_shortcut("delete"), Some("Delete"));
        assert_eq!(menu_shortcut("line"), None);
        let file = native_model(MenuStyle::Rhino, true)
            .into_iter()
            .find(|m| m.title == "File")
            .unwrap();
        let save = file.items.iter().find_map(|it| match it {
            NativeItem::Leaf { label, shortcut, .. } if label == "Save" => Some(shortcut.clone()),
            _ => None,
        });
        assert_eq!(save, Some(Some("Cmd+S".to_string())));
    }

    #[test]
    fn zoom_and_settings_shortcuts_present() {
        assert_eq!(action_shortcut(&MenuAction::ModelSetup), Some("Cmd+,"));
        assert_eq!(action_shortcut(&MenuAction::ZoomStep(true)), Some("Cmd+="));
        assert_eq!(action_shortcut(&MenuAction::ZoomStep(false)), Some("Cmd+-"));
        assert_eq!(action_shortcut(&MenuAction::ZoomReset), Some("Cmd+0"));
    }

    #[test]
    fn menu_shortcuts_match_the_keymap() {
        use crate::keymap::{KeyContext, keymap};
        use egui::{Key, Modifiers};
        let cmd = Modifiers::COMMAND;
        let cmd_shift = Modifiers::COMMAND | Modifiers::SHIFT;
        let ctx = KeyContext {
            typing: false,
            draw_active: false,
            has_selection: true,
            last_command: None,
        };
        assert_eq!(keymap(Key::S, cmd, ctx).as_deref(), Some("save"));
        assert_eq!(menu_shortcut("save"), Some("Cmd+S"));
        assert_eq!(keymap(Key::Z, cmd, ctx).as_deref(), Some("undo"));
        assert_eq!(menu_shortcut("undo"), Some("Cmd+Z"));
        assert_eq!(keymap(Key::Z, cmd_shift, ctx).as_deref(), Some("redo"));
        assert_eq!(menu_shortcut("redo"), Some("Cmd+Shift+Z"));
        assert_eq!(keymap(Key::Delete, Modifiers::NONE, ctx).as_deref(), Some("delete sel"));
        assert_eq!(menu_shortcut("delete"), Some("Delete"));
        assert_eq!(keymap(Key::A, cmd, ctx).as_deref(), Some("select all"));
        assert_eq!(menu_shortcut("select all"), Some("Cmd+A"));
    }

    #[test]
    fn every_registry_verb_has_a_menu_icon() {
        for spec in registry() {
            assert!(!verb_icon(spec.name).name().is_empty(), "verb {} has no icon", spec.name);
        }
    }
}
