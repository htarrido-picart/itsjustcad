// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! Solar position algorithm — NOAA simplified SPA.
//!
//! Reference: NOAA Solar Calculator spreadsheet equations (public domain,
//! <https://www.esrl.noaa.gov/gmd/grad/solcalc/calcdetails.html>) and
//! Jean Meeus, *Astronomical Algorithms*, 2nd ed., Ch. 25–27.
//!
//! All intermediate angles are in degrees unless noted. Accuracy: ±0.01° for
//! dates within ±200 years of J2000.0 (well within the ±0.5° task tolerance).

/// Output of `solar_position`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SunPos {
    /// Azimuth measured clockwise from North, degrees [0, 360).
    pub azimuth_deg: f64,
    /// Altitude above the horizon, degrees (negative = below horizon).
    pub altitude_deg: f64,
}

/// Compute the solar position for a given instant and observer location.
///
/// # Parameters
/// - `year`, `month` (1-based), `day` (1-based)
/// - `hour_utc`, `minute_utc` — UTC time components
/// - `lat_deg` — observer latitude, degrees (north positive)
/// - `lon_deg` — observer longitude, degrees (east positive)
pub fn solar_position(
    year: i32,
    month: u32,
    day: u32,
    hour_utc: u32,
    minute_utc: u32,
    lat_deg: f64,
    lon_deg: f64,
) -> SunPos {
    // --- Julian Day Number (JDN) following Meeus §7, eq. 7.1 ----------
    // Works for the full Gregorian calendar.
    let (y, m) = if month <= 2 {
        (year as f64 - 1.0, month as f64 + 12.0)
    } else {
        (year as f64, month as f64)
    };
    let a = (y / 100.0).floor();
    let b = 2.0 - a + (a / 4.0).floor(); // Gregorian correction
    let day_frac = day as f64 + (hour_utc as f64 + minute_utc as f64 / 60.0) / 24.0;
    let jd = (365.25 * (y + 4716.0)).floor()
        + (30.6001 * (m + 1.0)).floor()
        + day_frac
        + b
        - 1524.5;

    // Julian centuries since J2000.0 (JD 2451545.0)
    let t = (jd - 2_451_545.0) / 36_525.0;

    // --- Geometric mean longitude of the Sun (deg), corrected for aberration
    let l0 = (280.46646 + t * (36_000.769_83 + t * 0.0003032)).rem_euclid(360.0);

    // --- Geometric mean anomaly of the Sun (deg)
    let m_sun = (357.52911 + t * (35_999.050_29 - t * 0.0001537)).rem_euclid(360.0);
    let m_rad = m_sun.to_radians();

    // --- Equation of the center
    let c = (1.914602 - t * (0.004817 + 0.000014 * t)) * m_rad.sin()
        + (0.019993 - 0.000101 * t) * (2.0 * m_rad).sin()
        + 0.000289 * (3.0 * m_rad).sin();

    // --- Sun's true longitude and anomaly
    let sun_lon = l0 + c;

    // --- Apparent longitude (corrected for nutation and aberration, deg)
    let omega = 125.04 - 1934.136 * t;
    let sun_app_lon = sun_lon - 0.00569 - 0.00478 * omega.to_radians().sin();

    // --- Mean obliquity of the ecliptic (deg)
    let mean_obliq = 23.0
        + (26.0
            + (21.448 - t * (46.8150 + t * (0.00059 - t * 0.001813))) / 60.0)
            / 60.0;

    // --- Corrected obliquity
    let obliq_corr = mean_obliq + 0.00256 * omega.to_radians().cos();

    // --- Right ascension (RA) and declination, both in degrees
    let sun_app_rad = sun_app_lon.to_radians();
    let obliq_rad = obliq_corr.to_radians();

    // Right ascension is an intermediate — not used directly but computed
    // as part of the standard SPA derivation (declination uses the same trig).
    let _ra = f64::atan2(
        obliq_rad.cos() * sun_app_rad.sin(),
        sun_app_rad.cos(),
    )
    .to_degrees();

    let decl = f64::asin(obliq_rad.sin() * sun_app_rad.sin()).to_degrees();

    // --- Earth's orbital eccentricity (NOAA spreadsheet)
    let ecc = 0.016708634 - t * (0.000042037 + 0.0000001267 * t);

    // --- Equation of Time (minutes) — NOAA spreadsheet, column R
    let y_e = (obliq_rad / 2.0).tan().powi(2);
    let l0_rad = l0.to_radians();
    let eqtime = 4.0
        * (y_e * (2.0 * l0_rad).sin()
            - 2.0 * ecc * m_rad.sin()
            + 4.0 * ecc * y_e * m_rad.sin() * (2.0 * l0_rad).cos()
            - 0.5 * y_e.powi(2) * (4.0 * l0_rad).sin()
            - 1.25 * ecc.powi(2) * (2.0 * m_rad).sin())
        .to_degrees();

    // --- True solar time (minutes past midnight, UTC)
    let time_minutes = hour_utc as f64 * 60.0 + minute_utc as f64;
    // Longitude offset: +4 min per degree east
    let true_solar_time = (time_minutes + eqtime + 4.0 * lon_deg).rem_euclid(1440.0);

    // --- Hour angle (deg).
    // NOAA spreadsheet: IF(TST/4 < 180, TST/4-180, TST/4-540)
    // This maps [0,1440) onto [-180,+180) except for the [720,1440) half where
    // the result is negative again (wraps around). We keep the raw value for
    // cos(ha) (which is symmetric), but track the afternoon flag separately for
    // the azimuth branch.
    let ha_raw = true_solar_time / 4.0 - 180.0; // linear, may exceed ±180
    let ha = if ha_raw < 180.0 { ha_raw } else { ha_raw - 360.0 };
    // True afternoon flag: true solar time past noon (TST > 720 min)
    let is_afternoon = true_solar_time > 720.0;

    // --- Solar zenith and altitude
    // NOAA spreadsheet: "Solar Zenith Angle (deg)" = DEGREES(ACOS(...))
    let lat_rad = lat_deg.to_radians();
    let decl_rad = decl.to_radians();
    let ha_rad = ha.to_radians();

    let cos_zenith =
        lat_rad.sin() * decl_rad.sin() + lat_rad.cos() * decl_rad.cos() * ha_rad.cos();
    let zenith_rad = cos_zenith.clamp(-1.0, 1.0).acos();
    let zenith_deg = zenith_rad.to_degrees();
    let altitude_deg = 90.0 - zenith_deg;

    // --- Azimuth (degrees, clockwise from North)
    // NOAA spreadsheet formula exactly:
    //   cos_az = (sin(lat)*cos(zenith) - sin(decl)) / (cos(lat)*sin(zenith))
    //   if ha > 0: az = (acos(cos_az) + 180) mod 360
    //   else:      az = (540 - acos(cos_az)) mod 360
    let sin_zenith = zenith_rad.sin();
    let azimuth_deg = if sin_zenith.abs() < 1e-9 {
        // Sun directly overhead or below: azimuth is undefined; use 0.
        0.0
    } else {
        let cos_az = (lat_rad.sin() * cos_zenith - decl_rad.sin())
            / (lat_rad.cos() * sin_zenith);
        let az_base = cos_az.clamp(-1.0, 1.0).acos().to_degrees();
        if is_afternoon {
            (az_base + 180.0).rem_euclid(360.0)
        } else {
            (540.0 - az_base).rem_euclid(360.0)
        }
    };

    SunPos { azimuth_deg, altitude_deg }
}

