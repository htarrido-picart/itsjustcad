//! Non-pinhole camera projections (panorama + fisheye).
//!
//! A pinhole `view_proj` matrix cannot express a >180° field of view or the
//! curved image plane a real fisheye/panoramic lens produces. The pragmatic
//! route (see the feature brief) is to render the scene into a **cubemap** —
//! six pinhole faces captured from the eye — and then remap that cube into the
//! output image in a fullscreen post pass. Each output pixel maps to a ray
//! direction; that direction samples the cube.
//!
//! This module is the pure, GPU-independent core: the pixel <-> ray-direction
//! math for both projections, plus the eye-space basis the faces are captured
//! in. It is unit-tested in isolation (round-trips, radius mapping) so the
//! shader only has to mirror these formulas.

use glam::Vec3;

/// Which non-pinhole projection a panoramic/fisheye camera uses.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PanoProjection {
    /// Equirectangular (lat/long) panorama: x maps to longitude over the full
    /// 360°, y maps to latitude over 180° (top = zenith, bottom = nadir).
    Equirect,
    /// Fisheye hemisphere with a given field of view (radians). Equidistant
    /// mapping: image radius is proportional to the angle from the view axis
    /// (`r/r_max = theta / (fov/2)`), the classic "f-theta" lens.
    Fisheye { fov: f32 },
}

impl PanoProjection {
    /// Default fisheye field of view: a 180° hemisphere.
    pub fn default_fisheye() -> Self {
        PanoProjection::Fisheye { fov: std::f32::consts::PI }
    }
}

/// Normalised device coordinates for an output pixel, each axis in `-1..=1`
/// with +y up and the origin at the image centre. This is the input the post
/// shader hands to the ray functions.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Ndc {
    pub x: f32,
    pub y: f32,
}

/// Convert a pixel centre `(px, py)` in a `w x h` image to centred NDC with
/// +y up (pixel rows run top-to-bottom, so y is flipped).
pub fn pixel_to_ndc(px: f32, py: f32, w: f32, h: f32) -> Ndc {
    Ndc {
        x: (px + 0.5) / w * 2.0 - 1.0,
        y: 1.0 - (py + 0.5) / h * 2.0,
    }
}

/// Ray direction (unit vector, in the camera's view basis where +z is forward,
/// +x right, +y up) for an equirectangular output pixel.
///
/// Longitude `lon = x * PI` sweeps the full 360° across the width; latitude
/// `lat = y * PI/2` sweeps 180° over the height. `lon = 0, lat = 0` looks
/// straight ahead (+z). This is the inverse of [`equirect_dir_to_ndc`].
pub fn equirect_ndc_to_dir(ndc: Ndc) -> Vec3 {
    let lon = ndc.x * std::f32::consts::PI;
    let lat = ndc.y * std::f32::consts::FRAC_PI_2;
    let (sla, cla) = lat.sin_cos();
    let (slo, clo) = lon.sin_cos();
    // forward = +z, right = +x, up = +y.
    Vec3::new(cla * slo, sla, cla * clo)
}

/// Project a view-basis ray direction back to equirectangular NDC. Returns
/// `None` only for a zero vector. Inverse of [`equirect_ndc_to_dir`].
pub fn equirect_dir_to_ndc(dir: Vec3) -> Option<Ndc> {
    let d = dir.normalize_or_zero();
    if d == Vec3::ZERO {
        return None;
    }
    let lat = d.y.clamp(-1.0, 1.0).asin();
    let lon = d.x.atan2(d.z); // atan2(right, forward)
    Some(Ndc {
        x: lon / std::f32::consts::PI,
        y: lat / std::f32::consts::FRAC_PI_2,
    })
}

/// Fisheye (equidistant) ray direction for an output pixel, or `None` when the
/// pixel falls outside the circular image (corners of a square frame).
///
/// The image is a disc of unit radius in NDC. A pixel at radius `r` from centre
/// maps to angle `theta = r * (fov/2)` from the forward axis; the azimuth is the
/// pixel's polar angle. `r = 0` looks forward (+z); `r = 1` sits at the rim
/// (`theta = fov/2`).
pub fn fisheye_ndc_to_dir(ndc: Ndc, fov: f32) -> Option<Vec3> {
    let r = (ndc.x * ndc.x + ndc.y * ndc.y).sqrt();
    if r > 1.0 {
        return None; // outside the image circle
    }
    let theta = r * (fov * 0.5);
    if r < 1e-6 {
        return Some(Vec3::Z); // straight ahead
    }
    // Azimuth in the image plane: (x, y) direction, scaled by sin(theta).
    let (ax, ay) = (ndc.x / r, ndc.y / r);
    let (st, ct) = theta.sin_cos();
    Vec3::new(ax * st, ay * st, ct).normalize().into()
}

