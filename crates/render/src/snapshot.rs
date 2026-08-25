// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

use glam::{DVec2, DVec3};
use itsjustcad_doc::{
    hatch::{hatch_brick, hatch_concrete, hatch_earth, hatch_insulation, hatch_lines},
    Annotation, Document, Geometry, HatchPattern, SceneObject,
};

use crate::renderer::{hue_from_seed, ColorMode, SceneData};
use crate::sketchy::{sketchify_segments, SketchyParams};

/// Parameters for color resolution that accompany a snapshot call.
#[derive(Clone, Copy, Debug, Default)]
pub struct ColorModeSnapshot {
    pub color_mode: ColorMode,
    /// When true, mesh feature edges are classified into profile (silhouette /
    /// sharp crease) vs interior, and profile edges are emitted as thick ribbon
    /// outlines (`SceneData::profile_edges`) — the SketchUp "objects have
    /// lineweight" look. Interior edges stay as thin lines. Off by default so
    /// existing render paths are unchanged.
    pub profile_edges: bool,
    /// Hand-drawn "sketchy edges" character (NPR stage 2): jitter overdraw,
    /// endpoint overshoot, thick endpoints and depth cue applied to the feature
    /// edge geometry. Inactive by default (identity transform).
    pub sketchy: SketchyParams,
    /// Camera eye + scene radius for the sketchy depth cue. `None` skips the
    /// depth term (uniform sketchy amount).
    pub sketchy_eye: Option<glam::Vec3>,
    pub sketchy_radius: f32,
}

impl ColorModeSnapshot {
    /// Apply the sketchy transform to a feature-edge segment soup, if active.
    fn sketchify(&self, segments: Vec<[f32; 3]>) -> Vec<[f32; 3]> {
        if self.sketchy.active() {
            sketchify_segments(&segments, self.sketchy, self.sketchy_eye, self.sketchy_radius)
        } else {
            segments
        }
    }
}

/// Dark "ink" color for SketchUp-style profile edges — a near-black charcoal
/// that reads as a bold outline on both dark viewports and the light sky/ground
/// gradient background.
const PROFILE_INK: [f32; 4] = [0.08, 0.08, 0.10, 1.0];

/// Ribbon half-width (world units) for profile edges. ~2 cm at building scale
/// reads as a bold outline against the thin (1-pixel) interior edges without
/// swamping small detail.
const PROFILE_HALF_WIDTH: f32 = 0.02;

/// Dihedral threshold: faces meeting at a sharper angle than this are a
/// form-defining crease → profile edge. cos(30°) ≈ 0.866; normals whose dot is
/// below this (angle > 30°) count as sharp. Boundary edges are always profile.
const PROFILE_CREASE_COS: f64 = 0.866;

/// Classify a mesh's feature edges into (profile, interior) segment lists.
/// Mirrors `kernel_mesh::feature_edges` but keeps the adjacency so we can tell a
/// silhouette/crease edge (thick) from a soft interior edge (thin). An edge is
/// PROFILE when it is a boundary edge (belongs to one face) or the two adjacent
/// faces meet at a sharp dihedral angle; otherwise it is INTERIOR.
fn classify_edges(mesh: &kernel_mesh::Mesh) -> (Vec<[f32; 3]>, Vec<[f32; 3]>) {
    use std::collections::BTreeMap;
    let pos = mesh.positions();
    let mut edges: BTreeMap<(u32, u32), Vec<DVec3>> = BTreeMap::new();
    for face in mesh.faces() {
        let [a, b, c] = face.map(|i| pos[i as usize]);
        let n = (b - a).cross(c - a).normalize_or_zero();
        for (i, j) in [(face[0], face[1]), (face[1], face[2]), (face[2], face[0])] {
            edges.entry((i.min(j), i.max(j))).or_default().push(n);
        }
    }
    let mut profile = Vec::new();
    let mut interior = Vec::new();
    for ((i, j), normals) in edges {
        // Coplanar interior diagonal (2 faces, same normal): not a feature edge
        // at all — matches feature_edges, skip entirely.
        let coplanar = normals.len() == 2 && normals[0].dot(normals[1]) > 1.0 - 1e-9;
        if coplanar {
            continue;
        }
        let a = pos[i as usize];
        let b = pos[j as usize];
        let seg = [
            [a.x as f32, a.y as f32, a.z as f32],
            [b.x as f32, b.y as f32, b.z as f32],
        ];
        let is_profile = normals.len() != 2 // boundary / non-manifold edge
            || normals[0].dot(normals[1]) < PROFILE_CREASE_COS; // sharp crease
        if is_profile {
            profile.extend_from_slice(&seg);
        } else {
            interior.extend_from_slice(&seg);
        }
    }
    (profile, interior)
}

