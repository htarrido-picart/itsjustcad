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
}

/// Draw-tool verbs (mirror `draw_tool::try_start`). A menu pick of one of these
/// starts the interactive picker rather than typing text.
const DRAW_VERBS: [&str; 4] = ["line", "polyline", "rect", "circle"];

/// One user-plugin verb surfaced in the "Plugins" menu. Built from the session
/// [`itsjustcad_commands::plugin::Plugin`] registry (name, its declared menu
/// group, whether it takes params, and a one-line summary for the tooltip).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginMenuEntry {
    pub name: String,
    pub category: String,
    /// True when the plugin declares parameters — picking it prefills the
    /// command line (so the user can supply args) instead of executing at once.
    pub has_params: bool,
    pub summary: String,
}

/// The [`MenuAction`] a plugin menu pick dispatches: a parameterless plugin runs
/// immediately; a parameterised one is prefilled for the user to complete. Both
/// still lower to substrate commands through the ordinary command-line path.
pub fn plugin_action(entry: &PluginMenuEntry) -> MenuAction {
    if entry.has_params {
        MenuAction::Insert(format!("{} ", entry.name))
    } else {
        MenuAction::Execute(entry.name.clone())
    }
}

/// Group plugin entries by their menu category, preserving first-seen order of
/// groups and entry order within each. Feeds both the in-window and (test-only)
/// menu models so plugins appear grouped under "Plugins ▸ <category>".
pub fn plugin_groups(entries: &[PluginMenuEntry]) -> Vec<(String, Vec<PluginMenuEntry>)> {
    let mut groups: Vec<(String, Vec<PluginMenuEntry>)> = Vec::new();
    for e in entries {
        if let Some(g) = groups.iter_mut().find(|(k, _)| *k == e.category) {
            g.1.push(e.clone());
        } else {
            groups.push((e.category.clone(), vec![e.clone()]));
        }
    }
    groups
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

/// Render one menu row as `<icon>  <label>` with a Lucide icon tinted to the
/// current foreground, so every menu item carries a leading mark in a consistent
/// column. Returns the row's click [`Response`].
fn item(ui: &mut egui::Ui, icons: &Icons, icon: Icon, label: &str) -> egui::Response {
    icons.menu_item(ui, icon, label)
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

/// Top-level menu titles and the categories each gathers, for a given preset
/// style. Every [`Category`] appears in exactly one menu (checked by
/// `categories_partition_all` in tests). File/Edit/Help are special — File and
/// Edit still gather their registry categories but the bar also wires the
/// app-level save/open/undo/redo/export/import/print actions; Help is synthetic.
pub fn top_menus(style: MenuStyle) -> Vec<(&'static str, Vec<Category>)> {
    match style {
        // Rhino: File / Edit / View / Curve / Solid / Transform / Dimension /
        // Analyze / Tools / Help.
        MenuStyle::Rhino => vec![
            ("File", vec![Category::File]),
            ("Edit", vec![Category::Edit]),
            ("View", vec![Category::View]),
            ("Curve", vec![Category::Draw2d, Category::Curve]),
            ("Solid", vec![Category::Solid, Category::Boolean]),
            ("Transform", vec![Category::Transform]),
            ("Dimension", vec![Category::Dimension]),
            ("Analyze", vec![Category::Analyze]),
            ("Structure", vec![Category::Structure]),
            ("Tools", vec![Category::Annotate, Category::Tools]),
        ],
        // AutoCAD: File / Edit / View / Draw / Modify / Dimension / Format /
        // Tools / Help.
        MenuStyle::AutoCAD => vec![
            ("File", vec![Category::File]),
            ("Edit", vec![Category::Edit]),
            ("View", vec![Category::View]),
            (
                "Draw",
                vec![Category::Draw2d, Category::Curve, Category::Solid],
            ),
            ("Modify", vec![Category::Transform, Category::Boolean]),
            ("Dimension", vec![Category::Dimension]),
            ("Format", vec![Category::Annotate]),
            ("Structure", vec![Category::Structure]),
            // AutoCAD groups inquiry/analysis tools under Tools.
            ("Tools", vec![Category::Tools, Category::Analyze]),
        ],
    }
}

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
    },
    /// A visual divider between item groups.
    Separator,
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
pub fn native_model(style: MenuStyle) -> Vec<NativeMenu> {
    let mut menus: Vec<NativeMenu> = Vec::new();
    for (title, cats) in top_menus(style) {
        let mut items: Vec<NativeItem> = Vec::new();
        let leaf = |id: &str, label: &str, action: MenuAction| NativeItem::Leaf {
            id: format!("{title}/{id}"),
            label: label.to_string(),
            action,
        };
        // File / Edit / Tools prepend their app-wired actions, mirroring `ui`.
        if title == "File" {
            items.push(leaf("new", "New", MenuAction::NewDocument));
            items.push(leaf("new_session", "New file session", MenuAction::NewSession));
            items.push(NativeItem::Separator);
            for (label, verb) in [
                ("Open…", "open"),
                ("Save…", "save"),
                ("Import…", "import"),
                ("Export…", "export"),
                ("Print…", "print"),
            ] {
                items.push(leaf(verb, label, menu_action(verb)));
            }
            items.push(NativeItem::Separator);
        } else if title == "Edit" {
            items.push(leaf("undo", "Undo", MenuAction::Execute("undo".into())));
            items.push(leaf("redo", "Redo", MenuAction::Execute("redo".into())));
            items.push(leaf("history", "Edit history…", MenuAction::EditHistory));
            items.push(NativeItem::Separator);
        } else if title == "Tools" {
            items.push(leaf("model_setup", "Model Setup…", MenuAction::ModelSetup));
            items.push(NativeItem::Separator);
        }
        // Registry verbs grouped by category, a separator between groups — the
        // File/Edit wired verbs already surfaced above are skipped.
        let mut first_group = true;
        for cat in &cats {
            let verbs: Vec<&str> = verbs_in(std::slice::from_ref(cat))
                .into_iter()
                .filter(|verb| {
                    !(title == "File"
                        && matches!(*verb, "open" | "save" | "import" | "export" | "print"))
                        && !(title == "Edit" && matches!(*verb, "undo" | "redo"))
                })
                .collect();
            if verbs.is_empty() {
                continue;
            }
            if !first_group {
                items.push(NativeItem::Separator);
            }
            first_group = false;
            for verb in verbs {
                items.push(leaf(verb, verb, menu_action(verb)));
            }
        }
        menus.push(NativeMenu {
            title: title.to_string(),
            items,
        });
    }
    // Help is synthetic (not a registry category), same as the in-window bar.
    menus.push(NativeMenu {
        title: "Help".to_string(),
        items: vec![
            NativeItem::Leaf {
                id: "Help/reference".into(),
                label: "Command reference".into(),
                action: MenuAction::Help,
            },
            NativeItem::Leaf {
                id: "Help/about".into(),
                label: "About ItsJustCAD".into(),
                action: MenuAction::About,
            },
        ],
    });
    menus
}

/// Registry verbs belonging to any of `cats`, in registry order.
pub fn verbs_in(cats: &[Category]) -> Vec<&'static str> {
    registry()
        .iter()
        .filter(|s| cats.contains(&s.category))
        .map(|s| s.name)
        .collect()
}

