use glam::DVec3;
use kernel_curve::{clamped_uniform_knots, Curve};
use kernel_mesh::extrude_profile;
use mydrafter_doc::{Document, Geometry, ObjectId, SceneObject};

use crate::error::ExecError;
use crate::{Command, MirrorPlane, Selector};

/// Chord tolerance used when tessellating profile curves for extrusion.
const PROFILE_TOL: f64 = 0.01;

#[derive(Debug)]
pub struct ApplyOutcome {
    /// Ids of objects created by this command.
    pub created: Vec<ObjectId>,
    /// Human/LLM-readable result line, echoed to the command line and deck.
    pub message: String,
}

struct AppliedOp {
    /// Forward op with ids filled in — this is what gets saved and replayed.
    op: Command,
    inverse: Inverse,
}

enum Inverse {
    DeleteCreated(Vec<ObjectId>),
    MoveBack { ids: Vec<ObjectId>, delta: DVec3 },
    Restore(Vec<(SceneObject, usize)>),
    Rename(Vec<(ObjectId, Option<String>)>),
    /// Booleans: delete the result, restore the consumed inputs.
    Replace {
        created: Vec<ObjectId>,
        consumed: Vec<(SceneObject, usize)>,
    },
    /// Transforms: write back pre-transform geometry snapshots. Exact —
    /// inverse matrices drift and cannot un-tessellate an arc.
    SetGeometry(Vec<(ObjectId, Geometry)>),
}

/// Owns the document plus its op-log; the single mutation path for both the
/// human command line and the LLM deck.
#[derive(Default)]
pub struct Session {
    pub doc: Document,
    log: Vec<AppliedOp>,
    cursor: usize,
}

impl Session {
    pub fn run(&mut self, cmd: Command) -> Result<ApplyOutcome, ExecError> {
        match cmd {
            Command::Undo => self.undo(),
            Command::Redo => self.redo(),
            cmd => {
                let logged = cmd.is_logged();
                let (op, inverse, outcome) = apply_forward(&mut self.doc, cmd)?;
                if logged {
                    self.log.truncate(self.cursor);
                    self.log.push(AppliedOp { op, inverse });
                    self.cursor = self.log.len();
                }
                Ok(outcome)
            }
        }
    }

    fn undo(&mut self) -> Result<ApplyOutcome, ExecError> {
        if self.cursor == 0 {
            return Err(ExecError::NothingToUndo);
        }
        self.cursor -= 1;
        let applied = &self.log[self.cursor];
        match &applied.inverse {
            Inverse::DeleteCreated(ids) => {
                // Redo replays the forward op (ids already filled in the log),
                // recreating identical objects — no snapshot needed here.
                let ids = ids.clone();
                for id in ids {
                    self.doc.remove(id);
                }
            }
            Inverse::MoveBack { ids, delta } => {
                for id in ids {
                    if let Some(obj) = self.doc.get_mut(*id) {
                        obj.geometry.translate(-*delta);
                    }
                }
            }
            Inverse::Restore(objs) => {
                for (obj, index) in objs.iter().rev() {
                    self.doc.restore(obj.clone(), *index);
                }
            }
            Inverse::Rename(prev) => {
                for (id, name) in prev {
                    if let Some(obj) = self.doc.get_mut(*id) {
                        obj.name = name.clone();
                    }
                }
            }
            Inverse::SetGeometry(snapshots) => {
                for (id, geometry) in snapshots.clone() {
                    if let Some(obj) = self.doc.get_mut(id) {
                        obj.geometry = geometry;
                    }
                }
            }
            Inverse::Replace { created, consumed } => {
                let created = created.clone();
                for id in created {
                    self.doc.remove(id);
                }
                for (obj, index) in consumed.iter().rev() {
                    self.doc.restore(obj.clone(), *index);
                }
            }
        }
        Ok(ApplyOutcome {
            created: Vec::new(),
            message: format!("undid: {}", describe(&self.log[self.cursor].op)),
        })
    }

    fn redo(&mut self) -> Result<ApplyOutcome, ExecError> {
        if self.cursor >= self.log.len() {
            return Err(ExecError::NothingToRedo);
        }
        let op = self.log[self.cursor].op.clone();
        let (op, inverse, outcome) = apply_forward(&mut self.doc, op)?;
        self.log[self.cursor] = AppliedOp { op, inverse };
        self.cursor += 1;
        Ok(outcome)
    }

    /// Effective forward log (up to the undo cursor) — this is the file format.
    pub fn save_log(&self) -> Vec<Command> {
        self.log[..self.cursor].iter().map(|a| a.op.clone()).collect()
    }

    /// Rebuild a session by replaying a saved log through the same `apply`
    /// path used live. Ids stored in the log are reused, so the result is
    /// identical to the session that saved it.
    pub fn replay(log: Vec<Command>) -> Result<Self, ExecError> {
        let mut session = Session::default();
        for cmd in log {
            session.run(cmd)?;
        }
        Ok(session)
    }
}