/// Chord tolerance for display tessellation of curves.
const DISPLAY_TOL: f64 = 0.005;

/// Drafting palette tuned per background so geometry always reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Theme {
    Dark,
    Light,
}

impl Theme {
    /// Viewport clear color (the app returns this from `eframe::App::clear_color`).
    /// Dark mode is a near-black off-grey so the 3D model space reads as black
    /// (Rhino/AutoCAD dark-theme convention), not a washed-out mid-grey.
    pub fn background(self) -> [f32; 4] {
        match self {
            Theme::Dark => [0.05, 0.05, 0.06, 1.0],
            Theme::Light => [0.83, 0.85, 0.87, 1.0],
        }
    }

    pub fn mesh(self) -> [f32; 4] {
        match self {
            Theme::Dark => [0.72, 0.73, 0.78, 1.0],
            Theme::Light => [0.60, 0.62, 0.66, 1.0],
        }
    }

    pub fn curve(self) -> [f32; 4] {
        match self {
            Theme::Dark => [0.92, 0.94, 0.97, 1.0],
            Theme::Light => [0.10, 0.11, 0.14, 1.0],
        }
    }

    /// Amber selection reads on both backgrounds (yellow dies on light).
    pub fn selected(self) -> [f32; 4] {
        match self {
            Theme::Dark => [0.98, 0.75, 0.10, 1.0],
            Theme::Light => [0.90, 0.50, 0.05, 1.0],
        }
    }
}

/// A scene line entry: (points, rgba, lineweight_mm).
type SceneLine = (Vec<[f32; 3]>, [f32; 4], f32);

/// Push segment pairs as 2-point line strips into the scene lines list.
/// `lw_mm` is the effective lineweight in mm for the owning object.
fn push_hatch_segs(lines: &mut Vec<SceneLine>, segs: Vec<[DVec3; 2]>, color: [f32; 4], lw_mm: f32) {
    for [a, b] in segs {
        lines.push((
            vec![
                [a.x as f32, a.y as f32, a.z as f32],
                [b.x as f32, b.y as f32, b.z as f32],
            ],
            color,
            lw_mm,
        ));
    }
}

/// Resolve the display color for an object given the active color mode.
/// Selection always wins over everything; otherwise the mode determines priority.
fn resolve_color(
    obj: &SceneObject,
    layer_color: Option<[f32; 4]>,
    theme: Theme,
    selected: bool,
    mode: ColorMode,
    is_mesh: bool,
) -> [f32; 4] {
    if selected {
        return theme.selected();
    }
    match mode {
        // ByLayer: object color beats layer color beats theme default.
        // The `color` command stores object.color; it always wins over layer.
        ColorMode::ByLayer => {
            obj.color
                .map(|[r, g, b]| [r, g, b, 1.0])
                .or(layer_color)
                .unwrap_or(if is_mesh { theme.mesh() } else { theme.curve() })
        }
        // ByObject: identical priority to ByLayer; name signals intent — the
        // user wants per-object colors front and center.
        ColorMode::ByObject => {
            obj.color
                .map(|[r, g, b]| [r, g, b, 1.0])
                .or(layer_color)
                .unwrap_or(if is_mesh { theme.mesh() } else { theme.curve() })
        }
        ColorMode::ByType => {
            if is_mesh {
                [0.35, 0.75, 0.72, 1.0]
            } else {
                match &obj.geometry {
                    Geometry::Annotation(_) => [0.92, 0.70, 0.20, 1.0],
                    _ => if theme == Theme::Dark { [0.92, 0.94, 0.97, 1.0] } else { [0.10, 0.11, 0.14, 1.0] },
                }
            }
        }
        ColorMode::Random => {
            // Stable hash of the ObjectId (UUID bytes).
            let bytes = obj.id.0.as_bytes();
            let seed = u64::from_le_bytes(bytes[..8].try_into().unwrap());
            hue_from_seed(seed)
        }
    }
}

/// Default material scalars for a mesh with no `material2`: mid-roughness
/// dielectric. Matches the shader's neutral appearance for legacy objects.
const DEFAULT_ROUGH_METAL: [f32; 2] = [0.5, 0.0];

/// Resolve the mesh fill color AND its roughness/metallic for the shader. A
/// `material2` on the object overrides the base color (unless selection wins)
/// and supplies the PBR scalars; otherwise we fall back to the flat color path
/// with default scalars.
fn resolve_mesh_material(
    obj: &SceneObject,
    layer_color: Option<[f32; 4]>,
    theme: Theme,
    selected: bool,
    mode: ColorMode,
) -> ([f32; 4], [f32; 2]) {
    let flat = resolve_color(obj, layer_color, theme, selected, mode, true);
    match &obj.material {
        Some(m) if !selected => {
            let ([r, g, b], rough, metal) = m.pbr();
            ([r, g, b, flat[3]], [rough, metal])
        }
        _ => (flat, DEFAULT_ROUGH_METAL),
    }
}

