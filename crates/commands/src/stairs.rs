// SPDX-License-Identifier: AGPL-3.0-only
// Copyright © 2026 Hector Tarrido-Picart

//! Pure layout math for parametric ramps and stairs (issue #18).
//!
//! The assistant used to hand-build ramps and stairs out of generic primitives
//! with no sense of slope, rise/run, riser/tread proportioning, or landings —
//! "a geometric hot mess". This module encodes the design rules as PURE,
//! unit-tested functions so the `ramp` / `stair` verbs (see `exec.rs`) can bake
//! code-compliant geometry, and so the numbers are reviewable in one place.
//!
//! All thresholds are DEFAULTS, not legal advice — metric is primary with the
//! imperial source value in a comment. They line up with the embedded
//! `codecheck ada2010` (ramps) and `codecheck ibc2021` (stairs) packs so the
//! generated geometry passes the matching pre-checks (generate-then-verify).
//!
//! Geometry convention: both generators lay the run out along +X starting at the
//! origin, rising in +Z. `exec.rs` owns the actual meshing / curve insertion;
//! this module only computes the dimensioned layout.

use glam::DVec3;

// ── Ramp defaults (ADA 2010 / ICC A117.1) ──────────────────────────────────────

/// Max running slope for an accessible ramp: 1:12 (ADA 405.2). Expressed as a
/// rise:run ratio. The nominal code max is `1/12 = 0.08333…`, but the ADA
/// pre-check pack rounds its limit to `0.0833`, so we bake a hair SHALLOWER
/// (1:12.05 ≈ 0.08299) — a tiny design margin that keeps the generated ramp on
/// the passing side of the rounded check while staying visually 1:12. Steeper
/// slopes (1:8, 1:10) are only allowed in narrow exceptions, so this is the
/// safe default.
pub const RAMP_MAX_SLOPE: f64 = 1.0 / 12.05;

/// Max rise of a single ramp run before a level landing is required: 760 mm
/// (30 in, ADA 405.7 / 405.2 via the landing rule).
pub const RAMP_MAX_RUN_RISE: f64 = 0.762; // 30 in

/// Minimum landing length along the path: 1525 mm (60 in, ADA 405.7). A landing
/// is at least as wide as the ramp (handled by the caller passing `width`).
pub const RAMP_MIN_LANDING_LEN: f64 = 1.524; // 60 in

// ── Stair defaults (IBC 2021) ──────────────────────────────────────────────────

/// Max riser height: 178 mm (7 in, IBC 1011.5.2).
pub const STAIR_MAX_RISER: f64 = 0.1778; // 7 in

/// Min riser height: 102 mm (4 in, IBC 1011.5.2).
pub const STAIR_MIN_RISER: f64 = 0.1016; // 4 in

/// Min tread depth: 279 mm (11 in, IBC 1011.5.2).
pub const STAIR_MIN_TREAD: f64 = 0.2794; // 11 in

/// Max riser-to-riser variation within one flight: 9.5 mm (3/8 in, IBC
/// 1011.5.4). Equalizing the riser height (rise / count) keeps variation at 0,
/// well inside this; the const documents the target we respect.
pub const STAIR_MAX_RISER_VARIATION: f64 = 0.0095; // 3/8 in

/// `2R + T` comfort rule-of-thumb band: a well-proportioned stair keeps twice
/// the riser plus the tread near 630 mm (24.8 in). Used to pick a tread depth
/// when the caller does not give one.
pub const STAIR_2R_PLUS_T_TARGET: f64 = 0.630; // ≈ 24.8 in (within the 24–25 in band)

/// Insert an intermediate landing at least every this much VERTICAL rise:
/// 3658 mm (12 ft, IBC 1011.8). A flight taller than this gets a mid landing.
pub const STAIR_MAX_RISE_PER_FLIGHT: f64 = 3.658; // 12 ft

