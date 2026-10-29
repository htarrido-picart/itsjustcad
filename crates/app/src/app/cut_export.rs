// SPDX-License-Identifier: AGPL-3.0-only
// Copyright © 2026 Hector Tarrido-Picart

//! "Export for SendCutSend…" (#175, #209; desktop host of #153): check the
//! drawing against SendCutSend's limits (or the rule of thumb), then export a
//! cut DXF in mm or inches. Mirrors the iOS sheet (itsjustcad-ios
//! `CutExportSheet`): the preset (SendCutSend, or Generic
//! laser/waterjet/router: rule of thumb, no SendCutSend data or link, kept
//! per document), units, scope, material or thickness, the SendCutSend data, **Run Check**
//! (`cutcheck …`, FAIL then WARN rows that select their markers), and
//! **Export DXF…** (`export <path> profile=cut …`, confirmed first while FAIL
//! rows remain). Both lines run through the command line, so they are logged.
//!
//! The SendCutSend feeds are downloaded only when the user clicks the data
//! button, on a background thread, into [`scs::default_dir`]; nothing is
//! written unless both files arrive as JSON objects. Without them the check
//! uses the rule of thumb.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use egui::{Margin, RichText};
use itsjustcad_commands::cut::CutUnit;
use itsjustcad_commands::cut_preset::CutPreset;
use itsjustcad_commands::scs::{self, ScsData};
use itsjustcad_commands::{Command, Session, checkengine::COMPLIANCE_LAYER, cutcheck};
use itsjustcad_doc::{ComplianceReport, Document, ObjectId};

use super::App;
use crate::command_line::CommandLine;
use crate::i18n::t;
use crate::theme::{ColorRoles, Spacing, to_color32};

/// The window's egui id (screenshots crop to its rect).
pub const WINDOW_ID: &str = "cut_export";
/// SendCutSend's ordering site, linked after an export (no integration).
pub const SITE_URL: &str = "https://app.sendcutsend.com";
/// Default panel width (logical points).
const WIDTH: f32 = 400.0;
/// Materials listed at once in the picker (narrow the search for more).
const MATERIAL_ROWS: usize = 60;

/// What the check or the export covers.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CutScope {
    /// Every visible object except the `compliance` markers.
    #[default]
    All,
    Selection,
    Layers,
}

/// What the panel's form holds. `thickness` is in `units`; a SKU in
/// `material` carries its own thickness, so the thickness goes only without
/// one. The generic preset never sends a material.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CutForm {
    pub preset: CutPreset,
    pub units: CutUnit,
    pub scope: CutScope,
    pub layers: Vec<String>,
    pub material: Option<String>,
    pub thickness: Option<f64>,
}

impl CutForm {
    /// A layer name the command line can carry (split on spaces and commas).
    pub fn is_token_safe(name: &str) -> bool {
        !name.is_empty() && !name.chars().any(|c| c.is_whitespace() || c == ',')
    }

    /// Why the check can't run yet (an i18n key), or `None`.
    pub fn blocker(&self) -> Option<&'static str> {
        if self.scope == CutScope::Layers && self.layers.is_empty() {
            return Some("cut.blocker.layers");
        }
        if self.material_to_send().is_none() && !self.thickness.is_some_and(|t| t > 0.0) {
            return Some(if self.preset.is_scs() {
                "cut.blocker.material"
            } else {
                "cut.blocker.thickness"
            });
        }
        None
    }

    /// Whether the export can run (it needs no thickness).
    pub fn can_export(&self) -> bool {
        !(self.scope == CutScope::Layers && self.layers.is_empty())
    }

    fn material_to_send(&self) -> Option<&str> {
        self.material.as_deref().filter(|_| self.preset.is_scs())
    }

    fn thickness_to_send(&self) -> Option<f64> {
        self.material_to_send().is_none().then_some(self.thickness).flatten()
    }

    /// The `cutcheck` line.
    pub fn check_line(&self) -> String {
        let mut parts = vec!["cutcheck".to_string()];
        if self.scope == CutScope::Selection {
            parts.push("sel".into());
        }
        if !self.preset.is_scs() {
            parts.push(format!("preset={}", self.preset.label()));
        }
        parts.push(format!("units={}", self.units.label()));
        if let Some(t) = self.thickness_to_send() {
            parts.push(format!("thickness={}", fmt_num(t)));
        }
        if let Some(m) = self.material_to_send() {
            parts.push(format!("material={m}"));
        }
        if self.scope == CutScope::Layers {
            parts.push(format!("layers={}", self.layers.join(",")));
        }
        parts.join(" ")
    }

    /// The `export … profile=cut` line for `path`.
    pub fn export_line(&self, path: &Path) -> String {
        let mut parts = vec![
            "export".to_string(),
            path.display().to_string(),
            "profile=cut".into(),
            format!("units={}", self.units.label()),
        ];
        if self.scope == CutScope::Layers {
            parts.push(format!("layers={}", self.layers.join(",")));
        }
        if self.scope == CutScope::Selection {
            parts.push("sel".into());
        }
        parts.join(" ")
    }

    /// The export as a command. Parsed with a placeholder path, then given the
    /// real one, so a path with spaces survives the whitespace tokenizer.
    pub fn export_command(&self, path: &Path) -> Result<Command, String> {
        let line = self.export_line(Path::new("cut.dxf"));
        match itsjustcad_commands::parse(&line).map_err(|e| e.to_string())? {
            Command::Export { cut, .. } => Ok(Command::Export {
                path: path.display().to_string(),
                cut,
            }),
            other => Err(format!("unexpected command {other:?}")),
        }
    }
}

/// Up to 3 decimals, trailing zeros trimmed.
pub fn fmt_num(v: f64) -> String {
    let s = format!("{v:.3}");
    let s = s.trim_end_matches('0').trim_end_matches('.');
    if s == "-0" { "0".into() } else { s.into() }
}

/// One failing or warning check, with its markers on the `compliance` layer.
#[derive(Clone, Debug, PartialEq)]
pub struct CutFinding {
    pub id: String,
    pub verdict: String,
    pub message: String,
    pub measured: Option<f64>,
    pub limit: Option<f64>,
    pub unit: String,
    pub objects: usize,
    pub markers: Vec<ObjectId>,
}

impl CutFinding {
    pub fn is_fail(&self) -> bool {
        self.verdict == "fail"
    }

    /// The localized rule title (the rule id when unknown).
    pub fn title(&self) -> String {
        let key = format!("cut.rule.{}", self.id);
        let s = t(&key);
        if s == key {
            self.id.clone()
        } else {
            s.to_string()
        }
    }

    /// "0.5 mm, limit 1.5 mm" — or the object count when nothing is measured.
    pub fn value_text(&self) -> String {
        let n = |v: Option<f64>| v.map(|x| format!("{} {}", fmt_num(x), self.unit));
        match (n(self.measured), n(self.limit)) {
            (Some(m), Some(l)) => t("cut.value.limit").replace("{m}", &m).replace("{l}", &l),
            (Some(m), None) => m,
            _ if self.objects == 1 => t("cut.objects.one").to_string(),
            _ => t("cut.objects.n").replace("{n}", &self.objects.to_string()),
        }
    }
}