/// Snapshot the document into GPU-ready buffers.
pub fn snapshot(doc: &Document, theme: Theme) -> SceneData {
    snapshot_with_mode(doc, theme, ColorModeSnapshot::default())
}

/// Snapshot with an explicit color mode.
pub fn snapshot_with_mode(doc: &Document, theme: Theme, cms: ColorModeSnapshot) -> SceneData {
    let mode = cms.color_mode;
    let mut scene = SceneData {
        meshes: Vec::new(),
        lines: Vec::new(),
        edges: Vec::new(),
        profile_edges: Vec::new(),
        points: Vec::new(),
        // The underlay image is decoded app-side (the app owns the `image`
        // dependency) and attached to the returned scene.
        underlay: None,
        // The basemap is transient session state (doc.basemap); the app attaches
        // its already-decoded pixels to the scene, same as the underlay.
        basemap: None,
        show_lineweights: doc.show_lineweights,
    };
    for obj in doc.objects() {
        if !obj.visible {
            continue; // hidden object (hideobj)
        }
        let style = doc.layers.get(&obj.layer);
        if style.is_some_and(|s| !s.visible) {
            continue; // hidden layer
        }
        let layer_color = style.and_then(|s| s.color);
        let selected = doc.selection.contains(&obj.id);
        let lw_mm = doc.effective_lineweight(obj) as f32;
        match &obj.geometry {
            // Frame/area structural members carry a derived mesh; render them
            // exactly like a solid mesh.
            Geometry::Mesh(mesh)
            | Geometry::Frame { mesh, .. }
            | Geometry::Area { mesh, .. } => {
                let (color, rm) =
                    resolve_mesh_material(obj, layer_color, theme, selected, mode);
                scene.meshes.push((mesh.to_render(), color, rm));
                // Feature edges for the wireframe/x-ray/ghosted display modes.
                let edge_color = if selected { theme.selected() } else { theme.curve() };
                if cms.profile_edges {
                    // SketchUp look: thin interior edges + thick profile ribbons.
                    // Profile ribbons are drawn in dark "ink" (unless selected)
                    // so they read as bold outlines on the light gradient
                    // background — theme.curve() would be near-white and vanish.
                    let (profile, interior) = classify_edges(mesh);
                    scene.edges.push((cms.sketchify(interior), edge_color, lw_mm));
                    if !profile.is_empty() {
                        let ink = if selected { theme.selected() } else { PROFILE_INK };
                        scene.profile_edges.push((
                            cms.sketchify(profile),
                            ink,
                            PROFILE_HALF_WIDTH,
                        ));
                    }
                } else {
                    let segments: Vec<[f32; 3]> = kernel_mesh::feature_edges(mesh)
                        .iter()
                        .flat_map(|(a, b)| {
                            [
                                [a.x as f32, a.y as f32, a.z as f32],
                                [b.x as f32, b.y as f32, b.z as f32],
                            ]
                        })
                        .collect();
                    scene.edges.push((cms.sketchify(segments), edge_color, lw_mm));
                }
            }
            Geometry::Curve(curve) => {
                let color = resolve_color(obj, layer_color, theme, selected, mode, false);
                let mut pts: Vec<[f32; 3]> = curve
                    .tessellate(DISPLAY_TOL)
                    .iter()
                    .map(|p| [p.x as f32, p.y as f32, p.z as f32])
                    .collect();
                if curve.is_closed()
                    && let Some(first) = pts.first().copied()
                {
                    pts.push(first); // close the strip
                }
                scene.lines.push((pts, color, lw_mm));
            }
            // Hatches are scene geometry (fill triangles / pattern lines);
            // dimensions and text are drawn as an egui overlay by the app.
            Geometry::Points { positions } => {
                let color = resolve_color(obj, layer_color, theme, selected, mode, false);
                let pts: Vec<[f32; 3]> = positions
                    .iter()
                    .map(|p| [p.x as f32, p.y as f32, p.z as f32])
                    .collect();
                scene.points.push((pts, color));
            }
            Geometry::Annotation(Annotation::Hatch { boundary, pattern }) => {
                let color = resolve_color(obj, layer_color, theme, selected, mode, false);
                match pattern {
                    HatchPattern::Solid => {
                        let pts2: Vec<DVec2> =
                            boundary.iter().map(|p| p.truncate()).collect();
                        let faces = kernel_mesh::earcut(&pts2);
                        if !faces.is_empty() {
                            let mesh = kernel_mesh::Mesh::new(boundary.clone(), faces);
                            scene.meshes.push((mesh.to_render(), color, DEFAULT_ROUGH_METAL));
                        }
                    }
                    HatchPattern::Lines { angle_deg, spacing } => {
                        push_hatch_segs(&mut scene.lines, hatch_lines(boundary, *angle_deg, *spacing), color, lw_mm);
                    }
                    HatchPattern::Crosshatch { angle_deg, spacing } => {
                        push_hatch_segs(&mut scene.lines, hatch_lines(boundary, *angle_deg, *spacing), color, lw_mm);
                        push_hatch_segs(&mut scene.lines, hatch_lines(boundary, *angle_deg + 90.0, *spacing), color, lw_mm);
                    }
                    HatchPattern::Brick { spacing } => {
                        push_hatch_segs(&mut scene.lines, hatch_brick(boundary, *spacing), color, lw_mm);
                    }
                    HatchPattern::Concrete { spacing } => {
                        push_hatch_segs(&mut scene.lines, hatch_concrete(boundary, *spacing), color, lw_mm);
                    }
                    HatchPattern::Insulation { spacing } => {
                        push_hatch_segs(&mut scene.lines, hatch_insulation(boundary, *spacing), color, lw_mm);
                    }
                    HatchPattern::Earth { spacing } => {
                        push_hatch_segs(&mut scene.lines, hatch_earth(boundary, *spacing), color, lw_mm);
                    }
                }
            }
            Geometry::Annotation(Annotation::Text { pos, text, height }) => {
                let color = resolve_color(obj, layer_color, theme, selected, mode, false);
                let strokes = itsjustcad_doc::hershey::text_strokes(
                    text,
                    [pos.x, pos.y],
                    *height,
                );
                for poly in strokes {
                    let pts: Vec<[f32; 3]> = poly
                        .iter()
                        .map(|p| [p[0] as f32, p[1] as f32, pos.z as f32])
                        .collect();
                    if pts.len() >= 2 {
                        scene.lines.push((pts, color, lw_mm));
                    }
                }
            }
            Geometry::Annotation(_) => {}
            // Block instances: resolved to constituent geometry at render time.
            Geometry::Instance { block, position, rotation_deg, scale, .. } => {
                if let Some(defs) = doc.blocks.get(block) {
                    let s = *scale;
                    let rot = rotation_deg.to_radians();
                    let (sin_r, cos_r) = rot.sin_cos();
                    let transform = |p: DVec3| -> DVec3 {
                        // Scale, rotate about Z, then translate.
                        let ps = p * s;
                        DVec3::new(
                            ps.x * cos_r - ps.y * sin_r + position.x,
                            ps.x * sin_r + ps.y * cos_r + position.y,
                            ps.z * s + position.z,
                        )
                    };
                    let color = resolve_color(obj, layer_color, theme, selected, mode, false);
                    for def_geo in defs {
                        match def_geo {
                            itsjustcad_doc::BlockGeometry::Mesh(m) => {
                                // Transform positions and build a new mesh.
                                let new_pos: Vec<DVec3> =
                                    m.positions().iter().map(|&p| transform(p)).collect();
                                let new_mesh = kernel_mesh::Mesh::new(new_pos, m.faces().to_vec());
                                let mesh_color = resolve_color(obj, layer_color, theme, selected, mode, true);
                                scene.meshes.push((new_mesh.to_render(), mesh_color, DEFAULT_ROUGH_METAL));
                                let segments: Vec<[f32; 3]> = kernel_mesh::feature_edges(&new_mesh)
                                    .iter()
                                    .flat_map(|(a, b)| {
                                        [
                                            [a.x as f32, a.y as f32, a.z as f32],
                                            [b.x as f32, b.y as f32, b.z as f32],
                                        ]
                                    })
                                    .collect();
                                scene.edges.push((segments, color, lw_mm));
                            }
                            itsjustcad_doc::BlockGeometry::Curve(c) => {
                                let mut pts: Vec<[f32; 3]> = c
                                    .tessellate(DISPLAY_TOL)
                                    .iter()
                                    .map(|&p| {
                                        let tp = transform(p);
                                        [tp.x as f32, tp.y as f32, tp.z as f32]
                                    })
                                    .collect();
                                if c.is_closed()
                                    && let Some(first) = pts.first().copied()
                                {
                                    pts.push(first);
                                }
                                scene.lines.push((pts, color, lw_mm));
                            }
                            itsjustcad_doc::BlockGeometry::Annotation(_) => {
                                // Annotations in block definitions are not rendered in
                                // the viewport (no egui overlay bridge for instances).
                            }
                        }
                    }
                }
            }
        }
    }
    scene
}

