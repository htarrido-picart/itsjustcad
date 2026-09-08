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

use crate::i18n::Lang;
use crate::i18n::t as tr;
use crate::icons::{Icon, Icons};
use crate::preset::{CadOrigin, MenuStyle};

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
    /// Check GitHub Releases for a newer version and show the update popup.
    /// OFFLINE-first: reaches api.github.com ONLY on this explicit user pick.
    /// Fired by the Help ▸ Check for Updates… menu item.
    CheckForUpdates,
    /// Open the Model Setup panel (download/manage local models). Available any
    /// time from Tools, not just at first run.
    ModelSetup,
    /// Open the Plugins popup window: installed user/LLM-authored macros as
    /// cards, with search + per-card run/JSON/reload/delete. Modeless, like
    /// Model Setup / About. Fired by the top-level Plugins ▸ Plugins… menu item.
    ShowPlugins,
    /// Open the modeless object-snap popup (checkbox per snap kind + master +
    /// grid). Fired by the View ▸ Object Snap… menu item; the same popup the
    /// clickable status-bar osnap chip opens.
    ShowOsnap,
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
    /// Open the models directory (`~/.config/itsjustcad/models`) in the OS file
    /// manager. Fired by the LLM ▸ Reveal Models Folder item.
    RevealModelsFolder,
    /// Download the recommended default local model, if one is offered (not
    /// already installed / no download running / no healthy local deck). Fired by
    /// the LLM ▸ Download Default Model item.
    DownloadDefaultModel,
    /// Toggle "local only" (hide non-local decks / block remote calls). Checkable
    /// LLM-menu item; the app owns the live state and syncs the checkmark.
    ToggleLocalOnly,
    /// Toggle "allow web search" for the next turn. Checkable LLM-menu item;
    /// forced off (and disabled) while local-only is on.
    ToggleWebSearch,
    /// Toggle "terse replies" for the ACTIVE cassette (style rules + a hard
    /// max-token cap). Checkable LLM-menu item; defaults ON for local models.
    ToggleTerse,
    /// Toggle the right docked panel (Deck/chat + inspectors) visibility. The
    /// View menu shows this as a stateful "Hide Panel" ⇄ "Show Panel" flip and
    /// mirrors the ⌘\ hotkey. UI state, not op-log.
    TogglePanel,
    /// Switch the legacy-CAD skin (palette/fonts/accent + alias table). Radio in
    /// the Theme menu's Skin group; fires the same apply+persist path the
    /// `skin <name>` verb uses. UI/session state, not op-log.
    SetSkin(CadOrigin),
    /// Switch the UI language. Radio in the Theme menu's Language group; fires the
    /// same apply+persist path the `language en|es` verb uses. UI/session state,
    /// not op-log.
    SetLanguage(Lang),
    /// Quit the application (⌘Q). Routes through the dirty-doc guard, then closes
    /// the (single) window — which terminates the app. Present in the File menu on
    /// every platform so ⌘Q is always reachable from the menu bar; the macOS app
    /// menu additionally surfaces a native Quit.
    Quit,
    /// Close the current window/document (⌘W). This is a single-window app, so
    /// Close behaves like Quit — it routes through the same dirty-doc guard and
    /// closes the window. Kept a distinct variant so the accelerator (⌘W) and
    /// label read as the platform-standard Close.
    Close,
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
        // NB: cut/copy/paste intentionally have NO menu accelerator — a native
        // ⌘X/⌘C/⌘V would be grabbed by AppKit before the focused text field. The
        // keymap fires object clipboard on those keys only when not typing.
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
        MenuAction::TogglePanel => Some("Cmd+\\"),
        MenuAction::Quit => Some("Cmd+Q"),
        MenuAction::Close => Some("Cmd+W"),
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
pub const TOP_TITLES: [&str; 6] = ["File", "Edit", "View", "Theme", "LLM", "Plugins"];

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
    /// A checkable menu entry (rendered with a checkmark). `checked`/`enabled` in
    /// the model are INITIAL values; the app syncs the live state onto the native
    /// item's handle each frame (see `NativeMenuBar::sync_toggles`). The in-window
    /// bar reads the live state passed into [`ui`].
    Check {
        id: String,
        label: String,
        action: MenuAction,
        checked: bool,
        enabled: bool,
    },
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
    /// Quit the application (⌘Q). Retained for the in-window/`predefined` mapping
    /// and Linux fallback; the macOS app menu now carries a native Quit and the
    /// File menu carries a routed [`MenuAction::Quit`], so the model no longer
    /// emits this predefined variant directly.
    #[allow(dead_code)]
    Quit,
}

/// Live View-menu state that drives the stateful (radio / flip) items so the
/// menu reads as toggles, not fire-and-forget commands. Threaded into
/// [`native_model`] so the in-window bar renders the marks directly; the native
/// (muda) bar syncs these onto its check handles each frame
/// (`NativeMenuBar::sync_view_state`), mirroring the LLM-toggle pattern.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ViewState {
    /// Active display mode of the focused viewport (`Some` ⇒ that radio item is
    /// checked). Matched by the `display <mode>` command string.
    pub display: Option<DisplayModeTag>,
    /// Active lighting mode (`Some` ⇒ that radio item is checked). Matched by the
    /// `lightmode <mode>` command string.
    pub lighting: Option<LightModeTag>,
    /// Active camera projection of the focused viewport (`Some` ⇒ that radio
    /// item is checked; `None` for ortho standard views). Matched by the
    /// `camera <mode>` command string.
    pub camera: Option<CameraTag>,
    /// Whether the right docked panel is currently shown; flips the Panel item's
    /// label between "Hide Panel" and "Show Panel".
    pub panel_visible: bool,
    /// Active legacy-CAD skin — drives the check on the Theme ▸ Skin radio group.
    /// Matched against each `SetSkin(origin)` item's origin.
    pub skin: CadOrigin,
    /// Active UI language — drives the check on the Theme ▸ Language radio group.
    /// Matched against each `SetLanguage(lang)` item's lang.
    pub lang: Lang,
}

/// The display-mode radio choices the View menu offers, in menu order. A copy of
/// the render enum's identity kept menu-local so `menu.rs` needs no render dep;
/// the `cmd` is the command-line string each fires.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DisplayModeTag {
    Shaded,
    Wireframe,
    XRay,
    Pencil,
}

/// The lighting-mode radio choices the View menu offers, in menu order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LightModeTag {
    Working,
    Sun,
    Presentation,
}