/// One check's result: the report's context and its FAIL/WARN rows.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CutResult {
    pub error: Option<String>,
    pub context: String,
    pub findings: Vec<CutFinding>,
    pub fail: usize,
    pub warn: usize,
}

impl CutResult {
    pub fn failure(error: impl Into<String>) -> Self {
        Self {
            error: Some(error.into()),
            ..Self::default()
        }
    }

    /// FAIL then WARN rows, each with its slice of `markers` (one marker per
    /// violating object, in rule order — how `cutcheck` places them).
    pub fn from_report(report: &ComplianceReport, markers: &[ObjectId]) -> Self {
        let mut at = 0;
        let mut out: Vec<CutFinding> = Vec::new();
        for r in &report.rules {
            let n = r.objects.len();
            let ids = markers[at.min(markers.len())..(at + n).min(markers.len())].to_vec();
            at += n;
            if r.verdict != "fail" && r.verdict != "warn" {
                continue;
            }
            out.push(CutFinding {
                id: r.rule_id.clone(),
                verdict: r.verdict.clone(),
                message: r.message.clone(),
                measured: r.measured,
                limit: r.required,
                unit: r.unit.clone(),
                objects: n,
                markers: ids,
            });
        }
        let (fail, warn): (Vec<_>, Vec<_>) = out.into_iter().partition(CutFinding::is_fail);
        Self {
            error: None,
            context: report.context.clone(),
            fail: fail.len(),
            warn: warn.len(),
            findings: fail.into_iter().chain(warn).collect(),
        }
    }

    /// Where the limits came from: the SKU and data date, or the rule of thumb.
    pub fn limits_source(&self) -> Option<&str> {
        let (_, tail) = self.context.split_once("limits: ")?;
        Some(tail.split("; NOTE: ").next().unwrap_or(tail))
    }

    /// The core's notes (fallbacks, thickness mismatch, stock).
    pub fn notes(&self) -> Vec<&str> {
        self.context.split("; NOTE: ").skip(1).collect()
    }

    /// The limits as one line ("min hole 1.5, bridge 1.5, … mm").
    pub fn limits_summary(&self) -> Option<String> {
        let (_, tail) = self.context.split_once("min hole ")?;
        let (body, _) = tail.split_once("; limits: ")?;
        Some(t("cut.limits.summary").replace("{s}", body))
    }

    /// Whether exporting must be confirmed first.
    pub fn needs_confirm(&self) -> bool {
        self.fail > 0
    }
}