// The hatch tessellation functions live in itsjustcad_doc::hatch and are
// imported at the top of this file. Re-export hatch_lines so existing
// test references to `hatch_segments` work without renaming.
#[allow(unused_imports)]
pub use itsjustcad_doc::hatch::hatch_lines as hatch_segments;

#[cfg(test)]
mod tests {
    use glam::DVec3;
    use itsjustcad_doc::{Document, LayerStyle, ObjectId, SceneObject};

    use super::*;

    fn insert_tri(doc: &mut Document, layer: &str) -> ObjectId {
        let mesh = kernel_mesh::Mesh::new(
            vec![DVec3::ZERO, DVec3::X, DVec3::Y],
            vec![[0, 1, 2]],
        );
        let obj = SceneObject {
            visible: true,
            id: ObjectId::new(),
            name: None,
            layer: layer.to_string(),
            color: None,
            material: None,
            lineweight_mm: None,
            geometry: Geometry::Mesh(mesh),
        };
        let id = obj.id;
        doc.insert(obj);
        id
    }

    #[test]
    fn hidden_layers_are_skipped() {
        let mut doc = Document::default();
        insert_tri(&mut doc, "default");
        doc.layers.insert(
            "hidden".into(),
            LayerStyle { color: None, visible: false, ..LayerStyle::default() },
        );
        insert_tri(&mut doc, "hidden");
        let scene = snapshot(&doc, Theme::Dark);
        assert_eq!(scene.meshes.len(), 1);
    }

