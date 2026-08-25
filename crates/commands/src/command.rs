// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

use glam::DVec3;
use itsjustcad_doc::{
    AreaKind, FrameKind, HatchPattern, NamedView, ObjectId, PaperSize,
    RestraintKind, Section as StructSection, Units, ViewDirection,
};
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

/// Compass direction naming an elevation view. `North` names the elevation you
/// see standing to the north looking south (i.e. the building's north face).
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CompassDir {
    North,
    South,
    East,
    West,
}

impl std::fmt::Display for CompassDir {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            CompassDir::North => "north",
            CompassDir::South => "south",
            CompassDir::East => "east",
            CompassDir::West => "west",
        };
        f.write_str(s)
    }
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
    /// Revolve a closed profile curve about an axis (default: z axis through
    /// the origin, full circle). Partial angles are capped.
    Revolve {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<ObjectId>,
        profile: Selector,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        axis_point: Option<DVec3>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        axis_dir: Option<DVec3>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        angle_deg: Option<f64>,
    },
    /// Skin 2+ closed curves (in creation order) into one capped solid.
    Loft {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<ObjectId>,
        targets: Selector,
    },
    /// Sweep a closed profile curve along an open rail curve, capped.
    Sweep {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<ObjectId>,
        profile: Selector,
        rail: Selector,
    },
    /// Sweep a closed profile between two open rails, lofting it so its ends
    /// track both rails at every station. Capped, watertight.
    Sweep2 {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<ObjectId>,
        profile: Selector,
        rail_a: Selector,
        rail_b: Selector,
    },
    /// Revolve a closed profile about an axis where the radius follows a rail
    /// curve (nearest-angle radius sampling). Full turn.
    RailRevolve {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<ObjectId>,
        profile: Selector,
        rail: Selector,
        axis_point: DVec3,
        axis_dir: DVec3,
    },
    /// Sweep a circular profile along a curve with a linearly interpolated
    /// radius (variable-radius pipe). Capped, watertight.
    Pipe {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<ObjectId>,
        curve: Selector,
        radius: f64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        end_radius: Option<f64>,
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
    /// C2 cubic curve interpolating the given points exactly. Append "closed"
    /// to make a periodic loop.
    InterpCurve {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<ObjectId>,
        points: Vec<DVec3>,
        closed: bool,
    },
    /// 3D helix about +Z through `center` (dense polyline).
    Helix {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<ObjectId>,
        center: DVec3,
        radius: f64,
        height: f64,
        turns: f64,
    },
    /// Move one control/vertex point of a NURBS or Polyline curve.
    SetPoint {
        target: Selector,
        index: u32,
        position: DVec3,
    },
    /// Resample a curve to `count` points (open/closed matching the source).
    Rebuild {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<ObjectId>,
        target: Selector,
        count: u32,
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
    // -- sections (mesh/plane cuts -> polylines on layer "sections") --
    /// Cut meshes with a plane; each closed intersection loop becomes a
    /// closed polyline on layer "sections".
    Section {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ids: Option<Vec<ObjectId>>,
        targets: Selector,
        point: DVec3,
        normal: DVec3,
    },
    /// Horizontal section of every mesh at z = height (the plan cut).
    Plan {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ids: Option<Vec<ObjectId>>,
        height: f64,
    },
    /// Orthographic side-view outline: feature edges of every mesh projected
    /// onto the vertical plane for the named compass direction, on layer
    /// "elevations". `depth` offsets the projection plane outward (default 0).
    Elevation {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ids: Option<Vec<ObjectId>>,
        direction: CompassDir,
        #[serde(default)]
        depth: f64,
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
    /// Rectangular grid of copies; the originals occupy cell (0,0,0).
    Array {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ids: Option<Vec<ObjectId>>,
        targets: Selector,
        counts: [u32; 3],
        delta: DVec3,
    },
    /// Circular array of copies about the z axis through `center` (default:
    /// targets' AABB center). Default sweep is a full circle.
    PolarArray {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ids: Option<Vec<ObjectId>>,
        targets: Selector,
        count: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        center: Option<DVec3>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        total_angle_deg: Option<f64>,
    },
    Delete {
        targets: Selector,
    },
    Name {
        targets: Selector,
        name: String,
    },
    // -- groups (named id-sets; pick and selectors expand to the whole group) --
    /// Group objects under a name. `name` is `None` when typed without one;
    /// exec fills a generated name and writes it back for replay.
    Group {
        targets: Selector,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },
    /// Dissolve every group containing the selected objects (objects stay).
    Ungroup {
        targets: Selector,
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
    /// Set a layer's print lineweight in millimetres.
    LayerWeight {
        layer: String,
        mm: f64,
    },
    Hide {
        layer: String,
    },
    Show {
        layer: String,
    },
    // -- per-object visibility --
    /// Hide individual objects (they stay in the model and the op-log).
    HideObj {
        targets: Selector,
    },
    ShowObj {
        targets: Selector,
    },
    // -- per-object color --
    /// Set a per-object color override (RGB 0..1 or 0..255). Beats layer color.
    Color {
        targets: Selector,
        color: [f32; 3],
    },
    /// Clear per-object color override (reverts to layer/theme color).
    ColorOff {
        targets: Selector,
    },
    /// Set a per-object appearance material: a named preset (concrete/glass/
    /// metal/wood) or explicit base color + roughness + metallic. Distinct verb
    /// `material2` (not `material`) because `material` is the structural material
    /// definition (E + density). This one is render-only appearance.
    Material2 {
        targets: Selector,
        material: itsjustcad_doc::ObjectMaterial,
    },
    /// Clear a per-object appearance material (reverts to the flat color path).
    Material2Off {
        targets: Selector,
    },
    /// Set the document display unit. Logged so replayed files keep their
    /// unit; geometry always stores meters regardless.
    Units {
        units: Units,
    },
    /// Set the document solar position (azimuth + altitude from NOAA SPA).
    /// Logged so saved files replay with identical lighting. `None` reverts to
    /// headlight-only shading. The renderer picks this up on the next frame.
    Sun {
        /// Azimuth clockwise from North, degrees [0, 360).
        azimuth_deg: f64,
        /// Altitude above the horizon, degrees.
        altitude_deg: f64,
        /// Observer latitude/longitude the position was computed for. Recorded
        /// as the document location so `shadowstudy`/`sunhours` can reuse it.
        /// `#[serde(default)]` keeps old logs (which lacked these) loading — they
        /// replay to lat/lon 0 and simply leave no usable location for analyses.
        #[serde(default)]
        lat_deg: f64,
        #[serde(default)]
        lon_deg: f64,
    },
    /// Remove the solar position (revert to headlight shading).
    SunOff,
    /// Record the observer location (lat/lon/tz) on the document. Set by the
    /// `sun` command and by EPW import; required by `shadowstudy`/`sunhours`.
    /// Logged so saved files replay the location.
    Location {
        /// Latitude, degrees (north positive).
        lat_deg: f64,
        /// Longitude, degrees (east positive).
        lon_deg: f64,
        /// Time-zone offset from UTC in hours (east positive).
        #[serde(default)]
        tz_hours: f64,
    },
    /// Ground shadow study: for each time step across a day, project every mesh
    /// silhouette onto the ground (`z=0`) along the sun direction and emit the
    /// projected convex hull as a closed polygon on a per-time `shadows-HH:MM`
    /// layer. Requires a document location (set via `sun` or EPW import).
    ShadowStudy {
        /// Ids of the created shadow polygons, filled in on first exec and
        /// reused on replay so the op-log reproduces identical objects.
        #[serde(default)]
        ids: Option<Vec<ObjectId>>,
        /// Date the sun positions are computed for.
        year: i32,
        month: u32,
        day: u32,
        /// Inclusive start / end local clock times, minutes past midnight.
        from_min: u32,
        to_min: u32,
        /// Step between stamps, minutes (> 0).
        step_min: u32,
    },
    /// Sunlight-hours heatmap: sample a ground grid over the scene bbox, ray-cast
    /// toward the sun every 30 min of the day, and emit a colored mesh overlay on
    /// the `analysis` layer (blue = few hours, red = most). Requires a location.
    SunHours {
        /// Id of the created heatmap mesh, filled in on first exec and reused on
        /// replay for op-log stability.
        #[serde(default)]
        ids: Option<Vec<ObjectId>>,
        year: i32,
        month: u32,
        day: u32,
        /// Grid spacing in meters (> 0).
        spacing: f64,
    },
    // -- underlay (raster reference image on the ground plane) --
    /// Place a raster image (PNG) on the ground plane. `height` is `None` when
    /// typed/emitted; exec fills it from the image's aspect ratio (width /
    /// aspect) and writes it back into the logged op, so replay reproduces the
    /// same placement even if the file later goes missing.
    Underlay {
        path: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        corner: Option<DVec3>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        width: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        height: Option<f64>,
    },
    /// Set the underlay's blend opacity (0..1).
    UnderlayOpacity {
        opacity: f32,
    },
    /// Remove the underlay.
    UnderlayOff,
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
    /// Export the whole document as DXF R12, SVG, CSV, or 3D mesh format.
    /// Not logged: export is I/O, not model state.
    Export {
        path: String,
    },
    /// Write three control images from the current view for hand-off to a
    /// diffusion / image-editing tool: `<prefix>_depth.png` (near/far depth
    /// gradient), `<prefix>_edge.png` (feature-edge linework), and
    /// `<prefix>_mask.png` (a flat semantic color per layer). Requires the GPU
    /// view, so the app/headless runner performs the render; not logged (I/O).
    ControlImages {
        prefix: String,
    },
    /// Import a DXF file: each supported entity becomes its own logged
    /// substrate op (Line/Polyline/Circle/Arc/Text, plus Layer switches), so
    /// the op-log — not the DXF file — is the record. Import itself is never
    /// logged; replaying a saved file needs no access to the imported DXF.
    /// Mesh imports (.obj/.stl/.gltf/.glb) use MeshLiteral ops instead.
    Import {
        path: String,
    },
    /// Build a terrain surface mesh from a file and add it on layer "terrain".
    /// `.csv` → Delaunay-triangulate x,y,z survey points; `.geojson` →
    /// triangulate the vertices of elevation contour LineStrings. Expands into a
    /// single MeshLiteral op (self-contained); the Terrain op itself is not
    /// logged, exactly like Import.
    Terrain {
        path: String,
    },
    /// Build OSM building context from a saved Overpass API JSON export: each
    /// building footprint way is extruded (height tag or 9 m default) into a
    /// MeshLiteral op on layer "context". Not logged (its expansions are).
    OsmFile {
        path: String,
    },
    /// A raw triangle mesh carried verbatim in the op-log. Used by mesh import
    /// (.obj/.stl/.gltf/.glb) so each imported object is one self-contained
    /// logged op — no external file dependency on replay. Not exposed in the
    /// registry (internal: suppress from LLM prompt generation).
    MeshLiteral {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<ObjectId>,
        positions: Vec<DVec3>,
        faces: Vec<[u32; 3]>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },
    /// Decimated point cloud carried verbatim in the op-log. Produced by LAS
    /// import; self-contained so replay needs no external file. Not exposed in
    /// the registry.
    PointLiteral {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<ObjectId>,
        positions: Vec<DVec3>,
    },
    // -- named views --
    /// Save the active viewport camera under a name. `camera` is `None` when
    /// typed; the app fills it before apply and it is written back into the
    /// logged op, so replay restores identical views.
    ViewSave {
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        camera: Option<NamedView>,
    },
    /// Restore a named view into the active viewport. Display-only: not logged.
    ViewRestore {
        name: String,
    },
    /// List saved views (query, never logged).
    ViewList,
    Select {
        targets: Selector,
    },
    SelectNone,
    // -- measure (queries; read-only, never logged) --
    /// Distance between two points, reported in the document unit.
    Distance {
        a: DVec3,
        b: DVec3,
    },
    /// Area of closed curves (shoelace) and mesh surfaces (summed faces).
    Area {
        targets: Selector,
    },
    /// Signed volume of closed meshes.
    Volume {
        targets: Selector,
    },
    /// Combined axis-aligned bounding box of the targets.
    Bbox {
        targets: Selector,
    },
    /// Print a schedule table (name/id/layer/type/area/volume) to the command
    /// line, grouped by name. Query only; never logged.
    Schedule {
        /// Optional layer filter; `None` means all layers.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        layer: Option<String>,
    },
    /// Place a schedule table on a sheet (logged). The table is written into
    /// the PDF at print time; no geometry is created in the 3D scene.
    SheetTable {
        sheet: String,
        /// Optional layer filter; `None` means all layers.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        layer: Option<String>,
    },
    /// Add a paper-space dimension to a sheet (logged). `a` and `b` are paper
    /// coordinates in mm; `offset` is the perpendicular dim-line offset in mm.
    /// The numeric label is derived at PDF time from the model distance via the
    /// view scale, so it always agrees with the geometry.
    SheetDim {
        sheet: String,
        a: [f64; 2],
        b: [f64; 2],
        #[serde(default, skip_serializing_if = "Option::is_none")]
        offset: Option<f64>,
        /// Which sheet view index to use for paper→model conversion (0-based).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        view_index: Option<usize>,
    },
    // -- blocks (reusable geometry definitions + instancing) --
    /// Capture the geometry of selected objects as a named block definition.
    /// The source objects remain in the scene; the definition is a snapshot.
    /// `geometries` is `None` when typed; exec fills it and writes back for
    /// replay, so saved files are self-contained (no live object dependency).
    BlockDefine {
        targets: Selector,
        name: String,
        /// Geometry snapshots filled at apply time for replay.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        geometries: Option<Vec<itsjustcad_doc::BlockGeometry>>,
    },
    /// Place an instance of a block definition at a point.
    BlockInsert {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<ObjectId>,
        name: String,
        position: DVec3,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        rotation_deg: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        scale: Option<f64>,
    },
    /// List block definitions (query; never logged).
    BlocksList,
    // -- block content library (.block.json on disk) --
    /// List block names available in ~/.config/itsjustcad/blocks/ (query; never logged).
    BlockLibList,
    /// Load a library block into the document as a named block definition.
    /// `geometries` is `None` when typed; exec fills it for replay.
    BlockLibLoad {
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        geometries: Option<Vec<itsjustcad_doc::BlockGeometry>>,
    },
    /// Write the current block definition `name` from the document back to the
    /// library as `~/.config/itsjustcad/blocks/<name>.block.json`.
    BlockLibSave {
        name: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        description: String,
    },
    // -- structure (grids, stories, sections, materials, frame/area members) --
    /// Define (or replace) a named structural section (profile). Stored on the
    /// document; referenced by frame members.
    DefSection {
        name: String,
        section: StructSection,
    },
    /// Define (or replace) a named structural material. Stored, never analyzed.
    DefMaterial {
        name: String,
        elastic_modulus_e: f64,
        density: f64,
    },
    /// Define (or replace) a named reference grid: labeled X/Y axes at fixed
    /// coordinates, plus optional level lines. Rendered as reference geometry.
    DefGrid {
        name: String,
        x_axes: Vec<(String, f64)>,
        y_axes: Vec<(String, f64)>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        levels: Vec<f64>,
    },
    /// Define (or replace) a building story/level by name and elevation.
    DefStory {
        name: String,
        elevation: f64,
    },
    /// Frame member (beam or column): a line member with a named section swept
    /// along it, rolled by `orientation_deg`. `beam` and `column` are two verbs
    /// mapping to this one command; the `kind` disambiguates. `id` is filled at
    /// apply time and written back for replay stability.
    FrameMember {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<ObjectId>,
        kind: FrameKind,
        a: DVec3,
        b: DVec3,
        section: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        material: Option<String>,
        #[serde(default, skip_serializing_if = "is_zero_opt")]
        orientation_deg: Option<f64>,
    },
    /// Area member (slab or wall): a closed boundary extruded by `thickness`.
    /// `slab` extrudes vertically (+Z), `wall` extrudes along its in-plane
    /// normal. `id` is filled at apply time and written back for replay.
    AreaMember {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<ObjectId>,
        kind: AreaKind,
        boundary: Vec<DVec3>,
        thickness: f64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        material: Option<String>,
    },
    // -- structural loads & supports (data + light viz; no analysis) --
    /// Add a structural load to the document. Three geometry kinds: `point`
    /// (concentrated force at a node), `line` (distributed along a member),
    /// `area` (pressure on a surface). Magnitude is in SI units (N / N/m / Pa).
    /// Direction is a normalised world-space vector. Not a SceneObject — the
    /// load is stored in `doc.loads` and drawn as a 2D overlay arrow.
    AddLoad {
        /// Human label (e.g. "dead", "live-floor").
        name: String,
        /// Load geometry kind and location.
        geometry: itsjustcad_doc::LoadGeometry,
        /// Magnitude in N (point), N/m (line) or Pa (area).
        magnitude: f64,
        /// World-space force direction (stored normalised).
        direction: DVec3,
        /// Index into `doc.loads` filled on first exec; used for replay
        /// stability so undo removes the exact same entry.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        index: Option<usize>,
    },
    /// Add a support / boundary condition to the document. Stored in
    /// `doc.supports` and drawn as a 2D overlay symbol. Not a SceneObject.
    AddSupport {
        /// World-space position.
        position: DVec3,
        /// Restraint type.
        kind: RestraintKind,
        /// Free-translation axis for roller supports (ignored otherwise).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        roller_axis: Option<DVec3>,
        /// Index into `doc.supports` filled on first exec; replay stability.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        index: Option<usize>,
    },
    Undo,
    Redo,
    /// Rewrite history: replace the logged op at `step` (0-based) and rebuild
    /// the document by replaying the whole log. Never itself logged — the
    /// edited log IS the record.
    Amend {
        step: usize,
        with: Box<Command>,
    },
    /// Design options: named branches of the op-log. Meta-level, like Undo —
    /// mutates the session's branch table (and may replay), never itself logged.
    Option(OptionOp),
}

/// The four `option` sub-commands. See [`crate::exec::Session::option`] for the
/// switching/auto-save semantics.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum OptionOp {
    /// Snapshot the current effective log as branch `name` and make it current.
    Save { name: String },
    /// Switch to branch `name`: auto-save current work to the current branch if
    /// it diverged, then replay `name`.
    Switch { name: String },
    /// List branch names (marks the current one).
    List,
    /// Delete branch `name` (never the current branch).
    Delete { name: String },
}

fn is_zero_opt(v: &Option<f64>) -> bool {
    v.is_none_or(|x| x.abs() < 1e-12)
}

impl Command {
    /// Commands that mutate geometry are logged; view/undo commands are not.
    pub fn is_logged(&self) -> bool {
        !matches!(
            self,
            Command::Select { .. }
                | Command::SelectNone
                | Command::ViewRestore { .. }
                | Command::ViewList
                | Command::Print { .. }
                | Command::Export { .. }
                | Command::ControlImages { .. }
                | Command::Import { .. }
                | Command::Terrain { .. }
                | Command::OsmFile { .. }
                | Command::Distance { .. }
                | Command::Area { .. }
                | Command::Volume { .. }
                | Command::Bbox { .. }
                | Command::Schedule { .. }
                | Command::BlocksList
                | Command::BlockLibList
                | Command::Undo
                | Command::Redo
                | Command::Amend { .. }
                | Command::Option(..)
        )
    }

    /// Commands that reach outside the in-memory document — filesystem reads /
    /// writes, subprocesses, or network — as opposed to pure geometry edits.
    ///
    /// The deck (LLM) may auto-run pure ops, but side-effecting ops it emits
    /// must be confirmed by a human first (security C-2 / H-7). A human typing
    /// the same command at the command line is unaffected; this classifier only
    /// gates *deck-originated* execution.
    ///
    /// If the file grows a new fs/net/subprocess command, add it here — the
    /// default is "pure" so an omission fails open, but the unit tests below
    /// pin the current side-effecting set.
    pub fn is_side_effecting(&self) -> bool {
        matches!(
            self,
            // fs writes
            Command::Export { .. }
                | Command::ControlImages { .. }
                | Command::Print { .. }
                | Command::BlockLibSave { .. }
                // fs reads (can be probed to exfiltrate / DoS)
                | Command::Import { .. }
                | Command::Terrain { .. }
                | Command::OsmFile { .. }
                | Command::Underlay { .. }
                | Command::BlockLibLoad { .. }
        )
    }

    /// A short human-readable path/target for the confirm affordance, when the
    /// command touches the filesystem. `None` for pure ops.
    pub fn side_effect_summary(&self) -> Option<String> {
        match self {
            Command::Export { path } => Some(format!("export → {path}")),
            Command::ControlImages { prefix } => {
                Some(format!("control images → {prefix}_[depth|edge|mask].png"))
            }
            Command::Print { path, sheet } => Some(format!("print sheet '{sheet}' → {path}")),
            Command::Import { path } => Some(format!("import ← {path}")),
            Command::Terrain { path } => Some(format!("terrain ← {path}")),
            Command::OsmFile { path } => Some(format!("osm ← {path}")),
            Command::Underlay { path, .. } => Some(format!("underlay ← {path}")),
            Command::BlockLibLoad { name, .. } => {
                Some(format!("blockload ← library:{name}"))
            }
            Command::BlockLibSave { name, .. } => {
                Some(format!("blocksave → library:{name}"))
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod classify_tests {
    use super::*;

    fn sel() -> Selector {
        Selector::Last { n: 1 }
    }

    #[test]
    fn fs_touching_commands_are_side_effecting() {
        let cases: Vec<Command> = vec![
            Command::Export { path: "/tmp/x.dxf".into() },
            Command::Print { sheet: "S1".into(), path: "/tmp/x.pdf".into() },
            Command::Import { path: "/etc/passwd".into() },
            Command::Terrain { path: "/tmp/t.csv".into() },
            Command::OsmFile { path: "/tmp/o.json".into() },
            Command::Underlay { path: "/tmp/p.png".into(), corner: None, width: None, height: None },
        ];
        for c in &cases {
            assert!(c.is_side_effecting(), "{c:?} must be side-effecting");
            assert!(
                c.side_effect_summary().is_some(),
                "{c:?} must carry a summary path"
            );
        }
    }

    #[test]
    fn pure_geometry_ops_are_not_side_effecting() {
        let cases: Vec<Command> = vec![
            Command::Box { id: None, corner: DVec3::ZERO, size: DVec3::ONE },
            Command::Move { targets: sel(), delta: DVec3::X },
            Command::Union { id: None, targets: sel() },
            Command::Difference { id: None, target: sel(), tools: sel() },
            Command::Delete { targets: sel() },
            Command::Extrude { id: None, profile: sel(), height: 3.0 },
            Command::Undo,
            Command::Redo,
        ];
        for c in &cases {
            assert!(!c.is_side_effecting(), "{c:?} must be pure");
            assert!(c.side_effect_summary().is_none(), "{c:?} has no fs path");
        }
    }
}
