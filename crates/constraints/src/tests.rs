// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

use super::*;

const TOL: f64 = 1e-6;

fn assert_near(a: f64, b: f64, msg: &str) {
    assert!((a - b).abs() < TOL, "{msg}: {a} vs {b} (Δ {})", (a - b).abs());
}

fn dist(a: (f64, f64), b: (f64, f64)) -> f64 {
    ((b.0 - a.0).powi(2) + (b.1 - a.1).powi(2)).sqrt()
}

// ───────────────────────────── single constraints ────────────────────────────

#[test]
fn coincident_pulls_points_together() {
    let mut sk = Sketch::new();
    let a = sk.add_point(0.0, 0.0);
    let b = sk.add_point(3.0, 4.0);
    sk.add_constraint(Constraint::Coincident(a, b));
    let res = sk.solve();
    assert!(res.converged(), "{res:?}");
    assert!(dist(sk.point(a), sk.point(b)) < TOL);
}

#[test]
fn horizontal_flattens_line() {
    let mut sk = Sketch::new();
    let a = sk.add_point(0.0, 0.0);
    let b = sk.add_point(5.0, 2.0);
    let l = sk.add_line(a, b);
    sk.add_constraint(Constraint::Horizontal(l));
    let res = sk.solve();
    assert!(res.converged());
    assert_near(sk.point(a).1, sk.point(b).1, "y equal");
}

#[test]
fn vertical_uprights_line() {
    let mut sk = Sketch::new();
    let a = sk.add_point(0.0, 0.0);
    let b = sk.add_point(2.0, 5.0);
    let l = sk.add_line(a, b);
    sk.add_constraint(Constraint::Vertical(l));
    let res = sk.solve();
    assert!(res.converged());
    assert_near(sk.point(a).0, sk.point(b).0, "x equal");
}

#[test]
fn distance_point_point() {
    let mut sk = Sketch::new();
    let a = sk.add_point(0.0, 0.0);
    let b = sk.add_point(1.0, 1.0);
    sk.add_constraint(Constraint::Distance(a, b, 10.0));
    let res = sk.solve();
    assert!(res.converged());
    assert_near(dist(sk.point(a), sk.point(b)), 10.0, "distance");
}

#[test]
fn distance_point_line() {
    let mut sk = Sketch::new();
    let a = sk.add_point(0.0, 0.0);
    let b = sk.add_point(10.0, 0.0);
    let l = sk.add_line(a, b);
    let p = sk.add_point(5.0, 1.0);
    sk.add_constraint(Constraint::Fixed(a, 0.0, 0.0));
    sk.add_constraint(Constraint::Fixed(b, 10.0, 0.0));
    sk.add_constraint(Constraint::DistancePointLine(p, l, 3.0));
    let res = sk.solve();
    assert!(res.converged());
    assert_near(sk.point(p).1, 3.0, "stays on initial (positive) side");
}

#[test]
fn distance_point_line_negative_side_latched() {
    let mut sk = Sketch::new();
    let a = sk.add_point(0.0, 0.0);
    let b = sk.add_point(10.0, 0.0);
    let l = sk.add_line(a, b);
    let p = sk.add_point(5.0, -1.0);
    sk.add_constraint(Constraint::Fixed(a, 0.0, 0.0));
    sk.add_constraint(Constraint::Fixed(b, 10.0, 0.0));
    sk.add_constraint(Constraint::DistancePointLine(p, l, 3.0));
    let res = sk.solve();
    assert!(res.converged());
    assert_near(sk.point(p).1, -3.0, "stays on initial (negative) side");
}

#[test]
fn length_sets_segment_length() {
    let mut sk = Sketch::new();
    let a = sk.add_point(0.0, 0.0);
    let b = sk.add_point(1.0, 2.0);
    let l = sk.add_line(a, b);
    sk.add_constraint(Constraint::Length(l, 7.5));
    let res = sk.solve();
    assert!(res.converged());
    assert_near(dist(sk.point(a), sk.point(b)), 7.5, "length");
}