    #[test]
    fn hidden_objects_are_skipped() {
        let mut doc = Document::default();
        insert_tri(&mut doc, "default");
        let id = insert_tri(&mut doc, "default");
        doc.get_mut(id).unwrap().visible = false;
        let scene = snapshot(&doc, Theme::Dark);
        assert_eq!(scene.meshes.len(), 1);
        assert_eq!(scene.edges.len(), 1, "hidden mesh contributes no edges");
    }

    #[test]
    fn box_all_edges_are_profile_creases() {
        // A box's 12 edges are all 90° creases → all profile, none interior.
        let b = kernel_mesh::make_box(DVec3::ZERO, DVec3::new(2.0, 1.0, 3.0));
        let (profile, interior) = super::classify_edges(&b);
        // 12 edges → 24 endpoints in the profile list.
        assert_eq!(profile.len(), 24, "all box edges are sharp profile edges");
        assert!(interior.is_empty(), "a box has no soft interior edges");
    }

    #[test]
    fn profile_mode_populates_profile_edges_and_thins_interior() {
        let mut doc = Document::default();
        doc.insert(SceneObject {
            visible: true,
            id: ObjectId::new(),
            name: None,
            layer: "default".into(),
            color: None,
            material: None,
            lineweight_mm: None,
            geometry: Geometry::Mesh(kernel_mesh::make_box(
                DVec3::ZERO,
                DVec3::new(2.0, 1.0, 3.0),
            )),
        });
        // Default (profile off): all edges in scene.edges, none profile.
        let off = snapshot(&doc, Theme::Dark);
        assert!(off.profile_edges.is_empty());
        assert_eq!(off.edges[0].0.len(), 24);

        // Profile on: profile list populated, interior edge list empty for a box.
        let cms = ColorModeSnapshot { profile_edges: true, ..Default::default() };
        let on = snapshot_with_mode(&doc, Theme::Dark, cms);
        assert_eq!(on.profile_edges.len(), 1, "one mesh → one profile entry");
        assert_eq!(on.profile_edges[0].0.len(), 24, "12 profile edges");
        assert!(on.profile_edges[0].2 > 0.0, "profile ribbons carry a half-width");
        assert!(on.edges[0].0.is_empty(), "box has no thin interior edges");
    }

    #[test]
    fn box_snapshot_carries_12_feature_edges() {
        let mut doc = Document::default();
        doc.insert(SceneObject {
            visible: true,
            id: ObjectId::new(),
            name: None,
            layer: "default".into(),
            color: None,
            material: None,
            lineweight_mm: None,
            geometry: Geometry::Mesh(kernel_mesh::make_box(
                DVec3::ZERO,
                DVec3::new(2.0, 1.0, 3.0),
            )),
        });
        let scene = snapshot(&doc, Theme::Dark);
        assert_eq!(scene.edges.len(), 1);
        // 12 feature edges (flat quad diagonals excluded) = 24 endpoints.
        assert_eq!(scene.edges[0].0.len(), 24);
        // Theme-aware ink: near-white on dark, near-black on light. No profile
        // ribbons (thin default edges are distinct from the sketchup preset).
        assert_eq!(scene.edges[0].1, Theme::Dark.curve());
        assert!(scene.profile_edges.is_empty());
        let light = snapshot(&doc, Theme::Light);
        assert_eq!(light.edges[0].0.len(), 24);
        assert_eq!(light.edges[0].1, Theme::Light.curve());
        // Ink actually differs between themes so edges read on both backgrounds.
        assert_ne!(scene.edges[0].1, light.edges[0].1);
    }

