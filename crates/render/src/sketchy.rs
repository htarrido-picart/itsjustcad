// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! Sketchy NPR character (SketchUp-style "sketchy edges").
//!
//! Stage 1 shipped the pencil hidden-line look and profile/silhouette edges.
//! Stage 2 adds *hand-drawn character* as a pure rendering / display concern —
//! view state, never logged, consistent with [`crate::DisplayMode`]. The
//! transforms operate on the segment soup already produced by the snapshot
//! (point pairs), so both the live view and the pencil / NPR path get the same
//! wobble.
//!
//! Effects, each toggled/tuned via [`SketchyParams`]:
//!   * **JITTER** — each edge is drawn several times at a small perpendicular
//!     offset for a hand-drawn wobble. `Math.random` is unavailable in scripts,
//!     so the offset is a *deterministic* hash of the edge id (its quantized
//!     endpoints) — same input → same wobble, every frame and every replay.
//!   * **EXTENSION** — edges overshoot their endpoints slightly, the way a
//!     pencil stroke runs past a corner.
//!   * **ENDPOINTS** — the overshoot tips are emitted as their own short
//!     segments so the pen-down / pen-up ends read thicker.
//!   * **DEPTH CUE** — foreground edges are jittered/extended more than
//!     background ones (needs the camera eye), so near linework feels bolder.
//!
//! Hatching (sun/shading-driven face fills) is handled by the caller with the
//! existing hatch machinery; [`SketchyParams::hatching`] just carries the flag.

/// Tunable parameters for the sketchy edge look. All amounts are in *world
/// units* (metres at building scale) except the unitless flags; `0` disables an
/// effect, so `SketchyParams::default()` is a clean (non-sketchy) pass.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SketchyParams {
    /// Master switch. When `false` the transform is the identity.
    pub enabled: bool,
    /// Perpendicular wobble amplitude (world units). Each drawn pass is offset
    /// by up to `±jitter` from the true edge line.
    pub jitter: f32,
    /// Number of overdraw passes per edge (>=1). Two or three copies read as a
    /// confident hand-drawn stroke.
    pub passes: u32,
    /// Overshoot length past each endpoint (world units).
    pub extension: f32,
    /// Emit the overshoot tips as separate short "endpoint" segments so the
    /// stroke ends read thicker (SketchUp "endpoints").
    pub endpoints: bool,
    /// Depth-cue strength (0..1): fraction by which foreground edges get
    /// *more* jitter/extension than background edges. `0` = uniform.
    pub depthcue: f32,
    /// Sun/shading-driven hatching on shaded faces. Carried as a flag; the
    /// snapshot builder owns the actual hatch geometry.
    pub hatching: bool,
}

impl Default for SketchyParams {
    fn default() -> Self {
        SketchyParams {
            enabled: false,
            jitter: 0.03,
            passes: 2,
            extension: 0.05,
            endpoints: false,
            depthcue: 0.0,
            hatching: false,
        }
    }
}

impl SketchyParams {
    /// A pleasant default sketchy look: wobble + overshoot, two passes.
    pub fn on() -> Self {
        SketchyParams { enabled: true, ..Default::default() }
    }

    /// True when any effect would actually change geometry.
    pub fn active(self) -> bool {
        self.enabled && (self.jitter > 0.0 || self.extension > 0.0)
    }

    /// Apply `key=value` tuning tokens (as from the `edgefx` verb) onto these
    /// params, returning the updated struct. Unknown keys and malformed values
    /// are ignored so a partial `edgefx jitter=.05` still works. Recognised
    /// keys: `jitter`, `passes`, `extension`/`ext`, `depthcue`/`depth`,
    /// `endpoints` (0/1), `hatching`/`hatch` (0/1).
    pub fn apply_tokens<'a>(mut self, tokens: impl IntoIterator<Item = &'a str>) -> Self {
        for tok in tokens {
            let Some((k, v)) = tok.split_once('=') else { continue };
            match k {
                "jitter" => {
                    if let Ok(x) = v.parse() {
                        self.jitter = x;
                    }
                }
                "passes" => {
                    if let Ok(x) = v.parse::<u32>() {
                        self.passes = x.max(1);
                    }
                }
                "extension" | "ext" => {
                    if let Ok(x) = v.parse() {
                        self.extension = x;
                    }
                }
                "depthcue" | "depth" => {
                    if let Ok(x) = v.parse::<f32>() {
                        self.depthcue = x.clamp(0.0, 1.0);
                    }
                }
                "endpoints" | "ends" => self.endpoints = parse_flag(v).unwrap_or(self.endpoints),
                "hatching" | "hatch" => self.hatching = parse_flag(v).unwrap_or(self.hatching),
                _ => {}
            }
        }
        self
    }
}

