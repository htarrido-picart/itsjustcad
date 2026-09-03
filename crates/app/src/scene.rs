// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! Scene helpers for the app shell. The LLM `digest` now lives in the `deck`
//! crate so the desktop app and the iOS FFI shell feed the model an identical,
//! identically-sanitized scene description; it is re-exported here so existing
//! `crate::scene::digest(...)` call sites keep working unchanged.

pub use itsjustcad_deck::digest;
pub use itsjustcad_render::{snapshot_with_mode, SceneData, Theme};

use itsjustcad_doc::{Document, Geometry};

/// Default plan-symbol color (a muted planting green) when neither the object
/// nor its layer defines one.
const PLANT_SYMBOL_FALLBACK: [f32; 4] = [0.20, 0.55, 0.25, 1.0];

/// Append 2D top-view planting symbols to a snapshot's line soup, one polyline
/// per symbol segment. Each `plant:<id>` mesh contributes its plan drafting
/// glyph (see [`itsjustcad_commands::landscape::plant_object_symbol`]),
/// colored by the object/layer color so it reads on the planting layer. The 3D
/// canopy mesh in the snapshot is untouched — this only overlays the plan
/// representation, so callers gate it behind the `plantsymbols` toggle.
///
/// The symbols lie flat on the ground, so they read as a planting plan in the
/// top/plan view and as ground rings in perspective — the standard drafting
/// convention.
pub fn append_plant_symbols(scene: &mut SceneData, doc: &Document) {
    for obj in doc.objects() {
        if !obj.visible || !doc.layer_visible(&obj.layer) {
            continue;
        }
        let Some(name) = obj.name.as_deref() else { continue };
        let Geometry::Mesh(mesh) = &obj.geometry else { continue };
        let Some(segs) =
            itsjustcad_commands::landscape::plant_object_symbol(name, mesh.positions())
        else {
            continue;
        };
        let color = obj
            .color
            .map(|[r, g, b]| [r, g, b, 1.0])
            .or_else(|| doc.layers.get(&obj.layer).and_then(|s| s.color))
            .unwrap_or(PLANT_SYMBOL_FALLBACK);
        let lw = doc.effective_lineweight(obj) as f32;
        for (a, b) in segs {
            scene.lines.push((
                vec![
                    [a.x as f32, a.y as f32, a.z as f32],
                    [b.x as f32, b.y as f32, b.z as f32],
                ],
                color,
                lw,
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use itsjustcad_commands::{parse, Session};

    #[test]
    fn append_plant_symbols_adds_lines_for_planted_tree() {
        let mut s = Session::default();
        s.run(parse("plant oak 0,0").unwrap()).unwrap();
        let mut scene = snapshot_with_mode(&s.doc, Theme::Dark, Default::default());
        let before = scene.lines.len();
        append_plant_symbols(&mut scene, &s.doc);
        // Round symbol = 24-gon ring + 8 branch stubs = 32 polylines.
        assert_eq!(scene.lines.len() - before, 32, "oak plan symbol lines");
        // Each symbol polyline is a single segment (2 points).
        for pl in &scene.lines[before..] {
            assert_eq!(pl.0.len(), 2);
        }
    }

    #[test]
    fn append_plant_symbols_noop_without_plants() {
        let mut s = Session::default();
        s.run(parse("box 0,0,0 1,1,1").unwrap()).unwrap();
        let mut scene = snapshot_with_mode(&s.doc, Theme::Dark, Default::default());
        let before = scene.lines.len();
        append_plant_symbols(&mut scene, &s.doc);
        assert_eq!(scene.lines.len(), before, "no plants → no symbol lines");
    }
}
