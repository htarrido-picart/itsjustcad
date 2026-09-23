// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! Pure derivations for the DYNAMIC right-dock tabs (Blocks / Plugins).
//!
//! The tabs themselves are VIEW + verb-trigger surfaces only: everything they
//! display is derived read-only from the live document / plugin registry here,
//! and every action they offer fires a normal command line (`insert …`,
//! `blockdelete …`, `blockload …`, `plugin …`) through the one substrate path —
//! never a second mutation path, so op-log/undo/replay invariants hold
//! untouched. Keeping the derivations pure (no egui) makes them unit-testable
//! standalone, like `tabstrip`.

use itsjustcad_doc::{Document, Geometry, ObjectId, Sheet};

/// Prefix of per-instance baked dynamic-block entries in `Document::blocks`.
/// These are implementation detail (see `exec::param_bake_key`), NOT user
/// definitions, and must never surface in the Blocks tab.
const PARAM_BAKE_PREFIX: &str = "__param/";

/// One row of the Blocks tab: a user-facing block definition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockRow {
    pub name: String,
    /// Live instances referencing this definition (plain: `block` name match;
    /// dynamic: `source` match). Drives the delete-definition guard.
    pub instances: usize,
    /// Geometry snapshot count for plain blocks (0 for a pure pblock that has
    /// never baked).
    pub geometries: usize,
    /// Parametric signature `name=default …` for dynamic blocks, `None` for
    /// plain ones.
    pub param_signature: Option<String>,
}

/// True when the document has at least one user-facing block definition
/// (plain or parametric). Baked per-instance entries don't count — they are
/// derived data, not definitions. Retained as a pure predicate over the
/// definition set (exercised in tests); the Blocks tab itself is now
/// reveal-driven and scopes to *instanced* blocks via [`drawing_block_rows`].
#[allow(dead_code)] // part of the dyntabs derivation API; exercised in tests
pub fn has_block_defs(doc: &Document) -> bool {
    !doc.param_blocks.is_empty()
        || doc.blocks.keys().any(|k| !k.starts_with(PARAM_BAKE_PREFIX))
}

/// Derive the ordered Blocks-tab rows from the document: every plain block
/// definition plus every parametric definition, alphabetical, with live
/// instance counts. Read-only.
pub fn block_rows(doc: &Document) -> Vec<BlockRow> {
    use std::collections::BTreeMap;
    // name → (geometries, param signature)
    let mut defs: BTreeMap<&str, (usize, Option<String>)> = BTreeMap::new();
    for (name, geoms) in &doc.blocks {
        if name.starts_with(PARAM_BAKE_PREFIX) {
            continue;
        }
        defs.insert(name.as_str(), (geoms.len(), None));
    }
    for (name, def) in &doc.param_blocks {
        let sig = def
            .params
            .iter()
            .map(|p| format!("{}={}", p.name, p.default))
            .collect::<Vec<_>>()
            .join(" ");
        let entry = defs.entry(name.as_str()).or_insert((0, None));
        entry.1 = Some(sig);
    }
    // Count instances per definition in ONE pass over the scene.
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for obj in doc.objects() {
        if let Geometry::Instance { block, source, .. } = &obj.geometry {
            let def_name = source.as_deref().unwrap_or(block.as_str());
            *counts.entry(def_name).or_default() += 1;
        }
    }
    defs.into_iter()
        .map(|(name, (geometries, param_signature))| BlockRow {
            name: name.to_string(),
            instances: counts.get(name).copied().unwrap_or(0),
            geometries,
            param_signature,
        })
        .collect()
}

/// Scope [`block_rows`] to the blocks actually PRESENT on the drawing: only
/// definitions with at least one live instance in the document. This is what the
/// redesigned Blocks tab lists — "blocks in this drawing", not every starter or
/// library definition that merely exists as a def. A def with zero instances is
/// excluded (the user reaches those through the "+" library search instead).
pub fn drawing_block_rows(doc: &Document) -> Vec<BlockRow> {
    block_rows(doc)
        .into_iter()
        .filter(|r| r.instances > 0)
        .collect()
}

