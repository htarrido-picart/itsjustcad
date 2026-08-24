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
}