/// Parse a boolean-ish flag token (`on`/`off`/`1`/`0`/`true`/`false`).
fn parse_flag(v: &str) -> Option<bool> {
    match v {
        "on" | "1" | "true" | "yes" => Some(true),
        "off" | "0" | "false" | "no" => Some(false),
        _ => None,
    }
}

/// Deterministic 64-bit hash of an edge, derived from its (quantized) endpoints.
/// Quantizing to `~0.1 mm` makes the seed independent of tiny float noise while
/// staying unique per distinct edge — the "edge id" the wobble hangs off. The
/// endpoints are folded order-independently so `a→b` and `b→a` hash the same.
pub fn edge_seed(a: [f32; 3], b: [f32; 3]) -> u64 {
    fn quant(v: f32) -> i64 {
        (v as f64 * 10_000.0).round() as i64
    }
    fn fold(mut h: u64, x: i64) -> u64 {
        h ^= x as u64;
        h.wrapping_mul(0x100000001b3) // FNV prime
    }
    let mut ha = 0xcbf29ce484222325u64;
    for v in a {
        ha = fold(ha, quant(v));
    }
    let mut hb = 0xcbf29ce484222325u64;
    for v in b {
        hb = fold(hb, quant(v));
    }
    // Order-independent combine (XOR + mix) so segment direction doesn't matter.
    let mixed = ha ^ hb;
    mixed
        .wrapping_mul(0x9e3779b97f4a7c15)
        .rotate_left(31)
        .wrapping_add(ha.wrapping_add(hb))
}

/// A pseudo-random scalar in `[-1, 1]` from a seed + pass/axis salt. Pure hash,
/// no RNG state — identical input always yields the identical value.
fn hashed_unit(seed: u64, salt: u64) -> f32 {
    let mut h = seed ^ salt.wrapping_mul(0x9e3779b97f4a7c15);
    h ^= h >> 33;
    h = h.wrapping_mul(0xff51afd7ed558ccd);
    h ^= h >> 33;
    // Top 24 bits → [0,1) → [-1,1].
    let unit = (h >> 40) as f32 / (1u64 << 24) as f32;
    unit * 2.0 - 1.0
}

/// Deterministic jitter offset (world units) for a given edge, drawn pass and
/// perpendicular basis. `perp1`/`perp2` are two unit vectors spanning the plane
/// perpendicular to the edge; the returned vector lies in that plane so the
/// wobble is always *across* the stroke, never along it.
pub fn jitter_offset(
    seed: u64,
    pass: u32,
    amplitude: f32,
    perp1: glam::Vec3,
    perp2: glam::Vec3,
) -> glam::Vec3 {
    let s = seed ^ ((pass as u64).wrapping_add(1).wrapping_mul(0xa24baed4963ee407));
    let u = hashed_unit(s, 0x1111_1111);
    let v = hashed_unit(s, 0x2222_2222);
    (perp1 * u + perp2 * v) * amplitude
}

/// Two orthonormal vectors perpendicular to `dir` (assumed non-zero, need not be
/// unit). Used as the plane for jitter offsets.
fn perp_basis(dir: glam::Vec3) -> (glam::Vec3, glam::Vec3) {
    let d = dir.normalize_or_zero();
    // Pick a reference axis least aligned with `d` to avoid a degenerate cross.
    let up = if d.z.abs() < 0.9 { glam::Vec3::Z } else { glam::Vec3::X };
    let p1 = d.cross(up).normalize_or_zero();
    let p2 = d.cross(p1).normalize_or_zero();
    (p1, p2)
}

/// Depth-cue multiplier for a point: foreground (near the eye) → up to
/// `1 + strength`, far background → `1`. `eye` is the camera position; the falloff
/// is over the scene's own extent so it is scale-independent. Returns `1.0` when
/// depth cue is disabled or no eye is supplied.
fn depth_gain(mid: glam::Vec3, eye: Option<glam::Vec3>, strength: f32, radius: f32) -> f32 {
    match eye {
        Some(e) if strength > 0.0 && radius > 0.0 => {
            let d = (mid - e).length();
            // Near (d small) → t≈1; far (d≈2*radius) → t≈0.
            let t = (1.0 - (d / (2.0 * radius)).clamp(0.0, 1.0)).clamp(0.0, 1.0);
            1.0 + strength * t
        }
        _ => 1.0,
    }
}