/// The camera-projection radio choices the View menu offers, in menu order.
/// Mirrors the `camera <mode>` verb family (perspective / two-point / panorama /
/// fisheye); ortho standard views check nothing (projection is view-implied).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CameraTag {
    Perspective,
    TwoPoint,
    Panorama,
    Fisheye,
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
/// The rows that populate the top-level **Theme** menu: theme (Light / Dark /
/// System) followed by the text-size stepper (Increase / Decrease / Reset). Each
/// is `(id_suffix, label, MenuAction)` and routes through `apply_menu_action`,
/// the same substrate dispatch every other menu pick uses. On macOS/Windows these
/// are the native Theme menu's items (Linux keeps the in-window strip — no native
/// bar). Kept as a standalone fn so it is unit-testable.
pub fn appearance_native_items() -> Vec<(&'static str, &'static str, MenuAction)> {
    vec![
        ("theme_light", tr("menu.theme.light"), MenuAction::SetTheme(Some(false))),
        ("theme_dark", tr("menu.theme.dark"), MenuAction::SetTheme(Some(true))),
        ("theme_system", tr("menu.theme.system"), MenuAction::SetTheme(None)),
        ("text_bigger", tr("menu.theme.text_bigger"), MenuAction::ZoomStep(true)),
        ("text_smaller", tr("menu.theme.text_smaller"), MenuAction::ZoomStep(false)),
        ("text_reset", tr("menu.theme.text_reset"), MenuAction::ZoomReset),
    ]
}