/// Image radius (in `0..=1` NDC units) for a ray at angle `theta` from the
/// forward axis under an equidistant fisheye of field of view `fov`. Rays past
/// the rim (`theta > fov/2`) return `> 1`. Pure inverse of the radius part of
/// [`fisheye_ndc_to_dir`].
pub fn fisheye_radius(theta: f32, fov: f32) -> f32 {
    theta / (fov * 0.5).max(1e-6)
}

/// The six cubemap faces, in the wgpu cube layer order (+X, -X, +Y, -Y, +Z,
/// -Z). Each is a 90° pinhole capture from the eye.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CubeFace {
    PosX,
    NegX,
    PosY,
    NegY,
    PosZ,
    NegZ,
}

impl CubeFace {
    pub const ALL: [CubeFace; 6] = [
        CubeFace::PosX,
        CubeFace::NegX,
        CubeFace::PosY,
        CubeFace::NegY,
        CubeFace::PosZ,
        CubeFace::NegZ,
    ];

    /// Forward (face centre) direction in the camera view basis.
    pub fn forward(self) -> Vec3 {
        match self {
            CubeFace::PosX => Vec3::X,
            CubeFace::NegX => Vec3::NEG_X,
            CubeFace::PosY => Vec3::Y,
            CubeFace::NegY => Vec3::NEG_Y,
            CubeFace::PosZ => Vec3::Z,
            CubeFace::NegZ => Vec3::NEG_Z,
        }
    }

    /// Up vector for a right-handed look-at that keeps the six faces seamless.
    pub fn up(self) -> Vec3 {
        match self {
            CubeFace::PosY => Vec3::NEG_Z,
            CubeFace::NegY => Vec3::Z,
            _ => Vec3::Y,
        }
    }

