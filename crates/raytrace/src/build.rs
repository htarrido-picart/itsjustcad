// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! Extract a ray-traceable [`Scene`] from an `itsjustcad_doc::Document`.
//!
//! This mirrors `itsjustcad_render::snapshot`'s scene extraction: it walks
//! visible objects on visible layers, resolves each mesh's colour (object →
//! layer → neutral default) and PBR material (`obj.material.pbr()`), and expands
//! block instances to their constituent meshes with the same transform the
//! viewport uses. Curves, points, hatches and annotations are ignored (meshes
//! first — thin-tube curves are a later cart).
//!
//! The sun is taken from `doc.sun` (az/alt via `itsjustcad_solar::sun_direction`)
//! or, failing that, a sensible default so an untagged model still renders lit.

use glam::DVec3;
use itsjustcad_doc::{Document, Geometry, SceneObject};

use crate::material::{srgb_to_linear, Material};
use crate::scene::{Scene, SceneBuilder, Sky, Sun};

/// Neutral matte fallback colour (sRGB) for objects with no colour/material.
const DEFAULT_ALBEDO: [f32; 3] = [0.72, 0.73, 0.78];

/// Resolve the doc sun into a ray-tracer [`Sun`], or a default overhead-ish sun
/// when the document has none. Reuses `itsjustcad_solar::sun_direction` for the
/// az/alt → world-vector conversion (X=East, Y=North, Z=Up), matching solar.
pub fn sun_from_doc(doc: &Document) -> Sun {
    match doc.sun {
        Some(sp) => {
            let [x, y, z] = itsjustcad_solar::sun_direction(sp.azimuth_deg, sp.altitude_deg);
            let dir = DVec3::new(x as f64, y as f64, z as f64).normalize_or_zero();
            let dir = if dir == DVec3::ZERO { DVec3::Z } else { dir };
            // Warm the low sun (golden hour), cool-white when high.
            let warmth = 1.0 - (sp.altitude_deg / 90.0).clamp(0.0, 1.0);
            let color = DVec3::new(1.0, 0.95 - 0.15 * warmth, 0.85 - 0.35 * warmth);
            Sun { dir, radiance: color * 6.0, angular_radius: 0.02 }
        }
        None => Sun::default(),
    }
}

/// Resolve a mesh object's material: a doc `material2` PBR triple maps to a full
/// BSDF; otherwise the object/layer colour becomes a diffuse albedo.
fn material_for(obj: &SceneObject, layer_color: Option<[f32; 3]>) -> Material {
    if let Some(m) = &obj.material {
        let (albedo, rough, metal) = m.pbr();
        return Material::from_pbr(albedo, rough, metal);
    }
    let rgb = obj
        .color
        .or(layer_color)
        .unwrap_or(DEFAULT_ALBEDO);
    Material::diffuse(srgb_to_linear(DVec3::new(
        rgb[0] as f64,
        rgb[1] as f64,
        rgb[2] as f64,
    )))
}

/// Build a [`Scene`] from a document. `sky` sets the ambient/IBL; pass
/// [`Sky::default`] for a physical-ish blue sky.
pub fn scene_from_doc(doc: &Document, sky: Sky) -> Scene {
    let mut b = SceneBuilder::new();
    let sun = sun_from_doc(doc);

    for obj in doc.objects() {
        if !obj.visible {
            continue;
        }
        let style = doc.layers.get(&obj.layer);
        if style.is_some_and(|s| !s.visible) {
            continue;
        }
        // Layer colour is stored as RGBA; drop alpha for the albedo.
        let layer_color = style
            .and_then(|s| s.color)
            .map(|[r, g, b, _]| [r, g, b]);

        match &obj.geometry {
            Geometry::Mesh(mesh)
            | Geometry::Frame { mesh, .. }
            | Geometry::Area { mesh, .. } => {
                let mat = b.add_material(material_for(obj, layer_color));
                b.add_mesh(mesh.positions(), mesh.faces(), mat);
            }
            Geometry::Instance { block, position, rotation_deg, scale, .. } => {
                let Some(defs) = doc.blocks.get(block) else { continue };
                let s = *scale;
                let rot = rotation_deg.to_radians();
                let (sin_r, cos_r) = rot.sin_cos();
                let transform = |p: DVec3| -> DVec3 {
                    // `ps` is already uniformly scaled by `s` on all axes, so Z
                    // only needs the instance origin added (no second *s).
                    let ps = p * s;
                    DVec3::new(
                        ps.x * cos_r - ps.y * sin_r + position.x,
                        ps.x * sin_r + ps.y * cos_r + position.y,
                        ps.z + position.z,
                    )
                };
                let mat = b.add_material(material_for(obj, layer_color));
                for def in defs {
                    if let itsjustcad_doc::BlockGeometry::Mesh(m) = def {
                        let positions: Vec<DVec3> =
                            m.positions().iter().map(|&p| transform(p)).collect();
                        b.add_mesh(&positions, m.faces(), mat);
                    }
                }
            }
            // Curves / points / annotations: not raytraced in part 1.
            _ => {}
        }
    }

    b.build(Some(sun), sky)
}