/// The SendCutSend feed URLs and their file names.
pub fn feed_files() -> [(String, &'static str); 2] {
    [scs::SPECS_FILE, scs::CATALOG_FILE].map(|f| (format!("{}{f}", scs::FEED_URL), f))
}

/// Where a data update is.
#[derive(Clone, Debug, PartialEq)]
pub enum UpdateState {
    /// Fetching feed `n` (0-based) of [`feed_files`].
    Fetching(usize),
    Done,
    Failed(String),
}

/// Fetch both feeds, check each is a JSON object, then write both into `dir`.
/// Nothing is written unless both arrive intact, so a failed update keeps the
/// old cache. `progress` hears each feed's index before its fetch.
pub fn update_data(
    dir: &Path,
    fetch: &mut dyn FnMut(&str) -> Result<Vec<u8>, String>,
    progress: &mut dyn FnMut(usize),
) -> Result<(), String> {
    let mut got = Vec::new();
    for (i, (url, name)) in feed_files().iter().enumerate() {
        progress(i);
        let bytes = fetch(url).map_err(|e| {
            t("cut.data.err.download")
                .replace("{file}", name)
                .replace("{e}", &e)
        })?;
        if !serde_json::from_slice::<serde_json::Value>(&bytes).is_ok_and(|v| v.is_object()) {
            return Err(t("cut.data.err.invalid").replace("{file}", name));
        }
        got.push((*name, bytes));
    }
    let save = |e: std::io::Error| t("cut.data.err.save").replace("{e}", &e.to_string());
    std::fs::create_dir_all(dir).map_err(save)?;
    for (name, bytes) in got {
        // Write beside, then rename: a crash never leaves half a feed.
        let tmp = dir.join(format!("{name}.part"));
        std::fs::write(&tmp, bytes).map_err(save)?;
        std::fs::rename(&tmp, dir.join(name)).map_err(save)?;
    }
    Ok(())
}

/// What the user asked for this frame (the host acts on it).
#[derive(Clone, Debug, PartialEq)]
pub enum PanelAction {
    Check,
    /// Select these markers and zoom to them.
    Focus(Vec<ObjectId>),
    /// Pick a path, then export (FAILs already confirmed).
    Export,
    UpdateData,
    OpenSite,
}

pub struct CutExportPanel {
    pub form: CutForm,
    /// The selection when the panel opened: a focused marker replaces it.
    scope_selection: Vec<ObjectId>,
    layer_names: Vec<String>,
    data_dir: Option<PathBuf>,
    /// The cached feeds; `None` when nothing is cached.
    data: Option<ScsData>,
    material_query: String,
    materials_open: bool,
    thickness_text: String,
    update: Option<Arc<Mutex<UpdateState>>>,
    update_error: Option<String>,
    pub result: Option<CutResult>,
    focused: Option<String>,
    /// The last check's markers, replaced by the next check.
    last_markers: Vec<ObjectId>,
    pub confirm_export: bool,
    pub exported: Option<PathBuf>,
    pub export_error: Option<String>,
    /// The form the result was computed for (a change clears the result).
    checked_form: CutForm,
    /// Scroll the findings into view on the next frame (after a check).
    scroll_findings: bool,
}

impl CutExportPanel {
    /// Open on `doc`: scope = the selection when there is one; the data from
    /// `data_dir` (normally [`scs::default_dir`]).
    pub fn new(doc: &Document, data_dir: Option<PathBuf>) -> Self {
        let scope_selection: Vec<ObjectId> = doc.selection.iter().copied().collect();
        let form = CutForm {
            scope: if scope_selection.is_empty() {
                CutScope::All
            } else {
                CutScope::Selection
            },
            ..CutForm::default()
        };
        let layer_names = doc
            .layers
            .keys()
            .filter(|n| n.as_str() != COMPLIANCE_LAYER && CutForm::is_token_safe(n))
            .cloned()
            .collect();
        let data = data_dir.as_deref().and_then(|d| scs::load(d).ok());
        Self {
            checked_form: form.clone(),
            form,
            scope_selection,
            layer_names,
            data_dir,
            data,
            material_query: String::new(),
            materials_open: false,
            thickness_text: String::new(),
            update: None,
            update_error: None,
            result: None,
            focused: None,
            last_markers: Vec::new(),
            confirm_export: false,
            exported: None,
            export_error: None,
            scroll_findings: false,
        }
    }

    /// Start on `preset` (the document's last one).
    pub fn with_preset(mut self, preset: CutPreset) -> Self {
        self.form.preset = preset;
        self.checked_form.preset = preset;
        self
    }

    /// Put back the selection the scope means before the core reads it.
    fn restore_scope(&self, doc: &mut Document) {
        if self.form.scope == CutScope::Selection {
            doc.selection = self.scope_selection.iter().copied().collect();
        }
    }

    /// Run `cutcheck` (logged): drop the previous check's markers, restore the
    /// scope's selection, then read the stored report.
    pub fn run_check(&mut self, cl: &mut CommandLine, session: &mut Session) {
        let old: BTreeSet<ObjectId> = self
            .last_markers
            .drain(..)
            .filter(|id| session.doc.get(*id).is_some())
            .collect();
        if !old.is_empty() {
            session.doc.selection = old;
            cl.execute(session, "delete sel");
        }
        session.doc.selection.clear();
        self.restore_scope(&mut session.doc);
        self.focused = None;
        self.confirm_export = false;
        self.checked_form = self.form.clone();
        self.scroll_findings = true;
        let line = self.form.check_line();
        self.result = Some(match cl.execute_outcome(session, &line) {
            Ok(outcome) => match session.doc.compliance_reports.get(cutcheck::PACK) {
                Some(r) => {
                    self.last_markers = outcome.created.clone();
                    CutResult::from_report(r, &outcome.created)
                }
                None => CutResult::failure(t("cut.check.failed")),
            },
            Err(e) => CutResult::failure(e),
        });
    }

    /// Export through the command line (logged) to `path`.
    pub fn export_to(&mut self, cl: &mut CommandLine, session: &mut Session, path: &Path) {
        self.confirm_export = false;
        session.doc.selection.clear();
        self.restore_scope(&mut session.doc);
        let line = self.form.export_line(path);
        let res = self
            .form
            .export_command(path)
            .and_then(|cmd| cl.execute_command_outcome(session, &line, cmd));
        match res {
            Ok(_) => {
                self.exported = Some(path.to_path_buf());
                self.export_error = None;
            }
            Err(e) => {
                self.exported = None;
                self.export_error = Some(if e.contains("no curves in scope") {
                    t("cut.export.empty").to_string()
                } else {
                    e
                });
            }
        }
    }

    /// Start a data update on a background thread with `fetch`.
    pub fn start_update(
        &mut self,
        mut fetch: impl FnMut(&str) -> Result<Vec<u8>, String> + Send + 'static,
    ) {
        let Some(dir) = self.data_dir.clone() else {
            return;
        };
        let state = Arc::new(Mutex::new(UpdateState::Fetching(0)));
        self.update = Some(state.clone());
        self.update_error = None;
        std::thread::spawn(move || {
            let s = state.clone();
            let mut progress = move |i| *s.lock().expect("update state") = UpdateState::Fetching(i);
            let done = match update_data(&dir, &mut fetch, &mut progress) {
                Ok(()) => UpdateState::Done,
                Err(e) => UpdateState::Failed(e),
            };
            *state.lock().expect("update state") = done;
        });
    }

    /// Fold a finished update into the panel; `true` while one runs.
    fn poll_update(&mut self) -> bool {
        let Some(state) = &self.update else {
            return false;
        };
        let s = state.lock().expect("update state").clone();
        match s {
            UpdateState::Fetching(_) => return true,
            UpdateState::Done => {
                self.data = self.data_dir.as_deref().and_then(|d| scs::load(d).ok());
            }
            UpdateState::Failed(e) => self.update_error = Some(e),
        }
        self.update = None;
        false
    }

    /// Draw the window; `open` turns false when it is closed.
    pub fn ui(
        &mut self,
        ctx: &egui::Context,
        roles: &ColorRoles,
        open: &mut bool,
    ) -> Option<PanelAction> {
        if self.form != self.checked_form {
            self.checked_form = self.form.clone();
            self.result = None;
            self.focused = None;
            self.exported = None;
            self.confirm_export = false;
        }
        let updating = self.poll_update();
        if updating {
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
        }
        let mut action = None;
        egui::Window::new(t("cut.title"))
            .id(egui::Id::new(WINDOW_ID))
            .collapsible(false)
            .resizable(true)
            .default_width(WIDTH)
            // Tall enough for the form and a few findings; scrolls past that.
            .default_height((ctx.content_rect().height() - 96.0).clamp(240.0, 820.0))
            .open(open)
            .show(ctx, |ui| {
                egui::ScrollArea::vertical()
                    .auto_shrink([false, true])
                    .show(ui, |ui| {
                        egui::Frame::NONE
                            .inner_margin(Margin::symmetric(Spacing::M as i8, Spacing::S as i8))
                            .show(ui, |ui| {
                                ui.spacing_mut().item_spacing = egui::vec2(Spacing::S, 6.0);
                                ui.spacing_mut().button_padding = egui::vec2(Spacing::SM, 5.0);
                                self.body(ui, roles, updating, &mut action);
                            });
                    });
            });
        action
    }

    fn body(
        &mut self,
        ui: &mut egui::Ui,
        roles: &ColorRoles,
        updating: bool,
        action: &mut Option<PanelAction>,
    ) {
        // Preset.
        section(ui, t("cut.preset"), |ui| {
            ui.horizontal_wrapped(|ui| {
                for (p, key) in [
                    (CutPreset::Scs, "cut.preset.scs"),
                    (CutPreset::Generic, "cut.preset.generic"),
                ] {
                    ui.selectable_value(&mut self.form.preset, p, t(key));
                }
            });
        });
        let scs = self.form.preset.is_scs();
        footer(
            ui,
            t(if scs {
                "cut.preset.footer.scs"
            } else {
                "cut.preset.footer.generic"
            }),
        );

        // Units.
        section(ui, t("cut.units"), |ui| {
            ui.horizontal(|ui| {
                ui.selectable_value(&mut self.form.units, CutUnit::Mm, t("cut.units.mm"));
                ui.selectable_value(&mut self.form.units, CutUnit::In, t("cut.units.in"));
            });
        });

        // Scope.
        section(ui, t("cut.scope"), |ui| {
            ui.horizontal_wrapped(|ui| {
                for (s, key) in [
                    (CutScope::All, "cut.scope.all"),
                    (CutScope::Selection, "cut.scope.selection"),
                    (CutScope::Layers, "cut.scope.layers"),
                ] {
                    ui.selectable_value(&mut self.form.scope, s, t(key));
                }
            });
            if self.form.scope == CutScope::Layers {
                for name in &self.layer_names {
                    let mut on = self.form.layers.contains(name);
                    if ui.checkbox(&mut on, name.as_str()).changed() {
                        self.form.layers.retain(|l| l != name);
                        if on {
                            self.form.layers.push(name.clone());
                        }
                    }
                }
            }
        });
        let n = self.scope_selection.len();
        match self.form.scope {
            CutScope::Selection => footer(
                ui,
                &match n {
                    0 => t("cut.scope.none_selected").to_string(),
                    1 => t("cut.scope.one_selected").to_string(),
                    _ => t("cut.scope.n_selected").replace("{n}", &n.to_string()),
                },
            ),
            CutScope::Layers if self.layer_names.is_empty() => footer(ui, t("cut.scope.no_layers")),
            _ => {}
        }

        if scs {
            self.scs_ui(ui, roles, updating, action);
        } else {
            // Generic: the rule of thumb needs only a thickness.
            section(ui, t("cut.material"), |ui| self.thickness_row(ui));
        }

        // Run Check.
        ui.add_space(Spacing::M);
        let blocker = self.form.blocker();
        if prominent(ui, roles, t("cut.check"), blocker.is_none()).clicked() {
            *action = Some(PanelAction::Check);
        }
        if let Some(b) = blocker {
            footer(ui, t(b));
        }

        // Findings.
        if let Some(r) = self.result.clone() {
            self.findings_ui(ui, roles, &r, action);
        }

        // Export.
        ui.add_space(Spacing::M);
        if self.confirm_export {
            self.confirm_ui(ui, roles, action);
        } else if prominent(ui, roles, t("cut.export"), self.form.can_export()).clicked() {
            if self.result.as_ref().is_some_and(CutResult::needs_confirm) {
                self.confirm_export = true;
            } else {
                *action = Some(PanelAction::Export);
            }
        }
        if let Some(e) = &self.export_error {
            ui.add_space(Spacing::XS);
            ui.label(RichText::new(e).color(to_color32(roles.destructive)));
        }
        if let Some(p) = &self.exported {
            ui.add_space(Spacing::XS);
            ui.label(t("cut.export.done").replace("{path}", &p.display().to_string()));
            // Only SendCutSend takes the file online.
            if self.form.preset.is_scs() && ui.link(t("cut.open_scs")).clicked() {
                *action = Some(PanelAction::OpenSite);
            }
        }
        footer(
            ui,
            t(if self.form.units == CutUnit::Mm {
                "cut.export.footer.mm"
            } else {
                "cut.export.footer.in"
            }),
        );
        ui.add_space(Spacing::S);
    }

    /// The SendCutSend preset's material picker and data section.
    fn scs_ui(
        &mut self,
        ui: &mut egui::Ui,
        roles: &ColorRoles,
        updating: bool,
        action: &mut Option<PanelAction>,
    ) {
        // Material or thickness.
        section(ui, t("cut.material"), |ui| self.material_ui(ui));
        footer(
            ui,
            t(if self.data.is_none() {
                "cut.material.footer_nodata"
            } else {
                "cut.material.footer"
            }),
        );

        // SendCutSend data.
        section(ui, t("cut.data"), |ui| {
            match &self.data {
                Some(d) => {
                    let date = d.date.as_deref().map(|s| s.get(..10).unwrap_or(s));
                    row(
                        ui,
                        t("cut.data.date"),
                        date.unwrap_or(t("cut.data.unknown_date")),
                    );
                }
                None => {
                    ui.label(t("cut.data.none"));
                }
            }
            ui.horizontal(|ui| {
                let label = t(if self.data.is_none() {
                    "cut.data.download"
                } else {
                    "cut.data.update"
                });
                let b = ui.add_enabled(
                    !updating && self.data_dir.is_some(),
                    egui::Button::new(label),
                );
                if b.clicked() {
                    *action = Some(PanelAction::UpdateData);
                }
                if updating {
                    ui.spinner();
                    if let Some(UpdateState::Fetching(i)) = self
                        .update
                        .as_ref()
                        .map(|s| s.lock().expect("update state").clone())
                    {
                        let file = feed_files()[i.min(1)].1;
                        ui.weak(t("cut.data.progress").replace("{file}", file));
                    }
                }
            });
        });
        if let Some(e) = &self.update_error {
            ui.add_space(Spacing::XS);
            ui.label(RichText::new(e).color(to_color32(roles.destructive)));
        } else if self.data.is_none() {
            footer(ui, t("cut.data.rule_of_thumb"));
        } else {
            footer(ui, t("cut.data.offline"));
        }
    }

    fn material_ui(&mut self, ui: &mut egui::Ui) {
        let current = self
            .form
            .material
            .clone()
            .unwrap_or_else(|| t("cut.material.none").into());
        ui.horizontal(|ui| {
            ui.label(t("cut.material"));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let b = ui
                    .add_enabled(
                        self.data.is_some(),
                        egui::Button::new(format!("{current}     ")),
                    )
                    .on_disabled_hover_text(t("cut.material.footer_nodata"));
                // A drawn chevron (the UI font has no ▾ glyph).
                let c = egui::pos2(b.rect.right() - Spacing::SM, b.rect.center().y);
                let color = ui.visuals().widgets.style(&b).fg_stroke.color;
                let (w, h) = (4.0, 2.5);
                ui.painter().add(egui::Shape::convex_polygon(
                    vec![
                        c + egui::vec2(-w, -h),
                        c + egui::vec2(w, -h),
                        c + egui::vec2(0.0, h),
                    ],
                    color,
                    egui::Stroke::NONE,
                ));
                if b.clicked() {
                    self.materials_open = !self.materials_open;
                }
            });
        });
        if self.materials_open
            && let Some(data) = &self.data
        {
            ui.add(
                egui::TextEdit::singleline(&mut self.material_query)
                    .hint_text(t("cut.material.search"))
                    .desired_width(f32::INFINITY),
            );
            let hits = data.filter(&self.material_query);
            let mut pick: Option<Option<String>> = None;
            egui::ScrollArea::vertical()
                .id_salt("cut_materials")
                .max_height(180.0)
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    if material_row(
                        ui,
                        t("cut.material.none"),
                        t("cut.material.none_detail"),
                        self.form.material.is_none(),
                    ) {
                        pick = Some(None);
                    }
                    for m in hits.iter().take(MATERIAL_ROWS) {
                        let thick = m
                            .thickness
                            .map(|t| format!("{} in ({:.2} mm)", fmt_num(t), t * 25.4));
                        let detail: Vec<&str> =
                            [Some(m.sku.as_str()), thick.as_deref(), m.process.as_deref()]
                                .into_iter()
                                .flatten()
                                .collect();
                        let on = self.form.material.as_deref() == Some(m.sku.as_str());
                        if material_row(ui, &m.name, &detail.join(" · "), on) {
                            pick = Some(Some(m.sku.clone()));
                        }
                    }
                    if hits.is_empty() {
                        ui.weak(t("cut.material.no_match"));
                    }
                });
            if let Some(p) = pick {
                self.form.material = p;
                self.materials_open = false;
            }
        }
        if self.form.material.is_none() {
            self.thickness_row(ui);
        }
    }

    /// "Thickness ……… [3] mm".
    fn thickness_row(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label(t("cut.thickness"));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.weak(self.form.units.label());
                let r = ui.add(
                    egui::TextEdit::singleline(&mut self.thickness_text)
                        .hint_text("3")
                        .desired_width(72.0)
                        .horizontal_align(egui::Align::Max),
                );
                if r.changed() {
                    self.form.thickness = self.thickness_text.trim().replace(',', ".").parse().ok();
                }
            });
        });
    }

    fn findings_ui(
        &mut self,
        ui: &mut egui::Ui,
        roles: &ColorRoles,
        r: &CutResult,
        action: &mut Option<PanelAction>,
    ) {
        let title = if r.error.is_some() {
            t("cut.check.header")
        } else {
            t("cut.findings")
        };
        let header = section(ui, title, |ui| {
            if let Some(e) = &r.error {
                ui.label(RichText::new(e).color(to_color32(roles.destructive)));
                return;
            }
            if r.findings.is_empty() {
                ui.label(
                    RichText::new(format!("✔ {}", t("cut.clean")))
                        .color(egui::Color32::from_rgb(0x2e, 0xa0, 0x43)),
                );
            }
            for f in &r.findings {
                let focused = self.focused.as_deref() == Some(f.id.as_str());
                if finding_row(ui, roles, f, focused).clicked() {
                    self.focused = Some(f.id.clone());
                    *action = Some(PanelAction::Focus(f.markers.clone()));
                }
            }
        });
        if std::mem::take(&mut self.scroll_findings) {
            header.scroll_to_me(Some(egui::Align::Min));
        }
        if r.error.is_none() {
            if let Some(s) = r.limits_summary() {
                footer(ui, &s);
            }
            if let Some(src) = r.limits_source() {
                footer(ui, &t("cut.limits").replace("{src}", src));
            }
            for n in r.notes() {
                footer(ui, &t("cut.note").replace("{n}", n));
            }
        }
    }

    fn confirm_ui(
        &mut self,
        ui: &mut egui::Ui,
        roles: &ColorRoles,
        action: &mut Option<PanelAction>,
    ) {
        let n = self.result.as_ref().map_or(0, |r| r.fail);
        let title = if n == 1 {
            t("cut.confirm.one").to_string()
        } else {
            t("cut.confirm.n").replace("{n}", &n.to_string())
        };
        egui::Frame::group(ui.style())
            .stroke(egui::Stroke::new(1.0, to_color32(roles.destructive)))
            .inner_margin(Margin::same(Spacing::S as i8 + 2))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.label(RichText::new(title).strong());
                ui.label(t("cut.confirm.body"));
                ui.add_space(Spacing::XS);
                ui.horizontal(|ui| {
                    if ui
                        .button(
                            RichText::new(t("cut.confirm.export"))
                                .color(to_color32(roles.destructive)),
                        )
                        .clicked()
                    {
                        *action = Some(PanelAction::Export);
                    }
                    if ui.button(t("btn.cancel")).clicked() {
                        self.confirm_export = false;
                    }
                });
            });
    }
}

