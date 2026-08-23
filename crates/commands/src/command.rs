use glam::DVec3;
use mydrafter_doc::{HatchPattern, ObjectId, PaperSize, Units, ViewDirection};
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
    // -- drafting (dimensions, notes, hatches) --
    /// Linear dimension between two points; the measured value is derived at
    /// display time, never stored.
    Dim {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<ObjectId>,
        a: DVec3,
        b: DVec3,
        offset: f64,
    },
    Text {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<ObjectId>,
        pos: DVec3,
        text: String,
        height: f64,
    },
    /// Hatch the region bounded by a closed curve.
    Hatch {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<ObjectId>,
        target: Selector,
        pattern: HatchPattern,
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
    /// Split a curve at the nearest point on it to `point`; the original is
    /// replaced by the pieces.
    Split {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ids: Option<Vec<ObjectId>>,
        target: Selector,
        point: DVec3,
    },
    /// Trim a curve at its intersections with cutter curves, keeping only the
    /// piece nearest `keep`; the rest is removed.
    Trim {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<ObjectId>,
        target: Selector,
        cutter: Selector,
        keep: DVec3,
    },
    /// Extend both open ends of curves tangentially by `distance`.
    Extend {
        targets: Selector,
        distance: f64,
    },
    /// Join end-touching curves into one polyline; inputs are consumed.
    Join {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<ObjectId>,
        targets: Selector,
    },
    /// Fillet two lines with a tangent arc, trimming both to tangency.
    Fillet {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<ObjectId>,
        a: Selector,
        b: Selector,
        radius: f64,
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
    // -- layers --
    /// Create (if needed) and switch the current layer for new objects.
    Layer {
        name: String,
    },
    /// Move objects onto a layer (created if needed).
    ToLayer {
        targets: Selector,
        layer: String,
    },
    /// Set a layer's display color (rgb 0..1).
    LayerColor {
        layer: String,
        color: [f32; 3],
    },
    Hide {
        layer: String,
    },
    Show {
        layer: String,
    },
    /// Set the document display unit. Logged so replayed files keep their
    /// unit; geometry always stores meters regardless.
    Units {
        units: Units,
    },
    // -- sheets / layouts --
    /// Create a named paper sheet (landscape).
    Sheet {
        name: String,
        paper: PaperSize,
    },
    /// Add a scaled ortho view to a sheet. `scale` is the denominator (1:100 -> 100).
    SheetView {
        sheet: String,
        direction: ViewDirection,
        scale: f64,
    },
    /// Export a sheet as a vector PDF. Not logged: printing is I/O, not model state.
    Print {
        sheet: String,
        path: String,
    },
    /// Export the whole document as DXF R12. Not logged, like Print.
    Export {
        path: String,
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
            Command::Select { .. }
                | Command::SelectNone
                | Command::Print { .. }
                | Command::Export { .. }
                | Command::Undo
                | Command::Redo
        )
    }
}
