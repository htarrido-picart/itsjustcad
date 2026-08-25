// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! Object snapping: click-to-draw sticks to endpoints, midpoints and centers
//! of existing geometry (screen-space radius), falling back to the 10cm grid.

use glam::DVec3;
use kernel_curve::Curve;
use itsjustcad_doc::{Document, Geometry};

/// Screen-space pick radius in logical pixels.
pub const SNAP_RADIUS_PX: f32 = 10.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SnapKind {
    End,
    Mid,
    Center,
}

impl SnapKind {
    pub fn label(self) -> &'static str {
        match self {
            SnapKind::End => "end",
            SnapKind::Mid => "mid",
            SnapKind::Center => "cen",
        }
    }
}

/// Fallback grid snap (10cm) for clicks in empty space.
pub fn grid_snap(p: DVec3) -> DVec3 {
    (p * 10.0).round() / 10.0
}

/// Collect snap candidates from the whole document (no culling). The live app
/// uses [`candidates_filtered`] with a screen-proximity predicate; this
/// unfiltered form is the reference used by tests and any caller that wants
/// every point.
#[cfg_attr(not(test), allow(dead_code))]
pub fn candidates(doc: &Document) -> Vec<(DVec3, SnapKind)> {
    candidates_filtered(doc, |_| true)
}

/// Screen-proximity culled variant: only objects for which `keep(aabb)` returns
/// true contribute candidates. Callers pass a predicate that projects the
/// object's world AABB and keeps it when it lands within the snap radius of the
/// cursor, so a 10k-object scene only pushes points from the handful of objects
/// under the pointer. Equivalent to [`candidates`] when `keep` is always true.
pub fn candidates_filtered(
    doc: &Document,
    keep: impl Fn(kernel_mesh::Aabb) -> bool,
) -> Vec<(DVec3, SnapKind)> {
    let mut out = Vec::new();
    for obj in doc.objects() {
        if !obj.visible || !doc.layer_visible(&obj.layer) {
            continue; // invisible geometry must not attract the cursor
        }
        if !keep(obj.geometry.aabb()) {
            continue; // object nowhere near the cursor — skip its points
        }
        match &obj.geometry {
            Geometry::Curve(c) => curve_candidates(c, &mut out),
            Geometry::Mesh(m)
            | Geometry::Frame { mesh: m, .. }
            | Geometry::Area { mesh: m, .. } => {
                // Mesh vertices are the natural corners of massing solids and
                // structural members.
                out.extend(m.positions().iter().map(|p| (*p, SnapKind::End)));
            }
            // Annotation anchors (dim points, text position, hatch boundary).
            Geometry::Annotation(a) => {
                out.extend(a.points().into_iter().map(|p| (p, SnapKind::End)));
            }
            // Block instance: snap to insertion point.
            Geometry::Instance { position, .. } => {
                out.push((*position, SnapKind::End));
            }
            // Point clouds: only snap when the cloud is small; large clouds
            // would swamp the candidate list and hurt performance.
            Geometry::Points { positions } if positions.len() <= 5_000 => {
                out.extend(positions.iter().map(|p| (*p, SnapKind::End)));
            }
            Geometry::Points { .. } => {
                // Too many points — skip osnap to keep the candidate list lean.
            }
        }
    }
    out
}

fn curve_candidates(c: &Curve, out: &mut Vec<(DVec3, SnapKind)>) {
    match c {
        Curve::Line { a, b } => {
            out.push((*a, SnapKind::End));
            out.push((*b, SnapKind::End));
            out.push(((*a + *b) / 2.0, SnapKind::Mid));
        }
        Curve::Polyline { points, closed } => {
            out.extend(points.iter().map(|p| (*p, SnapKind::End)));
            let segs = if *closed { points.len() } else { points.len().saturating_sub(1) };
            for i in 0..segs {
                let mid = (points[i] + points[(i + 1) % points.len()]) / 2.0;
                out.push((mid, SnapKind::Mid));
            }
        }
        Curve::Arc { center, radius, start, end } => {
            out.push((*center, SnapKind::Center));
            if !c.is_closed() {
                for t in [*start, *end] {
                    out.push((
                        *center + DVec3::new(radius * t.cos(), radius * t.sin(), 0.0),
                        SnapKind::End,
                    ));
                }
            } else {
                // Full circle: quadrants snap as midpoints.
                for i in 0..4 {
                    let t = std::f64::consts::FRAC_PI_2 * i as f64;
                    out.push((
                        *center + DVec3::new(radius * t.cos(), radius * t.sin(), 0.0),
                        SnapKind::Mid,
                    ));
                }
            }
        }
        Curve::Ellipse { center, .. } => out.push((*center, SnapKind::Center)),
        Curve::Nurbs { control, .. } => {
            if let (Some(a), Some(b)) = (control.first(), control.last()) {
                out.push((*a, SnapKind::End));
                out.push((*b, SnapKind::End));
            }
        }
    }
}

