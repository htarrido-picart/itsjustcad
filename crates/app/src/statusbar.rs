// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! Bottom status strip: pure formatting for cursor coordinates, counts,
//! snap state and the active view. The egui strip in `app.rs` only lays
//! these strings out, so everything user-visible here is unit-testable.

use itsjustcad_doc::{format_length, Units};

/// Fixed column width for each coordinate value. Values are right-aligned to
/// this width (in a monospace font) so a growing/shrinking `x` value never
/// shifts the `y`/`z` fields — each axis label stays put regardless of value.
/// 9 fits the widest common readouts ("-2000 mm", "-12.50 m").
const COORD_W: usize = 9;

/// Cursor position on the ground plane, each axis in document units.
/// No cursor over a viewport reads as an em-dash placeholder. Each value is
/// padded to [`COORD_W`] so the x/y/z fields are alignment-stable.
pub fn format_cursor(units: Units, world: Option<glam::DVec3>) -> String {
    let (vx, vy, vz) = match world {
        Some(p) => (
            format_length(units, p.x),
            format_length(units, p.y),
            format_length(units, p.z),
        ),
        None => ("—".to_string(), "—".to_string(), "—".to_string()),
    };
    format!("x {vx:>COORD_W$}  y {vy:>COORD_W$}  z {vz:>COORD_W$}")
}

/// Selection vs total object count, e.g. "2 sel / 10 obj".
pub fn format_counts(selected: usize, total: usize) -> String {
    format!("{selected} sel / {total} obj")
}

/// Snap readout: the active snap kind while one is hit, grid fallback while
/// a draw tool is picking, idle otherwise.
pub fn snap_label(draw_active: bool, hit: Option<&str>) -> String {
    match (hit, draw_active) {
        (Some(kind), _) => format!("osnap: {kind}"),
        (None, true) => "osnap: grid".to_string(),
        (None, false) => "osnap: idle".to_string(),
    }
}

/// Name of the active camera's view: a standard view when yaw/pitch match
/// one (ortho only), otherwise "Persp" or a free "Ortho" orbit.
pub fn view_label(yaw: f32, pitch: f32, ortho: bool) -> &'static str {
    use std::f32::consts::{FRAC_PI_2, PI, TAU};
    if !ortho {
        return "Persp";
    }
    // Wrap yaw to (-PI, PI] so orbits that lapped the circle still match.
    let yaw = (yaw + PI).rem_euclid(TAU) - PI;
    const EPS: f32 = 1e-3;
    let near = |a: f32, b: f32| (a - b).abs() < EPS || (a - b).abs() > TAU - EPS;
    let table: [(&str, f32, f32); 6] = [
        ("Top", -FRAC_PI_2, FRAC_PI_2),
        ("Bottom", -FRAC_PI_2, -FRAC_PI_2),
        ("Front", -FRAC_PI_2, 0.0),
        ("Back", FRAC_PI_2, 0.0),
        ("Right", 0.0, 0.0),
        ("Left", PI, 0.0),
    ];
    for (name, y, p) in table {
        // Straight up/down: yaw is irrelevant, the pitch pins the view.
        let yaw_ok = near(yaw, y) || p.abs() == FRAC_PI_2 && near(pitch, p);
        if yaw_ok && near(pitch, p) {
            return name;
        }
    }
    "Ortho"
}

#[cfg(test)]
mod tests {
    use super::*;
    use itsjustcad_doc::METERS_PER_FOOT;
    use std::f32::consts::{FRAC_PI_2, FRAC_PI_4, PI, TAU};

    #[test]
    fn cursor_formats_in_document_units() {
        // Collapse the fixed-width padding (runs of spaces → one) to recover the
        // underlying readout in document units.
        let norm = |s: String| s.split_whitespace().collect::<Vec<_>>().join(" ");
        let p = glam::DVec3::new(1.5, -2.0, 0.0);
        assert_eq!(norm(format_cursor(Units::M, Some(p))), "x 1.50 m y -2.00 m z 0.00 m");
        assert_eq!(norm(format_cursor(Units::Mm, Some(p))), "x 1500 mm y -2000 mm z 0 mm");
        let ft = glam::DVec3::new(12.5 * METERS_PER_FOOT, 0.0, 0.0);
        assert_eq!(norm(format_cursor(Units::Ft, Some(ft))), "x 12.50' y 0.00' z 0.00'");
    }