fn resolve(doc: &Document, sel: &Selector) -> Result<Vec<ObjectId>, ExecError> {
    let ids = match sel {
        Selector::Ids { ids } => ids.clone(),
        Selector::Named { name } => doc.find_named(name),
        Selector::Last { n } => doc.last_ids(*n),
        Selector::All => doc.all_ids(),
        Selector::Selected => doc.selection.iter().copied().collect(),
    };
    if ids.is_empty() {
        return Err(ExecError::EmptySelection(match sel {
            Selector::Named { name } => format!("no object named '{name}'"),
            Selector::Selected => "selection is empty".to_string(),
            _ => format!("document has {} objects", doc.len()),
        }));
    }
    Ok(ids)
}

fn insert_curve(
    doc: &mut Document,
    id: Option<ObjectId>,
    curve: Curve,
    what: &str,
) -> (ObjectId, ApplyOutcome) {
    let id = id.unwrap_or_default();
    doc.insert(SceneObject {
        id,
        name: None,
        geometry: Geometry::Curve(curve),
    });
    let outcome = ApplyOutcome {
        created: vec![id],
        message: format!("{what} {id}"),
    };
    (id, outcome)
}

/// Apply `linear` about `center` (targets' combined AABB center when `None`)
/// to every resolved target, snapshotting geometry for exact undo.
fn apply_about_center(
    doc: &mut Document,
    ids: &[ObjectId],
    center: Option<DVec3>,
    linear: glam::DMat4,
) -> (Inverse, usize) {
    let center = center.unwrap_or_else(|| {
        let mut bb = doc.get(ids[0]).expect("resolved").geometry.aabb();
        for id in &ids[1..] {
            bb = bb.union(doc.get(*id).expect("resolved").geometry.aabb());
        }
        bb.center()
    });
    let m = glam::DMat4::from_translation(center) * linear
        * glam::DMat4::from_translation(-center);
    let mut snapshots = Vec::with_capacity(ids.len());
    let mut tessellated = 0usize;
    for id in ids {
        let obj = doc.get_mut(*id).expect("resolved");
        snapshots.push((*id, obj.geometry.clone()));
        if !obj.geometry.transform(&m, PROFILE_TOL) {
            tessellated += 1;
        }
    }
    (Inverse::SetGeometry(snapshots), tessellated)
}

/// "…, 2 curve(s) tessellated to polylines" suffix when a transform degraded
/// arcs/ellipses.
fn tessellation_note(count: usize) -> String {
    if count == 0 {
        String::new()
    } else {
        format!(", {count} curve(s) tessellated to polylines")
    }
}

/// Collect mesh clones for a boolean; curves are rejected with a hint the
/// LLM can act on.
fn boolean_inputs(
    doc: &Document,
    ids: &[ObjectId],
) -> Result<Vec<kernel_mesh::Mesh>, ExecError> {
    ids.iter()
        .map(|id| {
            let obj = doc.get(*id).expect("resolved id exists");
            match &obj.geometry {
                Geometry::Mesh(m) => Ok(m.clone()),
                Geometry::Curve(_) => Err(ExecError::Invalid(format!(
                    "'{id}' is a curve; booleans need meshes — extrude it first"
                ))),
            }
        })
        .collect()
}

fn fold_csg(
    meshes: Vec<kernel_mesh::Mesh>,
    op: fn(&kernel_mesh::Mesh, &kernel_mesh::Mesh) -> kernel_mesh::Mesh,
) -> kernel_mesh::Mesh {
    let mut iter = meshes.into_iter();
    let first = iter.next().expect("callers guarantee at least one mesh");
    iter.fold(first, |acc, m| op(&acc, &m))
}

/// Consume the input objects and insert the boolean result. Errors (leaving
/// the document untouched) when the result is empty.
fn replace_with_result(
    doc: &mut Document,
    id: Option<ObjectId>,
    input_ids: &[ObjectId],
    result: kernel_mesh::Mesh,
    name: Option<String>,
    what: &str,
) -> Result<(ObjectId, Inverse, String), ExecError> {
    if result.faces().is_empty() {
        let boxes: Vec<String> = input_ids
            .iter()
            .map(|id| {
                let bb = doc.get(*id).expect("resolved").geometry.aabb();
                format!("{id}: {:.2}..{:.2}", bb.min, bb.max)
            })
            .collect();
        return Err(ExecError::Invalid(format!(
            "{what} is empty: the objects do not overlap. Check their positions ({})",
            boxes.join("; ")
        )));
    }
    let volume = kernel_mesh::signed_volume(&result);
    let mut consumed = Vec::new();
    for id in input_ids {
        if let Some(pair) = doc.remove(*id) {
            consumed.push(pair);
        }
    }
    let id = id.unwrap_or_default();
    doc.insert(SceneObject {
        id,
        name,
        geometry: Geometry::Mesh(result),
    });
    Ok((
        id,
        Inverse::Replace { created: vec![id], consumed },
        format!(
            "{what} of {} object(s) -> {id} (volume {volume:.2})",
            input_ids.len()
        ),
    ))
}

