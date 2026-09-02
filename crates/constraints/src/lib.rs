// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! 2D sketch dimensional-constraint solver, SolveSpace-style.
//!
//! A [`Sketch`] holds a flat parameter vector plus entities (points, lines,
//! circles) that index into it, and a list of [`Constraint`]s over those
//! entities. [`Sketch::solve`] runs Newton–Raphson with Levenberg–Marquardt
//! damping on the residual system (numeric Jacobian), which handles both
//! well-determined and over-determined (redundant-but-consistent) systems.
//!
//! Diagnostics: the result reports remaining degrees of freedom (rank-based),
//! redundant constraints (rows linearly dependent on earlier ones), and — when
//! the system is inconsistent — which constraints failed to be satisfied.
//!
//! Pure std, no dependencies; everything is deterministic.

mod solver;

pub use solver::{SolveResult, SolveStatus};

/// Handle to a point entity in a [`Sketch`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PointId(pub(crate) usize);

/// Handle to a line entity in a [`Sketch`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct LineId(pub(crate) usize);

/// Handle to a circle entity in a [`Sketch`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CircleId(pub(crate) usize);

#[derive(Debug, Clone)]
pub(crate) enum Entity {
    /// px, py: parameter indices.
    Point { px: usize, py: usize },
    /// p1, p2: entity indices of the endpoint Points.
    Line { p1: usize, p2: usize },
    /// center: entity index of the center Point; pr: radius parameter index.
    Circle { center: usize, pr: usize },
}

/// A constraint over sketch entities. Dimensional values (distances, radii)
/// are in sketch units; angles are in degrees.
#[derive(Debug, Clone)]
pub enum Constraint {
    /// Two points at the same location (2 residuals).
    Coincident(PointId, PointId),
    /// Line parallel to the X axis.
    Horizontal(LineId),
    /// Line parallel to the Y axis.
    Vertical(LineId),
    /// Distance between two points.
    Distance(PointId, PointId, f64),
    /// Perpendicular distance from a point to an (infinite) line. Signed side
    /// is chosen from the initial configuration.
    DistancePointLine(PointId, LineId, f64),
    /// Length of a line segment.
    Length(LineId, f64),
    /// Angle between two lines, in degrees (direction p1→p2 of each).
    Angle(LineId, LineId, f64),
    /// Lines parallel.
    Parallel(LineId, LineId),
    /// Lines perpendicular.
    Perpendicular(LineId, LineId),
    /// Equal segment lengths.
    EqualLength(LineId, LineId),
    /// Equal circle radii.
    EqualRadius(CircleId, CircleId),
    /// Circle radius equals the given value.
    Radius(CircleId, f64),
    /// Point locked at the given location (2 residuals).
    Fixed(PointId, f64, f64),
    /// Point on the (infinite) line through the segment.
    PointOnLine(PointId, LineId),
    /// Point on the circle.
    PointOnCircle(PointId, CircleId),
    /// Line tangent to circle. The circle stays on its initial side.
    Tangent(LineId, CircleId),
    /// Two circles tangent (externally or internally, whichever is closer at
    /// add time).
    TangentCircles(CircleId, CircleId),
    /// `a` and `b` mirror images about the line (2 residuals: midpoint on
    /// line, segment perpendicular to line).
    Symmetric(PointId, PointId, LineId),
    /// Point at the midpoint of the line segment (2 residuals).
    Midpoint(PointId, LineId),
}

impl Constraint {
    /// Number of scalar residual equations this constraint contributes.
    pub fn residual_count(&self) -> usize {
        match self {
            Constraint::Coincident(..)
            | Constraint::Fixed(..)
            | Constraint::Symmetric(..)
            | Constraint::Midpoint(..) => 2,
            _ => 1,
        }
    }

