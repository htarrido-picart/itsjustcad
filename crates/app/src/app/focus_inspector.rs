// SPDX-License-Identifier: AGPL-3.0-only
// Copyright © 2026 Hector Tarrido-Picart

//! Focused Lot and Street inspectors (#159). The Parameters panel switches on
//! [`lot::focus_target`]: one or more generated lots (or the buildings on
//! them) open the Lot inspector, the roads of one street open the Street
//! inspector, and anything else of a subdivision opens the plan-wide
//! subdivision inspector. A "Site plan ›" breadcrumb reaches that from a lot
//! or street. Every edit runs one logged command (one undo step) and then
//! re-selects the edited lot(s) / street, whose ids the re-derive replaced.

use egui::RichText;
use itsjustcad_commands::lot::{self, Focus, LotRules, LotView, StreetView};
use itsjustcad_commands::{Command, LotEdit, LotField, Selector};
use itsjustcad_doc::{ObjectId, StreetClass};
use subdivision::{EdgeRole, Typology};

use super::{App, project};
use crate::i18n::t;

/// Which inspector the Parameters panel shows for a focus.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Inspector {
    None,
    /// The plan-wide subdivision inspector; `back` is set when reached from a
    /// lot or street through the breadcrumb.
    Site {
        source: ObjectId,
        back: bool,
    },
    Lots {
        source: ObjectId,
        lots: Vec<ObjectId>,
    },
    Street {
        source: ObjectId,
        street_id: u32,
    },
}

/// The inspector for `focus`; `site_override` is the breadcrumb choice.
pub(super) fn inspector_for(focus: &Focus, site_override: bool) -> Inspector {
    match focus.clone() {
        Focus::None => Inspector::None,
        Focus::Site(source) => Inspector::Site {
            source,
            back: false,
        },
        f if site_override => Inspector::Site {
            source: f.source().expect("lot or street"),
            back: true,
        },
        Focus::Lots { source, lots } => Inspector::Lots { source, lots },
        Focus::Street { source, street_id } => Inspector::Street { source, street_id },
    }
}

/// The floor count shared by all `views`, if they agree.
pub(super) fn common_floors(views: &[LotView]) -> Option<usize> {
    let first = views.first()?.floors?;
    views
        .iter()
        .all(|v| v.floors == Some(first))
        .then_some(first)
}

/// The value `get` reads off every view's per-lot rules, if all views are site
/// plan lots and agree.
pub(super) fn common_rule<T: PartialEq>(
    views: &[LotView],
    get: impl Fn(&LotRules) -> T,
) -> Option<T> {
    let mut vals = views.iter().map(|v| v.rules.as_ref().map(&get));
    let first = vals.next()??;
    vals.all(|v| v.as_ref() == Some(&first)).then_some(first)
}

/// Some view pins `field` (`lotset`).
pub(super) fn field_overridden(views: &[LotView], field: LotField) -> bool {
    views
        .iter()
        .any(|v| v.rules.as_ref().is_some_and(|r| r.is_overridden(field)))
}

/// The setback fields of the Lot inspector with their labels, in grid order.
pub(super) const SETBACKS: [(LotField, &str); 4] = [
    (LotField::SetbackFront, "param.subdivision.setback_front"),
    (LotField::SetbackSide, "param.subdivision.setback_side"),
    (
        LotField::SetbackStreetSide,
        "param.subdivision.setback_street_side",
    ),
    (LotField::SetbackRear, "param.subdivision.setback_rear"),
];

/// A lot's effective setback `field` (m); `None` for a non-setback field.
pub(super) fn setback_of(r: &LotRules, field: LotField) -> Option<f64> {
    let s = &r.effective;
    match field {
        LotField::SetbackFront => Some(s.setback_front),
        LotField::SetbackSide => Some(s.setback_side),
        LotField::SetbackStreetSide => Some(s.street_side_setback()),
        LotField::SetbackRear => Some(s.setback_rear),
        LotField::Floors | LotField::Typology => None,
    }
}

/// A `lotset` edit setting setback `field` to `m`.
pub(super) fn setback_edit(field: LotField, m: f64) -> LotEdit {
    let mut e = LotEdit::default();
    match field {
        LotField::SetbackFront => e.setback_front = Some(m),
        LotField::SetbackSide => e.setback_side = Some(m),
        LotField::SetbackStreetSide => e.setback_street_side = Some(m),
        LotField::SetbackRear => e.setback_rear = Some(m),
        LotField::Floors | LotField::Typology => {}
    }
    e
}

