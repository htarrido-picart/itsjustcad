// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! Plot styles (AutoCAD CTB/STB pen tables): a named, saved mapping that
//! controls how objects PLOT (print/export) — pen color, lineweight, screening
//! — WITHOUT changing the model. A plot style is resolved only at print/export
//! time; the geometry, per-object colors and layer styles are all untouched.
//!
//! A table maps a SOURCE KEY (a layer name, or an object color token `r,g,b`)
//! to plot properties. Print looks each drawn object up by its layer first,
//! then by its color; the first match overrides the pen. Unmapped objects draw
//! as today.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// One entry in a plot-style table: the plot properties a matched object draws
/// with. Every field is optional so an entry can override just the color, just
/// the weight, etc. `None` fields defer to the object's normal pen.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct PlotStyleEntry {
    /// Plot (pen) color, RGB 0..1. `None` keeps the object's normal color.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<[f32; 3]>,
    /// Plot lineweight in mm. `None` keeps the object's effective lineweight.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub weight_mm: Option<f64>,
    /// Screening percentage 0..100 (100 = full ink, 0 = invisible). AutoCAD's
    /// pen "screening"; applied as a lightening factor toward white at plot
    /// time. `None` = 100% (no screening).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub screen: Option<f64>,
}

/// A named plot-style table: SOURCE KEY → plot properties. Keys are either a
/// layer name or an object color token `"r,g,b"` (three ints 0..255). Layer
/// keys are matched first, then color keys.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct PlotStyleTable {
    pub entries: BTreeMap<String, PlotStyleEntry>,
}

/// The resolved pen an object draws with under an active plot style.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ResolvedPen {
    /// Effective RGB color 0..1 (already screened).
    pub color: [f32; 3],
    /// Effective lineweight in mm.
    pub weight_mm: f64,
}

impl PlotStyleTable {
    /// Canonical color key for an RGB-0..1 triple: `"r,g,b"` with each channel
    /// quantised to 0..255. Both `plotstyle set` (color keys) and the print-time
    /// lookup use this so a mapped color matches an object's color exactly.
    pub fn color_key(rgb: [f32; 3]) -> String {
        let q = |c: f32| (c.clamp(0.0, 1.0) * 255.0).round() as u32;
        format!("{},{},{}", q(rgb[0]), q(rgb[1]), q(rgb[2]))
    }

    /// Resolve the plot pen for an object, given its layer and its *base* pen
    /// (the color + lineweight it would draw with WITHOUT any plot style). The
    /// table is consulted by layer key first, then by the base color key; the
    /// first matching entry overrides the pen (per-field). Screening lightens
    /// the resulting color toward white. Returns the base pen unchanged when no
    /// key matches.
    pub fn resolve_pen(&self, layer: &str, base: ResolvedPen) -> ResolvedPen {
        let entry = self
            .entries
            .get(layer)
            .or_else(|| self.entries.get(&Self::color_key(base.color)));
        let Some(entry) = entry else {
            return base;
        };
        let mut color = entry.color.unwrap_or(base.color);
        if let Some(pct) = entry.screen {
            let t = (1.0 - (pct.clamp(0.0, 100.0) / 100.0)) as f32;
            // Lerp toward white by the un-inked fraction (100% screen = full ink).
            color = [
                color[0] + (1.0 - color[0]) * t,
                color[1] + (1.0 - color[1]) * t,
                color[2] + (1.0 - color[2]) * t,
            ];
        }
        ResolvedPen {
            color,
            weight_mm: entry.weight_mm.unwrap_or(base.weight_mm),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tbl() -> PlotStyleTable {
        let mut t = PlotStyleTable::default();
        t.entries.insert(
            "walls".into(),
            PlotStyleEntry { color: Some([1.0, 0.0, 0.0]), weight_mm: Some(0.5), screen: None },
        );
        t
    }

    #[test]
    fn layer_key_overrides_pen() {
        let base = ResolvedPen { color: [0.0, 0.0, 0.0], weight_mm: 0.18 };
        let pen = tbl().resolve_pen("walls", base);
        assert_eq!(pen.color, [1.0, 0.0, 0.0]);
        assert_eq!(pen.weight_mm, 0.5);
    }

    #[test]
    fn unmapped_object_keeps_base_pen() {
        let base = ResolvedPen { color: [0.2, 0.2, 0.2], weight_mm: 0.25 };
        let pen = tbl().resolve_pen("grid", base);
        assert_eq!(pen, base);
    }

    #[test]
    fn color_key_matches_base_color() {
        let mut t = PlotStyleTable::default();
        // Map the color token for pure green to a blue pen.
        t.entries.insert(
            PlotStyleTable::color_key([0.0, 1.0, 0.0]),
            PlotStyleEntry { color: Some([0.0, 0.0, 1.0]), weight_mm: None, screen: None },
        );
        let base = ResolvedPen { color: [0.0, 1.0, 0.0], weight_mm: 0.3 };
        let pen = t.resolve_pen("anything", base);
        assert_eq!(pen.color, [0.0, 0.0, 1.0]);
        assert_eq!(pen.weight_mm, 0.3, "weight untouched when entry omits it");
    }

    #[test]
    fn screening_lightens_toward_white() {
        let mut t = PlotStyleTable::default();
        t.entries.insert(
            "faded".into(),
            PlotStyleEntry { color: Some([0.0, 0.0, 0.0]), weight_mm: None, screen: Some(50.0) },
        );
        let base = ResolvedPen { color: [0.0, 0.0, 0.0], weight_mm: 0.18 };
        let pen = t.resolve_pen("faded", base);
        // 50% screen on black → mid grey.
        for c in pen.color {
            assert!((c - 0.5).abs() < 1e-6, "expected ~0.5, got {c}");
        }
    }
}