/// Nearest candidate within `radius_px` of the cursor, in screen space —
/// depth does not matter, what's visually closest wins (Rhino behavior).
pub fn resolve(
    candidates: &[(DVec3, SnapKind)],
    cursor: egui::Pos2,
    radius_px: f32,
    project: impl Fn(DVec3) -> Option<egui::Pos2>,
) -> Option<(DVec3, SnapKind)> {
    let mut best: Option<(f32, DVec3, SnapKind)> = None;
    for (p, kind) in candidates {
        let Some(screen) = project(*p) else { continue };
        let d = screen.distance(cursor);
        if d <= radius_px && best.is_none_or(|(bd, _, _)| d < bd) {
            best = Some((d, *p, *kind));
        }
    }
    best.map(|(_, p, k)| (p, k))
}

#[cfg(test)]
mod tests {
    use super::*;
    use itsjustcad_doc::{ObjectId, SceneObject};

    fn doc_with(geometry: Geometry) -> Document {
        let mut doc = Document::default();
        doc.insert(SceneObject {
            visible: true,
            id: ObjectId::new(),
            name: None,
            layer: itsjustcad_doc::DEFAULT_LAYER.to_string(),
            color: None,
            material: None,
            lineweight_mm: None,
            geometry,
        });
        doc
    }

    /// Identity-ish projection: world xy -> screen xy (z ignored).
    fn flat(p: DVec3) -> Option<egui::Pos2> {
        Some(egui::pos2(p.x as f32, p.y as f32))
    }

    #[test]
    fn line_candidates_ends_and_mid() {
        let doc = doc_with(Geometry::Curve(Curve::Line {
            a: DVec3::ZERO,
            b: DVec3::new(10.0, 0.0, 0.0),
        }));
        let c = candidates(&doc);
        assert!(c.contains(&(DVec3::ZERO, SnapKind::End)));
        assert!(c.contains(&(DVec3::new(10.0, 0.0, 0.0), SnapKind::End)));
        assert!(c.contains(&(DVec3::new(5.0, 0.0, 0.0), SnapKind::Mid)));
    }

    #[test]
    fn closed_polyline_wraps_midpoints() {
        let doc = doc_with(Geometry::Curve(Curve::Polyline {
            points: vec![
                DVec3::ZERO,
                DVec3::new(4.0, 0.0, 0.0),
                DVec3::new(4.0, 4.0, 0.0),
                DVec3::new(0.0, 4.0, 0.0),
            ],
            closed: true,
        }));
        let c = candidates(&doc);
        // 4 ends + 4 mids including the closing segment's
        assert!(c.contains(&(DVec3::new(0.0, 2.0, 0.0), SnapKind::Mid)));
        assert_eq!(c.iter().filter(|(_, k)| *k == SnapKind::Mid).count(), 4);
    }

    #[test]
    fn circle_center_and_quadrants() {
        let doc = doc_with(Geometry::Curve(Curve::Arc {
            center: DVec3::new(2.0, 3.0, 0.0),
            radius: 1.0,
            start: 0.0,
            end: std::f64::consts::TAU,
        }));
        let c = candidates(&doc);
        assert!(c.contains(&(DVec3::new(2.0, 3.0, 0.0), SnapKind::Center)));
        assert_eq!(c.iter().filter(|(_, k)| *k == SnapKind::Mid).count(), 4);
        assert!(!c.iter().any(|(_, k)| *k == SnapKind::End)); // closed: no ends
    }

    #[test]
    fn mesh_vertices_snap_as_ends() {
        let doc = doc_with(Geometry::Mesh(kernel_mesh::make_box(
            DVec3::ZERO,
            DVec3::splat(2.0),
        )));
        let c = candidates(&doc);
        assert_eq!(c.len(), 8);
        assert!(c.contains(&(DVec3::splat(2.0), SnapKind::End)));
    }