/// A `lotset` edit resetting `field` to the plan's rule.
pub(super) fn clear_edit(field: LotField) -> LotEdit {
    LotEdit {
        clear: vec![field],
        ..LotEdit::default()
    }
}

fn typology_label(ty: Typology) -> &'static str {
    match ty {
        Typology::Detached => t("focus.typology.detached"),
        Typology::Row => t("focus.typology.row"),
        Typology::Courtyard => t("focus.typology.courtyard"),
        Typology::Slab => t("focus.typology.slab"),
    }
}

/// The lot(s)' own yield rows: (i18n key, value, unit).
pub(super) fn lot_yield_rows(views: &[LotView]) -> Vec<(&'static str, String, &'static str)> {
    let area: f64 = views.iter().map(|v| v.area).sum();
    let mut rows = vec![("focus.area", format!("{area:.0}"), "m²")];
    if !views.is_empty() {
        let frontage = views.iter().map(|v| v.frontage).sum::<f64>() / views.len() as f64;
        rows.push(("focus.frontage", format!("{frontage:.1}"), "m"));
    }
    let gfas: Vec<f64> = views.iter().filter_map(|v| v.gfa).collect();
    if !gfas.is_empty() {
        let gfa: f64 = gfas.iter().sum();
        rows.push(("focus.gfa", format!("{gfa:.0}"), "m²"));
        if area > 0.0 {
            rows.push(("focus.far", format!("{:.2}", gfa / area), ""));
        }
    }
    rows
}

const ROLES: [Option<EdgeRole>; 5] = [
    None,
    Some(EdgeRole::Front),
    Some(EdgeRole::StreetSide),
    Some(EdgeRole::Side),
    Some(EdgeRole::Rear),
];

fn role_label(role: Option<EdgeRole>) -> &'static str {
    match role {
        None => t("focus.role.auto"),
        Some(EdgeRole::Front) => t("focus.role.front"),
        Some(EdgeRole::StreetSide) => t("focus.role.street_side"),
        Some(EdgeRole::Side) => t("focus.role.side"),
        Some(EdgeRole::Rear) => t("focus.role.rear"),
    }
}

const CLASSES: [StreetClass; 4] = [
    StreetClass::Arterial,
    StreetClass::Collector,
    StreetClass::Local,
    StreetClass::Alley,
];

fn class_label(class: StreetClass) -> &'static str {
    match class {
        StreetClass::Arterial => t("focus.class.arterial"),
        StreetClass::Collector => t("focus.class.collector"),
        StreetClass::Local => t("focus.class.local"),
        StreetClass::Alley => t("focus.class.alley"),
    }
}

/// The breadcrumb title of a lot / street focus.
fn crumb_title(doc: &itsjustcad_doc::Document, focus: &Focus) -> String {
    match focus {
        Focus::Lots { lots, .. } if lots.len() == 1 => {
            format!("{} {}", t("focus.lot"), lots[0].short())
        }
        Focus::Lots { lots, .. } => format!("{} {}", lots.len(), t("focus.lots")),
        Focus::Street { source, street_id } => lot::street_view(doc, *source, *street_id)
            .and_then(|v| v.name)
            .unwrap_or_else(|| format!("{} {street_id}", t("focus.street"))),
        _ => String::new(),
    }
}

/// A numeric field edited in place: the in-flight value lives in egui temp
/// memory under `id` and is returned once the gesture ends (drag released or
/// focus lost), so one edit is one command.
fn drag_commit<T>(
    ui: &mut egui::Ui,
    id: egui::Id,
    shown: T,
    build: impl FnOnce(&mut T) -> egui::DragValue<'_>,
) -> Option<T>
where
    T: egui::emath::Numeric + Default + Clone + Send + Sync + 'static,
{
    let mut v = ui.data(|d| d.get_temp::<T>(id)).unwrap_or(shown);
    let resp = ui.add(build(&mut v));
    if resp.changed() {
        ui.data_mut(|d| d.insert_temp(id, v));
    }
    if resp.drag_stopped()
        || resp.lost_focus()
        || (resp.changed() && !resp.dragged() && !resp.has_focus())
    {
        return ui.data_mut(|d| d.remove_temp::<T>(id));
    }
    None
}

