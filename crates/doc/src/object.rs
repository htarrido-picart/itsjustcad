// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

use glam::DVec3;
use kernel_curve::Curve;
use kernel_mesh::{Aabb, Mesh};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::structure::Section;

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[serde(transparent)]
pub struct ObjectId(pub Uuid);

impl ObjectId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }

    /// Short display form for the command line and LLM scene digest.
    pub fn short(&self) -> String {
        self.0.simple().to_string()[..8].to_string()
    }
}

impl Default for ObjectId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for ObjectId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.short())
    }
}

/// Hatch fill pattern.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "pattern", rename_all = "snake_case")]
pub enum HatchPattern {
    Solid,
    Lines { angle_deg: f64, spacing: f64 },
    /// Two sets of parallel lines at `angle_deg` and `angle_deg + 90°`.
    Crosshatch { angle_deg: f64, spacing: f64 },
    /// Running bond brick pattern: horizontal courses with staggered joints.
    Brick { spacing: f64 },
    /// Concrete: irregular short dashes approximating the standard dash-dot scatter.
    Concrete { spacing: f64 },
    /// Insulation batt: zigzag line along the boundary's long axis.
    Insulation { spacing: f64 },
    /// Earth fill: 45° short dashes (standard drafting earth hatch).
    Earth { spacing: f64 },
}

/// Drafting objects: they live in the document like geometry (layers,
/// selection, undo) but carry measured/typed content instead of shape.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "ann", rename_all = "snake_case")]
pub enum Annotation {
    /// Linear dimension between `a` and `b`; the dimension line sits `offset`
    /// to the left of a→b in the XY plane. The measured value is derived.
    LinearDim { a: DVec3, b: DVec3, offset: f64 },
    Text { pos: DVec3, text: String, height: f64 },
    /// Hatch of a closed boundary polygon (tessellated at creation time).
    Hatch { boundary: Vec<DVec3>, pattern: HatchPattern },
}

impl Annotation {
    /// Points that bound the annotation for AABB/picking purposes.
    pub fn points(&self) -> Vec<DVec3> {
        match self {
            Annotation::LinearDim { a, b, .. } => vec![*a, *b],
            Annotation::Text { pos, .. } => vec![*pos],
            Annotation::Hatch { boundary, .. } => boundary.clone(),
        }
    }
}

/// A geometry snapshot stored in a block definition. The same enum as
/// `Geometry` minus recursive Instance references (blocks are flat).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "geo", rename_all = "snake_case")]
pub enum BlockGeometry {
    Mesh(Mesh),
    Curve(Curve),
    Annotation(Annotation),
}

/// One declared parameter of a parametric (dynamic) block: a name plus a
/// default value (as a string, substituted verbatim into the command template).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ParamBlockParam {
    pub name: String,
    pub default: String,
}

/// A parametric block DEFINITION. Its geometry is a template of command lines
/// (the same `{param}` substitution used by plugin macros); instances bake the
/// template at their own param values. Stored on the `Document`; instances that
/// reference it via `Geometry::Instance { source, params, .. }` re-derive their
/// baked geometry when a param changes.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ParamBlockDef {
    pub params: Vec<ParamBlockParam>,
    /// Command-template lines. `{name}` tokens are replaced by param values.
    pub body: Vec<String>,
}

impl ParamBlockDef {
    /// Default param values as an ordered `name -> value` map.
    pub fn default_values(&self) -> std::collections::BTreeMap<String, String> {
        self.params
            .iter()
            .map(|p| (p.name.clone(), p.default.clone()))
            .collect()
    }

    /// Resolve effective param values: defaults, overridden by `overrides`
    /// (unknown override keys are ignored — the definition owns the param set).
    pub fn resolve_values(
        &self,
        overrides: &std::collections::BTreeMap<String, String>,
    ) -> std::collections::BTreeMap<String, String> {
        let mut vals = self.default_values();
        for (k, v) in overrides {
            if vals.contains_key(k) {
                vals.insert(k.clone(), v.clone());
            }
        }
        vals
    }