#[test]
fn angle_between_lines() {
    let mut sk = Sketch::new();
    let o = sk.add_point(0.0, 0.0);
    let a = sk.add_point(10.0, 0.0);
    let b = sk.add_point(10.0, 1.0);
    let l1 = sk.add_line(o, a);
    let l2 = sk.add_line(o, b);
    sk.add_constraint(Constraint::Fixed(o, 0.0, 0.0));
    sk.add_constraint(Constraint::Fixed(a, 10.0, 0.0));
    sk.add_constraint(Constraint::Angle(l1, l2, 45.0));
    let res = sk.solve();
    assert!(res.converged(), "{res:?}");
    let (bx, by) = sk.point(b);
    assert_near(by.atan2(bx).to_degrees(), 45.0, "45 degrees");
}

#[test]
fn parallel_lines() {
    let mut sk = Sketch::new();
    let a = sk.add_point(0.0, 0.0);
    let b = sk.add_point(10.0, 0.0);
    let c = sk.add_point(0.0, 5.0);
    let d = sk.add_point(9.0, 8.0);
    let l1 = sk.add_line(a, b);
    let l2 = sk.add_line(c, d);
    sk.add_constraint(Constraint::Fixed(a, 0.0, 0.0));
    sk.add_constraint(Constraint::Fixed(b, 10.0, 0.0));
    sk.add_constraint(Constraint::Parallel(l1, l2));
    let res = sk.solve();
    assert!(res.converged());
    let ((cx, cy), (dx2, dy2)) = sk.line_points(l2);
    assert_near(dy2 - cy, 0.0, "parallel to horizontal → Δy 0");
    assert!((dx2 - cx).abs() > 1.0, "did not degenerate to a point");
}

#[test]
fn perpendicular_lines() {
    let mut sk = Sketch::new();
    let a = sk.add_point(0.0, 0.0);
    let b = sk.add_point(10.0, 0.0);
    let c = sk.add_point(5.0, 0.0);
    let d = sk.add_point(8.0, 6.0);
    let l1 = sk.add_line(a, b);
    let l2 = sk.add_line(c, d);
    sk.add_constraint(Constraint::Fixed(a, 0.0, 0.0));
    sk.add_constraint(Constraint::Fixed(b, 10.0, 0.0));
    sk.add_constraint(Constraint::Perpendicular(l1, l2));
    let res = sk.solve();
    assert!(res.converged());
    let ((cx, cy), (dx2, dy2)) = sk.line_points(l2);
    let ((ax, ay), (bx, by)) = sk.line_points(l1);
    let dd = (bx - ax) * (dx2 - cx) + (by - ay) * (dy2 - cy);
    assert!(dd.abs() < 1e-4, "dot ~ 0, got {dd}");
}

#[test]
fn equal_length() {
    let mut sk = Sketch::new();
    let a = sk.add_point(0.0, 0.0);
    let b = sk.add_point(4.0, 0.0);
    let c = sk.add_point(0.0, 2.0);
    let d = sk.add_point(10.0, 2.0);
    let l1 = sk.add_line(a, b);
    let l2 = sk.add_line(c, d);
    sk.add_constraint(Constraint::Fixed(a, 0.0, 0.0));
    sk.add_constraint(Constraint::Fixed(b, 4.0, 0.0));
    sk.add_constraint(Constraint::EqualLength(l1, l2));
    let res = sk.solve();
    assert!(res.converged());
    assert_near(dist(sk.point(c), sk.point(d)), 4.0, "second line matches first");
}

#[test]
fn equal_radius_and_radius() {
    let mut sk = Sketch::new();
    let c1 = sk.add_point(0.0, 0.0);
    let c2 = sk.add_point(10.0, 0.0);
    let a = sk.add_circle(c1, 2.0);
    let b = sk.add_circle(c2, 5.0);
    sk.add_constraint(Constraint::Radius(a, 3.0));
    sk.add_constraint(Constraint::EqualRadius(a, b));
    let res = sk.solve();
    assert!(res.converged());
    assert_near(sk.radius(a), 3.0, "radius a");
    assert_near(sk.radius(b), 3.0, "radius b");
}