    #[test]
    fn resolve_picks_nearest_within_radius() {
        let cands = vec![
            (DVec3::new(100.0, 100.0, 0.0), SnapKind::End),
            (DVec3::new(104.0, 100.0, 0.0), SnapKind::Mid),
        ];
        // Cursor at 103,100: both within 10px, Mid is nearer.
        let hit = resolve(&cands, egui::pos2(103.0, 100.0), 10.0, flat).unwrap();
        assert_eq!(hit.1, SnapKind::Mid);
        // Far away: no snap.
        assert!(resolve(&cands, egui::pos2(300.0, 300.0), 10.0, flat).is_none());
    }

    #[test]
    fn grid_snap_rounds_to_10cm() {
        assert_eq!(
            grid_snap(DVec3::new(1.234, 5.678, 0.0)),
            DVec3::new(1.2, 5.7, 0.0)
        );
    }

    // ---- stress harness: 10k-object pick + osnap under a loose time bound ----

    /// Build a document with `n` unit boxes on a grid so pick/osnap have a large,
    /// spatially spread scene to cull against.
    fn grid_doc(n: usize) -> Document {
        let mut doc = Document::default();
        let side = (n as f64).sqrt().ceil() as usize;
        for i in 0..n {
            let (gx, gy) = ((i % side) as f64, (i / side) as f64);
            let corner = DVec3::new(gx * 3.0, gy * 3.0, 0.0);
            doc.insert(SceneObject {
                visible: true,
                id: ObjectId::new(),
                name: None,
                layer: itsjustcad_doc::DEFAULT_LAYER.to_string(),
                color: None,
                material: None,
                lineweight_mm: None,
                geometry: Geometry::Mesh(kernel_mesh::make_box(corner, corner + DVec3::ONE)),
            });
        }
        doc
    }

    #[test]
    fn stress_pick_and_osnap_10k_objects() {
        let n = 10_000;
        let doc = grid_doc(n);

        // BVH-accelerated pick: cast a ray at the scene and cull object AABBs.
        let boxes: Vec<kernel_mesh::Aabb> =
            doc.objects().map(|o| o.geometry.aabb()).collect();
        let t0 = std::time::Instant::now();
        let bvh = kernel_mesh::Bvh::build(&boxes);
        let build_ms = t0.elapsed().as_secs_f64() * 1e3;

        let origin = DVec3::new(15.0, 15.0, 100.0);
        let dir = DVec3::new(0.0, 0.0, -1.0);
        let t1 = std::time::Instant::now();
        let mut picks = 0usize;
        for _ in 0..1000 {
            picks += bvh.ray_candidates(origin, dir).len();
        }
        let pick_ms = t1.elapsed().as_secs_f64() * 1e3 / 1000.0;
        assert!(picks > 0, "ray should cross at least one box");

        // Osnap culling: keep only objects whose AABB overlaps a small world
        // window around a query point (stand-in for the screen-proximity cull).
        let win = kernel_mesh::Aabb::from_points([
            DVec3::new(14.0, 14.0, -1.0),
            DVec3::new(16.0, 16.0, 2.0),
        ]);
        let t2 = std::time::Instant::now();
        let cands = candidates_filtered(&doc, |bb| {
            bb.min.x <= win.max.x
                && bb.max.x >= win.min.x
                && bb.min.y <= win.max.y
                && bb.max.y >= win.min.y
        });
        let osnap_ms = t2.elapsed().as_secs_f64() * 1e3;

        // Loose, non-flaky bound: just require the whole thing to complete well
        // under a second on any dev machine, and log the real numbers.
        eprintln!(
            "stress {n} objs: bvh build {build_ms:.2} ms, pick {pick_ms:.4} ms/ray, \
             osnap cull {osnap_ms:.2} ms -> {} candidates",
            cands.len()
        );
        assert!(build_ms < 1000.0, "bvh build too slow: {build_ms} ms");
        assert!(pick_ms < 50.0, "pick too slow: {pick_ms} ms/ray");
        assert!(osnap_ms < 1000.0, "osnap cull too slow: {osnap_ms} ms");
        // The cull must actually shrink the candidate set versus the whole scene.
        assert!(cands.len() < n, "cull should drop most objects, got {}", cands.len());
    }
}