    /// Expand the template body, replacing `{name}` tokens with `values[name]`.
    /// Unknown `{tokens}` are left verbatim.
    pub fn expand(&self, values: &std::collections::BTreeMap<String, String>) -> Vec<String> {
        self.body.iter().map(|line| subst(line, values)).collect()
    }
}

/// Replace `{name}` tokens in `line` using `values`; unknown tokens survive.
fn subst(line: &str, values: &std::collections::BTreeMap<String, String>) -> String {
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        if let Some(close_rel) = rest[open..].find('}') {
            let token = &rest[open + 1..open + close_rel];
            match values.get(token) {
                Some(v) => out.push_str(v),
                None => out.push_str(&rest[open..open + close_rel + 1]),
            }
            rest = &rest[open + close_rel + 1..];
        } else {
            out.push_str(&rest[open..]);
            rest = "";
        }
    }
    out.push_str(rest);
    out
}

impl BlockGeometry {
    pub fn aabb(&self) -> Aabb {
        match self {
            BlockGeometry::Mesh(m) => m.aabb(),
            BlockGeometry::Curve(c) => Aabb::from_points(c.points_bound()),
            BlockGeometry::Annotation(a) => Aabb::from_points(a.points()),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "geo", rename_all = "snake_case")]
pub enum Geometry {
    Mesh(Mesh),
    Curve(Curve),
    Annotation(Annotation),
    /// Reference to a named block definition, placed at `position` with
    /// optional rotation (degrees CCW about Z) and uniform scale.
    ///
    /// For a *dynamic* (parametric) block, `source` names the parametric
    /// definition and `params` holds this instance's param values; `block` then
    /// points at a per-instance baked entry in `Document::blocks`. Plain blocks
    /// leave `source` `None` and `params` empty (old files load unchanged).
    Instance {
        block: String,
        position: DVec3,
        #[serde(default, skip_serializing_if = "is_zero")]
        rotation_deg: f64,
        #[serde(default = "one", skip_serializing_if = "is_one")]
        scale: f64,
        /// Parametric source-block name, when this is a dynamic-block instance.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source: Option<String>,
        /// This instance's param values (empty for plain blocks).
        #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
        params: std::collections::BTreeMap<String, String>,
    },
    /// Decimated point cloud from a LAS import. Positions are world-space
    /// after applying LAS scale factors and offsets.
    Points { positions: Vec<DVec3> },
    /// Structural frame member (beam or column): a line from `a` to `b` given a
    /// named section swept along it, rolled by `orientation_deg`. The `mesh` is
    /// the derived solid, kept so pick/move/export treat this like any solid.
    Frame {
        kind: FrameKind,
        a: DVec3,
        b: DVec3,
        section: Section,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        material: Option<String>,
        #[serde(default, skip_serializing_if = "is_zero")]
        orientation_deg: f64,
        mesh: Mesh,
    },
    /// Structural area member (slab or wall): a closed `boundary` extruded by
    /// `thickness` along `dir`. The `mesh` is the derived solid.
    Area {
        kind: AreaKind,
        boundary: Vec<DVec3>,
        thickness: f64,
        dir: DVec3,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        material: Option<String>,
        mesh: Mesh,
    },
}

/// Frame member ergonomic subtype. Both use the same underlying representation;
/// the distinction drives defaults (beam ~horizontal, column ~vertical) and
/// display/scheduling labels.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FrameKind {
    Beam,
    Column,
}

impl FrameKind {
    pub fn label(self) -> &'static str {
        match self {
            FrameKind::Beam => "beam",
            FrameKind::Column => "column",
        }
    }
}

/// Area member ergonomic subtype (slab extrudes vertically, wall along its
/// normal).
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AreaKind {
    Slab,
    Wall,
}

impl AreaKind {
    pub fn label(self) -> &'static str {
        match self {
            AreaKind::Slab => "slab",
            AreaKind::Wall => "wall",
        }
    }
}

fn is_zero(v: &f64) -> bool {
    v.abs() < 1e-12
}