    #[test]
    fn layer_color_overrides_theme_selection_overrides_layer() {
        let mut doc = Document::default();
        let red = [1.0, 0.0, 0.0, 1.0];
        doc.layers.insert(
            "walls".into(),
            LayerStyle { color: Some(red), visible: true, ..LayerStyle::default() },
        );
        let id = insert_tri(&mut doc, "walls");
        insert_tri(&mut doc, "default");

        let scene = snapshot(&doc, Theme::Dark);
        assert_eq!(scene.meshes[0].1, red);
        assert_eq!(scene.meshes[1].1, Theme::Dark.mesh());

        doc.selection.insert(id);
        let scene = snapshot(&doc, Theme::Dark);
        assert_eq!(scene.meshes[0].1, Theme::Dark.selected());
    }

    #[test]
    fn hatch_solid_becomes_mesh_lines_become_segments() {
        let square = vec![
            DVec3::ZERO,
            DVec3::new(2.0, 0.0, 0.0),
            DVec3::new(2.0, 2.0, 0.0),
            DVec3::new(0.0, 2.0, 0.0),
        ];
        let mut doc = Document::default();
        doc.insert(SceneObject {
            visible: true,
            id: ObjectId::new(),
            name: None,
            layer: "default".into(),
            color: None,
            material: None,
            lineweight_mm: None,
            geometry: Geometry::Annotation(itsjustcad_doc::Annotation::Hatch {
                boundary: square.clone(),
                pattern: itsjustcad_doc::HatchPattern::Solid,
            }),
        });
        let scene = snapshot(&doc, Theme::Dark);
        assert_eq!(scene.meshes.len(), 1, "solid hatch fills as triangles");
        assert!(scene.lines.is_empty());

        let mut doc = Document::default();
        doc.insert(SceneObject {
            visible: true,
            id: ObjectId::new(),
            name: None,
            layer: "default".into(),
            color: None,
            material: None,
            lineweight_mm: None,
            geometry: Geometry::Annotation(itsjustcad_doc::Annotation::Hatch {
                boundary: square,
                pattern: itsjustcad_doc::HatchPattern::Lines { angle_deg: 0.0, spacing: 0.5 },
            }),
        });
        let scene = snapshot(&doc, Theme::Dark);
        assert!(scene.meshes.is_empty());
        // horizontal lines at y = 0.5, 1.0, 1.5, 2.0 that survive clipping
        assert!(!scene.lines.is_empty());
        for (strip, _, _) in &scene.lines {
            assert_eq!(strip.len(), 2);
        }
    }

    #[test]
    fn dim_is_overlay_only_text_renders_as_strokes() {
        // LinearDim is still rendered as an egui overlay (no strokes in scene).
        let mut doc_dim = Document::default();
        doc_dim.insert(SceneObject {
            visible: true,
            id: ObjectId::new(),
            name: None,
            layer: "default".into(),
            color: None,
            material: None,
            lineweight_mm: None,
            geometry: Geometry::Annotation(itsjustcad_doc::Annotation::LinearDim {
                a: DVec3::ZERO,
                b: DVec3::X,
                offset: 0.5,
            }),
        });
        let scene_dim = snapshot(&doc_dim, Theme::Dark);
        assert!(scene_dim.meshes.is_empty() && scene_dim.lines.is_empty(),
            "LinearDim should not produce scene geometry");

        // Text annotations now render as Hershey vector strokes in world space.
        let mut doc_text = Document::default();
        doc_text.insert(SceneObject {
            visible: true,
            id: ObjectId::new(),
            name: None,
            layer: "default".into(),
            color: None,
            material: None,
            lineweight_mm: None,
            geometry: Geometry::Annotation(itsjustcad_doc::Annotation::Text {
                pos: DVec3::ZERO,
                text: "note".into(),
                height: 0.2,
            }),
        });
        let scene_text = snapshot(&doc_text, Theme::Dark);
        assert!(scene_text.meshes.is_empty(), "text should not produce meshes");
        assert!(!scene_text.lines.is_empty(),
            "text annotation must produce Hershey stroke lines in the scene");
    }

