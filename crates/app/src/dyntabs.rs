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
/// derived data, not definitions.
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