fn one() -> f64 {
    1.0
}

fn is_one(v: &f64) -> bool {
    (v - 1.0).abs() < 1e-12
}

impl Geometry {
    pub fn translate(&mut self, d: DVec3) {
        match self {
            Geometry::Mesh(m) => m.transform(glam::DMat4::from_translation(d)),
            Geometry::Curve(c) => c.translate(d),
            Geometry::Annotation(a) => match a {
                Annotation::LinearDim { a, b, .. } => {
                    *a += d;
                    *b += d;
                }
                Annotation::Text { pos, .. } => *pos += d,
                Annotation::Hatch { boundary, .. } => {
                    boundary.iter_mut().for_each(|p| *p += d)
                }
            },
            Geometry::Instance { position, .. } => *position += d,
            Geometry::Points { positions } => positions.iter_mut().for_each(|p| *p += d),
            Geometry::Frame { a, b, mesh, .. } => {
                *a += d;
                *b += d;
                mesh.transform(glam::DMat4::from_translation(d));
            }
            Geometry::Area { boundary, mesh, .. } => {
                boundary.iter_mut().for_each(|p| *p += d);
                mesh.transform(glam::DMat4::from_translation(d));
            }
        }
    }

    /// The derived/backing triangle mesh for solid-facing consumers (export,
    /// volume, section cuts). `None` for non-solid geometry.
    pub fn mesh(&self) -> Option<&Mesh> {
        match self {
            Geometry::Mesh(m) | Geometry::Frame { mesh: m, .. } | Geometry::Area { mesh: m, .. } => {
                Some(m)
            }
            _ => None,
        }
    }

    /// Apply an affine transform. Returns `false` when a curve had to be
    /// tessellated to represent the result (see [`Curve::transform`]).
    pub fn transform(&mut self, m: &glam::DMat4, tol: f64) -> bool {
        match self {
            Geometry::Mesh(mesh) => {
                mesh.transform(*m);
                true
            }
            Geometry::Curve(c) => c.transform(m, tol),
            Geometry::Annotation(a) => {
                // Anchor points transform exactly; scalar sizes (offset, text
                // height) follow the X-axis scale so uniform scales behave.
                let s = m.transform_vector3(DVec3::X).length();
                match a {
                    Annotation::LinearDim { a, b, offset } => {
                        *a = m.transform_point3(*a);
                        *b = m.transform_point3(*b);
                        *offset *= s;
                    }
                    Annotation::Text { pos, height, .. } => {
                        *pos = m.transform_point3(*pos);
                        *height *= s;
                    }
                    Annotation::Hatch { boundary, pattern } => {
                        boundary.iter_mut().for_each(|p| *p = m.transform_point3(*p));
                        let s = m.transform_vector3(DVec3::X).length();
                        match pattern {
                            HatchPattern::Lines { spacing, .. }
                            | HatchPattern::Crosshatch { spacing, .. }
                            | HatchPattern::Brick { spacing }
                            | HatchPattern::Concrete { spacing }
                            | HatchPattern::Insulation { spacing }
                            | HatchPattern::Earth { spacing } => *spacing *= s,
                            HatchPattern::Solid => {}
                        }
                    }
                }
                true
            }
            Geometry::Instance { position, scale, .. } => {
                let s = m.transform_vector3(DVec3::X).length();
                *position = m.transform_point3(*position);
                *scale *= s;
                true
            }
            Geometry::Points { positions } => {
                positions.iter_mut().for_each(|p| *p = m.transform_point3(*p));
                true
            }
            Geometry::Frame { a, b, mesh, .. } => {
                *a = m.transform_point3(*a);
                *b = m.transform_point3(*b);
                mesh.transform(*m);
                true
            }
            Geometry::Area { boundary, dir, mesh, .. } => {
                boundary.iter_mut().for_each(|p| *p = m.transform_point3(*p));
                *dir = m.transform_vector3(*dir);
                mesh.transform(*m);
                true
            }
        }
    }

