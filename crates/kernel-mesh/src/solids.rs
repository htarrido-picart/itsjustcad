// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

use glam::{DQuat, DVec2, DVec3};

use crate::csg::signed_volume;
use crate::earcut::{earcut, signed_area};
use crate::Mesh;

/// Revolve a closed planar profile about the axis through `axis_pt` along
/// `axis_dir` by `angle` radians (CCW, right-hand rule). Full revolutions
/// produce a closed ring of side strips; partial ones are capped with the
/// profile at the start and end stations. `tol` is the max chord deviation of
/// the angular tessellation.
pub fn revolve_profile(
    profile: &[DVec3],
    axis_pt: DVec3,
    axis_dir: DVec3,
    angle: f64,
    tol: f64,
) -> Mesh {
    assert!(profile.len() >= 3, "profile needs at least 3 points");
    let axis = axis_dir.normalize();
    let rmax = profile
        .iter()
        .map(|p| {
            let d = *p - axis_pt;
            (d - axis * d.dot(axis)).length()
        })
        .fold(0.0, f64::max);
    let full = angle >= std::f64::consts::TAU - 1e-9;
    let n = segments_for(rmax, angle, tol);
    let stations = if full { n } else { n + 1 };
    let rings: Vec<Vec<DVec3>> = (0..stations)
        .map(|k| {
            let q = DQuat::from_axis_angle(axis, angle * k as f64 / n as f64);
            profile.iter().map(|p| axis_pt + q * (*p - axis_pt)).collect()
        })
        .collect();
    skin_stack(&rings, full)
}

/// Skin 2+ closed profile loops in stacking order into one solid: each loop is
/// resampled by arclength to a common point count, windings are aligned to the
/// first loop, seams are rotated to minimize twist, and the ends are capped.
pub fn loft_profiles(profiles: &[Vec<DVec3>]) -> Mesh {
    assert!(profiles.len() >= 2, "loft needs at least 2 profiles");
    let n = profiles.iter().map(Vec::len).max().expect("non-empty").max(3);
    let reference = newell(&profiles[0]);
    let mut rings: Vec<Vec<DVec3>> = Vec::with_capacity(profiles.len());
    for pts in profiles {
        let mut ring = resample_closed(pts, n);
        if newell(&ring).dot(reference) < 0.0 {
            ring.reverse();
        }
        if let Some(prev) = rings.last() {
            let s = (0..n)
                .min_by(|&a, &b| {
                    ring[a]
                        .distance_squared(prev[0])
                        .total_cmp(&ring[b].distance_squared(prev[0]))
                })
                .expect("n >= 3");
            ring.rotate_left(s);
        }
        rings.push(ring);
    }
    skin_stack(&rings, false)
}

/// Sweep a closed profile along an open rail polyline using parallel-transport
/// frames (minimal rotation between successive tangents, no twist), capping
/// both ends. The profile is centered on the rail at its own centroid.
pub fn sweep_profile(profile: &[DVec3], rail: &[DVec3]) -> Mesh {
    assert!(profile.len() >= 3, "profile needs at least 3 points");
    assert!(rail.len() >= 2, "rail needs at least 2 points");
    let centroid = profile.iter().copied().sum::<DVec3>() / profile.len() as f64;
    let normal = newell(profile).normalize_or_zero();
    let (pu, pv) = plane_basis(normal);
    let local: Vec<DVec2> = profile
        .iter()
        .map(|p| DVec2::new((*p - centroid).dot(pu), (*p - centroid).dot(pv)))
        .collect();

    let m = rail.len();
    // Vertex tangents: averaged segment directions at interior vertices.
    let tangent = |i: usize| -> DVec3 {
        let ahead = if i + 1 < m { rail[i + 1] - rail[i] } else { DVec3::ZERO };
        let behind = if i > 0 { rail[i] - rail[i - 1] } else { DVec3::ZERO };
        (ahead.normalize_or_zero() + behind.normalize_or_zero()).normalize_or_zero()
    };

    // Initial frame: rotate the profile plane so its normal follows the rail,
    // then parallel-transport that frame from tangent to tangent.
    let t0 = tangent(0);
    let q0 = DQuat::from_rotation_arc(normal, t0);
    let (mut u, mut v) = (q0 * pu, q0 * pv);
    let mut prev_t = t0;
    let rings: Vec<Vec<DVec3>> = (0..m)
        .map(|i| {
            let t = tangent(i);
            let q = DQuat::from_rotation_arc(prev_t, t);
            u = q * u;
            v = q * v;
            prev_t = t;
            local.iter().map(|l| rail[i] + u * l.x + v * l.y).collect()
        })
        .collect();
    skin_stack(&rings, false)
}