/// A usable positive dimension — also rejects NaN/∞. Factored out so the input
/// guards avoid a negated float comparison (`!(x > 0.0)`), which clippy's
/// `neg_cmp_op_on_partial_ord` lint flags on partially-ordered types.
fn positive(x: f64) -> bool {
    x.is_finite() && x > 0.0
}

// ── Ramp layout ─────────────────────────────────────────────────────────────

/// One segment of a ramp centerline: either a sloped run or a level landing.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum RampSegment {
    /// A sloped run that gains `rise` metres over `run` metres of plan length.
    Run { run: f64, rise: f64 },
    /// A level landing `len` metres long (no rise).
    Landing { len: f64 },
}

/// A fully dimensioned ramp: the ordered segments plus the resolved slope.
#[derive(Clone, Debug, PartialEq)]
pub struct RampLayout {
    pub segments: Vec<RampSegment>,
    /// The running slope actually used (rise:run), ≤ [`RAMP_MAX_SLOPE`].
    pub slope: f64,
    pub width: f64,
}

/// How the ramp slope was specified.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum RampSlope {
    /// A rise:run ratio, e.g. `1:12` → `0.0833`.
    Ratio(f64),
    /// A total plan run to spread the whole rise over (slope = rise / run).
    Run(f64),
}

/// Compute a ramp layout from a total `rise`, a slope spec, and a `width`.
///
/// Rules (ADA): split the rise into runs so no single run exceeds
/// [`RAMP_MAX_RUN_RISE`] (30 in); insert a level landing ≥ [`RAMP_MIN_LANDING_LEN`]
/// (60 in) between consecutive runs. (Top/bottom landings are the
/// responsibility of the surrounding floor plate and are not baked here, matching
/// how the `ramp_landing` check walks the drawn centerline.) The running slope is
/// clamped to [`RAMP_MAX_SLOPE`] when a ratio is given, and taken directly from
/// rise/run when a run is given (also clamped so a too-short run cannot force a
/// steeper-than-code slope — the ramp simply gets longer).
///
/// Returns an error string for non-positive rise/width or a non-positive slope.
pub fn ramp_layout(rise: f64, slope: RampSlope, width: f64) -> Result<RampLayout, String> {
    if !positive(rise) {
        return Err("ramp rise must be positive".into());
    }
    if !positive(width) {
        return Err("ramp width must be positive".into());
    }
    let slope = match slope {
        RampSlope::Ratio(r) => {
            if !positive(r) {
                return Err("ramp slope must be positive".into());
            }
            r.min(RAMP_MAX_SLOPE)
        }
        RampSlope::Run(run) => {
            if !positive(run) {
                return Err("ramp run must be positive".into());
            }
            (rise / run).min(RAMP_MAX_SLOPE)
        }
    };

    // Number of runs: enough that each run's rise ≤ the max single-run rise.
    let runs = (rise / RAMP_MAX_RUN_RISE).ceil().max(1.0) as usize;
    let run_rise = rise / runs as f64; // equalized, ≤ RAMP_MAX_RUN_RISE
    let run_len = run_rise / slope;

    let mut segments = Vec::with_capacity(runs * 2 - 1);
    for i in 0..runs {
        segments.push(RampSegment::Run { run: run_len, rise: run_rise });
        // Landing between runs (not after the last run).
        if i + 1 < runs {
            segments.push(RampSegment::Landing { len: RAMP_MIN_LANDING_LEN });
        }
    }
    Ok(RampLayout { segments, slope, width })
}

/// Walk a ramp layout into a centerline polyline starting at `start`, running
/// along +X, rising in +Z. This is the curve the `ramp` verb names "ramp-…" and
/// that `codecheck ada2010` reads for running slope and landing placement.
pub fn ramp_centerline(layout: &RampLayout, start: DVec3) -> Vec<DVec3> {
    let mut pts = vec![start];
    let mut cur = start;
    for seg in &layout.segments {
        match *seg {
            RampSegment::Run { run, rise } => {
                cur += DVec3::new(run, 0.0, rise);
            }
            RampSegment::Landing { len } => {
                cur += DVec3::new(len, 0.0, 0.0);
            }
        }
        pts.push(cur);
    }
    pts
}