#[test]
fn fixed_anchors_point() {
    let mut sk = Sketch::new();
    let a = sk.add_point(1.0, 1.0);
    let b = sk.add_point(4.0, 5.0);
    sk.add_constraint(Constraint::Fixed(a, 1.0, 1.0));
    sk.add_constraint(Constraint::Distance(a, b, 2.0));
    let res = sk.solve();
    assert!(res.converged());
    assert_near(sk.point(a).0, 1.0, "anchor x");
    assert_near(sk.point(a).1, 1.0, "anchor y");
    assert_near(dist(sk.point(a), sk.point(b)), 2.0, "distance");
}

#[test]
fn point_on_line() {
    let mut sk = Sketch::new();
    let a = sk.add_point(0.0, 0.0);
    let b = sk.add_point(10.0, 10.0);
    let l = sk.add_line(a, b);
    let p = sk.add_point(3.0, 7.0);
    sk.add_constraint(Constraint::Fixed(a, 0.0, 0.0));
    sk.add_constraint(Constraint::Fixed(b, 10.0, 10.0));
    sk.add_constraint(Constraint::PointOnLine(p, l));
    let res = sk.solve();
    assert!(res.converged());
    let (px, py) = sk.point(p);
    assert_near(px, py, "on the diagonal");
}

#[test]
fn point_on_circle() {
    let mut sk = Sketch::new();
    let c = sk.add_point(0.0, 0.0);
    let circ = sk.add_circle(c, 5.0);
    let p = sk.add_point(2.0, 1.0);
    sk.add_constraint(Constraint::Fixed(c, 0.0, 0.0));
    sk.add_constraint(Constraint::Radius(circ, 5.0));
    sk.add_constraint(Constraint::PointOnCircle(p, circ));
    let res = sk.solve();
    assert!(res.converged());
    assert_near(dist(sk.point(p), (0.0, 0.0)), 5.0, "on circle");
}

#[test]
fn tangent_line_circle() {
    let mut sk = Sketch::new();
    let a = sk.add_point(0.0, 0.0);
    let b = sk.add_point(10.0, 0.0);
    let l = sk.add_line(a, b);
    let c = sk.add_point(5.0, 2.0);
    let circ = sk.add_circle(c, 3.0);
    sk.add_constraint(Constraint::Fixed(a, 0.0, 0.0));
    sk.add_constraint(Constraint::Fixed(b, 10.0, 0.0));
    sk.add_constraint(Constraint::Radius(circ, 3.0));
    sk.add_constraint(Constraint::Tangent(l, circ));
    let res = sk.solve();
    assert!(res.converged());
    assert_near(sk.point(c).1, 3.0, "center sits one radius above, same side");
}

#[test]
fn tangent_circles_external() {
    let mut sk = Sketch::new();
    let p1 = sk.add_point(0.0, 0.0);
    let p2 = sk.add_point(10.0, 0.0);
    let a = sk.add_circle(p1, 2.0);
    let b = sk.add_circle(p2, 3.0);
    sk.add_constraint(Constraint::Fixed(p1, 0.0, 0.0));
    sk.add_constraint(Constraint::Radius(a, 2.0));
    sk.add_constraint(Constraint::Radius(b, 3.0));
    sk.add_constraint(Constraint::TangentCircles(a, b));
    let res = sk.solve();
    assert!(res.converged());
    assert_near(dist(sk.point(p1), sk.point(p2)), 5.0, "external tangency");
}