/// Sweep a closed profile between two open rails, lofting it station by station
/// so its extent tracks both rails. Rails are resampled to a common station
/// count; at each station the frame is built from rail-a's tangent and the chord
/// to rail-b, and the profile is scaled so its width spans the rail gap and
/// placed on the chord midpoint. Capped at both ends, watertight.
pub fn sweep2_profile(profile: &[DVec3], rail_a: &[DVec3], rail_b: &[DVec3]) -> Mesh {
    assert!(profile.len() >= 3, "profile needs at least 3 points");
    assert!(rail_a.len() >= 2 && rail_b.len() >= 2, "rails need 2+ points");
    let n = rail_a.len().max(rail_b.len()).max(2);
    let a = resample_open(rail_a, n);
    let b = resample_open(rail_b, n);

    // Profile local coords in its own plane; u spans profile width, so scaling u
    // to the rail gap makes the swept profile's ends touch both rails.
    let centroid = profile.iter().copied().sum::<DVec3>() / profile.len() as f64;
    let normal = newell(profile).normalize_or_zero();
    let (pu, pv) = plane_basis(normal);
    let local: Vec<DVec2> = profile
        .iter()
        .map(|p| DVec2::new((*p - centroid).dot(pu), (*p - centroid).dot(pv)))
        .collect();
    let half_w = local.iter().map(|l| l.x.abs()).fold(0.0, f64::max).max(1e-9);

    let tangent = |r: &[DVec3], i: usize| -> DVec3 {
        let ahead = if i + 1 < n { r[i + 1] - r[i] } else { DVec3::ZERO };
        let behind = if i > 0 { r[i] - r[i - 1] } else { DVec3::ZERO };
        (ahead.normalize_or_zero() + behind.normalize_or_zero()).normalize_or_zero()
    };

    let rings: Vec<Vec<DVec3>> = (0..n)
        .map(|i| {
            let chord = b[i] - a[i];
            let gap = chord.length().max(1e-9);
            let u = chord / gap; // points from rail-a toward rail-b
            let t = tangent(&a, i);
            // v is normal to both the rail run and the chord, so the profile's
            // height sits across the swept sheet (not along the run direction).
            let mut w = t.cross(u);
            if w.length_squared() < 1e-18 {
                w = plane_basis(u).0;
            }
            let v = w.normalize();
            let mid = (a[i] + b[i]) * 0.5;
            let sx = gap / (2.0 * half_w); // scale profile width to the gap
            local
                .iter()
                .map(|l| mid + u * (l.x * sx) + v * l.y)
                .collect()
        })
        .collect();
    skin_stack(&rings, false)
}