/// Case-insensitive substring filter over block rows by name. An empty (or
/// whitespace-only) query returns every row unchanged. Drives the search field
/// at the top of the Blocks tab. Pure, so it is unit-tested standalone.
pub fn filter_block_rows(rows: &[BlockRow], query: &str) -> Vec<BlockRow> {
    let q = query.trim().to_ascii_lowercase();
    if q.is_empty() {
        return rows.to_vec();
    }
    rows.iter()
        .filter(|r| r.name.to_ascii_lowercase().contains(&q))
        .cloned()
        .collect()
}

/// Case-insensitive substring filter over library block names (the `blocklib`
/// listing: starter symbols + `~/.config/itsjustcad/blocks/*.block.json`). An
/// empty query returns every name. Drives the "+" library search popup. Pure.
pub fn filter_library(names: &[String], query: &str) -> Vec<String> {
    let q = query.trim().to_ascii_lowercase();
    if q.is_empty() {
        return names.to_vec();
    }
    names
        .iter()
        .filter(|n| n.to_ascii_lowercase().contains(&q))
        .cloned()
        .collect()
}

/// Pure decision for the viewport double-click trigger: a double-click that hits
/// an object reveals the Blocks tab **iff** that object is a block instance
/// (`Geometry::Instance`). Any other geometry (or a miss) leaves the tab alone.
/// The caller passes the hit object's geometry; `None` means the double-click
/// hit empty space. Returns the block/definition name to scroll to when it fires.
pub fn double_click_reveals_blocks(hit: Option<&Geometry>) -> Option<String> {
    match hit {
        Some(Geometry::Instance { block, source, .. }) => {
            Some(source.as_deref().unwrap_or(block.as_str()).to_string())
        }
        _ => None,
    }
}

/// One card in the Plugins popup: a user/LLM-authored macro.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginRow {
    pub name: String,
    /// `name <param> …` positional usage line.
    pub usage: String,
    pub summary: String,
    /// Pretty-printed JSON source (what `plugin define` took / disk holds).
    pub json: String,
    /// True when the plugin is present in the registry / on disk (loaded). For
    /// ItsJustCAD every listed plugin is user/LLM-authored JSON in the plugins
    /// dir, so this is always `true` today — the field is carried on the card so
    /// an available-but-not-installed catalog (if one is ever added) can set it
    /// `false` without reshaping the popup.
    pub installed: bool,
}

/// Derive the ordered Plugins-popup rows from the live registry. Read-only.
/// Every registry plugin is installed (loaded from disk), so `installed` is set.
pub fn plugin_rows(reg: &itsjustcad_commands::plugin::PluginRegistry) -> Vec<PluginRow> {
    reg.iter()
        .map(|p| PluginRow {
            name: p.name.clone(),
            usage: p.usage(),
            summary: p.summary(),
            json: serde_json::to_string_pretty(p).unwrap_or_else(|_| "{}".into()),
            installed: true,
        })
        .collect()
}

/// Case-insensitive substring filter over plugin cards by name OR summary. An
/// empty (or whitespace-only) query returns every row unchanged. Drives the
/// search field at the top of the Plugins popup. Pure, so it is unit-tested
/// standalone.
pub fn filter_plugin_rows(rows: &[PluginRow], query: &str) -> Vec<PluginRow> {
    let q = query.trim().to_ascii_lowercase();
    if q.is_empty() {
        return rows.to_vec();
    }
    rows.iter()
        .filter(|r| {
            r.name.to_ascii_lowercase().contains(&q)
                || r.summary.to_ascii_lowercase().contains(&q)
        })
        .cloned()
        .collect()
}

// ── Sheets tab derivations ───────────────────────────────────────────────────
//
// The Sheets tab is a DYNAMIC right-dock tab: it appears when the document has
// ≥1 sheet (or when explicitly pinned open). Its body is a LIST — one row per
// sheet — each showing the sheet name, a paper/view descriptor, and a live
// mini-preview of the sheet's paper + viewport frames. The preview layout is a
// pure value (`SheetPreview`) so it is unit-testable independent of egui; the
// actual painting happens in the app from these rects.

