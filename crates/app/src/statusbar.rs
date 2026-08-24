//! Bottom status strip: pure formatting for cursor coordinates, counts,
//! snap state and the active view. The egui strip in `app.rs` only lays
//! these strings out, so everything user-visible here is unit-testable.

use itsjustcad_doc::{format_length, Units};

/// Cursor position on the ground plane, each axis in document units.
/// No cursor over a viewport reads as an em-dash placeholder.
pub fn format_cursor(units: Units, world: Option<glam::DVec3>) -> String {
    match world {
        Some(p) => format!(
            "x {}  y {}  z {}",
            format_length(units, p.x),
            format_length(units, p.y),
            format_length(units, p.z)
        ),
        None => "x —  y —  z —".to_string(),
    }
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
        let p = glam::DVec3::new(1.5, -2.0, 0.0);
        assert_eq!(
            format_cursor(Units::M, Some(p)),
            "x 1.50 m  y -2.00 m  z 0.00 m"
        );
        assert_eq!(
            format_cursor(Units::Mm, Some(p)),
            "x 1500 mm  y -2000 mm  z 0 mm"
        );
        let ft = glam::DVec3::new(12.5 * METERS_PER_FOOT, 0.0, 0.0);
        assert_eq!(format_cursor(Units::Ft, Some(ft)), "x 12.50'  y 0.00'  z 0.00'");
    }

    #[test]
    fn cursor_placeholder_without_position() {
        assert_eq!(format_cursor(Units::M, None), "x —  y —  z —");
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
    fn free_orbits_fall_back() {
        assert_eq!(view_label(-FRAC_PI_4, 0.5, false), "Persp");
        assert_eq!(view_label(0.3, 0.2, true), "Ortho");
    }
}