#[test]
fn tangent_circles_internal() {
    let mut sk = Sketch::new();
    let p1 = sk.add_point(0.0, 0.0);
    let p2 = sk.add_point(1.5, 0.0); // gap 1.5 ≈ |5-3|=2 closer than 8 → internal
    let a = sk.add_circle(p1, 5.0);
    let b = sk.add_circle(p2, 3.0);
    sk.add_constraint(Constraint::Fixed(p1, 0.0, 0.0));
    sk.add_constraint(Constraint::Radius(a, 5.0));
    sk.add_constraint(Constraint::Radius(b, 3.0));
    sk.add_constraint(Constraint::TangentCircles(a, b));
    let res = sk.solve();
    assert!(res.converged());
    assert_near(dist(sk.point(p1), sk.point(p2)), 2.0, "internal tangency |r1-r2|");
}

#[test]
fn symmetric_about_line() {
    let mut sk = Sketch::new();
    let a = sk.add_point(0.0, -5.0);
    let b = sk.add_point(0.0, 5.0);
    let axis = sk.add_line(a, b); // y axis
    let p = sk.add_point(-3.0, 2.0);
    let q = sk.add_point(4.0, 2.5);
    sk.add_constraint(Constraint::Fixed(a, 0.0, -5.0));
    sk.add_constraint(Constraint::Fixed(b, 0.0, 5.0));
    sk.add_constraint(Constraint::Fixed(p, -3.0, 2.0));
    sk.add_constraint(Constraint::Symmetric(p, q, axis));
    let res = sk.solve();
    assert!(res.converged());
    let (qx, qy) = sk.point(q);
    assert_near(qx, 3.0, "mirrored x");
    assert_near(qy, 2.0, "same y");
}

#[test]
fn midpoint_of_line() {
    let mut sk = Sketch::new();
    let a = sk.add_point(0.0, 0.0);
    let b = sk.add_point(10.0, 4.0);
    let l = sk.add_line(a, b);
    let p = sk.add_point(1.0, 1.0);
    sk.add_constraint(Constraint::Fixed(a, 0.0, 0.0));
    sk.add_constraint(Constraint::Fixed(b, 10.0, 4.0));
    sk.add_constraint(Constraint::Midpoint(p, l));
    let res = sk.solve();
    assert!(res.converged());
    let (px, py) = sk.point(p);
    assert_near(px, 5.0, "mid x");
    assert_near(py, 2.0, "mid y");
}

// ───────────────────────────── combined systems ──────────────────────────────

/// Four separate lines constrained into a 4×3 axis-aligned rectangle anchored
/// at the origin: coincident corners, horizontal/vertical, two dimensions.
#[test]
fn rectangle_solves_fully() {
    let mut sk = Sketch::new();
    // Sloppy near-rectangle start.
    let p1 = sk.add_point(0.2, -0.1);
    let p2 = sk.add_point(3.7, 0.3);
    let p3 = sk.add_point(4.1, 2.8);
    let p4 = sk.add_point(-0.3, 3.2);
    let bottom = sk.add_line(p1, p2);
    let right = sk.add_line(p2, p3);
    let top = sk.add_line(p3, p4);
    let left = sk.add_line(p4, p1);
    sk.add_constraint(Constraint::Fixed(p1, 0.0, 0.0));
    sk.add_constraint(Constraint::Horizontal(bottom));
    sk.add_constraint(Constraint::Horizontal(top));
    sk.add_constraint(Constraint::Vertical(left));
    sk.add_constraint(Constraint::Vertical(right));
    sk.add_constraint(Constraint::Length(bottom, 4.0));
    sk.add_constraint(Constraint::Length(right, 3.0));
    let res = sk.solve();
    assert!(res.converged(), "{res:?}");
    assert_eq!(res.dof, 0, "fully constrained");
    assert!(res.redundant.is_empty());
    assert_near(sk.point(p2).0, 4.0, "p2.x");
    assert_near(sk.point(p2).1, 0.0, "p2.y");
    assert_near(sk.point(p3).0, 4.0, "p3.x");
    assert_near(sk.point(p3).1, 3.0, "p3.y");
    assert_near(sk.point(p4).0, 0.0, "p4.x");
    assert_near(sk.point(p4).1, 3.0, "p4.y");
}