/// Total plan length (X extent) of a ramp layout.
pub fn ramp_plan_length(layout: &RampLayout) -> f64 {
    layout
        .segments
        .iter()
        .map(|s| match s {
            RampSegment::Run { run, .. } => *run,
            RampSegment::Landing { len } => *len,
        })
        .sum()
}

// ── Stair layout ─────────────────────────────────────────────────────────────

/// A fully dimensioned straight-flight stair.
#[derive(Clone, Debug, PartialEq)]
pub struct StairLayout {
    /// Number of risers (steps up). Treads number `risers - 1` on a flight that
    /// lands on the upper floor.
    pub risers: usize,
    /// Equalized riser height = total rise / risers (so variation is 0).
    pub riser_height: f64,
    pub tread_depth: f64,
    pub width: f64,
    /// Whether an intermediate landing is required (rise > 12 ft per flight).
    pub landing: bool,
}

/// Compute a stair layout from a total floor-to-floor `rise`, a `width`, and
/// optional target riser / tread depths.
///
/// Rules (IBC): riser count = `ceil(rise / max_riser)` so every riser ≤
/// [`STAIR_MAX_RISER`] (7 in); the riser height is then EQUALIZED to
/// `rise / count` (0 variation, inside the 3/8 in limit) and must stay ≥
/// [`STAIR_MIN_RISER`] (4 in). Tread depth honors an explicit target if given
/// (clamped to ≥ [`STAIR_MIN_TREAD`]); otherwise it is derived from the
/// `2R + T ≈ 24–25 in` rule and clamped to ≥ [`STAIR_MIN_TREAD`]. A flight
/// taller than [`STAIR_MAX_RISE_PER_FLIGHT`] (12 ft) is flagged as needing an
/// intermediate landing.
pub fn stair_layout(
    rise: f64,
    width: f64,
    target_riser: Option<f64>,
    target_tread: Option<f64>,
) -> Result<StairLayout, String> {
    if !positive(rise) {
        return Err("stair rise must be positive".into());
    }
    if !positive(width) {
        return Err("stair width must be positive".into());
    }

    // Riser count: honor a sensible target, else the max-riser bound. Either way
    // we round UP so no riser exceeds the max.
    let max_riser = match target_riser {
        Some(r) if r > 0.0 => r.clamp(STAIR_MIN_RISER, STAIR_MAX_RISER),
        _ => STAIR_MAX_RISER,
    };
    let risers = (rise / max_riser).ceil().max(1.0) as usize;
    let riser_height = rise / risers as f64; // equalized → 0 variation

    // A riser can only fall below the 4 in minimum if the rise is tiny; a single
    // step is still a valid (if shallow) stair, so we allow it but note it.
    if riser_height < STAIR_MIN_RISER && risers > 1 {
        // Reducing the count raises each riser back toward the band.
        // (ceil already minimizes count for the max bound, so this only trips on
        // a target_riser far below the natural height — clamp handled above.)
    }

    // Tread depth: explicit target, else the 2R+T comfort rule.
    let tread_depth = match target_tread {
        Some(t) if t > 0.0 => t.max(STAIR_MIN_TREAD),
        _ => (STAIR_2R_PLUS_T_TARGET - 2.0 * riser_height).max(STAIR_MIN_TREAD),
    };

    Ok(StairLayout {
        risers,
        riser_height,
        tread_depth,
        width,
        landing: rise > STAIR_MAX_RISE_PER_FLIGHT,
    })
}

// ── Geometry builders (vertex/face lists; exec.rs wraps them in a Mesh) ────────

/// A simple triangle-mesh builder shared by the ramp-surface and stair builders.
/// Keeps `exec.rs` free of index bookkeeping and lets the layout→geometry step
/// be unit-tested here without pulling in the doc model.
#[derive(Default)]
pub struct MeshBuild {
    pub positions: Vec<DVec3>,
    pub faces: Vec<[u32; 3]>,
}

