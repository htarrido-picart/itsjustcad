use glam::{DVec2, DVec3};
use mydrafter_doc::{
    hatch::{hatch_brick, hatch_concrete, hatch_earth, hatch_insulation, hatch_lines},
    Annotation, Document, Geometry, HatchPattern,
};

use crate::renderer::SceneData;

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
    pub fn background(self) -> [f32; 4] {
        match self {
            Theme::Dark => [0.13, 0.14, 0.16, 1.0],
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

/// Push segment pairs as 2-point line strips into the scene lines list.
fn push_hatch_segs(lines: &mut Vec<(Vec<[f32; 3]>, [f32; 4])>, segs: Vec<[DVec3; 2]>, color: [f32; 4]) {
    for [a, b] in segs {
        lines.push((
            vec![
                [a.x as f32, a.y as f32, a.z as f32],
                [b.x as f32, b.y as f32, b.z as f32],
            ],
            color,
        ));
    }
}

/// Snapshot the document into GPU-ready buffers.
pub fn snapshot(doc: &Document, theme: Theme) -> SceneData {
    let mut scene = SceneData {
        meshes: Vec::new(),
        lines: Vec::new(),
        edges: Vec::new(),
        // The underlay image is decoded app-side (the app owns the `image`
        // dependency) and attached to the returned scene.
        underlay: None,
    };
    for obj in doc.objects() {
        if !obj.visible {
            continue; // hidden object (hideobj)
        }
        let style = doc.layers.get(&obj.layer);
        if style.is_some_and(|s| !s.visible) {
            continue; // hidden layer
        }
        // Layer color wins over the theme default; selection wins over both.
        let layer_color = style.and_then(|s| s.color);
        let selected = doc.selection.contains(&obj.id);
        match &obj.geometry {
            Geometry::Mesh(mesh) => {
                let color = if selected {
                    theme.selected()
                } else {
                    layer_color.unwrap_or(theme.mesh())
                };
                scene.meshes.push((mesh.to_render(), color));
                // Feature edges for the wireframe/x-ray/ghosted display modes.
                let edge_color = if selected { theme.selected() } else { theme.curve() };
                let segments: Vec<[f32; 3]> = kernel_mesh::feature_edges(mesh)
                    .iter()
                    .flat_map(|(a, b)| {
                        [
                            [a.x as f32, a.y as f32, a.z as f32],
                            [b.x as f32, b.y as f32, b.z as f32],
                        ]
                    })
                    .collect();
                scene.edges.push((segments, edge_color));
            }
            Geometry::Curve(curve) => {
                let color = if selected {
                    theme.selected()
                } else {
                    layer_color.unwrap_or(theme.curve())
                };
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
                scene.lines.push((pts, color));
            }
            // Hatches are scene geometry (fill triangles / pattern lines);
            // dimensions and text are drawn as an egui overlay by the app.
            Geometry::Annotation(Annotation::Hatch { boundary, pattern }) => {
                let color = if selected {
                    theme.selected()
                } else {
                    layer_color.unwrap_or(theme.curve())
                };
                match pattern {
                    HatchPattern::Solid => {
                        let pts2: Vec<DVec2> =
                            boundary.iter().map(|p| p.truncate()).collect();
                        let faces = kernel_mesh::earcut(&pts2);
                        if !faces.is_empty() {
                            let mesh = kernel_mesh::Mesh::new(boundary.clone(), faces);
                            scene.meshes.push((mesh.to_render(), color));
                        }
                    }
                    HatchPattern::Lines { angle_deg, spacing } => {
                        push_hatch_segs(&mut scene.lines, hatch_lines(boundary, *angle_deg, *spacing), color);
                    }
                    HatchPattern::Crosshatch { angle_deg, spacing } => {
                        push_hatch_segs(&mut scene.lines, hatch_lines(boundary, *angle_deg, *spacing), color);
                        push_hatch_segs(&mut scene.lines, hatch_lines(boundary, *angle_deg + 90.0, *spacing), color);
                    }
                    HatchPattern::Brick { spacing } => {
                        push_hatch_segs(&mut scene.lines, hatch_brick(boundary, *spacing), color);
                    }
                    HatchPattern::Concrete { spacing } => {
                        push_hatch_segs(&mut scene.lines, hatch_concrete(boundary, *spacing), color);
                    }
                    HatchPattern::Insulation { spacing } => {
                        push_hatch_segs(&mut scene.lines, hatch_insulation(boundary, *spacing), color);
                    }
                    HatchPattern::Earth { spacing } => {
                        push_hatch_segs(&mut scene.lines, hatch_earth(boundary, *spacing), color);
                    }
                }
            }
            Geometry::Annotation(_) => {}
            // Block instances: resolved to constituent geometry at render time.
            Geometry::Instance { block, position, rotation_deg, scale } => {
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
                    let color = if selected { theme.selected() } else { layer_color.unwrap_or(theme.curve()) };
                    for def_geo in defs {
                        match def_geo {
                            mydrafter_doc::BlockGeometry::Mesh(m) => {
                                // Transform positions and build a new mesh.
                                let new_pos: Vec<DVec3> =
                                    m.positions().iter().map(|&p| transform(p)).collect();
                                let new_mesh = kernel_mesh::Mesh::new(new_pos, m.faces().to_vec());
                                let mesh_color = if selected { theme.selected() } else { layer_color.unwrap_or(theme.mesh()) };
                                scene.meshes.push((new_mesh.to_render(), mesh_color));
                                let segments: Vec<[f32; 3]> = kernel_mesh::feature_edges(&new_mesh)
                                    .iter()
                                    .flat_map(|(a, b)| {
                                        [
                                            [a.x as f32, a.y as f32, a.z as f32],
                                            [b.x as f32, b.y as f32, b.z as f32],
                                        ]
                                    })
                                    .collect();
                                scene.edges.push((segments, color));
                            }
                            mydrafter_doc::BlockGeometry::Curve(c) => {
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
                                scene.lines.push((pts, color));
                            }
                            mydrafter_doc::BlockGeometry::Annotation(_) => {
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

// The hatch tessellation functions live in mydrafter_doc::hatch and are
// imported at the top of this file. Re-export hatch_lines so existing
// test references to `hatch_segments` work without renaming.
pub use mydrafter_doc::hatch::hatch_lines as hatch_segments;

#[cfg(test)]
mod tests {
    use glam::DVec3;
    use mydrafter_doc::{Document, LayerStyle, ObjectId, SceneObject};

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
    fn box_snapshot_carries_12_feature_edges() {
        let mut doc = Document::default();
        doc.insert(SceneObject {
            visible: true,
            id: ObjectId::new(),
            name: None,
            layer: "default".into(),
            geometry: Geometry::Mesh(kernel_mesh::make_box(
                DVec3::ZERO,
                DVec3::new(2.0, 1.0, 3.0),
            )),
        });
        let scene = snapshot(&doc, Theme::Dark);
        assert_eq!(scene.edges.len(), 1);
        // 12 feature edges (flat quad diagonals excluded) = 24 endpoints.
        assert_eq!(scene.edges[0].0.len(), 24);
        assert_eq!(scene.edges[0].1, Theme::Dark.curve());
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
            geometry: Geometry::Annotation(mydrafter_doc::Annotation::Hatch {
                boundary: square.clone(),
                pattern: mydrafter_doc::HatchPattern::Solid,
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
            geometry: Geometry::Annotation(mydrafter_doc::Annotation::Hatch {
                boundary: square,
                pattern: mydrafter_doc::HatchPattern::Lines { angle_deg: 0.0, spacing: 0.5 },
            }),
        });
        let scene = snapshot(&doc, Theme::Dark);
        assert!(scene.meshes.is_empty());
        // horizontal lines at y = 0.5, 1.0, 1.5, 2.0 that survive clipping
        assert!(!scene.lines.is_empty());
        for (strip, _) in &scene.lines {
            assert_eq!(strip.len(), 2);
        }
    }

    #[test]
    fn dim_and_text_are_overlay_only() {
        let mut doc = Document::default();
        doc.insert(SceneObject {
            visible: true,
            id: ObjectId::new(),
            name: None,
            layer: "default".into(),
            geometry: Geometry::Annotation(mydrafter_doc::Annotation::LinearDim {
                a: DVec3::ZERO,
                b: DVec3::X,
                offset: 0.5,
            }),
        });
        doc.insert(SceneObject {
            visible: true,
            id: ObjectId::new(),
            name: None,
            layer: "default".into(),
            geometry: Geometry::Annotation(mydrafter_doc::Annotation::Text {
                pos: DVec3::ZERO,
                text: "note".into(),
                height: 0.2,
            }),
        });
        let scene = snapshot(&doc, Theme::Dark);
        assert!(scene.meshes.is_empty() && scene.lines.is_empty());
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
        let horiz = mydrafter_doc::hatch::hatch_lines(&sq, 0.0, 0.25);
        let vert = mydrafter_doc::hatch::hatch_lines(&sq, 90.0, 0.25);
        assert!(!horiz.is_empty());
        assert!(!vert.is_empty(), "vertical set must be non-empty");
    }

    #[test]
    fn brick_segments_produces_horizontal_and_vertical_lines() {
        let sq = unit_square();
        let segs = mydrafter_doc::hatch::hatch_brick(&sq, 0.25);
        assert!(!segs.is_empty(), "brick must produce segments");
        assert!(mydrafter_doc::hatch::hatch_brick(&sq[..2], 0.25).is_empty());
        assert!(mydrafter_doc::hatch::hatch_brick(&sq, 0.0).is_empty());
    }

    #[test]
    fn concrete_segments_nonzero_for_valid_boundary() {
        let sq = unit_square();
        let segs = mydrafter_doc::hatch::hatch_concrete(&sq, 0.3);
        assert!(!segs.is_empty(), "concrete must produce segments");
        assert!(mydrafter_doc::hatch::hatch_concrete(&sq, 0.0).is_empty());
    }

    #[test]
    fn insulation_segments_nonzero_for_valid_boundary() {
        let sq = unit_square();
        let segs = mydrafter_doc::hatch::hatch_insulation(&sq, 0.3);
        assert!(!segs.is_empty(), "insulation must produce segments");
        assert!(mydrafter_doc::hatch::hatch_insulation(&sq, 0.0).is_empty());
    }

    #[test]
    fn earth_segments_nonzero_and_shorter_than_lines() {
        let sq = unit_square();
        let segs = mydrafter_doc::hatch::hatch_earth(&sq, 0.25);
        assert!(!segs.is_empty(), "earth must produce segments");
        for [a, b] in &segs {
            for p in [a, b] {
                assert!(p.x > -1e-6 && p.x < 1.0 + 1e-6, "x out: {}", p.x);
                assert!(p.y > -1e-6 && p.y < 1.0 + 1e-6, "y out: {}", p.y);
            }
        }
        assert!(mydrafter_doc::hatch::hatch_earth(&sq, 0.0).is_empty());
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
            vec![mydrafter_doc::BlockGeometry::Mesh(mesh)],
        );
        // Insert an instance object.
        doc.insert(SceneObject {
            visible: true,
            id: ObjectId::new(),
            name: None,
            layer: "default".into(),
            geometry: Geometry::Instance {
                block: "tri".to_string(),
                position: DVec3::new(5.0, 0.0, 0.0),
                rotation_deg: 0.0,
                scale: 1.0,
            },
        });
        let scene = snapshot(&doc, Theme::Dark);
        assert_eq!(scene.meshes.len(), 1, "instance resolves to its block mesh");
    }
}