    #[test]
    fn hatch_segments_clip_even_odd() {
        // Unit square, horizontal lines every 0.25: y = 0.25, 0.5, 0.75, 1.0
        let square = [
            DVec3::ZERO,
            DVec3::new(1.0, 0.0, 0.0),
            DVec3::new(1.0, 1.0, 0.0),
            DVec3::new(0.0, 1.0, 0.0),
        ];
        let segs = hatch_segments(&square, 0.0, 0.25);
        // y=1.0 grazes the top edge; at least the three interior lines exist
        assert!(segs.len() >= 3, "got {}", segs.len());
        for [a, b] in &segs {
            assert!((b.x - a.x - 1.0).abs() < 1e-9 || (b.x - a.x) > 0.0);
            assert!((a.y - b.y).abs() < 1e-9, "horizontal");
        }
        // interior spans are the full square width
        let full = segs
            .iter()
            .filter(|[a, b]| (b.x - a.x - 1.0).abs() < 1e-9)
            .count();
        assert!(full >= 3);

        // 45° pattern stays inside the boundary
        for [a, b] in hatch_segments(&square, 45.0, 0.2) {
            for p in [a, b] {
                assert!(p.x > -1e-9 && p.x < 1.0 + 1e-9, "{p}");
                assert!(p.y > -1e-9 && p.y < 1.0 + 1e-9, "{p}");
            }
        }
        // degenerate inputs are empty, not panics
        assert!(hatch_segments(&square[..2], 0.0, 0.25).is_empty());
        assert!(hatch_segments(&square, 0.0, 0.0).is_empty());
    }

    #[test]
    fn unknown_layer_renders_with_theme_default() {
        let mut doc = Document::default();
        insert_tri(&mut doc, "never-created");
        let scene = snapshot(&doc, Theme::Light);
        assert_eq!(scene.meshes.len(), 1);
        assert_eq!(scene.meshes[0].1, Theme::Light.mesh());
    }

    fn unit_square() -> Vec<DVec3> {
        vec![
            DVec3::ZERO,
            DVec3::new(1.0, 0.0, 0.0),
            DVec3::new(1.0, 1.0, 0.0),
            DVec3::new(0.0, 1.0, 0.0),
        ]
    }

    #[test]
    fn crosshatch_has_twice_the_segments_of_lines() {
        let sq = unit_square();
        let horiz = itsjustcad_doc::hatch::hatch_lines(&sq, 0.0, 0.25);
        let vert = itsjustcad_doc::hatch::hatch_lines(&sq, 90.0, 0.25);
        assert!(!horiz.is_empty());
        assert!(!vert.is_empty(), "vertical set must be non-empty");
    }

    #[test]
    fn brick_segments_produces_horizontal_and_vertical_lines() {
        let sq = unit_square();
        let segs = itsjustcad_doc::hatch::hatch_brick(&sq, 0.25);
        assert!(!segs.is_empty(), "brick must produce segments");
        assert!(itsjustcad_doc::hatch::hatch_brick(&sq[..2], 0.25).is_empty());
        assert!(itsjustcad_doc::hatch::hatch_brick(&sq, 0.0).is_empty());
    }

    #[test]
    fn concrete_segments_nonzero_for_valid_boundary() {
        let sq = unit_square();
        let segs = itsjustcad_doc::hatch::hatch_concrete(&sq, 0.3);
        assert!(!segs.is_empty(), "concrete must produce segments");
        assert!(itsjustcad_doc::hatch::hatch_concrete(&sq, 0.0).is_empty());
    }

    #[test]
    fn insulation_segments_nonzero_for_valid_boundary() {
        let sq = unit_square();
        let segs = itsjustcad_doc::hatch::hatch_insulation(&sq, 0.3);
        assert!(!segs.is_empty(), "insulation must produce segments");
        assert!(itsjustcad_doc::hatch::hatch_insulation(&sq, 0.0).is_empty());
    }

    #[test]
    fn earth_segments_nonzero_and_shorter_than_lines() {
        let sq = unit_square();
        let segs = itsjustcad_doc::hatch::hatch_earth(&sq, 0.25);
        assert!(!segs.is_empty(), "earth must produce segments");
        for [a, b] in &segs {
            for p in [a, b] {
                assert!(p.x > -1e-6 && p.x < 1.0 + 1e-6, "x out: {}", p.x);
                assert!(p.y > -1e-6 && p.y < 1.0 + 1e-6, "y out: {}", p.y);
            }
        }
        assert!(itsjustcad_doc::hatch::hatch_earth(&sq, 0.0).is_empty());
    }