/// Revolve a closed profile about an axis where the radial offset is modulated
/// by a rail: at each angular station the profile is pushed out by the rail's
/// distance from the axis (resampled to the station count), so the swept radius
/// follows the rail. Full turns wrap; partial turns are capped.
pub fn rail_revolve_profile(
    profile: &[DVec3],
    rail: &[DVec3],
    axis_pt: DVec3,
    axis_dir: DVec3,
    tol: f64,
) -> Mesh {
    assert!(profile.len() >= 3, "profile needs at least 3 points");
    assert!(rail.len() >= 2, "rail needs at least 2 points");
    let axis = axis_dir.normalize();
    let (bu, _bv) = plane_basis(axis);
    // Radial distance of each rail point from the axis, sampled by rail angle
    // (angle of its projection about the axis, measured from bu).
    let mut samples: Vec<(f64, f64)> = rail
        .iter()
        .map(|p| {
            let d = *p - axis_pt;
            let radial = d - axis * d.dot(axis);
            let ang = radial.dot(axis.cross(bu)).atan2(radial.dot(bu));
            let ang = if ang < 0.0 { ang + std::f64::consts::TAU } else { ang };
            (ang, radial.length())
        })
        .collect();
    samples.sort_by(|a, b| a.0.total_cmp(&b.0));
    let radius_at = |a: f64| -> f64 {
        // Nearest-angle radius (rails are coarse guides, not smooth fields).
        samples
            .iter()
            .min_by(|x, y| {
                let da = ((x.0 - a).abs()).min(std::f64::consts::TAU - (x.0 - a).abs());
                let db = ((y.0 - a).abs()).min(std::f64::consts::TAU - (y.0 - a).abs());
                da.total_cmp(&db)
            })
            .map(|s| s.1)
            .unwrap_or(0.0)
    };
    let rmax = samples.iter().map(|s| s.1).fold(0.0, f64::max).max(1e-9);
    let angle = std::f64::consts::TAU;
    let n = segments_for(rmax, angle, tol);
    let rings: Vec<Vec<DVec3>> = (0..n)
        .map(|k| {
            let a = angle * k as f64 / n as f64;
            let q = DQuat::from_axis_angle(axis, a);
            let scale = radius_at(a) / rmax;
            profile
                .iter()
                .map(|p| {
                    let d = *p - axis_pt;
                    let radial = d - axis * d.dot(axis);
                    let along = axis * d.dot(axis);
                    axis_pt + q * (radial * scale + along)
                })
                .collect()
        })
        .collect();
    skin_stack(&rings, true)
}

/// Sweep a circular profile along a curve with the radius interpolated linearly
/// from `r0` at the start to `r1` at the end (a variable-radius pipe). Uses
/// parallel-transport frames like `sweep_profile`; capped, watertight.
pub fn pipe_curve(curve: &[DVec3], r0: f64, r1: f64, tol: f64) -> Mesh {
    assert!(curve.len() >= 2, "pipe curve needs 2+ points");
    assert!(r0 > 0.0 && r1 > 0.0, "pipe radii must be positive");
    let sides = segments_for(r0.max(r1), std::f64::consts::TAU, tol);
    let unit: Vec<DVec2> = (0..sides)
        .map(|i| {
            let a = std::f64::consts::TAU * i as f64 / sides as f64;
            DVec2::new(a.cos(), a.sin())
        })
        .collect();

    let m = curve.len();
    let tangent = |i: usize| -> DVec3 {
        let ahead = if i + 1 < m { curve[i + 1] - curve[i] } else { DVec3::ZERO };
        let behind = if i > 0 { curve[i] - curve[i - 1] } else { DVec3::ZERO };
        (ahead.normalize_or_zero() + behind.normalize_or_zero()).normalize_or_zero()
    };
    let t0 = tangent(0);
    let (mut u, mut v) = plane_basis(t0);
    let mut prev_t = t0;
    let rings: Vec<Vec<DVec3>> = (0..m)
        .map(|i| {
            let t = tangent(i);
            let q = DQuat::from_rotation_arc(prev_t, t);
            u = q * u;
            v = q * v;
            prev_t = t;
            let f = if m > 1 { i as f64 / (m - 1) as f64 } else { 0.0 };
            let r = r0 + (r1 - r0) * f;
            unit.iter().map(|c| curve[i] + u * (c.x * r) + v * (c.y * r)).collect()
        })
        .collect();
    skin_stack(&rings, false)
}

/// Sweep a closed 2D section boundary (in its local x/y plane, first point not
/// repeated) along the straight axis from `a` to `b` into a watertight prism.
/// The section's local +x is rotated by `orientation` radians about the axis
/// (member "roll"); its local +y then completes the right-handed frame. This is
/// the frame-member (beam/column) mesher: the result's volume equals the section
/// area times the axis length exactly (flat parallel end caps).
pub fn frame_member(section: &[DVec2], a: DVec3, b: DVec3, orientation: f64) -> Mesh {
    assert!(section.len() >= 3, "section needs at least 3 points");
    let axis = (b - a).normalize_or_zero();
    let axis = if axis == DVec3::ZERO { DVec3::Z } else { axis };
    // Base in-plane frame perpendicular to the axis, then rolled by orientation.
    let (u0, v0) = plane_basis(axis);
    let (c, s) = (orientation.cos(), orientation.sin());
    let u = u0 * c + v0 * s;
    let v = -u0 * s + v0 * c;
    let place = |base: DVec3| -> Vec<DVec3> {
        section.iter().map(|p| base + u * p.x + v * p.y).collect()
    };
    skin_stack(&[place(a), place(b)], false)
}

