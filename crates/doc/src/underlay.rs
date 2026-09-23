// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

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
    /// Counter-clockwise rotation about the quad's centre, in degrees. Serde
    /// defaults to 0 so old save files (no rotation) load unrotated.
    #[serde(default)]
    pub rotation_deg: f64,
}

impl Underlay {
    /// The four corners of the quad in CCW order starting at the (pre-rotation)
    /// lower-left, all at z = 0, with `rotation_deg` applied about the centre.
    /// Handy for rendering and placement tests. When `rotation_deg` is 0 this
    /// is exactly the axis-aligned corner/width/height rectangle.
    pub fn quad_corners(&self) -> [DVec2; 4] {
        underlay_quad_corners(self.corner, self.width, self.height, self.rotation_deg)
    }

    /// Centre of the quad (unaffected by rotation, which is about the centre).
    pub fn center(&self) -> DVec2 {
        DVec2::new(self.corner.x + self.width * 0.5, self.corner.y + self.height * 0.5)
    }
}

/// Pure corner computation for a placed underlay: the axis-aligned rectangle
/// `corner .. corner + (width, height)` rotated `rotation_deg` degrees CCW about
/// its centre. Returned CCW from the (pre-rotation) lower-left, all at z = 0.
/// Split out so the geometry is unit-testable without an `Underlay` value.
pub fn underlay_quad_corners(corner: DVec2, width: f64, height: f64, rotation_deg: f64) -> [DVec2; 4] {
    let base = [
        DVec2::new(corner.x, corner.y),
        DVec2::new(corner.x + width, corner.y),
        DVec2::new(corner.x + width, corner.y + height),
        DVec2::new(corner.x, corner.y + height),
    ];
    if rotation_deg == 0.0 {
        return base;
    }
    let center = DVec2::new(corner.x + width * 0.5, corner.y + height * 0.5);
    let (s, c) = rotation_deg.to_radians().sin_cos();
    base.map(|p| {
        let d = p - center;
        center + DVec2::new(d.x * c - d.y * s, d.x * s + d.y * c)
    })
}

/// A georeferenced satellite/OSM ground image placed under the model at the
/// site location. Unlike [`Underlay`], the basemap is **transient session
/// state**: it is never serialized into the op-log or save file (it may be
/// several megabytes of tile pixels, and it is reproducible from the location).
/// The app rebuilds/refetches it on demand. Corners are in local meters (same
/// projection as GeoJSON import) so it lines up with imported site geometry.
#[derive(Clone, Debug, PartialEq)]
pub struct Basemap {
    /// Stitched RGBA8 pixels, row-major, `width_px * height_px * 4` bytes.
    pub rgba: Vec<u8>,
    pub width_px: u32,
    pub height_px: u32,
    /// Lower-left corner on the ground plane (z=0) in local meters.
    pub corner: DVec2,
    /// Width along +x / height along +y in local meters.
    pub width: f64,
    pub height: f64,
    /// Blend opacity, 0 (invisible) .. 1 (opaque).
    pub opacity: f32,
    /// Provider slug + zoom, for the status line ("osm z16").
    pub label: String,
}

impl Basemap {
    /// The four ground-plane corners, CCW from lower-left (all at z=0).
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
    fn basemap_quad_corners_match_placement() {
        let b = Basemap {
            rgba: vec![],
            width_px: 0,
            height_px: 0,
            corner: DVec2::new(-100.0, -50.0),
            width: 200.0,
            height: 100.0,
            opacity: 0.8,
            label: "osm z16".into(),
        };
        assert_eq!(
            b.quad_corners(),
            [
                DVec2::new(-100.0, -50.0),
                DVec2::new(100.0, -50.0),
                DVec2::new(100.0, 50.0),
                DVec2::new(-100.0, 50.0),
            ]
        );
    }

    #[test]
    fn quad_corners_span_corner_to_corner_plus_size() {
        let u = Underlay {
            path: "site.png".into(),
            corner: DVec2::new(1.0, 2.0),
            width: 10.0,
            height: 5.0,
            opacity: 0.5,
            rotation_deg: 0.0,
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
            rotation_deg: 30.0,
        };
        let json = serde_json::to_string(&u).unwrap();
        let back: Underlay = serde_json::from_str(&json).unwrap();
        assert_eq!(u, back);
    }

    #[test]
    fn old_json_without_rotation_defaults_to_zero() {
        // Back-compat: files saved before rotation existed carry no field.
        let json = r#"{"path":"a.png","corner":[1.0,2.0],"width":10.0,"height":5.0,"opacity":1.0}"#;
        let u: Underlay = serde_json::from_str(json).unwrap();
        assert_eq!(u.rotation_deg, 0.0);
    }

    #[test]
    fn rotation_90_maps_corners_about_center() {
        // A 10x5 rect at origin; centre is (5, 2.5). Rotating 90° CCW sends the
        // lower-left corner (0,0) to (7.5, -2.5).
        let corners = underlay_quad_corners(DVec2::new(0.0, 0.0), 10.0, 5.0, 90.0);
        let eps = 1e-9;
        assert!((corners[0].x - 7.5).abs() < eps, "{:?}", corners[0]);
        assert!((corners[0].y - (-2.5)).abs() < eps, "{:?}", corners[0]);
        // Centre is preserved: the mean of the corners stays at (5, 2.5).
        let cx = corners.iter().map(|p| p.x).sum::<f64>() / 4.0;
        let cy = corners.iter().map(|p| p.y).sum::<f64>() / 4.0;
        assert!((cx - 5.0).abs() < eps && (cy - 2.5).abs() < eps);
    }

    #[test]
    fn rotation_360_is_identity() {
        let a = underlay_quad_corners(DVec2::new(1.0, 2.0), 10.0, 5.0, 0.0);
        let b = underlay_quad_corners(DVec2::new(1.0, 2.0), 10.0, 5.0, 360.0);
        for (p, q) in a.iter().zip(b.iter()) {
            assert!((p.x - q.x).abs() < 1e-9 && (p.y - q.y).abs() < 1e-9);
        }
    }
}