/// Same rectangle via perpendicular + equal + parallel instead of H/V.
#[test]
fn rectangle_via_perpendicular_and_equal() {
    let mut sk = Sketch::new();
    let p1 = sk.add_point(0.0, 0.0);
    let p2 = sk.add_point(3.9, 0.2);
    let p3 = sk.add_point(4.2, 3.1);
    let p4 = sk.add_point(0.1, 2.9);
    let bottom = sk.add_line(p1, p2);
    let right = sk.add_line(p2, p3);
    let top = sk.add_line(p4, p3);
    let left = sk.add_line(p1, p4);
    sk.add_constraint(Constraint::Fixed(p1, 0.0, 0.0));
    sk.add_constraint(Constraint::Horizontal(bottom));
    sk.add_constraint(Constraint::Perpendicular(bottom, right));
    sk.add_constraint(Constraint::Parallel(bottom, top));
    sk.add_constraint(Constraint::Parallel(right, left));
    sk.add_constraint(Constraint::EqualLength(top, bottom));
    sk.add_constraint(Constraint::EqualLength(left, right));
    sk.add_constraint(Constraint::Length(bottom, 4.0));
    sk.add_constraint(Constraint::Length(right, 3.0));
    let res = sk.solve();
    assert!(res.converged(), "{res:?}");
    assert_near(dist(sk.point(p1), sk.point(p2)), 4.0, "bottom");
    assert_near(dist(sk.point(p2), sk.point(p3)), 3.0, "right");
    assert_near(dist(sk.point(p4), sk.point(p3)), 4.0, "top");
    assert_near(dist(sk.point(p1), sk.point(p4)), 3.0, "left");
    // right angle at p2
    let (ax, ay) = sk.point(p1);
    let (bx, by) = sk.point(p2);
    let (cx, cy) = sk.point(p3);
    let dd = (bx - ax) * (cx - bx) + (by - ay) * (cy - by);
    assert!(dd.abs() < 1e-4, "corner square, dot {dd}");
}

/// Triangle with two side lengths + included angle: classic dimensioned solve.
#[test]
fn dimensioned_triangle() {
    let mut sk = Sketch::new();
    let a = sk.add_point(0.0, 0.0);
    let b = sk.add_point(5.5, 0.5);
    let c = sk.add_point(2.0, 3.5);
    let ab = sk.add_line(a, b);
    let ac = sk.add_line(a, c);
    sk.add_constraint(Constraint::Fixed(a, 0.0, 0.0));
    sk.add_constraint(Constraint::Horizontal(ab));
    sk.add_constraint(Constraint::Length(ab, 6.0));
    sk.add_constraint(Constraint::Length(ac, 4.0));
    sk.add_constraint(Constraint::Angle(ab, ac, 60.0));
    let res = sk.solve();
    assert!(res.converged(), "{res:?}");
    assert_near(dist(sk.point(a), sk.point(b)), 6.0, "ab");
    assert_near(dist(sk.point(a), sk.point(c)), 4.0, "ac");
    // law of cosines for bc
    let expect = (36.0f64 + 16.0 - 2.0 * 6.0 * 4.0 * 60f64.to_radians().cos()).sqrt();
    assert_near(dist(sk.point(b), sk.point(c)), expect, "bc via law of cosines");
}

