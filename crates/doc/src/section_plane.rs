// SPDX-License-Identifier: AGPL-3.0-only

//! Live section planes (#359, SketchUp's Section Plane): named planes kept on
//! the document, at most one of them active. The active plane cuts the
//! viewport live (the renderer discards the cut side) and picking ignores
//! what it cut away.
//!
//! A plane is a point and a unit normal. The normal points at the side the
//! cut REMOVES: a plane placed on a face takes the face's outward normal, so
//! the face itself stays and everything past it is cut. `reverse` flips it.
//!
//! Planes and the active one are document state, set by the logged
//! `sectionplane` ops, so a file replays its cut. SketchUp keeps one active
//! cut per context (group / component); this model has no edit contexts yet,
//! so there is one active cut per document.
//!
//! Whether the cut is shown at all (`sections cuts on|off`) is view state:
//! [`crate::Document::section_cuts_hidden`], never saved.

use glam::DVec3;
use serde::{Deserialize, Serialize};

/// One named section plane (#359).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SectionPlane {
    pub name: String,
    /// A point on the plane (where it was placed).
    pub point: DVec3,
    /// Unit normal, pointing at the side the cut removes.
    pub normal: DVec3,
}

impl SectionPlane {
    /// A plane through `point` facing `normal` (normalized); `None` for a
    /// zero or non-finite normal or point.
    pub fn new(name: impl Into<String>, point: DVec3, normal: DVec3) -> Option<Self> {
        let normal = normal.try_normalize()?;
        point.is_finite().then(|| Self { name: name.into(), point, normal })
    }

    /// Signed distance of `p` from the plane: positive on the cut side.
    pub fn signed(&self, p: DVec3) -> f64 {
        (p - self.point).dot(self.normal)
    }

    /// Whether `p` survives the cut: on the kept side, or on the plane within
    /// a small tolerance relative to the coordinates (a face the plane was
    /// placed on stays).
    pub fn keeps(&self, p: DVec3) -> bool {
        self.signed(p) <= 1e-6 * (1.0 + self.point.abs().max_element() + p.abs().max_element())
    }

    /// Whether an axis-aligned box lies wholly on the cut side.
    pub fn cuts_box(&self, min: DVec3, max: DVec3) -> bool {
        // The box corner deepest on the kept side.
        let corner = DVec3::select(self.normal.cmpge(DVec3::ZERO), min, max);
        !self.keeps(corner)
    }

    /// Where along the ray `origin + t·dir` the kept side starts and ends:
    /// `Some((t0, t1))`, `t0 <= t1` (either may be infinite), or `None` when
    /// the whole ray is cut.
    pub fn kept_span(&self, origin: DVec3, dir: DVec3) -> Option<(f64, f64)> {
        let s0 = self.signed(origin);
        let ds = dir.dot(self.normal);
        if ds.abs() < 1e-15 {
            return self.keeps(origin).then_some((f64::NEG_INFINITY, f64::INFINITY));
        }
        let t = -s0 / ds;
        Some(if ds > 0.0 { (f64::NEG_INFINITY, t) } else { (t, f64::INFINITY) })
    }
}

/// Default name of the `n`-th plane (1-based): `section1`, `section2`, …
pub fn section_plane_name(n: usize) -> String {
    format!("section{n}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_the_back_side_and_the_plane_itself() {
        let p = SectionPlane::new("s", DVec3::new(0.0, 0.0, 2.0), DVec3::new(0.0, 0.0, 3.0)).unwrap();
        assert_eq!(p.normal, DVec3::Z, "normalized");
        assert!(p.keeps(DVec3::new(5.0, 5.0, 1.0)));
        assert!(p.keeps(DVec3::new(5.0, 5.0, 2.0)), "on the plane stays");
        assert!(!p.keeps(DVec3::new(0.0, 0.0, 2.1)));
        assert!(p.cuts_box(DVec3::new(0.0, 0.0, 3.0), DVec3::new(1.0, 1.0, 4.0)));
        assert!(!p.cuts_box(DVec3::new(0.0, 0.0, 1.0), DVec3::new(1.0, 1.0, 4.0)));
        assert!(SectionPlane::new("z", DVec3::ZERO, DVec3::ZERO).is_none());
    }

    #[test]
    fn kept_span_of_a_ray() {
        let p = SectionPlane::new("s", DVec3::new(0.0, 0.0, 2.0), DVec3::Z).unwrap();
        // Looking down from above the cut: kept from z = 2 on.
        let (t0, t1) = p.kept_span(DVec3::new(0.0, 0.0, 10.0), DVec3::NEG_Z).unwrap();
        assert!((t0 - 8.0).abs() < 1e-12 && t1.is_infinite());
        // Looking up from below: kept until z = 2.
        let (t0, t1) = p.kept_span(DVec3::ZERO, DVec3::Z).unwrap();
        assert!(t0.is_infinite() && (t1 - 2.0).abs() < 1e-12);
        // Parallel rays: all or nothing.
        assert!(p.kept_span(DVec3::new(0.0, 0.0, 3.0), DVec3::X).is_none());
        assert!(p.kept_span(DVec3::ZERO, DVec3::X).is_some());
    }
}