/// Apply a (non-undo) command. Returns the op with ids filled for the log.
fn apply_forward(
    doc: &mut Document,
    cmd: Command,
) -> Result<(Command, Inverse, ApplyOutcome), ExecError> {
    match cmd {
        Command::Box { id, corner, size } => {
            if size.min_element() <= 0.0 {
                return Err(ExecError::Invalid(format!(
                    "box size must be positive, got {size}"
                )));
            }
            let id = id.unwrap_or_default();
            doc.insert(SceneObject {
                id,
                name: None,
                geometry: Geometry::Mesh(kernel_mesh::make_box(corner, size)),
            });
            Ok((
                Command::Box { id: Some(id), corner, size },
                Inverse::DeleteCreated(vec![id]),
                ApplyOutcome {
                    created: vec![id],
                    message: format!("box {id} ({} x {} x {})", size.x, size.y, size.z),
                },
            ))
        }
        Command::Extrude { id, profile, height } => {
            let ids = resolve(doc, &profile)?;
            if ids.len() != 1 {
                return Err(ExecError::BadProfile(format!(
                    "selector matched {} objects, expected exactly 1",
                    ids.len()
                )));
            }
            let src = doc.get(ids[0]).expect("resolved id exists");
            let Geometry::Curve(curve) = &src.geometry else {
                return Err(ExecError::BadProfile("selected object is a mesh, not a curve".into()));
            };
            if !curve.is_closed() {
                return Err(ExecError::BadProfile(
                    "curve is not closed (close it or use 'polyline ... closed')".into(),
                ));
            }
            let pts3 = curve.tessellate(PROFILE_TOL);
            let base_z = pts3.first().map(|p| p.z).unwrap_or(0.0);
            let profile2d: Vec<glam::DVec2> = pts3.iter().map(|p| p.truncate()).collect();
            let id = id.unwrap_or_default();
            doc.insert(SceneObject {
                id,
                name: None,
                geometry: Geometry::Mesh(extrude_profile(&profile2d, base_z, height)),
            });
            Ok((
                Command::Extrude { id: Some(id), profile, height },
                Inverse::DeleteCreated(vec![id]),
                ApplyOutcome {
                    created: vec![id],
                    message: format!("extruded {} -> {id} (h={height})", ids[0]),
                },
            ))
        }
        Command::Line { id, a, b } => {
            let (id, outcome) = insert_curve(doc, id, Curve::Line { a, b }, "line");
            Ok((
                Command::Line { id: Some(id), a, b },
                Inverse::DeleteCreated(vec![id]),
                outcome,
            ))
        }
        Command::Polyline { id, points, closed } => {
            if closed && points.len() < 3 {
                return Err(ExecError::Invalid(
                    "closed polyline needs at least 3 points".into(),
                ));
            }
            let curve = Curve::Polyline { points: points.clone(), closed };
            let (id, outcome) = insert_curve(doc, id, curve, "polyline");
            Ok((
                Command::Polyline { id: Some(id), points, closed },
                Inverse::DeleteCreated(vec![id]),
                outcome,
            ))
        }
        Command::Rectangle { id, corner, width, height } => {
            if width <= 0.0 || height <= 0.0 {
                return Err(ExecError::Invalid("rect width/height must be positive".into()));
            }
            let c = corner;
            let curve = Curve::Polyline {
                points: vec![
                    c,
                    c + DVec3::new(width, 0.0, 0.0),
                    c + DVec3::new(width, height, 0.0),
                    c + DVec3::new(0.0, height, 0.0),
                ],
                closed: true,
            };
            let (id, outcome) = insert_curve(doc, id, curve, "rect");
            Ok((
                Command::Rectangle { id: Some(id), corner, width, height },
                Inverse::DeleteCreated(vec![id]),
                outcome,
            ))
        }
        Command::Circle { id, center, radius } => {
            if radius <= 0.0 {
                return Err(ExecError::Invalid("circle radius must be positive".into()));
            }
            let curve = Curve::Arc { center, radius, start: 0.0, end: std::f64::consts::TAU };
            let (id, outcome) = insert_curve(doc, id, curve, "circle");
            Ok((
                Command::Circle { id: Some(id), center, radius },
                Inverse::DeleteCreated(vec![id]),
                outcome,
            ))
        }
        Command::Arc { id, center, radius, start_deg, end_deg } => {
            if radius <= 0.0 {
                return Err(ExecError::Invalid("arc radius must be positive".into()));
            }
            let curve = Curve::Arc {
                center,
                radius,
                start: start_deg.to_radians(),
                end: end_deg.to_radians(),
            };
            let (id, outcome) = insert_curve(doc, id, curve, "arc");
            Ok((
                Command::Arc { id: Some(id), center, radius, start_deg, end_deg },
                Inverse::DeleteCreated(vec![id]),
                outcome,
            ))
        }
        Command::Ellipse { id, center, rx, ry } => {
            if rx <= 0.0 || ry <= 0.0 {
                return Err(ExecError::Invalid("ellipse radii must be positive".into()));
            }
            let curve = Curve::Ellipse { center, rx, ry };
            let (id, outcome) = insert_curve(doc, id, curve, "ellipse");
            Ok((
                Command::Ellipse { id: Some(id), center, rx, ry },
                Inverse::DeleteCreated(vec![id]),
                outcome,
            ))
        }
        Command::Polygon { id, center, radius, sides } => {
            if sides < 3 {
                return Err(ExecError::Invalid("polygon needs at least 3 sides".into()));
            }
            if radius <= 0.0 {
                return Err(ExecError::Invalid("polygon radius must be positive".into()));
            }
            let points = (0..sides)
                .map(|i| {
                    let t = std::f64::consts::TAU * f64::from(i) / f64::from(sides);
                    center + DVec3::new(radius * t.cos(), radius * t.sin(), 0.0)
                })
                .collect();
            let curve = Curve::Polyline { points, closed: true };
            let (id, outcome) = insert_curve(doc, id, curve, "polygon");
            Ok((
                Command::Polygon { id: Some(id), center, radius, sides },
                Inverse::DeleteCreated(vec![id]),
                outcome,
            ))
        }
        Command::Curve { id, points, degree } => {
            if points.len() < 2 {
                return Err(ExecError::Invalid("curve needs at least 2 control points".into()));
            }
            let degree = (degree as usize).clamp(1, points.len() - 1);
            let curve = Curve::Nurbs {
                control: points.clone(),
                weights: vec![1.0; points.len()],
                knots: clamped_uniform_knots(points.len(), degree),
                degree,
            };
            let (id, outcome) = insert_curve(doc, id, curve, "curve");
            Ok((
                Command::Curve { id: Some(id), points, degree: degree as u32 },
                Inverse::DeleteCreated(vec![id]),
                outcome,
            ))
        }
        Command::Union { id, targets } => {
            let ids = resolve(doc, &targets)?;
            if ids.len() < 2 {
                return Err(ExecError::Invalid(
                    "union needs at least 2 meshes (selector matched 1)".into(),
                ));
            }
            let meshes = boolean_inputs(doc, &ids)?;
            let result = fold_csg(meshes, kernel_mesh::csg_union);
            let (id, inverse, message) =
                replace_with_result(doc, id, &ids, result, None, "union")?;
            Ok((
                Command::Union { id: Some(id), targets },
                inverse,
                ApplyOutcome { created: vec![id], message },
            ))
        }
        Command::Difference { id, target, tools } => {
            let tool_ids = resolve(doc, &tools)?;
            // Tools win overlaps, so "difference last 2 last" reads naturally:
            // targets = the two most recent minus the tool = the older one.
            let target_ids: Vec<ObjectId> = resolve(doc, &target)?
                .into_iter()
                .filter(|id| !tool_ids.contains(id))
                .collect();
            if target_ids.is_empty() {
                return Err(ExecError::Invalid(
                    "difference target selector matched only the tools".into(),
                ));
            }
            let mut all_ids = target_ids.clone();
            all_ids.extend(&tool_ids);
            let meshes = boolean_inputs(doc, &all_ids)?;
            let mut iter = meshes.into_iter();
            let mut base = iter.next().expect("target present");
            for _ in 1..target_ids.len() {
                base = kernel_mesh::csg_union(&base, &iter.next().expect("counted"));
            }
            let tool = fold_csg(iter.collect(), kernel_mesh::csg_union);
            let result = kernel_mesh::csg_difference(&base, &tool);
            // The result keeps the target's name — natural for the LLM
            // ("tower" with a hole is still "tower").
            let name = doc.get(target_ids[0]).expect("resolved").name.clone();
            let (id, inverse, message) =
                replace_with_result(doc, id, &all_ids, result, name, "difference")?;
            Ok((
                Command::Difference { id: Some(id), target, tools },
                inverse,
                ApplyOutcome { created: vec![id], message },
            ))
        }
        Command::Intersect { id, targets } => {
            let ids = resolve(doc, &targets)?;
            if ids.len() < 2 {
                return Err(ExecError::Invalid(
                    "intersect needs at least 2 meshes (selector matched 1)".into(),
                ));
            }
            let meshes = boolean_inputs(doc, &ids)?;
            let result = fold_csg(meshes, kernel_mesh::csg_intersection);
            let (id, inverse, message) =
                replace_with_result(doc, id, &ids, result, None, "intersection")?;
            Ok((
                Command::Intersect { id: Some(id), targets },
                inverse,
                ApplyOutcome { created: vec![id], message },
            ))
        }
        Command::Move { targets, delta } => {
            let ids = resolve(doc, &targets)?;
            for id in &ids {
                doc.get_mut(*id).expect("resolved").geometry.translate(delta);
            }
            Ok((
                Command::Move { targets, delta },
                Inverse::MoveBack { ids: ids.clone(), delta },
                ApplyOutcome {
                    created: Vec::new(),
                    message: format!("moved {} object(s) by {delta}", ids.len()),
                },
            ))
        }
        Command::Rotate { targets, angle_deg, axis, center } => {
            let ids = resolve(doc, &targets)?;
            let axis_n = axis.normalize_or_zero();
            if axis_n == DVec3::ZERO {
                return Err(ExecError::Invalid("rotate axis must be non-zero".into()));
            }
            let linear = glam::DMat4::from_axis_angle(axis_n, angle_deg.to_radians());
            let (inverse, tessellated) = apply_about_center(doc, &ids, center, linear);
            Ok((
                Command::Rotate { targets, angle_deg, axis, center },
                inverse,
                ApplyOutcome {
                    created: Vec::new(),
                    message: format!(
                        "rotated {} object(s) {angle_deg}°{}",
                        ids.len(),
                        tessellation_note(tessellated)
                    ),
                },
            ))
        }
        Command::Scale { targets, factors, center } => {
            if factors.x.abs() < 1e-12 || factors.y.abs() < 1e-12 || factors.z.abs() < 1e-12 {
                return Err(ExecError::Invalid(format!(
                    "scale factors must be non-zero, got {factors}"
                )));
            }
            let ids = resolve(doc, &targets)?;
            let linear = glam::DMat4::from_scale(factors);
            let (inverse, tessellated) = apply_about_center(doc, &ids, center, linear);
            Ok((
                Command::Scale { targets, factors, center },
                inverse,
                ApplyOutcome {
                    created: Vec::new(),
                    message: format!(
                        "scaled {} object(s) by {factors}{}",
                        ids.len(),
                        tessellation_note(tessellated)
                    ),
                },
            ))
        }
        Command::Mirror { targets, plane } => {
            let ids = resolve(doc, &targets)?;
            let (point, normal) = match &plane {
                MirrorPlane::Xy => (DVec3::ZERO, DVec3::Z),
                MirrorPlane::Yz => (DVec3::ZERO, DVec3::X),
                MirrorPlane::Xz => (DVec3::ZERO, DVec3::Y),
                MirrorPlane::PointNormal { point, normal } => (*point, *normal),
            };
            let n = normal.normalize_or_zero();
            if n == DVec3::ZERO {
                return Err(ExecError::Invalid("mirror normal must be non-zero".into()));
            }
            // Householder reflection I - 2nnᵀ across the plane through `point`.
            let h = glam::DMat4::from_cols(
                (DVec3::X - 2.0 * n.x * n).extend(0.0),
                (DVec3::Y - 2.0 * n.y * n).extend(0.0),
                (DVec3::Z - 2.0 * n.z * n).extend(0.0),
                glam::DVec4::W,
            );
            let (inverse, tessellated) = apply_about_center(doc, &ids, Some(point), h);
            Ok((
                Command::Mirror { targets, plane },
                inverse,
                ApplyOutcome {
                    created: Vec::new(),
                    message: format!(
                        "mirrored {} object(s){}",
                        ids.len(),
                        tessellation_note(tessellated)
                    ),
                },
            ))
        }
        Command::Offset { id, target, distance } => {
            let ids = resolve(doc, &target)?;
            if ids.len() != 1 {
                return Err(ExecError::Invalid(format!(
                    "offset selector matched {} objects, expected exactly 1",
                    ids.len()
                )));
            }
            let src = doc.get(ids[0]).expect("resolved");
            let Geometry::Curve(curve) = &src.geometry else {
                return Err(ExecError::Invalid(
                    "offset works on curves; meshes cannot be offset".into(),
                ));
            };
            let offset = curve.offset(distance, PROFILE_TOL).ok_or_else(|| {
                ExecError::Invalid(format!(
                    "offset by {distance} collapses the curve — use a smaller inward distance"
                ))
            })?;
            let exact = !matches!(
                (curve, &offset),
                (Curve::Ellipse { .. } | Curve::Nurbs { .. }, Curve::Polyline { .. })
            );
            let id = id.unwrap_or_default();
            doc.insert(SceneObject {
                id,
                name: None,
                geometry: Geometry::Curve(offset),
            });
            Ok((
                Command::Offset { id: Some(id), target, distance },
                Inverse::DeleteCreated(vec![id]),
                ApplyOutcome {
                    created: vec![id],
                    message: format!(
                        "offset {} by {distance} -> {id} (original kept{})",
                        ids[0],
                        if exact { "" } else { ", result tessellated to a polyline" }
                    ),
                },
            ))
        }
        Command::Copy { ids, targets, delta } => {
            let src = resolve(doc, &targets)?;
            // Reuse logged ids on replay; mint new ones live.
            let new_ids: Vec<ObjectId> = match ids {
                Some(ids) if ids.len() == src.len() => ids,
                _ => src.iter().map(|_| ObjectId::new()).collect(),
            };
            for (src_id, new_id) in src.iter().zip(&new_ids) {
                let mut obj = doc.get(*src_id).expect("resolved").clone();
                obj.id = *new_id;
                obj.geometry.translate(delta);
                doc.insert(obj);
            }
            Ok((
                Command::Copy { ids: Some(new_ids.clone()), targets, delta },
                Inverse::DeleteCreated(new_ids.clone()),
                ApplyOutcome {
                    message: format!("copied {} object(s)", new_ids.len()),
                    created: new_ids,
                },
            ))
        }
        Command::Delete { targets } => {
            let ids = resolve(doc, &targets)?;
            let mut removed = Vec::new();
            for id in &ids {
                if let Some(pair) = doc.remove(*id) {
                    removed.push(pair);
                }
            }
            Ok((
                Command::Delete { targets },
                Inverse::Restore(removed),
                ApplyOutcome {
                    created: Vec::new(),
                    message: format!("deleted {} object(s)", ids.len()),
                },
            ))
        }
        Command::Name { targets, name } => {
            let ids = resolve(doc, &targets)?;
            let mut prev = Vec::new();
            for id in &ids {
                let obj = doc.get_mut(*id).expect("resolved");
                prev.push((*id, obj.name.clone()));
                obj.name = Some(name.clone());
            }
            Ok((
                Command::Name { targets, name: name.clone() },
                Inverse::Rename(prev),
                ApplyOutcome {
                    created: Vec::new(),
                    message: format!("named {} object(s) '{name}'", ids.len()),
                },
            ))
        }
        Command::Select { targets } => {
            let ids = resolve(doc, &targets)?;
            doc.selection = ids.iter().copied().collect();
            let n = ids.len();
            Ok((
                Command::Select { targets },
                Inverse::Rename(Vec::new()), // never logged; inverse unused
                ApplyOutcome {
                    created: Vec::new(),
                    message: format!("selected {n} object(s)"),
                },
            ))
        }
        Command::SelectNone => {
            doc.selection.clear();
            Ok((
                Command::SelectNone,
                Inverse::Rename(Vec::new()),
                ApplyOutcome {
                    created: Vec::new(),
                    message: "selection cleared".into(),
                },
            ))
        }
        Command::Undo | Command::Redo => unreachable!("handled in Session::run"),
    }
}