/// Slider-crank-like chain: anchored pivot, fixed-length link, endpoint on a
/// horizontal guide line.
#[test]
fn link_endpoint_on_guide() {
    let mut sk = Sketch::new();
    let pivot = sk.add_point(0.0, 0.0);
    let end = sk.add_point(2.0, 2.0);
    let g1 = sk.add_point(-10.0, 1.0);
    let g2 = sk.add_point(10.0, 1.0);
    let guide = sk.add_line(g1, g2);
    sk.add_constraint(Constraint::Fixed(pivot, 0.0, 0.0));
    sk.add_constraint(Constraint::Fixed(g1, -10.0, 1.0));
    sk.add_constraint(Constraint::Fixed(g2, 10.0, 1.0));
    sk.add_constraint(Constraint::Distance(pivot, end, 3.0));
    sk.add_constraint(Constraint::PointOnLine(end, guide));
    let res = sk.solve();
    assert!(res.converged());
    let (ex, ey) = sk.point(end);
    assert_near(ey, 1.0, "on guide");
    assert_near(ex, (9.0f64 - 1.0).sqrt(), "crank reach");
    assert_eq!(res.dof, 0);
}

// ───────────────────────────── diagnostics ───────────────────────────────────

#[test]
fn under_constrained_reports_dof() {
    let mut sk = Sketch::new();
    let a = sk.add_point(0.0, 0.0);
    let b = sk.add_point(3.0, 0.0);
    sk.add_constraint(Constraint::Distance(a, b, 5.0));
    let res = sk.solve();
    assert!(res.converged());
    // 4 params − 1 equation = 3 DOF (translate ×2 + rotate).
    assert_eq!(res.dof, 3);
}

#[test]
fn unconstrained_sketch_is_all_dof() {
    let mut sk = Sketch::new();
    sk.add_point(0.0, 0.0);
    sk.add_point(1.0, 1.0);
    let res = sk.solve();
    assert!(res.converged());
    assert_eq!(res.dof, 4);
}

#[test]
fn redundant_consistent_detected() {
    let mut sk = Sketch::new();
    let a = sk.add_point(0.1, 0.0);
    let b = sk.add_point(5.0, 0.4);
    let l = sk.add_line(a, b);
    sk.add_constraint(Constraint::Fixed(a, 0.0, 0.0));
    sk.add_constraint(Constraint::Horizontal(l));
    sk.add_constraint(Constraint::Horizontal(l)); // exact duplicate
    let res = sk.solve();
    assert!(res.converged());
    assert_eq!(res.redundant, vec![2], "second horizontal is redundant");
}

#[test]
fn conflicting_constraints_report_inconsistent() {
    let mut sk = Sketch::new();
    let a = sk.add_point(0.0, 0.0);
    let b = sk.add_point(4.0, 0.0);
    sk.add_constraint(Constraint::Fixed(a, 0.0, 0.0));
    sk.add_constraint(Constraint::Fixed(b, 4.0, 0.0));
    sk.add_constraint(Constraint::Distance(a, b, 9.0)); // impossible
    let res = sk.solve();
    assert_eq!(res.status, SolveStatus::Inconsistent);
    assert!(res.failed.contains(&2), "distance flagged: {res:?}");
}

#[test]
fn fully_constrained_rectangle_dof_zero_and_extra_redundant() {
    let mut sk = Sketch::new();
    let p1 = sk.add_point(0.0, 0.0);
    let p2 = sk.add_point(4.0, 0.0);
    let p3 = sk.add_point(4.0, 3.0);
    let p4 = sk.add_point(0.0, 3.0);
    let bottom = sk.add_line(p1, p2);
    let right = sk.add_line(p2, p3);
    let top = sk.add_line(p3, p4);
    let left = sk.add_line(p4, p1);
    sk.add_constraint(Constraint::Fixed(p1, 0.0, 0.0));
    sk.add_constraint(Constraint::Horizontal(bottom));
    sk.add_constraint(Constraint::Horizontal(top));
    sk.add_constraint(Constraint::Vertical(left));
    sk.add_constraint(Constraint::Vertical(right));
    sk.add_constraint(Constraint::Length(bottom, 4.0));
    sk.add_constraint(Constraint::Length(right, 3.0));
    // Redundant-but-consistent extra dimension.
    sk.add_constraint(Constraint::Length(top, 4.0));
    let res = sk.solve();
    assert!(res.converged(), "{res:?}");
    assert_eq!(res.dof, 0);
    assert_eq!(res.redundant, vec![7], "extra top dimension is redundant");
}