    /// Short machine name for messages.
    pub fn kind_name(&self) -> &'static str {
        match self {
            Constraint::Coincident(..) => "coincident",
            Constraint::Horizontal(..) => "horizontal",
            Constraint::Vertical(..) => "vertical",
            Constraint::Distance(..) => "distance",
            Constraint::DistancePointLine(..) => "distance",
            Constraint::Length(..) => "length",
            Constraint::Angle(..) => "angle",
            Constraint::Parallel(..) => "parallel",
            Constraint::Perpendicular(..) => "perpendicular",
            Constraint::EqualLength(..) => "equal",
            Constraint::EqualRadius(..) => "equal",
            Constraint::Radius(..) => "radius",
            Constraint::Fixed(..) => "fixed",
            Constraint::PointOnLine(..) => "on",
            Constraint::PointOnCircle(..) => "on",
            Constraint::Tangent(..) => "tangent",
            Constraint::TangentCircles(..) => "tangent",
            Constraint::Symmetric(..) => "symmetric",
            Constraint::Midpoint(..) => "midpoint",
        }
    }
}

/// Per-constraint metadata resolved once when the constraint is added
/// (side/branch choices that must stay stable across iterations).
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct ConstraintAux {
    /// Sign for signed-distance style constraints (+1/-1).
    pub sign: f64,
    /// `true` = internal tangency for [`Constraint::TangentCircles`].
    pub internal: bool,
}

/// A 2D sketch: parameters, entities, constraints.
#[derive(Debug, Clone, Default)]
pub struct Sketch {
    pub(crate) params: Vec<f64>,
    pub(crate) entities: Vec<Entity>,
    pub(crate) constraints: Vec<Constraint>,
    pub(crate) aux: Vec<ConstraintAux>,
}

impl Sketch {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a free point at (x, y).
    pub fn add_point(&mut self, x: f64, y: f64) -> PointId {
        let px = self.params.len();
        self.params.push(x);
        self.params.push(y);
        self.entities.push(Entity::Point { px, py: px + 1 });
        PointId(self.entities.len() - 1)
    }

    /// Add a line between two existing points.
    pub fn add_line(&mut self, a: PointId, b: PointId) -> LineId {
        self.entities.push(Entity::Line { p1: a.0, p2: b.0 });
        LineId(self.entities.len() - 1)
    }

    /// Add a circle with an existing center point and the given radius.
    pub fn add_circle(&mut self, center: PointId, radius: f64) -> CircleId {
        let pr = self.params.len();
        self.params.push(radius);
        self.entities.push(Entity::Circle { center: center.0, pr });
        CircleId(self.entities.len() - 1)
    }

    /// Current position of a point.
    pub fn point(&self, id: PointId) -> (f64, f64) {
        match self.entities[id.0] {
            Entity::Point { px, py } => (self.params[px], self.params[py]),
            _ => unreachable!("PointId always references a Point"),
        }
    }

    /// Current radius of a circle.
    pub fn radius(&self, id: CircleId) -> f64 {
        match self.entities[id.0] {
            Entity::Circle { pr, .. } => self.params[pr],
            _ => unreachable!("CircleId always references a Circle"),
        }
    }

    /// Endpoints of a line.
    pub fn line_points(&self, id: LineId) -> ((f64, f64), (f64, f64)) {
        let (p1, p2) = match self.entities[id.0] {
            Entity::Line { p1, p2 } => (p1, p2),
            _ => unreachable!("LineId always references a Line"),
        };
        (self.point(PointId(p1)), self.point(PointId(p2)))
    }

    pub fn constraint_count(&self) -> usize {
        self.constraints.len()
    }

    /// Total number of parameters (before constraints); 2 per point + 1 per
    /// circle radius.
    pub fn param_count(&self) -> usize {
        self.params.len()
    }

    /// Add a constraint. Side/branch choices (tangency side, point-line side)
    /// are latched from the current configuration.
    pub fn add_constraint(&mut self, c: Constraint) {
        let aux = self.resolve_aux(&c);
        self.constraints.push(c);
        self.aux.push(aux);
    }

    fn resolve_aux(&self, c: &Constraint) -> ConstraintAux {
        let mut aux = ConstraintAux { sign: 1.0, internal: false };
        match c {
            Constraint::DistancePointLine(p, l, _) => {
                let d = self.signed_point_line(*p, *l);
                aux.sign = if d < 0.0 { -1.0 } else { 1.0 };
            }
            Constraint::Tangent(l, circ) => {
                let (cx, cy) = self.circle_center(*circ);
                let d = self.signed_xy_line((cx, cy), *l);
                aux.sign = if d < 0.0 { -1.0 } else { 1.0 };
            }
            Constraint::TangentCircles(a, b) => {
                let (ax, ay) = self.circle_center(*a);
                let (bx, by) = self.circle_center(*b);
                let d = ((bx - ax).powi(2) + (by - ay).powi(2)).sqrt();
                let (ra, rb) = (self.radius(*a), self.radius(*b));
                // Internal tangency when the current gap is closer to |ra-rb|
                // than to ra+rb.
                aux.internal = (d - (ra - rb).abs()).abs() < (d - (ra + rb)).abs();
            }
            _ => {}
        }
        aux
    }

