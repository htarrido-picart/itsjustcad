use glam::DVec3;
use kernel_curve::Curve;
use kernel_mesh::{Aabb, Mesh};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

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
    Instance {
        block: String,
        position: DVec3,
        #[serde(default, skip_serializing_if = "is_zero")]
        rotation_deg: f64,
        #[serde(default = "one", skip_serializing_if = "is_one")]
        scale: f64,
    },
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
        }
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
}

impl Default for LayerStyle {
    fn default() -> Self {
        Self { color: None, visible: true, lineweight_mm: 0.18 }
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
    pub geometry: Geometry,
}