/// The Theme ▸ Skin radio group: `(id_suffix, CadOrigin)` in picker order
/// (Native first). Each becomes a `Check` firing `SetSkin(origin)` — the same
/// apply+persist path the `skin <name>` verb uses. Stable ids (`skin_native` …)
/// keep tests / native routing language-independent; labels localize via the
/// skin's `label_key`. Standalone so it is unit-testable and shared by the native
/// + in-window bars.
pub fn skin_radio_items() -> [(&'static str, CadOrigin); 4] {
    [
        ("skin_native", CadOrigin::None),
        ("skin_autocad", CadOrigin::AutoCAD),
        ("skin_rhino", CadOrigin::Rhino),
        ("skin_revit", CadOrigin::Revit),
    ]
}

/// The Theme ▸ Language radio group: `(id_suffix, Lang)`. Each becomes a `Check`
/// firing `SetLanguage(lang)` — the same apply+persist path the `language en|es`
/// verb uses. Labels use each language's own `native_name` (English / Español).
pub fn lang_radio_items() -> [(&'static str, Lang); 2] {
    [("lang_en", Lang::En), ("lang_es", Lang::Es)]
}

/// Map a top-level menu's canonical English id (`"File"`, `"Edit"`, …) to its
/// i18n key so the displayed title localizes while ids/tests stay stable.
fn menu_title_key(id: &str) -> &'static str {
    match id {
        "File" => "menu.file",
        "Edit" => "menu.edit",
        "View" => "menu.view",
        "Theme" => "menu.theme",
        "LLM" => "menu.llm",
        "Plugins" => "menu.plugins",
        "Window" => "menu.window",
        "Help" => "menu.help",
        _ => "menu.file",
    }
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
pub fn native_model(_style: MenuStyle, has_selection: bool, view: ViewState) -> Vec<NativeMenu> {
    let mut menus: Vec<NativeMenu> = Vec::new();

    // ── File ────────────────────────────────────────────────────────────────
    // New, Open…, Save, Save As…, Import…, Export…, Settings (⌘,), Quit.
    {
        let t = "File";
        #[cfg_attr(target_os = "macos", allow(unused_mut))]
        let mut items = vec![
            wired_leaf(t, "new", tr("menu.file.new"), MenuAction::NewDocument),
            wired_leaf(t, "new_session", tr("menu.file.new_session"), MenuAction::NewSession),
            NativeItem::Separator,
            NativeItem::Leaf {
                id: format!("{t}/open"),
                label: tr("menu.file.open").into(),
                shortcut: menu_shortcut("open").map(str::to_string),
                enabled: true,
                action: menu_action("open"),
            },
            NativeItem::Leaf {
                id: format!("{t}/save"),
                label: tr("menu.file.save").into(),
                shortcut: menu_shortcut("save").map(str::to_string),
                enabled: true,
                action: menu_action("save"),
            },
            NativeItem::Leaf {
                id: format!("{t}/saveas"),
                label: tr("menu.file.save_as").into(),
                shortcut: None,
                enabled: true,
                // `save ` prefilled so the user supplies a path (Save As).
                action: MenuAction::Insert("save ".into()),
            },
            NativeItem::Separator,
            NativeItem::Leaf {
                id: format!("{t}/import"),
                label: tr("menu.file.import").into(),
                shortcut: None,
                enabled: true,
                action: MenuAction::ImportDialog,
            },
            NativeItem::Leaf {
                id: format!("{t}/export"),
                label: tr("menu.file.export").into(),
                shortcut: None,
                enabled: true,
                action: MenuAction::ExportDialog,
            },
            NativeItem::Separator,
            wired_leaf(t, "settings", tr("menu.file.settings"), MenuAction::ModelSetup),
            NativeItem::Separator,
            // Close (⌘W) closes the current window/document. Single-window app, so
            // it routes through the same dirty-doc guard as Quit.
            wired_leaf(t, "close", tr("menu.file.close"), MenuAction::Close),
            // Quit (⌘Q) is surfaced in the File menu on EVERY platform so it is
            // always reachable from the menu bar — the earlier `#[cfg(not(macos))]`
            // gate left macOS with no reachable menu-bar Quit. On macOS the native
            // app menu additionally carries a predefined Quit (see native_menu).
            wired_leaf(t, "quit", tr("menu.file.quit"), MenuAction::Quit),
        ];
        menus.push(NativeMenu { title: tr(menu_title_key(t)).into(), items });
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
            wired_leaf(t, "undo", tr("menu.edit.undo"), MenuAction::Execute("undo".into())),
            wired_leaf(t, "redo", tr("menu.edit.redo"), MenuAction::Execute("redo".into())),
            NativeItem::Separator,
            // Cut = copy-selection then delete-selection (app clipboard verbs).
            // NO ⌘X/⌘C/⌘V accelerators here: on macOS a native muda accelerator
            // is intercepted by AppKit BEFORE the focused text field, which stole
            // copy/paste from the chat + command-line inputs. The keymap still
            // fires object cut/copy/paste on ⌘X/⌘C/⌘V when NOT typing (it returns
            // None while a text field is focused), so text clipboard works in
            // fields and object clipboard works in the viewport — no conflict.
            sel_leaf("cut", tr("menu.edit.cut"), MenuAction::Execute("cut".into()), None),
            sel_leaf("copy", tr("menu.edit.copy"), MenuAction::Execute("copyselection".into()), None),
            NativeItem::Leaf {
                id: format!("{t}/paste"),
                label: tr("menu.edit.paste").into(),
                shortcut: None,
                enabled: true, // paste doesn't need a selection
                action: MenuAction::Execute("pasteselection".into()),
            },
            sel_leaf("delete", tr("menu.edit.delete"), MenuAction::Execute("delete sel".into()), Some("Delete")),
            NativeItem::Separator,
            NativeItem::Leaf {
                id: format!("{t}/selectall"),
                label: tr("menu.edit.select_all").into(),
                shortcut: menu_shortcut("select all").map(str::to_string),
                enabled: true,
                action: MenuAction::Execute("select all".into()),
            },
            sel_leaf("deselect", tr("menu.edit.deselect"), MenuAction::Execute("selectnone".into()), None),
            NativeItem::Separator,
            NativeItem::Leaf {
                id: format!("{t}/history"),
                label: tr("menu.edit.history").into(),
                shortcut: None,
                enabled: true,
                action: MenuAction::EditHistory,
            },
        ];
        menus.push(NativeMenu { title: tr(menu_title_key(t)).into(), items });
    }

    // ── View ─────────────────────────────────────────────────────────────────
    // Display mode, lighting mode, viewports 1/2/4, standard views, Zoom
    // Extents, Command Palette. (Appearance + Text Size live in the Theme menu.)
    {
        let t = "View";
        let ex = |id: &str, label: &str, line: &str| NativeItem::Leaf {
            id: format!("{t}/{id}"),
            label: label.into(),
            shortcut: None,
            enabled: true,
            action: MenuAction::Execute(line.into()),
        };
        // A radio-style display/lighting choice: shows a check when it is the
        // active mode. Rendered as a `Check`, the same mechanism the Theme menu's
        // Light/Dark/System items use, so the active mode is unmistakable.
        let radio = |id: &str, label: &str, line: &str, on: bool| NativeItem::Check {
            id: format!("{t}/{id}"),
            label: label.into(),
            action: MenuAction::Execute(line.into()),
            checked: on,
            enabled: true,
        };
        // Panel visibility is a stateful flip, not a fire-and-forget verb.
        let panel_item = NativeItem::Leaf {
            id: format!("{t}/panel"),
            label: if view.panel_visible { tr("menu.view.hide_panel").into() } else { tr("menu.view.show_panel").into() },
            shortcut: action_shortcut(&MenuAction::TogglePanel).map(str::to_string),
            enabled: true,
            action: MenuAction::TogglePanel,
        };
        let items = vec![
            // Command palette is the primary discoverability surface.
            wired_leaf(t, "palette", tr("menu.view.palette"), MenuAction::CommandPalette),
            NativeItem::Separator,
            // Panel visibility as a stateful "Hide Panel" ⇄ "Show Panel" flip.
            panel_item,
            NativeItem::Separator,
            // Display modes — radio: the active one carries a check.
            radio("disp_shaded", tr("menu.view.disp_shaded"), "display shaded", view.display == Some(DisplayModeTag::Shaded)),
            radio("disp_wire", tr("menu.view.disp_wire"), "display wireframe", view.display == Some(DisplayModeTag::Wireframe)),
            radio("disp_xray", tr("menu.view.disp_xray"), "display xray", view.display == Some(DisplayModeTag::XRay)),
            radio("disp_pencil", tr("menu.view.disp_pencil"), "display pencil", view.display == Some(DisplayModeTag::Pencil)),
            NativeItem::Separator,
            // Lighting modes — radio: the active one carries a check.
            radio("light_working", tr("menu.view.light_working"), "lightmode working", view.lighting == Some(LightModeTag::Working)),
            radio("light_sun", tr("menu.view.light_sun"), "lightmode sun", view.lighting == Some(LightModeTag::Sun)),
            radio("light_present", tr("menu.view.light_present"), "lightmode presentation", view.lighting == Some(LightModeTag::Presentation)),
            NativeItem::Separator,
            // Camera projections — radio: the active one carries a check. Ortho
            // standard views (top/front/…) check nothing; the projection there
            // is implied by the view. Fires the same `camera <mode>` verbs the
            // command line and the deck use.
            radio("cam_persp", tr("menu.view.cam_persp"), "camera persp", view.camera == Some(CameraTag::Perspective)),
            radio("cam_2point", tr("menu.view.cam_2point"), "camera 2point", view.camera == Some(CameraTag::TwoPoint)),
            radio("cam_pano", tr("menu.view.cam_pano"), "camera pano", view.camera == Some(CameraTag::Panorama)),
            radio("cam_fisheye", tr("menu.view.cam_fisheye"), "camera fisheye", view.camera == Some(CameraTag::Fisheye)),
            NativeItem::Separator,
            // Viewport layout.
            ex("vp1", tr("menu.view.vp1"), "viewports 1"),
            ex("vp2", tr("menu.view.vp2"), "viewports 2"),
            ex("vp4", tr("menu.view.vp4"), "viewports 4"),
            NativeItem::Separator,
            // Standard views.
            ex("v_top", tr("menu.view.top"), "top"),
            ex("v_front", tr("menu.view.front"), "front"),
            ex("v_right", tr("menu.view.right"), "right"),
            ex("v_persp", tr("menu.view.persp"), "persp"),
            NativeItem::Separator,
            ex("ze", tr("menu.view.zoom_extents"), "ze"),
            NativeItem::Separator,
            // Object Snap… opens the modeless osnap popup (per-kind toggles).
            wired_leaf(t, "osnap", tr("menu.view.osnap"), MenuAction::ShowOsnap),
        ];
        menus.push(NativeMenu { title: tr(menu_title_key(t)).into(), items });
    }

    // ── Theme ─────────────────────────────────────────────────────────────────
    // A dedicated top-level menu (same rank as View) carrying the egui-only
    // Appearance (Light / Dark / System) and Text Size (Increase / Decrease /
    // Reset) controls. Sourced from `appearance_native_items()` so the native
    // bar and the in-window fallback never drift; a separator splits the theme
    // group from the text-size group.
    {
        let t = "Theme";
        let mut items = Vec::new();
        for (i, (id, label, action)) in appearance_native_items().into_iter().enumerate() {
            if i == 3 {
                items.push(NativeItem::Separator);
            }
            items.push(wired_leaf(t, id, label, action));
        }
        // ── Skin radio group (CadOrigin) ─────────────────────────────────────
        // Native / AutoCAD / Rhino / Revit; the active skin carries the check.
        // Fires the same apply+persist path the `skin <name>` verb uses.
        items.push(NativeItem::Separator);
        for (id, origin) in skin_radio_items() {
            items.push(NativeItem::Check {
                id: format!("{t}/{id}"),
                label: tr(origin.label_key()).into(),
                action: MenuAction::SetSkin(origin),
                checked: view.skin == origin,
                enabled: true,
            });
        }
        // ── Language radio group (Lang) ──────────────────────────────────────
        // English / Español; the active language carries the check. Fires the
        // same apply+persist path the `language en|es` verb uses.
        items.push(NativeItem::Separator);
        for (id, lang) in lang_radio_items() {
            items.push(NativeItem::Check {
                id: format!("{t}/{id}"),
                label: lang.native_name().to_string(),
                action: MenuAction::SetLanguage(lang),
                checked: view.lang == lang,
                enabled: true,
            });
        }
        menus.push(NativeMenu { title: tr(menu_title_key(t)).into(), items });
    }

    // ── LLM ─────────────────────────────────────────────────────────────────────
    // The local-model hub: open Model Setup (download / reveal / delete), reveal
    // the models folder, download the recommended default, and the two per-turn
    // toggles (Local Only / Allow Web Search). The toggles are checkable; the app
    // syncs their live state onto the native items each frame.
    {
        let t = "LLM";
        let items = vec![
            wired_leaf(t, "model_setup", tr("menu.llm.model_setup"), MenuAction::ModelSetup),
            NativeItem::Leaf {
                id: format!("{t}/reveal_models"),
                label: tr("menu.llm.reveal_models").into(),
                shortcut: None,
                enabled: true,
                action: MenuAction::RevealModelsFolder,
            },
            NativeItem::Leaf {
                id: format!("{t}/download_default"),
                label: tr("menu.llm.download_default").into(),
                shortcut: None,
                enabled: true,
                action: MenuAction::DownloadDefaultModel,
            },
            NativeItem::Separator,
            NativeItem::Check {
                id: format!("{t}/local_only"),
                label: tr("menu.llm.local_only").into(),
                action: MenuAction::ToggleLocalOnly,
                checked: false, // synced live by the app
                enabled: true,
            },
            NativeItem::Check {
                id: format!("{t}/web_search"),
                label: tr("menu.llm.web_search").into(),
                action: MenuAction::ToggleWebSearch,
                checked: false, // synced live by the app
                enabled: true,
            },
            NativeItem::Check {
                id: format!("{t}/terse"),
                label: tr("menu.llm.terse").into(),
                action: MenuAction::ToggleTerse,
                checked: false, // synced live by the app
                enabled: true,
            },
        ];
        menus.push(NativeMenu { title: tr(menu_title_key(t)).into(), items });
    }

    // ── Plugins ───────────────────────────────────────────────────────────────
    // A dedicated top-level menu (sibling of File/Edit/View/Theme/LLM). Its one
    // leaf, Plugins…, opens the plugins popup window (installed user/LLM-authored
    // macros as cards, with search + per-card run/JSON/reload/delete) via the
    // existing ShowPlugins action — the same modeless popup Model Setup / About
    // use. Kept out of LLM so it reads as its own root-level surface.
    {
        let t = "Plugins";
        let items = vec![NativeItem::Leaf {
            id: format!("{t}/manage"),
            label: tr("menu.plugins.manage").into(),
            shortcut: None,
            enabled: true,
            action: MenuAction::ShowPlugins,
        }];
        menus.push(NativeMenu { title: tr(menu_title_key(t)).into(), items });
    }

    // ── Window (macOS) ────────────────────────────────────────────────────────
    // Standard OS-handled items: Minimize ⌘M / Zoom / Bring All to Front / Full
    // Screen. No MenuAction — AppKit implements them.
    #[cfg(target_os = "macos")]
    menus.push(NativeMenu {
        title: tr("menu.window").to_string(),
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
        title: tr("menu.help").to_string(),
        items: vec![
            NativeItem::Leaf {
                id: "Help/docs".into(),
                label: tr("menu.help.docs").into(),
                action: MenuAction::Insert("help ".into()),
                shortcut: None,
                enabled: true,
            },
            NativeItem::Leaf {
                id: "Help/reference".into(),
                label: tr("menu.help.reference").into(),
                action: MenuAction::Help,
                shortcut: None,
                enabled: true,
            },
            NativeItem::Separator,
            NativeItem::Leaf {
                id: "Help/palette".into(),
                label: tr("menu.help.palette").into(),
                action: MenuAction::CommandPalette,
                shortcut: action_shortcut(&MenuAction::CommandPalette).map(str::to_string),
                enabled: true,
            },
            NativeItem::Separator,
            NativeItem::Leaf {
                id: "Help/updates".into(),
                label: tr("menu.help.updates").into(),
                action: MenuAction::CheckForUpdates,
                shortcut: None,
                enabled: true,
            },
            NativeItem::Leaf {
                id: "Help/about".into(),
                label: tr("menu.help.about").into(),
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

/// A Lucide [`Icon`] for a menu leaf, chosen by its stable `id` so the in-window
/// bar keeps a scannable icon column regardless of the active UI language (the
/// label is localized; the id is not). Falls back to a neutral mark.
fn leaf_icon(id: &str, _label: &str) -> Icon {
    let suffix = id.rsplit('/').next().unwrap_or("");
    match suffix {
        "new" => return Icon::New,
        "new_session" => return Icon::NewSession,
        "open" => return Icon::Open,
        "save" | "saveas" => return Icon::Save,
        "import" => return Icon::Import,
        "export" => return Icon::Export,
        "settings" => return Icon::Model,
        "manage" => return Icon::ToolsCat, // Plugins… — lucide "wrench"
        "close" | "quit" => return Icon::Close,
        "undo" => return Icon::Undo,
        "redo" => return Icon::Redo,
        "history" => return Icon::History,
        "reference" | "docs" => return Icon::Help,
        "updates" => return Icon::About,
        "about" => return Icon::About,
        _ => {}
    }
    verb_icon(suffix)
}

/// Draw the minimal menu bar in-window (fallback when no native OS bar). Renders
/// the SAME [`native_model`] the native bar uses, so the two never drift. Returns
/// the action the user picked this frame, if any.
pub fn ui(
    ui: &mut egui::Ui,
    icons: &Icons,
    style: MenuStyle,
    has_selection: bool,
    toggles: MenuToggles,
    view: ViewState,
) -> Option<MenuAction> {
    let mut action = None;
    let model = native_model(style, has_selection, view);
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
                                PredefinedKind::Minimize => tr("menu.window.minimize"),
                                PredefinedKind::Zoom => tr("menu.window.zoom"),
                                PredefinedKind::BringAllToFront => tr("menu.window.bring_all_to_front"),
                                PredefinedKind::Fullscreen => tr("menu.window.fullscreen"),
                                PredefinedKind::Quit => tr("menu.file.quit"),
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
                        NativeItem::Check { label, action: a, checked: model_checked, enabled, .. } => {
                            // LLM toggles read their live state from `toggles`; all
                            // other checks (View display/lighting radios) render the
                            // model's `checked`, which the caller already resolved
                            // from the live ViewState.
                            let (checked, live_enabled) = match a {
                                MenuAction::ToggleLocalOnly
                                | MenuAction::ToggleWebSearch
                                | MenuAction::ToggleTerse => {
                                    toggles.for_action(a, *enabled)
                                }
                                _ => (*model_checked, *enabled),
                            };
                            let mark = if checked { "☑ " } else { "☐ " };
                            if ui
                                .add_enabled(
                                    live_enabled,
                                    egui::Button::new(format!("{mark}{label}")),
                                )
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

/// Live state for the checkable LLM-menu toggles, passed into [`ui`] so the
/// in-window bar shows the real checkmarks (the native bar syncs via handles).
#[derive(Clone, Copy, Debug, Default)]
pub struct MenuToggles {
    pub local_only: bool,
    pub web_search: bool,
    pub terse: bool,
}

impl MenuToggles {
    /// Resolve `(checked, enabled)` for a toggle action. Web-search is forced
    /// off-looking and disabled while local-only is on.
    fn for_action(&self, action: &MenuAction, model_enabled: bool) -> (bool, bool) {
        match action {
            MenuAction::ToggleLocalOnly => (self.local_only, model_enabled),
            MenuAction::ToggleWebSearch => (self.web_search && !self.local_only, !self.local_only),
            MenuAction::ToggleTerse => (self.terse, model_enabled),
            _ => (false, model_enabled),
        }
    }
}


/// Right-aligned Appearance controls: dark/light toggle + text-size stepper,
/// applied app-wide. Factored out because these are egui-only widgets that
/// CANNOT be native menu items — when the true native OS menu bar (muda) is
/// attached, we still render this slim in-window strip for them (see
/// [`appearance_only`]).
fn appearance_controls(ui: &mut egui::Ui, icons: &Icons) {
    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
        let zoom = ui.ctx().zoom_factor();
        // Small TEXT, full-size HIT AREA: `small_button` renders ~19px tall,
        // under the hit-target floor (flagged by the M-guitest hit-target
        // audit) — keep the compact look but give the button the floor size.
        let hit = crate::theme::Spacing::HIT_TARGET;
        let stepper = |t: &str| egui::Button::new(egui::RichText::new(t).small());
        if ui
            .add_sized([hit, hit], stepper("A+"))
            .on_hover_text("bigger text (Cmd =)")
            .clicked()
        {
            ui.ctx().set_zoom_factor((zoom + 0.1).min(3.0));
        }
        ui.label(format!("{:.0}%", zoom * 100.0));
        if ui
            .add_sized([hit, hit], stepper("A−"))
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
    // with their shortcut hints) is visible in the shot. A default ViewState
    // (panel shown, Shaded/Working active) drives the stateful View items.
    let view = ViewState {
        display: Some(DisplayModeTag::Shaded),
        lighting: Some(LightModeTag::Working),
        camera: Some(CameraTag::Perspective),
        panel_visible: true,
        skin: CadOrigin::None,
        lang: Lang::En,
    };
    let Some(menu) = native_model(style, false, view).into_iter().find(|m| m.title == title) else {
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
                        NativeItem::Check { label, checked, enabled, .. } => {
                            let mark = if *checked { "☑ " } else { "☐ " };
                            let _ = ui.add_enabled(
                                *enabled,
                                egui::Button::new(format!("{mark}{label}")),
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
                NativeItem::Leaf { id, label, action, .. }
                | NativeItem::Check { id, label, action, .. } => {
                    Some((id.clone(), label.clone(), action.clone()))
                }
                NativeItem::Separator | NativeItem::Predefined(_) => None,
            })
            .collect()
    }

    // MENU BAR IS MINIMAL: exactly File / Edit / View / Theme / LLM (+ Window on
    // macOS) + Help. No geometry category menus.

    #[test]
    fn menu_bar_top_titles() {
        // The geometry-free top titles are the documented contract. Displayed
        // titles are localized; assert against the active-locale catalog values
        // (via `tr`) so the test is language-independent.
        assert_eq!(TOP_TITLES, ["File", "Edit", "View", "Theme", "LLM", "Plugins"]);
        for style in [MenuStyle::Rhino, MenuStyle::AutoCAD] {
            let titles: Vec<String> =
                native_model(style, true, ViewState::default()).iter().map(|m| m.title.clone()).collect();
            let mut expected = vec![
                tr("menu.file").to_string(),
                tr("menu.edit").into(),
                tr("menu.view").into(),
                tr("menu.theme").into(),
                tr("menu.llm").into(),
                tr("menu.plugins").into(),
            ];
            #[cfg(target_os = "macos")]
            expected.push(tr("menu.window").to_string());
            expected.push(tr("menu.help").to_string());
            assert_eq!(titles, expected, "menu titles differ for {style:?}");
        }
    }

    #[test]
    fn no_geometry_category_menus_present() {
        // The removed category menus (Draw/Curve/Solid/Transform/Modify/Dimension/
        // Analyze/Structure/Tools/Format) must not appear as top-level menus.
        // NB: "Plugins" is now a LEGITIMATE top-level menu (root-level plugins
        // popup) — it is intentionally absent from this banned list.
        let banned = [
            "Draw", "Curve", "Solid", "Transform", "Modify", "Dimension", "Annotate",
            "Analyze", "Structure", "Tools", "Format", "Boolean",
        ];
        for style in [MenuStyle::Rhino, MenuStyle::AutoCAD] {
            for m in native_model(style, true, ViewState::default()) {
                assert!(!banned.contains(&m.title.as_str()), "banned menu {} present", m.title);
            }
        }
    }

    #[test]
    fn menu_bar_identical_across_presets() {
        // The minimal bar ignores the preset style.
        assert_eq!(native_model(MenuStyle::Rhino, true, ViewState::default()), native_model(MenuStyle::AutoCAD, true, ViewState::default()));
    }

    #[test]
    fn file_menu_has_curated_items() {
        let file = native_model(MenuStyle::Rhino, true, ViewState::default())
            .into_iter()
            .find(|m| m.title == tr("menu.file"))
            .unwrap();
        let ls = leaves(&[file]);
        let has = |label: &str| ls.iter().any(|(_, l, _)| l == label);
        assert!(has(tr("menu.file.new")));
        assert!(has(tr("menu.file.open")));
        assert!(has(tr("menu.file.save")));
        assert!(has(tr("menu.file.save_as")));
        assert!(has(tr("menu.file.import")));
        assert!(has(tr("menu.file.export")));
        assert!(has(tr("menu.file.settings")));
        // Import/Export route to native dialogs.
        assert!(ls.iter().any(|(_, l, a)| l == tr("menu.file.import") && *a == MenuAction::ImportDialog));
        assert!(ls.iter().any(|(_, l, a)| l == tr("menu.file.export") && *a == MenuAction::ExportDialog));
        // Save As prefills `save ` for a path.
        assert!(ls.iter().any(|(_, l, a)| l == tr("menu.file.save_as") && *a == MenuAction::Insert("save ".into())));
    }

    #[test]
    fn help_menu_has_check_for_updates() {
        // M-autoupdate: the Help menu must carry a "Check for Updates…" leaf that
        // dispatches MenuAction::CheckForUpdates (routed by apply_menu_action to
        // open the update popup + start the check).
        let help = native_model(MenuStyle::Rhino, true, ViewState::default())
            .into_iter()
            .find(|m| m.title == tr("menu.help"))
            .expect("Help menu present");
        let ls = leaves(&[help]);
        assert!(
            ls.iter()
                .any(|(id, l, a)| id == "Help/updates"
                    && l == tr("menu.help.updates")
                    && *a == MenuAction::CheckForUpdates),
            "Help ▸ Check for Updates… leaf missing or misrouted"
        );
    }

    #[test]
    fn file_menu_has_reachable_quit_and_close() {
        // Regression: macOS previously had NO menu-bar Quit (it was cfg'd out,
        // and the native app menu did not surface one). Quit MUST be in the File
        // menu on every platform, with ⌘Q; Close MUST be present with ⌘W.
        let file = native_model(MenuStyle::Rhino, true, ViewState::default())
            .into_iter()
            .find(|m| m.title == tr("menu.file"))
            .unwrap();
        let quit = file.items.iter().find_map(|it| match it {
            NativeItem::Leaf { label, action, shortcut, .. }
                if *action == MenuAction::Quit =>
            {
                Some((label.clone(), shortcut.clone()))
            }
            _ => None,
        });
        assert_eq!(
            quit,
            Some((tr("menu.file.quit").to_string(), Some("Cmd+Q".to_string()))),
            "File ▸ Quit must be present with ⌘Q on every platform"
        );
        let close = file.items.iter().find_map(|it| match it {
            NativeItem::Leaf { label, action, shortcut, .. }
                if *action == MenuAction::Close =>
            {
                Some((label.clone(), shortcut.clone()))
            }
            _ => None,
        });
        assert_eq!(
            close,
            Some((tr("menu.file.close").to_string(), Some("Cmd+W".to_string()))),
            "File ▸ Close must be present with ⌘W"
        );
    }

    #[test]
    fn quit_close_shortcuts_present() {
        assert_eq!(action_shortcut(&MenuAction::Quit), Some("Cmd+Q"));
        assert_eq!(action_shortcut(&MenuAction::Close), Some("Cmd+W"));
    }

    #[test]
    fn edit_menu_has_curated_items() {
        let edit = native_model(MenuStyle::Rhino, true, ViewState::default())
            .into_iter()
            .find(|m| m.title == tr("menu.edit"))
            .unwrap();
        let ls = leaves(&[edit]);
        for key in [
            "menu.edit.undo", "menu.edit.redo", "menu.edit.cut", "menu.edit.copy",
            "menu.edit.paste", "menu.edit.delete", "menu.edit.select_all",
            "menu.edit.deselect", "menu.edit.history",
        ] {
            let label = tr(key);
            assert!(ls.iter().any(|(_, l, _)| l == label), "Edit missing {label}");
        }
        assert!(ls.iter().any(|(_, l, a)| l == tr("menu.edit.undo") && *a == MenuAction::Execute("undo".into())));
        assert!(ls.iter().any(|(_, l, a)| l == tr("menu.edit.copy") && *a == MenuAction::Execute("copyselection".into())));
        assert!(ls.iter().any(|(_, l, a)| l == tr("menu.edit.paste") && *a == MenuAction::Execute("pasteselection".into())));
        assert!(ls.iter().any(|(_, l, a)| l == tr("menu.edit.delete") && *a == MenuAction::Execute("delete sel".into())));
        assert!(ls.iter().any(|(_, l, a)| l == tr("menu.edit.select_all") && *a == MenuAction::Execute("select all".into())));
    }

    #[test]
    fn view_menu_has_display_lighting_viewports_views_and_palette() {
        let view = native_model(MenuStyle::Rhino, true, ViewState::default())
            .into_iter()
            .find(|m| m.title == tr("menu.view"))
            .unwrap();
        let ls = leaves(&[view]);
        let by_action = |a: &MenuAction| ls.iter().any(|(_, _, act)| act == a);
        assert!(by_action(&MenuAction::Execute("display shaded".into())), "display mode missing");
        assert!(by_action(&MenuAction::Execute("lightmode sun".into())), "lighting missing");
        assert!(by_action(&MenuAction::Execute("viewports 4".into())), "viewport layout missing");
        assert!(by_action(&MenuAction::Execute("top".into())), "standard view missing");
        assert!(by_action(&MenuAction::Execute("ze".into())), "zoom extents missing");
        assert!(by_action(&MenuAction::CommandPalette), "command palette entry missing");
        // Appearance + Text Size are NOT in View anymore — they live in Theme.
        assert!(!by_action(&MenuAction::SetTheme(Some(true))), "theme leaked into View");
        assert!(!by_action(&MenuAction::ZoomStep(true)), "text size leaked into View");
        // Object Snap… opens the osnap popup (same as the status-bar chip).
        assert!(by_action(&MenuAction::ShowOsnap), "Object Snap… entry missing from View");
    }

    #[test]
    fn llm_menu_has_setup_reveal_download_and_toggles() {
        let llm = native_model(MenuStyle::Rhino, true, ViewState::default())
            .into_iter()
            .find(|m| m.title == tr("menu.llm"))
            .expect("LLM menu present at top level");
        // Leaves: Model Setup, Reveal Models Folder, Download Default Model.
        let ls = leaves(&[llm.clone()]);
        let by_action = |a: &MenuAction| ls.iter().any(|(_, _, act)| act == a);
        assert!(by_action(&MenuAction::ModelSetup), "Model Setup missing");
        assert!(by_action(&MenuAction::RevealModelsFolder), "Reveal Models Folder missing");
        assert!(by_action(&MenuAction::DownloadDefaultModel), "Download Default missing");
        // Plugins… MOVED to its own root-level Plugins menu — it must no longer
        // appear under LLM (see `plugins_menu_is_top_level_and_routes_show`).
        assert!(!by_action(&MenuAction::ShowPlugins), "Plugins… must not be in the LLM menu");
        assert!(
            !ls.iter().any(|(id, _, _)| id == "LLM/plugins"),
            "stale LLM/plugins leaf must be gone"
        );
        // Two checkable toggles with stable ids the app syncs against.
        let checks: Vec<&str> = llm
            .items
            .iter()
            .filter_map(|it| match it {
                NativeItem::Check { id, .. } => Some(id.as_str()),
                _ => None,
            })
            .collect();
        assert!(checks.contains(&"LLM/local_only"), "Local Only toggle missing");
        assert!(checks.contains(&"LLM/web_search"), "Allow Web Search toggle missing");
        assert!(checks.contains(&"LLM/terse"), "Terse Replies toggle missing");
    }

    #[test]
    fn plugins_menu_is_top_level_and_routes_show() {
        // Owner intent: Plugins is its OWN top-level menu (sibling of LLM), with a
        // single leaf that opens the plugins popup via the existing ShowPlugins.
        for style in [MenuStyle::Rhino, MenuStyle::AutoCAD] {
            let plugins = native_model(style, true, ViewState::default())
                .into_iter()
                .find(|m| m.title == tr("menu.plugins"))
                .expect("Plugins menu present at top level");
            let ls = leaves(&[plugins]);
            assert!(
                ls.iter().any(|(id, l, a)| id == "Plugins/manage"
                    && l == tr("menu.plugins.manage")
                    && *a == MenuAction::ShowPlugins),
                "Plugins ▸ Plugins… must have id Plugins/manage routing ShowPlugins ({style:?})"
            );
        }
    }

    #[test]
    fn terse_toggle_state_resolution() {
        // The Terse Replies checkmark mirrors the live state and stays enabled
        // regardless of local-only (terse applies to local AND cloud cassettes).
        let t = MenuToggles { local_only: true, web_search: true, terse: true };
        assert_eq!(t.for_action(&MenuAction::ToggleTerse, true), (true, true));
        let t = MenuToggles { local_only: false, web_search: false, terse: false };
        assert_eq!(t.for_action(&MenuAction::ToggleTerse, true), (false, true));
    }

    #[test]
    fn theme_menu_has_appearance_and_text_size() {
        let theme = native_model(MenuStyle::Rhino, true, ViewState::default())
            .into_iter()
            .find(|m| m.title == tr("menu.theme"))
            .expect("Theme menu present at top level");
        let ls = leaves(&[theme]);
        let by_action = |a: &MenuAction| ls.iter().any(|(_, _, act)| act == a);
        // All three appearance choices.
        assert!(by_action(&MenuAction::SetTheme(Some(false))), "Light missing");
        assert!(by_action(&MenuAction::SetTheme(Some(true))), "Dark missing");
        assert!(by_action(&MenuAction::SetTheme(None)), "System missing");
        // All three text-size steps.
        assert!(by_action(&MenuAction::ZoomStep(true)), "Increase missing");
        assert!(by_action(&MenuAction::ZoomStep(false)), "Decrease missing");
        assert!(by_action(&MenuAction::ZoomReset), "Reset missing");
    }

    /// Helper: collect the (id, checked, action) of Theme-menu Check items.
    fn theme_checks(view: ViewState) -> Vec<(String, bool, MenuAction)> {
        native_model(MenuStyle::Rhino, true, view)
            .into_iter()
            .find(|m| m.title == tr("menu.theme"))
            .expect("Theme menu present")
            .items
            .iter()
            .filter_map(|it| match it {
                NativeItem::Check { id, checked, action, .. } => {
                    Some((id.clone(), *checked, action.clone()))
                }
                _ => None,
            })
            .collect()
    }

    #[test]
    fn theme_menu_has_skin_group_with_all_four_options() {
        let checks = theme_checks(ViewState::default());
        for (id, origin) in skin_radio_items() {
            let full = format!("Theme/{id}");
            let found = checks
                .iter()
                .find(|(i, _, _)| i == &full)
                .unwrap_or_else(|| panic!("skin item {full} missing"));
            assert_eq!(found.2, MenuAction::SetSkin(origin), "{full} wrong action");
        }
    }

    #[test]
    fn theme_menu_skin_radio_checks_active_skin() {
        // Exactly the active skin carries the check.
        for (_, active) in skin_radio_items() {
            let view = ViewState { skin: active, ..Default::default() };
            for (id, origin) in skin_radio_items() {
                let full = format!("Theme/{id}");
                let checked = theme_checks(view)
                    .into_iter()
                    .find(|(i, _, _)| i == &full)
                    .map(|(_, c, _)| c)
                    .unwrap();
                assert_eq!(checked, origin == active, "{full} check wrong for active {active:?}");
            }
        }
    }

    #[test]
    fn theme_menu_has_language_group_with_correct_check() {
        for (_, active) in lang_radio_items() {
            let view = ViewState { lang: active, ..Default::default() };
            let checks = theme_checks(view);
            for (id, lang) in lang_radio_items() {
                let full = format!("Theme/{id}");
                let (_, checked, action) = checks
                    .iter()
                    .find(|(i, _, _)| i == &full)
                    .unwrap_or_else(|| panic!("lang item {full} missing"))
                    .clone();
                assert_eq!(action, MenuAction::SetLanguage(lang), "{full} wrong action");
                assert_eq!(checked, lang == active, "{full} check wrong for active {active:?}");
            }
        }
    }

    #[test]
    fn skin_and_language_items_route_to_apply_actions() {
        // Selecting a Theme skin/language item dispatches the apply+persist action
        // (SetSkin / SetLanguage), the same the app routes through apply_menu_action.
        let checks = theme_checks(ViewState::default());
        assert!(checks.iter().any(|(_, _, a)| *a == MenuAction::SetSkin(CadOrigin::Rhino)));
        assert!(checks.iter().any(|(_, _, a)| *a == MenuAction::SetLanguage(Lang::Es)));
    }

    #[test]
    fn help_menu_has_docs_reference_palette_about() {
        let help = native_model(MenuStyle::AutoCAD, true, ViewState::default())
            .into_iter()
            .find(|m| m.title == tr("menu.help"))
            .unwrap();
        let ls = leaves(&[help]);
        assert!(ls.iter().any(|(_, l, _)| l == tr("menu.help.docs")));
        assert!(ls.iter().any(|(_, l, a)| l == tr("menu.help.reference") && *a == MenuAction::Help));
        assert!(ls.iter().any(|(_, _, a)| *a == MenuAction::CommandPalette));
        assert!(ls.iter().any(|(_, l, a)| l == tr("menu.help.about") && *a == MenuAction::About));
    }

    #[test]
    fn command_palette_bound_to_cmd_k() {
        assert_eq!(action_shortcut(&MenuAction::CommandPalette), Some("Cmd+K"));
    }

    #[test]
    fn native_leaf_ids_are_unique() {
        for style in [MenuStyle::Rhino, MenuStyle::AutoCAD] {
            let ls = leaves(&native_model(style, true, ViewState::default()));
            let ids: HashSet<&String> = ls.iter().map(|(id, _, _)| id).collect();
            assert_eq!(ids.len(), ls.len(), "duplicate native menu id for {style:?}");
        }
    }

    #[test]
    fn no_show_tab_bar_item_anywhere() {
        for style in [MenuStyle::Rhino, MenuStyle::AutoCAD] {
            for (_, label, _) in leaves(&native_model(style, true, ViewState::default())) {
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
        let empty = native_model(MenuStyle::Rhino, false, ViewState::default());
        let filled = native_model(MenuStyle::Rhino, true, ViewState::default());
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

    // ── Stateful View menu (radios + Panel flip) ─────────────────────────────

    /// Helper: the `checked` flag of a View check leaf by id.
    fn view_check(view: ViewState, id: &str) -> Option<bool> {
        native_model(MenuStyle::Rhino, true, view)
            .into_iter()
            .find(|m| m.title == tr("menu.view"))?
            .items
            .iter()
            .find_map(|it| match it {
                NativeItem::Check { id: i, checked, .. } if i == id => Some(*checked),
                _ => None,
            })
    }

    #[test]
    fn view_display_mode_is_radio_checked() {
        let v = ViewState { display: Some(DisplayModeTag::Wireframe), ..Default::default() };
        // Exactly the active display mode carries the check.
        assert_eq!(view_check(v, "View/disp_wire"), Some(true));
        assert_eq!(view_check(v, "View/disp_shaded"), Some(false));
        assert_eq!(view_check(v, "View/disp_xray"), Some(false));
        assert_eq!(view_check(v, "View/disp_pencil"), Some(false));
    }

    #[test]
    fn view_lighting_mode_is_radio_checked() {
        let v = ViewState { lighting: Some(LightModeTag::Sun), ..Default::default() };
        assert_eq!(view_check(v, "View/light_sun"), Some(true));
        assert_eq!(view_check(v, "View/light_working"), Some(false));
        assert_eq!(view_check(v, "View/light_present"), Some(false));
    }

    #[test]
    fn view_camera_projection_is_radio_checked() {
        let v = ViewState { camera: Some(CameraTag::TwoPoint), ..Default::default() };
        assert_eq!(view_check(v, "View/cam_2point"), Some(true));
        assert_eq!(view_check(v, "View/cam_persp"), Some(false));
        assert_eq!(view_check(v, "View/cam_pano"), Some(false));
        assert_eq!(view_check(v, "View/cam_fisheye"), Some(false));
    }

    #[test]
    fn view_camera_none_checks_no_projection() {
        // Ortho standard views: no camera radio carries a check.
        let v = ViewState { camera: None, ..Default::default() };
        for id in ["View/cam_persp", "View/cam_2point", "View/cam_pano", "View/cam_fisheye"] {
            assert_eq!(view_check(v, id), Some(false), "{id} must be unchecked");
        }
    }

    #[test]
    fn view_camera_items_fire_camera_verbs() {
        // Each camera radio dispatches the same `camera <mode>` verb the
        // command line / deck use, so the menu stays a thin stateful skin.
        let view = native_model(MenuStyle::Rhino, true, ViewState::default())
            .into_iter()
            .find(|m| m.title == tr("menu.view"))
            .unwrap();
        let action_of = |id: &str| -> Option<MenuAction> {
            view.items.iter().find_map(|it| match it {
                NativeItem::Check { id: i, action, .. } if i == id => Some(action.clone()),
                _ => None,
            })
        };
        assert_eq!(action_of("View/cam_persp"), Some(MenuAction::Execute("camera persp".into())));
        assert_eq!(action_of("View/cam_2point"), Some(MenuAction::Execute("camera 2point".into())));
        assert_eq!(action_of("View/cam_pano"), Some(MenuAction::Execute("camera pano".into())));
        assert_eq!(
            action_of("View/cam_fisheye"),
            Some(MenuAction::Execute("camera fisheye".into()))
        );
    }

    #[test]
    fn view_panel_item_flips_label_and_carries_shortcut() {
        let panel_of = |view: ViewState| -> (String, Option<String>) {
            native_model(MenuStyle::Rhino, true, view)
                .into_iter()
                .find(|m| m.title == tr("menu.view"))
                .unwrap()
                .items
                .iter()
                .find_map(|it| match it {
                    NativeItem::Leaf { id, label, shortcut, action, .. }
                        if id == "View/panel" =>
                    {
                        assert_eq!(*action, MenuAction::TogglePanel);
                        Some((label.clone(), shortcut.clone()))
                    }
                    _ => None,
                })
                .expect("View/panel present")
        };
        let (shown, sc) = panel_of(ViewState { panel_visible: true, ..Default::default() });
        assert_eq!(shown, tr("menu.view.hide_panel"));
        assert_eq!(sc, Some("Cmd+\\".to_string()));
        let (hidden, _) = panel_of(ViewState { panel_visible: false, ..Default::default() });
        assert_eq!(hidden, tr("menu.view.show_panel"));
    }

    #[test]
    fn toggle_panel_shortcut_present() {
        // The native (muda) layer's `every_native_shortcut_parses` asserts the
        // Panel leaf's "Cmd+\\" parses as an accelerator; here we just pin the
        // string so the View flip and the ⌘\ hotkey stay in sync.
        assert_eq!(action_shortcut(&MenuAction::TogglePanel), Some("Cmd+\\"));
    }

    // ── Window menu (macOS) ──────────────────────────────────────────────────

    #[cfg(target_os = "macos")]
    #[test]
    fn window_menu_present_with_standard_items() {
        let win = native_model(MenuStyle::Rhino, false, ViewState::default())
            .into_iter()
            .find(|m| m.title == tr("menu.window"))
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
        let file = native_model(MenuStyle::Rhino, true, ViewState::default())
            .into_iter()
            .find(|m| m.title == tr("menu.file"))
            .unwrap();
        let save = file.items.iter().find_map(|it| match it {
            NativeItem::Leaf { label, shortcut, .. } if *label == tr("menu.file.save") => Some(shortcut.clone()),
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