/// Paper-space layout constants for the sheet preview. Mirrors the print
/// pipeline (`itsjustcad_commands::pdf`): a title strip along the bottom, an
/// even margin, and equal horizontal viewport slices separated by a gutter. Kept
/// in sync with `pdf.rs`; the `sheet_preview` invariants are asserted in tests.
const PREVIEW_MARGIN_MM: f64 = 10.0;
const PREVIEW_TITLE_MM: f64 = 12.0;
const PREVIEW_GUTTER_MM: f64 = 5.0;

/// An axis-aligned rectangle in paper millimeters, origin at the sheet's
/// lower-left (same convention as the PDF pipeline). `min` is the lower-left
/// corner, `max` the upper-right.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RectMm {
    pub min: [f64; 2],
    pub max: [f64; 2],
}

impl RectMm {
    /// Frame width in mm. Part of the `RectMm` API (used by preview-layout tests
    /// and any consumer reasoning about frame extents).
    #[allow(dead_code)]
    pub fn width(&self) -> f64 {
        self.max[0] - self.min[0]
    }
    /// Frame height in mm. See [`RectMm::width`].
    #[allow(dead_code)]
    pub fn height(&self) -> f64 {
        self.max[1] - self.min[1]
    }
}

/// A pure, GUI-independent layout of a sheet for the tab thumbnail: the paper
/// extents and each viewport's frame rectangle, all in paper millimeters. The
/// app maps these into an egui painter rect (flipping Y, since egui's origin is
/// top-left) to draw the mini-preview. Computed by [`sheet_preview`].
#[derive(Debug, Clone, PartialEq)]
pub struct SheetPreview {
    /// Full paper size (landscape) in mm: `[width, height]`.
    pub paper_mm: [f64; 2],
    /// One frame rectangle per viewport, laid out as equal horizontal slices
    /// above the title strip — same geometry the PDF renderer uses.
    pub viewports: Vec<RectMm>,
}

/// Compute the pure paper-space layout for a sheet's preview thumbnail: the
/// paper rectangle plus one frame per viewport. The viewports are equal
/// horizontal slices between the side margins, above the bottom title strip,
/// mirroring `pdf::sheet_pdf`. Read-only; no egui. A sheet with zero views
/// yields an empty `viewports` list (the thumbnail still shows the paper).
pub fn sheet_preview(sheet: &Sheet) -> SheetPreview {
    let (paper_w, paper_h) = sheet.paper.landscape_mm();
    let n = sheet.views.len();
    let mut viewports = Vec::with_capacity(n);
    if n > 0 {
        // Bottom title strip reserves MARGIN + TITLE; views fill the rest up to
        // the top margin. (The schedule table is intentionally ignored in the
        // thumbnail — it is a coarse "what's on the sheet" hint, not the PDF.)
        let y0 = PREVIEW_MARGIN_MM + PREVIEW_TITLE_MM;
        let y1 = paper_h - PREVIEW_MARGIN_MM;
        let area_w = paper_w - 2.0 * PREVIEW_MARGIN_MM;
        let view_w = (area_w - PREVIEW_GUTTER_MM * (n as f64 - 1.0)) / n as f64;
        for i in 0..n {
            let x0 = PREVIEW_MARGIN_MM + i as f64 * (view_w + PREVIEW_GUTTER_MM);
            viewports.push(RectMm {
                min: [x0, y0],
                max: [x0 + view_w, y1],
            });
        }
    }
    SheetPreview { paper_mm: [paper_w, paper_h], viewports }
}

/// A descriptor of a sheet's paper + view mix for the row subtitle, e.g.
/// `"a3 · 2 views"`. Pure, so the row text is unit-testable.
fn sheet_descriptor(sheet: &Sheet) -> String {
    let n = sheet.views.len();
    format!(
        "{} · {} view{}",
        sheet.paper.label(),
        n,
        if n == 1 { "" } else { "s" }
    )
}

/// One row of the Sheets tab: a sheet on this document.
#[derive(Debug, Clone, PartialEq)]
pub struct SheetRow {
    /// The sheet name (unique per document; the `print` verb keys off it).
    pub name: String,
    /// Number of viewports on the sheet.
    pub views: usize,
    /// Human-readable paper/view descriptor for the row subtitle.
    pub descriptor: String,
    /// The pure preview layout the app paints as a thumbnail.
    pub preview: SheetPreview,
}