/// A subtle "overridden" marker.
fn override_mark(ui: &mut egui::Ui, on: bool) {
    if on {
        ui.label(RichText::new("●").small().color(ui.visuals().warn_fg_color))
            .on_hover_text(t("focus.overridden"));
    }
}

impl App {
    /// Render the inspector the focus asks for (the subdivision one for a
    /// site focus or after the breadcrumb).
    pub(super) fn focused_inspector(&mut self, ui: &mut egui::Ui, focus: &Focus) {
        if self
            .focus_site_override
            .as_ref()
            .is_some_and(|f| f != focus)
        {
            self.focus_site_override = None;
        }
        match inspector_for(focus, self.focus_site_override.is_some()) {
            Inspector::None => {}
            Inspector::Site { source, back } => {
                if back {
                    ui.horizontal(|ui| {
                        ui.strong(t("focus.site_plan"));
                        ui.weak("›");
                        if ui.link(crumb_title(&self.session.doc, focus)).clicked() {
                            self.focus_site_override = None;
                        }
                    });
                }
                self.subdivision_inspector(ui, source);
            }
            Inspector::Lots { source, lots } => self.lot_inspector(ui, focus, source, &lots),
            Inspector::Street { source, street_id } => {
                self.street_inspector(ui, focus, source, street_id)
            }
        }
    }

    fn focus_breadcrumb(&mut self, ui: &mut egui::Ui, focus: &Focus) {
        let title = crumb_title(&self.session.doc, focus);
        ui.horizontal(|ui| {
            if ui.link(t("focus.site_plan")).clicked() {
                self.focus_site_override = Some(focus.clone());
            }
            ui.weak("›");
            ui.strong(title);
        });
        ui.add_space(4.0);
    }

    /// Run one inspector edit, then re-select what was focused (the
    /// re-derive replaced its ids). Errors go to the command-line history.
    fn run_focus_edit(&mut self, focus: &Focus, cmd: Command) {
        let anchor = lot::focus_anchor(&self.session.doc, focus);
        match self.session.run(cmd) {
            Ok(_) => {
                if let Some(anchor) = anchor {
                    let sel = lot::refocus(&self.session.doc, &anchor);
                    if !sel.is_empty() {
                        self.session.doc.selection = sel;
                    }
                }
            }
            Err(e) => self.command_line.push_line(e.to_string()),
        }
    }