    /// Pick the face a ray direction lands on: the axis of largest magnitude.
    pub fn of_dir(dir: Vec3) -> CubeFace {
        let a = dir.abs();
        if a.x >= a.y && a.x >= a.z {
            if dir.x >= 0.0 { CubeFace::PosX } else { CubeFace::NegX }
        } else if a.y >= a.z {
            if dir.y >= 0.0 { CubeFace::PosY } else { CubeFace::NegY }
        } else if dir.z >= 0.0 {
            CubeFace::PosZ
        } else {
            CubeFace::NegZ
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx(a: f32, b: f32, eps: f32) -> bool {
        (a - b).abs() < eps
    }

    #[test]
    fn pixel_to_ndc_corners_and_centre() {
        // Centre pixel of an even grid maps near origin.
        let c = pixel_to_ndc(639.5, 399.5, 1280.0, 800.0);
        assert!(approx(c.x, 0.0, 1e-3) && approx(c.y, 0.0, 1e-3), "{c:?}");
        // Top-left pixel: x near -1, y near +1 (y flipped).
        let tl = pixel_to_ndc(0.0, 0.0, 1280.0, 800.0);
        assert!(tl.x < -0.99 && tl.y > 0.99, "{tl:?}");
        // Bottom-right: x near +1, y near -1.
        let br = pixel_to_ndc(1279.0, 799.0, 1280.0, 800.0);
        assert!(br.x > 0.99 && br.y < -0.99, "{br:?}");
    }

    #[test]
    fn equirect_centre_looks_forward() {
        let d = equirect_ndc_to_dir(Ndc { x: 0.0, y: 0.0 });
        assert!((d - Vec3::Z).length() < 1e-5, "{d:?}");
    }

    #[test]
    fn equirect_edges_are_behind() {
        // x = ±1 is longitude ±180° -> looking backward (-z).
        let l = equirect_ndc_to_dir(Ndc { x: 1.0, y: 0.0 });
        assert!((l - Vec3::NEG_Z).length() < 1e-5, "{l:?}");
        // y = +1 is the zenith (+y up).
        let top = equirect_ndc_to_dir(Ndc { x: 0.0, y: 1.0 });
        assert!((top - Vec3::Y).length() < 1e-5, "{top:?}");
    }

    #[test]
    fn equirect_pixel_dir_pixel_round_trip() {
        // A grid of pixels: pixel -> dir -> pixel must return the same NDC.
        for &x in &[-0.9f32, -0.3, 0.0, 0.25, 0.8] {
            for &y in &[-0.8f32, -0.1, 0.0, 0.4, 0.9] {
                let ndc = Ndc { x, y };
                let dir = equirect_ndc_to_dir(ndc);
                let back = equirect_dir_to_ndc(dir).expect("nonzero");
                assert!(
                    approx(back.x, ndc.x, 1e-4) && approx(back.y, ndc.y, 1e-4),
                    "round trip {ndc:?} -> {dir:?} -> {back:?}"
                );
            }
        }
    }

    #[test]
    fn equirect_dir_is_unit_length() {
        for &x in &[-1.0f32, -0.5, 0.0, 0.7, 1.0] {
            for &y in &[-1.0f32, 0.0, 0.5, 1.0] {
                let d = equirect_ndc_to_dir(Ndc { x, y });
                assert!(approx(d.length(), 1.0, 1e-5), "{x},{y} -> len {}", d.length());
            }
        }
    }

    #[test]
    fn fisheye_centre_looks_forward() {
        let d = fisheye_ndc_to_dir(Ndc { x: 0.0, y: 0.0 }, std::f32::consts::PI).unwrap();
        assert!((d - Vec3::Z).length() < 1e-5, "{d:?}");
    }

    #[test]
    fn fisheye_rim_is_90_deg_for_180_fov() {
        // At r = 1 with a 180° fov, theta = 90°: the ray is perpendicular to
        // forward (lands on the +x axis for a pixel on the +x rim).
        let d = fisheye_ndc_to_dir(Ndc { x: 1.0, y: 0.0 }, std::f32::consts::PI).unwrap();
        assert!(approx(d.z, 0.0, 1e-5), "rim should be side-on: {d:?}");
        assert!(approx(d.x, 1.0, 1e-5), "{d:?}");
    }

    #[test]
    fn fisheye_outside_circle_is_none() {
        // Corner of a square frame (r = sqrt(2)) is outside the image disc.
        assert!(fisheye_ndc_to_dir(Ndc { x: 1.0, y: 1.0 }, std::f32::consts::PI).is_none());
    }

    #[test]
    fn fisheye_radius_matches_dir_mapping() {
        // radius(theta) is the inverse of the ndc->dir angle: pick an NDC radius,
        // find its ray, measure the angle from forward, map back to radius.
        let fov = std::f32::consts::PI; // 180°
        for &r in &[0.0f32, 0.25, 0.5, 0.75, 1.0] {
            let ndc = Ndc { x: r, y: 0.0 };
            let dir = fisheye_ndc_to_dir(ndc, fov).unwrap();
            let theta = dir.z.clamp(-1.0, 1.0).acos(); // angle from +z forward
            let back = fisheye_radius(theta, fov);
            assert!(approx(back, r, 1e-4), "r={r} theta={theta} back={back}");
        }
    }

    #[test]
    fn fisheye_narrower_fov_pushes_rays_outward() {
        // A given pixel radius maps to a larger angle under a wider fov.
        let ndc = Ndc { x: 0.5, y: 0.0 };
        let wide = fisheye_ndc_to_dir(ndc, std::f32::consts::PI).unwrap();
        let narrow = fisheye_ndc_to_dir(ndc, std::f32::consts::FRAC_PI_2).unwrap();
        let theta_wide = wide.z.clamp(-1.0, 1.0).acos();
        let theta_narrow = narrow.z.clamp(-1.0, 1.0).acos();
        assert!(theta_wide > theta_narrow, "{theta_wide} vs {theta_narrow}");
    }

    #[test]
    fn cube_face_of_dir_picks_dominant_axis() {
        assert_eq!(CubeFace::of_dir(Vec3::new(0.9, 0.1, 0.2)), CubeFace::PosX);
        assert_eq!(CubeFace::of_dir(Vec3::new(-0.9, 0.1, 0.2)), CubeFace::NegX);
        assert_eq!(CubeFace::of_dir(Vec3::new(0.1, 0.9, 0.2)), CubeFace::PosY);
        assert_eq!(CubeFace::of_dir(Vec3::new(0.1, 0.2, -0.9)), CubeFace::NegZ);
        // Equirect forward (+z) and its zenith (+y) land on the expected faces.
        assert_eq!(CubeFace::of_dir(equirect_ndc_to_dir(Ndc { x: 0.0, y: 0.0 })), CubeFace::PosZ);
        assert_eq!(CubeFace::of_dir(equirect_ndc_to_dir(Ndc { x: 0.0, y: 1.0 })), CubeFace::PosY);
    }

    #[test]
    fn every_equirect_pixel_lands_on_some_face() {
        // Sanity: the cube covers the full sphere, so no equirect pixel is
        // left unsampled (of_dir is total).
        for i in 0..20 {
            for j in 0..20 {
                let ndc = pixel_to_ndc(i as f32, j as f32, 20.0, 20.0);
                let dir = equirect_ndc_to_dir(ndc);
                let _ = CubeFace::of_dir(dir); // must not panic / always returns
                assert!(dir.is_finite());
            }
        }
    }
}
