use glam::DVec2;
use serde::{Deserialize, Serialize};

/// A raster image placed on the ground plane (z = 0) as a reference underlay.
/// One per document (the workhorse case: a site plan or sketch to trace over).
/// Placement is a corner in the XY plane plus a width in meters; the height
/// follows from the image's aspect ratio, resolved at command time and carried
/// on the command so replay reproduces identical placement even if the file has
/// since gone missing.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Underlay {
    /// Path to the raster file (PNG). Kept as typed; a missing file on open is
    /// a warning, not an error — the placement still replays.
    pub path: String,
    /// Lower-left corner in doc space (meters).
    pub corner: DVec2,
    /// Width along +x in meters.
    pub width: f64,
    /// Height along +y in meters (width / image aspect ratio).
    pub height: f64,
    /// Blend opacity, 0 (invisible) .. 1 (opaque).
    pub opacity: f32,
}

impl Underlay {
    /// The four corners of the quad in CCW order starting at the lower-left,
    /// all at z = 0. Handy for rendering and placement tests.
    pub fn quad_corners(&self) -> [DVec2; 4] {
        let DVec2 { x, y } = self.corner;
        [
            DVec2::new(x, y),
            DVec2::new(x + self.width, y),
            DVec2::new(x + self.width, y + self.height),
            DVec2::new(x, y + self.height),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quad_corners_span_corner_to_corner_plus_size() {
        let u = Underlay {
            path: "site.png".into(),
            corner: DVec2::new(1.0, 2.0),
            width: 10.0,
            height: 5.0,
            opacity: 0.5,
        };
        assert_eq!(
            u.quad_corners(),
            [
                DVec2::new(1.0, 2.0),
                DVec2::new(11.0, 2.0),
                DVec2::new(11.0, 7.0),
                DVec2::new(1.0, 7.0),
            ]
        );
    }

    #[test]
    fn serde_round_trips() {
        let u = Underlay {
            path: "a/b.png".into(),
            corner: DVec2::new(-3.0, 4.5),
            width: 8.0,
            height: 6.0,
            opacity: 0.75,
        };
        let json = serde_json::to_string(&u).unwrap();
        let back: Underlay = serde_json::from_str(&json).unwrap();
        assert_eq!(u, back);
    }
}