    #[test]
    fn cursor_fields_are_alignment_stable() {
        // The whole point: a wider x value must NOT shift where y/z start. With
        // each value padded to COORD_W chars, the TOTAL char count is constant
        // regardless of the values (and of the em-dash placeholder), so the
        // fields line up column-for-column in the monospace status bar.
        let len = |w| format_cursor(Units::M, w).chars().count();
        let small = len(Some(glam::DVec3::new(1.0, 1.0, 1.0)));
        // Realistic wide readout (each value ≤ COORD_W chars, e.g. "-99.50 m").
        let big = len(Some(glam::DVec3::new(-99.5, 42.0, -7.0)));
        let none = len(None);
        assert_eq!(small, big, "char width shifts when x widens");
        assert_eq!(small, none, "char width shifts for the placeholder");
    }

    #[test]
    fn cursor_placeholder_without_position() {
        // Placeholder is padded to the same widths as real values (stable layout).
        let s = format_cursor(Units::M, None);
        assert!(s.starts_with("x "), "{s}");
        assert!(s.contains(" y ") && s.contains(" z "), "{s}");
        assert!(s.contains('—'), "{s}");
    }

    #[test]
    fn counts() {
        assert_eq!(format_counts(0, 0), "0 sel / 0 obj");
        assert_eq!(format_counts(2, 10), "2 sel / 10 obj");
    }

    #[test]
    fn snap_states() {
        assert_eq!(snap_label(true, Some("End")), "osnap: End");
        assert_eq!(snap_label(false, Some("Mid")), "osnap: Mid");
        assert_eq!(snap_label(true, None), "osnap: grid");
        assert_eq!(snap_label(false, None), "osnap: idle");
    }

    #[test]
    fn standard_views_are_named() {
        // Same table as OrbitCamera::set_view.
        assert_eq!(view_label(-FRAC_PI_2, FRAC_PI_2, true), "Top");
        assert_eq!(view_label(-FRAC_PI_2, -FRAC_PI_2, true), "Bottom");
        assert_eq!(view_label(-FRAC_PI_2, 0.0, true), "Front");
        assert_eq!(view_label(FRAC_PI_2, 0.0, true), "Back");
        assert_eq!(view_label(0.0, 0.0, true), "Right");
        assert_eq!(view_label(PI, 0.0, true), "Left");
    }

    #[test]
    fn top_view_matches_regardless_of_yaw() {
        // Orbiting in Top view spins yaw but stays straight-down.
        assert_eq!(view_label(1.234, FRAC_PI_2, true), "Top");
        assert_eq!(view_label(1.234, -FRAC_PI_2, true), "Bottom");
    }

    #[test]
    fn wrapped_yaw_still_matches() {
        assert_eq!(view_label(-FRAC_PI_2 + TAU, 0.0, true), "Front");
        assert_eq!(view_label(-PI, 0.0, true), "Left"); // -PI wraps to PI
    }

    #[test]
    fn four_viewport_pane_labels() {
        // The 4-viewport layout's camera slots (0 Persp, 1 Top, 2 Front,
        // 3 Right) each yield their corner annotation string.
        assert_eq!(view_label(-FRAC_PI_4, 0.6, false), "Persp");
        assert_eq!(view_label(-FRAC_PI_2, FRAC_PI_2, true), "Top");
        assert_eq!(view_label(-FRAC_PI_2, 0.0, true), "Front");
        assert_eq!(view_label(0.0, 0.0, true), "Right");
    }

    #[test]
    fn free_orbits_fall_back() {
        assert_eq!(view_label(-FRAC_PI_4, 0.5, false), "Persp");
        assert_eq!(view_label(0.3, 0.2, true), "Ortho");
    }
}