#[cfg(test)]
mod tests {
    use super::*;
    use itsjustcad_doc::{Geometry, ObjectId, SceneObject, SunPosition};

    fn mesh_obj(layer: &str) -> SceneObject {
        SceneObject {
            visible: true,
            id: ObjectId::new(),
            name: None,
            layer: layer.to_string(),
            color: None,
            material: None,
            lineweight_mm: None,
            geometry: Geometry::Mesh(kernel_mesh::make_box(
                DVec3::ZERO,
                DVec3::new(1.0, 1.0, 1.0),
            )),
        }
    }

    #[test]
    fn box_doc_yields_twelve_triangles() {
        let mut doc = Document::default();
        doc.insert(mesh_obj("default"));
        let scene = scene_from_doc(&doc, Sky::default());
        // A box is 12 triangles.
        assert_eq!(scene.triangle_count(), 12);
    }

    #[test]
    fn hidden_object_contributes_nothing() {
        let mut doc = Document::default();
        let mut o = mesh_obj("default");
        o.visible = false;
        doc.insert(o);
        let scene = scene_from_doc(&doc, Sky::default());
        assert_eq!(scene.triangle_count(), 0);
    }

    #[test]
    fn doc_sun_direction_matches_solar() {
        let mut doc = Document::default();
        doc.sun = Some(SunPosition { azimuth_deg: 180.0, altitude_deg: 45.0 });
        let sun = sun_from_doc(&doc);
        // Due-south, 45° up: pointing north-ish and up in X=E,Y=N,Z=Up world.
        // azimuth 180 (south) → -Y; altitude 45 → +Z.
        assert!(sun.dir.z > 0.6, "high sun: {}", sun.dir);
        assert!(sun.dir.y < 0.0, "south sun points -Y: {}", sun.dir);
    }

    #[test]
    fn no_sun_falls_back_to_default() {
        let doc = Document::default();
        let sun = sun_from_doc(&doc);
        assert!(sun.dir.length() > 0.99);
    }

    /// A scaled block instance must scale local Z exactly once (uniform scale),
    /// then translate by the instance origin — regression guard for the
    /// double-Z-scale bug (`ps.z * s` instead of `ps.z`), which turned world Z
    /// into `z*s²`. Verified through the real `scene_from_doc` transform via the
    /// resulting scene's Z bounds.
    #[test]
    fn scaled_block_instance_scales_z_once() {
        use itsjustcad_doc::BlockGeometry;

        // Block-local triangle spanning z in [0, 1] with non-zero area.
        let tri = kernel_mesh::Mesh::new(
            vec![
                DVec3::new(0.0, 0.0, 0.0),
                DVec3::new(1.0, 0.0, 0.0),
                DVec3::new(0.0, 1.0, 1.0),
            ],
            vec![[0, 1, 2]],
        );

        let mut doc = Document::default();
        doc.blocks
            .insert("blk".to_string(), vec![BlockGeometry::Mesh(tri)]);

        let scale = 2.0;
        let origin = DVec3::new(3.0, 4.0, 1000.0);
        doc.insert(SceneObject {
            visible: true,
            id: ObjectId::new(),
            name: None,
            layer: "default".to_string(),
            color: None,
            material: None,
            lineweight_mm: None,
            geometry: Geometry::Instance {
                block: "blk".to_string(),
                position: origin,
                rotation_deg: 0.0,
                scale,
                source: None,
                params: Default::default(),
            },
        });

        let scene = scene_from_doc(&doc, Sky::default());
        let bounds = scene.bounds.expect("instance produced geometry");

        // Local z=1 → world z = 1*scale + origin.z = 1002 (NOT 1*scale² = 1004).
        let expected_max_z = 1.0 * scale + origin.z;
        let expected_min_z = 0.0 * scale + origin.z;
        assert!(
            (bounds.max.z - expected_max_z).abs() < 1e-9,
            "max z double-scaled: got {}, want {}",
            bounds.max.z,
            expected_max_z
        );
        assert!(
            (bounds.min.z - expected_min_z).abs() < 1e-9,
            "min z off: got {}, want {}",
            bounds.min.z,
            expected_min_z
        );
    }
}