    #[test]
    fn block_instance_renders_via_snapshot() {
        let mesh = kernel_mesh::Mesh::new(
            vec![DVec3::ZERO, DVec3::X, DVec3::Y],
            vec![[0, 1, 2]],
        );
        let mut doc = Document::default();
        // Register block definition manually.
        doc.blocks.insert(
            "tri".to_string(),
            vec![itsjustcad_doc::BlockGeometry::Mesh(mesh)],
        );
        // Insert an instance object.
        doc.insert(SceneObject {
            visible: true,
            id: ObjectId::new(),
            name: None,
            layer: "default".into(),
            color: None,
            material: None,
            lineweight_mm: None,
            geometry: Geometry::Instance {
                block: "tri".to_string(),
                position: DVec3::new(5.0, 0.0, 0.0),
                rotation_deg: 0.0,
                scale: 1.0,
                source: None,
                params: Default::default(),
            },
        });
        let scene = snapshot(&doc, Theme::Dark);
        assert_eq!(scene.meshes.len(), 1, "instance resolves to its block mesh");
    }

    // -- color mode tests --

    fn two_mesh_doc() -> Document {
        let mut doc = Document::default();
        insert_tri(&mut doc, "default");
        insert_tri(&mut doc, "default");
        doc
    }

    #[test]
    fn by_object_color_overrides_theme() {
        let mut doc = two_mesh_doc();
        let id = doc.all_ids()[0];
        doc.get_mut(id).unwrap().color = Some([1.0, 0.0, 0.0]);

        let cms = ColorModeSnapshot { color_mode: crate::renderer::ColorMode::ByObject, ..Default::default() };
        let scene = snapshot_with_mode(&doc, Theme::Dark, cms);
        // First object should be red (object color wins)
        assert_eq!(scene.meshes[0].1, [1.0, 0.0, 0.0, 1.0]);
        // Second has no color override — falls back to layer then theme
        assert_eq!(scene.meshes[1].1, Theme::Dark.mesh());
    }

    #[test]
    fn by_type_mesh_hue_distinct_from_curve() {
        let mut doc = Document::default();
        // Add a mesh and a closed curve
        insert_tri(&mut doc, "default");
        doc.insert(SceneObject {
            visible: true,
            id: ObjectId::new(),
            name: None,
            layer: "default".into(),
            color: None,
            material: None,
            lineweight_mm: None,
            geometry: Geometry::Curve(kernel_curve::Curve::Polyline {
                points: vec![DVec3::ZERO, DVec3::X, DVec3::Y],
                closed: true,
            }),
        });

        let cms = ColorModeSnapshot { color_mode: crate::renderer::ColorMode::ByType, ..Default::default() };
        let scene = snapshot_with_mode(&doc, Theme::Dark, cms);
        assert!(!scene.meshes.is_empty());
        assert!(!scene.lines.is_empty());
        // Mesh color is the teal ByType hue
        assert_eq!(scene.meshes[0].1, [0.35, 0.75, 0.72, 1.0]);
        // Curve color is theme curve (not mesh)
        assert_ne!(scene.lines[0].1, scene.meshes[0].1);
    }

    #[test]
    fn random_mode_differs_per_object() {
        let doc = two_mesh_doc();
        let cms = ColorModeSnapshot { color_mode: crate::renderer::ColorMode::Random, ..Default::default() };
        let scene = snapshot_with_mode(&doc, Theme::Dark, cms);
        assert_eq!(scene.meshes.len(), 2);
        // Two distinct objects should (almost certainly) get different random colors.
        // This is probabilistic but the uuid space makes collision astronomically rare.
        assert_ne!(scene.meshes[0].1, scene.meshes[1].1, "random colors should differ");
    }

    #[test]
    fn random_mode_is_stable_across_calls() {
        let doc = two_mesh_doc();
        let cms = ColorModeSnapshot { color_mode: crate::renderer::ColorMode::Random, ..Default::default() };
        let scene1 = snapshot_with_mode(&doc, Theme::Dark, cms);
        let scene2 = snapshot_with_mode(&doc, Theme::Dark, cms);
        assert_eq!(scene1.meshes[0].1, scene2.meshes[0].1, "random color must be stable");
        assert_eq!(scene1.meshes[1].1, scene2.meshes[1].1, "random color must be stable");
    }

    #[test]
    fn hue_from_seed_produces_valid_rgba() {
        use crate::renderer::hue_from_seed;
        for seed in [0u64, 1, 42, u64::MAX, 0x9e3779b97f4a7c15] {
            let [r, g, b, a] = hue_from_seed(seed);
            assert!((0.0..=1.0).contains(&r), "r={r}");
            assert!((0.0..=1.0).contains(&g), "g={g}");
            assert!((0.0..=1.0).contains(&b), "b={b}");
            assert_eq!(a, 1.0);
        }
    }
}
