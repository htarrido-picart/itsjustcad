//! Object snapping: click-to-draw sticks to endpoints, midpoints and centers
//! of existing geometry (screen-space radius), falling back to the 10cm grid.

use glam::DVec3;
use kernel_curve::Curve;
use mydrafter_doc::{Document, Geometry};

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

/// Collect snap candidates from the whole document. Cheap at massing scale;
/// callers may cache on `doc.generation` if it ever shows up in a profile.
pub fn candidates(doc: &Document) -> Vec<(DVec3, SnapKind)> {
    let mut out = Vec::new();
    for obj in doc.objects() {
        match &obj.geometry {
            Geometry::Curve(c) => curve_candidates(c, &mut out),
            Geometry::Mesh(m) => {
                // Mesh vertices are the natural corners of massing solids.
                out.extend(m.positions().iter().map(|p| (*p, SnapKind::End)));
            }
            // Annotation anchors (dim points, text position, hatch boundary).
            Geometry::Annotation(a) => {
                out.extend(a.points().into_iter().map(|p| (p, SnapKind::End)));
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
    use mydrafter_doc::{ObjectId, SceneObject};

    fn doc_with(geometry: Geometry) -> Document {
        let mut doc = Document::default();
        doc.insert(SceneObject {
            id: ObjectId::new(),
            name: None,
            layer: mydrafter_doc::DEFAULT_LAYER.to_string(),
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
}
