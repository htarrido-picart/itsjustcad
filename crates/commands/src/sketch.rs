// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! Bridge between document geometry and the `itsjustcad-constraints` sketch
//! solver. Document lines (`Curve::Line`) and circles/arcs (`Curve::Arc`)
//! become solver entities; stored [`SketchConstraint`]s become solver
//! constraints; the solved coordinates are written back as fresh `Geometry`
//! values (z is preserved — the sketch solves in XY).

use std::collections::BTreeMap;

use itsjustcad_constraints as ic;
use itsjustcad_doc::{Document, Geometry, ObjectId, PointRef, SketchConstraint};
use kernel_curve::Curve;

use crate::error::ExecError;

/// Solver outcome distilled for command messages + geometry write-back.
pub struct SolveReport {
    /// Objects whose geometry changed, with the solved replacement.
    pub changed: Vec<(ObjectId, Geometry)>,
    pub converged: bool,
    /// Remaining degrees of freedom (excludes the anchored/fixed directions).
    pub dof: usize,
    /// 1-based indices (as shown by `constraints list`) of redundant constraints.
    pub redundant: Vec<usize>,
    /// 1-based indices of constraints that could not be satisfied.
    pub failed: Vec<usize>,
    /// Human summary line.
    pub message: String,
}

/// What a document object looks like to the solver.
enum SketchEntity {
    Line { line: ic::LineId, a: ic::PointId, b: ic::PointId, za: f64, zb: f64 },
    Circle { circle: ic::CircleId, center: ic::PointId, z: f64 },
}

/// The world-space point a [`PointRef`] names right now (for nearest-endpoint
/// resolution when building constraints).
pub fn point_ref_pos(doc: &Document, r: PointRef) -> Option<(f64, f64)> {
    let obj = doc.get(r.object)?;
    match &obj.geometry {
        Geometry::Curve(Curve::Line { a, b }) => match r.index {
            0 => Some((a.x, a.y)),
            _ => Some((b.x, b.y)),
        },
        Geometry::Curve(Curve::Arc { center, .. }) => Some((center.x, center.y)),
        _ => None,
    }
}

/// Candidate point refs on an object: line endpoints, or the circle center.
pub fn point_refs(doc: &Document, id: ObjectId) -> Result<Vec<PointRef>, ExecError> {
    match doc.get(id).map(|o| &o.geometry) {
        Some(Geometry::Curve(Curve::Line { .. })) => Ok(vec![
            PointRef { object: id, index: 0 },
            PointRef { object: id, index: 1 },
        ]),
        Some(Geometry::Curve(Curve::Arc { .. })) => Ok(vec![PointRef { object: id, index: 0 }]),
        _ => Err(unsupported(doc, id)),
    }
}

/// Nearest pair of candidate points between two objects.
pub fn nearest_point_pair(
    doc: &Document,
    a: ObjectId,
    b: ObjectId,
) -> Result<(PointRef, PointRef), ExecError> {
    let ra = point_refs(doc, a)?;
    let rb = point_refs(doc, b)?;
    let mut best = None;
    for &pa in &ra {
        for &pb in &rb {
            let (ax, ay) = point_ref_pos(doc, pa).expect("candidate resolves");
            let (bx, by) = point_ref_pos(doc, pb).expect("candidate resolves");
            let d2 = (ax - bx).powi(2) + (ay - by).powi(2);
            if best.map(|(bd, _, _)| d2 < bd).unwrap_or(true) {
                best = Some((d2, pa, pb));
            }
        }
    }
    let (_, pa, pb) = best.expect("both objects have candidate points");
    Ok((pa, pb))
}

pub fn is_line(doc: &Document, id: ObjectId) -> bool {
    matches!(doc.get(id).map(|o| &o.geometry), Some(Geometry::Curve(Curve::Line { .. })))
}

pub fn is_circle(doc: &Document, id: ObjectId) -> bool {
    matches!(doc.get(id).map(|o| &o.geometry), Some(Geometry::Curve(Curve::Arc { .. })))
}