/// Extrude a closed 2D boundary (in the XY plane, world coords, first point not
/// repeated) along `dir` by `thickness` into a watertight slab/wall solid. When
/// `dir` is +Z this is a floor slab; an in-plane normal makes a wall. Volume
/// equals the boundary area times the thickness exactly.
pub fn area_member(boundary: &[DVec3], dir: DVec3, thickness: f64) -> Mesh {
    assert!(boundary.len() >= 3, "boundary needs at least 3 points");
    let d = dir.normalize_or_zero();
    let d = if d == DVec3::ZERO { DVec3::Z } else { d };
    let offset = d * thickness;
    let top: Vec<DVec3> = boundary.iter().map(|p| *p + offset).collect();
    skin_stack(&[boundary.to_vec(), top], false)
}

/// Resample an open polyline to `n` points spaced uniformly by arclength,
/// keeping the endpoints.
fn resample_open(pts: &[DVec3], n: usize) -> Vec<DVec3> {
    let m = pts.len();
    let mut cum = Vec::with_capacity(m);
    cum.push(0.0);
    for i in 1..m {
        cum.push(cum[i - 1] + pts[i - 1].distance(pts[i]));
    }
    let total = *cum.last().expect("non-empty");
    if total <= 0.0 {
        return vec![pts[0]; n];
    }
    (0..n)
        .map(|k| {
            let s = total * k as f64 / (n - 1).max(1) as f64;
            let i = (cum.partition_point(|&c| c <= s).min(m - 1)).max(1) - 1;
            let seg = cum[i + 1] - cum[i];
            let t = if seg > 0.0 { (s - cum[i]) / seg } else { 0.0 };
            pts[i].lerp(pts[i + 1], t)
        })
        .collect()
}

/// Connect a stack of same-count rings with quad strips. `wrap` joins the last
/// ring back to the first (full revolution); otherwise both ends are capped.
/// The result is coherently wound, degenerate-free, and oriented outward.
fn skin_stack(rings: &[Vec<DVec3>], wrap: bool) -> Mesh {
    let p = rings[0].len();
    let r = rings.len();
    let mut positions = Vec::with_capacity(p * r);
    for ring in rings {
        debug_assert_eq!(ring.len(), p, "rings must have equal point counts");
        positions.extend_from_slice(ring);
    }
    let mut faces = Vec::new();
    let bands = if wrap { r } else { r - 1 };
    for k in 0..bands {
        let (b, t) = ((k * p) as u32, (((k + 1) % r) * p) as u32);
        for i in 0..p as u32 {
            let j = (i + 1) % p as u32;
            faces.push([b + i, b + j, t + j]);
            faces.push([b + i, t + j, t + i]);
        }
    }
    if !wrap {
        // Side strips traverse the bottom ring in point order and the top ring
        // reversed; caps must use the opposite direction to stay coherent.
        faces.extend(cap_faces(&rings[0], 0, false));
        faces.extend(cap_faces(&rings[r - 1], ((r - 1) * p) as u32, true));
    }
    finish(positions, faces)
}

/// Triangulate a planar ring into cap faces based at `base`. `along` selects
/// whether the triangles traverse the ring boundary in point order (true) or
/// reversed (false), so the cap is edge-coherent with the adjacent side strip.
fn cap_faces(ring: &[DVec3], base: u32, along: bool) -> Vec<[u32; 3]> {
    let (u, v) = plane_basis(newell(ring).normalize_or_zero());
    let origin = ring[0];
    let proj: Vec<DVec2> = ring
        .iter()
        .map(|p| DVec2::new((*p - origin).dot(u), (*p - origin).dot(v)))
        .collect();
    // earcut boundary triangles follow point order iff the projection is CCW.
    let ccw = signed_area(&proj) >= 0.0;
    earcut(&proj)
        .into_iter()
        .map(|[a, b, c]| {
            if ccw == along {
                [base + a, base + b, base + c]
            } else {
                [base + a, base + c, base + b]
            }
        })
        .collect()
}

