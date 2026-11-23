// SPDX-License-Identifier: AGPL-3.0-only
// Copyright © 2026 Hector Tarrido-Picart

//! Edge inferences and screen tips (#373, SketchUp parity): On Edge, Extend
//! Edge, Parallel / Perpendicular to Edge (from the last edge the cursor
//! dwelt on, ↓ locks it), From Point (SmartTrack's axis guides from an
//! acquired point), and the one tip at the cursor that names whichever
//! inference placed the point. The geometry is in
//! [`itsjustcad_guided::inference`]; this holds the app-side state rules.
//!
//! Cursor precedence while picking a point (app `drawing_input`):
//!   1. an object snap point (end, mid, …);
//!   2. On Edge — a point on an edge under the cursor;
//!   3. On Face — the face under the cursor, else the ground, is the base
//!      point the inferences below then move:
//!   4. SmartTrack guides (From Point), then Extend Edge, then the band
//!      direction: parallel / perpendicular to the reference edge or a
//!      world axis, whichever is closest on screen (an axis on a tie);
//!   5. ortho.
//!
//! A lock (→ ← ↑ axis, ↓ edge, Shift hold) overrides all of them.

use std::time::{Duration, Instant};

use itsjustcad_guided::inference::{Edge, EdgeRel};

/// How long the cursor rests on an edge before it becomes the reference
/// edge for parallel / perpendicular (as SmartTrack's point dwell).
pub(crate) const EDGE_DWELL: Duration = Duration::from_millis(250);

/// SketchUp's magenta for edge inferences (band, edge highlight, tip).
pub(crate) const EDGE_RGB: [u8; 3] = [225, 70, 225];

/// The edge being dwelt on and since when.
pub(crate) type Dwell = Option<(Edge, Instant)>;

/// Advance the dwell on `hovered` at `now`: the edge, once the cursor has
/// rested on it for [`EDGE_DWELL`] (it then becomes the reference edge).
pub(crate) fn dwell(state: &mut Dwell, hovered: Option<Edge>, now: Instant, tol: f64) -> Option<Edge> {
    let Some(e) = hovered else {
        *state = None;
        return None;
    };
    match *state {
        Some((d, since)) if d.same_as(&e, tol) => (now.duration_since(since) >= EDGE_DWELL).then_some(d),
        _ => {
            *state = Some((e, now));
            None
        }
    }
}

/// The world axis a ground-plane SmartTrack guide runs along (`x`/`y`), if
/// any — From Point guides take that axis's colour.
pub(crate) fn guide_axis(dir: glam::DVec3) -> Option<char> {
    if dir.y.abs() < 1e-9 {
        Some('x')
    } else if dir.x.abs() < 1e-9 {
        Some('y')
    } else {
        None
    }
}

/// The screen tip at the cursor: which inference placed the point.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Tip {
    OnEdge,
    ExtendEdge,
    Edge { rel: EdgeRel, locked: bool },
    Axis { axis: char, locked: bool },
    FromPoint(char),
    OnFace { held: bool },
}

/// What placed this frame's point, for [`choose_tip`].
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Placed {
    pub on_edge: bool,
    pub extend_edge: bool,
    pub edge_rel: Option<(EdgeRel, bool)>,
    pub axis: Option<(char, bool)>,
    pub from_point: Option<char>,
    pub on_face: Option<bool>,
}

/// One tip, the most specific first: on/extend edge, an edge relation, an
/// axis, From Point, On Face.
pub(crate) fn choose_tip(p: Placed) -> Option<Tip> {
    if p.on_edge {
        return Some(Tip::OnEdge);
    }
    if p.extend_edge {
        return Some(Tip::ExtendEdge);
    }
    if let Some((rel, locked)) = p.edge_rel {
        return Some(Tip::Edge { rel, locked });
    }
    if let Some((axis, locked)) = p.axis {
        return Some(Tip::Axis { axis, locked });
    }
    if let Some(a) = p.from_point {
        return Some(Tip::FromPoint(a));
    }
    p.on_face.map(|held| Tip::OnFace { held })
}

/// The tip's i18n key, and whether "· locked" follows it.
fn tip_key(tip: Tip) -> (&'static str, bool) {
    match tip {
        Tip::OnEdge => ("infer.on_edge", false),
        Tip::ExtendEdge => ("infer.extend_edge", false),
        Tip::Edge { rel: EdgeRel::Parallel, locked } => ("infer.parallel", locked),
        Tip::Edge { rel: EdgeRel::Perpendicular, locked } => ("infer.perpendicular", locked),
        Tip::Axis { axis: 'x', locked } => ("axis.on_red", locked),
        Tip::Axis { axis: 'y', locked } => ("axis.on_green", locked),
        Tip::Axis { locked, .. } => ("axis.on_blue", locked),
        Tip::FromPoint(_) => ("infer.from_point", false),
        Tip::OnFace { held } => ("face.on", held),
    }
}

