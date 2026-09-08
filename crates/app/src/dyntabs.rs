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

use itsjustcad_doc::{Document, Geometry};

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

/// One row of the Plugins tab: an installed user/LLM-authored macro.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginRow {
    pub name: String,
    /// `name <param> …` positional usage line.
    pub usage: String,
    pub summary: String,
    /// Pretty-printed JSON source (what `plugin define` took / disk holds).
    pub json: String,
}

/// Derive the ordered Plugins-tab rows from the live registry. Read-only.
pub fn plugin_rows(reg: &itsjustcad_commands::plugin::PluginRegistry) -> Vec<PluginRow> {
    reg.iter()
        .map(|p| PluginRow {
            name: p.name.clone(),
            usage: p.usage(),
            summary: p.summary(),
            json: serde_json::to_string_pretty(p).unwrap_or_else(|_| "{}".into()),
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
        // JSON source round-trips back to the same plugin.
        let back: Plugin = serde_json::from_str(&rows[0].json).unwrap();
        assert_eq!(back.name, "column-grid");
    }
}