// ───────────────────────────── robustness ────────────────────────────────────

/// Solve the H/V rectangle from a batch of scrambled starts — must converge to
/// the identical rectangle every time.
#[test]
fn rectangle_converges_from_perturbed_starts() {
    // Deterministic LCG so the test is reproducible.
    let mut seed: u64 = 0x2545F4914F6CDD1D;
    let mut rand = move || {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        ((seed >> 33) as f64 / (1u64 << 31) as f64) - 1.0 // ∈ (-1, 1)
    };
    for trial in 0..25 {
        let j = 1.5; // jitter amplitude
        let mut sk = Sketch::new();
        let p1 = sk.add_point(rand() * j, rand() * j);
        let p2 = sk.add_point(4.0 + rand() * j, rand() * j);
        let p3 = sk.add_point(4.0 + rand() * j, 3.0 + rand() * j);
        let p4 = sk.add_point(rand() * j, 3.0 + rand() * j);
        let bottom = sk.add_line(p1, p2);
        let right = sk.add_line(p2, p3);
        let top = sk.add_line(p3, p4);
        let left = sk.add_line(p4, p1);
        sk.add_constraint(Constraint::Fixed(p1, 0.0, 0.0));
        sk.add_constraint(Constraint::Horizontal(bottom));
        sk.add_constraint(Constraint::Horizontal(top));
        sk.add_constraint(Constraint::Vertical(left));
        sk.add_constraint(Constraint::Vertical(right));
        sk.add_constraint(Constraint::Length(bottom, 4.0));
        sk.add_constraint(Constraint::Length(right, 3.0));
        let res = sk.solve();
        assert!(res.converged(), "trial {trial}: {res:?}");
        assert_near(sk.point(p3).0, 4.0, "p3.x");
        assert_near(sk.point(p3).1, 3.0, "p3.y");
    }
}

/// A solved sketch re-solved is a no-op (stability / idempotence).
#[test]
fn resolve_is_idempotent() {
    let mut sk = Sketch::new();
    let a = sk.add_point(0.0, 0.0);
    let b = sk.add_point(1.0, 1.0);
    sk.add_constraint(Constraint::Fixed(a, 0.0, 0.0));
    sk.add_constraint(Constraint::Distance(a, b, 5.0));
    assert!(sk.solve().converged());
    let before = sk.point(b);
    let res = sk.solve();
    assert!(res.converged());
    assert!(res.iterations <= 2, "already solved: {res:?}");
    let after = sk.point(b);
    assert!(dist(before, after) < 1e-9, "no drift on re-solve");
}

/// Circles + line: tangent line to two dimensioned circles keeps everything
/// consistent (multi-entity mixed system).
#[test]
fn belt_line_tangent_to_two_circles() {
    let mut sk = Sketch::new();
    let c1 = sk.add_point(0.0, 0.0);
    let c2 = sk.add_point(10.0, 0.0);
    let big = sk.add_circle(c1, 3.0);
    let small = sk.add_circle(c2, 3.0);
    let a = sk.add_point(0.0, 3.2);
    let b = sk.add_point(10.0, 3.2);
    let belt = sk.add_line(a, b);
    sk.add_constraint(Constraint::Fixed(c1, 0.0, 0.0));
    sk.add_constraint(Constraint::Fixed(c2, 10.0, 0.0));
    sk.add_constraint(Constraint::Radius(big, 3.0));
    sk.add_constraint(Constraint::Radius(small, 3.0));
    sk.add_constraint(Constraint::Tangent(belt, big));
    sk.add_constraint(Constraint::Tangent(belt, small));
    sk.add_constraint(Constraint::Horizontal(belt));
    let res = sk.solve();
    assert!(res.converged(), "{res:?}");
    let ((_, ay), (_, by)) = sk.line_points(belt);
    assert_near(ay, 3.0, "belt height a");
    assert_near(by, 3.0, "belt height b");
}
