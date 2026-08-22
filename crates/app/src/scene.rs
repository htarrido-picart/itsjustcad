use mydrafter_doc::{Document, Geometry};
use mydrafter_render::SceneData;

/// Chord tolerance for display tessellation of curves.
const DISPLAY_TOL: f64 = 0.005;

pub const MESH_COLOR: [f32; 4] = [0.72, 0.73, 0.78, 1.0];
pub const MESH_SELECTED: [f32; 4] = [0.95, 0.82, 0.3, 1.0];
pub const CURVE_COLOR: [f32; 4] = [0.15, 0.16, 0.18, 1.0];
pub const CURVE_SELECTED: [f32; 4] = [0.98, 0.75, 0.1, 1.0];

/// Snapshot the document into GPU-ready buffers.
pub fn snapshot(doc: &Document) -> SceneData {
    let mut scene = SceneData {
        meshes: Vec::new(),
        lines: Vec::new(),
    };
    for obj in doc.objects() {
        let selected = doc.selection.contains(&obj.id);
        match &obj.geometry {
            Geometry::Mesh(mesh) => {
                let color = if selected { MESH_SELECTED } else { MESH_COLOR };
                scene.meshes.push((mesh.to_render(), color));
            }
            Geometry::Curve(curve) => {
                let color = if selected { CURVE_SELECTED } else { CURVE_COLOR };
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
        }
    }
    scene
}