    pub fn aabb(&self) -> Aabb {
        match self {
            Geometry::Mesh(m) => m.aabb(),
            Geometry::Curve(c) => Aabb::from_points(c.points_bound()),
            Geometry::Annotation(a) => Aabb::from_points(a.points()),
            // Approximate AABB: a small sphere around the insertion point.
            // The real size is only known with the block definition, which lives
            // on Document; keep it a point so picking still works.
            Geometry::Instance { position, scale, .. } => {
                let s = *scale;
                Aabb::from_points(vec![*position - DVec3::splat(s), *position + DVec3::splat(s)])
            }
            Geometry::Points { positions } => Aabb::from_points(positions.clone()),
            Geometry::Frame { mesh, .. } | Geometry::Area { mesh, .. } => mesh.aabb(),
        }
    }
}

/// A named physical-material preset for rendering. Each preset maps to a
/// canonical base color / roughness / metallic so `material2 sel glass` reads
/// distinctly from `material2 sel concrete` in the viewport and in the exported
/// control images. Kept separate from the *structural* `Material` (E + density)
/// which drives analysis and IFC — this one is purely appearance.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MaterialPreset {
    Concrete,
    Glass,
    Metal,
    Wood,
}

impl MaterialPreset {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "concrete" => Some(MaterialPreset::Concrete),
            "glass" => Some(MaterialPreset::Glass),
            "metal" => Some(MaterialPreset::Metal),
            "wood" => Some(MaterialPreset::Wood),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            MaterialPreset::Concrete => "concrete",
            MaterialPreset::Glass => "glass",
            MaterialPreset::Metal => "metal",
            MaterialPreset::Wood => "wood",
        }
    }

    /// Canonical (base_color, roughness, metallic) for the preset.
    pub fn pbr(self) -> ([f32; 3], f32, f32) {
        match self {
            // Light grey, matte, dielectric.
            MaterialPreset::Concrete => ([0.62, 0.62, 0.60], 0.90, 0.0),
            // Cool tinted, very smooth, dielectric (rendered translucent-ish).
            MaterialPreset::Glass => ([0.55, 0.72, 0.80], 0.05, 0.0),
            // Neutral bright, smooth, fully metallic.
            MaterialPreset::Metal => ([0.80, 0.81, 0.83], 0.25, 1.0),
            // Warm brown, medium roughness, dielectric.
            MaterialPreset::Wood => ([0.55, 0.36, 0.20], 0.65, 0.0),
        }
    }
}

/// Per-object appearance material: an explicit base color + PBR-ish scalars, or
/// a named [`MaterialPreset`]. Applied by `material2`. Serialized with
/// `#[serde(default)]` on the object field so pre-material files still load.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ObjectMaterial {
    Preset { preset: MaterialPreset },
    Custom { color: [f32; 3], roughness: f32, metallic: f32 },
}

impl ObjectMaterial {
    /// Resolve to (base_color, roughness, metallic) regardless of the variant.
    pub fn pbr(&self) -> ([f32; 3], f32, f32) {
        match self {
            ObjectMaterial::Preset { preset } => preset.pbr(),
            ObjectMaterial::Custom { color, roughness, metallic } => (*color, *roughness, *metallic),
        }
    }

    /// Base color alone (drives the mesh fill color path).
    pub fn base_color(&self) -> [f32; 3] {
        self.pbr().0
    }
}

/// Name of the layer every document starts with; objects land here unless the
/// current layer was switched.
pub const DEFAULT_LAYER: &str = "default";

fn default_layer() -> String {
    DEFAULT_LAYER.to_string()
}

fn default_visible() -> bool {
    true
}

fn default_lineweight() -> f64 {
    0.18
}

fn is_default_lineweight(v: &f64) -> bool {
    (*v - 0.18).abs() < 1e-9
}