    fn lot_inspector(
        &mut self,
        ui: &mut egui::Ui,
        focus: &Focus,
        source: ObjectId,
        lots: &[ObjectId],
    ) {
        let doc = &self.session.doc;
        let views: Vec<LotView> = lots.iter().filter_map(|l| lot::lot_view(doc, *l)).collect();
        if views.is_empty() {
            return;
        }
        let frozen = doc.subdiv_link(source).is_some_and(|l| l.frozen);
        self.focus_breadcrumb(ui, focus);
        let targets = || Selector::Ids { ids: lots.to_vec() };
        // One `lotset` over every focused lot: one undo step.
        let set = |edit: LotEdit| Command::LotSet {
            targets: targets(),
            edit,
            ids: None,
        };
        // The overridden marker + ↺ reset cell of a `lotset` field.
        let reset_cell = |ui: &mut egui::Ui, field: LotField, edit: &mut Option<Command>| {
            let on = field_overridden(&views, field);
            ui.horizontal(|ui| {
                override_mark(ui, on);
                if on
                    && ui
                        .small_button("↺")
                        .on_hover_text(t("focus.reset_field"))
                        .clicked()
                {
                    *edit = Some(set(clear_edit(field)));
                }
            });
        };
        let mut edit: Option<Command> = None;

        ui.add_enabled_ui(!frozen, |ui| {
            egui::Grid::new(("focus_lot", source))
                .num_columns(3)
                .spacing([8.0, 6.0])
                .show(ui, |ui| {
                    // Floors: editing pins every focused lot (one `lotset`).
                    if views.iter().all(|v| v.floors.is_some()) {
                        let common = common_floors(&views);
                        ui.label(t("focus.floors"));
                        ui.horizontal(|ui| {
                            let id = ui.id().with(("focus_floors", lots));
                            let shown = common.unwrap_or(0) as u32;
                            let mixed = common.is_none();
                            let committed = drag_commit(ui, id, shown, |v| {
                                egui::DragValue::new(v)
                                    .range(if mixed { 0..=200 } else { 1..=200 })
                                    .speed(0.1)
                                    .custom_formatter(move |n, _| {
                                        if mixed && n == 0.0 {
                                            "—".into()
                                        } else {
                                            format!("{n:.0}")
                                        }
                                    })
                            });
                            if let Some(n) = committed.filter(|n| *n >= 1) {
                                edit = Some(set(LotEdit {
                                    floors: Some(n),
                                    ..LotEdit::default()
                                }));
                            }
                            if !views.iter().all(|v| v.floors_pinned)
                                && let Some(n) = common
                                && ui
                                    .small_button(t("focus.pin"))
                                    .on_hover_text(t("focus.pin.tooltip"))
                                    .clicked()
                            {
                                edit = Some(set(LotEdit {
                                    floors: Some(n as u32),
                                    ..LotEdit::default()
                                }));
                            }
                        });
                        reset_cell(ui, LotField::Floors, &mut edit);
                        ui.end_row();
                    }
                    // Typology and setbacks (`lotset`), when every lot is a
                    // site-plan lot.
                    if views.iter().all(|v| v.rules.is_some()) {
                        let common = common_rule(&views, |r| r.effective.typology);
                        ui.label(t("param.subdivision.typology"));
                        egui::ComboBox::from_id_salt(("focus_typology", lots))
                            .selected_text(common.map_or("—", typology_label))
                            .show_ui(ui, |ui| {
                                for ty in lot::TYPOLOGIES {
                                    if ui
                                        .selectable_label(common == Some(ty), typology_label(ty))
                                        .clicked()
                                        && common != Some(ty)
                                    {
                                        edit = Some(set(LotEdit {
                                            typology: Some(ty),
                                            ..LotEdit::default()
                                        }));
                                    }
                                }
                            });
                        reset_cell(ui, LotField::Typology, &mut edit);
                        ui.end_row();

                        for (field, label) in SETBACKS {
                            let common = common_rule(&views, |r| setback_of(r, field)).flatten();
                            ui.label(t(label));
                            let id = ui.id().with(("focus_setback", field.name(), lots));
                            let mixed = common.is_none();
                            let committed = drag_commit(ui, id, common.unwrap_or(-1.0), |v| {
                                egui::DragValue::new(v)
                                    .range(if mixed { -1.0..=100.0 } else { 0.0..=100.0 })
                                    .speed(0.1)
                                    .suffix(" m")
                                    .custom_formatter(move |n, _| {
                                        if mixed && n < 0.0 {
                                            "—".into()
                                        } else {
                                            format!("{n:.1}")
                                        }
                                    })
                            });
                            if let Some(m) = committed.filter(|m| *m >= 0.0) {
                                edit = Some(set(setback_edit(field, m)));
                            }
                            reset_cell(ui, field, &mut edit);
                            ui.end_row();
                        }
                    }
                });

            // Edge roles: per edge of one lot (`edgerole`).
            if let [view] = views.as_slice()
                && let Some(edges) = &view.edges
            {
                egui::CollapsingHeader::new(t("focus.edge_roles"))
                    .default_open(true)
                    .id_salt(("focus_edges", view.id))
                    .show(ui, |ui| {
                        egui::Grid::new(("focus_edge_grid", view.id))
                            .num_columns(4)
                            .striped(true)
                            .show(ui, |ui| {
                                for (i, tag) in edges.iter().enumerate() {
                                    ui.label(format!("{} {i}", t("focus.edge")));
                                    ui.monospace(format!("{:.1} m", tag.length));
                                    let current = tag.role.filter(|_| tag.overridden);
                                    let text = match (tag.overridden, tag.role) {
                                        (false, Some(r)) => format!(
                                            "{} ({})",
                                            role_label(None),
                                            role_label(Some(r))
                                        ),
                                        _ => role_label(current).to_string(),
                                    };
                                    egui::ComboBox::from_id_salt(("focus_role", view.id, i))
                                        .selected_text(text)
                                        .show_ui(ui, |ui| {
                                            for role in ROLES {
                                                if ui
                                                    .selectable_label(
                                                        current == role,
                                                        role_label(role),
                                                    )
                                                    .clicked()
                                                    && current != role
                                                {
                                                    edit = Some(Command::EdgeRole {
                                                        targets: Selector::Ids {
                                                            ids: vec![view.id],
                                                        },
                                                        edge: i,
                                                        role,
                                                        ids: None,
                                                    });
                                                }
                                            }
                                        });
                                    ui.horizontal(|ui| {
                                        override_mark(ui, tag.overridden);
                                        if tag.overridden
                                            && ui
                                                .small_button("↺")
                                                .on_hover_text(t("focus.reset_field"))
                                                .clicked()
                                        {
                                            edit = Some(Command::EdgeRole {
                                                targets: Selector::Ids { ids: vec![view.id] },
                                                edge: i,
                                                role: None,
                                                ids: None,
                                            });
                                        }
                                    });
                                    ui.end_row();
                                }
                            });
                    });
            }

            ui.add_space(4.0);
            let overridden = views.iter().any(LotView::overridden);
            if ui
                .add_enabled(overridden, egui::Button::new(t("focus.reset_lot")))
                .clicked()
            {
                edit = Some(Command::LotReset {
                    targets: targets(),
                    ids: None,
                });
            }
        });

        // The lot's own yield.
        ui.add_space(6.0);
        ui.strong(t("focus.yield"));
        egui::Grid::new(("focus_lot_yield", source))
            .num_columns(3)
            .striped(true)
            .show(ui, |ui| {
                let mut rows = lot_yield_rows(&views);
                if let Some(n) = common_floors(&views) {
                    rows.insert(2, ("focus.floors", n.to_string(), ""));
                }
                for (key, value, unit) in rows {
                    ui.label(t(key));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.monospace(value);
                    });
                    ui.monospace(unit);
                    ui.end_row();
                }
            });

        if let Some(cmd) = edit {
            self.run_focus_edit(focus, cmd);
        }
    }

    fn street_inspector(
        &mut self,
        ui: &mut egui::Ui,
        focus: &Focus,
        source: ObjectId,
        street_id: u32,
    ) {
        let doc = &self.session.doc;
        let Some(v): Option<StreetView> = lot::street_view(doc, source, street_id) else {
            return;
        };
        let frozen = doc.subdiv_link(source).is_some_and(|l| l.frozen);
        self.focus_breadcrumb(ui, focus);
        let set = |name: Option<String>, class: Option<StreetClass>, width: Option<f64>| {
            Command::StreetSet {
                targets: Selector::Ids {
                    ids: v.roads.clone(),
                },
                name,
                class,
                width,
                ids: None,
            }
        };
        let mut edit: Option<Command> = None;
        let mut select_lot: Option<ObjectId> = None;

        ui.add_enabled_ui(!frozen, |ui| {
            egui::Grid::new(("focus_street", source, street_id))
                .num_columns(3)
                .spacing([8.0, 6.0])
                .show(ui, |ui| {
                    ui.label(t("focus.name"));
                    let id = ui.id().with(("focus_street_name", source, street_id));
                    let current = v.name.clone().unwrap_or_default();
                    let mut text = ui
                        .data(|d| d.get_temp::<String>(id))
                        .unwrap_or_else(|| current.clone());
                    let resp = ui.add(egui::TextEdit::singleline(&mut text).desired_width(140.0));
                    if resp.changed() {
                        ui.data_mut(|d| d.insert_temp(id, text.clone()));
                    }
                    if resp.lost_focus() {
                        ui.data_mut(|d| d.remove_temp::<String>(id));
                        if text.trim() != current {
                            edit = Some(set(Some(text.trim().to_string()), None, None));
                        }
                    }
                    override_mark(ui, v.name_overridden);
                    ui.end_row();

                    ui.label(t("focus.class"));
                    egui::ComboBox::from_id_salt(("focus_class", source, street_id))
                        .selected_text(class_label(v.class))
                        .show_ui(ui, |ui| {
                            for class in CLASSES {
                                if ui
                                    .selectable_label(v.class == class, class_label(class))
                                    .clicked()
                                    && v.class != class
                                {
                                    edit = Some(set(None, Some(class), None));
                                }
                            }
                        });
                    override_mark(ui, v.class_overridden);
                    ui.end_row();

                    ui.label(t("focus.width"));
                    let id = ui.id().with(("focus_street_width", source, street_id));
                    let committed = drag_commit(ui, id, v.width, |w| {
                        egui::DragValue::new(w)
                            .range(1.0..=200.0)
                            .speed(0.1)
                            .suffix(" m")
                            .max_decimals(2)
                    });
                    if let Some(w) = committed.filter(|w| (w - v.width).abs() > 1e-9) {
                        edit = Some(set(None, None, Some(w)));
                    }
                    override_mark(ui, v.width_overridden);
                    ui.end_row();

                    ui.label(t("focus.length"));
                    ui.monospace(format!("{:.1} m", v.length));
                    ui.end_row();
                });
        });
        if v.edited {
            ui.colored_label(ui.visuals().warn_fg_color, t("focus.street_edited"));
        }

        ui.add_space(6.0);
        egui::CollapsingHeader::new(format!("{} ({})", t("focus.fronting_lots"), v.lots.len()))
            .default_open(true)
            .id_salt(("focus_fronting", source, street_id))
            .show(ui, |ui| {
                ui.horizontal_wrapped(|ui| {
                    for lot in &v.lots {
                        if ui.link(lot.short()).clicked() {
                            select_lot = Some(*lot);
                        }
                    }
                });
            });

        if let Some(lot) = select_lot {
            self.session.doc.selection = [lot].into();
        }
        if let Some(cmd) = edit {
            self.run_focus_edit(focus, cmd);
        }
    }

    /// Viewport overlay: outline the focused lot(s) / street, and a subtle
    /// badge on lots with `lotset` overrides and on hand-set edge roles. Pure 2D
    /// painting over the scene; cheap (pins and roles are few).
    pub(super) fn draw_focus_overlay(
        &self,
        ui: &egui::Ui,
        rect: egui::Rect,
        view_proj: glam::Mat4,
    ) {
        use itsjustcad_doc::Geometry;
        let doc = &self.session.doc;
        let painter = ui.painter_at(rect);
        let badge = egui::Color32::from_rgba_unmultiplied(232, 162, 48, 200);
        if doc.layer_visible(lot::LOTS_LAYER) {
            for link in doc.subdivision_links.values() {
                let z = link.source_z;
                for pin in &link.lot_overrides {
                    let at = glam::DVec3::new(pin.centroid.x, pin.centroid.y, z);
                    if let Some(p) = project(view_proj, rect, at) {
                        painter.circle_filled(p, 3.5, badge);
                    }
                }
                for role in &link.edge_roles {
                    let at = glam::DVec3::new(role.mid.x, role.mid.y, z);
                    if let Some(p) = project(view_proj, rect, at) {
                        painter.circle_stroke(p, 2.5, egui::Stroke::new(1.2, badge));
                    }
                }
            }
        }

        let ids: Vec<ObjectId> = match lot::focus_target(doc, &doc.selection) {
            Focus::Lots { lots, .. } => lots,
            Focus::Street { source, street_id } => doc
                .subdiv_link(source)
                .map(|l| {
                    l.produced
                        .iter()
                        .copied()
                        .filter(|id| {
                            doc.get(*id)
                                .and_then(|o| o.street.as_ref())
                                .is_some_and(|s| s.id == street_id)
                        })
                        .collect()
                })
                .unwrap_or_default(),
            _ => return,
        };
        let c = ui.visuals().selection.stroke.color;
        let stroke = egui::Stroke::new(
            3.0,
            egui::Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), 150),
        );
        for obj in ids.iter().filter_map(|id| doc.get(*id)) {
            let Geometry::Curve(curve) = &obj.geometry else {
                continue;
            };
            let mut pts: Vec<egui::Pos2> = curve
                .tessellate(0.05)
                .iter()
                .filter_map(|p| project(view_proj, rect, *p))
                .collect();
            if curve.is_closed()
                && let Some(&first) = pts.first()
            {
                pts.push(first);
            }
            painter.add(egui::Shape::line(pts, stroke));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view(area: f64, floors: Option<usize>, gfa: Option<f64>) -> LotView {
        LotView {
            id: ObjectId::new(),
            source: ObjectId::new(),
            area,
            frontage: 10.0,
            floors,
            floors_pinned: false,
            rules: None,
            gfa,
            buildings: Vec::new(),
            edges: None,
        }
    }

    #[test]
    fn inspector_follows_focus_and_breadcrumb() {
        let (source, lot) = (ObjectId::new(), ObjectId::new());
        let lots = Focus::Lots {
            source,
            lots: vec![lot],
        };
        assert_eq!(
            inspector_for(&lots, false),
            Inspector::Lots {
                source,
                lots: vec![lot]
            }
        );
        assert_eq!(
            inspector_for(&lots, true),
            Inspector::Site { source, back: true }
        );
        let street = Focus::Street {
            source,
            street_id: 3,
        };
        assert_eq!(
            inspector_for(&street, false),
            Inspector::Street {
                source,
                street_id: 3
            }
        );
        assert_eq!(
            inspector_for(&Focus::Site(source), true),
            Inspector::Site {
                source,
                back: false
            }
        );
        assert_eq!(inspector_for(&Focus::None, false), Inspector::None);
    }

    #[test]
    fn common_floors_needs_agreement() {
        assert_eq!(
            common_floors(&[view(1.0, Some(4), None), view(1.0, Some(4), None)]),
            Some(4)
        );
        assert_eq!(
            common_floors(&[view(1.0, Some(4), None), view(1.0, Some(5), None)]),
            None
        );
        assert_eq!(common_floors(&[view(1.0, None, None)]), None);
        assert_eq!(common_floors(&[]), None);
    }

    #[test]
    fn lot_yield_sums_area_and_gfa() {
        let rows = lot_yield_rows(&[
            view(200.0, Some(2), Some(300.0)),
            view(300.0, Some(2), Some(200.0)),
        ]);
        let get = |k: &str| rows.iter().find(|r| r.0 == k).map(|r| r.1.clone());
        assert_eq!(get("focus.area").as_deref(), Some("500"));
        assert_eq!(get("focus.gfa").as_deref(), Some("500"));
        assert_eq!(get("focus.far").as_deref(), Some("1.00"));
        assert!(
            lot_yield_rows(&[view(100.0, None, None)])
                .iter()
                .all(|r| r.0 != "focus.gfa")
        );
    }

    fn rules(edit: impl FnOnce(&mut LotRules)) -> LotRules {
        let s = subdivision::SubdivisionSettings::default();
        let mut r = LotRules {
            rule: s.clone(),
            rule_floors: 3,
            effective: s,
            floors: 3,
            overridden: Vec::new(),
        };
        edit(&mut r);
        r
    }

    #[test]
    fn lot_rule_fields_agree_or_show_mixed() {
        let with = |r: LotRules| LotView {
            rules: Some(r),
            ..view(1.0, Some(3), None)
        };
        let a = with(rules(|_| {}));
        let b = with(rules(|r| {
            r.effective.typology = Typology::Row;
            r.effective.setback_front = 7.0;
            r.overridden = vec![LotField::Typology, LotField::SetbackFront];
        }));
        let both = [a.clone(), b.clone()];
        let typ = |r: &LotRules| r.effective.typology;
        assert_eq!(
            common_rule(std::slice::from_ref(&b), typ),
            Some(Typology::Row)
        );
        assert_eq!(common_rule(&both, typ), None, "mixed typology");
        assert_eq!(
            common_rule(&[view(1.0, None, None)], typ),
            None,
            "not a site plan"
        );
        let side = |r: &LotRules| setback_of(r, LotField::SetbackSide);
        assert_eq!(
            common_rule(&both, side).flatten(),
            Some(a.rules.as_ref().unwrap().effective.setback_side)
        );
        let front = |r: &LotRules| setback_of(r, LotField::SetbackFront);
        assert_eq!(common_rule(&both, front), None);
        assert!(field_overridden(&both, LotField::Typology));
        assert!(!field_overridden(&both, LotField::SetbackRear));
        assert!(!field_overridden(&[a], LotField::Typology));
    }

    #[test]
    fn lot_rule_edits_are_one_field_lotsets() {
        for (field, _) in SETBACKS {
            let e = setback_edit(field, 4.0);
            let set = [
                e.setback_front,
                e.setback_side,
                e.setback_street_side,
                e.setback_rear,
            ];
            assert_eq!(set.iter().flatten().count(), 1, "{field:?}");
            assert!(e.floors.is_none() && e.typology.is_none() && e.clear.is_empty());
            assert!(setback_of(&rules(|_| {}), field).is_some());
        }
        assert!(setback_edit(LotField::Floors, 4.0).is_noop());
        assert_eq!(
            clear_edit(LotField::Typology).clear,
            vec![LotField::Typology]
        );
        assert_eq!(lot::TYPOLOGIES.len(), 4);
        for ty in lot::TYPOLOGIES {
            assert!(lotset_accepts(ty), "{ty:?}");
        }
    }

    /// The typology menu only offers what `lotset typology=` parses.
    fn lotset_accepts(ty: Typology) -> bool {
        itsjustcad_commands::parse(&format!("lotset typology={}", lot::typology_name(ty))).is_ok()
    }
}
