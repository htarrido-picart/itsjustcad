use glam::{DVec2, DVec3};
use mydrafter_doc::{Annotation, Document, Geometry, HatchPattern};

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

/// Snapshot the document into GPU-ready buffers.
pub fn snapshot(doc: &Document, theme: Theme) -> SceneData {
    let mut scene = SceneData {
        meshes: Vec::new(),
        lines: Vec::new(),
    };
    for obj in doc.objects() {
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
                        for [a, b] in hatch_segments(boundary, *angle_deg, *spacing) {
                            scene.lines.push((
                                vec![
                                    [a.x as f32, a.y as f32, a.z as f32],
                                    [b.x as f32, b.y as f32, b.z as f32],
                                ],
                                color,
                            ));
                        }
                    }
                }
            }
            Geometry::Annotation(_) => {}
        }
    }
    scene
}

/// Parallel hatch lines clipped to a closed polygon (even-odd rule) in the
/// XY plane; the boundary's first-point z carries through.
pub fn hatch_segments(boundary: &[DVec3], angle_deg: f64, spacing: f64) -> Vec<[DVec3; 2]> {
    if boundary.len() < 3 || spacing <= 0.0 {
        return Vec::new();
    }
    let z = boundary[0].z;
    let angle = angle_deg.to_radians();
    let (sin, cos) = angle.sin_cos();
    // Rotate into the pattern frame: hatch lines become horizontal.
    let to_pattern = |p: DVec3| DVec2::new(p.x * cos + p.y * sin, -p.x * sin + p.y * cos);
    let from_pattern =
        |p: DVec2| DVec3::new(p.x * cos - p.y * sin, p.x * sin + p.y * cos, z);
    let pts: Vec<DVec2> = boundary.iter().map(|&p| to_pattern(p)).collect();
    let (min_y, max_y) = pts
        .iter()
        .fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), p| {
            (lo.min(p.y), hi.max(p.y))
        });

    let mut segments = Vec::new();
    let mut k = (min_y / spacing).ceil() as i64;
    while (k as f64) * spacing <= max_y {
        let y = k as f64 * spacing;
        // Even-odd: collect x crossings of the scanline with every edge.
        let mut xs: Vec<f64> = Vec::new();
        for i in 0..pts.len() {
            let a = pts[i];
            let b = pts[(i + 1) % pts.len()];
            // Half-open rule avoids double-counting vertices on the line.
            if (a.y <= y) != (b.y <= y) {
                xs.push(a.x + (y - a.y) / (b.y - a.y) * (b.x - a.x));
            }
        }
        xs.sort_by(|p, q| p.partial_cmp(q).expect("finite"));
        for &[x0, x1] in xs.as_chunks::<2>().0 {
            if x1 - x0 > 1e-9 {
                segments.push([
                    from_pattern(DVec2::new(x0, y)),
                    from_pattern(DVec2::new(x1, y)),
                ]);
            }
        }
        k += 1;
    }
    segments
}

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
            LayerStyle { color: None, visible: false },
        );
        insert_tri(&mut doc, "hidden");
        let scene = snapshot(&doc, Theme::Dark);
        assert_eq!(scene.meshes.len(), 1);
    }

    #[test]
    fn layer_color_overrides_theme_selection_overrides_layer() {
        let mut doc = Document::default();
        let red = [1.0, 0.0, 0.0, 1.0];
        doc.layers.insert(
            "walls".into(),
            LayerStyle { color: Some(red), visible: true },
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
}