    pub(crate) fn circle_center(&self, id: CircleId) -> (f64, f64) {
        match self.entities[id.0] {
            Entity::Circle { center, .. } => self.point(PointId(center)),
            _ => unreachable!("CircleId always references a Circle"),
        }
    }

    fn signed_point_line(&self, p: PointId, l: LineId) -> f64 {
        self.signed_xy_line(self.point(p), l)
    }

    /// Signed perpendicular distance from (x, y) to the infinite line.
    pub(crate) fn signed_xy_line(&self, (x, y): (f64, f64), l: LineId) -> f64 {
        let ((x1, y1), (x2, y2)) = self.line_points(l);
        let (dx, dy) = (x2 - x1, y2 - y1);
        let len = (dx * dx + dy * dy).sqrt().max(1e-12);
        (dx * (y - y1) - dy * (x - x1)) / len
    }

    /// Evaluate all constraint residuals at the given parameter vector.
    pub(crate) fn residuals(&self, q: &[f64]) -> Vec<f64> {
        let mut out = Vec::new();
        for (c, aux) in self.constraints.iter().zip(&self.aux) {
            self.residual_of(c, aux, q, &mut out);
        }
        out
    }

    fn pt(&self, id: PointId, q: &[f64]) -> (f64, f64) {
        match self.entities[id.0] {
            Entity::Point { px, py } => (q[px], q[py]),
            _ => unreachable!(),
        }
    }

    fn line(&self, id: LineId, q: &[f64]) -> ((f64, f64), (f64, f64)) {
        match self.entities[id.0] {
            Entity::Line { p1, p2 } => (self.pt(PointId(p1), q), self.pt(PointId(p2), q)),
            _ => unreachable!(),
        }
    }

    fn circ(&self, id: CircleId, q: &[f64]) -> ((f64, f64), f64) {
        match self.entities[id.0] {
            Entity::Circle { center, pr } => (self.pt(PointId(center), q), q[pr]),
            _ => unreachable!(),
        }
    }