/// Per-layer display style. `color: None` means "use the theme default".
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct LayerStyle {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<[f32; 4]>,
    #[serde(default = "default_visible")]
    pub visible: bool,
    /// Print lineweight in millimetres. Default 0.18 mm (ISO thin).
    #[serde(default = "default_lineweight", skip_serializing_if = "is_default_lineweight")]
    pub lineweight_mm: f64,
    /// Display-order sort key for the Layers panel (lower shows first). Storage
    /// stays alphabetical (BTreeMap); the UI sorts by this then by name. Serde
    /// default 0 keeps pre-order JSON loading (ties break by name, as before).
    #[serde(default, skip_serializing_if = "is_default_order")]
    pub order: i32,
    /// When true, objects on this layer are not selectable/editable (`layerlock`).
    /// Serde default `false` keeps pre-lock JSON loading (old files → unlocked).
    #[serde(default, skip_serializing_if = "is_false")]
    pub locked: bool,
    /// Layer linetype (`layerlinetype`). Serde default `Continuous` keeps
    /// pre-linetype JSON loading (old files have no field → Continuous).
    #[serde(default, skip_serializing_if = "LineType::is_default")]
    pub linetype: LineType,
}

fn is_default_order(o: &i32) -> bool {
    *o == 0
}

fn is_false(b: &bool) -> bool {
    !*b
}

/// Layer linetype pattern. Stored per layer; used by render/export where
/// feasible and shown in the Layers-panel dropdown.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum LineType {
    #[default]
    Continuous,
    Dashed,
    Dotted,
    DashDot,
}

impl LineType {
    fn is_default(&self) -> bool {
        matches!(self, LineType::Continuous)
    }

    /// Canonical lowercase token used by the `layerlinetype` command and serde.
    pub fn token(self) -> &'static str {
        match self {
            LineType::Continuous => "continuous",
            LineType::Dashed => "dashed",
            LineType::Dotted => "dotted",
            LineType::DashDot => "dashdot",
        }
    }

    /// Human-readable label for the Layers-panel dropdown.
    pub fn label(self) -> &'static str {
        match self {
            LineType::Continuous => "Continuous",
            LineType::Dashed => "Dashed",
            LineType::Dotted => "Dotted",
            LineType::DashDot => "DashDot",
        }
    }

    /// Parse a case-insensitive token (`continuous|dashed|dotted|dashdot`).
    pub fn parse_token(s: &str) -> Option<LineType> {
        match s.to_ascii_lowercase().as_str() {
            "continuous" => Some(LineType::Continuous),
            "dashed" => Some(LineType::Dashed),
            "dotted" => Some(LineType::Dotted),
            "dashdot" => Some(LineType::DashDot),
            _ => None,
        }
    }

    /// All variants in display order (for the dropdown).
    pub const ALL: [LineType; 4] =
        [LineType::Continuous, LineType::Dashed, LineType::Dotted, LineType::DashDot];
}

