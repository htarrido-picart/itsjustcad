// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

use serde::{Deserialize, Serialize};

/// ISO A-series paper, always used landscape for drawing sheets.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PaperSize {
    A4,
    A3,
    A2,
    A1,
    A0,
}

impl PaperSize {
    /// Landscape (width, height) in millimeters.
    pub fn landscape_mm(self) -> (f64, f64) {
        match self {
            PaperSize::A4 => (297.0, 210.0),
            PaperSize::A3 => (420.0, 297.0),
            PaperSize::A2 => (594.0, 420.0),
            PaperSize::A1 => (841.0, 594.0),
            PaperSize::A0 => (1189.0, 841.0),
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            PaperSize::A4 => "a4",
            PaperSize::A3 => "a3",
            PaperSize::A2 => "a2",
            PaperSize::A1 => "a1",
            PaperSize::A0 => "a0",
        }
    }
}

/// Camera direction for a sheet viewport. All projections are orthographic;
/// `Iso` is a 30° axonometric ("persp" on the command line).
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ViewDirection {
    Top,
    Front,
    Right,
    Iso,
}

impl ViewDirection {
    pub fn label(self) -> &'static str {
        match self {
            ViewDirection::Top => "top",
            ViewDirection::Front => "front",
            ViewDirection::Right => "right",
            ViewDirection::Iso => "persp",
        }
    }
}

/// One viewport on a sheet: a direction at a drawing scale. `scale` is the
/// denominator — 100 means 1:100. Viewport rectangles are laid out at print
/// time from the view count, so the stored state stays minimal.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
pub struct SheetView {
    pub direction: ViewDirection,
    pub scale: f64,
}

/// One row in a material/object schedule table.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ScheduleRow {
    pub id: String,
    pub name: String,
    pub layer: String,
    pub kind: String,
    /// Footprint area in m² (closed curves: XY shoelace; meshes: surface area).
    pub area_m2: f64,
    /// Signed volume in m³; 0.0 for curves and annotations.
    pub volume_m3: f64,
}

/// A schedule table placed on a sheet.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct SheetTable {
    /// Optional layer filter; `None` means all layers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layer: Option<String>,
    /// Cached rows (built at place time, regenerated on replay via exec).
    pub rows: Vec<ScheduleRow>,
}

/// A paper-space linear dimension: two paper-space anchor points (in mm,
/// from the sheet lower-left) and a dim-line offset (mm, perpendicular to
/// the measurement direction). The numeric label is derived at PDF time from
/// the model distance so it always matches the geometry.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct SheetDim {
    /// First anchor in paper space (mm).
    pub a_mm: [f64; 2],
    /// Second anchor in paper space (mm).
    pub b_mm: [f64; 2],
    /// Perpendicular offset of the dimension line from a→b (mm).
    pub offset_mm: f64,
    /// The view whose scale converts paper→model for the label value.
    pub view_index: usize,
}

/// A paper-space text note placed on a sheet. `pos_mm` is the lower-left of
/// the text in paper millimeters (from the sheet lower-left); `height_mm` is
/// the cap height IN MILLIMETERS ON PAPER. Because the size is stated in paper
/// mm — never model units — the note renders at exactly the same physical size
/// regardless of any viewport's scale. This is the scale-independence fix that
/// model-space `Annotation::Text` (model-unit height) does not have.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct SheetText {
    /// Text anchor (lower-left) in paper space (mm).
    pub pos_mm: [f64; 2],
    /// The note text.
    pub text: String,
    /// Cap height in millimeters ON PAPER (scale-independent).
    pub height_mm: f64,
    /// Optional view this note is associated with (does not affect its size;
    /// kept for grouping / future anchoring). `None` = free-floating on paper.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub view_index: Option<usize>,
}

/// A paper-space leader: an arrow tip, a kink (knee), then a text label — all
/// in paper millimeters, so the whole annotation is scale-independent.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct SheetLeader {
    /// Arrow tip (what the leader points at) in paper space (mm).
    pub tip_mm: [f64; 2],
    /// The kink/knee where the leader bends toward the text (mm).
    pub knee_mm: [f64; 2],
    /// Text anchor (lower-left) in paper space (mm).
    pub text_pos_mm: [f64; 2],
    /// The label text.
    pub text: String,
    /// Cap height in millimeters ON PAPER (scale-independent).
    pub height_mm: f64,
    /// Optional associated view (grouping only). `None` = free-floating.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub view_index: Option<usize>,
}

/// The bubble/tag outline shape for a `SheetTag`.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TagShape {
    Bubble,
    Square,
    Diamond,
}

impl TagShape {
    pub fn label(self) -> &'static str {
        match self {
            TagShape::Bubble => "bubble",
            TagShape::Square => "square",
            TagShape::Diamond => "diamond",
        }
    }
}

/// A paper-space callout/tag: a shaped bubble with a short label centered in
/// it (grid bubble, detail number, keynote, etc.). Position + size in paper
/// millimeters → scale-independent.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct SheetTag {
    /// Bubble center in paper space (mm).
    pub pos_mm: [f64; 2],
    /// The tag text (e.g. a grid line "A" or a detail number "3").
    pub text: String,
    /// Bubble outline shape.
    pub shape: TagShape,
    /// Optional associated view (grouping / grid reference). `None` = free.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub view_index: Option<usize>,
}

/// A named paper layout holding scaled views of the model.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Sheet {
    pub name: String,
    pub paper: PaperSize,
    #[serde(default)]
    pub views: Vec<SheetView>,
    /// Optional schedule table placed below the views.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub table: Option<SheetTable>,
    /// Paper-space dimensions added via `sheetdim`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dims: Vec<SheetDim>,
    /// Paper-space text notes added via `sheettext` (scale-independent).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub texts: Vec<SheetText>,
    /// Paper-space leaders added via `sheetleader` (scale-independent).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub leaders: Vec<SheetLeader>,
    /// Paper-space callouts/tags added via `sheettag` (scale-independent).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<SheetTag>,
}