    fn residual_of(&self, c: &Constraint, aux: &ConstraintAux, q: &[f64], out: &mut Vec<f64>) {
        let signed_dist = |(x, y): (f64, f64), l: LineId| -> f64 {
            let ((x1, y1), (x2, y2)) = self.line(l, q);
            let (dx, dy) = (x2 - x1, y2 - y1);
            let len = (dx * dx + dy * dy).sqrt().max(1e-12);
            (dx * (y - y1) - dy * (x - x1)) / len
        };
        let dir = |l: LineId| -> (f64, f64) {
            let ((x1, y1), (x2, y2)) = self.line(l, q);
            (x2 - x1, y2 - y1)
        };
        match c {
            Constraint::Coincident(a, b) => {
                let (ax, ay) = self.pt(*a, q);
                let (bx, by) = self.pt(*b, q);
                out.push(ax - bx);
                out.push(ay - by);
            }
            Constraint::Horizontal(l) => {
                let ((_, y1), (_, y2)) = self.line(*l, q);
                out.push(y2 - y1);
            }
            Constraint::Vertical(l) => {
                let ((x1, _), (x2, _)) = self.line(*l, q);
                out.push(x2 - x1);
            }
            Constraint::Distance(a, b, d) => {
                let (ax, ay) = self.pt(*a, q);
                let (bx, by) = self.pt(*b, q);
                out.push(((bx - ax).powi(2) + (by - ay).powi(2)).sqrt() - d);
            }
            Constraint::DistancePointLine(p, l, d) => {
                out.push(aux.sign * signed_dist(self.pt(*p, q), *l) - d);
            }
            Constraint::Length(l, d) => {
                let (dx, dy) = dir(*l);
                out.push((dx * dx + dy * dy).sqrt() - d);
            }
            Constraint::Angle(l1, l2, deg) => {
                let (d1x, d1y) = dir(*l1);
                let (d2x, d2y) = dir(*l2);
                let cross = d1x * d2y - d1y * d2x;
                let dot = d1x * d2x + d1y * d2y;
                let mut e = cross.atan2(dot) - deg.to_radians();
                // wrap to (-pi, pi]
                while e > std::f64::consts::PI {
                    e -= std::f64::consts::TAU;
                }
                while e <= -std::f64::consts::PI {
                    e += std::f64::consts::TAU;
                }
                out.push(e);
            }
            Constraint::Parallel(l1, l2) => {
                let (d1x, d1y) = dir(*l1);
                let (d2x, d2y) = dir(*l2);
                let scale = ((d1x * d1x + d1y * d1y) * (d2x * d2x + d2y * d2y))
                    .sqrt()
                    .max(1e-12);
                out.push((d1x * d2y - d1y * d2x) / scale.sqrt());
            }
            Constraint::Perpendicular(l1, l2) => {
                let (d1x, d1y) = dir(*l1);
                let (d2x, d2y) = dir(*l2);
                let scale = ((d1x * d1x + d1y * d1y) * (d2x * d2x + d2y * d2y))
                    .sqrt()
                    .max(1e-12);
                out.push((d1x * d2x + d1y * d2y) / scale.sqrt());
            }
            Constraint::EqualLength(l1, l2) => {
                let (d1x, d1y) = dir(*l1);
                let (d2x, d2y) = dir(*l2);
                out.push((d1x * d1x + d1y * d1y).sqrt() - (d2x * d2x + d2y * d2y).sqrt());
            }
            Constraint::EqualRadius(a, b) => {
                let (_, ra) = self.circ(*a, q);
                let (_, rb) = self.circ(*b, q);
                out.push(ra - rb);
            }
            Constraint::Radius(cid, r) => {
                let (_, rc) = self.circ(*cid, q);
                out.push(rc - r);
            }
            Constraint::Fixed(p, x0, y0) => {
                let (x, y) = self.pt(*p, q);
                out.push(x - x0);
                out.push(y - y0);
            }
            Constraint::PointOnLine(p, l) => {
                out.push(signed_dist(self.pt(*p, q), *l));
            }
            Constraint::PointOnCircle(p, cid) => {
                let (x, y) = self.pt(*p, q);
                let ((cx, cy), r) = self.circ(*cid, q);
                out.push(((x - cx).powi(2) + (y - cy).powi(2)).sqrt() - r);
            }
            Constraint::Tangent(l, cid) => {
                let ((cx, cy), r) = self.circ(*cid, q);
                out.push(aux.sign * signed_dist((cx, cy), *l) - r);
            }
            Constraint::TangentCircles(a, b) => {
                let ((ax, ay), ra) = self.circ(*a, q);
                let ((bx, by), rb) = self.circ(*b, q);
                let d = ((bx - ax).powi(2) + (by - ay).powi(2)).sqrt();
                let target = if aux.internal { (ra - rb).abs() } else { ra + rb };
                out.push(d - target);
            }
            Constraint::Symmetric(a, b, l) => {
                let (ax, ay) = self.pt(*a, q);
                let (bx, by) = self.pt(*b, q);
                let mid = ((ax + bx) / 2.0, (ay + by) / 2.0);
                out.push(signed_dist(mid, *l));
                let (dx, dy) = dir(*l);
                let len = (dx * dx + dy * dy).sqrt().max(1e-12);
                out.push(((bx - ax) * dx + (by - ay) * dy) / len);
            }
            Constraint::Midpoint(p, l) => {
                let (x, y) = self.pt(*p, q);
                let ((x1, y1), (x2, y2)) = self.line(*l, q);
                out.push(x - (x1 + x2) / 2.0);
                out.push(y - (y1 + y2) / 2.0);
            }
        }
    }

    /// Solve the sketch in place. On success the entity positions are updated;
    /// on failure the parameters are left at the best attempt.
    pub fn solve(&mut self) -> SolveResult {
        solver::solve(self)
    }
}

#[cfg(test)]
mod tests;