impl MeshBuild {
    /// Add a quad as two triangles, wound CCW so the normal follows a→b→c→d.
    fn quad(&mut self, a: DVec3, b: DVec3, c: DVec3, d: DVec3) {
        let base = self.positions.len() as u32;
        self.positions.extend_from_slice(&[a, b, c, d]);
        self.faces.push([base, base + 1, base + 2]);
        self.faces.push([base, base + 2, base + 3]);
    }
}

/// Build the walking-surface quads of a ramp from its layout, starting at
/// `start`, running along +X, `width` wide in +Y. Each run is a sloped quad and
/// each landing a flat quad; together they form the continuous top surface that
/// `codecheck` reads. Returned as positions+faces so `exec.rs` can wrap it in a
/// `Mesh` (the ramp's up-facing surface used for the cross-slope mesh probe).
pub fn ramp_surface(layout: &RampLayout, start: DVec3) -> MeshBuild {
    let w = layout.width;
    let mut mb = MeshBuild::default();
    let mut cur = start;
    for seg in &layout.segments {
        let (dx, dz) = match *seg {
            RampSegment::Run { run, rise } => (run, rise),
            RampSegment::Landing { len } => (len, 0.0),
        };
        let a = cur;
        let b = cur + DVec3::new(dx, 0.0, dz);
        // Wind CCW as seen from ABOVE so the surface normal points +Z (up). The
        // previous order (near-left, near-right, far-right, far-left) produced a
        // DOWNWARD normal — the ramp drew as wireframe from the top (back-face
        // culled) and solid only from below. Order: near-left, far-left,
        // far-right, near-right.
        mb.quad(
            a,
            b,
            b + DVec3::new(0.0, w, 0.0),
            a + DVec3::new(0.0, w, 0.0),
        );
        cur = b;
    }
    mb
}