/// Draw the menu bar. Returns the action the user picked this frame, if any.
/// File/Edit menus prepend the app-wired actions (save/open/…); Help is added
/// as the last menu.
pub fn ui(
    ui: &mut egui::Ui,
    icons: &Icons,
    style: MenuStyle,
    plugins: &[PluginMenuEntry],
) -> Option<MenuAction> {
    let mut action = None;
    egui::MenuBar::new().ui(ui, |ui| {
        for (title, cats) in top_menus(style) {
            ui.menu_button(title, |ui| {
                // File / Edit get their app-wired actions first. Leading Lucide
                // icons give the menu a clean, scannable column.
                if title == "File" {
                    // New group.
                    if item(ui, icons, Icon::New, "New").clicked() {
                        action = Some(MenuAction::NewDocument);
                        ui.close();
                    }
                    if item(ui, icons, Icon::NewSession, "New file session").clicked() {
                        action = Some(MenuAction::NewSession);
                        ui.close();
                    }
                    ui.separator();
                    for (icon, label, verb) in [
                        (Icon::Open, "Open…", "open"),
                        (Icon::Save, "Save…", "save"),
                        (Icon::Import, "Import…", "import"),
                        (Icon::Export, "Export…", "export"),
                        (Icon::Print, "Print…", "print"),
                    ] {
                        if item(ui, icons, icon, label).clicked() {
                            action = Some(menu_action(verb));
                            ui.close();
                        }
                    }
                    ui.separator();
                } else if title == "Edit" {
                    for (icon, label, verb) in
                        [(Icon::Undo, "Undo", "undo"), (Icon::Redo, "Redo", "redo")]
                    {
                        if item(ui, icons, icon, label).clicked() {
                            action = Some(MenuAction::Execute(verb.to_string()));
                            ui.close();
                        }
                    }
                    if item(ui, icons, Icon::History, "Edit history…").clicked() {
                        action = Some(MenuAction::EditHistory);
                        ui.close();
                    }
                    ui.separator();
                } else if title == "Tools" {
                    // App-wired: opens the Model Setup panel (download/manage a
                    // local model) at any time — this is the "download a local
                    // model" entry point users can reach from the menu bar.
                    if item(ui, icons, Icon::Model, "Model Setup…").clicked() {
                        action = Some(MenuAction::ModelSetup);
                        ui.close();
                    }
                    if item(ui, icons, Icon::Model, "Download local model…").clicked() {
                        action = Some(MenuAction::ModelSetup);
                        ui.close();
                    }
                    ui.separator();
                }
                // List verbs grouped BY CATEGORY, a separator between groups, so
                // a multi-category menu (e.g. Curve = Draw2d + Curve) reads as
                // distinct AutoCAD-style sub-groups rather than one flat list.
                let mut first_group = true;
                for cat in &cats {
                    let verbs: Vec<&str> = verbs_in(std::slice::from_ref(cat))
                        .into_iter()
                        .filter(|verb| {
                            // File/Edit already surfaced their wired verbs above.
                            !(title == "File"
                                && matches!(*verb, "open" | "save" | "import" | "export" | "print"))
                                && !(title == "Edit" && matches!(*verb, "undo" | "redo"))
                        })
                        .collect();
                    if verbs.is_empty() {
                        continue;
                    }
                    if !first_group {
                        ui.separator();
                    }
                    first_group = false;
                    for verb in verbs {
                        if item(ui, icons, verb_icon(verb), verb).clicked() {
                            action = Some(menu_action(verb));
                            ui.close();
                        }
                    }
                }
            });
        }
        // User plugins get their own top-level menu, grouped by declared
        // category. Only shown when at least one plugin is loaded so the bar
        // stays clean on a fresh install.
        if !plugins.is_empty() {
            ui.menu_button("Plugins", |ui| {
                let mut first_group = true;
                for (group, entries) in plugin_groups(plugins) {
                    if !first_group {
                        ui.separator();
                    }
                    first_group = false;
                    // A non-default group name gets a faint header row.
                    if group != "Plugins" {
                        ui.label(egui::RichText::new(group).weak().small());
                    }
                    for entry in entries {
                        let resp = item(ui, icons, Icon::ToolsCat, &entry.name);
                        let resp = if entry.summary.is_empty() {
                            resp
                        } else {
                            resp.on_hover_text(&entry.summary)
                        };
                        if resp.clicked() {
                            action = Some(plugin_action(&entry));
                            ui.close();
                        }
                    }
                }
            });
        }

        // Help is synthetic (not a registry category).
        ui.menu_button("Help", |ui| {
            if item(ui, icons, Icon::Help, "Command reference").clicked() {
                action = Some(MenuAction::Help);
                ui.close();
            }
            if item(ui, icons, Icon::About, "About ItsJustCAD").clicked() {
                action = Some(MenuAction::About);
                ui.close();
            }
        });

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

/// Slim in-window bar shown when the native OS menu bar owns the File/Edit/…
/// verbs: it carries only the egui-only Appearance controls (dark/light +
/// text-size), which can't live in a native menu. Returns no [`MenuAction`] —
/// verb dispatch comes from the native bar's event channel.
pub fn appearance_only(ui: &mut egui::Ui, icons: &Icons) {
    egui::MenuBar::new().ui(ui, |ui| {
        appearance_controls(ui, icons);
    });
}

/// Dev/screenshot hook: render a given top-level menu's grouped items as an
/// open dropdown-style panel just under the bar, so `ITSJUSTCAD_SHOT` frames can
/// show the grouping without a live click. Set `ITSJUSTCAD_MENU_DEMO=<title>`
/// (e.g. `Solid`). Faithful — it lists exactly `verbs_in(categories)`.
pub fn demo_open(
    ctx: &egui::Context,
    icons: &Icons,
    style: MenuStyle,
    title: &str,
    at: egui::Pos2,
    plugins: &[PluginMenuEntry],
) {
    // The synthetic "Plugins" menu is demoed from the plugin list, not from a
    // registry category — used by the sanity screenshot.
    if title == "Plugins" {
        egui::Area::new(egui::Id::new("menu_demo"))
            .fixed_pos(at)
            .show(ctx, |ui| {
                egui::Frame::menu(ui.style()).show(ui, |ui| {
                    ui.set_min_width(180.0);
                    ui.label(egui::RichText::new("Plugins").strong());
                    ui.separator();
                    for (group, entries) in plugin_groups(plugins) {
                        if group != "Plugins" {
                            ui.label(egui::RichText::new(group).weak().small());
                        }
                        for entry in entries {
                            let _ = item(ui, icons, Icon::ToolsCat, &entry.name);
                        }
                    }
                });
            });
        return;
    }
    let Some((_, cats)) = top_menus(style).into_iter().find(|(t, _)| *t == title) else {
        return;
    };
    egui::Area::new(egui::Id::new("menu_demo"))
        .fixed_pos(at)
        .show(ctx, |ui| {
            egui::Frame::menu(ui.style()).show(ui, |ui| {
                ui.set_min_width(160.0);
                ui.label(egui::RichText::new(title).strong());
                ui.separator();
                for verb in verbs_in(&cats) {
                    let _ = item(ui, icons, verb_icon(verb), verb);
                }
            });
        });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn entry(name: &str, cat: &str, has_params: bool) -> PluginMenuEntry {
        PluginMenuEntry {
            name: name.into(),
            category: cat.into(),
            has_params,
            summary: String::new(),
        }
    }

    /// A parameterless plugin executes immediately; a parameterised one prefills.
    #[test]
    fn plugin_action_executes_or_prefills_by_params() {
        assert_eq!(
            plugin_action(&entry("greek-column", "Classical", false)),
            MenuAction::Execute("greek-column".into())
        );
        assert_eq!(
            plugin_action(&entry("grid", "Plugins", true)),
            MenuAction::Insert("grid ".into())
        );
    }

    /// Grouping preserves first-seen group order and collates entries per group.
    #[test]
    fn plugin_groups_collate_by_category() {
        let entries = vec![
            entry("a", "Classical", false),
            entry("b", "Plugins", false),
            entry("c", "Classical", true),
        ];
        let groups = plugin_groups(&entries);
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].0, "Classical");
        assert_eq!(groups[0].1.len(), 2);
        assert_eq!(groups[1].0, "Plugins");
        assert_eq!(groups[1].1.len(), 1);
    }

    /// The 13 categories, for exhaustiveness checks.
    const ALL_CATEGORIES: [Category; 13] = [
        Category::File,
        Category::Edit,
        Category::View,
        Category::Draw2d,
        Category::Curve,
        Category::Solid,
        Category::Boolean,
        Category::Transform,
        Category::Annotate,
        Category::Dimension,
        Category::Analyze,
        Category::Structure,
        Category::Tools,
    ];

    fn assert_partition(style: MenuStyle) {
        let mut seen: Vec<Category> = Vec::new();
        for (_, cats) in top_menus(style) {
            for c in cats {
                assert!(
                    !seen.contains(&c),
                    "category {c:?} appears in two menus for {style:?}"
                );
                seen.push(c);
            }
        }
        let seen_set: HashSet<_> = seen.iter().copied().collect();
        for c in ALL_CATEGORIES {
            assert!(
                seen_set.contains(&c),
                "category {c:?} missing from menus for {style:?}"
            );
        }
        assert_eq!(
            seen.len(),
            ALL_CATEGORIES.len(),
            "categories not a clean partition"
        );
    }

    #[test]
    fn rhino_menus_partition_all_categories() {
        assert_partition(MenuStyle::Rhino);
    }

    #[test]
    fn autocad_menus_partition_all_categories() {
        assert_partition(MenuStyle::AutoCAD);
    }

    /// The core guarantee: EVERY registry verb maps to exactly one menu, no
    /// orphans, for both preset styles.
    #[test]
    fn every_registry_verb_lands_in_exactly_one_menu() {
        for style in [MenuStyle::Rhino, MenuStyle::AutoCAD] {
            for spec in registry() {
                let menus: Vec<&str> = top_menus(style)
                    .into_iter()
                    .filter(|(_, cats)| cats.contains(&spec.category))
                    .map(|(t, _)| t)
                    .collect();
                assert_eq!(
                    menus.len(),
                    1,
                    "verb '{}' (category {:?}) maps to {menus:?} for {style:?}",
                    spec.name,
                    spec.category
                );
            }
        }
    }

    #[test]
    fn rhino_and_autocad_have_expected_titles() {
        let rhino: Vec<_> = top_menus(MenuStyle::Rhino)
            .iter()
            .map(|(t, _)| *t)
            .collect();
        assert_eq!(
            rhino,
            [
                "File",
                "Edit",
                "View",
                "Curve",
                "Solid",
                "Transform",
                "Dimension",
                "Analyze",
                "Structure",
                "Tools"
            ]
        );
        let acad: Vec<_> = top_menus(MenuStyle::AutoCAD)
            .iter()
            .map(|(t, _)| *t)
            .collect();
        assert_eq!(
            acad,
            [
                "File",
                "Edit",
                "View",
                "Draw",
                "Modify",
                "Dimension",
                "Format",
                "Structure",
                "Tools"
            ]
        );
    }

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
        // `box` needs a corner + size; the menu prefills for typing.
        assert_eq!(menu_action("box"), MenuAction::Insert("box ".to_string()));
        assert_eq!(menu_action("move"), MenuAction::Insert("move ".to_string()));
    }

    #[test]
    fn import_export_route_to_native_dialog() {
        // Picking Import/Export from a menu pops a native file dialog first
        // (rather than prefilling the command line), then runs through the
        // substrate with the chosen path.
        assert_eq!(menu_action("import"), MenuAction::ImportDialog);
        assert_eq!(menu_action("export"), MenuAction::ExportDialog);
    }

    #[test]
    fn every_registry_verb_has_a_menu_icon() {
        // Each category and each specific verb resolves to a Lucide [`Icon`] with
        // a non-empty stable name, so the menu icon column is always populated.
        for spec in registry() {
            assert!(
                !verb_icon(spec.name).name().is_empty(),
                "verb {} has no icon",
                spec.name
            );
        }
    }

    #[test]
    fn category_icons_are_distinct_per_category() {
        // Each registry category maps to its own group mark (no two categories
        // collapse to the same icon), so menu groups stay visually separable.
        use std::collections::HashSet;
        let cats = [
            Category::File,
            Category::Edit,
            Category::View,
            Category::Draw2d,
            Category::Curve,
            Category::Solid,
            Category::Boolean,
            Category::Transform,
            Category::Annotate,
            Category::Dimension,
            Category::Analyze,
            Category::Structure,
            Category::Tools,
        ];
        let mut seen = HashSet::new();
        for c in cats {
            // File and Tools may share their category-default with a wired action
            // icon, but the 11 drawing/analysis categories must be distinct.
            if matches!(c, Category::File | Category::Tools) {
                continue;
            }
            assert!(
                seen.insert(category_icon(c)),
                "category {c:?} icon collides"
            );
        }
    }

    // ── Native menu model (muda) tests ───────────────────────────────────────

    /// Collect all leaf (id, label, action) triples from a native model.
    fn leaves(menus: &[NativeMenu]) -> Vec<(String, String, MenuAction)> {
        menus
            .iter()
            .flat_map(|m| &m.items)
            .filter_map(|it| match it {
                NativeItem::Leaf { id, label, action } => {
                    Some((id.clone(), label.clone(), action.clone()))
                }
                NativeItem::Separator => None,
            })
            .collect()
    }

    #[test]
    fn native_model_has_same_top_titles_as_in_window_bar() {
        for style in [MenuStyle::Rhino, MenuStyle::AutoCAD] {
            let native: Vec<String> = native_model(style).iter().map(|m| m.title.clone()).collect();
            let mut expected: Vec<String> =
                top_menus(style).iter().map(|(t, _)| t.to_string()).collect();
            expected.push("Help".to_string());
            assert_eq!(native, expected, "native titles differ for {style:?}");
        }
    }

    #[test]
    fn native_file_menu_contains_expected_verbs_and_actions() {
        let file = native_model(MenuStyle::Rhino)
            .into_iter()
            .find(|m| m.title == "File")
            .expect("File menu");
        let ls = leaves(&[file]);
        // Wired File actions map to the right MenuAction.
        assert!(ls.iter().any(|(_, l, a)| l == "New" && *a == MenuAction::NewDocument));
        assert!(ls.iter().any(|(_, l, a)| l == "New file session" && *a == MenuAction::NewSession));
        assert!(ls.iter().any(|(_, l, a)| l == "Open…" && *a == MenuAction::Insert("open ".into())));
        // Import/Export route through the native file dialog, not the command line.
        assert!(ls.iter().any(|(_, l, a)| l == "Import…" && *a == MenuAction::ImportDialog));
        assert!(ls.iter().any(|(_, l, a)| l == "Export…" && *a == MenuAction::ExportDialog));
    }

    #[test]
    fn native_edit_menu_has_undo_redo_history() {
        let edit = native_model(MenuStyle::Rhino)
            .into_iter()
            .find(|m| m.title == "Edit")
            .expect("Edit menu");
        let ls = leaves(&[edit]);
        assert!(ls.iter().any(|(_, l, a)| l == "Undo" && *a == MenuAction::Execute("undo".into())));
        assert!(ls.iter().any(|(_, l, a)| l == "Redo" && *a == MenuAction::Execute("redo".into())));
        assert!(ls.iter().any(|(_, l, a)| l == "Edit history…" && *a == MenuAction::EditHistory));
    }

    #[test]
    fn native_view_menu_gathers_view_verbs() {
        let view = native_model(MenuStyle::Rhino)
            .into_iter()
            .find(|m| m.title == "View")
            .expect("View menu");
        let ls = leaves(&[view]);
        // Every registry View verb appears as a leaf.
        for v in verbs_in(&[Category::View]) {
            assert!(
                ls.iter().any(|(_, l, _)| l == v),
                "View menu missing verb {v}"
            );
        }
    }

    #[test]
    fn native_help_menu_is_synthetic() {
        let help = native_model(MenuStyle::AutoCAD)
            .into_iter()
            .find(|m| m.title == "Help")
            .expect("Help menu");
        let ls = leaves(&[help]);
        assert!(ls.iter().any(|(_, l, a)| l == "Command reference" && *a == MenuAction::Help));
        assert!(ls.iter().any(|(_, l, a)| l == "About ItsJustCAD" && *a == MenuAction::About));
    }

    #[test]
    fn native_leaf_ids_are_unique_and_map_back_to_actions() {
        // The muda layer keys a HashMap<id, MenuAction> off these ids, so every
        // leaf id must be unique within the bar.
        for style in [MenuStyle::Rhino, MenuStyle::AutoCAD] {
            let ls = leaves(&native_model(style));
            let ids: HashSet<&String> = ls.iter().map(|(id, _, _)| id).collect();
            assert_eq!(ids.len(), ls.len(), "duplicate native menu id for {style:?}");
        }
    }

    #[test]
    fn native_model_covers_every_registry_verb_once() {
        // Mirrors the in-window guarantee: every registry verb surfaces exactly
        // once. Most verbs use their name as the leaf label; the wired File/Edit
        // verbs get a nicer label ("Open…") but the same MenuAction, so match by
        // the action the leaf dispatches, not just the label text.
        let action_verb = |a: &MenuAction| -> Option<String> {
            match a {
                MenuAction::Execute(v) | MenuAction::StartDraw(v) => Some(v.clone()),
                MenuAction::Insert(p) => Some(p.trim_end().to_string()),
                MenuAction::ImportDialog => Some("import".into()),
                MenuAction::ExportDialog => Some("export".into()),
                _ => None,
            }
        };
        for style in [MenuStyle::Rhino, MenuStyle::AutoCAD] {
            let ls = leaves(&native_model(style));
            let verbs: Vec<String> = ls.iter().filter_map(|(_, _, a)| action_verb(a)).collect();
            for spec in registry() {
                let count = verbs.iter().filter(|v| *v == spec.name).count();
                assert_eq!(
                    count, 1,
                    "verb '{}' appears {count} times in native model for {style:?}",
                    spec.name
                );
            }
        }
    }

    #[test]
    fn verbs_in_returns_registry_members() {
        let solids = verbs_in(&[Category::Solid]);
        assert!(solids.contains(&"box"));
        assert!(solids.contains(&"extrude"));
        assert!(!solids.contains(&"line"));
    }
}