/// True when the document has at least one sheet. Drives the dynamic-tab
/// appearance (`TabState::visible_tabs(_, has_sheets)`). Pure predicate.
pub fn has_sheets(doc: &Document) -> bool {
    !doc.sheets.is_empty()
}

/// Derive the ordered Sheets-tab rows from the document — one row per sheet, in
/// document order (the order sheets were created / stored). Read-only; every
/// action the tab offers routes through the normal `print` verb on the command
/// line, never a second mutation path.
pub fn sheet_rows(doc: &Document) -> Vec<SheetRow> {
    doc.sheets
        .iter()
        .map(|s| SheetRow {
            name: s.name.clone(),
            views: s.views.len(),
            descriptor: sheet_descriptor(s),
            preview: sheet_preview(s),
        })
        .collect()
}

// ── Parameters tab (M-parametric) ───────────────────────────────────────────

/// One row of the Parameters tab: a live parametric object in the document.
#[derive(Debug, Clone, PartialEq)]
pub struct ParametricRow {
    /// The object id (drives selection + `paramset` targeting).
    pub id: ObjectId,
    /// Short display id (matches the command-line / digest form).
    pub short_id: String,
    /// Optional user-assigned object name.
    pub name: Option<String>,
    /// The generator behind the object.
    pub generator: itsjustcad_doc::GeneratorKind,
    /// i18n label key for the generator kind (card title).
    pub generator_label_key: &'static str,
    /// Compact key-param summary for the card subtitle ("frequency=3, radius=5").
    pub summary: String,
    /// This object's current params (the editor reads/writes these).
    pub params: itsjustcad_doc::ParamMap,
}

/// True when the document has at least one parametric object. Drives the
/// dynamic-tab appearance (`TabState::visible_tabs(_, _, has_parametric)`).
/// Pure predicate.
pub fn has_parametric(doc: &Document) -> bool {
    doc.objects().any(|o| matches!(o.geometry, Geometry::Parametric { .. }))
}