/// Drop degenerate triangles (profile points on a revolve axis collapse their
/// quads), then flip the whole mesh if its signed volume is negative so
/// normals point outward — the invariant CSG expects.
fn finish(positions: Vec<DVec3>, mut faces: Vec<[u32; 3]>) -> Mesh {
    faces.retain(|f| {
        let [a, b, c] = f.map(|i| positions[i as usize]);
        (b - a).cross(c - a).length_squared() > 1e-24
    });
    let mesh = Mesh::new(positions, faces);
    if signed_volume(&mesh) < 0.0 {
        let flipped = mesh.faces().iter().map(|&[a, b, c]| [a, c, b]).collect();
        return Mesh::new(mesh.positions().to_vec(), flipped);
    }
    mesh
}

/// Resample a closed loop (first point not repeated) to `n` points spaced
/// uniformly by arclength, keeping the start point and direction.
fn resample_closed(pts: &[DVec3], n: usize) -> Vec<DVec3> {
    let m = pts.len();
    let mut cum = Vec::with_capacity(m + 1);
    cum.push(0.0);
    for i in 0..m {
        cum.push(cum[i] + pts[i].distance(pts[(i + 1) % m]));
    }
    let total = *cum.last().expect("non-empty");
    if total <= 0.0 {
        return vec![pts[0]; n];
    }
    (0..n)
        .map(|k| {
            let s = total * k as f64 / n as f64;
            let i = (cum.partition_point(|&c| c <= s) - 1).min(m - 1);
            let seg = cum[i + 1] - cum[i];
            let t = if seg > 0.0 { (s - cum[i]) / seg } else { 0.0 };
            pts[i].lerp(pts[(i + 1) % m], t)
        })
        .collect()
}

/// Polygon normal by Newell's method (robust for non-convex loops); not
/// normalized — callers compare directions or normalize as needed.
fn newell(pts: &[DVec3]) -> DVec3 {
    let mut n = DVec3::ZERO;
    for (i, p) in pts.iter().enumerate() {
        let q = pts[(i + 1) % pts.len()];
        n += DVec3::new(
            (p.y - q.y) * (p.z + q.z),
            (p.z - q.z) * (p.x + q.x),
            (p.x - q.x) * (p.y + q.y),
        );
    }
    n
}

/// Orthonormal in-plane basis (u, v) with u x v = n.
fn plane_basis(n: DVec3) -> (DVec3, DVec3) {
    let pick = if n.x.abs() < 0.9 { DVec3::X } else { DVec3::Y };
    let u = (pick - n * pick.dot(n)).normalize_or_zero();
    (u, n.cross(u))
}