pub fn unsupported(doc: &Document, id: ObjectId) -> ExecError {
    let what = doc
        .get(id)
        .map(|o| match &o.geometry {
            Geometry::Curve(c) => match c {
                Curve::Line { .. } => "line",
                Curve::Polyline { .. } => "polyline",
                Curve::Arc { .. } => "arc",
                Curve::Ellipse { .. } => "ellipse",
                Curve::Nurbs { .. } => "nurbs curve",
            },
            Geometry::Mesh(_) => "mesh",
            _ => "object",
        })
        .unwrap_or("missing object");
    ExecError::Invalid(format!(
        "constraints work on lines and circles/arcs; {id} is a {what}"
    ))
}

/// Build the solver sketch for every stored constraint, run it, and report.
/// Constraints referencing deleted or unsupported objects are skipped (their
/// 1-based indices are called out in the message).
pub fn solve_document(doc: &Document) -> Result<SolveReport, ExecError> {
    let mut sk = ic::Sketch::new();
    let mut entities: BTreeMap<ObjectId, SketchEntity> = BTreeMap::new();
    let mut skipped: Vec<usize> = Vec::new();
    // Constraint index in the *solver* → doc constraint 1-based index.
    let mut index_map: Vec<usize> = Vec::new();

    // Register an object's entity (lazily).
    fn ensure<'a>(
        sk: &mut ic::Sketch,
        entities: &'a mut BTreeMap<ObjectId, SketchEntity>,
        doc: &Document,
        id: ObjectId,
    ) -> Option<&'a SketchEntity> {
        if !entities.contains_key(&id) {
            match doc.get(id).map(|o| &o.geometry) {
                Some(Geometry::Curve(Curve::Line { a, b })) => {
                    let pa = sk.add_point(a.x, a.y);
                    let pb = sk.add_point(b.x, b.y);
                    let line = sk.add_line(pa, pb);
                    entities.insert(
                        id,
                        SketchEntity::Line { line, a: pa, b: pb, za: a.z, zb: b.z },
                    );
                }
                Some(Geometry::Curve(Curve::Arc { center, radius, .. })) => {
                    let pc = sk.add_point(center.x, center.y);
                    let circle = sk.add_circle(pc, *radius);
                    entities.insert(id, SketchEntity::Circle { circle, center: pc, z: center.z });
                }
                _ => return None,
            }
        }
        entities.get(&id)
    }

    let line_of = |e: &SketchEntity| match e {
        SketchEntity::Line { line, .. } => Some(*line),
        _ => None,
    };
    let circle_of = |e: &SketchEntity| match e {
        SketchEntity::Circle { circle, .. } => Some(*circle),
        _ => None,
    };

    for (i, c) in doc.constraints.iter().enumerate() {
        let n1 = i + 1; // 1-based, as listed
        // All referenced objects must register.
        let mut ok = true;
        for id in c.objects() {
            if ensure(&mut sk, &mut entities, doc, id).is_none() {
                ok = false;
            }
        }
        if !ok {
            skipped.push(n1);
            continue;
        }
        let point_of = |r: PointRef, entities: &BTreeMap<ObjectId, SketchEntity>| match entities
            .get(&r.object)
        {
            Some(SketchEntity::Line { a, b, .. }) => Some(if r.index == 0 { *a } else { *b }),
            Some(SketchEntity::Circle { center, .. }) => Some(*center),
            None => None,
        };
        let get_line = |id: ObjectId, entities: &BTreeMap<ObjectId, SketchEntity>| {
            entities.get(&id).and_then(line_of)
        };
        let get_circle = |id: ObjectId, entities: &BTreeMap<ObjectId, SketchEntity>| {
            entities.get(&id).and_then(circle_of)
        };
        let push = |cc: ic::Constraint, sk: &mut ic::Sketch, index_map: &mut Vec<usize>| {
            sk.add_constraint(cc);
            index_map.push(n1);
        };
        let mapped: Option<()> = (|| {
            match c {
                SketchConstraint::Coincident { a, b } => {
                    let (pa, pb) = (point_of(*a, &entities)?, point_of(*b, &entities)?);
                    push(ic::Constraint::Coincident(pa, pb), &mut sk, &mut index_map);
                }
                SketchConstraint::Horizontal { line } => {
                    push(
                        ic::Constraint::Horizontal(get_line(*line, &entities)?),
                        &mut sk,
                        &mut index_map,
                    );
                }
                SketchConstraint::Vertical { line } => {
                    push(
                        ic::Constraint::Vertical(get_line(*line, &entities)?),
                        &mut sk,
                        &mut index_map,
                    );
                }
                SketchConstraint::Distance { a, b, value } => {
                    let (pa, pb) = (point_of(*a, &entities)?, point_of(*b, &entities)?);
                    push(ic::Constraint::Distance(pa, pb, *value), &mut sk, &mut index_map);
                }
                SketchConstraint::DistanceToLine { point, line, value } => {
                    let p = point_of(*point, &entities)?;
                    let l = get_line(*line, &entities)?;
                    push(ic::Constraint::DistancePointLine(p, l, *value), &mut sk, &mut index_map);
                }
                SketchConstraint::Length { line, value } => {
                    push(
                        ic::Constraint::Length(get_line(*line, &entities)?, *value),
                        &mut sk,
                        &mut index_map,
                    );
                }
                SketchConstraint::Angle { a, b, degrees } => {
                    let (la, lb) = (get_line(*a, &entities)?, get_line(*b, &entities)?);
                    push(ic::Constraint::Angle(la, lb, *degrees), &mut sk, &mut index_map);
                }
                SketchConstraint::Parallel { a, b } => {
                    let (la, lb) = (get_line(*a, &entities)?, get_line(*b, &entities)?);
                    push(ic::Constraint::Parallel(la, lb), &mut sk, &mut index_map);
                }
                SketchConstraint::Perpendicular { a, b } => {
                    let (la, lb) = (get_line(*a, &entities)?, get_line(*b, &entities)?);
                    push(ic::Constraint::Perpendicular(la, lb), &mut sk, &mut index_map);
                }
                SketchConstraint::EqualLength { a, b } => {
                    let (la, lb) = (get_line(*a, &entities)?, get_line(*b, &entities)?);
                    push(ic::Constraint::EqualLength(la, lb), &mut sk, &mut index_map);
                }
                SketchConstraint::EqualRadius { a, b } => {
                    let (ca, cb) = (get_circle(*a, &entities)?, get_circle(*b, &entities)?);
                    push(ic::Constraint::EqualRadius(ca, cb), &mut sk, &mut index_map);
                }
                SketchConstraint::Radius { circle, value } => {
                    push(
                        ic::Constraint::Radius(get_circle(*circle, &entities)?, *value),
                        &mut sk,
                        &mut index_map,
                    );
                }
                SketchConstraint::Fixed { object } => match entities.get(object)? {
                    SketchEntity::Line { a, b, .. } => {
                        let (ax, ay) = sk.point(*a);
                        let (bx, by) = sk.point(*b);
                        let (a, b) = (*a, *b);
                        push(ic::Constraint::Fixed(a, ax, ay), &mut sk, &mut index_map);
                        push(ic::Constraint::Fixed(b, bx, by), &mut sk, &mut index_map);
                    }
                    SketchEntity::Circle { circle, center, .. } => {
                        let (cx, cy) = sk.point(*center);
                        let r = sk.radius(*circle);
                        let (circle, center) = (*circle, *center);
                        push(ic::Constraint::Fixed(center, cx, cy), &mut sk, &mut index_map);
                        push(ic::Constraint::Radius(circle, r), &mut sk, &mut index_map);
                    }
                },
                SketchConstraint::Tangent { line, circle } => {
                    let l = get_line(*line, &entities)?;
                    let ci = get_circle(*circle, &entities)?;
                    push(ic::Constraint::Tangent(l, ci), &mut sk, &mut index_map);
                }
                SketchConstraint::TangentCircles { a, b } => {
                    let (ca, cb) = (get_circle(*a, &entities)?, get_circle(*b, &entities)?);
                    push(ic::Constraint::TangentCircles(ca, cb), &mut sk, &mut index_map);
                }
                SketchConstraint::PointOnLine { point, line } => {
                    let p = point_of(*point, &entities)?;
                    let l = get_line(*line, &entities)?;
                    push(ic::Constraint::PointOnLine(p, l), &mut sk, &mut index_map);
                }
                SketchConstraint::PointOnCircle { point, circle } => {
                    let p = point_of(*point, &entities)?;
                    let ci = get_circle(*circle, &entities)?;
                    push(ic::Constraint::PointOnCircle(p, ci), &mut sk, &mut index_map);
                }
                SketchConstraint::Midpoint { point, line } => {
                    let p = point_of(*point, &entities)?;
                    let l = get_line(*line, &entities)?;
                    push(ic::Constraint::Midpoint(p, l), &mut sk, &mut index_map);
                }
            }
            Some(())
        })();
        if mapped.is_none() {
            skipped.push(n1);
        }
    }

    let res = sk.solve();

    // Write back changed geometry.
    let mut changed = Vec::new();
    for (id, ent) in &entities {
        let obj = doc.get(*id).expect("registered objects exist");
        let new_geo = match (ent, &obj.geometry) {
            (SketchEntity::Line { a, b, za, zb, .. }, _) => {
                let (ax, ay) = sk.point(*a);
                let (bx, by) = sk.point(*b);
                Geometry::Curve(Curve::Line {
                    a: glam::DVec3::new(ax, ay, *za),
                    b: glam::DVec3::new(bx, by, *zb),
                })
            }
            (
                SketchEntity::Circle { circle, center, z },
                Geometry::Curve(Curve::Arc { start, end, .. }),
            ) => {
                let (cx, cy) = sk.point(*center);
                Geometry::Curve(Curve::Arc {
                    center: glam::DVec3::new(cx, cy, *z),
                    radius: sk.radius(*circle),
                    start: *start,
                    end: *end,
                })
            }
            _ => continue,
        };
        if new_geo != obj.geometry {
            changed.push((*id, new_geo));
        }
    }

    // Map solver constraint indices back to 1-based doc indices (deduped).
    let to_doc = |idxs: &[usize]| -> Vec<usize> {
        let mut v: Vec<usize> = idxs.iter().map(|&i| index_map[i]).collect();
        v.sort_unstable();
        v.dedup();
        v
    };
    let redundant = to_doc(&res.redundant);
    let failed = to_doc(&res.failed);

    let mut message = if !res.converged() {
        format!(
            "NOT SOLVED — conflicting constraints: {} cannot be satisfied (geometry left at best fit)",
            failed
                .iter()
                .map(|i| format!("#{i} ({})", doc.constraints[i - 1].kind_name()))
                .collect::<Vec<_>>()
                .join(", ")
        )
    } else {
        let mut m = match res.dof {
            0 => "solved — fully constrained (0 DOF)".to_string(),
            d => format!("solved — under-constrained, {d} DOF remaining"),
        };
        if !redundant.is_empty() {
            m.push_str(&format!(
                "; redundant: {}",
                redundant
                    .iter()
                    .map(|i| format!("#{i} ({})", doc.constraints[i - 1].kind_name()))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        m
    };
    if !skipped.is_empty() {
        message.push_str(&format!(
            "; skipped {} constraint(s) referencing deleted/unsupported objects: {}",
            skipped.len(),
            skipped.iter().map(|i| format!("#{i}")).collect::<Vec<_>>().join(", ")
        ));
    }

    Ok(SolveReport {
        changed,
        converged: res.converged(),
        dof: res.dof,
        redundant,
        failed,
        message,
    })
}

/// Build the concrete document constraint for a command-line `constrain`
/// request: validates target types, picks the concrete form for polymorphic
/// kinds (equal/tangent/on/distance) and resolves point references
/// nearest-first from the current geometry.
pub fn build_doc_constraint(
    doc: &Document,
    kind: crate::ConstrainKind,
    a: ObjectId,
    b: Option<ObjectId>,
    value: Option<f64>,
) -> Result<SketchConstraint, ExecError> {
    use crate::ConstrainKind as K;
    let need_b = || b.ok_or_else(|| two_targets(kind));
    let need_value = |what: &str| {
        value.ok_or_else(|| ExecError::Invalid(format!("constrain {} needs {what}", kind.name())))
    };
    let need_line = |id: ObjectId| {
        if is_line(doc, id) { Ok(id) } else { Err(unsupported_for(doc, kind, id, "a line")) }
    };
    let need_circle = |id: ObjectId| {
        if is_circle(doc, id) {
            Ok(id)
        } else {
            Err(unsupported_for(doc, kind, id, "a circle/arc"))
        }
    };
    Ok(match kind {
        K::Horizontal => SketchConstraint::Horizontal { line: need_line(a)? },
        K::Vertical => SketchConstraint::Vertical { line: need_line(a)? },
        K::Fixed => {
            if !is_line(doc, a) && !is_circle(doc, a) {
                return Err(unsupported(doc, a));
            }
            SketchConstraint::Fixed { object: a }
        }
        K::Length => SketchConstraint::Length { line: need_line(a)?, value: need_value("a length")? },
        K::Radius => {
            SketchConstraint::Radius { circle: need_circle(a)?, value: need_value("a radius")? }
        }
        K::Coincident => {
            let b = need_b()?;
            let (pa, pb) = nearest_point_pair(doc, a, b)?;
            SketchConstraint::Coincident { a: pa, b: pb }
        }
        K::Distance => {
            let b = need_b()?;
            let value = need_value("a distance value")?;
            match (is_line(doc, a), is_line(doc, b), is_circle(doc, a), is_circle(doc, b)) {
                // circle ↔ line: perpendicular distance from the center.
                (true, false, false, true) => SketchConstraint::DistanceToLine {
                    point: PointRef { object: b, index: 0 },
                    line: a,
                    value,
                },
                (false, true, true, false) => SketchConstraint::DistanceToLine {
                    point: PointRef { object: a, index: 0 },
                    line: b,
                    value,
                },
                _ => {
                    let (pa, pb) = nearest_point_pair(doc, a, b)?;
                    SketchConstraint::Distance { a: pa, b: pb, value }
                }
            }
        }
        K::Angle => SketchConstraint::Angle {
            a: need_line(a)?,
            b: need_line(need_b()?)?,
            degrees: need_value("an angle in degrees")?,
        },
        K::Parallel => {
            SketchConstraint::Parallel { a: need_line(a)?, b: need_line(need_b()?)? }
        }
        K::Perpendicular => {
            SketchConstraint::Perpendicular { a: need_line(a)?, b: need_line(need_b()?)? }
        }
        K::Equal => {
            let b = need_b()?;
            if is_line(doc, a) && is_line(doc, b) {
                SketchConstraint::EqualLength { a, b }
            } else if is_circle(doc, a) && is_circle(doc, b) {
                SketchConstraint::EqualRadius { a, b }
            } else {
                return Err(ExecError::Invalid(
                    "constrain equal needs two lines (equal length) or two circles (equal radius)"
                        .into(),
                ));
            }
        }
        K::Tangent => {
            let b = need_b()?;
            if is_line(doc, a) && is_circle(doc, b) {
                SketchConstraint::Tangent { line: a, circle: b }
            } else if is_circle(doc, a) && is_line(doc, b) {
                SketchConstraint::Tangent { line: b, circle: a }
            } else if is_circle(doc, a) && is_circle(doc, b) {
                SketchConstraint::TangentCircles { a, b }
            } else {
                return Err(ExecError::Invalid(
                    "constrain tangent needs a line + circle, or two circles".into(),
                ));
            }
        }
        K::Midpoint => {
            let line = need_line(need_b()?)?;
            let ((x1, y1), (x2, y2)) = line_endpoints(doc, line)?;
            let mid = ((x1 + x2) / 2.0, (y1 + y2) / 2.0);
            let point = nearest_ref_to(doc, a, mid)?;
            SketchConstraint::Midpoint { point, line }
        }
        K::On => {
            let b = need_b()?;
            if is_line(doc, b) {
                let ((x1, y1), (x2, y2)) = line_endpoints(doc, b)?;
                let mid = ((x1 + x2) / 2.0, (y1 + y2) / 2.0);
                let point = nearest_ref_to(doc, a, mid)?;
                SketchConstraint::PointOnLine { point, line: b }
            } else if is_circle(doc, b) {
                let c = point_ref_pos(doc, PointRef { object: b, index: 0 })
                    .expect("circle has a center");
                let point = nearest_ref_to(doc, a, c)?;
                SketchConstraint::PointOnCircle { point, circle: b }
            } else {
                return Err(unsupported(doc, b));
            }
        }
    })
}

fn two_targets(kind: crate::ConstrainKind) -> ExecError {
    ExecError::Invalid(format!("constrain {} needs two target objects", kind.name()))
}

fn unsupported_for(
    doc: &Document,
    kind: crate::ConstrainKind,
    id: ObjectId,
    wanted: &str,
) -> ExecError {
    ExecError::Invalid(format!(
        "constrain {} needs {wanted}; {} is not",
        kind.name(),
        doc.get(id).and_then(|o| o.name.clone()).unwrap_or_else(|| id.short()),
    ))
}

fn line_endpoints(doc: &Document, id: ObjectId) -> Result<((f64, f64), (f64, f64)), ExecError> {
    match doc.get(id).map(|o| &o.geometry) {
        Some(Geometry::Curve(Curve::Line { a, b })) => Ok(((a.x, a.y), (b.x, b.y))),
        _ => Err(unsupported(doc, id)),
    }
}

/// The candidate point on `obj` closest to the given position.
fn nearest_ref_to(
    doc: &Document,
    obj: ObjectId,
    (tx, ty): (f64, f64),
) -> Result<PointRef, ExecError> {
    let refs = point_refs(doc, obj)?;
    refs.into_iter()
        .min_by(|&p, &q| {
            let d = |r: PointRef| {
                let (x, y) = point_ref_pos(doc, r).expect("candidate resolves");
                (x - tx).powi(2) + (y - ty).powi(2)
            };
            d(p).total_cmp(&d(q))
        })
        .ok_or_else(|| unsupported(doc, obj))
}

/// One listing line for `constraints list` (1-based index prepended by caller).
pub fn describe_constraint(doc: &Document, c: &SketchConstraint) -> String {
    let name = |id: ObjectId| -> String {
        doc.get(id)
            .and_then(|o| o.name.clone())
            .unwrap_or_else(|| id.short())
    };
    let pr = |r: &PointRef| -> String {
        let base = name(r.object);
        if is_circle(doc, r.object) {
            format!("{base}.center")
        } else if r.index == 0 {
            format!("{base}.a")
        } else {
            format!("{base}.b")
        }
    };
    match c {
        SketchConstraint::Coincident { a, b } => format!("coincident {} {}", pr(a), pr(b)),
        SketchConstraint::Horizontal { line } => format!("horizontal {}", name(*line)),
        SketchConstraint::Vertical { line } => format!("vertical {}", name(*line)),
        SketchConstraint::Distance { a, b, value } => {
            format!("distance {} {} = {value}", pr(a), pr(b))
        }
        SketchConstraint::DistanceToLine { point, line, value } => {
            format!("distance {} to {} = {value}", pr(point), name(*line))
        }
        SketchConstraint::Length { line, value } => format!("length {} = {value}", name(*line)),
        SketchConstraint::Angle { a, b, degrees } => {
            format!("angle {} {} = {degrees}°", name(*a), name(*b))
        }
        SketchConstraint::Parallel { a, b } => format!("parallel {} {}", name(*a), name(*b)),
        SketchConstraint::Perpendicular { a, b } => {
            format!("perpendicular {} {}", name(*a), name(*b))
        }
        SketchConstraint::EqualLength { a, b } => format!("equal length {} {}", name(*a), name(*b)),
        SketchConstraint::EqualRadius { a, b } => format!("equal radius {} {}", name(*a), name(*b)),
        SketchConstraint::Radius { circle, value } => {
            format!("radius {} = {value}", name(*circle))
        }
        SketchConstraint::Fixed { object } => format!("fixed {}", name(*object)),
        SketchConstraint::Tangent { line, circle } => {
            format!("tangent {} {}", name(*line), name(*circle))
        }
        SketchConstraint::TangentCircles { a, b } => {
            format!("tangent {} {}", name(*a), name(*b))
        }
        SketchConstraint::PointOnLine { point, line } => {
            format!("{} on {}", pr(point), name(*line))
        }
        SketchConstraint::PointOnCircle { point, circle } => {
            format!("{} on {}", pr(point), name(*circle))
        }
        SketchConstraint::Midpoint { point, line } => {
            format!("{} at midpoint of {}", pr(point), name(*line))
        }
    }
}