/// Convert azimuth + altitude to a unit direction vector pointing *toward* the
/// sun in a right-handed Z-up coordinate system (X = East, Y = North, Z = Up).
///
/// Azimuth is clockwise from North; altitude is above the horizon.
pub fn sun_direction(az_deg: f64, alt_deg: f64) -> [f32; 3] {
    let az = az_deg.to_radians();
    let alt = alt_deg.to_radians();
    // North = +Y, East = +X, Up = +Z
    let x = alt.cos() * az.sin(); // east component
    let y = alt.cos() * az.cos(); // north component
    let z = alt.sin(); // up component
    [x as f32, y as f32, z as f32]
}

/// Astronomical daylight hours for a date/latitude — the length of time the
/// centre of the sun is above the true horizon (altitude > 0), ignoring
/// atmospheric refraction and local terrain. This is the theoretical maximum an
/// unobstructed, sky-facing point can receive, and is the reference an
/// occlusion-free insolation sample should approach.
///
/// Uses the standard sunrise hour-angle equation
/// `cos(H0) = -tan(lat)·tan(decl)` with the sun's declination taken from the
/// SPA solar-noon position (accurate to well within a minute for our purposes).
/// Handles polar day (returns 24 h) and polar night (returns 0 h).
pub fn daylight_hours(year: i32, month: u32, day: u32, lat_deg: f64) -> f64 {
    // Declination at solar noon UTC on this date is a good representative value
    // for the whole (short) day. Recover it from the noon altitude/azimuth by
    // re-deriving: altitude at the meridian gives decl for a known latitude.
    // Simpler: sample the SPA at a longitude/time whose hour angle is ~0.
    // We approximate decl via the altitude at local solar noon where
    // altitude = 90 - |lat - decl|, so decl = lat - (90 - alt) with the sun to
    // the south, or lat + (90 - alt) to the north. To avoid the branch we take
    // decl directly from a small dedicated computation below.
    let decl = sun_declination(year, month, day);
    let lat = lat_deg.to_radians();
    let d = decl.to_radians();
    let cos_h0 = -lat.tan() * d.tan();
    if cos_h0 <= -1.0 {
        24.0 // sun never sets (polar day)
    } else if cos_h0 >= 1.0 {
        0.0 // sun never rises (polar night)
    } else {
        // H0 in radians; total daylight spans 2·H0. Convert to hours:
        // the sun sweeps 15°/h, so hours = 2·H0(deg)/15 = 2·H0(rad)·(12/π).
        let h0 = cos_h0.acos(); // radians, [0, π]
        2.0 * h0 * (12.0 / std::f64::consts::PI)
    }
}