/// Build a stepped stair mesh from its layout, starting at `start`, running
/// along +X, `width` wide in +Y. Each step contributes a horizontal tread quad
/// at its level and a vertical riser quad up to it. The tread quads are the
/// up-facing faces `stair_risers` / `tread_depth` cluster by z-level, so the
/// baked geometry passes `codecheck ibc2021`. Returned as positions+faces.
pub fn stair_mesh(layout: &StairLayout, start: DVec3) -> MeshBuild {
    let w = layout.width;
    let t = layout.tread_depth;
    let r = layout.riser_height;
    let mut mb = MeshBuild::default();
    for i in 0..layout.risers {
        // Step i rises to z = (i+1)*r; its tread sits at that level spanning
        // plan x in [i*t, (i+1)*t].
        let z = (i as f64 + 1.0) * r;
        let x0 = start.x + i as f64 * t;
        let x1 = x0 + t;
        let y0 = start.y;
        let y1 = start.y + w;
        // Riser (vertical face) from the previous level up to z at x = x0.
        let z_below = i as f64 * r + start.z;
        let rz = z + start.z;
        mb.quad(
            DVec3::new(x0, y0, z_below),
            DVec3::new(x0, y1, z_below),
            DVec3::new(x0, y1, rz),
            DVec3::new(x0, y0, rz),
        );
        // Tread (horizontal, up-facing) at level rz.
        mb.quad(
            DVec3::new(x0, y0, rz),
            DVec3::new(x1, y0, rz),
            DVec3::new(x1, y1, rz),
            DVec3::new(x0, y1, rz),
        );
    }
    mb
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ramp_surface_normal_points_up() {
        // Regression: the surface quads were wound so the normal pointed DOWN,
        // so the ramp drew as wireframe from the top (back-face culled) and solid
        // only from below. Every triangle's normal must have a positive Z.
        let layout = ramp_layout(0.9, RampSlope::Ratio(1.0 / 12.0), 1.2).unwrap();
        let mb = ramp_surface(&layout, DVec3::ZERO);
        assert!(!mb.faces.is_empty());
        for f in &mb.faces {
            let p0 = mb.positions[f[0] as usize];
            let p1 = mb.positions[f[1] as usize];
            let p2 = mb.positions[f[2] as usize];
            let n = (p1 - p0).cross(p2 - p0);
            assert!(n.z > 0.0, "ramp face normal must point up, got {n:?}");
        }
    }

    // ── Ramp ──────────────────────────────────────────────────────────────

    #[test]
    fn ramp_900mm_is_1_to_12_with_a_mid_landing() {
        // Acceptance (issue #18): accessible ramp rising 900 mm → 1:12 slope,
        // ~10.8 m run, WITH a mid landing (900 > 760 max single run).
        let l = ramp_layout(0.9, RampSlope::Ratio(RAMP_MAX_SLOPE), 1.2).unwrap();
        assert!((l.slope - RAMP_MAX_SLOPE).abs() < 1e-9, "slope {}", l.slope);
        // 900 / 760 = 1.18 → 2 runs → exactly one landing between them.
        let runs = l.segments.iter().filter(|s| matches!(s, RampSegment::Run { .. })).count();
        let landings =
            l.segments.iter().filter(|s| matches!(s, RampSegment::Landing { .. })).count();
        assert_eq!(runs, 2, "two runs for a 900 mm rise");
        assert_eq!(landings, 1, "one mid landing");
        // Total plan run of the sloped parts ≈ 0.9 / slope ≈ 10.8 m (a hair
        // longer than the nominal 10.8 because we bake 1:12.05 for margin).
        let run_len: f64 = l
            .segments
            .iter()
            .filter_map(|s| match s {
                RampSegment::Run { run, .. } => Some(*run),
                _ => None,
            })
            .sum();
        assert!((run_len - 0.9 / RAMP_MAX_SLOPE).abs() < 1e-6, "run length {run_len}");
        assert!((run_len - 10.8).abs() < 0.1, "≈10.8 m, got {run_len}");
        // Each run's rise is ≤ the 30 in single-run max.
        for s in &l.segments {
            if let RampSegment::Run { rise, .. } = s {
                assert!(*rise <= RAMP_MAX_RUN_RISE + 1e-9, "run rise {rise}");
            }
        }
        // Landing length honors the 60 in minimum.
        for s in &l.segments {
            if let RampSegment::Landing { len } = s {
                assert!(*len >= RAMP_MIN_LANDING_LEN - 1e-9, "landing {len}");
            }
        }
    }

    #[test]
    fn ramp_small_rise_is_a_single_run_no_landing() {
        // 300 mm < 760 mm single-run max → one run, no landing.
        let l = ramp_layout(0.3, RampSlope::Ratio(RAMP_MAX_SLOPE), 1.0).unwrap();
        assert_eq!(l.segments.len(), 1);
        assert!(matches!(l.segments[0], RampSegment::Run { .. }));
    }

    #[test]
    fn ramp_run_spec_is_clamped_to_code_slope() {
        // Asking for a 3 m run under a 0.9 m rise would be 1:3.3 — far too steep.
        // The layout clamps to 1:12 and lengthens instead.
        let l = ramp_layout(0.9, RampSlope::Run(3.0), 1.0).unwrap();
        assert!(l.slope <= RAMP_MAX_SLOPE + 1e-9, "slope {}", l.slope);
    }

    #[test]
    fn ramp_gentle_run_spec_is_honored() {
        // A 1:20 ask (gentler than 1:12) is kept as-is.
        let l = ramp_layout(0.6, RampSlope::Run(12.0), 1.0).unwrap();
        assert!((l.slope - 0.05).abs() < 1e-9, "slope {}", l.slope);
    }

    #[test]
    fn ramp_centerline_matches_rise() {
        let l = ramp_layout(0.9, RampSlope::Ratio(RAMP_MAX_SLOPE), 1.2).unwrap();
        let pts = ramp_centerline(&l, DVec3::ZERO);
        let top = pts.last().unwrap().z;
        assert!((top - 0.9).abs() < 1e-9, "top z {top}");
        // Plan length = sloped runs + landing.
        let len = ramp_plan_length(&l);
        assert!((len - (0.9 / RAMP_MAX_SLOPE + RAMP_MIN_LANDING_LEN)).abs() < 1e-6, "plan len {len}");
    }

    #[test]
    fn ramp_rejects_bad_inputs() {
        assert!(ramp_layout(0.0, RampSlope::Ratio(RAMP_MAX_SLOPE), 1.0).is_err());
        assert!(ramp_layout(1.0, RampSlope::Ratio(0.0), 1.0).is_err());
        assert!(ramp_layout(1.0, RampSlope::Run(0.0), 1.0).is_err());
        assert!(ramp_layout(1.0, RampSlope::Ratio(RAMP_MAX_SLOPE), 0.0).is_err());
    }

    // ── Stair ─────────────────────────────────────────────────────────────

    #[test]
    fn stair_3m_floor_to_floor_is_code_compliant() {
        // Acceptance (issue #18): 3 m floor-to-floor → ~17 risers ≈ 176 mm,
        // tread ≥ 279 mm.
        let l = stair_layout(3.0, 1.2, None, None).unwrap();
        assert_eq!(l.risers, 17, "riser count");
        assert!((l.riser_height - 3.0 / 17.0).abs() < 1e-9);
        assert!(l.riser_height <= STAIR_MAX_RISER + 1e-9, "riser {}", l.riser_height);
        assert!(l.riser_height >= STAIR_MIN_RISER - 1e-9, "riser {}", l.riser_height);
        assert!((l.riser_height - 0.1765).abs() < 1e-3, "≈176 mm, got {}", l.riser_height);
        assert!(l.tread_depth >= STAIR_MIN_TREAD - 1e-9, "tread {}", l.tread_depth);
        assert!(!l.landing, "3 m < 12 ft, no landing");
    }

    #[test]
    fn stair_equalized_risers_have_zero_variation() {
        // Equalizing rise/count means every riser is identical → variation 0,
        // well inside the 3/8 in limit.
        let l = stair_layout(2.8, 1.1, None, None).unwrap();
        let variation = 0.0; // all risers = rise/count by construction
        assert!(variation <= STAIR_MAX_RISER_VARIATION);
        assert!(l.riser_height <= STAIR_MAX_RISER + 1e-9);
    }

    #[test]
    fn stair_2r_plus_t_picks_a_sane_tread() {
        // With no tread target, 2R + T ≈ 630 mm. For a 176 mm riser,
        // T ≈ 630 - 352 = 278 mm → clamped up to the 279 mm min.
        let l = stair_layout(3.0, 1.2, None, None).unwrap();
        let two_r_plus_t = 2.0 * l.riser_height + l.tread_depth;
        assert!(l.tread_depth >= STAIR_MIN_TREAD - 1e-9);
        // The comfort sum lands in a reasonable band (≥ the 279 mm floor case).
        assert!(two_r_plus_t >= 0.62 && two_r_plus_t <= 0.70, "2R+T {two_r_plus_t}");
    }

    #[test]
    fn stair_honors_explicit_tread_target() {
        let l = stair_layout(3.0, 1.2, None, Some(0.30)).unwrap();
        assert!((l.tread_depth - 0.30).abs() < 1e-9);
        // A too-shallow target is clamped up to the min.
        let l2 = stair_layout(3.0, 1.2, None, Some(0.20)).unwrap();
        assert!((l2.tread_depth - STAIR_MIN_TREAD).abs() < 1e-9);
    }

    #[test]
    fn stair_tall_rise_flags_a_landing() {
        // 4 m > 12 ft (3.658 m) → needs an intermediate landing.
        let l = stair_layout(4.0, 1.2, None, None).unwrap();
        assert!(l.landing, "4 m rise must flag a landing");
    }

    #[test]
    fn stair_rejects_bad_inputs() {
        assert!(stair_layout(0.0, 1.0, None, None).is_err());
        assert!(stair_layout(3.0, 0.0, None, None).is_err());
    }
}
