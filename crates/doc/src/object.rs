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

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "geo", rename_all = "snake_case")]
pub enum Geometry {
    Mesh(Mesh),
    Curve(Curve),
    Annotation(Annotation),
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
                        if let HatchPattern::Lines { spacing, .. } = pattern {
                            *spacing *= s;
                        }
                    }
                }
                true
            }
        }
    }

    pub fn aabb(&self) -> Aabb {
        match self {
            Geometry::Mesh(m) => m.aabb(),
            Geometry::Curve(c) => Aabb::from_points(c.points_bound()),
            Geometry::Annotation(a) => Aabb::from_points(a.points()),
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
    pub geometry: Geometry,
}