/// Sun declination (degrees) at ~solar-noon UTC on a date. Extracted from the
/// same series `solar_position` uses so the two stay consistent.
fn sun_declination(year: i32, month: u32, day: u32) -> f64 {
    let (y, m) = if month <= 2 {
        (year as f64 - 1.0, month as f64 + 12.0)
    } else {
        (year as f64, month as f64)
    };
    let a = (y / 100.0).floor();
    let b = 2.0 - a + (a / 4.0).floor();
    let day_frac = day as f64 + 0.5; // noon UTC
    let jd = (365.25 * (y + 4716.0)).floor()
        + (30.6001 * (m + 1.0)).floor()
        + day_frac
        + b
        - 1524.5;
    let t = (jd - 2_451_545.0) / 36_525.0;
    let m_sun = (357.52911 + t * (35_999.050_29 - t * 0.0001537)).rem_euclid(360.0);
    let m_rad = m_sun.to_radians();
    let c = (1.914602 - t * (0.004817 + 0.000014 * t)) * m_rad.sin()
        + (0.019993 - 0.000101 * t) * (2.0 * m_rad).sin()
        + 0.000289 * (3.0 * m_rad).sin();
    let l0 = (280.46646 + t * (36_000.769_83 + t * 0.0003032)).rem_euclid(360.0);
    let sun_lon = l0 + c;
    let omega = 125.04 - 1934.136 * t;
    let sun_app_lon = sun_lon - 0.00569 - 0.00478 * omega.to_radians().sin();
    let mean_obliq = 23.0
        + (26.0 + (21.448 - t * (46.8150 + t * (0.00059 - t * 0.001813))) / 60.0) / 60.0;
    let obliq_corr = mean_obliq + 0.00256 * omega.to_radians().cos();
    let sun_app_rad = sun_app_lon.to_radians();
    let obliq_rad = obliq_corr.to_radians();
    f64::asin(obliq_rad.sin() * sun_app_rad.sin()).to_degrees()
}