impl Default for LayerStyle {
    fn default() -> Self {
        Self {
            color: None,
            visible: true,
            lineweight_mm: 0.18,
            order: 0,
            locked: false,
            linetype: LineType::Continuous,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct SceneObject {
    pub id: ObjectId,
    pub name: Option<String>,
    /// Serde default keeps pre-layer JSON loading.
    #[serde(default = "default_layer")]
    pub layer: String,
    /// Per-object display flag (`hideobj`/`showobj`); serde default keeps
    /// pre-visibility JSON loading.
    #[serde(default = "default_visible")]
    pub visible: bool,
    /// Per-object override color (RGB 0..1). `None` defers to layer/theme.
    /// Serde default keeps pre-color JSON loading.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<[f32; 3]>,
    /// Per-object appearance material (`material2`): base color + roughness +
    /// metallic, or a named preset. `None` defers to the flat color path.
    /// Serde default keeps pre-material JSON loading.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub material: Option<ObjectMaterial>,
    /// Per-object lineweight override in mm (`lineweight` command). `None`
    /// defers to the layer lineweight. Serde default keeps pre-lineweight
    /// JSON loading (old files have no field → None).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lineweight_mm: Option<f64>,
    pub geometry: Geometry,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn param_block_expand_substitutes_and_resolves() {
        let def = ParamBlockDef {
            params: vec![
                ParamBlockParam { name: "w".into(), default: "0.9".into() },
                ParamBlockParam { name: "h".into(), default: "2.0".into() },
            ],
            body: vec!["rect 0,0,0 {w} {h}".into(), "note {unknown}".into()],
        };
        // Defaults.
        let out = def.expand(&def.default_values());
        assert_eq!(out[0], "rect 0,0,0 0.9 2.0");
        // Unknown token survives verbatim.
        assert_eq!(out[1], "note {unknown}");
        // Override wins; unknown override keys ignored.
        let mut ov = std::collections::BTreeMap::new();
        ov.insert("w".to_string(), "1.5".to_string());
        ov.insert("bogus".to_string(), "9".to_string());
        let vals = def.resolve_values(&ov);
        assert_eq!(vals.get("w").map(String::as_str), Some("1.5"));
        assert_eq!(vals.get("h").map(String::as_str), Some("2.0"));
        assert!(!vals.contains_key("bogus"), "unknown override key dropped");
    }

    /// Pre-dynamic-block instance JSON (no `source`/`params`) must still load,
    /// defaulting to a plain block instance.
    #[test]
    fn pre_dynamic_instance_json_loads() {
        let json = r#"{ "geo": "instance", "block": "tree",
            "position": [1.0, 2.0, 0.0] }"#;
        let g: Geometry = serde_json::from_str(json).unwrap();
        match g {
            Geometry::Instance { source, params, scale, rotation_deg, .. } => {
                assert_eq!(source, None);
                assert!(params.is_empty());
                assert_eq!(scale, 1.0);
                assert_eq!(rotation_deg, 0.0);
            }
            g => panic!("expected instance, got {g:?}"),
        }
    }

    #[test]
    fn material_preset_parse_and_pbr() {
        assert_eq!(MaterialPreset::parse("glass"), Some(MaterialPreset::Glass));
        assert_eq!(MaterialPreset::parse("nope"), None);
        // Presets must be visually distinct: glass is smooth, concrete matte,
        // metal fully metallic.
        let (_, glass_rough, _) = MaterialPreset::Glass.pbr();
        let (_, concrete_rough, _) = MaterialPreset::Concrete.pbr();
        let (_, _, metal_metallic) = MaterialPreset::Metal.pbr();
        assert!(glass_rough < concrete_rough, "glass must be smoother than concrete");
        assert_eq!(metal_metallic, 1.0, "metal must be fully metallic");
    }

    #[test]
    fn object_material_resolves_uniformly() {
        let custom = ObjectMaterial::Custom { color: [0.1, 0.2, 0.3], roughness: 0.7, metallic: 0.4 };
        let (c, r, m) = custom.pbr();
        assert_eq!(c, [0.1, 0.2, 0.3]);
        assert!((r - 0.7).abs() < 1e-6 && (m - 0.4).abs() < 1e-6);
        assert_eq!(custom.base_color(), [0.1, 0.2, 0.3]);
    }

    /// A pre-material SceneObject JSON (no `material` field) must still load,
    /// defaulting the material to None.
    #[test]
    fn pre_material_scene_object_loads() {
        let json = r#"{
            "id": "00000000000000000000000000000001",
            "name": null,
            "layer": "default",
            "visible": true,
            "geometry": { "geo": "points", "positions": [] }
        }"#;
        // Deserialize just the fields we care about via a permissive check: the
        // material field must default to None on an object that omits it.
        let parsed: serde_json::Value = serde_json::from_str(json).unwrap();
        assert!(parsed.get("material").is_none(), "fixture omits material");
        // Round-trip through SceneObject: material defaults to None and a
        // re-serialized object omits the field (skip_serializing_if None).
        let obj: SceneObject = serde_json::from_value(parsed).unwrap();
        assert!(obj.material.is_none(), "missing material defaults to None");
        let back = serde_json::to_value(&obj).unwrap();
        assert!(back.get("material").is_none(), "None material is not serialized");
    }
}