/// Apply the sketchy transform to a segment soup (consecutive point pairs).
/// Returns a new segment soup — possibly several times larger — carrying the
/// jitter overdraw, endpoint overshoot and depth-cued amplitude. When
/// [`SketchyParams::active`] is false the input is returned unchanged (cloned),
/// so callers can apply unconditionally.
///
/// `eye`/`radius` drive the depth cue: `radius` is roughly the scene's bounding
/// radius so the falloff is scale-independent. Pass `eye = None` to skip it.
pub fn sketchify_segments(
    segments: &[[f32; 3]],
    params: SketchyParams,
    eye: Option<glam::Vec3>,
    radius: f32,
) -> Vec<[f32; 3]> {
    if !params.active() {
        return segments.to_vec();
    }
    let passes = params.passes.max(1);
    let mut out: Vec<[f32; 3]> = Vec::with_capacity(segments.len() * passes as usize);
    let n = segments.len() / 2 * 2;
    let mut k = 0;
    while k + 1 < n {
        let a = glam::Vec3::from_array(segments[k]);
        let b = glam::Vec3::from_array(segments[k + 1]);
        k += 2;
        let dir = b - a;
        let len = dir.length();
        if len < 1e-9 {
            continue;
        }
        let dirn = dir / len;
        let (p1, p2) = perp_basis(dir);
        let mid = (a + b) * 0.5;
        let gain = depth_gain(mid, eye, params.depthcue, radius);
        let ext = params.extension * gain;
        let jit = params.jitter * gain;
        // Overshoot the endpoints along the edge direction.
        let a_ext = a - dirn * ext;
        let b_ext = b + dirn * ext;
        let seed = edge_seed(segments[k - 2], segments[k - 1]);
        for pass in 0..passes {
            // Jitter each endpoint independently so the stroke bows, not just
            // shifts. End jitter is scaled down so the ends stay near the true
            // corner even as the middle wobbles.
            let ja = jitter_offset(seed, pass * 2, jit * 0.5, p1, p2);
            let jb = jitter_offset(seed, pass * 2 + 1, jit * 0.5, p1, p2);
            out.push((a_ext + ja).to_array());
            out.push((b_ext + jb).to_array());
        }
        if params.endpoints {
            // Emit the overshoot tips as their own short segments so the pen
            // ends read thicker (drawn on top of the main stroke).
            let cap = (ext.max(jit) * 0.6).max(len * 0.02);
            out.push(a.to_array());
            out.push((a - dirn * cap).to_array());
            out.push(b.to_array());
            out.push((b + dirn * cap).to_array());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_inactive_identity() {
        let p = SketchyParams::default();
        assert!(!p.active(), "default (disabled) is not active");
        let segs = vec![[0.0, 0.0, 0.0], [1.0, 0.0, 0.0]];
        assert_eq!(sketchify_segments(&segs, p, None, 10.0), segs);
    }

    #[test]
    fn edge_seed_is_order_independent_and_stable() {
        let a = [0.0, 1.0, 2.0];
        let b = [3.0, 4.0, 5.0];
        let s1 = edge_seed(a, b);
        let s2 = edge_seed(b, a);
        assert_eq!(s1, s2, "a→b and b→a hash the same");
        // Stable across calls.
        assert_eq!(edge_seed(a, b), s1);
        // Different edges differ.
        assert_ne!(edge_seed(a, b), edge_seed(a, [9.0, 9.0, 9.0]));
    }

    #[test]
    fn jitter_is_deterministic_same_input_same_offset() {
        let seed = edge_seed([0.0, 0.0, 0.0], [1.0, 0.0, 0.0]);
        let (p1, p2) = perp_basis(glam::Vec3::X);
        let o1 = jitter_offset(seed, 0, 0.1, p1, p2);
        let o2 = jitter_offset(seed, 0, 0.1, p1, p2);
        assert_eq!(o1, o2, "identical input → identical offset");
        // Different pass → different offset (overdraw copies don't coincide).
        assert_ne!(jitter_offset(seed, 0, 0.1, p1, p2), jitter_offset(seed, 1, 0.1, p1, p2));
        // Offset stays in the perpendicular plane (no along-edge component).
        assert!(o1.dot(glam::Vec3::X).abs() < 1e-5, "jitter is across the stroke");
        // Amplitude bounds it.
        assert!(o1.length() <= 0.1 * std::f32::consts::SQRT_2 + 1e-5);
    }

    #[test]
    fn sketchify_produces_overdraw_and_extension() {
        let params = SketchyParams { passes: 3, ..SketchyParams::on() };
        let segs = vec![[0.0, 0.0, 0.0], [1.0, 0.0, 0.0]];
        let out = sketchify_segments(&segs, params, None, 10.0);
        // 3 passes → 3 segments → 6 points.
        assert_eq!(out.len(), 6, "one edge → passes copies");
        // Extension: first pass start overshoots below x=0.
        assert!(out[0][0] < 0.0, "start overshoots past the endpoint");
        assert!(out[1][0] > 1.0, "end overshoots past the endpoint");
    }

    #[test]
    fn sketchify_whole_output_is_deterministic() {
        let params = SketchyParams { passes: 2, endpoints: true, ..SketchyParams::on() };
        let segs = vec![
            [0.0, 0.0, 0.0], [1.0, 0.0, 0.0],
            [1.0, 0.0, 0.0], [1.0, 1.0, 0.0],
        ];
        let a = sketchify_segments(&segs, params, None, 10.0);
        let b = sketchify_segments(&segs, params, None, 10.0);
        assert_eq!(a, b, "same input → byte-identical output");
    }

    #[test]
    fn endpoints_add_extra_cap_segments() {
        let base = SketchyParams { passes: 1, ..SketchyParams::on() };
        let segs = vec![[0.0, 0.0, 0.0], [1.0, 0.0, 0.0]];
        let without = sketchify_segments(&segs, base, None, 10.0);
        let with = sketchify_segments(&segs, SketchyParams { endpoints: true, ..base }, None, 10.0);
        assert!(with.len() > without.len(), "endpoints add cap segments");
        assert_eq!(with.len(), without.len() + 4, "two caps → 4 extra points");
    }

    #[test]
    fn apply_tokens_tunes_params() {
        let p = SketchyParams::on()
            .apply_tokens(["jitter=0.1", "passes=4", "ext=0.2", "depth=0.5", "endpoints=on"]);
        assert_eq!(p.jitter, 0.1);
        assert_eq!(p.passes, 4);
        assert_eq!(p.extension, 0.2);
        assert_eq!(p.depthcue, 0.5);
        assert!(p.endpoints);
        // Unknown/malformed tokens ignored, others still applied.
        let q = SketchyParams::on().apply_tokens(["bogus", "jitter=oops", "hatch=1"]);
        assert_eq!(q.jitter, SketchyParams::on().jitter, "bad value left default");
        assert!(q.hatching);
        // depthcue clamps to [0,1].
        assert_eq!(SketchyParams::on().apply_tokens(["depth=5"]).depthcue, 1.0);
        // passes floors at 1.
        assert_eq!(SketchyParams::on().apply_tokens(["passes=0"]).passes, 1);
    }

    #[test]
    fn depth_cue_boosts_foreground_over_background() {
        let params = SketchyParams { depthcue: 1.0, passes: 1, ..SketchyParams::on() };
        // Near edge (close to eye) vs far edge, same geometry length.
        let eye = Some(glam::Vec3::new(0.0, 0.0, 0.0));
        let near = vec![[0.5, 0.0, 0.0], [1.5, 0.0, 0.0]]; // mid ~1 from eye
        let far = vec![[18.0, 0.0, 0.0], [19.0, 0.0, 0.0]]; // mid ~18.5 from eye
        let near_out = sketchify_segments(&near, params, eye, 10.0);
        let far_out = sketchify_segments(&far, params, eye, 10.0);
        // Foreground overshoot is larger (extension * gain) → its start reaches
        // further below its own left endpoint than the far edge does.
        let near_overshoot = 0.5 - near_out[0][0];
        let far_overshoot = 18.0 - far_out[0][0];
        assert!(
            near_overshoot > far_overshoot,
            "foreground extension {near_overshoot} > background {far_overshoot}"
        );
    }
}