// ---------------------------------------------------------------------------
// Environmental analysis geometry: shadow projection + ray-casting.
// Pure f64 math, no GPU/doc dependencies, so it lives here beside the SPA and
// is unit-testable in isolation.
// ---------------------------------------------------------------------------

/// A 3D point/vector in doc space (X=East, Y=North, Z=Up), f64.
pub type Vec3 = [f64; 3];

fn sub(a: Vec3, b: Vec3) -> Vec3 {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}
fn cross(a: Vec3, b: Vec3) -> Vec3 {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}
fn dot(a: Vec3, b: Vec3) -> f64 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

/// Project a point onto the ground plane `z = 0` by casting a ray from the
/// point *away* from the sun (i.e. along `-sun_dir`, the direction light
/// travels) until it hits `z = 0`. `sun_dir` points **toward** the sun and must
/// have a positive altitude (`sun_dir[2] > 0`); otherwise the sun is at/below
/// the horizon and no ground shadow exists (returns `None`).
///
/// A point already on or below the ground projects to its own XY.
pub fn project_to_ground(p: Vec3, sun_dir: Vec3) -> Option<Vec3> {
    let up = sun_dir[2];
    if up <= 1e-9 {
        return None; // sun at/below horizon — no finite ground shadow
    }
    if p[2] <= 0.0 {
        return Some([p[0], p[1], 0.0]);
    }
    // Travel along -sun_dir a distance t such that p.z - t*up = 0 → t = p.z/up.
    let t = p[2] / up;
    Some([p[0] - t * sun_dir[0], p[1] - t * sun_dir[1], 0.0])
}

/// 2D convex hull (Andrew's monotone chain) of XY points; returns the hull in
/// CCW order without the closing duplicate. Points are `(x, y)`.
pub fn convex_hull_xy(mut pts: Vec<[f64; 2]>) -> Vec<[f64; 2]> {
    pts.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    pts.dedup();
    let n = pts.len();
    if n < 3 {
        return pts;
    }
    let cross2 = |o: [f64; 2], a: [f64; 2], b: [f64; 2]| {
        (a[0] - o[0]) * (b[1] - o[1]) - (a[1] - o[1]) * (b[0] - o[0])
    };
    let mut hull: Vec<[f64; 2]> = Vec::with_capacity(2 * n);
    // lower
    for &p in &pts {
        while hull.len() >= 2 && cross2(hull[hull.len() - 2], hull[hull.len() - 1], p) <= 0.0 {
            hull.pop();
        }
        hull.push(p);
    }
    // upper
    let lower_len = hull.len() + 1;
    for &p in pts.iter().rev() {
        while hull.len() >= lower_len
            && cross2(hull[hull.len() - 2], hull[hull.len() - 1], p) <= 0.0
        {
            hull.pop();
        }
        hull.push(p);
    }
    hull.pop(); // last point == first
    hull
}

/// Möller–Trumbore ray/triangle intersection. Returns the ray parameter `t > eps`
/// at the hit (distance along `dir`), or `None` if the ray misses. `dir` need
/// not be normalized; `t` is in units of `dir`'s length.
pub fn ray_triangle(origin: Vec3, dir: Vec3, v0: Vec3, v1: Vec3, v2: Vec3) -> Option<f64> {
    const EPS: f64 = 1e-9;
    let e1 = sub(v1, v0);
    let e2 = sub(v2, v0);
    let pvec = cross(dir, e2);
    let det = dot(e1, pvec);
    if det.abs() < EPS {
        return None; // ray parallel to triangle
    }
    let inv_det = 1.0 / det;
    let tvec = sub(origin, v0);
    let u = dot(tvec, pvec) * inv_det;
    if !(-EPS..=1.0 + EPS).contains(&u) {
        return None;
    }
    let qvec = cross(tvec, e1);
    let v = dot(dir, qvec) * inv_det;
    if v < -EPS || u + v > 1.0 + EPS {
        return None;
    }
    let t = dot(e2, qvec) * inv_det;
    if t > EPS {
        Some(t)
    } else {
        None
    }
}

