// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! Document model: pure scene state. The op-log and undo live in the
//! `commands` crate (which depends on this one); the document knows nothing
//! about how it is mutated.

mod constraint;
mod cplane;
mod document;
pub mod hatch;
pub mod hershey;
pub mod loads;
mod object;
pub mod param_schema;
mod plotstyle;
mod sheet;
mod structure;
mod underlay;
mod units;
mod view;

pub use constraint::{PointRef, SketchConstraint};
pub use cplane::CPlane;
pub use document::Document;
pub use loads::{LoadGeometry, RestraintKind, StructLoad, StructSupport};
pub use object::{
    angle_degrees, angular_arc_points, Annotation, AreaKind, BlockGeometry, ClipRect, DimAnchor,
    EndpointRef, FieldExpr, FrameKind, Geometry,
    HatchPattern, LayerStyle, LineType, MaterialPreset, ObjectId, ObjectMaterial, ParamBlockDef,
    ParamBlockParam, SceneObject, DEFAULT_LAYER,
};
pub use param_schema::{
    derive_mesh, param_summary, DeriveError, FieldKind, GeneratorKind, ParamField, ParamMap,
    ParamSchema, ParamValue, Unit, Widget,
};
pub use plotstyle::{PlotStyleEntry, PlotStyleTable, ResolvedPen};
pub use sheet::{
    PaperSize, ScheduleRow, Sheet, SheetDim, SheetLeader, SheetSet, SheetTable, SheetTag,
    SheetText, SheetView, TagShape, ViewDirection,
};
pub use structure::{Grid, Material, Room, Section, Story};
pub use underlay::{Basemap, Underlay};
pub use units::{
    format_angle, format_area, format_length, format_volume, Units, METERS_PER_FOOT,
    METERS_PER_INCH,
};
pub use view::{NamedView, PanoView};

use serde::{Deserialize, Serialize};

/// Solar position recorded by the `sun` command. Stored as azimuth + altitude
/// (NOAA simplified SPA output) so the value is self-contained in the op-log.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct SunPosition {
    /// Clockwise from North, degrees [0, 360).
    pub azimuth_deg: f64,
    /// Above the horizon, degrees (negative = below horizon).
    pub altitude_deg: f64,
}

/// Observer location on Earth, recorded by the `sun` command or an EPW import.
/// Needed by environmental analyses (`shadowstudy`, `sunhours`) to recompute
/// sun positions over a day. Stored in the op-log via a `location` op so saved
/// files replay identically.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct GeoLocation {
    /// Latitude, degrees (north positive).
    pub lat_deg: f64,
    /// Longitude, degrees (east positive).
    pub lon_deg: f64,
    /// Time-zone offset from UTC in hours (east positive). Sun-position math is
    /// UTC-based; this lets analyses interpret local clock times on a date.
    pub tz_hours: f64,
}

/// One sampled datum kept inside an [`AnalysisReport`] so the deck LLM can
/// point AT a place ("the north face at (0, 10, 2) gets 0.5 h"). Only the
/// extreme few survive — never the raw per-face/per-cell soup.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AnalysisSample {
    /// Value in the report's `unit`.
    pub value: f64,
    /// Sample location, meters (face centroid / grid-cell center /
    /// shadow-polygon centroid).
    pub at: [f64; 3],
    /// What the sample IS: the facing of the sampled surface ("up", "down",
    /// "north", "southwest", …) for face/cell analyses, or the "HH:MM" time
    /// stamp for shadow-study polygons.
    pub tag: String,
}

/// Compact structured summary of one environmental analysis run (`sunhours`,
/// `facesunhours`, `radiation`, `shadowstudy`), stored on the document keyed
/// by kind and served by the read-only `report` command so the deck LLM can
/// critique results (token-frugal: stats + bins + extreme samples, never raw
/// data). Regenerated whenever the analysis re-runs, including op-log replay;
/// an `undo` of the analysis leaves the last report in place (it describes the
/// last run, not live geometry).
/// One rule's outcome inside a [`ComplianceReport`]: the verdict plus the
/// numbers that ground it (measured vs required, violating object ids and
/// locations) so the deck LLM can critique with citations ("stair S2 riser
/// 0.21 m > max 0.178 m").
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RuleOutcome {
    /// Rule id from the pack (e.g. "ramp-slope").
    pub rule_id: String,
    /// The code section the rule cites (e.g. "ADA 405.2"). Informational.
    pub code_ref: String,
    /// Declared severity: "error" | "warn" | "info".
    pub severity: String,
    /// "pass" when no target violated the rule, otherwise "fail"/"warn"/"info"
    /// per the declared severity.
    pub verdict: String,
    /// The rule's message template (what a violation means).
    pub message: String,
    /// Short ids of the violating objects (empty on pass).
    pub objects: Vec<String>,
    /// Violation locations, meters (marker positions).
    pub locations: Vec<[f64; 3]>,
    /// Worst measured value across targets (`None` when nothing measurable
    /// matched the rule's target query).
    pub measured: Option<f64>,
    /// The rule's threshold, same unit as `measured`.
    pub required: Option<f64>,
    /// Unit of `measured`/`required` ("rise/run", "m", "count").
    pub unit: String,
    /// How many objects the rule was evaluated against.
    pub checked: usize,
}

/// Structured result of one `codecheck` run, stored on the document keyed by
/// pack name and served by the read-only `report` command (same plumbing as
/// [`AnalysisReport`]). ADVISORY ONLY: a geometric pre-check, never a code
/// review — every rendering of this report carries that disclaimer.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ComplianceReport {
    /// Check-pack name ("demo", "ibc-2021", …).
    pub pack: String,
    /// Human context for the run (story filter, rule count).
    pub context: String,
    /// Per-rule outcomes, in pack order.
    pub rules: Vec<RuleOutcome>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AnalysisReport {
    /// Analysis verb: "sunhours" | "facesunhours" | "radiation" | "shadowstudy".
    pub kind: String,
    /// Human context for the run: date, time window, EPW file.
    pub context: String,
    /// Unit of every value in this report ("h", "kWh/m2-yr", "m2").
    pub unit: String,
    /// Number of samples (faces / grid cells / shadow polygons).
    pub count: usize,
    pub min: f64,
    pub avg: f64,
    pub max: f64,
    /// Distribution: six equal-width bins spanning [0, max], as
    /// (inclusive upper bound, sample count). Empty when `max` is 0.
    pub bins: Vec<(f64, usize)>,
    /// The lowest-value samples, ascending (worst-lit faces, darkest cells).
    pub lowest: Vec<AnalysisSample>,
    /// The highest-value samples, descending (sunniest/hottest faces,
    /// largest shadows).
    pub highest: Vec<AnalysisSample>,
}