/// Derive the ordered Parameters-tab rows from the document — one row per
/// parametric object, in document order. Read-only; every edit the tab offers
/// routes through the normal `paramset` verb, never a second mutation path.
pub fn parametric_rows(doc: &Document) -> Vec<ParametricRow> {
    doc.objects()
        .filter_map(|o| match &o.geometry {
            Geometry::Parametric { generator, params, .. } => Some(ParametricRow {
                id: o.id,
                short_id: o.id.short(),
                name: o.name.clone(),
                generator: *generator,
                generator_label_key: generator.label_key(),
                summary: itsjustcad_doc::param_summary(*generator, params),
                params: params.clone(),
            }),
            _ => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use itsjustcad_commands::{parse, Session};

    fn run(s: &mut Session, line: &str) {
        s.run(parse(line).unwrap()).unwrap();
    }

    fn row<'a>(rows: &'a [BlockRow], name: &str) -> &'a BlockRow {
        rows.iter().find(|r| r.name == name).unwrap_or_else(|| panic!("no row '{name}'"))
    }

    #[test]
    fn default_session_lists_starter_parametric_blocks() {
        // A fresh session seeds the starter pblock catalog (door, window, …),
        // so the Blocks tab has content from day one: every starter surfaces
        // as a parametric row with zero instances.
        let s = Session::default();
        assert!(has_block_defs(&s.doc));
        let rows = block_rows(&s.doc);
        assert_eq!(rows.len(), s.doc.param_blocks.len());
        assert!(rows.iter().all(|r| r.param_signature.is_some() && r.instances == 0));
    }

    #[test]
    fn truly_empty_document_has_no_block_defs() {
        // The pure predicate on a bare Document (no starter seeding).
        let doc = itsjustcad_doc::Document::default();
        assert!(!has_block_defs(&doc));
        assert!(block_rows(&doc).is_empty());
    }

    #[test]
    fn plain_block_definition_derives_a_row() {
        let mut s = Session::default();
        let before = block_rows(&s.doc).len();
        run(&mut s, "box 0,0,0 1,1,1");
        run(&mut s, "block last mydoor");
        let rows = block_rows(&s.doc);
        assert_eq!(rows.len(), before + 1);
        let r = row(&rows, "mydoor");
        assert_eq!(r.instances, 0);
        assert_eq!(r.geometries, 1);
        assert_eq!(r.param_signature, None);
    }

    #[test]
    fn instance_counts_track_plain_inserts() {
        let mut s = Session::default();
        run(&mut s, "box 0,0,0 1,1,1");
        run(&mut s, "block last mytree");
        run(&mut s, "insert mytree 5,0,0");
        run(&mut s, "insert mytree 10,0,0");
        let rows = block_rows(&s.doc);
        assert_eq!(row(&rows, "mytree").instances, 2);
    }

    #[test]
    fn parametric_block_derives_signature_and_counts_dynamic_instances() {
        let mut s = Session::default();
        run(&mut s, "pblock mypdoor width=0.9 h=2.0 : rect 0,0,0 {width} 0.05");
        run(&mut s, "insert mypdoor 0,0,0");
        let rows = block_rows(&s.doc);
        let r = row(&rows, "mypdoor");
        assert_eq!(r.param_signature.as_deref(), Some("width=0.9 h=2.0"));
        assert_eq!(r.instances, 1, "dynamic instance counted via source");
    }

    #[test]
    fn baked_param_entries_never_count_as_defs() {
        let mut s = Session::default();
        let before = block_rows(&s.doc).len();
        run(&mut s, "pblock mypwin w=0.5 : rect 0,0,0 {w} 0.05");
        run(&mut s, "insert mypwin 0,0,0");
        // doc.blocks now holds a "__param/mypwin/…" bake; only mypwin is a def.
        assert!(s.doc.blocks.keys().any(|k| k.starts_with("__param/")));
        let rows = block_rows(&s.doc);
        assert_eq!(rows.len(), before + 1, "the baked __param/ entry must NOT surface");
    }

    #[test]
    fn rows_are_alphabetical() {
        let mut s = Session::default();
        run(&mut s, "box 0,0,0 1,1,1");
        run(&mut s, "block last zzebra");
        run(&mut s, "box 0,0,0 1,1,1");
        run(&mut s, "block last aalpha");
        let names: Vec<_> = block_rows(&s.doc).into_iter().map(|r| r.name).collect();
        let mut sorted = names.clone();
        sorted.sort();
        assert_eq!(names, sorted, "rows must be alphabetical");
        assert_eq!(names.first().map(String::as_str), Some("aalpha"));
        assert_eq!(names.last().map(String::as_str), Some("zzebra"));
    }

    #[test]
    fn deleting_definition_clears_the_row() {
        let mut s = Session::default();
        run(&mut s, "box 0,0,0 1,1,1");
        run(&mut s, "block last mytemp");
        assert!(block_rows(&s.doc).iter().any(|r| r.name == "mytemp"));
        run(&mut s, "blockdelete mytemp");
        assert!(!block_rows(&s.doc).iter().any(|r| r.name == "mytemp"));
    }

    // ---- drawing-scoped rows (blocks PRESENT on the drawing) ----

    #[test]
    fn drawing_rows_exclude_defs_with_zero_instances() {
        // A captured definition with no instances is a def but is NOT "on the
        // drawing": drawing_block_rows drops it. The full block_rows still has it.
        let mut s = Session::default();
        run(&mut s, "box 0,0,0 1,1,1");
        run(&mut s, "block last uninstanced");
        assert!(block_rows(&s.doc).iter().any(|r| r.name == "uninstanced"));
        assert!(
            !drawing_block_rows(&s.doc).iter().any(|r| r.name == "uninstanced"),
            "a def with 0 instances is not a drawing block"
        );
    }

    #[test]
    fn drawing_rows_include_only_instanced_blocks() {
        let mut s = Session::default();
        run(&mut s, "box 0,0,0 1,1,1");
        run(&mut s, "block last placed");
        run(&mut s, "insert placed 5,0,0");
        run(&mut s, "box 0,0,0 1,1,1");
        run(&mut s, "block last shelf"); // never inserted
        let rows = drawing_block_rows(&s.doc);
        assert!(rows.iter().any(|r| r.name == "placed" && r.instances == 1));
        assert!(!rows.iter().any(|r| r.name == "shelf"));
    }

    #[test]
    fn drawing_rows_scope_parametric_by_instances() {
        // Starter pblocks exist as defs but have no instances → excluded until
        // one is placed.
        let mut s = Session::default();
        assert!(drawing_block_rows(&s.doc).is_empty(), "no instances yet");
        run(&mut s, "pblock mypdoor width=0.9 : rect 0,0,0 {width} 0.05");
        run(&mut s, "insert mypdoor 0,0,0");
        let rows = drawing_block_rows(&s.doc);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].name, "mypdoor");
        assert!(rows[0].param_signature.is_some());
    }

    // ---- search filter (by name substring, case-insensitive) ----

    #[test]
    fn filter_matches_case_insensitive_substring() {
        let rows = vec![
            BlockRow { name: "DoorSingle".into(), instances: 1, geometries: 0, param_signature: None },
            BlockRow { name: "window".into(), instances: 2, geometries: 0, param_signature: None },
            BlockRow { name: "tree".into(), instances: 1, geometries: 0, param_signature: None },
        ];
        let hit = filter_block_rows(&rows, "OO"); // matches "DoorSingle"
        assert_eq!(hit.len(), 1);
        assert_eq!(hit[0].name, "DoorSingle");
        assert_eq!(filter_block_rows(&rows, "w").len(), 1); // "window"
        assert_eq!(filter_block_rows(&rows, "zzz").len(), 0);
    }

    #[test]
    fn filter_empty_query_returns_all() {
        let rows = vec![
            BlockRow { name: "a".into(), instances: 1, geometries: 0, param_signature: None },
            BlockRow { name: "b".into(), instances: 1, geometries: 0, param_signature: None },
        ];
        assert_eq!(filter_block_rows(&rows, "").len(), 2);
        assert_eq!(filter_block_rows(&rows, "   ").len(), 2);
    }

    // ---- library search (derives from blocklib names) ----

    #[test]
    fn library_filter_matches_substring() {
        let names = vec![
            "door-single".to_string(),
            "window-double".to_string(),
            "tree".to_string(),
        ];
        assert_eq!(filter_library(&names, "door"), vec!["door-single".to_string()]);
        assert_eq!(filter_library(&names, "DOUBLE"), vec!["window-double".to_string()]);
        assert_eq!(filter_library(&names, ""), names);
        assert!(filter_library(&names, "nope").is_empty());
    }

    // ---- double-click → reveal Blocks tab (pure decision) ----

    #[test]
    fn double_click_on_instance_reveals_blocks() {
        let inst = Geometry::Instance {
            block: "door".into(),
            source: None,
            position: glam::DVec3::ZERO,
            scale: 1.0,
            rotation_deg: 0.0,
            params: Default::default(),
            clip: None,
        };
        assert_eq!(double_click_reveals_blocks(Some(&inst)).as_deref(), Some("door"));
    }

    #[test]
    fn double_click_on_dynamic_instance_uses_source_name() {
        let inst = Geometry::Instance {
            block: "__param/mypdoor/abc".into(),
            source: Some("mypdoor".into()),
            position: glam::DVec3::ZERO,
            scale: 1.0,
            rotation_deg: 0.0,
            params: Default::default(),
            clip: None,
        };
        assert_eq!(
            double_click_reveals_blocks(Some(&inst)).as_deref(),
            Some("mypdoor"),
            "dynamic instance keys off the source definition, not the baked block"
        );
    }

    #[test]
    fn double_click_on_non_instance_does_not_reveal() {
        let line = Geometry::Curve(kernel_curve::Curve::Line {
            a: glam::DVec3::ZERO,
            b: glam::DVec3::X,
        });
        assert!(double_click_reveals_blocks(Some(&line)).is_none());
        assert!(double_click_reveals_blocks(None).is_none(), "miss reveals nothing");
    }

    #[test]
    fn plugin_rows_derive_from_registry() {
        use itsjustcad_commands::plugin::{Plugin, PluginParam, PluginRegistry};
        let mut reg = PluginRegistry::new();
        assert!(plugin_rows(&reg).is_empty());
        reg.insert(Plugin {
            name: "column-grid".into(),
            description: "Place a grid of columns.".into(),
            category: None,
            params: vec![
                PluginParam { name: "nx".into(), default: None },
                PluginParam { name: "ny".into(), default: None },
            ],
            body: vec!["box 0,0,0 1,1,3".into()],
        });
        let rows = plugin_rows(&reg);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].name, "column-grid");
        assert!(rows[0].usage.contains("<nx>"), "usage: {}", rows[0].usage);
        assert!(rows[0].summary.contains("grid of columns"));
        // A registry plugin is installed (loaded from disk).
        assert!(rows[0].installed, "a registry plugin must be flagged installed");
        // JSON source round-trips back to the same plugin.
        let back: Plugin = serde_json::from_str(&rows[0].json).unwrap();
        assert_eq!(back.name, "column-grid");
    }

    #[test]
    fn plugin_filter_matches_name_or_summary_case_insensitive() {
        let rows = vec![
            PluginRow {
                name: "column-grid".into(),
                usage: "column-grid <nx>".into(),
                summary: "Place a grid of columns.".into(),
                json: "{}".into(),
                installed: true,
            },
            PluginRow {
                name: "stair".into(),
                usage: "stair <n>".into(),
                summary: "A run of treads.".into(),
                json: "{}".into(),
                installed: true,
            },
        ];
        // Matches on name.
        assert_eq!(filter_plugin_rows(&rows, "COLUMN").len(), 1);
        assert_eq!(filter_plugin_rows(&rows, "column")[0].name, "column-grid");
        // Matches on summary substring.
        let by_summary = filter_plugin_rows(&rows, "treads");
        assert_eq!(by_summary.len(), 1);
        assert_eq!(by_summary[0].name, "stair");
        // Empty / whitespace → all rows.
        assert_eq!(filter_plugin_rows(&rows, "").len(), 2);
        assert_eq!(filter_plugin_rows(&rows, "   ").len(), 2);
        // No match.
        assert!(filter_plugin_rows(&rows, "zzz").is_empty());
    }

    // ---- Sheets tab derivations ----

    #[test]
    fn empty_document_has_no_sheets() {
        let s = Session::default();
        assert!(!has_sheets(&s.doc));
        assert!(sheet_rows(&s.doc).is_empty());
    }

    // ── Parameters tab ──────────────────────────────────────────────────────

    #[test]
    fn empty_document_has_no_parametric() {
        let s = Session::default();
        assert!(!has_parametric(&s.doc));
        assert!(parametric_rows(&s.doc).is_empty());
    }

    #[test]
    fn creating_a_generator_makes_the_parameters_tab_appear() {
        let mut s = Session::default();
        run(&mut s, "geodesic 3 5 dome");
        assert!(has_parametric(&s.doc), "a parametric object must reveal the tab");
        let rows = parametric_rows(&s.doc);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].generator, itsjustcad_doc::GeneratorKind::Geodesic);
        assert!(rows[0].summary.contains("frequency"));
    }

    #[test]
    fn a_plain_mesh_does_not_reveal_the_parameters_tab() {
        let mut s = Session::default();
        run(&mut s, "box 0,0,0 1,1,1");
        assert!(!has_parametric(&s.doc));
        assert!(parametric_rows(&s.doc).is_empty());
    }

    #[test]
    fn parametric_rows_list_each_object_in_order() {
        let mut s = Session::default();
        run(&mut s, "geodesic 3 5 dome");
        run(&mut s, "hypar 5 5 5 6 6");
        let rows = parametric_rows(&s.doc);
        let kinds: Vec<_> = rows.iter().map(|r| r.generator).collect();
        assert_eq!(
            kinds,
            vec![
                itsjustcad_doc::GeneratorKind::Geodesic,
                itsjustcad_doc::GeneratorKind::Hypar
            ],
            "document order preserved"
        );
    }

    #[test]
    fn creating_a_sheet_makes_the_tab_appear() {
        let mut s = Session::default();
        run(&mut s, "sheet plan a3");
        assert!(has_sheets(&s.doc), "a created sheet must reveal the tab");
        let rows = sheet_rows(&s.doc);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].name, "plan");
        assert_eq!(rows[0].views, 0);
    }

    #[test]
    fn sheet_rows_list_each_sheet_in_order() {
        let mut s = Session::default();
        run(&mut s, "sheet plan a3");
        run(&mut s, "sheet elevations a2");
        let rows = sheet_rows(&s.doc);
        let names: Vec<_> = rows.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, vec!["plan", "elevations"], "document order preserved");
    }

    #[test]
    fn sheet_row_descriptor_reports_paper_and_view_count() {
        let mut s = Session::default();
        run(&mut s, "sheet plan a3");
        run(&mut s, "sheetview plan top 100");
        run(&mut s, "sheetview plan front 100");
        let rows = sheet_rows(&s.doc);
        let r = &rows[0];
        assert_eq!(r.views, 2);
        assert_eq!(r.descriptor, "a3 · 2 views");
    }

    #[test]
    fn sheet_row_descriptor_singular_view() {
        let mut s = Session::default();
        run(&mut s, "sheet plan a4");
        run(&mut s, "sheetview plan top 50");
        assert_eq!(sheet_rows(&s.doc)[0].descriptor, "a4 · 1 view");
    }

    // ---- pure preview layout (paper rect + viewport rects) ----

    #[test]
    fn preview_paper_matches_landscape_size() {
        let sheet = Sheet {
            name: "s".into(),
            paper: itsjustcad_doc::PaperSize::A3,
            views: vec![],
            table: None,
            dims: vec![],
            texts: vec![],
            leaders: vec![],
            tags: vec![],
        };
        let p = sheet_preview(&sheet);
        assert_eq!(p.paper_mm, [420.0, 297.0], "A3 landscape");
        assert!(p.viewports.is_empty(), "no views → no frames, paper still drawn");
    }

    #[test]
    fn preview_lays_out_one_frame_per_view() {
        let sheet = Sheet {
            name: "s".into(),
            paper: itsjustcad_doc::PaperSize::A3,
            views: vec![
                itsjustcad_doc::SheetView { direction: itsjustcad_doc::ViewDirection::Top, scale: 100.0 },
                itsjustcad_doc::SheetView { direction: itsjustcad_doc::ViewDirection::Front, scale: 100.0 },
                itsjustcad_doc::SheetView { direction: itsjustcad_doc::ViewDirection::Right, scale: 100.0 },
            ],
            table: None,
            dims: vec![],
            texts: vec![],
            leaders: vec![],
            tags: vec![],
        };
        let p = sheet_preview(&sheet);
        assert_eq!(p.viewports.len(), 3, "one frame per viewport");
        // Frames are equal-width horizontal slices, all inside the paper margins,
        // and left-to-right in order.
        let w0 = p.viewports[0].width();
        for r in &p.viewports {
            assert!((r.width() - w0).abs() < 1e-9, "equal-width slices");
            assert!(r.min[0] >= PREVIEW_MARGIN_MM - 1e-9, "inside left margin");
            assert!(r.max[0] <= p.paper_mm[0] - PREVIEW_MARGIN_MM + 1e-9, "inside right margin");
            assert!(r.min[1] >= PREVIEW_MARGIN_MM + PREVIEW_TITLE_MM - 1e-9, "above title strip");
            assert!(r.max[1] <= p.paper_mm[1] - PREVIEW_MARGIN_MM + 1e-9, "below top margin");
        }
        assert!(p.viewports[0].min[0] < p.viewports[1].min[0]);
        assert!(p.viewports[1].min[0] < p.viewports[2].min[0]);
        // A gutter separates adjacent frames.
        let gap = p.viewports[1].min[0] - p.viewports[0].max[0];
        assert!((gap - PREVIEW_GUTTER_MM).abs() < 1e-9, "gutter between frames");
    }

    #[test]
    fn preview_matches_derived_row() {
        // The row carries the same preview `sheet_preview` computes directly.
        let mut s = Session::default();
        run(&mut s, "sheet plan a3");
        run(&mut s, "sheetview plan top 100");
        let rows = sheet_rows(&s.doc);
        let direct = sheet_preview(&s.doc.sheets[0]);
        assert_eq!(rows[0].preview, direct);
        assert_eq!(rows[0].preview.viewports.len(), 1);
    }
}