/// Parsed EnergyPlus Weather (EPW) file summary. Header location plus a light
/// summary of the 8760 hourly rows — no heavy data is retained.
#[derive(Clone, Debug, PartialEq)]
pub struct EpwSummary {
    pub city: String,
    pub lat_deg: f64,
    pub lon_deg: f64,
    /// Time-zone offset from UTC in hours (east positive), from the LOCATION line.
    pub tz_hours: f64,
    pub elevation_m: f64,
    /// Number of data rows parsed.
    pub rows: usize,
    /// Mean dry-bulb temperature (°C) across parsed rows, if any.
    pub mean_dry_bulb_c: Option<f64>,
    pub min_dry_bulb_c: Option<f64>,
    pub max_dry_bulb_c: Option<f64>,
}

/// Parse an EPW file: the `LOCATION` header line (fields 6–9 = lat, lon, tz,
/// elevation) plus every data row's dry-bulb temperature (field index 6).
///
/// EPW LOCATION line:
/// `LOCATION,City,State,Country,Source,WMO,Lat,Lon,TZ,Elevation`
/// Data rows begin after the 8 header lines; dry-bulb is column index 6.
pub fn parse_epw(text: &str) -> Result<EpwSummary, String> {
    let mut city = String::new();
    let (mut lat, mut lon, mut tz, mut elev) = (f64::NAN, f64::NAN, f64::NAN, 0.0);
    let mut found_location = false;
    let mut rows = 0usize;
    let mut sum = 0.0f64;
    let mut min = f64::INFINITY;
    let mut max = f64::NEG_INFINITY;

    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let fields: Vec<&str> = line.split(',').collect();
        if fields[0].eq_ignore_ascii_case("LOCATION") {
            // LOCATION,City,State,Country,Source,WMO,Lat,Lon,TZ,Elev
            if fields.len() < 10 {
                return Err("LOCATION line has too few fields".into());
            }
            city = fields[1].to_string();
            lat = fields[6].trim().parse().map_err(|_| "bad latitude in LOCATION")?;
            lon = fields[7].trim().parse().map_err(|_| "bad longitude in LOCATION")?;
            tz = fields[8].trim().parse().map_err(|_| "bad timezone in LOCATION")?;
            elev = fields[9].trim().parse().unwrap_or(0.0);
            found_location = true;
            continue;
        }
        // A data row starts with a 4-digit year in field 0. Header keyword lines
        // (DESIGN CONDITIONS, TYPICAL/EXTREME PERIODS, GROUND TEMPERATURES,
        // HOLIDAYS/DAYLIGHT SAVINGS, COMMENTS 1/2, DATA PERIODS) are skipped.
        let year_ok = fields[0].len() == 4 && fields[0].chars().all(|c| c.is_ascii_digit());
        if year_ok && fields.len() > 6 {
            // 99.9 is the EPW missing-data sentinel for dry-bulb — skip it.
            let db = fields[6].trim().parse::<f64>().ok().filter(|d| (d - 99.9).abs() > 1e-6);
            if let Some(db) = db {
                rows += 1;
                sum += db;
                min = min.min(db);
                max = max.max(db);
            }
        }
    }

    if !found_location {
        return Err("no LOCATION line found (not an EPW file?)".into());
    }
    let (mean, mn, mx) = if rows > 0 {
        (Some(sum / rows as f64), Some(min), Some(max))
    } else {
        (None, None, None)
    };
    Ok(EpwSummary {
        city,
        lat_deg: lat,
        lon_deg: lon,
        tz_hours: tz,
        elevation_m: elev,
        rows,
        mean_dry_bulb_c: mean,
        min_dry_bulb_c: mn,
        max_dry_bulb_c: mx,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOL: f64 = 0.5; // ±0.5° tolerance per task spec

    /// Helper: assert both az and alt are within TOL degrees of expected values.
    #[allow(clippy::too_many_arguments)] // test helper — grouping into a struct would add noise
    fn assert_sun(
        label: &str,
        year: i32, month: u32, day: u32, h: u32, min: u32,
        lat: f64, lon: f64,
        expect_az: f64,
        expect_alt: f64,
    ) {
        let s = solar_position(year, month, day, h, min, lat, lon);
        assert!(
            (s.azimuth_deg - expect_az).abs() <= TOL,
            "{label}: azimuth {:.3}° expected {expect_az:.3}° (delta {:.3}°)",
            s.azimuth_deg, (s.azimuth_deg - expect_az).abs()
        );
        assert!(
            (s.altitude_deg - expect_alt).abs() <= TOL,
            "{label}: altitude {:.3}° expected {expect_alt:.3}° (delta {:.3}°)",
            s.altitude_deg, (s.altitude_deg - expect_alt).abs()
        );
    }

    // --- NOAA Solar Calculator verification cases ---
    //
    // Expected values (az, alt) are computed by running this NOAA simplified SPA
    // implementation and cross-checked against analytic solar geometry:
    //   - Solar noon: alt ≈ 90° - |lat - decl|, az = 180° (N hemi) or 0° (S hemi)
    //   - Symmetric ±h from noon: same altitude, az symmetric around 180°
    //
    // Reference: NOAA Solar Calculator equations
    //   https://www.esrl.noaa.gov/gmd/grad/solcalc/calcdetails.html

    #[test]
    fn new_york_solar_noon_summer_solstice() {
        // New York (40.71°N, -74.01°E) on 2024-06-21.
        // Solar noon ≈ 16:58 UTC (EDT = UTC-4, eqtime ≈ -2 min).
        // Analytic alt check: 90 - (40.71 - 23.44) = 72.73°, az = 180° at noon.
        assert_sun(
            "NY summer solstice noon",
            2024, 6, 21, 16, 58,
            40.71, -74.01,
            180.0, 72.7,
        );
    }

    #[test]
    fn new_york_morning_summer_solstice() {
        // New York (40.71°N, -74.01°E) on 2024-06-21 at 13:58 UTC (3h before noon).
        // Symmetric case: expect az < 180° (east of south) and same alt as +3h.
        assert_sun(
            "NY summer solstice -3h",
            2024, 6, 21, 13, 58,
            40.71, -74.01,
            100.6, 48.7,
        );
    }

    #[test]
    fn new_york_evening_summer_solstice() {
        // New York (40.71°N, -74.01°E) on 2024-06-21 at 19:58 UTC (3h after noon).
        // Symmetric with morning case: az = 360 - 100.6 = 259.4°, same alt ≈ 48.7°.
        assert_sun(
            "NY summer solstice +3h",
            2024, 6, 21, 19, 58,
            40.71, -74.01,
            259.4, 48.7,
        );
    }

    #[test]
    fn london_winter_solstice_noon() {
        // London (51.51°N, -0.13°E) on 2024-12-21 at 12:01 UTC ≈ solar noon.
        // Analytic alt: 90 - (51.51 + 23.44) = 15.05°, az ≈ 180°.
        assert_sun(
            "London winter solstice noon",
            2024, 12, 21, 12, 1,
            51.51, -0.13,
            180.5, 15.0,
        );
    }

    #[test]
    fn sydney_summer_near_noon() {
        // Sydney (-33.87°S, 151.21°E) on 2024-01-15.
        // Solar noon at 02:09 UTC. At 02:00 UTC the sun is nearly overhead, az≈N.
        // Southern hemisphere: solar noon az ≈ 0°/360° (north-facing sun).
        assert_sun(
            "Sydney summer near noon",
            2024, 1, 15, 2, 0,
            -33.87, 151.21,
            4.5, 77.3,
        );
    }

    #[test]
    fn madrid_equinox_noon() {
        // Madrid (40.42°N, -3.70°E) on 2024-03-20 at 12:22 UTC = solar noon.
        // Analytic: decl≈0.15° on equinox, alt = 90 - (40.42 - 0.15) = 49.73°, az=180°.
        assert_sun(
            "Madrid equinox noon",
            2024, 3, 20, 12, 22,
            40.42, -3.70,
            180.0, 49.7,
        );
    }

    #[test]
    fn sun_direction_north() {
        // Azimuth 0° (north), altitude 0° → direction (0, 1, 0)
        let d = sun_direction(0.0, 0.0);
        assert!((d[0]).abs() < 1e-5, "x={}", d[0]);
        assert!((d[1] - 1.0).abs() < 1e-5, "y={}", d[1]);
        assert!((d[2]).abs() < 1e-5, "z={}", d[2]);
    }

    #[test]
    fn sun_direction_east_horizon() {
        // Azimuth 90° (east), altitude 0° → direction (1, 0, 0)
        let d = sun_direction(90.0, 0.0);
        assert!((d[0] - 1.0).abs() < 1e-5, "x={}", d[0]);
        assert!((d[1]).abs() < 1e-5, "y={}", d[1]);
        assert!((d[2]).abs() < 1e-5, "z={}", d[2]);
    }

    #[test]
    fn sun_direction_zenith() {
        // Any azimuth, altitude 90° → direction (0, 0, 1)
        let d = sun_direction(180.0, 90.0);
        assert!((d[0]).abs() < 1e-5, "x={}", d[0]);
        assert!((d[1]).abs() < 1e-5, "y={}", d[1]);
        assert!((d[2] - 1.0).abs() < 1e-5, "z={}", d[2]);
    }

    // --- shadow projection math ---

    #[test]
    fn project_overhead_sun_casts_point_straight_down() {
        // Sun at zenith (straight up): a point at (2,3,5) projects to (2,3,0).
        let g = project_to_ground([2.0, 3.0, 5.0], [0.0, 0.0, 1.0]).unwrap();
        assert!((g[0] - 2.0).abs() < 1e-9 && (g[1] - 3.0).abs() < 1e-9 && g[2] == 0.0);
    }

    #[test]
    fn project_45deg_sun_offsets_by_height() {
        // Sun at 45° altitude toward +X (east): dir = (cos45,0,sin45).
        // A point at height 10 casts a shadow 10 units to the WEST (away from sun).
        let s = std::f64::consts::FRAC_1_SQRT_2;
        let g = project_to_ground([0.0, 0.0, 10.0], [s, 0.0, s]).unwrap();
        assert!((g[0] - (-10.0)).abs() < 1e-6, "x={}", g[0]);
        assert!(g[1].abs() < 1e-9 && g[2] == 0.0);
    }

    #[test]
    fn project_sun_below_horizon_is_none() {
        assert!(project_to_ground([0.0, 0.0, 5.0], [1.0, 0.0, -0.1]).is_none());
        assert!(project_to_ground([0.0, 0.0, 5.0], [1.0, 0.0, 0.0]).is_none());
    }

    #[test]
    fn hull_of_square_is_four_corners() {
        // Square plus an interior point → 4-corner hull.
        let pts = vec![
            [0.0, 0.0],
            [1.0, 0.0],
            [1.0, 1.0],
            [0.0, 1.0],
            [0.5, 0.5],
        ];
        let hull = convex_hull_xy(pts);
        assert_eq!(hull.len(), 4, "hull={hull:?}");
    }

    // --- ray/triangle occlusion ---

    #[test]
    fn ray_hits_triangle_above_origin() {
        // Triangle in the plane z=5 spanning the origin; ray straight up hits it.
        let (v0, v1, v2) = ([-1.0, -1.0, 5.0], [2.0, -1.0, 5.0], [-1.0, 2.0, 5.0]);
        let t = ray_triangle([0.0, 0.0, 0.0], [0.0, 0.0, 1.0], v0, v1, v2);
        assert!(t.is_some(), "expected hit");
        assert!((t.unwrap() - 5.0).abs() < 1e-6, "t={:?}", t);
    }

    #[test]
    fn ray_misses_triangle_to_the_side() {
        let (v0, v1, v2) = ([-1.0, -1.0, 5.0], [0.0, -1.0, 5.0], [-1.0, 0.0, 5.0]);
        // Ray up from (5,5,0) is nowhere near the triangle near the origin.
        assert!(ray_triangle([5.0, 5.0, 0.0], [0.0, 0.0, 1.0], v0, v1, v2).is_none());
    }

    // --- EPW header parse ---

    #[test]
    fn epw_header_and_rows() {
        // Synthetic EPW: LOCATION line + 8 header lines + two data rows.
        let epw = "\
LOCATION,Denver Intl Ap,CO,USA,TMY3,725650,39.83,-104.65,-7.0,1650.0
DESIGN CONDITIONS,0
TYPICAL/EXTREME PERIODS,0
GROUND TEMPERATURES,0
HOLIDAYS/DAYLIGHT SAVINGS,No,0,0,0
COMMENTS 1,synthetic
COMMENTS 2,test
DATA PERIODS,1,1,Data,Sunday,1/1,12/31
1999,1,1,1,60,A7,10.0,5.0,80,81100
1999,1,1,2,60,A7,20.0,6.0,78,81100
";
        let s = parse_epw(epw).unwrap();
        assert_eq!(s.city, "Denver Intl Ap");
        assert!((s.lat_deg - 39.83).abs() < 1e-6);
        assert!((s.lon_deg - (-104.65)).abs() < 1e-6);
        assert!((s.tz_hours - (-7.0)).abs() < 1e-6);
        assert!((s.elevation_m - 1650.0).abs() < 1e-6);
        assert_eq!(s.rows, 2);
        assert!((s.mean_dry_bulb_c.unwrap() - 15.0).abs() < 1e-6);
        assert!((s.min_dry_bulb_c.unwrap() - 10.0).abs() < 1e-6);
        assert!((s.max_dry_bulb_c.unwrap() - 20.0).abs() < 1e-6);
    }

    #[test]
    fn epw_missing_location_errors() {
        assert!(parse_epw("1999,1,1,1,60,A7,10.0\n").is_err());
    }

    // --- daylight_hours (astronomical daylight duration) ---

    #[test]
    fn daylight_equinox_is_about_twelve_hours_everywhere() {
        // At an equinox the sun is ~on the celestial equator, so every latitude
        // gets ≈12 h of daylight (geometric-centre definition, no refraction).
        for lat in [-45.0, -10.0, 0.0, 23.5, 51.5, 60.0] {
            let h = daylight_hours(2024, 3, 20, lat);
            assert!(
                (h - 12.0).abs() < 0.25,
                "equinox at lat {lat}: {h:.3} h, expected ~12"
            );
        }
    }

    #[test]
    fn daylight_summer_longer_than_winter_in_north() {
        // London (51.5°N): long summer day, short winter day, summing to ~24 h
        // across the solstices (the pair is symmetric about the equinox).
        let summer = daylight_hours(2024, 6, 21, 51.5);
        let winter = daylight_hours(2024, 12, 21, 51.5);
        assert!(summer > 16.0 && summer < 17.0, "London summer {summer:.2} h");
        assert!(winter > 7.0 && winter < 8.5, "London winter {winter:.2} h");
        assert!(summer > winter);
        // Symmetry: summer + winter ≈ 24 h.
        assert!((summer + winter - 24.0).abs() < 0.6, "sum {:.2}", summer + winter);
    }

    #[test]
    fn daylight_polar_day_and_night() {
        // Above the Arctic Circle (78°N): 24 h sun in June, 0 h in December.
        assert_eq!(daylight_hours(2024, 6, 21, 78.0), 24.0);
        assert_eq!(daylight_hours(2024, 12, 21, 78.0), 0.0);
    }

    #[test]
    fn daylight_matches_ny_solstice() {
        // New York 40.71°N on the summer solstice: ~15 h of daylight.
        let h = daylight_hours(2024, 6, 21, 40.71);
        assert!(h > 14.8 && h < 15.3, "NY solstice daylight {h:.3} h");
    }
}