/// The tip's localized text ("On Red Axis · locked").
pub(crate) fn tip_text(tip: Tip) -> String {
    use crate::i18n::t;
    let (key, locked) = tip_key(tip);
    if locked {
        format!("{} · {}", t(key), t("axis.locked"))
    } else {
        t(key).to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use glam::DVec3;

    #[test]
    fn an_edge_becomes_the_reference_after_the_dwell() {
        let e = Edge { a: DVec3::ZERO, b: DVec3::X };
        let t0 = Instant::now();
        let mut s = None;
        assert_eq!(dwell(&mut s, Some(e), t0, 1e-6), None);
        assert_eq!(dwell(&mut s, Some(e), t0 + EDGE_DWELL / 2, 1e-6), None);
        let flipped = Edge { a: e.b, b: e.a };
        assert_eq!(dwell(&mut s, Some(flipped), t0 + EDGE_DWELL, 1e-6), Some(e), "same edge");
        // Another edge restarts; leaving all edges resets.
        let f = Edge { a: DVec3::Y, b: DVec3::ONE };
        assert_eq!(dwell(&mut s, Some(f), t0 + EDGE_DWELL * 3, 1e-6), None);
        assert_eq!(dwell(&mut s, None, t0 + EDGE_DWELL * 4, 1e-6), None);
        assert_eq!(dwell(&mut s, Some(f), t0 + EDGE_DWELL * 4, 1e-6), None, "reset");
    }

    #[test]
    fn the_tip_names_the_most_specific_inference() {
        let all = Placed {
            on_edge: true,
            extend_edge: true,
            edge_rel: Some((EdgeRel::Parallel, false)),
            axis: Some(('x', true)),
            from_point: Some('y'),
            on_face: Some(false),
        };
        assert_eq!(choose_tip(all), Some(Tip::OnEdge));
        let p = Placed { on_edge: false, ..all };
        assert_eq!(choose_tip(p), Some(Tip::ExtendEdge));
        let p = Placed { extend_edge: false, ..p };
        assert_eq!(choose_tip(p), Some(Tip::Edge { rel: EdgeRel::Parallel, locked: false }));
        let p = Placed { edge_rel: None, ..p };
        assert_eq!(choose_tip(p), Some(Tip::Axis { axis: 'x', locked: true }));
        let p = Placed { axis: None, ..p };
        assert_eq!(choose_tip(p), Some(Tip::FromPoint('y')));
        let p = Placed { from_point: None, ..p };
        assert_eq!(choose_tip(p), Some(Tip::OnFace { held: false }));
        assert_eq!(choose_tip(Placed::default()), None);
    }

    #[test]
    fn tips_read_like_sketchup_and_are_translated() {
        use crate::i18n::{Lang, lookup};
        let en = |tip| {
            let (key, locked) = tip_key(tip);
            for lang in [Lang::Es, Lang::Pt, Lang::Fr, Lang::It, Lang::Ro, Lang::Ca, Lang::Gl] {
                assert!(lookup(lang, key).is_some(), "{key} in {lang:?}");
            }
            let on = lookup(Lang::En, key).unwrap();
            if locked { format!("{on} · locked") } else { on.to_string() }
        };
        assert_eq!(en(Tip::OnEdge), "On Edge");
        assert_eq!(en(Tip::ExtendEdge), "On Edge Extension");
        assert_eq!(en(Tip::Edge { rel: EdgeRel::Parallel, locked: false }), "Parallel to Edge");
        assert_eq!(en(Tip::Edge { rel: EdgeRel::Perpendicular, locked: true }), "Perpendicular to Edge · locked");
        assert_eq!(en(Tip::FromPoint('x')), "From Point");
        assert_eq!(en(Tip::Axis { axis: 'z', locked: false }), "On Blue Axis");
        assert_eq!(en(Tip::OnFace { held: true }), "On Face · locked");
        assert_eq!(guide_axis(DVec3::X), Some('x'));
        assert_eq!(guide_axis(DVec3::NEG_Y), Some('y'));
        assert_eq!(guide_axis(DVec3::new(0.6, 0.8, 0.0)), None);
    }
}
