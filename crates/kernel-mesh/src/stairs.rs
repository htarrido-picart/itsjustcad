// SPDX-License-Identifier: AGPL-3.0-only
// Copyright © 2026 Hector Tarrido-Picart

//! Pure layout math for parametric ramps and stairs (issues #18 / #83).
//!
//! The assistant used to hand-build ramps and stairs out of generic primitives
//! with no sense of slope, rise/run, riser/tread proportioning, or landings —
//! "a geometric hot mess". This module encodes the design rules as PURE,
//! unit-tested functions so the `ramp` / `stair` verbs (and the parametric
//! re-derive path, see `itsjustcad_doc::derive_mesh`) can bake code-compliant
//! geometry, and so the numbers are reviewable in one place.
//!
//! It lives in `kernel-mesh` (not `commands`) so the `doc` crate's parametric
//! `derive_mesh` — which only depends on `kernel-mesh` — can re-run the SAME
//! layout math on a param edit. `commands::stairs` re-exports from here so the
//! verb-side call sites and their imports are unchanged.
//!
//! All thresholds are DEFAULTS, not legal advice — metric is primary with the
//! imperial source value in a comment. They line up with the embedded
//! `codecheck ada2010` (ramps) and `codecheck ibc2021` (stairs) packs so the
//! generated geometry passes the matching pre-checks (generate-then-verify).
//!
//! Geometry convention: both generators lay the run out along +X starting at the
//! origin, rising in +Z. The ramp surface is `width` wide in +Y; the stair is
//! likewise `width` wide in +Y.

use crate::Mesh;
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

/// How a multi-run ramp/stair turns between runs (issue #82, enhancement #2).
///
/// A turning landing lets a tall ramp/stair fold back instead of running on in
/// one very long straight line. `None` is the v1 straight-along-+X behavior.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TurnKind {
    /// Straight run along +X (v1 behavior).
    #[default]
    None,
    /// 180° U-turn at each landing: successive runs reverse direction.
    Switchback,
    /// 90° dogleg at each landing: successive runs rotate a quarter turn.
    Dogleg,
}

impl TurnKind {
    /// Parse a stable token (used by the schema enum + verb parsing).
    pub fn from_token(s: &str) -> Self {
        match s {
            "switchback" => TurnKind::Switchback,
            "dogleg" => TurnKind::Dogleg,
            _ => TurnKind::None,
        }
    }
    /// Stable token for serialization / the schema enum.
    pub fn token(self) -> &'static str {
        match self {
            TurnKind::None => "none",
            TurnKind::Switchback => "switchback",
            TurnKind::Dogleg => "dogleg",
        }
    }
    /// The heading change (radians about +Z) applied at each turning landing.
    fn turn_angle(self) -> f64 {
        match self {
            TurnKind::None => 0.0,
            TurnKind::Switchback => std::f64::consts::PI, // 180°
            TurnKind::Dogleg => std::f64::consts::FRAC_PI_2, // 90°
        }
    }
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
///
/// This is the v1 (code-compliant) entry point; it forwards to
/// [`ramp_layout_opts`] with `noncompliant = false`.
pub fn ramp_layout(rise: f64, slope: RampSlope, width: f64) -> Result<RampLayout, String> {
    ramp_layout_opts(rise, slope, width, false)
}

