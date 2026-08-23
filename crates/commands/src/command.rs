use glam::DVec3;
use mydrafter_doc::ObjectId;
use serde::{Deserialize, Serialize};

/// Object selector. `Last(n)` ("last", "last 3") is the workhorse for both the
/// command line and the LLM.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "sel", rename_all = "snake_case")]
pub enum Selector {
    Ids { ids: Vec<ObjectId> },
    Named { name: String },
    Last { n: usize },
    All,
    Selected,
}

/// Mirror plane: a canonical plane through the origin, or point + normal.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "plane", rename_all = "snake_case")]
pub enum MirrorPlane {
    Xy,
    Yz,
    Xz,
    PointNormal { point: DVec3, normal: DVec3 },
}

/// The shared command language. `id`/`ids` fields are `None` when typed or
/// emitted; they are filled at apply time and written back into the logged op
/// so replay reproduces identical ids.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Command {
    // -- 3D --
    Box {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<ObjectId>,
        corner: DVec3,
        size: DVec3,
    },
    /// Extrude a closed profile curve upward.
    Extrude {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<ObjectId>,
        profile: Selector,
        height: f64,
    },
    // -- 2D primitives (create Curve objects) --
    Line {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<ObjectId>,
        a: DVec3,
        b: DVec3,
    },
    Polyline {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<ObjectId>,
        points: Vec<DVec3>,
        closed: bool,
    },
    Rectangle {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<ObjectId>,
        corner: DVec3,
        width: f64,
        height: f64,
    },
    Circle {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<ObjectId>,
        center: DVec3,
        radius: f64,
    },
    Arc {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<ObjectId>,
        center: DVec3,
        radius: f64,
        start_deg: f64,
        end_deg: f64,
    },
    Ellipse {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<ObjectId>,
        center: DVec3,
        rx: f64,
        ry: f64,
    },
    Polygon {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<ObjectId>,
        center: DVec3,
        radius: f64,
        sides: u32,
    },
    /// NURBS curve by control points ("curve" on the command line).
    Curve {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<ObjectId>,
        points: Vec<DVec3>,
        degree: u32,
    },
    // -- booleans (mesh CSG; inputs are consumed, one result mesh replaces them) --
    Union {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<ObjectId>,
        targets: Selector,
    },
    Difference {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<ObjectId>,
        target: Selector,
        tools: Selector,
    },
    Intersect {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<ObjectId>,
        targets: Selector,
    },
    // -- edit --
    Move {
        targets: Selector,
        delta: DVec3,
    },
    /// Rotate about an axis through `center` (default: targets' AABB center).
    Rotate {
        targets: Selector,
        angle_deg: f64,
        axis: DVec3,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        center: Option<DVec3>,
    },
    /// Scale by per-axis factors about `center` (default: targets' AABB center).
    Scale {
        targets: Selector,
        factors: DVec3,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        center: Option<DVec3>,
    },
    Mirror {
        targets: Selector,
        plane: MirrorPlane,
    },
    /// Offset a curve in the XY plane; the original is kept.
    Offset {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<ObjectId>,
        target: Selector,
        distance: f64,
    },
    Copy {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ids: Option<Vec<ObjectId>>,
        targets: Selector,
        delta: DVec3,
    },
    Delete {
        targets: Selector,
    },
    Name {
        targets: Selector,
        name: String,
    },
    Select {
        targets: Selector,
    },
    SelectNone,
    Undo,
    Redo,
}

impl Command {
    /// Commands that mutate geometry are logged; view/undo commands are not.
    pub fn is_logged(&self) -> bool {
        !matches!(
            self,
            Command::Select { .. } | Command::SelectNone | Command::Undo | Command::Redo
        )
    }
}