/// A titled group with comfortable inner padding; returns the title's response.
fn section(ui: &mut egui::Ui, title: &str, add: impl FnOnce(&mut egui::Ui)) -> egui::Response {
    ui.add_space(Spacing::S);
    let header = ui.label(RichText::new(title).strong());
    egui::Frame::group(ui.style())
        .inner_margin(Margin::symmetric(Spacing::SM as i8, Spacing::S as i8))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui);
        });
    header
}

/// Secondary text under a section.
fn footer(ui: &mut egui::Ui, text: &str) {
    ui.add(egui::Label::new(RichText::new(text).weak().size(12.5)).wrap());
}

/// "Label ……… value".
fn row(ui: &mut egui::Ui, label: &str, value: &str) {
    ui.horizontal(|ui| {
        ui.label(label);
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.weak(value);
        });
    });
}

/// A full-width accent button.
fn prominent(ui: &mut egui::Ui, roles: &ColorRoles, label: &str, enabled: bool) -> egui::Response {
    let text = RichText::new(label).strong().color(egui::Color32::WHITE);
    let b = egui::Button::new(text)
        .fill(to_color32(roles.primary))
        .min_size(egui::vec2(ui.available_width(), 32.0));
    ui.add_enabled(enabled, b)
}

/// One picker row; `true` when clicked.
fn material_row(ui: &mut egui::Ui, title: &str, detail: &str, on: bool) -> bool {
    let r = egui::Frame::NONE
        .inner_margin(Margin::symmetric(Spacing::XS as i8, 2))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.style_mut().interaction.selectable_labels = false;
            ui.horizontal(|ui| {
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 1.0;
                    ui.label(title);
                    ui.label(RichText::new(detail).small().weak());
                });
                if on {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label("✔");
                    });
                }
            });
        })
        .response
        .interact(egui::Sense::click());
    if r.hovered() {
        ui.painter().rect_filled(
            r.rect,
            3.0,
            ui.visuals()
                .widgets
                .hovered
                .weak_bg_fill
                .gamma_multiply(0.4),
        );
    }
    r.clicked()
}