/// Compute a ramp layout, optionally honoring a non-compliant (verbatim) slope.
///
/// Issue #82, enhancement #1: when `noncompliant` is `true` the slope is used
/// EXACTLY as asked — no clamp to [`RAMP_MAX_SLOPE`] — so a designer can author
/// an intentionally steep service ramp (1:8, 1:10). The run still splits across
/// landings by the single-run-rise rule, and `codecheck ada2010` is free to flag
/// the steep slope as a failure (informational — it reads the baked geometry, it
/// does not clamp). When `noncompliant` is `false` this behaves exactly as v1
/// (slope clamped to the ADA max).
pub fn ramp_layout_opts(
    rise: f64,
    slope: RampSlope,
    width: f64,
    noncompliant: bool,
) -> Result<RampLayout, String> {
    if !positive(rise) {
        return Err("ramp rise must be positive".into());
    }
    if !positive(width) {
        return Err("ramp width must be positive".into());
    }
    // Clamp to the ADA max UNLESS the caller opted into a non-compliant override.
    let clamp = |r: f64| if noncompliant { r } else { r.min(RAMP_MAX_SLOPE) };
    let slope = match slope {
        RampSlope::Ratio(r) => {
            if !positive(r) {
                return Err("ramp slope must be positive".into());
            }
            clamp(r)
        }
        RampSlope::Run(run) => {
            if !positive(run) {
                return Err("ramp run must be positive".into());
            }
            clamp(rise / run)
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
/// along +X, rising in +Z. This is the curve the `ramp` verb names "ramp" and
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
    stair_layout_opts(rise, width, target_riser, target_tread, false)
}

/// Compute a stair layout, optionally honoring non-compliant riser/tread values.
///
/// Issue #82, enhancement #1: when `noncompliant` is `true` the target riser is
/// NOT clamped to the IBC 4–7 in band and the target tread is NOT clamped to the
/// 11 in minimum — a designer can author an intentionally steep/non-standard
/// stair, and `codecheck ibc2021` is free to flag the riser/tread as a failure
/// (informational). When `false` this behaves exactly as v1.
pub fn stair_layout_opts(
    rise: f64,
    width: f64,
    target_riser: Option<f64>,
    target_tread: Option<f64>,
    noncompliant: bool,
) -> Result<StairLayout, String> {
    if !positive(rise) {
        return Err("stair rise must be positive".into());
    }
    if !positive(width) {
        return Err("stair width must be positive".into());
    }

    // Riser count: honor a sensible target, else the max-riser bound. Either way
    // we round UP so no riser exceeds the max. Under a non-compliant override the
    // target riser is used verbatim (not clamped to the 4–7 in band), so a steep
    // riser is honored and the equalized height can exceed 7 in.
    let max_riser = match target_riser {
        Some(r) if r > 0.0 => {
            if noncompliant {
                r
            } else {
                r.clamp(STAIR_MIN_RISER, STAIR_MAX_RISER)
            }
        }
        _ => STAIR_MAX_RISER,
    };
    let risers = (rise / max_riser).ceil().max(1.0) as usize;
    let riser_height = rise / risers as f64; // equalized → 0 variation

    // Tread depth: explicit target, else the 2R+T comfort rule. Under a
    // non-compliant override an explicit tread is NOT floored at the 11 in min.
    let tread_depth = match target_tread {
        Some(t) if t > 0.0 => {
            if noncompliant {
                t
            } else {
                t.max(STAIR_MIN_TREAD)
            }
        }
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

// ── Geometry builders (vertex/face lists) ─────────────────────────────────────

/// A simple triangle-mesh builder shared by the ramp-surface and stair builders.
/// Keeps the callers free of index bookkeeping and lets the layout→geometry step
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

    /// Wrap the accumulated vertex/face lists in a [`Mesh`].
    pub fn into_mesh(self) -> Mesh {
        Mesh::new(self.positions, self.faces)
    }
}

/// Build the walking-surface quads of a ramp from its layout, starting at
/// `start`, running along +X, `width` wide in +Y. Each run is a sloped quad and
/// each landing a flat quad; together they form the continuous top surface that
/// `codecheck` reads. Returned as positions+faces so the caller can wrap it in a
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
        mb.quad(a, b, b + DVec3::new(0.0, w, 0.0), a + DVec3::new(0.0, w, 0.0));
        cur = b;
    }
    mb
}

/// Build the ramp walking surface as a [`Mesh`] directly. The pure
/// `(rise, slope, width)` → mesh entry point the parametric re-derive path calls
/// (`itsjustcad_doc::derive_mesh`). Returns an error string for invalid params.
pub fn ramp_mesh(rise: f64, slope: RampSlope, width: f64) -> Result<Mesh, String> {
    let layout = ramp_layout(rise, slope, width)?;
    Ok(ramp_surface(&layout, DVec3::ZERO).into_mesh())
}

// ── Turning / curved ramps (issue #82, enhancements #2 & #3) ───────────────────

/// A unit 2D heading in the XY plane plus its left normal, advanced as the walk
/// line turns. Used to lay out switchback/dogleg ramps and stairs so the geometry
/// folds back on itself with a turning landing instead of running straight +X.
#[derive(Clone, Copy, Debug)]
struct Frame {
    /// Plan position of the walk-line centre at the current station.
    pos: DVec3,
    /// Heading angle about +Z (radians). 0 == +X.
    heading: f64,
}

impl Frame {
    fn new(start: DVec3) -> Self {
        Frame { pos: start, heading: 0.0 }
    }
    /// Forward (travel) direction in the XY plane.
    fn fwd(&self) -> DVec3 {
        DVec3::new(self.heading.cos(), self.heading.sin(), 0.0)
    }
    /// Left direction (toward +width side) in the XY plane.
    fn left(&self) -> DVec3 {
        DVec3::new(-self.heading.sin(), self.heading.cos(), 0.0)
    }
    /// Advance `dx` along the heading, gaining `dz` in Z.
    fn advance(&mut self, dx: f64, dz: f64) {
        self.pos += self.fwd() * dx + DVec3::new(0.0, 0.0, dz);
    }
    /// Rotate the heading by `angle` radians about +Z (turning landing pivot).
    fn turn(&mut self, angle: f64) {
        self.heading += angle;
    }
    /// Shift the walk-line position sideways by `d` along the current LEFT
    /// direction (no change in heading or Z). Used at a switchback landing so the
    /// return run runs PARALLEL to the previous run, offset by one ramp width,
    /// instead of folding back collinear on top of it.
    fn shift_left(&mut self, d: f64) {
        self.pos += self.left() * d;
    }
}

/// The lateral offset applied to the walk line at a 180° switchback landing so the
/// return run sits beside the previous run instead of on top of it. One ramp width
/// places the two runs' bands edge-to-edge (centres `width` apart), which is the
/// standard switchback footprint. A 90° dogleg needs no lateral shift — the
/// perpendicular turn already separates the runs — so this applies to switchbacks
/// only.
fn switchback_lateral_offset(turn: TurnKind, width: f64) -> f64 {
    match turn {
        TurnKind::Switchback => width,
        TurnKind::None | TurnKind::Dogleg => 0.0,
    }
}

/// Build the ramp walking surface with a turning landing between runs
/// (switchback = 180°, dogleg = 90°). Each run is a sloped quad along the current
/// heading; each landing is a flat square turning platform, after which the
/// heading rotates so the next run folds back. `TurnKind::None` reproduces the
/// straight +X `ramp_surface` geometry (same rise, same plan run length) but is
/// kept separate so the v1 path stays byte-identical.
pub fn ramp_surface_turning(layout: &RampLayout, start: DVec3, turn: TurnKind) -> MeshBuild {
    if turn == TurnKind::None {
        return ramp_surface(layout, start);
    }
    let w = layout.width;
    let mut mb = MeshBuild::default();
    // The walk line runs down the CENTRE; the surface extends ±w/2 to each side so
    // a turning platform rotates about the walk-line centre cleanly.
    let mut f = Frame::new(start);
    let half = w * 0.5;
    let emit_quad = |mb: &mut MeshBuild, a: DVec3, b: DVec3, fr: &Frame| {
        // a..b is the centre segment; widen to the left/right by half-width using
        // the frame's left direction (constant across a straight segment).
        let l = fr.left() * half;
        // near-left, far-left, far-right, near-right → up-facing normal.
        mb.quad(a - l, b - l, b + l, a + l);
    };
    for seg in &layout.segments {
        match *seg {
            RampSegment::Run { run, rise } => {
                let a = f.pos;
                f.advance(run, rise);
                let b = f.pos;
                emit_quad(&mut mb, a, b, &f);
            }
            RampSegment::Landing { len } => {
                // Flat turning platform between two runs. The walk line enters along
                // the current heading and advances `len` across the platform. For a
                // SWITCHBACK the return run must sit BESIDE the entry run (offset one
                // width in the lateral/Y direction), so the platform spans BOTH
                // lateral bands (entry band + the shifted return band) and the walk
                // line is shifted sideways by `width` before the 180° pivot — the two
                // runs end up parallel and non-overlapping in plan. A DOGLEG needs no
                // lateral shift (the 90° turn already separates the runs); its
                // platform is the single-band square at the corner.
                let a = f.pos;
                f.advance(len, 0.0);
                let b = f.pos;
                let shift = switchback_lateral_offset(turn, w);
                let ldir = f.left(); // constant across the (straight) platform
                let near = a - ldir * half;
                let far = b - ldir * half;
                // Outer lateral edge: half-width for a dogleg, or across both bands
                // (half + shift) for a switchback so the platform connects the runs.
                let out = half + shift;
                // near-inner, far-inner, far-outer, near-outer → up-facing normal.
                mb.quad(near, far, b + ldir * out, a + ldir * out);
                // Start run 2 so it leaves the landing cleanly: switchback folds
                // back from the NEAR edge `a` (shifted a width below); dogleg exits
                // the landing's +left edge, centred in the landing depth, so the
                // perpendicular run sits within the platform (no stick-out).
                match turn {
                    TurnKind::Switchback => f.pos = a,
                    TurnKind::Dogleg => f.pos = (a + b) * 0.5 + ldir * half,
                    TurnKind::None => {}
                }
                // Shift sideways (switchback only), then rotate the heading so the
                // next run folds back / turns.
                if shift != 0.0 {
                    f.shift_left(shift);
                }
                f.turn(turn.turn_angle());
            }
        }
    }
    mb
}

/// Walk a ramp layout into a turning centerline polyline (switchback/dogleg).
/// Mirrors [`ramp_centerline`] but applies a heading rotation at each landing so
/// the drawn path folds back — this is the curve `codecheck ada2010` reads for
/// running slope and landing placement on a turning ramp.
pub fn ramp_centerline_turning(layout: &RampLayout, start: DVec3, turn: TurnKind) -> Vec<DVec3> {
    if turn == TurnKind::None {
        return ramp_centerline(layout, start);
    }
    let mut f = Frame::new(start);
    let mut pts = vec![f.pos];
    for seg in &layout.segments {
        match *seg {
            RampSegment::Run { run, rise } => {
                f.advance(run, rise);
                pts.push(f.pos);
            }
            RampSegment::Landing { len } => {
                let a = f.pos;
                f.advance(len, 0.0);
                let b = f.pos;
                pts.push(b);
                // Match `ramp_surface_turning`: switchback returns from the NEAR
                // edge `a` offset a width sideways; dogleg exits the landing's
                // +left edge centred in its depth. Record the points so the drawn
                // centerline follows the real path the compliance checks read.
                let shift = switchback_lateral_offset(turn, layout.width);
                match turn {
                    TurnKind::Switchback => f.pos = a,
                    TurnKind::Dogleg => f.pos = (a + b) * 0.5 + f.left() * (layout.width * 0.5),
                    TurnKind::None => {}
                }
                if turn == TurnKind::Dogleg {
                    pts.push(f.pos);
                }
                if shift != 0.0 {
                    f.shift_left(shift);
                    pts.push(f.pos);
                }
                f.turn(turn.turn_angle());
            }
        }
    }
    pts
}

/// Build a CURVED ramp walking surface: the centerline is a circular arc of plan
/// `radius` about a centre to the ramp's left, rising uniformly along its length
/// (issue #82, enhancement #3). The total plan arc length equals the straight
/// layout's plan length, so the rise/run grade is preserved along the walk line;
/// `segments_per_run` controls the facet count. The surface is a ribbon ±w/2
/// about the arc. A non-positive radius falls back to the straight surface.
pub fn ramp_surface_curved(
    layout: &RampLayout,
    start: DVec3,
    radius: f64,
    facets: usize,
) -> MeshBuild {
    if !positive(radius) {
        return ramp_surface(layout, start);
    }
    let w = layout.width;
    let half = w * 0.5;
    let total_len = ramp_plan_length(layout);
    let total_rise: f64 = layout
        .segments
        .iter()
        .map(|s| match s {
            RampSegment::Run { rise, .. } => *rise,
            RampSegment::Landing { .. } => 0.0,
        })
        .sum();
    let facets = facets.max(1);
    // Arc centre is to the LEFT of the start heading (+Y), radius away.
    let centre = start + DVec3::new(0.0, radius, 0.0);
    let total_angle = total_len / radius; // arc swept
    let mut mb = MeshBuild::default();
    // Inner/outer edge offset ±half along the radial direction.
    let edge = |u: f64, s: f64| -> DVec3 {
        let ang = -std::f64::consts::FRAC_PI_2 + total_angle * u;
        let z = start.z + total_rise * u;
        let r = radius + s * half; // s = +1 outer (left), -1 inner
        DVec3::new(centre.x + r * ang.cos(), centre.y + r * ang.sin(), z)
    };
    for i in 0..facets {
        let u0 = i as f64 / facets as f64;
        let u1 = (i + 1) as f64 / facets as f64;
        // near-inner, far-inner, far-outer, near-outer → up-facing normal.
        mb.quad(edge(u0, -1.0), edge(u1, -1.0), edge(u1, 1.0), edge(u0, 1.0));
    }
    mb
}

/// The centerline polyline of a curved ramp arc (see [`ramp_surface_curved`]).
pub fn ramp_centerline_curved(
    layout: &RampLayout,
    start: DVec3,
    radius: f64,
    facets: usize,
) -> Vec<DVec3> {
    if !positive(radius) {
        return ramp_centerline(layout, start);
    }
    let total_len = ramp_plan_length(layout);
    let total_rise: f64 = layout
        .segments
        .iter()
        .map(|s| match s {
            RampSegment::Run { rise, .. } => *rise,
            RampSegment::Landing { .. } => 0.0,
        })
        .sum();
    let facets = facets.max(1);
    let centre = start + DVec3::new(0.0, radius, 0.0);
    let total_angle = total_len / radius;
    (0..=facets)
        .map(|i| {
            let u = i as f64 / facets as f64;
            let ang = -std::f64::consts::FRAC_PI_2 + total_angle * u;
            let z = start.z + total_rise * u;
            DVec3::new(centre.x + radius * ang.cos(), centre.y + radius * ang.sin(), z)
        })
        .collect()
}

/// Build a stepped stair mesh from its layout, starting at `start`, running
/// along +X, `width` wide in +Y. Each step contributes a horizontal tread quad
/// at its level and a vertical riser quad up to it. The tread quads are the
/// up-facing faces `codecheck ibc2021` reads for `tread_depth`; the risers
/// cluster by z-level for the riser check. Returned as positions+faces.
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

/// Build the stepped stair as a [`Mesh`] directly. The pure
/// `(rise, width, riser, tread)` → mesh entry point the parametric re-derive
/// path calls (`itsjustcad_doc::derive_mesh`). Returns an error string for
/// invalid params.
pub fn stair_mesh_from_params(
    rise: f64,
    width: f64,
    target_riser: Option<f64>,
    target_tread: Option<f64>,
) -> Result<Mesh, String> {
    let layout = stair_layout(rise, width, target_riser, target_tread)?;
    Ok(stair_mesh(&layout, DVec3::ZERO).into_mesh())
}

// ── Turning / spiral stairs (issue #82, enhancements #2 & #3) ──────────────────

/// Build a stepped stair that TURNS at a mid landing (switchback = 180°, dogleg =
/// 90°). The flight is split into two runs of (nearly) equal rise with a square
/// landing between them; the heading rotates at the landing so the second run
/// folds back. `TurnKind::None` falls back to the straight `stair_mesh`. Each
/// step is a tread quad (up-facing) plus a riser quad, oriented to the walk-line
/// frame; the landing is a flat square turning platform at its level.
pub fn stair_mesh_turning(layout: &StairLayout, start: DVec3, turn: TurnKind) -> MeshBuild {
    if turn == TurnKind::None {
        return stair_mesh(layout, start);
    }
    let w = layout.width;
    let t = layout.tread_depth;
    let r = layout.riser_height;
    let half = w * 0.5;
    let mut mb = MeshBuild::default();
    let mut f = Frame::new(start);
    // Split the risers into two runs around a mid landing.
    let first = layout.risers / 2;
    let add_step = |mb: &mut MeshBuild, f: &mut Frame| {
        let l = f.left() * half;
        // Riser: rise in place before advancing along the tread.
        let a0 = f.pos;
        let a1 = a0 + DVec3::new(0.0, 0.0, r);
        // Riser quad (vertical) spanning the width at the step front.
        mb.quad(a0 - l, a0 + l, a1 + l, a1 - l);
        // Advance one tread depth at the raised level.
        f.pos = a1;
        f.advance(t, 0.0);
        let b = f.pos;
        // Tread quad (up-facing) from a1 to b.
        mb.quad(a1 - l, b - l, b + l, a1 + l);
    };
    for _ in 0..first {
        add_step(&mut mb, &mut f);
    }
    // Mid landing: a flat turning platform at the current level. For a SWITCHBACK
    // the upper run must run BESIDE the lower run (offset one width laterally) so
    // the two flights do not stack on top of each other; the platform spans both
    // lateral bands and the walk line shifts sideways by `width` before the 180°
    // pivot. A DOGLEG needs no lateral shift (the 90° turn separates the flights).
    {
        let a = f.pos;
        f.advance(w.max(t), 0.0); // platform at least width-deep
        let b = f.pos;
        let shift = switchback_lateral_offset(turn, w);
        let ldir = f.left();
        let out = half + shift;
        mb.quad(a - ldir * half, b - ldir * half, b + ldir * out, a + ldir * out);
        // Position the start of run 2 so it leaves the landing cleanly (both runs
        // aligned to the platform, no stick-out / overlap):
        //  • Switchback: the NEAR edge `a`, then shifted one width sideways below,
        //    so the upper run folds straight back beside the lower run.
        //  • Dogleg: the landing's +left edge, centred in the landing depth, so the
        //    perpendicular run exits squarely within the platform footprint.
        match turn {
            TurnKind::Switchback => f.pos = a,
            TurnKind::Dogleg => f.pos = (a + b) * 0.5 + ldir * half,
            TurnKind::None => {}
        }
        if shift != 0.0 {
            f.shift_left(shift);
        }
        f.turn(turn.turn_angle());
    }
    for _ in first..layout.risers {
        add_step(&mut mb, &mut f);
    }
    mb
}

/// Build a SPIRAL / helical stair about a `center`, winding `radius` from the
/// axis through `total_turn` radians as it climbs the full rise (issue #82,
/// enhancement #3). Each tread is a wedge fanning out from the axis; risers are
/// vertical faces at the inner-to-outer leading edge. Tread depth is measured
/// along the WALK LINE (a cylinder at `radius` from the axis), so the angular
/// step per riser is `tread / radius`. `tread_width` is the radial extent of each
/// tread (centred on `radius`). A non-positive radius falls back to the straight
/// stepped stair.
pub fn stair_mesh_spiral(
    layout: &StairLayout,
    center: DVec3,
    radius: f64,
    total_turn: f64,
) -> MeshBuild {
    if !positive(radius) {
        return stair_mesh(layout, center);
    }
    let r = layout.riser_height;
    let half = layout.width * 0.5;
    let r_in = (radius - half).max(0.0);
    let r_out = radius + half;
    let n = layout.risers;
    // Angular step per riser along the walk line, but never exceed what the total
    // turn allows (so the full stair spans `total_turn`).
    let dtheta = if n > 0 { total_turn / n as f64 } else { 0.0 };
    let mut mb = MeshBuild::default();
    let point = |theta: f64, rr: f64, z: f64| -> DVec3 {
        DVec3::new(center.x + rr * theta.cos(), center.y + rr * theta.sin(), center.z + z)
    };
    for i in 0..n {
        let theta0 = i as f64 * dtheta;
        let theta1 = (i + 1) as f64 * dtheta;
        let z = (i as f64 + 1.0) * r; // tread sits at the top of riser i
        let z_below = i as f64 * r;
        // Riser (vertical) along the leading radial edge at theta0, from z_below
        // up to z, spanning inner→outer radius.
        mb.quad(
            point(theta0, r_in, z_below),
            point(theta0, r_out, z_below),
            point(theta0, r_out, z),
            point(theta0, r_in, z),
        );
        // Tread wedge (up-facing) at level z, from theta0 to theta1. Wind CCW as
        // seen from ABOVE (inner→outer at theta0, along the arc to theta1, back
        // inner) so the normal points +Z — the a→b→c→d order above was CW, which
        // gave a downward normal (the spiral rendered dark/back-faced from above).
        mb.quad(
            point(theta0, r_in, z),
            point(theta0, r_out, z),
            point(theta1, r_out, z),
            point(theta1, r_in, z),
        );
    }
    mb
}

/// The helical walk-line polyline of a spiral stair (the curve `codecheck` would
/// read for the along-walk-line geometry). One point per tread edge.
pub fn stair_centerline_spiral(
    layout: &StairLayout,
    center: DVec3,
    radius: f64,
    total_turn: f64,
) -> Vec<DVec3> {
    let r = layout.riser_height;
    let n = layout.risers;
    let dtheta = if n > 0 { total_turn / n as f64 } else { 0.0 };
    (0..=n)
        .map(|i| {
            let theta = i as f64 * dtheta;
            let z = i as f64 * r;
            DVec3::new(center.x + radius * theta.cos(), center.y + radius * theta.sin(), center.z + z)
        })
        .collect()
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
        let runs = l.segments.iter().filter(|s| matches!(s, RampSegment::Run { .. })).count();
        let landings =
            l.segments.iter().filter(|s| matches!(s, RampSegment::Landing { .. })).count();
        assert_eq!(runs, 2, "two runs for a 900 mm rise");
        assert_eq!(landings, 1, "one mid landing");
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
        for s in &l.segments {
            if let RampSegment::Run { rise, .. } = s {
                assert!(*rise <= RAMP_MAX_RUN_RISE + 1e-9, "run rise {rise}");
            }
        }
        for s in &l.segments {
            if let RampSegment::Landing { len } = s {
                assert!(*len >= RAMP_MIN_LANDING_LEN - 1e-9, "landing {len}");
            }
        }
    }

    #[test]
    fn ramp_small_rise_is_a_single_run_no_landing() {
        let l = ramp_layout(0.3, RampSlope::Ratio(RAMP_MAX_SLOPE), 1.0).unwrap();
        assert_eq!(l.segments.len(), 1);
        assert!(matches!(l.segments[0], RampSegment::Run { .. }));
    }

    #[test]
    fn ramp_run_spec_is_clamped_to_code_slope() {
        let l = ramp_layout(0.9, RampSlope::Run(3.0), 1.0).unwrap();
        assert!(l.slope <= RAMP_MAX_SLOPE + 1e-9, "slope {}", l.slope);
    }

    #[test]
    fn ramp_gentle_run_spec_is_honored() {
        let l = ramp_layout(0.6, RampSlope::Run(12.0), 1.0).unwrap();
        assert!((l.slope - 0.05).abs() < 1e-9, "slope {}", l.slope);
    }

    #[test]
    fn ramp_centerline_matches_rise() {
        let l = ramp_layout(0.9, RampSlope::Ratio(RAMP_MAX_SLOPE), 1.2).unwrap();
        let pts = ramp_centerline(&l, DVec3::ZERO);
        let top = pts.last().unwrap().z;
        assert!((top - 0.9).abs() < 1e-9, "top z {top}");
        let len = ramp_plan_length(&l);
        assert!(
            (len - (0.9 / RAMP_MAX_SLOPE + RAMP_MIN_LANDING_LEN)).abs() < 1e-6,
            "plan len {len}"
        );
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
        let l = stair_layout(2.8, 1.1, None, None).unwrap();
        let variation = 0.0; // all risers = rise/count by construction
        assert!(variation <= STAIR_MAX_RISER_VARIATION);
        assert!(l.riser_height <= STAIR_MAX_RISER + 1e-9);
    }

    #[test]
    fn stair_2r_plus_t_picks_a_sane_tread() {
        let l = stair_layout(3.0, 1.2, None, None).unwrap();
        let two_r_plus_t = 2.0 * l.riser_height + l.tread_depth;
        assert!(l.tread_depth >= STAIR_MIN_TREAD - 1e-9);
        assert!((0.62..=0.70).contains(&two_r_plus_t), "2R+T {two_r_plus_t}");
    }

    #[test]
    fn stair_honors_explicit_tread_target() {
        let l = stair_layout(3.0, 1.2, None, Some(0.30)).unwrap();
        assert!((l.tread_depth - 0.30).abs() < 1e-9);
        let l2 = stair_layout(3.0, 1.2, None, Some(0.20)).unwrap();
        assert!((l2.tread_depth - STAIR_MIN_TREAD).abs() < 1e-9);
    }

    #[test]
    fn stair_tall_rise_flags_a_landing() {
        let l = stair_layout(4.0, 1.2, None, None).unwrap();
        assert!(l.landing, "4 m rise must flag a landing");
    }

    #[test]
    fn stair_rejects_bad_inputs() {
        assert!(stair_layout(0.0, 1.0, None, None).is_err());
        assert!(stair_layout(3.0, 0.0, None, None).is_err());
    }

    // ── #82 #1: non-compliant override ──────────────────────────────────────

    #[test]
    fn ramp_noncompliant_honors_exact_slope_no_clamp() {
        // A 1:8 service ramp (0.125) is far steeper than the ADA 1:12 max; with
        // the override the layout must use EXACTLY 1:8, not the clamped value.
        let steep = 1.0 / 8.0;
        let l = ramp_layout_opts(0.9, RampSlope::Ratio(steep), 1.2, true).unwrap();
        assert!((l.slope - steep).abs() < 1e-12, "slope must be verbatim 1:8, got {}", l.slope);
        assert!(l.slope > RAMP_MAX_SLOPE, "steeper than the ADA max, i.e. non-compliant");
        // And without the flag the SAME ask is clamped (v1 behavior preserved).
        let clamped = ramp_layout_opts(0.9, RampSlope::Ratio(steep), 1.2, false).unwrap();
        assert!((clamped.slope - RAMP_MAX_SLOPE).abs() < 1e-12, "clamped to ADA max");
    }

    #[test]
    fn ramp_noncompliant_run_spec_is_also_verbatim() {
        // rise 0.9 over a 7.2 m run is 1:8 — honored verbatim with the override.
        let l = ramp_layout_opts(0.9, RampSlope::Run(7.2), 1.2, true).unwrap();
        assert!((l.slope - 0.9 / 7.2).abs() < 1e-12, "slope {}", l.slope);
        assert!(l.slope > RAMP_MAX_SLOPE);
    }

    #[test]
    fn stair_noncompliant_honors_steep_riser_no_clamp() {
        // A 0.22 m (≈8.7 in) riser exceeds the IBC 7 in max. With the override the
        // equalized riser may exceed 7 in; without it the count is forced up so the
        // riser stays ≤ 7 in.
        let steep = stair_layout_opts(3.0, 1.2, Some(0.22), None, true).unwrap();
        assert!(steep.riser_height > STAIR_MAX_RISER, "riser {} not clamped", steep.riser_height);
        let compliant = stair_layout_opts(3.0, 1.2, Some(0.22), None, false).unwrap();
        assert!(
            compliant.riser_height <= STAIR_MAX_RISER + 1e-9,
            "v1 clamps to ≤ 7 in, got {}",
            compliant.riser_height
        );
    }

    #[test]
    fn stair_noncompliant_honors_shallow_tread_no_clamp() {
        // A 0.22 m tread is below the IBC 11 in (0.2794 m) min; honored verbatim.
        let l = stair_layout_opts(3.0, 1.2, None, Some(0.22), true).unwrap();
        assert!((l.tread_depth - 0.22).abs() < 1e-12, "tread {}", l.tread_depth);
        let v1 = stair_layout_opts(3.0, 1.2, None, Some(0.22), false).unwrap();
        assert!((v1.tread_depth - STAIR_MIN_TREAD).abs() < 1e-12, "v1 floors at 11 in");
    }

    #[test]
    fn compliant_layout_is_byte_identical_to_v1() {
        // The opts path with `noncompliant = false` must reproduce the v1 layout.
        let a = ramp_layout(0.9, RampSlope::Ratio(RAMP_MAX_SLOPE), 1.2).unwrap();
        let b = ramp_layout_opts(0.9, RampSlope::Ratio(RAMP_MAX_SLOPE), 1.2, false).unwrap();
        assert_eq!(a, b);
        let c = stair_layout(3.0, 1.2, None, None).unwrap();
        let d = stair_layout_opts(3.0, 1.2, None, None, false).unwrap();
        assert_eq!(c, d);
    }

    // ── #82 #2: switchback / dogleg turning landing ─────────────────────────

    /// Sum the Z of the last tread/landing vertex — the total rise of the shape.
    fn top_z(mb: &MeshBuild) -> f64 {
        mb.positions.iter().map(|p| p.z).fold(f64::MIN, f64::max)
    }

    #[test]
    fn ramp_switchback_folds_back_and_keeps_total_rise() {
        // A 1.5 m rise needs ≥3 runs (each ≤ 0.76 m) → ≥2 landings → it turns.
        let l = ramp_layout(1.5, RampSlope::Ratio(RAMP_MAX_SLOPE), 1.2).unwrap();
        let straight = ramp_surface(&l, DVec3::ZERO);
        let sw = ramp_surface_turning(&l, DVec3::ZERO, TurnKind::Switchback);
        // Same rise (top vertex Z) …
        assert!((top_z(&straight) - 1.5).abs() < 1e-9, "straight rise");
        assert!((top_z(&sw) - 1.5).abs() < 1e-9, "switchback keeps the total rise");
        // … but the switchback's X extent is much shorter than the straight run
        // (it folds back instead of running on in one line).
        let span = |mb: &MeshBuild| {
            let xs = mb.positions.iter().map(|p| p.x);
            xs.clone().fold(f64::MAX, f64::min)..xs.fold(f64::MIN, f64::max)
        };
        let s_straight = span(&straight);
        let s_sw = span(&sw);
        assert!(
            (s_sw.end - s_sw.start) < (s_straight.end - s_straight.start) * 0.75,
            "switchback is far more compact in X"
        );
    }

    /// Collect the per-run plan-Y range of each sloped run in a turning ramp
    /// surface. Each run contributes exactly one quad (4 verts) emitted in order,
    /// interleaved with landing quads. We rebuild the run list by re-walking the
    /// layout the same way the builder does, mapping each run to its quad's Y span.
    #[test]
    fn ramp_switchback_runs_are_parallel_and_offset_by_width_no_overlap() {
        // Bug 2 (#82): the switchback's return run must sit BESIDE the first run,
        // offset by ~one width in Y, NOT fold back collinear on top of it.
        let width = 1.2;
        let l = ramp_layout(1.5, RampSlope::Ratio(RAMP_MAX_SLOPE), width).unwrap();
        let sw = ramp_surface_turning(&l, DVec3::ZERO, TurnKind::Switchback);

        // Total rise preserved.
        assert!((top_z(&sw) - 1.5).abs() < 1e-9, "switchback keeps the total rise");

        // Walk the layout to find, for each RUN, the quad's Y range. The builder
        // emits one quad per segment (run OR landing) in order, 4 verts each.
        let mut run_y_ranges: Vec<(f64, f64)> = Vec::new();
        for (quad_i, seg) in l.segments.iter().enumerate() {
            let base = quad_i * 4;
            let ys: Vec<f64> = (base..base + 4).map(|i| sw.positions[i].y).collect();
            let ymin = ys.iter().cloned().fold(f64::MAX, f64::min);
            let ymax = ys.iter().cloned().fold(f64::MIN, f64::max);
            if matches!(seg, RampSegment::Run { .. }) {
                run_y_ranges.push((ymin, ymax));
            }
        }
        assert!(run_y_ranges.len() >= 2, "a switchback has ≥2 runs");

        // Run 1 band ≈ [-half, +half] (centre 0); run 2 band ≈ [+half, +3half]
        // (centre +width). Their centres differ by ~width and the bands are
        // disjoint (they may touch at the shared edge but must not overlap).
        let (r1min, r1max) = run_y_ranges[0];
        let (r2min, r2max) = run_y_ranges[1];
        let c1 = 0.5 * (r1min + r1max);
        let c2 = 0.5 * (r2min + r2max);
        assert!((c1 - 0.0).abs() < 1e-9, "run 1 centred on the start band, got {c1}");
        assert!(
            ((c2 - c1).abs() - width).abs() < 1e-6,
            "run-2 centre offset must be ~width ({width}); got {}",
            (c2 - c1).abs()
        );
        // Disjoint in plan: run 2's band starts at or above run 1's top edge.
        assert!(
            r2min >= r1max - 1e-6,
            "runs overlap in Y: run1 [{r1min},{r1max}] vs run2 [{r2min},{r2max}]"
        );
        // Each run's band is one width wide (not squashed / not doubled).
        assert!((r1max - r1min - width).abs() < 1e-6, "run 1 band is one width");
        assert!((r2max - r2min - width).abs() < 1e-6, "run 2 band is one width");
    }

    #[test]
    fn stair_switchback_runs_are_offset_laterally_no_overlap() {
        // Bug 2 (#82) for stairs: the upper flight sits beside the lower flight,
        // offset ~one width in Y, not stacked on top of it. Verify the overall Y
        // span exceeds a single width (so the two flights occupy different bands)
        // and the total rise is preserved.
        let width = 1.2;
        let l = stair_layout(3.0, width, None, None).unwrap();
        let mb = stair_mesh_turning(&l, DVec3::ZERO, TurnKind::Switchback);
        assert!((top_z(&mb) - 3.0).abs() < 1e-6, "switchback stair keeps the rise");
        let ymin = mb.positions.iter().map(|p| p.y).fold(f64::MAX, f64::min);
        let ymax = mb.positions.iter().map(|p| p.y).fold(f64::MIN, f64::max);
        // Two side-by-side bands of one width (touching) span ~2*width.
        assert!(
            (ymax - ymin) > width * 1.5,
            "switchback flights must occupy two lateral bands (~2 widths), got {}",
            ymax - ymin
        );
    }

    #[test]
    fn stair_dogleg_run2_aligns_within_the_landing() {
        // Dogleg alignment (#82): the perpendicular upper run must sit WITHIN the
        // landing's X band and exit from its +Y edge — not centred on the far edge
        // sticking half a width past the landing (the original bug).
        let width = 1.2;
        let l = stair_layout(3.0, width, None, None).unwrap();
        let mb = stair_mesh_turning(&l, DVec3::ZERO, TurnKind::Dogleg);
        assert!((top_z(&mb) - 3.0).abs() < 1e-6, "dogleg keeps the rise");
        let first = l.risers / 2;
        let x1 = first as f64 * l.tread_depth; // run-1 top x
        let land_far = x1 + width.max(l.tread_depth); // landing far X edge
        let run1_half = width * 0.5;
        // Every run-2 vertex (those beyond run-1's +Y edge) stays inside the
        // landing's X band.
        for p in &mb.positions {
            if p.y > run1_half + 1e-6 {
                assert!(
                    p.x >= x1 - 1e-6 && p.x <= land_far + 1e-6,
                    "run-2 vertex x={} sticks out of the landing band [{x1:.2},{land_far:.2}]",
                    p.x
                );
            }
        }
    }

    #[test]
    fn ramp_switchback_centerline_reverses_heading() {
        // After a 180° landing the path travels in −X relative to the first run.
        let l = ramp_layout(1.5, RampSlope::Ratio(RAMP_MAX_SLOPE), 1.2).unwrap();
        let pts = ramp_centerline_turning(&l, DVec3::ZERO, TurnKind::Switchback);
        // First segment advances +X; a later segment must advance −X.
        let d0 = pts[1].x - pts[0].x;
        assert!(d0 > 0.0, "first run goes +X");
        let reversed = pts.windows(2).any(|w| w[1].x - w[0].x < -1e-6);
        assert!(reversed, "a run after the switchback travels −X");
    }

    #[test]
    fn stair_dogleg_turns_ninety_degrees() {
        // A dogleg stair's second run travels along a perpendicular axis.
        let l = stair_layout(3.0, 1.2, None, None).unwrap();
        let mb = stair_mesh_turning(&l, DVec3::ZERO, TurnKind::Dogleg);
        assert!((top_z(&mb) - 3.0).abs() < 1e-6, "dogleg keeps total rise, got {}", top_z(&mb));
        // The mesh spreads in BOTH X and Y (the straight stair is X-only within the
        // tread axis), confirming the 90° turn.
        let yspan = mb.positions.iter().map(|p| p.y).fold(f64::MIN, f64::max)
            - mb.positions.iter().map(|p| p.y).fold(f64::MAX, f64::min);
        let xspan = mb.positions.iter().map(|p| p.x).fold(f64::MIN, f64::max)
            - mb.positions.iter().map(|p| p.x).fold(f64::MAX, f64::min);
        assert!(yspan > l.width * 1.5, "dogleg extends well beyond one width in Y");
        assert!(xspan > l.width * 1.5, "dogleg extends in X too");
    }

    #[test]
    fn turn_none_is_identical_to_straight() {
        let l = ramp_layout(1.5, RampSlope::Ratio(RAMP_MAX_SLOPE), 1.2).unwrap();
        let a = ramp_surface(&l, DVec3::ZERO);
        let b = ramp_surface_turning(&l, DVec3::ZERO, TurnKind::None);
        assert_eq!(a.positions, b.positions, "TurnKind::None == straight surface");
        let sl = stair_layout(3.0, 1.2, None, None).unwrap();
        let sa = stair_mesh(&sl, DVec3::ZERO);
        let sb = stair_mesh_turning(&sl, DVec3::ZERO, TurnKind::None);
        assert_eq!(sa.positions, sb.positions);
    }

    // ── #82 #3: curved ramp & spiral stair ──────────────────────────────────

    #[test]
    fn curved_ramp_follows_the_arc() {
        // The curved ramp's centerline points lie on a circle of plan `radius`
        // about the arc centre, and the top reaches the full rise.
        let radius = 6.0;
        let l = ramp_layout(0.9, RampSlope::Ratio(RAMP_MAX_SLOPE), 1.2).unwrap();
        let pts = ramp_centerline_curved(&l, DVec3::ZERO, radius, 24);
        let centre = DVec3::new(0.0, radius, 0.0);
        for p in &pts {
            let plan = DVec3::new(p.x, p.y, 0.0);
            let r = (plan - DVec3::new(centre.x, centre.y, 0.0)).length();
            assert!((r - radius).abs() < 1e-6, "point off the arc: r={r}");
        }
        assert!((pts.last().unwrap().z - 0.9).abs() < 1e-9, "curved ramp reaches the rise");
    }

    #[test]
    fn spiral_stair_risers_measured_along_walk_line() {
        // Acceptance (#82 #3): a spiral stair about a centre, with a given radius
        // and total rotation; each tread spans dθ = total_turn / risers along the
        // walk line, and every tread vertex lies between the inner and outer radius.
        let radius = 1.5;
        let total_turn = std::f64::consts::PI * 2.0; // 360°
        let l = stair_layout(3.0, 1.0, None, None).unwrap();
        let mb = stair_mesh_spiral(&l, DVec3::ZERO, radius, total_turn);
        assert!((top_z(&mb) - 3.0).abs() < 1e-6, "spiral reaches the full rise");
        // Walk-line polyline: consecutive points are one tread apart along the
        // cylinder at `radius` — arc length ≈ radius * dθ.
        let wl = stair_centerline_spiral(&l, DVec3::ZERO, radius, total_turn);
        let dtheta = total_turn / l.risers as f64;
        let expected_arc = radius * dtheta;
        for w in wl.windows(2) {
            let plan = DVec3::new(w[1].x - w[0].x, w[1].y - w[0].y, 0.0).length();
            // chord length of the small arc ≈ 2 r sin(dθ/2); check it is close.
            let chord = 2.0 * radius * (dtheta / 2.0).sin();
            assert!((plan - chord).abs() < 1e-6, "walk-line step {plan} vs chord {chord}");
        }
        // Every vertex within [r_in, r_out] of the axis.
        let r_in = radius - l.width * 0.5;
        let r_out = radius + l.width * 0.5;
        for p in &mb.positions {
            let r = DVec3::new(p.x, p.y, 0.0).length();
            assert!(r >= r_in - 1e-6 && r <= r_out + 1e-6, "vertex radius {r} out of band");
        }
        let _ = expected_arc;
    }

    #[test]
    fn spiral_stair_tread_normals_point_up() {
        // Regression: the tread quad was wound CW seen from above → downward
        // normal → the spiral rendered back-faced (dark) from the top. Each step
        // emits a riser quad (2 faces) then a tread quad (2 faces); the tread
        // faces are indices 4*i+2 and 4*i+3 and MUST have a +Z normal.
        let l = stair_layout(3.0, 1.0, None, None).unwrap();
        let mb = stair_mesh_spiral(&l, DVec3::ZERO, 1.5, std::f64::consts::PI * 2.0);
        for i in 0..l.risers {
            for f in [4 * i + 2, 4 * i + 3] {
                let [a, b, c] = mb.faces[f];
                let p0 = mb.positions[a as usize];
                let p1 = mb.positions[b as usize];
                let p2 = mb.positions[c as usize];
                let n = (p1 - p0).cross(p2 - p0);
                assert!(n.z > 0.0, "tread face {f} normal points down (z={})", n.z);
            }
        }
    }

    #[test]
    fn spiral_stair_total_rotation_is_respected() {
        // The last tread edge is at angle total_turn from the first.
        let total_turn = std::f64::consts::PI * 1.25; // 225°
        let l = stair_layout(3.0, 1.0, None, None).unwrap();
        let wl = stair_centerline_spiral(&l, DVec3::ZERO, 1.5, total_turn);
        let first = wl[1] - wl[0];
        let last = wl[wl.len() - 1] - DVec3::new(0.0, 1.5, 0.0) + DVec3::new(0.0, 1.5, 0.0);
        // Angle of the final walk-line point about the axis.
        let ang = last.y.atan2(last.x);
        // total_turn starts at angle 0 (first point at (radius, 0)); the final
        // point is at angle total_turn (mod 2π).
        let wrapped = total_turn.rem_euclid(std::f64::consts::TAU);
        let got = ang.rem_euclid(std::f64::consts::TAU);
        assert!((got - wrapped).abs() < 1e-6, "final angle {got} vs {wrapped}");
        let _ = first;
    }
}







