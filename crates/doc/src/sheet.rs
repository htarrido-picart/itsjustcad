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

/// A viewport rectangle in paper space (millimeters from the sheet lower-left):
/// `x`,`y` is the lower-left corner, `w`×`h` the size. When a `SheetView` carries
/// one, the PDF layout places that view at this explicit rect instead of the
/// auto-grid. Kept minimal (4 floats) so it serializes compactly.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
pub struct PageRect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

/// One viewport on a sheet: a direction at a drawing scale. `scale` is the
/// denominator — 100 means 1:100. Viewport rectangles are laid out at print
/// time from the view count, so the stored state stays minimal.
///
/// `title` and `rect` are optional overrides (default `None`, so existing sheets
/// are byte-identical): `title` replaces the derived callout (PLAN/ELEVATION/…)
/// with a custom string; `rect` pins the viewport to an explicit paper-space
/// rectangle instead of the auto-grid.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct SheetView {
    pub direction: ViewDirection,
    pub scale: f64,
    /// Custom callout title. `None` → the derived callout for the direction.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Explicit paper-space viewport rectangle (mm). `None` → auto-grid layout.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rect: Option<PageRect>,
}

impl SheetView {
    /// A view with the default auto-layout behavior: no custom title, no pinned
    /// rect. Construction-site helper so callers stay terse and future fields
    /// default in one place.
    pub fn new(direction: ViewDirection, scale: f64) -> Self {
        Self { direction, scale, title: None, rect: None }
    }
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

/// An architect's title block / stamp for a sheet: the metadata box (bottom-
/// right by convention) carrying project/client/sheet identity. Every field is
/// optional — when they are all absent/empty the stamp renders nothing, so
/// existing sheets are unchanged. Kept as plain strings (free text) because this
/// is annotation, not model state.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct TitleBlock {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub address: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sheet_no: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sheet_title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scale: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub date: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub drawn_by: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<String>,
}

impl TitleBlock {
    /// True when every field is empty/absent — the stamp should render nothing.
    pub fn is_empty(&self) -> bool {
        [
            &self.project,
            &self.address,
            &self.client,
            &self.sheet_no,
            &self.sheet_title,
            &self.scale,
            &self.date,
            &self.drawn_by,
            &self.revision,
        ]
        .iter()
        .all(|f| f.as_deref().map(str::is_empty).unwrap_or(true))
    }
}

/// A named paper layout holding scaled views of the model.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Sheet {
    pub name: String,
    pub paper: PaperSize,
    #[serde(default)]
    pub views: Vec<SheetView>,
    /// Architect title block / stamp. `None` → no stamp (behavior unchanged).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub titleblock: Option<TitleBlock>,
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

/// A named, ordered collection of existing sheets — the AutoCAD Sheet Set
/// Manager / InDesign document analogue. A set is a lightweight *index*: it
/// stores only sheet names, never a copy of their content, so the referenced
/// `Sheet`s remain the single source of truth. Sheet numbers are positional
/// (1-based) — the number of a sheet is `its index in `sheets` + 1`, so
/// reordering renumbers automatically and nothing needs to be stored.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct SheetSet {
    pub name: String,
    /// Sheet names in publication order. These reference `Document::sheets` by
    /// name; a name may linger here after its sheet is deleted (readers filter).
    #[serde(default)]
    pub sheets: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A view with the new optional fields round-trips through JSON, and the
    /// defaulted (None) case omits them so output stays compact.
    #[test]
    fn sheet_view_serde_round_trip_with_new_fields() {
        let v = SheetView {
            direction: ViewDirection::Top,
            scale: 100.0,
            title: Some("GROUND FLOOR".into()),
            rect: Some(PageRect { x: 10.0, y: 20.0, w: 100.0, h: 80.0 }),
        };
        let json = serde_json::to_string(&v).unwrap();
        assert_eq!(v, serde_json::from_str::<SheetView>(&json).unwrap());

        // A default view omits title/rect (skip_serializing_if).
        let plain = SheetView::new(ViewDirection::Front, 50.0);
        let plain_json = serde_json::to_string(&plain).unwrap();
        assert!(!plain_json.contains("\"title\""), "title omitted when None: {plain_json}");
        assert!(!plain_json.contains("\"rect\""), "rect omitted when None: {plain_json}");
    }

    /// OLD documents — views with only `direction`/`scale`, sheets with no
    /// `titleblock` — still deserialize, with the new fields defaulting. This is
    /// the critical backward-compatibility guarantee for stored checkpoints.
    #[test]
    fn old_json_defaults_new_fields() {
        // Pre-feature SheetView shape.
        let old_view = r#"{"direction":"top","scale":100.0}"#;
        let v: SheetView = serde_json::from_str(old_view).unwrap();
        assert!(v.title.is_none() && v.rect.is_none());

        // Pre-feature Sheet shape (no titleblock key).
        let old_sheet = r#"{"name":"s","paper":"a3","views":[]}"#;
        let s: Sheet = serde_json::from_str(old_sheet).unwrap();
        assert!(s.titleblock.is_none(), "titleblock defaults to None on old docs");
    }

    /// The title block round-trips and `is_empty` reflects filled vs blank.
    #[test]
    fn titleblock_serde_and_is_empty() {
        assert!(TitleBlock::default().is_empty());
        let tb = TitleBlock {
            project: Some("Villa".into()),
            drawn_by: Some("HTP".into()),
            ..TitleBlock::default()
        };
        assert!(!tb.is_empty());
        let json = serde_json::to_string(&tb).unwrap();
        assert_eq!(tb, serde_json::from_str::<TitleBlock>(&json).unwrap());
        // Blank strings count as empty.
        let blank = TitleBlock { project: Some(String::new()), ..TitleBlock::default() };
        assert!(blank.is_empty(), "all-blank fields → empty");
    }
}