/// One FAIL/WARN row; click selects its markers.
fn finding_row(
    ui: &mut egui::Ui,
    roles: &ColorRoles,
    f: &CutFinding,
    focused: bool,
) -> egui::Response {
    let color = if f.is_fail() {
        to_color32(roles.destructive)
    } else {
        ui.visuals().warn_fg_color
    };
    let fill = if focused {
        let [r, g, b, _] = to_color32(roles.primary).to_array();
        egui::Color32::from_rgba_unmultiplied(r, g, b, 56)
    } else {
        egui::Color32::TRANSPARENT
    };
    let r = egui::Frame::NONE
        .fill(fill)
        .corner_radius(4.0)
        .inner_margin(Margin::symmetric(Spacing::XS as i8 + 2, 4))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            // The whole row is the click target, not its text.
            ui.style_mut().interaction.selectable_labels = false;
            ui.horizontal_top(|ui| {
                ui.label(RichText::new(if f.is_fail() { "✖" } else { "⚠" }).color(color));
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 2.0;
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(f.title()).strong());
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.label(
                                RichText::new(f.verdict.to_uppercase())
                                    .small()
                                    .strong()
                                    .color(color),
                            );
                        });
                    });
                    ui.label(f.value_text());
                    ui.add(egui::Label::new(RichText::new(&f.message).size(12.5).weak()).wrap());
                });
            });
        })
        .response
        .interact(egui::Sense::click());
    let r = if f.markers.is_empty() {
        r
    } else {
        r.on_hover_text(t("cut.finding.hint"))
    };
    if r.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    r
}

impl App {
    /// Open (or re-open) the panel on the current document, on `preset` or
    /// the document's last one (#209).
    pub(super) fn open_cut_export_with(&mut self, preset: Option<CutPreset>) {
        if let Some(p) = preset {
            self.session.set_cut_preset(p);
        }
        let preset = self.session.cut_preset().unwrap_or_default();
        self.cut_export =
            Some(CutExportPanel::new(&self.session.doc, scs::default_dir()).with_preset(preset));
    }

    pub(super) fn open_cut_export(&mut self) {
        self.open_cut_export_with(None);
    }

