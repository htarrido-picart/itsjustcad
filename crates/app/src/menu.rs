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
pub fn ui(ui: &mut egui::Ui, icons: &Icons, style: MenuStyle) -> Option<MenuAction> {
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

        // Appearance controls, right-aligned on the menu bar (relocated from the
        // chat pane): dark/light toggle + text-size stepper apply app-wide.
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
    });
    action
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
) {
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

    #[test]
    fn verbs_in_returns_registry_members() {
        let solids = verbs_in(&[Category::Solid]);
        assert!(solids.contains(&"box"));
        assert!(solids.contains(&"extrude"));
        assert!(!solids.contains(&"line"));
    }
}
