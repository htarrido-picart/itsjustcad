use mydrafter_doc::{Document, Geometry};

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
        }
    }
    scene
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
    fn unknown_layer_renders_with_theme_default() {
        let mut doc = Document::default();
        insert_tri(&mut doc, "never-created");
        let scene = snapshot(&doc, Theme::Light);
        assert_eq!(scene.meshes.len(), 1);
        assert_eq!(scene.meshes[0].1, Theme::Light.mesh());
    }
}