    /// Draw the panel and act on its request.
    pub(super) fn cut_export_ui(&mut self, ctx: &egui::Context) {
        let roles = self.live_roles(ctx.global_style().visuals.dark_mode);
        let Some(panel) = self.cut_export.as_mut() else {
            return;
        };
        let mut open = true;
        let action = panel.ui(ctx, &roles, &mut open);
        // The preset is document metadata: a switch dirties the document.
        if self.session.cut_preset().unwrap_or_default() != panel.form.preset {
            self.session.set_cut_preset(panel.form.preset);
        }
        match action {
            Some(PanelAction::Check) => panel.run_check(&mut self.command_line, &mut self.session),
            Some(PanelAction::Focus(ids)) => {
                if !ids.is_empty() {
                    self.session.doc.selection = ids.into_iter().collect();
                    self.zoom_selected();
                }
            }
            Some(PanelAction::Export) => {
                let path = if self.headless_no_dialog() {
                    None
                } else {
                    rfd::FileDialog::new()
                        .add_filter("DXF", &["dxf"])
                        .set_file_name("ItsJustCAD-cut.dxf")
                        .save_file()
                };
                if let Some(path) = path {
                    let path = if path.extension().is_none() {
                        path.with_extension("dxf")
                    } else {
                        path
                    };
                    if let Some(panel) = self.cut_export.as_mut() {
                        panel.export_to(&mut self.command_line, &mut self.session, &path);
                    }
                } else if let Some(panel) = self.cut_export.as_mut() {
                    panel.confirm_export = false;
                }
            }
            Some(PanelAction::UpdateData) => {
                let handle = self.tokio.clone();
                panel.start_update(move |url| handle.block_on(crate::download::fetch_bytes(url)));
            }
            Some(PanelAction::OpenSite) => crate::update::open_external_url(SITE_URL),
            None => {}
        }
        if !open {
            self.cut_export = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui_kittest::kittest::Queryable as _;

    fn run(s: &mut Session, line: &str) {
        s.run(itsjustcad_commands::parse(line).unwrap()).unwrap();
    }

    /// The acceptance plate from #153: 50 × 30 mm, a 5 mm hole, a text label.
    fn plate() -> Session {
        let mut s = Session::default();
        run(&mut s, "rect 0,0,0 0.05 0.03");
        run(&mut s, "circle 0.01,0.015,0 0.0025");
        run(&mut s, "text 0.02,0.01,0 0.004 PLATE");
        s
    }

    #[test]
    fn check_line_carries_units_scope_and_limits() {
        let mut f = CutForm {
            thickness: Some(3.0),
            ..CutForm::default()
        };
        assert_eq!(f.check_line(), "cutcheck units=mm thickness=3");
        f.units = CutUnit::In;
        f.thickness = Some(0.125);
        f.scope = CutScope::Selection;
        assert_eq!(f.check_line(), "cutcheck sel units=in thickness=0.125");
        // A SKU carries its own thickness.
        f.material = Some("AL5052-0.125".into());
        f.scope = CutScope::Layers;
        f.layers = vec!["parts".into(), "holes".into()];
        assert_eq!(
            f.check_line(),
            "cutcheck units=in material=AL5052-0.125 layers=parts,holes"
        );
        for line in [
            f.check_line(),
            CutForm {
                thickness: Some(1.5),
                ..f.clone()
            }
            .check_line(),
        ] {
            itsjustcad_commands::parse(&line).unwrap_or_else(|e| panic!("{line}: {e}"));
        }
    }

    #[test]
    fn generic_preset_sends_the_rule_of_thumb_only() {
        let mut f = CutForm {
            preset: CutPreset::Generic,
            material: Some("AL5052-0.125".into()),
            ..CutForm::default()
        };
        // A SKU picked under SendCutSend is never sent: thickness only.
        assert_eq!(f.blocker(), Some("cut.blocker.thickness"));
        f.thickness = Some(3.0);
        assert_eq!(f.blocker(), None);
        assert_eq!(f.check_line(), "cutcheck preset=generic units=mm thickness=3");
        itsjustcad_commands::parse(&f.check_line()).unwrap();
        f.preset = CutPreset::Scs;
        assert_eq!(f.check_line(), "cutcheck units=mm material=AL5052-0.125");
        // The export is the same DXF for both presets.
        assert_eq!(
            f.export_line(Path::new("x.dxf")),
            CutForm { preset: CutPreset::Generic, ..f.clone() }.export_line(Path::new("x.dxf"))
        );
    }

    /// Generic: no material picker, no SendCutSend data, no link after the
    /// export; the check reports the rule of thumb. SendCutSend is the default.
    #[test]
    fn generic_preset_hides_sendcutsend() {
        let mut s = plate();
        let mut cl = CommandLine::default();
        let p = CutExportPanel::new(&s.doc, None);
        assert_eq!(p.form.preset, CutPreset::Scs);
        assert_eq!(p.form.units, CutUnit::Mm);
        let mut p = p.with_preset(CutPreset::Generic);
        p.form.thickness = Some(3.0);
        p.run_check(&mut cl, &mut s);
        let r = p.result.clone().unwrap();
        assert!(r.limits_source().unwrap().starts_with("rule of thumb"), "{r:?}");
        p.exported = Some(PathBuf::from("/tmp/x.dxf"));
        let mut h = harness(p, false, false);
        h.run_steps(2);
        assert!(h.state().0.result.is_some(), "with_preset keeps the result");
        assert!(h.query_by_label(t("cut.data")).is_none());
        assert!(h.query_by_label(t("cut.material.none")).is_none(), "no picker");
        assert!(h.query_by_label(t("cut.open_scs")).is_none());
        assert!(h.query_by_label(t("cut.preset.footer.generic")).is_some());
        // Switching back shows SendCutSend's sections (and drops the result).
        click(&mut h, t("cut.preset.scs"));
        h.run_steps(2);
        assert_eq!(h.state().0.form.preset, CutPreset::Scs);
        assert!(h.state().0.result.is_none());
        assert!(h.query_by_label(t("cut.data")).is_some());
    }

    #[test]
    fn export_line_and_command() {
        let mut f = CutForm::default();
        let p = Path::new("/tmp/My Parts/plate.dxf");
        assert_eq!(
            f.export_line(p),
            "export /tmp/My Parts/plate.dxf profile=cut units=mm"
        );
        f.units = CutUnit::In;
        f.scope = CutScope::Layers;
        f.layers = vec!["a".into(), "b".into()];
        assert_eq!(
            f.export_line(Path::new("x.dxf")),
            "export x.dxf profile=cut units=in layers=a,b"
        );
        f.scope = CutScope::Selection;
        assert_eq!(
            f.export_line(Path::new("x.dxf")),
            "export x.dxf profile=cut units=in sel"
        );
        // The path with spaces survives as one argument.
        match f.export_command(p).unwrap() {
            Command::Export { path, cut: Some(o) } => {
                assert_eq!(path, "/tmp/My Parts/plate.dxf");
                assert_eq!(o.units, CutUnit::In);
                assert!(o.targets.is_some(), "sel");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn blockers() {
        let mut f = CutForm::default();
        assert_eq!(f.blocker(), Some("cut.blocker.material"));
        f.thickness = Some(0.0);
        assert_eq!(f.blocker(), Some("cut.blocker.material"));
        f.thickness = Some(2.0);
        assert_eq!(f.blocker(), None);
        f.scope = CutScope::Layers;
        assert_eq!(f.blocker(), Some("cut.blocker.layers"));
        assert!(!f.can_export());
        f.layers.push("a".into());
        assert!(f.can_export() && f.blocker().is_none());
        assert!(!CutForm::is_token_safe("my layer") && !CutForm::is_token_safe("a,b"));
        assert_eq!(fmt_num(1.5), "1.5");
        assert_eq!(fmt_num(3.0), "3");
        assert_eq!(fmt_num(0.12345), "0.123");
    }

    #[test]
    fn report_parses_into_findings_with_markers() {
        let report = ComplianceReport {
            pack: "cutcheck".into(),
            context: "units mm, thickness 3 mm; min hole 1.5, bridge 1.5, hole-to-edge 1.5 mm; part 12.7x12.7 to 1117.6x609.6 mm; limits: rule of thumb (x); NOTE: no data; NOTE: second".into(),
            rules: vec![
                rule("text", "warn", 1, None),
                rule("open-contour", "pass", 0, None),
                rule("min-hole", "fail", 2, Some((0.5, 1.5))),
            ],
        };
        let m: Vec<ObjectId> = (0..3).map(|_| ObjectId::new()).collect();
        let r = CutResult::from_report(&report, &m);
        assert_eq!((r.fail, r.warn), (1, 1));
        assert_eq!(r.findings[0].id, "min-hole", "FAIL rows first");
        assert_eq!(r.findings[0].markers, m[1..3]);
        assert_eq!(r.findings[1].markers, m[..1]);
        let v = r.findings[0].value_text();
        assert!(v.contains("0.5 mm") && v.contains("1.5 mm"), "{v}");
        assert!(r.findings[1].value_text().contains('1'));
        assert_eq!(r.limits_source(), Some("rule of thumb (x)"));
        assert_eq!(r.notes(), ["no data", "second"]);
        assert!(
            r.limits_summary()
                .unwrap()
                .contains("1.5, bridge 1.5, hole-to-edge 1.5 mm")
        );
        assert!(r.needs_confirm());
        // Fewer markers than objects: no panic, shorter slices.
        let r = CutResult::from_report(&report, &m[..1]);
        assert!(r.findings[0].markers.is_empty());
    }

    fn rule(
        id: &str,
        verdict: &str,
        n: usize,
        v: Option<(f64, f64)>,
    ) -> itsjustcad_doc::RuleOutcome {
        itsjustcad_doc::RuleOutcome {
            rule_id: id.into(),
            code_ref: String::new(),
            severity: "error".into(),
            verdict: verdict.into(),
            message: format!("{id} message"),
            objects: (0..n).map(|i| format!("o{i}")).collect(),
            locations: vec![[0.0; 3]; n],
            measured: v.map(|p| p.0),
            required: v.map(|p| p.1),
            unit: "mm".into(),
            checked: 1,
        }
    }

    #[test]
    fn check_and_export_the_acceptance_plate() {
        let mut s = plate();
        let mut cl = CommandLine::default();
        let mut p = CutExportPanel::new(&s.doc, None);
        p.form.thickness = Some(3.0);
        p.run_check(&mut cl, &mut s);
        let r = p.result.clone().unwrap();
        assert!(r.error.is_none(), "{r:?}");
        assert_eq!(r.fail, 0, "{r:?}");
        let text = r
            .findings
            .iter()
            .find(|f| f.id == "text")
            .expect("text WARN");
        assert_eq!(text.markers.len(), 1);
        assert!(s.doc.get(text.markers[0]).is_some(), "marker exists");
        assert!(!r.needs_confirm());
        // Re-checking replaces the markers instead of piling them up.
        let before = s.doc.objects().count();
        p.run_check(&mut cl, &mut s);
        assert_eq!(s.doc.objects().count(), before);

        let dir = std::env::temp_dir().join(format!("ijc-cut175-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.join("plate cut.dxf");
        p.export_to(&mut cl, &mut s, &out);
        assert_eq!(p.export_error, None);
        let dxf = std::fs::read_to_string(&out).unwrap();
        let lines: Vec<&str> = dxf.lines().map(str::trim).collect();
        let at = lines
            .iter()
            .position(|l| *l == "$INSUNITS")
            .expect("$INSUNITS");
        assert_eq!(lines[at + 2], "4", "mm");
        assert!(!dxf.contains("PLATE"), "text left out");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn check_error_is_shown() {
        let mut s = Session::default();
        let mut cl = CommandLine::default();
        let mut p = CutExportPanel::new(&s.doc, None);
        p.form.thickness = Some(3.0);
        p.run_check(&mut cl, &mut s);
        assert!(
            p.result
                .unwrap()
                .error
                .unwrap()
                .contains("nothing in scope")
        );
    }

    #[test]
    fn fail_rows_gate_the_export_behind_a_confirm() {
        let mut s = Session::default();
        run(&mut s, "rect 0,0,0 0.05 0.03");
        run(&mut s, "circle 0.01,0.015,0 0.0005"); // 1 mm hole < 1.5 mm
        let mut cl = CommandLine::default();
        let mut p = CutExportPanel::new(&s.doc, None);
        p.form.thickness = Some(3.0);
        p.run_check(&mut cl, &mut s);
        assert!(p.result.as_ref().unwrap().needs_confirm());
        let mut h = harness(p, false, false);
        h.run_steps(2);
        // Export asks first: the confirm shows, nothing exports yet.
        click(&mut h, t("cut.export"));
        h.run_steps(2);
        assert!(h.state().0.confirm_export);
        assert_eq!(h.state().1, None);
        click(&mut h, t("cut.confirm.export"));
        h.run_steps(2);
        assert_eq!(h.state().1, Some(PanelAction::Export));
        // A form change drops the stale result (and the confirm).
        h.state_mut().0.confirm_export = true;
        h.state_mut().0.form.units = CutUnit::In;
        h.run_steps(1);
        assert!(h.state().0.result.is_none() && !h.state().0.confirm_export);
    }

    #[test]
    fn clean_check_exports_without_confirm() {
        let mut s = Session::default();
        run(&mut s, "rect 0,0,0 0.05 0.03");
        let mut cl = CommandLine::default();
        let mut p = CutExportPanel::new(&s.doc, None);
        p.form.thickness = Some(3.0);
        p.run_check(&mut cl, &mut s);
        assert!(!p.result.as_ref().unwrap().needs_confirm());
        let mut h = harness(p, false, false);
        h.run_steps(2);
        click(&mut h, t("cut.export"));
        h.run_steps(2);
        assert_eq!(h.state().1, Some(PanelAction::Export));
        assert!(!h.state().0.confirm_export);
    }

    #[test]
    fn data_update_writes_both_feeds_or_nothing() {
        let dir = std::env::temp_dir().join(format!("ijc-cut175-data-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut seen = Vec::new();
        // A bad second feed: nothing is written.
        let mut bad = |url: &str| -> Result<Vec<u8>, String> {
            seen.push(url.to_string());
            Ok(if url.ends_with(scs::SPECS_FILE) {
                b"{}".to_vec()
            } else {
                b"[1]".to_vec()
            })
        };
        let err = update_data(&dir, &mut bad, &mut |_| {}).unwrap_err();
        assert!(err.contains(scs::CATALOG_FILE), "{err}");
        assert_eq!(
            seen,
            [
                "https://cdn.sendcutsend.com/specs/sendcutsend-specs.json",
                "https://cdn.sendcutsend.com/specs/sendcutsend-catalog.json"
            ]
        );
        assert!(!dir.join(scs::SPECS_FILE).exists());
        // A network error names the file.
        let err = update_data(&dir, &mut |_| Err("offline".into()), &mut |_| {}).unwrap_err();
        assert!(
            err.contains(scs::SPECS_FILE) && err.contains("offline"),
            "{err}"
        );
        // Both good: both written, progress heard per feed.
        let mut steps = Vec::new();
        let specs = r#"{"generated_at":"2026-10-01T00:00:00Z","materials":[]}"#;
        update_data(&dir, &mut |_| Ok(specs.as_bytes().to_vec()), &mut |i| {
            steps.push(i)
        })
        .unwrap();
        assert_eq!(steps, [0, 1]);
        assert!(dir.join(scs::SPECS_FILE).exists() && dir.join(scs::CATALOG_FILE).exists());

        // The panel's background update: injected fetch, then the data reloads.
        let doc = Document::default();
        let mut p = CutExportPanel::new(&doc, Some(dir.clone()));
        let fresh = p.data.is_some();
        p.start_update(move |_| Ok(specs.as_bytes().to_vec()));
        let t0 = std::time::Instant::now();
        while p.poll_update() {
            assert!(t0.elapsed().as_secs() < 10, "update never finished");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(p.update_error.is_none() && p.data.is_some() && fresh);
        std::fs::remove_dir_all(&dir).ok();
    }

    type State = (CutExportPanel, Option<PanelAction>);

    /// Scroll the labelled widget into view, then click it (via AccessKit, so
    /// a row still below the scroll fold counts).
    fn click(h: &mut egui_kittest::Harness<'_, State>, label: &str) {
        h.get_by_label(label).scroll_to_me();
        h.run_steps(2);
        h.get_by_label(label).click_accesskit();
    }

    /// The panel in a kittest harness; the last action lands in `.1`.
    fn harness<'a>(p: CutExportPanel, dark: bool, gpu: bool) -> egui_kittest::Harness<'a, State> {
        let accent = crate::preset::preset_for(Default::default())
            .tokens()
            .colors
            .primary;
        let roles = crate::theme::roles_for_mode(dark, accent);
        let mut b = egui_kittest::Harness::builder()
            .with_size(egui::vec2(520.0, 1100.0))
            .with_theme(if dark {
                egui::Theme::Dark
            } else {
                egui::Theme::Light
            });
        if gpu {
            b = b.wgpu().with_pixels_per_point(2.0);
        }
        b.build_ui_state(
            move |ui, s: &mut State| {
                if let Some(a) = s.0.ui(ui.ctx(), &roles, &mut true) {
                    s.1 = Some(a);
                }
            },
            (p, None),
        )
    }

    /// The panel renders (CPU kittest) before and after a check at its
    /// default width, and a finding row's click focuses its markers.
    #[test]
    fn panel_renders_in_kittest() {
        let mut s = plate();
        let mut cl = CommandLine::default();
        let mut p = CutExportPanel::new(&s.doc, None);
        p.form.thickness = Some(3.0);
        p.thickness_text = "3".into();
        let mut h = harness(p, false, false);
        h.run_steps(2);
        h.state_mut().0.run_check(&mut cl, &mut s);
        h.run_steps(2);
        let rect = h
            .ctx
            .memory(|m| m.area_rect(egui::Id::new(WINDOW_ID)))
            .expect("window");
        assert!(rect.width() >= WIDTH - 1.0, "{rect:?}");
        // Click the first finding row: its markers come back as a Focus.
        let title = h.state().0.result.as_ref().unwrap().findings[0].title();
        h.get_by_label(&title).scroll_to_me();
        h.run_steps(2);
        h.get_by_label(&title).click();
        h.run_steps(2);
        assert!(
            matches!(&h.state().1, Some(PanelAction::Focus(ids)) if ids.len() == 1),
            "{:?}",
            h.state().1
        );
    }

    /// Screenshots of the panel, cropped to the window, light + dark, before
    /// and after Check, with SendCutSend data (the fixture feeds), and on the
    /// generic preset after Check:
    ///
    ///   IJC_SHOT_DIR=/tmp/shots cargo test -p itsjustcad cut_export_shots -- --ignored
    #[test]
    #[ignore = "needs a GPU adapter; run explicitly to render the panel"]
    fn cut_export_shots() {
        let dir = PathBuf::from(
            std::env::var("IJC_SHOT_DIR").unwrap_or_else(|_| "tests/snapshots".into()),
        );
        std::fs::create_dir_all(&dir).unwrap();
        let data = std::env::temp_dir().join(format!("ijc-cut175-shots-{}", std::process::id()));
        std::fs::create_dir_all(&data).unwrap();
        let fixtures = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../commands/tests/fixtures/sendcutsend"
        );
        for f in [scs::SPECS_FILE, scs::CATALOG_FILE] {
            std::fs::copy(Path::new(fixtures).join(f), data.join(f)).unwrap();
        }
        let empty = std::env::temp_dir().join(format!("ijc-cut175-empty-{}", std::process::id()));
        for dark in [false, true] {
            for stage in ["before", "after", "data", "generic"] {
                let mut s = plate();
                run(&mut s, "circle 0.04,0.015,0 0.0005"); // a 1 mm hole: a FAIL row
                let mut cl = CommandLine::default();
                let src = if stage == "data" {
                    data.clone()
                } else {
                    empty.clone()
                };
                let mut p = CutExportPanel::new(&s.doc, Some(src));
                if stage == "generic" {
                    p = p.with_preset(CutPreset::Generic);
                }
                if stage == "data" {
                    p.materials_open = true;
                } else {
                    p.form.thickness = Some(3.0);
                    p.thickness_text = "3".into();
                }
                let mut h = harness(p, dark, true);
                h.run_steps(3);
                if stage == "after" || stage == "generic" {
                    h.state_mut().0.run_check(&mut cl, &mut s);
                    h.run_steps(2);
                    // Show a focused row (the click is covered by
                    // `panel_renders_in_kittest`).
                    let id = h.state().0.result.as_ref().unwrap().findings[0].id.clone();
                    h.state_mut().0.focused = Some(id);
                }
                h.run_steps(4);
                let r = h
                    .ctx
                    .memory(|m| m.area_rect(egui::Id::new(WINDOW_ID)))
                    .expect("window");
                let img = h.render().expect("render");
                let k = 2.0;
                let (x, y) = ((r.min.x * k).max(0.0) as u32, (r.min.y * k).max(0.0) as u32);
                let (w, hh) = ((r.width() * k) as u32, (r.height() * k) as u32);
                let w = w.min(img.width() - x);
                let hh = hh.min(img.height() - y);
                let theme = if dark { "dark" } else { "light" };
                image::imageops::crop_imm(&img, x, y, w, hh)
                    .to_image()
                    .save(dir.join(format!("cut209-{stage}-{theme}.png")))
                    .unwrap();
                std::mem::forget(h);
            }
        }
    }
}