/// Angular segment count for max chord deviation `tol` at `radius`.
fn segments_for(radius: f64, sweep: f64, tol: f64) -> usize {
    if radius <= tol {
        return 8;
    }
    // Chord error e = r(1 - cos(dt/2))  =>  dt = 2 acos(1 - e/r)
    let dt = 2.0 * (1.0 - tol / radius).clamp(-1.0, 1.0).acos();
    ((sweep / dt.max(1e-4)).ceil() as usize).clamp(8, 512)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::csg::weld;

    /// Closed and coherently oriented after welding coincident vertices: every
    /// undirected edge is used equally often in both directions.
    fn watertight(mesh: &Mesh) -> bool {
        let welded = weld(mesh, 1e-9);
        let mut balance: std::collections::HashMap<(u32, u32), i64> =
            std::collections::HashMap::new();
        for f in welded.faces() {
            if f[0] == f[1] || f[1] == f[2] || f[0] == f[2] {
                continue; // collapsed by welding axis-coincident vertices
            }
            for (a, b) in [(f[0], f[1]), (f[1], f[2]), (f[2], f[0])] {
                *balance.entry((a.min(b), a.max(b))).or_default() +=
                    if a < b { 1 } else { -1 };
            }
        }
        !balance.is_empty() && balance.values().all(|&v| v == 0)
    }

    fn rect_xz(x0: f64, x1: f64, z0: f64, z1: f64) -> Vec<DVec3> {
        vec![
            DVec3::new(x0, 0.0, z0),
            DVec3::new(x1, 0.0, z0),
            DVec3::new(x1, 0.0, z1),
            DVec3::new(x0, 0.0, z1),
        ]
    }

    fn circle_xy(r: f64, z: f64, n: usize) -> Vec<DVec3> {
        (0..n)
            .map(|i| {
                let t = std::f64::consts::TAU * i as f64 / n as f64;
                DVec3::new(r * t.cos(), r * t.sin(), z)
            })
            .collect()
    }

    #[test]
    fn revolve_rect_full_circle_is_cylinder() {
        // r=1 h=2 profile touching the axis: V = pi r^2 h = 2 pi.
        let m = revolve_profile(
            &rect_xz(0.0, 1.0, 0.0, 2.0),
            DVec3::ZERO,
            DVec3::Z,
            std::f64::consts::TAU,
            0.001,
        );
        assert!((signed_volume(&m) - 2.0 * std::f64::consts::PI).abs() < 0.02);
        assert!(watertight(&m));
    }

    #[test]
    fn revolve_trapezoid_is_frustum() {
        // r0=2 -> r1=1 over h=3: V = pi h/3 (r0^2 + r0 r1 + r1^2) = 7 pi.
        let profile = vec![
            DVec3::new(0.0, 0.0, 0.0),
            DVec3::new(2.0, 0.0, 0.0),
            DVec3::new(1.0, 0.0, 3.0),
            DVec3::new(0.0, 0.0, 3.0),
        ];
        let m = revolve_profile(
            &profile,
            DVec3::ZERO,
            DVec3::Z,
            std::f64::consts::TAU,
            0.001,
        );
        assert!((signed_volume(&m) - 7.0 * std::f64::consts::PI).abs() < 0.1);
        assert!(watertight(&m));
    }

    #[test]
    fn revolve_partial_is_capped_and_watertight() {
        // Unit square at radius 1..2, 90 degrees. Pappus: V = A * theta * rc
        // = 1 * (pi/2) * 1.5.
        let m = revolve_profile(
            &rect_xz(1.0, 2.0, 0.0, 1.0),
            DVec3::ZERO,
            DVec3::Z,
            std::f64::consts::FRAC_PI_2,
            0.001,
        );
        let want = std::f64::consts::FRAC_PI_2 * 1.5;
        assert!((signed_volume(&m) - want).abs() < 0.01);
        assert!(watertight(&m));
    }

    #[test]
    fn revolve_off_z_axis() {
        // Same cylinder about the x axis through (0,0,5).
        let profile = vec![
            DVec3::new(0.0, 0.0, 5.0),
            DVec3::new(0.0, 1.0, 5.0),
            DVec3::new(2.0, 1.0, 5.0),
            DVec3::new(2.0, 0.0, 5.0),
        ];
        let m = revolve_profile(
            &profile,
            DVec3::new(0.0, 0.0, 5.0),
            DVec3::X,
            std::f64::consts::TAU,
            0.001,
        );
        assert!((signed_volume(&m) - 2.0 * std::f64::consts::PI).abs() < 0.02);
        assert!(watertight(&m));
    }

    #[test]
    fn loft_squares_is_prism() {
        // Two 2x2 squares 3 apart: exact box volume 12 (corner resampling).
        let sq = |z: f64| {
            vec![
                DVec3::new(0.0, 0.0, z),
                DVec3::new(2.0, 0.0, z),
                DVec3::new(2.0, 2.0, z),
                DVec3::new(0.0, 2.0, z),
            ]
        };
        let m = loft_profiles(&[sq(0.0), sq(3.0)]);
        assert!((signed_volume(&m) - 12.0).abs() < 1e-9);
        assert!(watertight(&m));
    }

    #[test]
    fn loft_circles_is_frustum() {
        // 64-gon rings r=2 -> r=1 over h=3; compare against the exact n-gon
        // frustum (ratio (n/2) sin(tau/n) / pi of the round 7 pi).
        let n = 64usize;
        let m = loft_profiles(&[circle_xy(2.0, 0.0, n), circle_xy(1.0, 3.0, n)]);
        let ngon = (n as f64 / 2.0) * (std::f64::consts::TAU / n as f64).sin() / std::f64::consts::PI;
        let want = 7.0 * std::f64::consts::PI * ngon;
        assert!((signed_volume(&m) - want).abs() < 0.05);
        assert!(watertight(&m));
    }

    #[test]
    fn loft_aligns_winding_and_point_counts() {
        // Second ring reversed (CW) and denser: loft still closes with the
        // same positive volume.
        let mut top: Vec<DVec3> = circle_xy(1.0, 2.0, 96);
        top.reverse();
        let m = loft_profiles(&[circle_xy(1.0, 0.0, 32), top]);
        let v = signed_volume(&m);
        assert!(v > 0.0 && (v - 2.0 * std::f64::consts::PI).abs() < 0.15, "{v}");
        assert!(watertight(&m));
    }

    #[test]
    fn sweep_square_along_straight_rail_is_box() {
        // Unit square centered at origin swept 4 up: exact volume 4.
        let profile = vec![
            DVec3::new(-0.5, -0.5, 0.0),
            DVec3::new(0.5, -0.5, 0.0),
            DVec3::new(0.5, 0.5, 0.0),
            DVec3::new(-0.5, 0.5, 0.0),
        ];
        let rail = vec![DVec3::ZERO, DVec3::new(0.0, 0.0, 4.0)];
        let m = sweep_profile(&profile, &rail);
        assert!((signed_volume(&m) - 4.0).abs() < 1e-9);
        assert!(watertight(&m));
    }

    #[test]
    fn sweep_along_elbow_rail_stays_closed_at_the_corner() {
        // Unit square along an L (3 up, 3 out). Parallel transport keeps the
        // corner ring rigid (tilted 45 deg, not miter-stretched); integrating
        // the two ruled bands gives exactly V = 3 (1 + sqrt(2)/2).
        let profile = vec![
            DVec3::new(-0.5, -0.5, 0.0),
            DVec3::new(0.5, -0.5, 0.0),
            DVec3::new(0.5, 0.5, 0.0),
            DVec3::new(-0.5, 0.5, 0.0),
        ];
        let rail = vec![
            DVec3::ZERO,
            DVec3::new(0.0, 0.0, 3.0),
            DVec3::new(3.0, 0.0, 3.0),
        ];
        let m = sweep_profile(&profile, &rail);
        let want = 3.0 * (1.0 + std::f64::consts::FRAC_1_SQRT_2);
        assert!((signed_volume(&m) - want).abs() < 1e-9);
        assert!(watertight(&m));
    }

    #[test]
    fn pipe_along_straight_line_is_cylinder() {
        // Circular pipe r=1 along a 5-long line: V = pi r^2 h = 5 pi.
        let curve = vec![DVec3::ZERO, DVec3::new(0.0, 0.0, 5.0)];
        let m = pipe_curve(&curve, 1.0, 1.0, 0.001);
        // n-gon cross section under-fills the true circle by (n/2)sin(tau/n)/pi.
        let sides = segments_for(1.0, std::f64::consts::TAU, 0.001) as f64;
        let ngon = (sides / 2.0) * (std::f64::consts::TAU / sides).sin() / std::f64::consts::PI;
        let want = 5.0 * std::f64::consts::PI * ngon;
        assert!((signed_volume(&m) - want).abs() < 0.01, "{}", signed_volume(&m));
        assert!(watertight(&m));
    }

    #[test]
    fn pipe_variable_radius_is_a_cone_frustum() {
        // r0=2 -> r1=1 over h=3: V = pi h/3 (r0^2 + r0 r1 + r1^2) = 7 pi (n-gon).
        let curve = vec![DVec3::ZERO, DVec3::new(0.0, 0.0, 3.0)];
        let m = pipe_curve(&curve, 2.0, 1.0, 0.001);
        let sides = segments_for(2.0, std::f64::consts::TAU, 0.001) as f64;
        let ngon = (sides / 2.0) * (std::f64::consts::TAU / sides).sin() / std::f64::consts::PI;
        let want = 7.0 * std::f64::consts::PI * ngon;
        assert!((signed_volume(&m) - want).abs() < 0.05, "{}", signed_volume(&m));
        assert!(watertight(&m));
    }

    #[test]
    fn sweep2_between_parallel_straight_rails_is_prism() {
        // Unit-square profile (width 1) between two parallel rails 2 apart,
        // running 4 in z. Profile width scales to the gap (2), height stays 1,
        // so the swept solid is a 2 x 1 x 4 prism: V = 8.
        let profile = vec![
            DVec3::new(-0.5, -0.5, 0.0),
            DVec3::new(0.5, -0.5, 0.0),
            DVec3::new(0.5, 0.5, 0.0),
            DVec3::new(-0.5, 0.5, 0.0),
        ];
        let rail_a = vec![DVec3::new(-1.0, 0.0, 0.0), DVec3::new(-1.0, 0.0, 4.0)];
        let rail_b = vec![DVec3::new(1.0, 0.0, 0.0), DVec3::new(1.0, 0.0, 4.0)];
        let m = sweep2_profile(&profile, &rail_a, &rail_b);
        assert!((signed_volume(&m) - 8.0).abs() < 1e-9, "{}", signed_volume(&m));
        assert!(watertight(&m));
    }

    #[test]
    fn rail_revolve_constant_rail_matches_revolve() {
        // A rail at constant radius reduces rail-revolve to a plain full revolve.
        let profile = rect_xz(2.0, 3.0, 0.0, 1.0);
        let rail = circle_xy(2.5, 0.0, 24); // constant radius 2.5 about z
        let m = rail_revolve_profile(&profile, &rail, DVec3::ZERO, DVec3::Z, 0.01);
        let plain = revolve_profile(
            &profile,
            DVec3::ZERO,
            DVec3::Z,
            std::f64::consts::TAU,
            0.01,
        );
        // Same scale factor everywhere (rail radius / rmax = 1), so volumes match.
        assert!(
            (signed_volume(&m) - signed_volume(&plain)).abs() < 0.05,
            "{} vs {}",
            signed_volume(&m),
            signed_volume(&plain)
        );
        assert!(watertight(&m));
    }

    #[test]
    fn frame_member_rect_section_is_prism() {
        // 0.4 x 0.6 rectangle swept 5 along +z: V = 0.4*0.6*5 = 1.2.
        let section = vec![
            DVec2::new(-0.2, -0.3),
            DVec2::new(0.2, -0.3),
            DVec2::new(0.2, 0.3),
            DVec2::new(-0.2, 0.3),
        ];
        let m = frame_member(&section, DVec3::ZERO, DVec3::new(0.0, 0.0, 5.0), 0.0);
        assert!((signed_volume(&m) - 1.2).abs() < 1e-9, "{}", signed_volume(&m));
        assert!(watertight(&m));
    }

    #[test]
    fn frame_member_orientation_preserves_volume() {
        let section = vec![
            DVec2::new(-0.2, -0.3),
            DVec2::new(0.2, -0.3),
            DVec2::new(0.2, 0.3),
            DVec2::new(-0.2, 0.3),
        ];
        // A horizontal beam rolled 30 degrees: volume is invariant to roll.
        let m = frame_member(
            &section,
            DVec3::ZERO,
            DVec3::new(4.0, 0.0, 0.0),
            30f64.to_radians(),
        );
        assert!((signed_volume(&m) - 0.4 * 0.6 * 4.0).abs() < 1e-9);
        assert!(watertight(&m));
    }

    #[test]
    fn area_member_slab_is_extruded_boundary() {
        // 6 x 4 slab, thickness 0.2 upward: V = 6*4*0.2 = 4.8.
        let boundary = vec![
            DVec3::new(0.0, 0.0, 0.0),
            DVec3::new(6.0, 0.0, 0.0),
            DVec3::new(6.0, 4.0, 0.0),
            DVec3::new(0.0, 4.0, 0.0),
        ];
        let m = area_member(&boundary, DVec3::Z, 0.2);
        assert!((signed_volume(&m) - 4.8).abs() < 1e-9, "{}", signed_volume(&m));
        assert!(watertight(&m));
    }

    #[test]
    fn revolved_solid_feeds_csg() {
        // Watertightness in practice: subtract a box from a revolved cylinder.
        let cyl = revolve_profile(
            &rect_xz(0.0, 1.0, 0.0, 2.0),
            DVec3::ZERO,
            DVec3::Z,
            std::f64::consts::TAU,
            0.001,
        );
        let cut = crate::make_box(DVec3::new(-2.0, -2.0, 1.0), DVec3::new(4.0, 4.0, 2.0));
        let half = crate::csg_difference(&cyl, &cut);
        assert!((signed_volume(&half) - std::f64::consts::PI).abs() < 0.02);
        assert!(!half.faces().is_empty());
    }
}