fn describe(cmd: &Command) -> &'static str {
    match cmd {
        Command::Box { .. } => "box",
        Command::Extrude { .. } => "extrude",
        Command::Line { .. } => "line",
        Command::Polyline { .. } => "polyline",
        Command::Rectangle { .. } => "rect",
        Command::Circle { .. } => "circle",
        Command::Arc { .. } => "arc",
        Command::Ellipse { .. } => "ellipse",
        Command::Polygon { .. } => "polygon",
        Command::Curve { .. } => "curve",
        Command::Union { .. } => "union",
        Command::Difference { .. } => "difference",
        Command::Intersect { .. } => "intersect",
        Command::Move { .. } => "move",
        Command::Rotate { .. } => "rotate",
        Command::Scale { .. } => "scale",
        Command::Mirror { .. } => "mirror",
        Command::Offset { .. } => "offset",
        Command::Copy { .. } => "copy",
        Command::Delete { .. } => "delete",
        Command::Name { .. } => "name",
        Command::Select { .. } => "select",
        Command::SelectNone => "selectnone",
        Command::Undo => "undo",
        Command::Redo => "redo",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse;

    fn run(s: &mut Session, line: &str) -> ApplyOutcome {
        s.run(parse(line).unwrap()).unwrap()
    }

    #[test]
    fn create_move_delete_undo_redo() {
        let mut s = Session::default();
        run(&mut s, "box 0,0,0 5,5,3");
        assert_eq!(s.doc.len(), 1);
        let bb0 = s.doc.scene_aabb().unwrap();
        run(&mut s, "move last 1,2,0");
        let bb1 = s.doc.scene_aabb().unwrap();
        assert_eq!(bb1.min - bb0.min, glam::DVec3::new(1.0, 2.0, 0.0));
        run(&mut s, "delete last");
        assert_eq!(s.doc.len(), 0);
        run(&mut s, "undo"); // un-delete
        assert_eq!(s.doc.len(), 1);
        run(&mut s, "undo"); // un-move
        assert_eq!(s.doc.scene_aabb().unwrap().min, bb0.min);
        run(&mut s, "undo"); // un-create
        assert_eq!(s.doc.len(), 0);
        run(&mut s, "redo");
        run(&mut s, "redo");
        run(&mut s, "redo");
        assert_eq!(s.doc.len(), 0); // ends at deleted state
        run(&mut s, "undo");
        assert_eq!(s.doc.len(), 1);
    }

    #[test]
    fn copy_and_named_selectors() {
        let mut s = Session::default();
        run(&mut s, "box 0,0,0 2,2,2");
        run(&mut s, "name last base");
        run(&mut s, "copy base 5,0,0");
        assert_eq!(s.doc.len(), 2);
        // the copy inherits the name, so 'base' now matches both
        run(&mut s, "delete base");
        assert_eq!(s.doc.len(), 0);
    }

    #[test]
    fn extrude_rect_profile() {
        let mut s = Session::default();
        run(&mut s, "rect 0,0,0 4 6");
        run(&mut s, "extrude last 3");
        assert_eq!(s.doc.len(), 2); // profile kept + mesh
        let bb = s.doc.scene_aabb().unwrap();
        assert_eq!(bb.size(), glam::DVec3::new(4.0, 6.0, 3.0));
    }

    #[test]
    fn extrude_rejects_open_curve() {
        let mut s = Session::default();
        run(&mut s, "line 0,0,0 5,0,0");
        let err = s.run(parse("extrude last 3").unwrap()).unwrap_err();
        assert!(err.to_string().contains("closed"), "{err}");
    }

    #[test]
    fn replay_reproduces_identical_document() {
        let mut s = Session::default();
        run(&mut s, "box 0,0,0 5,5,3");
        run(&mut s, "circle 10,0,0 2");
        run(&mut s, "extrude last 4");
        run(&mut s, "copy all 0,10,0");
        run(&mut s, "move last 2 0,0,1");
        run(&mut s, "polygon 20,0,0 3 6");
        run(&mut s, "delete last");
        run(&mut s, "undo");

        let log = s.save_log();
        let replayed = Session::replay(log.clone()).unwrap();
        let a: Vec<_> = s.doc.objects().collect();
        let b: Vec<_> = replayed.doc.objects().collect();
        assert_eq!(a.len(), b.len());
        for (x, y) in a.iter().zip(&b) {
            assert_eq!(x, y);
        }
        // and the log itself is stable across replay
        assert_eq!(
            serde_json::to_string(&log).unwrap(),
            serde_json::to_string(&replayed.save_log()).unwrap()
        );
    }

    #[test]
    fn difference_consumes_inputs_and_undoes() {
        let mut s = Session::default();
        run(&mut s, "box 0,0,0 10,10,3");
        run(&mut s, "name last slab");
        run(&mut s, "box 3,3,-1 4,4,5");
        run(&mut s, "difference slab last");
        assert_eq!(s.doc.len(), 1);
        let obj = s.doc.objects().next().unwrap();
        assert_eq!(obj.name.as_deref(), Some("slab")); // result inherits target name
        let Geometry::Mesh(m) = &obj.geometry else { panic!("expected mesh") };
        assert!((kernel_mesh::signed_volume(m) - (300.0 - 48.0)).abs() < 1e-6);

        run(&mut s, "undo");
        assert_eq!(s.doc.len(), 2); // both inputs restored
        let result_id = {
            run(&mut s, "redo");
            assert_eq!(s.doc.len(), 1);
            s.doc.objects().next().unwrap().id
        };
        // redo reproduces the same result id
        run(&mut s, "undo");
        run(&mut s, "redo");
        assert_eq!(s.doc.objects().next().unwrap().id, result_id);
    }

    #[test]
    fn union_and_intersect() {
        let mut s = Session::default();
        run(&mut s, "box 0,0,0 2,2,2");
        run(&mut s, "box 1,1,1 2,2,2");
        run(&mut s, "union last 2");
        assert_eq!(s.doc.len(), 1);
        let Geometry::Mesh(m) = &s.doc.objects().next().unwrap().geometry else {
            panic!("expected mesh")
        };
        assert!((kernel_mesh::signed_volume(m) - 15.0).abs() < 1e-6);

        run(&mut s, "undo");
        run(&mut s, "intersect last 2");
        let Geometry::Mesh(m) = &s.doc.objects().next().unwrap().geometry else {
            panic!("expected mesh")
        };
        assert!((kernel_mesh::signed_volume(m) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn boolean_rejects_curves_and_disjoint_intersect() {
        let mut s = Session::default();
        run(&mut s, "box 0,0,0 1,1,1");
        run(&mut s, "circle 5,5,0 1");
        let err = s.run(parse("union last 2").unwrap()).unwrap_err();
        assert!(err.to_string().contains("extrude it first"), "{err}");

        run(&mut s, "delete last"); // drop the circle
        run(&mut s, "box 10,10,10 1,1,1");
        let err = s.run(parse("intersect last 2").unwrap()).unwrap_err();
        assert!(err.to_string().contains("do not overlap"), "{err}");
        assert_eq!(s.doc.len(), 2); // failed boolean leaves the doc untouched
    }

    #[test]
    fn boolean_replay_stable() {
        let mut s = Session::default();
        run(&mut s, "box 0,0,0 10,10,3");
        run(&mut s, "box 3,3,-1 4,4,5");
        run(&mut s, "difference last 2 last");
        let log = s.save_log();
        let replayed = Session::replay(log.clone()).unwrap();
        let a: Vec<_> = s.doc.objects().collect();
        let b: Vec<_> = replayed.doc.objects().collect();
        assert_eq!(a, b);
    }

    #[test]
    fn rotate_about_center_and_undo() {
        let mut s = Session::default();
        run(&mut s, "box 0,0,0 4,2,1");
        let bb0 = s.doc.scene_aabb().unwrap();
        run(&mut s, "rotate last 90"); // about own center, z axis
        let bb1 = s.doc.scene_aabb().unwrap();
        // 4x2 footprint becomes 2x4 around the same center
        assert!((bb1.size() - glam::DVec3::new(2.0, 4.0, 1.0)).length() < 1e-9);
        assert!((bb1.center() - bb0.center()).length() < 1e-9);
        run(&mut s, "undo");
        assert_eq!(s.doc.scene_aabb().unwrap().min, bb0.min);
    }

    #[test]
    fn scale_per_axis_about_point() {
        let mut s = Session::default();
        run(&mut s, "box 0,0,0 2,2,2");
        run(&mut s, "scale last 1,1,3 about 0,0,0");
        let bb = s.doc.scene_aabb().unwrap();
        assert_eq!(bb.size(), glam::DVec3::new(2.0, 2.0, 6.0));
        assert_eq!(bb.min, glam::DVec3::ZERO); // anchored at origin

        let err = s.run(parse("scale last 0").unwrap()).unwrap_err();
        assert!(err.to_string().contains("non-zero"), "{err}");
    }

    #[test]
    fn mirror_keeps_volume_positive() {
        let mut s = Session::default();
        run(&mut s, "box 1,0,0 2,2,2");
        run(&mut s, "mirror last yz");
        let bb = s.doc.scene_aabb().unwrap();
        assert_eq!(bb.min.x, -3.0); // reflected across x=0
        let Geometry::Mesh(m) = &s.doc.objects().next().unwrap().geometry else {
            panic!("expected mesh")
        };
        // winding flipped back → outward normals → positive volume
        assert!((kernel_mesh::signed_volume(m) - 8.0).abs() < 1e-9);
    }

    #[test]
    fn rotate_arc_stays_arc_mirror_tessellates() {
        let mut s = Session::default();
        run(&mut s, "arc 0,0,0 5 0 90");
        run(&mut s, "rotate last 90 z about 0,0,0");
        let Geometry::Curve(c) = &s.doc.objects().next().unwrap().geometry else {
            panic!("expected curve")
        };
        assert!(matches!(c, kernel_curve::Curve::Arc { .. }));

        let out = run(&mut s, "mirror last xz");
        assert!(out.message.contains("tessellated"), "{}", out.message);
        let Geometry::Curve(c) = &s.doc.objects().next().unwrap().geometry else {
            panic!("expected curve")
        };
        assert!(matches!(c, kernel_curve::Curve::Polyline { .. }));

        run(&mut s, "undo"); // back to the rotated arc, exactly
        let Geometry::Curve(c) = &s.doc.objects().next().unwrap().geometry else {
            panic!("expected curve")
        };
        assert!(matches!(c, kernel_curve::Curve::Arc { .. }));
    }

    #[test]
    fn extrude_after_rotate_works() {
        let mut s = Session::default();
        run(&mut s, "rect 0,0,0 4 6");
        run(&mut s, "rotate last 45");
        run(&mut s, "extrude last 3");
        assert_eq!(s.doc.len(), 2);
    }

    #[test]
    fn transform_replay_stable() {
        let mut s = Session::default();
        run(&mut s, "box 0,0,0 4,2,1");
        run(&mut s, "rotate last 30");
        run(&mut s, "scale last 2 about 0,0,0");
        run(&mut s, "mirror last yz");
        run(&mut s, "circle 10,0,0 2");
        run(&mut s, "scale last 2,1,1"); // ellipse-ish: circle tessellates? (circle is Arc; non-uniform → polyline)
        let log = s.save_log();
        let replayed = Session::replay(log).unwrap();
        let a: Vec<_> = s.doc.objects().collect();
        let b: Vec<_> = replayed.doc.objects().collect();
        assert_eq!(a, b);
    }

    #[test]
    fn offset_walls_from_centerline() {
        let mut s = Session::default();
        run(&mut s, "rect 0,0,0 10 6");
        run(&mut s, "offset last 0.2"); // outward
        assert_eq!(s.doc.len(), 2); // original kept
        run(&mut s, "undo");
        assert_eq!(s.doc.len(), 1);
        run(&mut s, "offset last -0.2"); // inward
        let bb = s.doc.scene_aabb().unwrap();
        // inner offset shrinks the overall bounds only via the new curve? no —
        // original rect still bounds the scene at 10x6
        assert_eq!(bb.size().truncate(), glam::DVec2::new(10.0, 6.0));

        // collapse errors, doc untouched
        let err = s.run(parse("offset last -5").unwrap()).unwrap_err();
        assert!(err.to_string().contains("collapses"), "{err}");

        // meshes rejected
        run(&mut s, "box 20,0,0 1,1,1");
        let err = s.run(parse("offset last 1").unwrap()).unwrap_err();
        assert!(err.to_string().contains("curves"), "{err}");
    }

    #[test]
    fn offset_replay_stable() {
        let mut s = Session::default();
        run(&mut s, "circle 0,0,0 3");
        run(&mut s, "name last centerline");
        run(&mut s, "offset centerline 0.5");
        run(&mut s, "offset centerline -0.5");
        let log = s.save_log();
        let replayed = Session::replay(log).unwrap();
        let a: Vec<_> = s.doc.objects().collect();
        let b: Vec<_> = replayed.doc.objects().collect();
        assert_eq!(a, b);
    }

    #[test]
    fn undo_not_saved_in_log() {
        let mut s = Session::default();
        run(&mut s, "box 0,0,0 1,1,1");
        run(&mut s, "box 5,0,0 1,1,1");
        run(&mut s, "undo");
        let log = s.save_log();
        assert_eq!(log.len(), 1); // only the surviving box
    }
}
