use glam::DVec3;
use kernel_curve::{clamped_uniform_knots, Curve};
use kernel_mesh::extrude_profile;
use mydrafter_doc::{
    format_area, format_length, format_volume, Annotation, Document, Geometry, LayerStyle,
    NamedView, ObjectId, SceneObject, ScheduleRow, SheetDim, SheetTable, Underlay, Units,
};

use crate::error::ExecError;
use crate::{Command, CompassDir, MirrorPlane, Selector};

/// Chord tolerance used when tessellating profile curves for extrusion.
const PROFILE_TOL: f64 = 0.01;

/// Pixel aspect ratio (width / height) of a raster file, or `None` if it can't
/// be read. Only the header is decoded, so this is cheap.
fn image_aspect(path: &str) -> Option<f64> {
    let (w, h) = image::image_dimensions(path).ok()?;
    (h > 0).then(|| w as f64 / h as f64)
}

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
    /// `fillet`: delete the created arc, restore the trimmed curves.
    CreatedAndGeometry {
        created: Vec<ObjectId>,
        snapshots: Vec<(ObjectId, Geometry)>,
    },
    /// `section`/`plan`/`elevation`: delete the created loops, dropping any
    /// layers this command created ("sections", "sections-proj", "elevations").
    CreatedOnLayer {
        created: Vec<ObjectId>,
        layers_created: Vec<String>,
    },
    /// `layer`: restore the previous current layer, dropping the layer this
    /// command created (if any).
    LayerCurrent {
        prev: String,
        created: Option<String>,
    },
    /// `tolayer`: put objects back on their previous layers.
    ObjectLayers {
        prev: Vec<(ObjectId, String)>,
        created: Option<String>,
    },
    /// `layercolor`/`hide`/`show`: restore the previous layer style.
    LayerStyle { layer: String, prev: LayerStyle },
    /// `hideobj`/`showobj`: restore each object's previous visible flag.
    ObjectVisibility(Vec<(ObjectId, bool)>),
    /// `color`/`coloroff`: restore each object's previous color override.
    ObjectColor(Vec<(ObjectId, Option<[f32; 3]>)>),
    /// `units`: restore the previous display unit.
    Units { prev: Units },
    /// `underlay`/`underlayopacity`/`underlayoff`: restore the previous underlay.
    Underlay { prev: Option<Underlay> },
    /// `sun`/`sunoff`: restore the previous solar position.
    Sun { prev: Option<mydrafter_doc::SunPosition> },
    /// `view save`: restore the previously saved view of that name (if any).
    ViewSaved {
        name: String,
        prev: Option<NamedView>,
    },
    /// `group`: drop the group, restoring the binding it overwrote (if any).
    GroupSet {
        name: String,
        prev: Option<std::collections::BTreeSet<ObjectId>>,
    },
    /// `ungroup`: put the dissolved groups back.
    RestoreGroups(Vec<(String, std::collections::BTreeSet<ObjectId>)>),
    /// `sheet`: drop the sheet this command created.
    RemoveSheet(String),
    /// `sheetview`: drop the view most recently added to a sheet.
    PopSheetView(String),
    /// `sheettable`: clear the table that was placed on a sheet.
    SheetTableRemoved(String),
    /// `sheetdim`: remove the last dim appended to a sheet.
    PopSheetDim(String),
    /// `block`: remove the block definition this command created/replaced.
    BlockDef {
        name: String,
        /// Previous definition (None if newly created).
        prev: Option<Vec<mydrafter_doc::BlockGeometry>>,
    },
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
            Command::Amend { step, with } => self.amend(step, *with),
            Command::Import { path } => self.import(path),
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
            Inverse::CreatedAndGeometry { created, snapshots } => {
                for id in created.clone() {
                    self.doc.remove(id);
                }
                for (id, geometry) in snapshots.clone() {
                    if let Some(obj) = self.doc.get_mut(id) {
                        obj.geometry = geometry;
                    }
                }
            }
            Inverse::CreatedOnLayer { created, layers_created } => {
                for id in created.clone() {
                    self.doc.remove(id);
                }
                for name in layers_created.clone() {
                    self.doc.layers.remove(&name);
                    self.doc.generation += 1;
                }
            }
            Inverse::LayerCurrent { prev, created } => {
                if let Some(name) = created {
                    self.doc.layers.remove(name);
                }
                self.doc.current_layer = prev.clone();
                self.doc.generation += 1;
            }
            Inverse::ObjectLayers { prev, created } => {
                let created = created.clone();
                for (id, layer) in prev.clone() {
                    if let Some(obj) = self.doc.get_mut(id) {
                        obj.layer = layer;
                    }
                }
                if let Some(name) = created {
                    self.doc.layers.remove(&name);
                }
                self.doc.generation += 1;
            }
            Inverse::LayerStyle { layer, prev } => {
                if let Some(style) = self.doc.layers.get_mut(layer) {
                    *style = prev.clone();
                }
                self.doc.generation += 1;
            }
            Inverse::ObjectVisibility(prev) => {
                for (id, visible) in prev.clone() {
                    if let Some(obj) = self.doc.get_mut(id) {
                        obj.visible = visible;
                    }
                }
            }
            Inverse::ObjectColor(prev) => {
                for (id, color) in prev.clone() {
                    if let Some(obj) = self.doc.get_mut(id) {
                        obj.color = color;
                    }
                }
                self.doc.generation += 1;
            }
            Inverse::Units { prev } => {
                self.doc.units = *prev;
                self.doc.generation += 1;
            }
            Inverse::Underlay { prev } => {
                self.doc.underlay = prev.clone();
                self.doc.generation += 1;
            }
            Inverse::Sun { prev } => {
                self.doc.sun = *prev;
                self.doc.generation += 1;
            }
            Inverse::ViewSaved { name, prev } => {
                match prev {
                    Some(view) => self.doc.named_views.insert(name.clone(), *view),
                    None => self.doc.named_views.remove(name),
                };
                self.doc.generation += 1;
            }
            Inverse::GroupSet { name, prev } => {
                match prev {
                    Some(members) => self.doc.groups.insert(name.clone(), members.clone()),
                    None => self.doc.groups.remove(name),
                };
                self.doc.generation += 1;
            }
            Inverse::RestoreGroups(groups) => {
                for (name, members) in groups.clone() {
                    self.doc.groups.insert(name, members);
                }
                self.doc.generation += 1;
            }
            Inverse::RemoveSheet(name) => {
                self.doc.sheets.retain(|s| &s.name != name);
                self.doc.generation += 1;
            }
            Inverse::PopSheetView(sheet) => {
                let sheet = sheet.clone();
                if let Some(s) = self.doc.sheet_mut(&sheet) {
                    s.views.pop();
                }
                self.doc.generation += 1;
            }
            Inverse::SheetTableRemoved(sheet) => {
                let sheet = sheet.clone();
                if let Some(s) = self.doc.sheet_mut(&sheet) {
                    s.table = None;
                }
                self.doc.generation += 1;
            }
            Inverse::PopSheetDim(sheet) => {
                let sheet = sheet.clone();
                if let Some(s) = self.doc.sheet_mut(&sheet) {
                    s.dims.pop();
                }
                self.doc.generation += 1;
            }
            Inverse::BlockDef { name, prev } => {
                match prev {
                    Some(defs) => {
                        self.doc.blocks.insert(name.clone(), defs.clone());
                    }
                    None => {
                        self.doc.blocks.remove(name);
                    }
                }
                self.doc.generation += 1;
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

    /// Read-only view for the history panel: one describe() entry per logged
    /// op (oldest first) plus the cursor. Step N = state after the first N ops.
    pub fn history(&self) -> (Vec<String>, usize) {
        (
            self.log.iter().map(|a| describe(&a.op).to_string()).collect(),
            self.cursor,
        )
    }

    /// Move the cursor to `step` by running undo/redo through `run` — no new
    /// mutation path, so the log stays consistent. Returns ops executed.
    pub fn jump_to(&mut self, step: usize) -> Result<usize, ExecError> {
        let step = step.min(self.log.len());
        let mut moved = 0usize;
        while self.cursor > step {
            self.run(Command::Undo)?;
            moved += 1;
        }
        while self.cursor < step {
            self.run(Command::Redo)?;
            moved += 1;
        }
        Ok(moved)
    }

    /// Replace the op at `step` (0-based, within the effective log) with
    /// `new_cmd` and rebuild by replaying every op through the normal apply
    /// path. Downstream ops resolve against the rebuilt state, so positional
    /// selectors ('last', 'all') follow the change. On any replay failure the
    /// session is left exactly as it was and the failing step is reported.
    pub fn amend(&mut self, step: usize, new_cmd: Command) -> Result<ApplyOutcome, ExecError> {
        if !new_cmd.is_logged() {
            return Err(ExecError::Invalid(format!(
                "'{}' is not a geometry command and cannot be amended into history",
                describe(&new_cmd)
            )));
        }
        let mut log = self.save_log();
        if step >= log.len() {
            return Err(ExecError::BadAmendStep { step, len: log.len() });
        }
        log[step] = new_cmd;
        let mut fresh = Session::default();
        for (i, cmd) in log.into_iter().enumerate() {
            let op = describe(&cmd).to_string();
            if let Err(e) = fresh.run(cmd) {
                return Err(ExecError::AmendReplay {
                    step: i,
                    op,
                    source: Box::new(e),
                });
            }
        }
        // Strictly advance the generation so GPU/journal caches keyed on the
        // old document never mistake the rebuilt one for it.
        fresh.doc.generation = fresh.doc.generation.max(self.doc.generation) + 1;
        let count = fresh.log.len();
        *self = fresh;
        Ok(ApplyOutcome {
            created: Vec::new(),
            message: format!(
                "amended step {step} to '{}'; replayed {count} op(s)",
                describe(&self.log[step].op)
            ),
        })
    }

    /// Import a file by dispatching on extension.
    ///
    /// - `.dxf` → expand into substrate ops (Line/Polyline/Circle/Arc/Text +
    ///   Layer switches), one logged op per entity.
    /// - `.obj` / `.stl` / `.gltf` / `.glb` → one `MeshLiteral` logged op per
    ///   named object in the file.
    fn import(&mut self, path: String) -> Result<ApplyOutcome, ExecError> {
        let ext = path.rsplit('.').next().map(|e| e.to_ascii_lowercase()).unwrap_or_default();
        match ext.as_str() {
            "dxf" => self.import_dxf(path),
            "obj" | "stl" | "gltf" | "glb" => self.import_mesh(path),
            "ifc" => self.import_ifc(path),
            other => Err(ExecError::Invalid(format!(
                "unknown import extension '.{other}' (supported: .dxf, .obj, .stl, .gltf, .glb, .ifc)"
            ))),
        }
    }

    fn import_dxf(&mut self, path: String) -> Result<ApplyOutcome, ExecError> {
        let text = std::fs::read_to_string(&path)
            .map_err(|e| ExecError::Invalid(format!("cannot read '{path}': {e}")))?;
        let parsed = crate::dxf::parse_dxf(&text)
            .map_err(|e| ExecError::Invalid(format!("'{path}': {e}")))?;
        let prev_layer = self.doc.current_layer.clone();
        let total = parsed.entities.len();
        let mut created = Vec::new();
        for (layer, cmd) in parsed.entities {
            if self.doc.current_layer != layer {
                self.run(Command::Layer { name: layer })?;
            }
            created.extend(self.run(cmd)?.created);
        }
        if self.doc.current_layer != prev_layer {
            self.run(Command::Layer { name: prev_layer })?;
        }
        Ok(ApplyOutcome {
            created,
            message: format!(
                "imported {total} entities from {path} ({} skipped) — one logged op each",
                parsed.skipped
            ),
        })
    }

    fn import_mesh(&mut self, path: String) -> Result<ApplyOutcome, ExecError> {
        let bytes = std::fs::read(&path)
            .map_err(|e| ExecError::Invalid(format!("cannot read '{path}': {e}")))?;
        let parts = crate::mesh_import::import(&path, &bytes)
            .map_err(|e| ExecError::Invalid(format!("'{path}': {e}")))?;
        if parts.is_empty() {
            return Err(ExecError::Invalid(format!("'{path}' contains no importable meshes")));
        }
        let total = parts.len();
        let mut created = Vec::new();
        for (name, mesh) in parts {
            let positions = mesh.positions().to_vec();
            let faces = mesh.faces().to_vec();
            let out = self.run(Command::MeshLiteral {
                id: None,
                positions,
                faces,
                name: Some(name),
            })?;
            created.extend(out.created);
        }
        Ok(ApplyOutcome {
            created,
            message: format!("imported {total} mesh(es) from {path} — one MeshLiteral op each"),
        })
    }

    /// Import an IFC4 (or IFC2x3) file: each recovered mesh becomes one
    /// `MeshLiteral` op on the `ifc` layer, so the op-log — not the IFC file —
    /// is the record. Storey/element names carry through as the object name.
    fn import_ifc(&mut self, path: String) -> Result<ApplyOutcome, ExecError> {
        let bytes = std::fs::read(&path)
            .map_err(|e| ExecError::Invalid(format!("cannot read '{path}': {e}")))?;
        let parts = crate::ifc::import(&bytes)
            .map_err(|e| ExecError::Invalid(format!("'{path}': {e}")))?;
        if parts.is_empty() {
            return Err(ExecError::Invalid(format!(
                "'{path}' contains no importable IFC geometry"
            )));
        }
        let prev_layer = self.doc.current_layer.clone();
        if self.doc.current_layer != "ifc" {
            self.run(Command::Layer { name: "ifc".to_string() })?;
        }
        let total = parts.len();
        let mut created = Vec::new();
        for (name, mesh) in parts {
            let positions = mesh.positions().to_vec();
            let faces = mesh.faces().to_vec();
            let out = self.run(Command::MeshLiteral {
                id: None,
                positions,
                faces,
                name: Some(name),
            })?;
            created.extend(out.created);
        }
        if self.doc.current_layer != prev_layer {
            self.run(Command::Layer { name: prev_layer })?;
        }
        Ok(ApplyOutcome {
            created,
            message: format!("imported {total} mesh(es) from {path} — one MeshLiteral op each"),
        })
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
        visible: true,
        id,
        name: None,
        layer: doc.current_layer.clone(),
        color: None,
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
                Geometry::Annotation(_) => Err(ExecError::Invalid(format!(
                    "'{id}' is an annotation; booleans need meshes"
                ))),
                Geometry::Instance { block, .. } => Err(ExecError::Invalid(format!(
                    "'{id}' is a block instance ('{block}'); booleans need meshes — explode the instance or extrude it first"
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
    layer: String,
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
        visible: true,
        id,
        name,
        layer,
        color: None,
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

/// Enclosed XY area of a closed loop by the shoelace formula. `points` must
/// not repeat the first point at the end (tessellate() guarantees this).
fn shoelace_area(points: &[DVec3]) -> f64 {
    let mut sum = 0.0;
    for (i, p) in points.iter().enumerate() {
        let q = &points[(i + 1) % points.len()];
        sum += p.x * q.y - q.x * p.y;
    }
    sum.abs() / 2.0
}

/// Total surface area of a mesh (sum of triangle areas).
fn mesh_surface_area(mesh: &kernel_mesh::Mesh) -> f64 {
    let pos = mesh.positions();
    mesh.faces()
        .iter()
        .map(|f| {
            let (a, b, c) = (pos[f[0] as usize], pos[f[1] as usize], pos[f[2] as usize]);
            (b - a).cross(c - a).length() / 2.0
        })
        .sum()
}

/// Build schedule rows for objects on the given layer (all layers if `None`).
/// Rows are ordered by creation order.
pub(crate) fn build_schedule_rows(doc: &Document, layer: Option<&str>) -> Vec<ScheduleRow> {
    doc.objects()
        .filter(|o| layer.is_none_or(|l| o.layer == l))
        .map(|o| {
            let kind = match &o.geometry {
                Geometry::Mesh(_) => "mesh",
                Geometry::Curve(_) => "curve",
                Geometry::Annotation(_) => "annotation",
                Geometry::Instance { .. } => "instance",
            };
            let area_m2 = match &o.geometry {
                Geometry::Curve(c) if c.is_closed() => {
                    shoelace_area(&c.tessellate(PROFILE_TOL))
                }
                Geometry::Mesh(m) => mesh_surface_area(m),
                _ => 0.0,
            };
            let volume_m3 = match &o.geometry {
                Geometry::Mesh(m) => kernel_mesh::signed_volume(m),
                _ => 0.0,
            };
            ScheduleRow {
                id: o.id.short(),
                name: o.name.clone().unwrap_or_else(|| o.id.short()),
                layer: o.layer.clone(),
                kind: kind.to_string(),
                area_m2,
                volume_m3,
            }
        })
        .collect()
}

/// Render schedule rows as an ASCII table for the command line.
pub(crate) fn format_schedule_table(rows: &[ScheduleRow], units: Units) -> String {
    // Column widths: name, id, layer, type, area, volume.
    const HDR: [&str; 6] = ["Name", "ID", "Layer", "Type", "Area", "Volume"];
    let (per_m, label) = units.per_meter();
    let area_label = format!("Area ({label}²)");
    let vol_label = format!("Vol ({label}³)");

    // Compute per-row cell text.
    let cells: Vec<[String; 6]> = rows
        .iter()
        .map(|r| {
            [
                r.name.clone(),
                r.id.clone(),
                r.layer.clone(),
                r.kind.clone(),
                format!("{:.2}", r.area_m2 * per_m * per_m),
                format!("{:.2}", r.volume_m3 * per_m * per_m * per_m),
            ]
        })
        .collect();

    let hdrs: [String; 6] = [
        HDR[0].to_string(),
        HDR[1].to_string(),
        HDR[2].to_string(),
        HDR[3].to_string(),
        area_label,
        vol_label,
    ];
    let mut widths = [0usize; 6];
    for (i, h) in hdrs.iter().enumerate() {
        widths[i] = h.len();
    }
    for row in &cells {
        for (i, cell) in row.iter().enumerate() {
            widths[i] = widths[i].max(cell.len());
        }
    }

    let sep: String = widths.iter().map(|w| "-".repeat(w + 2)).collect::<Vec<_>>().join("+");
    let sep = format!("+{}+", sep);

    let fmt_row = |cols: &[String; 6]| -> String {
        let inner: String = cols
            .iter()
            .enumerate()
            .map(|(i, c)| format!(" {:<w$} ", c, w = widths[i]))
            .collect::<Vec<_>>()
            .join("|");
        format!("|{inner}|")
    };

    let mut out = String::new();
    out.push_str(&sep);
    out.push('\n');
    out.push_str(&fmt_row(&hdrs));
    out.push('\n');
    out.push_str(&sep);
    out.push('\n');
    for row in &cells {
        out.push_str(&fmt_row(row));
        out.push('\n');
    }
    out.push_str(&sep);

    // Append grouped counts by name.
    let mut name_counts: Vec<(String, usize)> = Vec::new();
    for row in rows {
        if let Some(entry) = name_counts.iter_mut().find(|(n, _)| n == &row.name) {
            entry.1 += 1;
        } else {
            name_counts.push((row.name.clone(), 1));
        }
    }
    if !name_counts.is_empty() {
        out.push_str("\nCounts by name:");
        for (name, count) in &name_counts {
            out.push_str(&format!("\n  {name}: {count}"));
        }
    }
    out
}

/// Layer that section/plan cut loops land on (created on demand).
const SECTIONS_LAYER: &str = "sections";
/// Layer for projected feature edges below/beyond a cut (thin lineweight).
const SECTIONS_PROJ_LAYER: &str = "sections-proj";
/// Layer for elevation (pure-projection) outlines.
const ELEVATIONS_LAYER: &str = "elevations";
/// Heavy cut lineweight (ISO medium) vs thin projected-edge lineweight.
const CUT_WEIGHT_MM: f64 = 0.5;
const PROJ_WEIGHT_MM: f64 = 0.13;

/// Ensure `layer` exists with the given lineweight; records it in `created` if
/// this call minted it (for undo).
fn ensure_layer(doc: &mut Document, layer: &str, weight_mm: f64, created: &mut Vec<String>) {
    if !doc.layers.contains_key(layer) {
        doc.layers.insert(
            layer.to_string(),
            LayerStyle { lineweight_mm: weight_mm, ..LayerStyle::default() },
        );
        created.push(layer.to_string());
    }
}

/// Slice every mesh among `target_ids` with the plane, inserting each closed
/// loop as a closed polyline on "sections", plus the feature edges of geometry
/// behind the plane (below a plan cut / beyond a section) projected onto the
/// plane as open polylines on "sections-proj" (thin lineweight). Returns the
/// created ids, the layers this call created (for undo), and the mesh count.
fn section_meshes(
    doc: &mut Document,
    ids: Option<Vec<ObjectId>>,
    target_ids: &[ObjectId],
    point: DVec3,
    normal: DVec3,
) -> Result<(Vec<ObjectId>, Vec<String>, usize), ExecError> {
    if normal.length() < 1e-9 {
        return Err(ExecError::Invalid("section plane normal cannot be zero".into()));
    }
    let mut loops = Vec::new();
    let mut proj_edges: Vec<(DVec3, DVec3)> = Vec::new();
    let mut meshes = 0usize;
    for id in target_ids {
        if let Geometry::Mesh(m) = &doc.get(*id).expect("resolved").geometry {
            meshes += 1;
            loops.extend(kernel_mesh::slice(m, point, normal, PROFILE_TOL));
            proj_edges.extend(kernel_mesh::project_edges_behind(m, point, normal, PROFILE_TOL));
        }
    }
    if meshes == 0 {
        return Err(ExecError::Invalid(
            "section works on meshes; the selector matched none (extrude or box first)".into(),
        ));
    }
    if loops.is_empty() {
        return Err(ExecError::Invalid(format!(
            "the section plane misses the {meshes} selected mesh(es) — check the plane point/height"
        )));
    }
    // Reuse logged ids on replay; mint new ones live. Cut loops come first,
    // then one polyline per projected edge, so id order is stable.
    let total = loops.len() + proj_edges.len();
    let new_ids: Vec<ObjectId> = match ids {
        Some(ids) if ids.len() == total => ids,
        _ => (0..total).map(|_| ObjectId::new()).collect(),
    };
    let mut layers_created = Vec::new();
    ensure_layer(doc, SECTIONS_LAYER, CUT_WEIGHT_MM, &mut layers_created);
    if !proj_edges.is_empty() {
        ensure_layer(doc, SECTIONS_PROJ_LAYER, PROJ_WEIGHT_MM, &mut layers_created);
    }
    let mut id_iter = new_ids.iter();
    for points in loops {
        doc.insert(SceneObject {
            visible: true,
            id: *id_iter.next().expect("id per loop"),
            name: None,
            layer: SECTIONS_LAYER.to_string(),
            color: None,
            geometry: Geometry::Curve(Curve::Polyline { points, closed: true }),
        });
    }
    for (a, b) in proj_edges {
        doc.insert(SceneObject {
            visible: true,
            id: *id_iter.next().expect("id per edge"),
            name: None,
            layer: SECTIONS_PROJ_LAYER.to_string(),
            color: None,
            geometry: Geometry::Curve(Curve::Polyline { points: vec![a, b], closed: false }),
        });
    }
    Ok((new_ids, layers_created, meshes))
}

/// Vertical projection plane for a compass elevation: the outward-facing
/// normal (toward the viewer) and a point on the geometry's bounding face on
/// that side, pushed out by `depth`. `north` = the face you see standing to the
/// north looking south, so its normal points +Y. Falls back to the origin when
/// the scene has no bounds.
fn elevation_plane(
    doc: &Document,
    _target_ids: &[ObjectId],
    dir: CompassDir,
    depth: f64,
) -> (DVec3, DVec3) {
    let normal = match dir {
        CompassDir::North => DVec3::Y,
        CompassDir::South => -DVec3::Y,
        CompassDir::East => DVec3::X,
        CompassDir::West => -DVec3::X,
    };
    let Some(aabb) = doc.scene_aabb() else {
        return (normal * depth, normal);
    };
    // Only the coordinate along the normal matters (projection collapses the
    // rest). Pick the bounding extreme the viewer faces: the corner furthest in
    // the +normal direction. dot with a +1/-1 axis selects max or min.
    let extreme = if normal.max_element() > 0.0 { aabb.max } else { aabb.min };
    (extreme + normal * depth, normal)
}

/// Project the feature edges of every mesh among `target_ids` orthographically
/// onto the vertical elevation plane, emitting them as open polylines on the
/// "elevations" layer. Returns the created ids, layers created, mesh count.
fn elevation_meshes(
    doc: &mut Document,
    ids: Option<Vec<ObjectId>>,
    target_ids: &[ObjectId],
    point: DVec3,
    normal: DVec3,
) -> Result<(Vec<ObjectId>, Vec<String>, usize), ExecError> {
    let mut edges: Vec<(DVec3, DVec3)> = Vec::new();
    let mut meshes = 0usize;
    for id in target_ids {
        if let Geometry::Mesh(m) = &doc.get(*id).expect("resolved").geometry {
            meshes += 1;
            edges.extend(kernel_mesh::project_edges_onto(m, point, normal, PROFILE_TOL));
        }
    }
    if meshes == 0 {
        return Err(ExecError::Invalid(
            "elevation works on meshes; the document has none (extrude or box first)".into(),
        ));
    }
    if edges.is_empty() {
        return Err(ExecError::Invalid(
            "elevation produced no edges — the geometry has no feature edges facing the view".into(),
        ));
    }
    let new_ids: Vec<ObjectId> = match ids {
        Some(ids) if ids.len() == edges.len() => ids,
        _ => (0..edges.len()).map(|_| ObjectId::new()).collect(),
    };
    let mut layers_created = Vec::new();
    ensure_layer(doc, ELEVATIONS_LAYER, PROJ_WEIGHT_MM, &mut layers_created);
    for ((a, b), id) in edges.into_iter().zip(&new_ids) {
        doc.insert(SceneObject {
            visible: true,
            id: *id,
            name: None,
            layer: ELEVATIONS_LAYER.to_string(),
            color: None,
            geometry: Geometry::Curve(Curve::Polyline { points: vec![a, b], closed: false }),
        });
    }
    Ok((new_ids, layers_created, meshes))
}

/// Look up a layer for style edits; missing layers get an actionable error.
fn layer_style_mut<'a>(
    doc: &'a mut Document,
    layer: &str,
) -> Result<&'a mut LayerStyle, ExecError> {
    if !doc.layers.contains_key(layer) {
        let known: Vec<&str> = doc.layers.keys().map(String::as_str).collect();
        return Err(ExecError::Invalid(format!(
            "no layer '{layer}' (layers: {}; create one with: layer {layer})",
            known.join(", ")
        )));
    }
    Ok(doc.layers.get_mut(layer).expect("checked above"))
}

/// Resolve a selector to exactly one curve object.
fn one_curve<'a>(
    doc: &'a Document,
    sel: &Selector,
    verb: &str,
) -> Result<(ObjectId, &'a Curve), ExecError> {
    let ids = resolve(doc, sel)?;
    if ids.len() != 1 {
        return Err(ExecError::Invalid(format!(
            "{verb} selector matched {} objects, expected exactly 1",
            ids.len()
        )));
    }
    curve_of(doc, ids[0], verb).map(|c| (ids[0], c))
}

fn curve_of<'a>(doc: &'a Document, id: ObjectId, verb: &str) -> Result<&'a Curve, ExecError> {
    match &doc.get(id).expect("resolved").geometry {
        Geometry::Curve(c) => Ok(c),
        _ => Err(ExecError::Invalid(format!(
            "{verb} works on curves; '{id}' is not a curve"
        ))),
    }
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
                visible: true,
                id,
                name: None,
                layer: doc.current_layer.clone(),
                color: None,
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
                visible: true,
                id,
                name: None,
                layer: doc.current_layer.clone(),
                color: None,
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
        Command::Revolve { id, profile, axis_point, axis_dir, angle_deg } => {
            let (src_id, curve) = one_curve(doc, &profile, "revolve")?;
            if !curve.is_closed() {
                return Err(ExecError::BadProfile(
                    "curve is not closed (close it or use 'polyline ... closed')".into(),
                ));
            }
            let axis_pt = axis_point.unwrap_or(DVec3::ZERO);
            let dir = axis_dir.unwrap_or(DVec3::Z);
            if dir.length() < 1e-9 {
                return Err(ExecError::Invalid("revolve axis direction cannot be zero".into()));
            }
            let angle = angle_deg.unwrap_or(360.0);
            if !(angle > 0.0 && angle <= 360.0) {
                return Err(ExecError::Invalid(format!(
                    "revolve angle must be in (0, 360] degrees, got {angle}"
                )));
            }
            let pts = curve.tessellate(PROFILE_TOL);
            let mesh = kernel_mesh::revolve_profile(
                &pts,
                axis_pt,
                dir.normalize(),
                angle.to_radians(),
                PROFILE_TOL,
            );
            let id = id.unwrap_or_default();
            doc.insert(SceneObject {
                visible: true,
                id,
                name: None,
                layer: doc.current_layer.clone(),
                color: None,
                geometry: Geometry::Mesh(mesh),
            });
            Ok((
                Command::Revolve { id: Some(id), profile, axis_point, axis_dir, angle_deg },
                Inverse::DeleteCreated(vec![id]),
                ApplyOutcome {
                    created: vec![id],
                    message: format!("revolved {src_id} -> {id} ({angle} deg)"),
                },
            ))
        }
        Command::Loft { id, targets } => {
            let ids = resolve(doc, &targets)?;
            if ids.len() < 2 {
                return Err(ExecError::BadProfile(format!(
                    "loft needs at least 2 closed curves, selector matched {}",
                    ids.len()
                )));
            }
            let mut profiles = Vec::with_capacity(ids.len());
            for oid in &ids {
                let curve = curve_of(doc, *oid, "loft")?;
                if !curve.is_closed() {
                    return Err(ExecError::BadProfile(format!(
                        "'{oid}' is not closed; loft needs closed curves"
                    )));
                }
                profiles.push(curve.tessellate(PROFILE_TOL));
            }
            let mesh = kernel_mesh::loft_profiles(&profiles);
            let id = id.unwrap_or_default();
            doc.insert(SceneObject {
                visible: true,
                id,
                name: None,
                layer: doc.current_layer.clone(),
                color: None,
                geometry: Geometry::Mesh(mesh),
            });
            Ok((
                Command::Loft { id: Some(id), targets },
                Inverse::DeleteCreated(vec![id]),
                ApplyOutcome {
                    created: vec![id],
                    message: format!("lofted {} profiles -> {id}", ids.len()),
                },
            ))
        }
        Command::Sweep { id, profile, rail } => {
            let (profile_id, profile_curve) = one_curve(doc, &profile, "sweep")?;
            if !profile_curve.is_closed() {
                return Err(ExecError::BadProfile(
                    "sweep profile is not closed (close it or use 'polyline ... closed')".into(),
                ));
            }
            let profile_pts = profile_curve.tessellate(PROFILE_TOL);
            let (rail_id, rail_curve) = one_curve(doc, &rail, "sweep")?;
            if rail_curve.is_closed() {
                return Err(ExecError::Invalid(
                    "closed rails are not supported yet — use an open rail curve".into(),
                ));
            }
            let rail_pts = rail_curve.tessellate(PROFILE_TOL);
            if rail_pts.len() < 2 {
                return Err(ExecError::Invalid("rail is degenerate (needs 2+ points)".into()));
            }
            let mesh = kernel_mesh::sweep_profile(&profile_pts, &rail_pts);
            let id = id.unwrap_or_default();
            doc.insert(SceneObject {
                visible: true,
                id,
                name: None,
                layer: doc.current_layer.clone(),
                color: None,
                geometry: Geometry::Mesh(mesh),
            });
            Ok((
                Command::Sweep { id: Some(id), profile, rail },
                Inverse::DeleteCreated(vec![id]),
                ApplyOutcome {
                    created: vec![id],
                    message: format!("swept {profile_id} along {rail_id} -> {id}"),
                },
            ))
        }
        Command::Sweep2 { id, profile, rail_a, rail_b } => {
            let (profile_id, profile_curve) = one_curve(doc, &profile, "sweep2")?;
            if !profile_curve.is_closed() {
                return Err(ExecError::BadProfile(
                    "sweep2 profile is not closed (close it or use 'polyline ... closed')".into(),
                ));
            }
            let profile_pts = profile_curve.tessellate(PROFILE_TOL);
            let (a_id, a_curve) = one_curve(doc, &rail_a, "sweep2")?;
            let (b_id, b_curve) = one_curve(doc, &rail_b, "sweep2")?;
            let a_pts = a_curve.tessellate(PROFILE_TOL);
            let b_pts = b_curve.tessellate(PROFILE_TOL);
            if a_pts.len() < 2 || b_pts.len() < 2 {
                return Err(ExecError::Invalid("sweep2 rails need 2+ points each".into()));
            }
            let mesh = kernel_mesh::sweep2_profile(&profile_pts, &a_pts, &b_pts);
            let id = id.unwrap_or_default();
            doc.insert(SceneObject {
                visible: true,
                id,
                name: None,
                layer: doc.current_layer.clone(),
                color: None,
                geometry: Geometry::Mesh(mesh),
            });
            Ok((
                Command::Sweep2 { id: Some(id), profile, rail_a, rail_b },
                Inverse::DeleteCreated(vec![id]),
                ApplyOutcome {
                    created: vec![id],
                    message: format!("swept {profile_id} along {a_id} & {b_id} -> {id}"),
                },
            ))
        }
        Command::RailRevolve { id, profile, rail, axis_point, axis_dir } => {
            let (profile_id, profile_curve) = one_curve(doc, &profile, "railrevolve")?;
            if !profile_curve.is_closed() {
                return Err(ExecError::BadProfile(
                    "railrevolve profile is not closed (close it or use 'polyline ... closed')".into(),
                ));
            }
            if axis_dir.length() < 1e-9 {
                return Err(ExecError::Invalid("railrevolve axis direction cannot be zero".into()));
            }
            let profile_pts = profile_curve.tessellate(PROFILE_TOL);
            let (rail_id, rail_curve) = one_curve(doc, &rail, "railrevolve")?;
            let rail_pts = rail_curve.tessellate(PROFILE_TOL);
            if rail_pts.len() < 2 {
                return Err(ExecError::Invalid("railrevolve rail needs 2+ points".into()));
            }
            let mesh = kernel_mesh::rail_revolve_profile(
                &profile_pts,
                &rail_pts,
                axis_point,
                axis_dir.normalize(),
                PROFILE_TOL,
            );
            let id = id.unwrap_or_default();
            doc.insert(SceneObject {
                visible: true,
                id,
                name: None,
                layer: doc.current_layer.clone(),
                color: None,
                geometry: Geometry::Mesh(mesh),
            });
            Ok((
                Command::RailRevolve { id: Some(id), profile, rail, axis_point, axis_dir },
                Inverse::DeleteCreated(vec![id]),
                ApplyOutcome {
                    created: vec![id],
                    message: format!("rail-revolved {profile_id} along {rail_id} -> {id}"),
                },
            ))
        }
        Command::Pipe { id, curve, radius, end_radius } => {
            if radius <= 0.0 {
                return Err(ExecError::Invalid(format!(
                    "pipe radius must be positive, got {radius}"
                )));
            }
            let r1 = end_radius.unwrap_or(radius);
            if r1 <= 0.0 {
                return Err(ExecError::Invalid(format!(
                    "pipe end radius must be positive, got {r1}"
                )));
            }
            let (curve_id, curve_geom) = one_curve(doc, &curve, "pipe")?;
            let pts = curve_geom.tessellate(PROFILE_TOL);
            if pts.len() < 2 {
                return Err(ExecError::Invalid("pipe curve needs 2+ points".into()));
            }
            let mesh = kernel_mesh::pipe_curve(&pts, radius, r1, PROFILE_TOL);
            let id = id.unwrap_or_default();
            doc.insert(SceneObject {
                visible: true,
                id,
                name: None,
                layer: doc.current_layer.clone(),
                color: None,
                geometry: Geometry::Mesh(mesh),
            });
            Ok((
                Command::Pipe { id: Some(id), curve, radius, end_radius },
                Inverse::DeleteCreated(vec![id]),
                ApplyOutcome {
                    created: vec![id],
                    message: format!("piped {curve_id} -> {id}"),
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
        Command::InterpCurve { id, points, closed } => {
            if points.len() < 3 {
                return Err(ExecError::Invalid(
                    "interpcurve needs at least 3 points".into(),
                ));
            }
            let curve = kernel_curve::interpolate_curve(&points, closed).ok_or_else(|| {
                ExecError::Invalid("interpcurve: points are degenerate (coincident)".into())
            })?;
            let (id, outcome) = insert_curve(doc, id, curve, "interpcurve");
            Ok((
                Command::InterpCurve { id: Some(id), points, closed },
                Inverse::DeleteCreated(vec![id]),
                outcome,
            ))
        }
        Command::Helix { id, center, radius, height, turns } => {
            let curve = kernel_curve::helix(center, radius, height, turns).ok_or_else(|| {
                ExecError::Invalid("helix needs radius > 0 and turns != 0".into())
            })?;
            let (id, outcome) = insert_curve(doc, id, curve, "helix");
            Ok((
                Command::Helix { id: Some(id), center, radius, height, turns },
                Inverse::DeleteCreated(vec![id]),
                outcome,
            ))
        }
        Command::SetPoint { target, index, position } => {
            let ids = resolve(doc, &target)?;
            let [tid] = ids[..] else {
                return Err(ExecError::Invalid(format!(
                    "setpoint target matched {} objects, expected exactly 1",
                    ids.len()
                )));
            };
            let curve = curve_of(doc, tid, "setpoint")?.clone();
            let mut new = curve.clone();
            let i = index as usize;
            match &mut new {
                Curve::Nurbs { control, .. } => {
                    if i >= control.len() {
                        return Err(ExecError::Invalid(format!(
                            "setpoint index {i} out of range (curve has {} control points)",
                            control.len()
                        )));
                    }
                    control[i] = position;
                }
                Curve::Polyline { points, .. } => {
                    if i >= points.len() {
                        return Err(ExecError::Invalid(format!(
                            "setpoint index {i} out of range (polyline has {} points)",
                            points.len()
                        )));
                    }
                    points[i] = position;
                }
                _ => {
                    return Err(ExecError::Invalid(
                        "setpoint works on NURBS and polyline curves only".into(),
                    ))
                }
            }
            let obj = doc.get_mut(tid).expect("resolved");
            let snapshot = obj.geometry.clone();
            obj.geometry = Geometry::Curve(new);
            Ok((
                Command::SetPoint { target, index, position },
                Inverse::SetGeometry(vec![(tid, snapshot)]),
                ApplyOutcome {
                    created: Vec::new(),
                    message: format!("setpoint {tid} [{i}] -> {position}"),
                },
            ))
        }
        Command::Rebuild { id, target, count } => {
            let ids = resolve(doc, &target)?;
            let [tid] = ids[..] else {
                return Err(ExecError::Invalid(format!(
                    "rebuild target matched {} objects, expected exactly 1",
                    ids.len()
                )));
            };
            if count < 2 {
                return Err(ExecError::Invalid("rebuild count must be >= 2".into()));
            }
            let curve = curve_of(doc, tid, "rebuild")?;
            let rebuilt = kernel_curve::rebuild(curve, count as usize, PROFILE_TOL)
                .ok_or_else(|| ExecError::Invalid("rebuild: curve is degenerate".into()))?;
            let id = id.unwrap_or_default();
            let (obj, index) = doc.remove(tid).expect("resolved");
            doc.insert(SceneObject {
                visible: true,
                id,
                name: obj.name.clone(),
                layer: obj.layer.clone(),
                color: obj.color,
                geometry: Geometry::Curve(rebuilt),
            });
            Ok((
                Command::Rebuild { id: Some(id), target, count },
                Inverse::Replace { created: vec![id], consumed: vec![(obj, index)] },
                ApplyOutcome {
                    created: vec![id],
                    message: format!("rebuilt {tid} -> {id} ({count} points)"),
                },
            ))
        }
        Command::Dim { id, a, b, offset } => {
            let length = (b - a).length();
            if length < 1e-9 {
                return Err(ExecError::Invalid(
                    "dimension points must be distinct".into(),
                ));
            }
            let id = id.unwrap_or_default();
            doc.insert(SceneObject {
                visible: true,
                id,
                name: None,
                layer: doc.current_layer.clone(),
                color: None,
                geometry: Geometry::Annotation(Annotation::LinearDim { a, b, offset }),
            });
            Ok((
                Command::Dim { id: Some(id), a, b, offset },
                Inverse::DeleteCreated(vec![id]),
                ApplyOutcome {
                    created: vec![id],
                    message: format!("dim {id} ({})", format_length(doc.units, length)),
                },
            ))
        }
        Command::Text { id, pos, text, height } => {
            if height <= 0.0 {
                return Err(ExecError::Invalid("text height must be positive".into()));
            }
            if text.is_empty() {
                return Err(ExecError::Invalid("text needs a string".into()));
            }
            let id = id.unwrap_or_default();
            doc.insert(SceneObject {
                visible: true,
                id,
                name: None,
                layer: doc.current_layer.clone(),
                color: None,
                geometry: Geometry::Annotation(Annotation::Text {
                    pos,
                    text: text.clone(),
                    height,
                }),
            });
            Ok((
                Command::Text { id: Some(id), pos, text: text.clone(), height },
                Inverse::DeleteCreated(vec![id]),
                ApplyOutcome {
                    created: vec![id],
                    message: format!("text {id} ('{text}')"),
                },
            ))
        }
        Command::Hatch { id, target, pattern } => {
            let ids = resolve(doc, &target)?;
            if ids.len() != 1 {
                return Err(ExecError::Invalid(format!(
                    "hatch selector matched {} objects, expected exactly 1",
                    ids.len()
                )));
            }
            let src = doc.get(ids[0]).expect("resolved");
            let Geometry::Curve(curve) = &src.geometry else {
                return Err(ExecError::Invalid(
                    "hatch needs a closed curve boundary".into(),
                ));
            };
            if !curve.is_closed() {
                return Err(ExecError::Invalid(
                    "hatch boundary is not closed (close it or use 'polyline ... closed')".into(),
                ));
            }
            let pattern_spacing = match &pattern {
                mydrafter_doc::HatchPattern::Lines { spacing, .. }
                | mydrafter_doc::HatchPattern::Crosshatch { spacing, .. }
                | mydrafter_doc::HatchPattern::Brick { spacing }
                | mydrafter_doc::HatchPattern::Concrete { spacing }
                | mydrafter_doc::HatchPattern::Insulation { spacing }
                | mydrafter_doc::HatchPattern::Earth { spacing } => Some(*spacing),
                mydrafter_doc::HatchPattern::Solid => None,
            };
            if let Some(sp) = pattern_spacing
                && sp <= 0.0
            {
                return Err(ExecError::Invalid("hatch spacing must be positive".into()));
            }
            let boundary = curve.tessellate(PROFILE_TOL);
            let id = id.unwrap_or_default();
            doc.insert(SceneObject {
                visible: true,
                id,
                name: None,
                layer: doc.current_layer.clone(),
                color: None,
                geometry: Geometry::Annotation(Annotation::Hatch {
                    boundary,
                    pattern: pattern.clone(),
                }),
            });
            Ok((
                Command::Hatch { id: Some(id), target, pattern },
                Inverse::DeleteCreated(vec![id]),
                ApplyOutcome {
                    created: vec![id],
                    message: format!("hatched {} -> {id}", ids[0]),
                },
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
            // The result inherits the first input's layer.
            let layer = doc.get(ids[0]).expect("resolved").layer.clone();
            let (id, inverse, message) =
                replace_with_result(doc, id, &ids, result, None, layer, "union")?;
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
            // The result keeps the target's name and layer — natural for the
            // LLM ("tower" with a hole is still "tower").
            let target_obj = doc.get(target_ids[0]).expect("resolved");
            let name = target_obj.name.clone();
            let layer = target_obj.layer.clone();
            let (id, inverse, message) =
                replace_with_result(doc, id, &all_ids, result, name, layer, "difference")?;
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
            let layer = doc.get(ids[0]).expect("resolved").layer.clone();
            let (id, inverse, message) =
                replace_with_result(doc, id, &ids, result, None, layer, "intersection")?;
            Ok((
                Command::Intersect { id: Some(id), targets },
                inverse,
                ApplyOutcome { created: vec![id], message },
            ))
        }
        Command::Section { ids, targets, point, normal } => {
            let target_ids = resolve(doc, &targets)?;
            let (new_ids, layers_created, meshes) =
                section_meshes(doc, ids, &target_ids, point, normal)?;
            Ok((
                Command::Section { ids: Some(new_ids.clone()), targets, point, normal },
                Inverse::CreatedOnLayer { created: new_ids.clone(), layers_created },
                ApplyOutcome {
                    message: format!(
                        "sectioned {meshes} mesh(es) -> {} curve(s) on '{SECTIONS_LAYER}'/'{SECTIONS_PROJ_LAYER}'",
                        new_ids.len()
                    ),
                    created: new_ids,
                },
            ))
        }
        Command::Plan { ids, height } => {
            let target_ids = doc.all_ids();
            if target_ids.is_empty() {
                return Err(ExecError::EmptySelection("document has 0 objects".to_string()));
            }
            let (new_ids, layers_created, meshes) = section_meshes(
                doc,
                ids,
                &target_ids,
                DVec3::new(0.0, 0.0, height),
                DVec3::Z,
            )?;
            Ok((
                Command::Plan { ids: Some(new_ids.clone()), height },
                Inverse::CreatedOnLayer { created: new_ids.clone(), layers_created },
                ApplyOutcome {
                    message: format!(
                        "plan cut at z={height}: {} curve(s) from {meshes} mesh(es) on '{SECTIONS_LAYER}'/'{SECTIONS_PROJ_LAYER}'",
                        new_ids.len()
                    ),
                    created: new_ids,
                },
            ))
        }
        Command::Elevation { ids, direction, depth } => {
            let target_ids = doc.all_ids();
            if target_ids.is_empty() {
                return Err(ExecError::EmptySelection("document has 0 objects".to_string()));
            }
            let (point, normal) = elevation_plane(doc, &target_ids, direction, depth);
            let (new_ids, layers_created, meshes) =
                elevation_meshes(doc, ids, &target_ids, point, normal)?;
            Ok((
                Command::Elevation { ids: Some(new_ids.clone()), direction, depth },
                Inverse::CreatedOnLayer { created: new_ids.clone(), layers_created },
                ApplyOutcome {
                    message: format!(
                        "{direction} elevation: {} edge(s) from {meshes} mesh(es) on '{ELEVATIONS_LAYER}'",
                        new_ids.len()
                    ),
                    created: new_ids,
                },
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
        Command::Split { ids, target, point } => {
            let (tid, curve) = one_curve(doc, &target, "split")?;
            let cp = kernel_curve::closest_point(curve, point, PROFILE_TOL);
            let pieces = kernel_curve::split_at_points(curve, &[cp], kernel_curve::JOIN_TOL)
                .ok_or_else(|| {
                    ExecError::Invalid(
                        "cannot split this curve: closed curves need 2+ cuts (use trim), \
                         and NURBS/ellipse splitting is not supported yet"
                            .into(),
                    )
                })?;
            if pieces.len() < 2 {
                return Err(ExecError::Invalid(
                    "split point falls on the curve's end — nothing to split".into(),
                ));
            }
            let new_ids: Vec<ObjectId> = match ids {
                Some(ids) if ids.len() == pieces.len() => ids,
                _ => pieces.iter().map(|_| ObjectId::new()).collect(),
            };
            let (obj, index) = doc.remove(tid).expect("resolved");
            for (piece, pid) in pieces.into_iter().zip(&new_ids) {
                doc.insert(SceneObject {
                    visible: true,
                    id: *pid,
                    name: obj.name.clone(),
                    layer: obj.layer.clone(),
                    color: None,
                    geometry: Geometry::Curve(piece),
                });
            }
            let listed: Vec<String> = new_ids.iter().map(|i| i.to_string()).collect();
            Ok((
                Command::Split { ids: Some(new_ids.clone()), target, point },
                Inverse::Replace { created: new_ids.clone(), consumed: vec![(obj, index)] },
                ApplyOutcome {
                    message: format!("split {tid} -> {}", listed.join(", ")),
                    created: new_ids,
                },
            ))
        }
        Command::Trim { id, target, cutter, keep } => {
            let cutter_ids = resolve(doc, &cutter)?;
            // Cutters win overlaps, so "trim last 2 last <point>" reads
            // naturally: target = the older of the two most recent curves.
            let target_ids: Vec<ObjectId> = resolve(doc, &target)?
                .into_iter()
                .filter(|tid| !cutter_ids.contains(tid))
                .collect();
            let [tid] = target_ids[..] else {
                return Err(ExecError::Invalid(format!(
                    "trim target selector matched {} objects (excluding cutters), expected exactly 1",
                    target_ids.len()
                )));
            };
            let curve = curve_of(doc, tid, "trim")?;
            let mut cuts = Vec::new();
            for cid in &cutter_ids {
                let cut_curve = curve_of(doc, *cid, "trim (cutter)")?;
                cuts.extend(kernel_curve::intersections(curve, cut_curve, PROFILE_TOL));
            }
            if cuts.is_empty() {
                return Err(ExecError::Invalid(
                    "target and cutter curves do not intersect — nothing to trim".into(),
                ));
            }
            let pieces = kernel_curve::split_at_points(curve, &cuts, kernel_curve::JOIN_TOL)
                .ok_or_else(|| {
                    ExecError::Invalid(
                        "cannot trim this curve: closed curves need 2+ intersections, \
                         and NURBS/ellipse trimming is not supported yet"
                            .into(),
                    )
                })?;
            if pieces.len() < 2 {
                return Err(ExecError::Invalid(
                    "the cutter only touches the curve's ends — nothing to trim".into(),
                ));
            }
            let count = pieces.len();
            let kept = pieces
                .into_iter()
                .min_by(|a, b| {
                    let da = kernel_curve::closest_point(a, keep, PROFILE_TOL).distance(keep);
                    let db = kernel_curve::closest_point(b, keep, PROFILE_TOL).distance(keep);
                    da.partial_cmp(&db).expect("finite distances")
                })
                .expect("count >= 2");
            let id = id.unwrap_or_default();
            let (obj, index) = doc.remove(tid).expect("resolved");
            doc.insert(SceneObject {
                visible: true,
                id,
                name: obj.name.clone(),
                layer: obj.layer.clone(),
                color: None,
                geometry: Geometry::Curve(kept),
            });
            Ok((
                Command::Trim { id: Some(id), target, cutter, keep },
                Inverse::Replace { created: vec![id], consumed: vec![(obj, index)] },
                ApplyOutcome {
                    created: vec![id],
                    message: format!(
                        "trimmed {tid} -> {id} (kept 1 of {count} pieces)"
                    ),
                },
            ))
        }
        Command::Extend { targets, distance } => {
            if distance <= 0.0 {
                return Err(ExecError::Invalid("extend distance must be positive".into()));
            }
            let ids = resolve(doc, &targets)?;
            // Compute every extension first so a failure leaves the doc untouched.
            let mut extended = Vec::with_capacity(ids.len());
            for id in &ids {
                let curve = curve_of(doc, *id, "extend")?;
                let new = kernel_curve::extend(curve, distance).ok_or_else(|| {
                    ExecError::Invalid(format!(
                        "'{id}' cannot be extended — only open lines, polylines and arcs can"
                    ))
                })?;
                extended.push((*id, new));
            }
            let mut snapshots = Vec::with_capacity(ids.len());
            for (id, new) in extended {
                let obj = doc.get_mut(id).expect("resolved");
                snapshots.push((id, obj.geometry.clone()));
                obj.geometry = Geometry::Curve(new);
            }
            Ok((
                Command::Extend { targets, distance },
                Inverse::SetGeometry(snapshots),
                ApplyOutcome {
                    created: Vec::new(),
                    message: format!("extended {} curve(s) by {distance}", ids.len()),
                },
            ))
        }
        Command::Join { id, targets } => {
            let ids = resolve(doc, &targets)?;
            if ids.len() < 2 {
                return Err(ExecError::Invalid(
                    "join needs at least 2 curves (selector matched 1)".into(),
                ));
            }
            let curves: Vec<Curve> = ids
                .iter()
                .map(|cid| curve_of(doc, *cid, "join").cloned())
                .collect::<Result<_, _>>()?;
            let joined =
                kernel_curve::join_curves(&curves, kernel_curve::JOIN_TOL, PROFILE_TOL)
                    .ok_or_else(|| {
                        ExecError::Invalid(
                            "curves do not touch end-to-end (1e-6 tolerance) or a closed \
                             curve was selected — nothing to join"
                                .into(),
                        )
                    })?;
            let closed = joined.is_closed();
            let mut consumed = Vec::new();
            let (name, layer) = {
                let first = doc.get(ids[0]).expect("resolved");
                (first.name.clone(), first.layer.clone())
            };
            for cid in &ids {
                if let Some(pair) = doc.remove(*cid) {
                    consumed.push(pair);
                }
            }
            let id = id.unwrap_or_default();
            doc.insert(SceneObject { id, name, layer, visible: true, color: None, geometry: Geometry::Curve(joined) });
            Ok((
                Command::Join { id: Some(id), targets },
                Inverse::Replace { created: vec![id], consumed },
                ApplyOutcome {
                    created: vec![id],
                    message: format!(
                        "joined {} curve(s) -> {id} ({} polyline)",
                        ids.len(),
                        if closed { "closed" } else { "open" }
                    ),
                },
            ))
        }
        Command::Fillet { id, a, b, radius } => {
            if radius <= 0.0 {
                return Err(ExecError::Invalid("fillet radius must be positive".into()));
            }
            let mut ids = resolve(doc, &a)?;
            for bid in resolve(doc, &b)? {
                if !ids.contains(&bid) {
                    ids.push(bid);
                }
            }
            if ids.len() != 2 {
                return Err(ExecError::Invalid(format!(
                    "fillet needs exactly 2 curves, selectors matched {}",
                    ids.len()
                )));
            }
            let line_of = |cid: ObjectId| -> Result<(DVec3, DVec3), ExecError> {
                match curve_of(doc, cid, "fillet")? {
                    Curve::Line { a, b } => Ok((*a, *b)),
                    _ => Err(ExecError::Invalid(format!(
                        "fillet works on lines for now; '{cid}' is not a line"
                    ))),
                }
            };
            let (la, lb) = (line_of(ids[0])?, line_of(ids[1])?);
            let (ta, arc, tb) = kernel_curve::fillet_lines(la, lb, radius).ok_or_else(|| {
                ExecError::Invalid(format!(
                    "cannot fillet: lines are parallel or radius {radius} does not fit"
                ))
            })?;
            let mut snapshots = Vec::with_capacity(2);
            for (cid, trimmed) in [(ids[0], ta), (ids[1], tb)] {
                let obj = doc.get_mut(cid).expect("resolved");
                snapshots.push((cid, obj.geometry.clone()));
                obj.geometry = Geometry::Curve(trimmed);
            }
            let id = id.unwrap_or_default();
            doc.insert(SceneObject {
                visible: true,
                id,
                name: None,
                layer: doc.current_layer.clone(),
                color: None,
                geometry: Geometry::Curve(arc),
            });
            Ok((
                Command::Fillet { id: Some(id), a, b, radius },
                Inverse::CreatedAndGeometry { created: vec![id], snapshots },
                ApplyOutcome {
                    created: vec![id],
                    message: format!(
                        "filleted {} + {} r={radius} -> arc {id} (lines trimmed to tangency)",
                        ids[0], ids[1]
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
                visible: true,
                id,
                name: None,
                layer: doc.current_layer.clone(),
                color: None,
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
        Command::Array { ids, targets, counts, delta } => {
            let [nx, ny, nz] = counts;
            if nx == 0 || ny == 0 || nz == 0 {
                return Err(ExecError::Invalid(format!(
                    "array counts must be at least 1, got {nx},{ny},{nz}"
                )));
            }
            let cells = nx as usize * ny as usize * nz as usize - 1;
            if cells == 0 {
                return Err(ExecError::Invalid(
                    "array 1,1,1 makes no copies — raise a count".into(),
                ));
            }
            let src = resolve(doc, &targets)?;
            let total = src.len() * cells;
            // Reuse logged ids on replay; mint new ones live.
            let new_ids: Vec<ObjectId> = match ids {
                Some(ids) if ids.len() == total => ids,
                _ => (0..total).map(|_| ObjectId::new()).collect(),
            };
            let mut idx = 0;
            for src_id in &src {
                let base = doc.get(*src_id).expect("resolved").clone();
                for k in 0..nz {
                    for j in 0..ny {
                        for i in 0..nx {
                            if i == 0 && j == 0 && k == 0 {
                                continue; // the original occupies this cell
                            }
                            let mut obj = base.clone();
                            obj.id = new_ids[idx];
                            idx += 1;
                            obj.geometry.translate(DVec3::new(
                                f64::from(i) * delta.x,
                                f64::from(j) * delta.y,
                                f64::from(k) * delta.z,
                            ));
                            doc.insert(obj);
                        }
                    }
                }
            }
            Ok((
                Command::Array { ids: Some(new_ids.clone()), targets, counts, delta },
                Inverse::DeleteCreated(new_ids.clone()),
                ApplyOutcome {
                    message: format!(
                        "arrayed {} object(s) into a {nx}x{ny}x{nz} grid ({total} copies)",
                        src.len()
                    ),
                    created: new_ids,
                },
            ))
        }
        Command::PolarArray { ids, targets, count, center, total_angle_deg } => {
            if count < 2 {
                return Err(ExecError::Invalid(
                    "polar array count must be at least 2".into(),
                ));
            }
            let src = resolve(doc, &targets)?;
            let center_pt = center.unwrap_or_else(|| {
                let mut bb = doc.get(src[0]).expect("resolved").geometry.aabb();
                for id in &src[1..] {
                    bb = bb.union(doc.get(*id).expect("resolved").geometry.aabb());
                }
                bb.center()
            });
            // Full circles divide evenly; partial sweeps land the last copy
            // exactly at the total angle.
            let step = match total_angle_deg {
                None => 360.0 / f64::from(count),
                Some(total) => total / f64::from(count - 1),
            };
            let copies = (count - 1) as usize;
            let total_new = src.len() * copies;
            let new_ids: Vec<ObjectId> = match ids {
                Some(ids) if ids.len() == total_new => ids,
                _ => (0..total_new).map(|_| ObjectId::new()).collect(),
            };
            let mut tessellated = 0usize;
            let mut idx = 0;
            for src_id in &src {
                let base = doc.get(*src_id).expect("resolved").clone();
                for k in 1..count {
                    let m = glam::DMat4::from_translation(center_pt)
                        * glam::DMat4::from_axis_angle(
                            DVec3::Z,
                            (f64::from(k) * step).to_radians(),
                        )
                        * glam::DMat4::from_translation(-center_pt);
                    let mut obj = base.clone();
                    obj.id = new_ids[idx];
                    idx += 1;
                    if !obj.geometry.transform(&m, PROFILE_TOL) {
                        tessellated += 1;
                    }
                    doc.insert(obj);
                }
            }
            Ok((
                Command::PolarArray {
                    ids: Some(new_ids.clone()),
                    targets,
                    count,
                    center,
                    total_angle_deg,
                },
                Inverse::DeleteCreated(new_ids.clone()),
                ApplyOutcome {
                    message: format!(
                        "polar array: {} object(s) x {count} about {:.2},{:.2}{}",
                        src.len(),
                        center_pt.x,
                        center_pt.y,
                        tessellation_note(tessellated)
                    ),
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
        Command::Group { targets, name } => {
            let ids = resolve(doc, &targets)?;
            // Replay reuses the logged name; live fills the first free groupN.
            let name = name.unwrap_or_else(|| {
                (1..)
                    .map(|n| format!("group{n}"))
                    .find(|n| !doc.groups.contains_key(n))
                    .expect("unbounded counter finds a free name")
            });
            let members: std::collections::BTreeSet<ObjectId> = ids.iter().copied().collect();
            let prev = doc.groups.insert(name.clone(), members);
            doc.generation += 1;
            Ok((
                Command::Group { targets, name: Some(name.clone()) },
                Inverse::GroupSet { name: name.clone(), prev },
                ApplyOutcome {
                    created: Vec::new(),
                    message: format!("grouped {} object(s) as '{name}'", ids.len()),
                },
            ))
        }
        Command::Ungroup { targets } => {
            let ids = resolve(doc, &targets)?;
            let removed = doc.groups_containing(&ids);
            if removed.is_empty() {
                return Err(ExecError::Invalid(
                    "no group contains the selected objects (make one with: group <selector> [name])"
                        .into(),
                ));
            }
            let names: Vec<&str> = removed.iter().map(|(n, _)| n.as_str()).collect();
            let message = format!("ungrouped: {}", names.join(", "));
            for (name, _) in &removed {
                doc.groups.remove(name);
            }
            doc.generation += 1;
            Ok((
                Command::Ungroup { targets },
                Inverse::RestoreGroups(removed),
                ApplyOutcome { created: Vec::new(), message },
            ))
        }
        Command::Layer { name } => {
            let prev = doc.current_layer.clone();
            let created = !doc.layers.contains_key(&name);
            if created {
                doc.layers.insert(name.clone(), LayerStyle::default());
            }
            doc.current_layer = name.clone();
            doc.generation += 1;
            Ok((
                Command::Layer { name: name.clone() },
                Inverse::LayerCurrent {
                    prev,
                    created: created.then(|| name.clone()),
                },
                ApplyOutcome {
                    created: Vec::new(),
                    message: format!(
                        "current layer: '{name}'{}",
                        if created { " (created)" } else { "" }
                    ),
                },
            ))
        }
        Command::ToLayer { targets, layer } => {
            let ids = resolve(doc, &targets)?;
            let created = !doc.layers.contains_key(&layer);
            if created {
                doc.layers.insert(layer.clone(), LayerStyle::default());
            }
            let mut prev = Vec::with_capacity(ids.len());
            for id in &ids {
                let obj = doc.get_mut(*id).expect("resolved");
                prev.push((*id, obj.layer.clone()));
                obj.layer = layer.clone();
            }
            Ok((
                Command::ToLayer { targets, layer: layer.clone() },
                Inverse::ObjectLayers {
                    prev,
                    created: created.then(|| layer.clone()),
                },
                ApplyOutcome {
                    created: Vec::new(),
                    message: format!(
                        "moved {} object(s) to layer '{layer}'{}",
                        ids.len(),
                        if created { " (created)" } else { "" }
                    ),
                },
            ))
        }
        Command::LayerColor { layer, color } => {
            let style = layer_style_mut(doc, &layer)?;
            let prev = style.clone();
            style.color = Some([color[0], color[1], color[2], 1.0]);
            doc.generation += 1;
            Ok((
                Command::LayerColor { layer: layer.clone(), color },
                Inverse::LayerStyle { layer: layer.clone(), prev },
                ApplyOutcome {
                    created: Vec::new(),
                    message: format!(
                        "layer '{layer}' color set to {:.2},{:.2},{:.2}",
                        color[0], color[1], color[2]
                    ),
                },
            ))
        }
        Command::LayerWeight { layer, mm } => {
            let style = layer_style_mut(doc, &layer)?;
            let prev = style.clone();
            style.lineweight_mm = mm;
            doc.generation += 1;
            Ok((
                Command::LayerWeight { layer: layer.clone(), mm },
                Inverse::LayerStyle { layer: layer.clone(), prev },
                ApplyOutcome {
                    created: Vec::new(),
                    message: format!("layer '{layer}' lineweight set to {mm:.3} mm"),
                },
            ))
        }
        Command::Hide { layer } => {
            let style = layer_style_mut(doc, &layer)?;
            let prev = style.clone();
            style.visible = false;
            doc.generation += 1;
            Ok((
                Command::Hide { layer: layer.clone() },
                Inverse::LayerStyle { layer: layer.clone(), prev },
                ApplyOutcome {
                    created: Vec::new(),
                    message: format!("layer '{layer}' hidden"),
                },
            ))
        }
        Command::Show { layer } => {
            let style = layer_style_mut(doc, &layer)?;
            let prev = style.clone();
            style.visible = true;
            doc.generation += 1;
            Ok((
                Command::Show { layer: layer.clone() },
                Inverse::LayerStyle { layer: layer.clone(), prev },
                ApplyOutcome {
                    created: Vec::new(),
                    message: format!("layer '{layer}' shown"),
                },
            ))
        }
        Command::HideObj { targets } => {
            let ids = resolve(doc, &targets)?;
            let mut prev = Vec::with_capacity(ids.len());
            for id in &ids {
                let obj = doc.get_mut(*id).expect("resolved");
                prev.push((*id, obj.visible));
                obj.visible = false;
            }
            Ok((
                Command::HideObj { targets },
                Inverse::ObjectVisibility(prev),
                ApplyOutcome {
                    created: Vec::new(),
                    message: format!("hid {} object(s)", ids.len()),
                },
            ))
        }
        Command::ShowObj { targets } => {
            let ids = resolve(doc, &targets)?;
            let mut prev = Vec::with_capacity(ids.len());
            for id in &ids {
                let obj = doc.get_mut(*id).expect("resolved");
                prev.push((*id, obj.visible));
                obj.visible = true;
            }
            Ok((
                Command::ShowObj { targets },
                Inverse::ObjectVisibility(prev),
                ApplyOutcome {
                    created: Vec::new(),
                    message: format!("showed {} object(s)", ids.len()),
                },
            ))
        }
        Command::Color { targets, color } => {
            let ids = resolve(doc, &targets)?;
            let mut prev = Vec::with_capacity(ids.len());
            for id in &ids {
                let obj = doc.get_mut(*id).expect("resolved");
                prev.push((*id, obj.color));
                obj.color = Some(color);
            }
            doc.generation += 1;
            Ok((
                Command::Color { targets, color },
                Inverse::ObjectColor(prev),
                ApplyOutcome {
                    created: Vec::new(),
                    message: format!(
                        "colored {} object(s) ({:.2},{:.2},{:.2})",
                        ids.len(),
                        color[0],
                        color[1],
                        color[2]
                    ),
                },
            ))
        }
        Command::ColorOff { targets } => {
            let ids = resolve(doc, &targets)?;
            let mut prev = Vec::with_capacity(ids.len());
            for id in &ids {
                let obj = doc.get_mut(*id).expect("resolved");
                prev.push((*id, obj.color));
                obj.color = None;
            }
            doc.generation += 1;
            Ok((
                Command::ColorOff { targets },
                Inverse::ObjectColor(prev),
                ApplyOutcome {
                    created: Vec::new(),
                    message: format!("cleared color on {} object(s)", ids.len()),
                },
            ))
        }
        Command::Units { units } => {
            let prev = doc.units;
            doc.units = units;
            doc.generation += 1;
            Ok((
                Command::Units { units },
                Inverse::Units { prev },
                ApplyOutcome {
                    created: Vec::new(),
                    message: format!(
                        "units: {} (e.g. {})",
                        units.label(),
                        format_length(units, 12.5)
                    ),
                },
            ))
        }
        Command::Underlay { path, corner, width, height } => {
            let prev = doc.underlay.clone();
            let corner = corner.unwrap_or(DVec3::ZERO);
            let width = width.unwrap_or(10.0);
            if width <= 0.0 {
                return Err(ExecError::Invalid("underlay width must be positive".into()));
            }
            // height carried on a replayed op wins (the file need not exist);
            // otherwise derive it from the image's pixel aspect ratio. A
            // missing/unreadable file is a warning, not an error: fall back to
            // a square so the placement still lands and replays.
            let (height, note) = match height {
                Some(h) => (h, ""),
                None => match image_aspect(&path) {
                    Some(aspect) if aspect > 0.0 => (width / aspect, ""),
                    _ => (width, " (image unreadable, assumed square)"),
                },
            };
            // Keep the previous opacity when swapping the image; new underlays
            // start fully opaque.
            let opacity = prev.as_ref().map_or(1.0, |u| u.opacity);
            doc.underlay = Some(Underlay {
                path: path.clone(),
                corner: corner.truncate(),
                width,
                height,
                opacity,
            });
            doc.generation += 1;
            Ok((
                Command::Underlay {
                    path,
                    corner: Some(corner),
                    width: Some(width),
                    height: Some(height),
                },
                Inverse::Underlay { prev },
                ApplyOutcome {
                    created: Vec::new(),
                    message: format!("underlay {width:.2} x {height:.2} m{note}"),
                },
            ))
        }
        Command::UnderlayOpacity { opacity } => {
            let prev = doc.underlay.clone();
            let opacity = opacity.clamp(0.0, 1.0);
            let Some(u) = doc.underlay.as_mut() else {
                return Err(ExecError::Invalid(
                    "no underlay to set opacity on (place one with: underlay <path>)".into(),
                ));
            };
            u.opacity = opacity;
            doc.generation += 1;
            Ok((
                Command::UnderlayOpacity { opacity },
                Inverse::Underlay { prev },
                ApplyOutcome {
                    created: Vec::new(),
                    message: format!("underlay opacity {opacity:.2}"),
                },
            ))
        }
        Command::UnderlayOff => {
            let prev = doc.underlay.take();
            if prev.is_none() {
                return Err(ExecError::Invalid("no underlay to remove".into()));
            }
            doc.generation += 1;
            Ok((
                Command::UnderlayOff,
                Inverse::Underlay { prev },
                ApplyOutcome {
                    created: Vec::new(),
                    message: "underlay removed".into(),
                },
            ))
        }
        Command::Sun { azimuth_deg, altitude_deg } => {
            let prev = doc.sun;
            doc.sun = Some(mydrafter_doc::SunPosition { azimuth_deg, altitude_deg });
            doc.generation += 1;
            Ok((
                Command::Sun { azimuth_deg, altitude_deg },
                Inverse::Sun { prev },
                ApplyOutcome {
                    created: Vec::new(),
                    message: format!(
                        "sun az={azimuth_deg:.1}° alt={altitude_deg:.1}°"
                    ),
                },
            ))
        }
        Command::SunOff => {
            let prev = doc.sun.take();
            doc.generation += 1;
            Ok((
                Command::SunOff,
                Inverse::Sun { prev },
                ApplyOutcome {
                    created: Vec::new(),
                    message: "sun removed (headlight shading)".into(),
                },
            ))
        }
        Command::Sheet { name, paper } => {
            if doc.sheet(&name).is_some() {
                return Err(ExecError::Invalid(format!(
                    "sheet '{name}' already exists (add views with: sheetview {name} top 1:100)"
                )));
            }
            let (w, h) = paper.landscape_mm();
            doc.sheets.push(mydrafter_doc::Sheet {
                name: name.clone(),
                paper,
                views: Vec::new(),
                table: None,
                dims: Vec::new(),
            });
            doc.generation += 1;
            Ok((
                Command::Sheet { name: name.clone(), paper },
                Inverse::RemoveSheet(name.clone()),
                ApplyOutcome {
                    created: Vec::new(),
                    message: format!("sheet '{name}' ({} landscape, {w}x{h}mm)", paper.label()),
                },
            ))
        }
        Command::SheetView { sheet, direction, scale } => {
            if scale <= 0.0 {
                return Err(ExecError::Invalid("view scale must be positive".into()));
            }
            let known: Vec<String> = doc.sheets.iter().map(|s| s.name.clone()).collect();
            let Some(s) = doc.sheet_mut(&sheet) else {
                return Err(ExecError::Invalid(format!(
                    "no sheet '{sheet}' (sheets: {}; create one with: sheet {sheet})",
                    known.join(", ")
                )));
            };
            s.views.push(mydrafter_doc::SheetView { direction, scale });
            let count = s.views.len();
            doc.generation += 1;
            Ok((
                Command::SheetView { sheet: sheet.clone(), direction, scale },
                Inverse::PopSheetView(sheet.clone()),
                ApplyOutcome {
                    created: Vec::new(),
                    message: format!(
                        "added {} view @ 1:{scale} to '{sheet}' ({count} view(s))",
                        direction.label()
                    ),
                },
            ))
        }
        Command::Print { sheet, path } => {
            let Some(s) = doc.sheet(&sheet) else {
                let known: Vec<String> = doc.sheets.iter().map(|s| s.name.clone()).collect();
                return Err(ExecError::Invalid(format!(
                    "no sheet '{sheet}' (sheets: {})",
                    known.join(", ")
                )));
            };
            if s.views.is_empty() {
                return Err(ExecError::Invalid(format!(
                    "sheet '{sheet}' has no views (add one with: sheetview {sheet} top 1:100)"
                )));
            }
            let (bytes, drawn) = crate::pdf::sheet_pdf(doc, s);
            let size = bytes.len();
            std::fs::write(&path, bytes).map_err(|e| {
                ExecError::Invalid(format!("cannot write '{path}': {e}"))
            })?;
            Ok((
                Command::Print { sheet: sheet.clone(), path: path.clone() },
                Inverse::Rename(Vec::new()), // never logged; inverse unused
                ApplyOutcome {
                    created: Vec::new(),
                    message: format!(
                        "printed '{sheet}' -> {path} ({drawn} lines, {size} bytes)"
                    ),
                },
            ))
        }
        Command::Export { path } => {
            let ext = path.rsplit('.').next().map(|e| e.to_ascii_lowercase()).unwrap_or_default();
            let (bytes, detail): (Vec<u8>, String) = match ext.as_str() {
                "dxf" => {
                    let (text, entities) = crate::dxf::document_dxf(doc);
                    (text.into_bytes(), format!("DXF, {entities} entities"))
                }
                "svg" => {
                    let (b, count) = crate::svg::export_svg(doc);
                    (b, format!("SVG, {count}"))
                }
                "csv" => {
                    let (b, count) = crate::csv::export_csv(doc);
                    (b, format!("CSV, {count}"))
                }
                "ifc" => {
                    let (b, count) = crate::ifc::export(doc, &path).map_err(ExecError::Invalid)?;
                    (b, format!("IFC4, {count}"))
                }
                _ => {
                    let (bytes, count) = crate::mesh_export::export(doc, &path)
                        .map_err(ExecError::Invalid)?;
                    let label = ext.to_ascii_uppercase();
                    (bytes, format!("{label}, {count}"))
                }
            };
            let size = bytes.len();
            std::fs::write(&path, bytes)
                .map_err(|e| ExecError::Invalid(format!("cannot write '{path}': {e}")))?;
            Ok((
                Command::Export { path: path.clone() },
                Inverse::Rename(Vec::new()), // never logged; inverse unused
                ApplyOutcome {
                    created: Vec::new(),
                    message: format!("exported {detail} -> {path} ({size} bytes)"),
                },
            ))
        }
        Command::ViewSave { name, camera } => {
            let Some(camera) = camera else {
                // Only the app can capture the live viewport; parse leaves None.
                return Err(ExecError::Invalid(format!(
                    "view save needs the live viewport camera — run 'view save {name}' from the app command line"
                )));
            };
            let prev = doc.named_views.insert(name.clone(), camera);
            doc.generation += 1;
            let verb = if prev.is_some() { "updated" } else { "saved" };
            Ok((
                Command::ViewSave { name: name.clone(), camera: Some(camera) },
                Inverse::ViewSaved { name: name.clone(), prev },
                ApplyOutcome {
                    created: Vec::new(),
                    message: format!(
                        "view '{name}' {verb} ({} saved view(s))",
                        doc.named_views.len()
                    ),
                },
            ))
        }
        Command::ViewRestore { name } => {
            let Some(view) = doc.named_views.get(&name).copied() else {
                let known = if doc.named_views.is_empty() {
                    "none".to_string()
                } else {
                    doc.named_views.keys().cloned().collect::<Vec<_>>().join(", ")
                };
                return Err(ExecError::Invalid(format!(
                    "no saved view '{name}' (saved: {known}; save one with: view save {name})"
                )));
            };
            // The camera lives in the UI, not the document: park the view in
            // the mailbox for the app to apply to the active viewport.
            doc.pending_view = Some(view);
            Ok((
                Command::ViewRestore { name: name.clone() },
                Inverse::Rename(Vec::new()), // never logged; inverse unused
                ApplyOutcome {
                    created: Vec::new(),
                    message: format!("view: {name}"),
                },
            ))
        }
        Command::ViewList => {
            let names: Vec<&str> = doc.named_views.keys().map(String::as_str).collect();
            Ok((
                Command::ViewList,
                Inverse::Rename(Vec::new()), // never logged; inverse unused
                ApplyOutcome {
                    created: Vec::new(),
                    message: if names.is_empty() {
                        "no saved views (save one with: view save <name>)".to_string()
                    } else {
                        format!("saved views: {}", names.join(", "))
                    },
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
        Command::Distance { a, b } => {
            let d = b - a;
            let u = doc.units;
            Ok((
                Command::Distance { a, b },
                Inverse::Rename(Vec::new()), // never logged; inverse unused
                ApplyOutcome {
                    created: Vec::new(),
                    message: format!(
                        "distance: {} (dx {}, dy {}, dz {})",
                        format_length(u, d.length()),
                        format_length(u, d.x),
                        format_length(u, d.y),
                        format_length(u, d.z)
                    ),
                },
            ))
        }
        Command::Area { targets } => {
            let ids = resolve(doc, &targets)?;
            let mut total = 0.0;
            for id in &ids {
                total += match &doc.get(*id).expect("resolved").geometry {
                    Geometry::Curve(c) if c.is_closed() => {
                        shoelace_area(&c.tessellate(PROFILE_TOL))
                    }
                    Geometry::Curve(_) => {
                        return Err(ExecError::Invalid(format!(
                            "'{id}' is an open curve — area needs a closed curve or a mesh"
                        )))
                    }
                    Geometry::Mesh(m) => mesh_surface_area(m),
                    Geometry::Annotation(_) => {
                        return Err(ExecError::Invalid(format!(
                            "'{id}' is an annotation — area needs a closed curve or a mesh"
                        )))
                    }
                    Geometry::Instance { block, .. } => {
                        return Err(ExecError::Invalid(format!(
                            "'{id}' is a block instance ('{block}') — area not supported on instances"
                        )))
                    }
                };
            }
            Ok((
                Command::Area { targets },
                Inverse::Rename(Vec::new()), // never logged; inverse unused
                ApplyOutcome {
                    created: Vec::new(),
                    message: format!(
                        "area of {} object(s): {}",
                        ids.len(),
                        format_area(doc.units, total)
                    ),
                },
            ))
        }
        Command::Volume { targets } => {
            let ids = resolve(doc, &targets)?;
            let mut total = 0.0;
            for id in &ids {
                match &doc.get(*id).expect("resolved").geometry {
                    Geometry::Mesh(m) => total += kernel_mesh::signed_volume(m),
                    Geometry::Curve(_) => {
                        return Err(ExecError::Invalid(format!(
                            "'{id}' is a curve; volume needs meshes — extrude it first"
                        )))
                    }
                    Geometry::Annotation(_) => {
                        return Err(ExecError::Invalid(format!(
                            "'{id}' is an annotation; volume needs meshes"
                        )))
                    }
                    Geometry::Instance { block, .. } => {
                        return Err(ExecError::Invalid(format!(
                            "'{id}' is a block instance ('{block}'); volume not supported on instances"
                        )))
                    }
                }
            }
            Ok((
                Command::Volume { targets },
                Inverse::Rename(Vec::new()), // never logged; inverse unused
                ApplyOutcome {
                    created: Vec::new(),
                    message: format!(
                        "volume of {} object(s): {}",
                        ids.len(),
                        format_volume(doc.units, total)
                    ),
                },
            ))
        }
        Command::Bbox { targets } => {
            let ids = resolve(doc, &targets)?;
            let mut bb = doc.get(ids[0]).expect("resolved").geometry.aabb();
            for id in &ids[1..] {
                bb = bb.union(doc.get(*id).expect("resolved").geometry.aabb());
            }
            let (per_m, label) = doc.units.per_meter();
            let v = |p: DVec3| {
                format!("{:.2},{:.2},{:.2}", p.x * per_m, p.y * per_m, p.z * per_m)
            };
            Ok((
                Command::Bbox { targets },
                Inverse::Rename(Vec::new()), // never logged; inverse unused
                ApplyOutcome {
                    created: Vec::new(),
                    message: format!(
                        "bbox of {} object(s) ({label}): min {} max {} size {}",
                        ids.len(),
                        v(bb.min),
                        v(bb.max),
                        v(bb.size())
                    ),
                },
            ))
        }
        Command::Schedule { layer } => {
            let rows = build_schedule_rows(doc, layer.as_deref());
            let msg = if rows.is_empty() {
                match &layer {
                    Some(l) => format!("no objects on layer '{l}'"),
                    None => "no objects in document".to_string(),
                }
            } else {
                format_schedule_table(&rows, doc.units)
            };
            Ok((
                Command::Schedule { layer },
                Inverse::Rename(Vec::new()), // never logged; inverse unused
                ApplyOutcome { created: Vec::new(), message: msg },
            ))
        }
        Command::SheetTable { sheet, layer } => {
            // Build rows before borrowing the sheet (avoid simultaneous borrows).
            let rows = build_schedule_rows(doc, layer.as_deref());
            let count = rows.len();
            let known: Vec<String> = doc.sheets.iter().map(|s| s.name.clone()).collect();
            let Some(s) = doc.sheet_mut(&sheet) else {
                return Err(ExecError::Invalid(format!(
                    "no sheet '{sheet}' (sheets: {}; create one with: sheet {sheet})",
                    known.join(", ")
                )));
            };
            s.table = Some(SheetTable { layer: layer.clone(), rows });
            doc.generation += 1;
            Ok((
                Command::SheetTable { sheet: sheet.clone(), layer },
                Inverse::SheetTableRemoved(sheet.clone()),
                ApplyOutcome {
                    created: Vec::new(),
                    message: format!(
                        "schedule table placed on '{sheet}' ({count} row(s))"
                    ),
                },
            ))
        }
        Command::SheetDim { sheet, a, b, offset, view_index } => {
            let offset_mm = offset.unwrap_or(8.0);
            let vi = view_index.unwrap_or(0);
            let known: Vec<String> = doc.sheets.iter().map(|s| s.name.clone()).collect();
            let Some(s) = doc.sheet_mut(&sheet) else {
                return Err(ExecError::Invalid(format!(
                    "no sheet '{sheet}' (sheets: {}; create one with: sheet {sheet})",
                    known.join(", ")
                )));
            };
            if vi >= s.views.len() && !s.views.is_empty() {
                return Err(ExecError::Invalid(format!(
                    "view index {vi} is out of range (sheet '{sheet}' has {} views)",
                    s.views.len()
                )));
            }
            let scale = s.views.get(vi).map(|v| v.scale).unwrap_or(100.0);
            s.dims.push(SheetDim { a_mm: a, b_mm: b, offset_mm, view_index: vi });
            doc.generation += 1;
            // Compute the model-space distance for the echo message.
            let paper_dist = {
                let dx = b[0] - a[0];
                let dy = b[1] - a[1];
                (dx * dx + dy * dy).sqrt()
            };
            let model_m = paper_dist * scale / 1000.0;
            Ok((
                Command::SheetDim {
                    sheet: sheet.clone(),
                    a,
                    b,
                    offset: Some(offset_mm),
                    view_index: Some(vi),
                },
                Inverse::PopSheetDim(sheet.clone()),
                ApplyOutcome {
                    created: Vec::new(),
                    message: format!(
                        "dim on '{sheet}' view {vi} @ 1:{scale}: {paper_dist:.1}mm paper = {model_m:.3}m model"
                    ),
                },
            ))
        }
        Command::MeshLiteral { id, positions, faces, name } => {
            if positions.is_empty() || faces.is_empty() {
                return Err(ExecError::Invalid(
                    "mesh_literal: positions and faces must be non-empty".into(),
                ));
            }
            // Validate face indices.
            let n = positions.len() as u32;
            for f in &faces {
                if f.iter().any(|&i| i >= n) {
                    return Err(ExecError::Invalid(format!(
                        "mesh_literal: face index out of range (max index={}, positions={})",
                        f.iter().copied().max().unwrap_or(0),
                        n
                    )));
                }
            }
            let id = id.unwrap_or_default();
            let mesh = kernel_mesh::Mesh::new(positions.clone(), faces.clone());
            let face_count = faces.len();
            doc.insert(SceneObject {
                visible: true,
                id,
                name: name.clone(),
                layer: doc.current_layer.clone(),
                color: None,
                geometry: Geometry::Mesh(mesh),
            });
            Ok((
                Command::MeshLiteral { id: Some(id), positions, faces, name: name.clone() },
                Inverse::DeleteCreated(vec![id]),
                ApplyOutcome {
                    created: vec![id],
                    message: format!(
                        "mesh {id} ({face_count} triangles{})",
                        name.as_deref().map(|n| format!(", '{n}'")).unwrap_or_default()
                    ),
                },
            ))
        }
        Command::BlockDefine { targets, name, geometries } => {
            use mydrafter_doc::BlockGeometry;
            let ids = resolve(doc, &targets)?;
            // Snapshot geometry from source objects.
            let snaps: Vec<BlockGeometry> = if let Some(g) = geometries {
                // Replay path: use stored snapshots.
                g
            } else {
                // Live path: snapshot from current objects.
                ids.iter()
                    .filter_map(|id| {
                        let obj = doc.get(*id)?;
                        Some(match &obj.geometry {
                            Geometry::Mesh(m) => BlockGeometry::Mesh(m.clone()),
                            Geometry::Curve(c) => BlockGeometry::Curve(c.clone()),
                            Geometry::Annotation(a) => BlockGeometry::Annotation(a.clone()),
                            // Instances within a block are flattened/skipped.
                            Geometry::Instance { .. } => return None,
                        })
                    })
                    .collect()
            };
            if snaps.is_empty() {
                return Err(ExecError::Invalid(
                    "block: selected objects produced no geometry snapshots (instances are not capturable)".into(),
                ));
            }
            let prev = doc.blocks.insert(name.clone(), snaps.clone());
            doc.generation += 1;
            let n = snaps.len();
            Ok((
                Command::BlockDefine {
                    targets,
                    name: name.clone(),
                    geometries: Some(snaps),
                },
                Inverse::BlockDef { name: name.clone(), prev },
                ApplyOutcome {
                    created: Vec::new(),
                    message: format!("block '{}' defined ({n} geometr{})", name, if n == 1 { "y" } else { "ies" }),
                },
            ))
        }
        Command::BlockInsert { id, name, position, rotation_deg, scale } => {
            if !doc.blocks.contains_key(&name) {
                return Err(ExecError::Invalid(format!(
                    "no block named '{name}' (use 'blocks' to list definitions)"
                )));
            }
            let id = id.unwrap_or_default();
            let rot = rotation_deg.unwrap_or(0.0);
            let sc = scale.unwrap_or(1.0);
            if sc <= 0.0 {
                return Err(ExecError::Invalid("block insert scale must be positive".into()));
            }
            doc.insert(SceneObject {
                visible: true,
                id,
                name: None,
                layer: doc.current_layer.clone(),
                color: None,
                geometry: Geometry::Instance {
                    block: name.clone(),
                    position,
                    rotation_deg: rot,
                    scale: sc,
                },
            });
            Ok((
                Command::BlockInsert {
                    id: Some(id),
                    name: name.clone(),
                    position,
                    rotation_deg: Some(rot),
                    scale: Some(sc),
                },
                Inverse::DeleteCreated(vec![id]),
                ApplyOutcome {
                    created: vec![id],
                    message: format!("insert '{}' -> {id} at {position}", name),
                },
            ))
        }
        Command::BlocksList => {
            let list: Vec<String> = doc
                .blocks
                .iter()
                .map(|(n, defs)| format!("  {n} ({} geometr{})", defs.len(), if defs.len() == 1 { "y" } else { "ies" }))
                .collect();
            let msg = if list.is_empty() {
                "no block definitions".to_string()
            } else {
                format!("blocks:\n{}", list.join("\n"))
            };
            Ok((
                Command::BlocksList,
                // BlocksList is not logged (is_logged returns false), so this
                // Inverse is never stored. Use a harmless variant.
                Inverse::DeleteCreated(Vec::new()),
                ApplyOutcome { created: Vec::new(), message: msg },
            ))
        }
        Command::Undo | Command::Redo | Command::Amend { .. } | Command::Import { .. } => {
            unreachable!("handled in Session::run")
        }
    }
}

fn describe(cmd: &Command) -> &'static str {
    match cmd {
        Command::Box { .. } => "box",
        Command::Extrude { .. } => "extrude",
        Command::Revolve { .. } => "revolve",
        Command::Loft { .. } => "loft",
        Command::Sweep { .. } => "sweep",
        Command::Sweep2 { .. } => "sweep2",
        Command::RailRevolve { .. } => "railrevolve",
        Command::Pipe { .. } => "pipe",
        Command::Line { .. } => "line",
        Command::Polyline { .. } => "polyline",
        Command::Rectangle { .. } => "rect",
        Command::Circle { .. } => "circle",
        Command::Arc { .. } => "arc",
        Command::Ellipse { .. } => "ellipse",
        Command::Polygon { .. } => "polygon",
        Command::Curve { .. } => "curve",
        Command::InterpCurve { .. } => "interpcurve",
        Command::Helix { .. } => "helix",
        Command::SetPoint { .. } => "setpoint",
        Command::Rebuild { .. } => "rebuild",
        Command::Dim { .. } => "dim",
        Command::Text { .. } => "text",
        Command::Hatch { .. } => "hatch",
        Command::Union { .. } => "union",
        Command::Difference { .. } => "difference",
        Command::Intersect { .. } => "intersect",
        Command::Section { .. } => "section",
        Command::Plan { .. } => "plan",
        Command::Elevation { .. } => "elevation",
        Command::Move { .. } => "move",
        Command::Rotate { .. } => "rotate",
        Command::Scale { .. } => "scale",
        Command::Mirror { .. } => "mirror",
        Command::Split { .. } => "split",
        Command::Trim { .. } => "trim",
        Command::Extend { .. } => "extend",
        Command::Join { .. } => "join",
        Command::Fillet { .. } => "fillet",
        Command::Offset { .. } => "offset",
        Command::Copy { .. } => "copy",
        Command::Array { .. } => "array",
        Command::PolarArray { .. } => "polararray",
        Command::Delete { .. } => "delete",
        Command::Name { .. } => "name",
        Command::Group { .. } => "group",
        Command::Ungroup { .. } => "ungroup",
        Command::Layer { .. } => "layer",
        Command::ToLayer { .. } => "tolayer",
        Command::LayerColor { .. } => "layercolor",
        Command::LayerWeight { .. } => "layerweight",
        Command::Hide { .. } => "hide",
        Command::Show { .. } => "show",
        Command::HideObj { .. } => "hideobj",
        Command::ShowObj { .. } => "showobj",
        Command::Color { .. } => "color",
        Command::ColorOff { .. } => "coloroff",
        Command::Units { .. } => "units",
        Command::Underlay { .. } => "underlay",
        Command::UnderlayOpacity { .. } => "underlayopacity",
        Command::UnderlayOff => "underlayoff",
        Command::Sun { .. } => "sun",
        Command::SunOff => "sunoff",
        Command::Sheet { .. } => "sheet",
        Command::SheetView { .. } => "sheetview",
        Command::Print { .. } => "print",
        Command::Export { .. } => "export",
        Command::Import { .. } => "import",
        Command::ViewSave { .. } => "view save",
        Command::ViewRestore { .. } => "view",
        Command::ViewList => "view list",
        Command::Select { .. } => "select",
        Command::SelectNone => "selectnone",
        Command::Distance { .. } => "distance",
        Command::Area { .. } => "area",
        Command::Volume { .. } => "volume",
        Command::Bbox { .. } => "bbox",
        Command::Schedule { .. } => "schedule",
        Command::SheetTable { .. } => "sheettable",
        Command::SheetDim { .. } => "sheetdim",
        Command::MeshLiteral { .. } => "mesh_literal",
        Command::BlockDefine { .. } => "block",
        Command::BlockInsert { .. } => "insert",
        Command::BlocksList => "blocks",
        Command::Undo => "undo",
        Command::Redo => "redo",
        Command::Amend { .. } => "amend",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse;

    fn run(s: &mut Session, line: &str) -> ApplyOutcome {
        s.run(parse(line).unwrap()).unwrap()
    }

    fn sample_view(distance: f32) -> NamedView {
        NamedView {
            target: [1.0, 2.0, 3.0],
            distance,
            yaw: 0.25,
            pitch: 0.5,
            fov_y: 45f32.to_radians(),
            ortho: true,
        }
    }

    #[test]
    fn view_save_restore_list_undo_redo() {
        let mut s = Session::default();
        let v = sample_view(12.0);
        s.run(Command::ViewSave { name: "entry".to_string(), camera: Some(v) })
            .unwrap();
        assert_eq!(s.doc.named_views.get("entry"), Some(&v));

        // Restore parks the saved camera in the mailbox for the UI.
        assert_eq!(s.doc.pending_view, None);
        let out = run(&mut s, "view entry");
        assert_eq!(out.message, "view: entry");
        assert_eq!(s.doc.pending_view, Some(v));

        let out = run(&mut s, "view list");
        assert!(out.message.contains("entry"), "{}", out.message);

        // Unknown views error, listing what exists.
        let err = s.run(parse("view nope").unwrap()).unwrap_err();
        assert!(err.to_string().contains("entry"), "{err}");

        // Overwrite; undo steps back to the previous view, then to none.
        let v2 = sample_view(50.0);
        s.run(Command::ViewSave { name: "entry".to_string(), camera: Some(v2) })
            .unwrap();
        assert_eq!(s.doc.named_views["entry"], v2);
        run(&mut s, "undo");
        assert_eq!(s.doc.named_views["entry"], v);
        run(&mut s, "undo");
        assert!(s.doc.named_views.is_empty());
        run(&mut s, "redo");
        assert_eq!(s.doc.named_views["entry"], v);
        run(&mut s, "redo");
        assert_eq!(s.doc.named_views["entry"], v2);
    }

    #[test]
    fn view_save_without_camera_errors() {
        // The parser leaves camera None; only the app can capture it.
        let mut s = Session::default();
        let err = s.run(parse("view save a").unwrap()).unwrap_err();
        assert!(err.to_string().contains("viewport camera"), "{err}");
        assert!(s.doc.named_views.is_empty());
    }

    #[test]
    fn view_restore_and_list_are_not_logged() {
        let mut s = Session::default();
        s.run(Command::ViewSave { name: "a".to_string(), camera: Some(sample_view(9.0)) })
            .unwrap();
        run(&mut s, "view a");
        run(&mut s, "view list");
        let log = s.save_log();
        assert_eq!(log.len(), 1, "only the save is logged");
        assert!(matches!(log[0], Command::ViewSave { .. }));
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
    fn group_ungroup_undo_redo() {
        let mut s = Session::default();
        run(&mut s, "box 0,0,0 1,1,1");
        run(&mut s, "box 5,0,0 1,1,1");
        run(&mut s, "box 10,0,0 1,1,1");

        let out = run(&mut s, "group last 2 boxes");
        assert_eq!(out.message, "grouped 2 object(s) as 'boxes'");
        let last2: std::collections::BTreeSet<ObjectId> =
            s.doc.last_ids(2).into_iter().collect();
        assert_eq!(s.doc.groups["boxes"], last2);

        // the group name works as a selector: move acts on the whole group
        let bb_before = s.doc.scene_aabb().unwrap();
        let out = run(&mut s, "move boxes 0,0,2");
        assert!(out.message.contains("2 object(s)"), "{}", out.message);
        assert_eq!(s.doc.scene_aabb().unwrap().max.z - bb_before.max.z, 2.0);
        run(&mut s, "undo");

        // auto-naming picks the first free groupN
        run(&mut s, "group last group-b");
        run(&mut s, "group all");
        assert!(s.doc.groups.contains_key("group1"), "{:?}", s.doc.groups.keys());

        // ungroup dissolves every group containing the ids; objects stay.
        // The last box sits in all three groups.
        let n = s.doc.len();
        let out = run(&mut s, "ungroup last");
        for name in ["boxes", "group-b", "group1"] {
            assert!(out.message.contains(name), "{}", out.message);
        }
        assert!(s.doc.groups.is_empty());
        assert_eq!(s.doc.len(), n);

        // undo restores the dissolved groups, then unwinds the group ops
        run(&mut s, "undo");
        assert_eq!(s.doc.groups.len(), 3);
        run(&mut s, "undo"); // un-group all
        assert!(!s.doc.groups.contains_key("group1"));
        run(&mut s, "undo"); // un-group group-b
        assert_eq!(s.doc.groups.len(), 1);
        run(&mut s, "redo");
        run(&mut s, "redo");
        assert_eq!(s.doc.groups.len(), 3);
        run(&mut s, "redo"); // re-ungroup
        assert!(s.doc.groups.is_empty());

        // group overwrite: undo restores the previous member set
        run(&mut s, "group last boxes");
        let prev = s.doc.groups["boxes"].clone();
        run(&mut s, "group last 2 boxes");
        assert_ne!(s.doc.groups["boxes"], prev);
        run(&mut s, "undo");
        assert_eq!(s.doc.groups["boxes"], prev);

        // ungroup with no matching group errors and leaves the doc untouched
        run(&mut s, "ungroup boxes");
        let err = s.run(parse("ungroup last").unwrap()).unwrap_err();
        assert!(err.to_string().contains("no group"), "{err}");
    }

    #[test]
    fn group_replay_is_stable() {
        let mut s = Session::default();
        for line in [
            "box 0,0,0 1,1,1",
            "box 5,0,0 1,1,1",
            "group last 2 boxes",
            "box 10,0,0 1,1,1",
            "group last", // auto-named group1
            "ungroup group1",
            "move boxes 0,0,3",
        ] {
            run(&mut s, line);
        }
        let json = crate::io::to_json(&s);
        assert!(json.contains("\"group\""), "{json}");
        let loaded = crate::io::from_json(&json).unwrap();
        assert_eq!(loaded.doc.groups, s.doc.groups);
        assert_eq!(loaded.doc.len(), s.doc.len());
        assert_eq!(crate::io::to_json(&loaded), json, "replay-stable");
        // deleting a member leaves the group entry; selectors filter it out
        run(&mut s, "delete last");
        assert!(s.doc.groups.contains_key("boxes"));
    }

    #[test]
    fn array_grid_counts_positions_undo() {
        let mut s = Session::default();
        run(&mut s, "box 0,0,0 0.4,0.4,3");
        let out = run(&mut s, "array last 5,3,1 3,4,0");
        assert_eq!(out.created.len(), 14); // 5*3*1 - 1 copies
        assert_eq!(s.doc.len(), 15);
        let bb = s.doc.scene_aabb().unwrap();
        // grid spans 4 bays x 3m and 2 bays x 4m plus the 0.4 column
        assert!((bb.min - DVec3::ZERO).length() < 1e-9);
        assert!((bb.max - DVec3::new(12.4, 8.4, 3.0)).length() < 1e-9);
        // every cell center is occupied
        for j in 0..3 {
            for i in 0..5 {
                let want = DVec3::new(0.2 + 3.0 * f64::from(i), 0.2 + 4.0 * f64::from(j), 1.5);
                assert!(
                    s.doc
                        .objects()
                        .any(|o| (o.geometry.aabb().center() - want).length() < 1e-9),
                    "missing cell {i},{j}"
                );
            }
        }
        run(&mut s, "undo");
        assert_eq!(s.doc.len(), 1, "undo deletes all copies");
        run(&mut s, "redo");
        assert_eq!(s.doc.len(), 15);

        // multi-target arrays copy every source
        run(&mut s, "undo");
        run(&mut s, "circle 20,0,0 1");
        let out = run(&mut s, "array last 2 2,2,1 30,30,0");
        assert_eq!(out.created.len(), 6); // 2 objects x 3 new cells
        assert_eq!(s.doc.len(), 8);

        // errors leave the doc untouched
        let n = s.doc.len();
        let err = s.run(parse("array last 0,2 1,0,0").unwrap()).unwrap_err();
        assert!(err.to_string().contains("at least 1"), "{err}");
        let err = s.run(parse("array last 1,1,1 1,0,0").unwrap()).unwrap_err();
        assert!(err.to_string().contains("no copies"), "{err}");
        assert_eq!(s.doc.len(), n);
    }

    #[test]
    fn polar_array_positions_and_undo() {
        let mut s = Session::default();
        // box centered at 10,0 — default center is its own AABB center, so
        // give an explicit center at the origin for a real orbit
        run(&mut s, "box 9,-1,0 2,2,1");
        let out = run(&mut s, "polararray last 4 0,0,0");
        assert_eq!(out.created.len(), 3);
        assert_eq!(s.doc.len(), 4);
        // full circle: copies at 90° steps land at (0,10), (-10,0), (0,-10)
        for want in [
            DVec3::new(10.0, 0.0, 0.5),
            DVec3::new(0.0, 10.0, 0.5),
            DVec3::new(-10.0, 0.0, 0.5),
            DVec3::new(0.0, -10.0, 0.5),
        ] {
            assert!(
                s.doc
                    .objects()
                    .any(|o| (o.geometry.aabb().center() - want).length() < 1e-9),
                "missing instance at {want}"
            );
        }
        run(&mut s, "undo");
        assert_eq!(s.doc.len(), 1);
        run(&mut s, "redo");
        assert_eq!(s.doc.len(), 4);

        // partial sweep: last copy lands exactly at the total angle
        let mut s = Session::default();
        run(&mut s, "box 9,-1,0 2,2,1");
        run(&mut s, "polararray last 3 0,0,0 180");
        let centers: Vec<DVec3> =
            s.doc.objects().map(|o| o.geometry.aabb().center()).collect();
        assert!(centers.iter().any(|c| (*c - DVec3::new(0.0, 10.0, 0.5)).length() < 1e-9));
        assert!(centers.iter().any(|c| (*c - DVec3::new(-10.0, 0.0, 0.5)).length() < 1e-9));

        // default center = targets' AABB center: copies coincide about itself
        let mut s = Session::default();
        run(&mut s, "box 0,0,0 2,2,2");
        run(&mut s, "polararray last 4");
        assert_eq!(s.doc.len(), 4);
        let bb = s.doc.scene_aabb().unwrap();
        assert!((bb.center() - DVec3::new(1.0, 1.0, 1.0)).length() < 1e-9);

        // count < 2 refuses
        let err = s.run(parse("polararray last 1").unwrap()).unwrap_err();
        assert!(err.to_string().contains("at least 2"), "{err}");
    }

    #[test]
    fn array_replay_reuses_ids() {
        let mut s = Session::default();
        run(&mut s, "box 0,0,0 0.4,0.4,3");
        run(&mut s, "array last 3,2,1 3,4,0");
        run(&mut s, "circle 20,0,0 1");
        run(&mut s, "polararray last 6 25,0,0");
        run(&mut s, "undo");
        run(&mut s, "redo");

        let log = s.save_log();
        // logged ops carry the minted ids
        assert!(matches!(&log[1], Command::Array { ids: Some(ids), .. } if ids.len() == 5));
        assert!(matches!(&log[3], Command::PolarArray { ids: Some(ids), .. } if ids.len() == 5));
        let replayed = Session::replay(log.clone()).unwrap();
        let a: Vec<_> = s.doc.objects().collect();
        let b: Vec<_> = replayed.doc.objects().collect();
        assert_eq!(a, b);
        assert_eq!(
            serde_json::to_string(&log).unwrap(),
            serde_json::to_string(&replayed.save_log()).unwrap()
        );
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

    fn mesh_volume(s: &Session) -> f64 {
        let obj = s.doc.objects().last().unwrap();
        let Geometry::Mesh(m) = &obj.geometry else { panic!("expected mesh") };
        kernel_mesh::signed_volume(m)
    }

    #[test]
    fn revolve_profile_full_circle_undo_redo() {
        let mut s = Session::default();
        // r=1 h=2 rectangle in the xz plane, touching the z axis
        run(&mut s, "polyline 0,0,0 1,0,0 1,0,2 0,0,2 closed");
        let out = run(&mut s, "revolve last");
        assert!(out.message.contains("360"), "{}", out.message);
        assert_eq!(s.doc.len(), 2); // profile kept + solid
        assert!((mesh_volume(&s) - 2.0 * std::f64::consts::PI).abs() < 0.1);
        run(&mut s, "undo");
        assert_eq!(s.doc.len(), 1);
        run(&mut s, "redo");
        assert_eq!(s.doc.len(), 2);
    }

    #[test]
    fn revolve_partial_angle_and_axis() {
        let mut s = Session::default();
        run(&mut s, "polyline 0,0,0 1,0,0 1,0,2 0,0,2 closed");
        run(&mut s, "name last prof");
        run(&mut s, "revolve prof 0,0,0 0,0,1 180");
        assert!((mesh_volume(&s) - std::f64::consts::PI).abs() < 0.05);

        // bad inputs leave the doc untouched
        let n = s.doc.len();
        let err = s.run(parse("revolve prof 400").unwrap()).unwrap_err();
        assert!(err.to_string().contains("(0, 360]"), "{err}");
        let err = s.run(parse("revolve prof 0,0,0 0,0,0 90").unwrap()).unwrap_err();
        assert!(err.to_string().contains("zero"), "{err}");
        assert_eq!(s.doc.len(), n);
    }

    #[test]
    fn revolve_rejects_open_curve() {
        let mut s = Session::default();
        run(&mut s, "line 0,0,0 5,0,0");
        let err = s.run(parse("revolve last").unwrap()).unwrap_err();
        assert!(err.to_string().contains("closed"), "{err}");
    }

    #[test]
    fn loft_two_rects_is_prism_undo_redo() {
        let mut s = Session::default();
        run(&mut s, "rect 0,0,0 2 2");
        run(&mut s, "rect 0,0,3 2 2");
        run(&mut s, "loft last 2");
        assert_eq!(s.doc.len(), 3); // profiles kept + solid
        assert!((mesh_volume(&s) - 12.0).abs() < 1e-9);
        run(&mut s, "undo");
        assert_eq!(s.doc.len(), 2);
        run(&mut s, "redo");
        assert_eq!(s.doc.len(), 3);

        // needs 2+ closed curves
        let err = s.run(parse("loft last").unwrap()).unwrap_err();
        assert!(err.to_string().contains("at least 2"), "{err}");
        run(&mut s, "line 0,0,0 1,0,0");
        let err = s.run(parse("loft all").unwrap()).unwrap_err();
        assert!(err.to_string().contains("closed") || err.to_string().contains("curves"), "{err}");
    }

    #[test]
    fn sweep_square_along_line_undo_redo() {
        let mut s = Session::default();
        run(&mut s, "rect -0.5,-0.5,0 1 1");
        run(&mut s, "name last prof");
        run(&mut s, "line 0,0,0 0,0,4");
        run(&mut s, "name last rail");
        run(&mut s, "sweep prof rail");
        assert_eq!(s.doc.len(), 3);
        assert!((mesh_volume(&s) - 4.0).abs() < 1e-9);
        run(&mut s, "undo");
        assert_eq!(s.doc.len(), 2);
        run(&mut s, "redo");
        assert_eq!(s.doc.len(), 3);

        // open profile / closed rail are rejected
        let err = s.run(parse("sweep rail rail").unwrap()).unwrap_err();
        assert!(err.to_string().contains("closed"), "{err}");
        run(&mut s, "circle 10,0,0 1");
        run(&mut s, "name last loop");
        let err = s.run(parse("sweep prof loop").unwrap()).unwrap_err();
        assert!(err.to_string().contains("open rail"), "{err}");
    }

    #[test]
    fn sweep2_between_parallel_rails_is_prism_undo_redo() {
        let mut s = Session::default();
        // Unit-square profile (width 1) between two rails 2 apart, running 4 up.
        run(&mut s, "rect -0.5,-0.5,0 1 1");
        run(&mut s, "name last prof");
        run(&mut s, "line -1,0,0 -1,0,4");
        run(&mut s, "name last ra");
        run(&mut s, "line 1,0,0 1,0,4");
        run(&mut s, "name last rb");
        run(&mut s, "sweep2 prof ra rb");
        assert_eq!(s.doc.len(), 4);
        assert!((mesh_volume(&s) - 8.0).abs() < 1e-6, "{}", mesh_volume(&s));
        run(&mut s, "undo");
        assert_eq!(s.doc.len(), 3);
        run(&mut s, "redo");
        assert_eq!(s.doc.len(), 4);

        // open profile is rejected
        let err = s.run(parse("sweep2 ra ra rb").unwrap()).unwrap_err();
        assert!(err.to_string().contains("closed"), "{err}");
    }

    #[test]
    fn pipe_straight_line_is_cylinder_undo_redo() {
        let mut s = Session::default();
        run(&mut s, "line 0,0,0 0,0,5");
        run(&mut s, "pipe last 1");
        assert_eq!(s.doc.len(), 2);
        // n-gon cross section under-fills the true circle; stay under 2%.
        let v = mesh_volume(&s);
        let ideal = 5.0 * std::f64::consts::PI;
        assert!(v > 0.0 && v <= ideal && (ideal - v) / ideal < 0.02, "{v}");
        run(&mut s, "undo");
        assert_eq!(s.doc.len(), 1);
        run(&mut s, "redo");
        assert_eq!(s.doc.len(), 2);

        let err = s.run(parse("pipe last 0").unwrap()).unwrap_err();
        assert!(err.to_string().contains("positive"), "{err}");
    }

    #[test]
    fn railrevolve_undo_redo_and_axis_check() {
        let mut s = Session::default();
        // Profile in the xz plane (spans the z axis it revolves about).
        run(&mut s, "polyline 2,0,0 3,0,0 3,0,1 2,0,1 closed");
        run(&mut s, "name last prof");
        run(&mut s, "circle 0,0,0 2.5");
        run(&mut s, "name last rail");
        run(&mut s, "railrevolve prof rail 0,0,0 0,0,1");
        assert_eq!(s.doc.len(), 3);
        assert!(mesh_volume(&s) > 0.0);
        run(&mut s, "undo");
        assert_eq!(s.doc.len(), 2);
        run(&mut s, "redo");
        assert_eq!(s.doc.len(), 3);

        let err = s.run(parse("railrevolve prof rail 0,0,0 0,0,0").unwrap()).unwrap_err();
        assert!(err.to_string().contains("axis"), "{err}");
    }

    #[test]
    fn solids_replay_stability() {
        let mut s = Session::default();
        run(&mut s, "polyline 0.2,0,0 1,0,0 0.8,0,1.5 0.3,0,2 0.2,0,2 closed");
        run(&mut s, "revolve last 300");
        run(&mut s, "rect 4,0,0 2 2");
        run(&mut s, "rect 4.5,0.5,2 1 1");
        run(&mut s, "loft last 2");
        run(&mut s, "rect -8.5,-0.5,0 1 1");
        run(&mut s, "name last prof");
        run(&mut s, "line -8,0,0 -8,0,3");
        run(&mut s, "name last rail");
        run(&mut s, "sweep prof rail");
        run(&mut s, "line -12,-1,0 -12,-1,3");
        run(&mut s, "name last ra");
        run(&mut s, "line -12,1,0 -12,1,3");
        run(&mut s, "name last rb");
        run(&mut s, "sweep2 prof ra rb");
        run(&mut s, "line -16,0,0 -16,0,4");
        run(&mut s, "pipe last 1 0.4");
        run(&mut s, "undo");
        run(&mut s, "redo");

        let log = s.save_log();
        // logged ops carry minted ids
        assert!(matches!(&log[1], Command::Revolve { id: Some(_), .. }));
        assert!(matches!(&log[4], Command::Loft { id: Some(_), .. }));
        assert!(log.iter().any(|c| matches!(c, Command::Sweep { id: Some(_), .. })));
        assert!(log.iter().any(|c| matches!(c, Command::Sweep2 { id: Some(_), .. })));
        assert!(log.iter().any(|c| matches!(c, Command::Pipe { id: Some(_), .. })));
        let replayed = Session::replay(log.clone()).unwrap();
        let a: Vec<_> = s.doc.objects().collect();
        let b: Vec<_> = replayed.doc.objects().collect();
        assert_eq!(a, b);
        assert_eq!(
            serde_json::to_string(&log).unwrap(),
            serde_json::to_string(&replayed.save_log()).unwrap()
        );
        // and the file format round-trips byte-identically
        let json1 = crate::io::to_json(&s);
        let json2 = crate::io::to_json(&crate::io::from_json(&json1).unwrap());
        assert_eq!(json1, json2);
    }

    #[test]
    fn section_plan_exec_undo_redo() {
        let mut s = Session::default();
        // Courtyard massing: 10x8 block minus a 4x4 through-cut.
        run(&mut s, "box 0,0,0 10,8,3");
        run(&mut s, "box 3,2,-0.5 4,4,4");
        run(&mut s, "difference last 2 last");
        assert_eq!(s.doc.len(), 1);
        run(&mut s, "name last court");

        let out = run(&mut s, "plan 1.5");
        assert!(out.message.contains("'sections'"), "{}", out.message);
        assert!(s.doc.layers.contains_key("sections"));
        assert!(s.doc.layers.contains_key("sections-proj"), "projected edges below");
        // Cut lineweight is heavier than projected lineweight.
        assert!(
            s.doc.layers["sections"].lineweight_mm > s.doc.layers["sections-proj"].lineweight_mm
        );
        // Two closed cut loops on "sections" (outer outline + courtyard hole),
        // both at the cut height; the rest are open projected edges below it.
        let mut cut_loops = 0;
        let mut proj_edges = 0;
        for id in &out.created {
            let obj = s.doc.get(*id).unwrap();
            match &obj.geometry {
                Geometry::Curve(Curve::Polyline { points, closed: true }) => {
                    assert_eq!(obj.layer, "sections");
                    assert!(points.iter().all(|p| (p.z - 1.5).abs() < 1e-9));
                    cut_loops += 1;
                }
                Geometry::Curve(Curve::Polyline { points, closed: false }) => {
                    assert_eq!(obj.layer, "sections-proj");
                    // projected onto the cut plane
                    assert!(points.iter().all(|p| (p.z - 1.5).abs() < 1e-9));
                    proj_edges += 1;
                }
                g => panic!("unexpected geometry {g:?}"),
            }
        }
        assert_eq!(cut_loops, 2, "outer outline + courtyard hole");
        assert!(proj_edges > 0, "geometry below the cut projects edges");
        let created_len = out.created.len();
        // undo removes every created curve AND both layers this cut created
        run(&mut s, "undo");
        assert_eq!(s.doc.len(), 1);
        assert!(!s.doc.layers.contains_key("sections"));
        assert!(!s.doc.layers.contains_key("sections-proj"));
        run(&mut s, "redo");
        assert_eq!(s.doc.len(), 1 + created_len);
        assert!(s.doc.layers.contains_key("sections"));

        // vertical section through the courtyard: two wall cut loops (+ any
        // projected edges beyond the plane)
        let out = run(&mut s, "section court 0,4,0 0,1,0");
        let cut_loops = out
            .created
            .iter()
            .filter(|id| s.doc.get(**id).unwrap().layer == "sections")
            .count();
        assert_eq!(cut_loops, 2, "wall on each side of the courtyard");

        // misses and non-meshes error without touching the document
        let n = s.doc.len();
        let err = s.run(parse("plan 99").unwrap()).unwrap_err();
        assert!(err.to_string().contains("misses"), "{err}");
        run(&mut s, "circle 20,0,0 1");
        let err = s.run(parse("section last 0,0,0 0,0,1").unwrap()).unwrap_err();
        assert!(err.to_string().contains("meshes"), "{err}");
        assert_eq!(s.doc.len(), n + 1);
    }

    #[test]
    fn elevation_exec_undo_redo() {
        let mut s = Session::default();
        // Two boxes side by side; south elevation looks north onto the y=min
        // face. Each box outlines to 8 non-degenerate projected edges.
        run(&mut s, "box 0,0,0 2,2,3");
        run(&mut s, "box 5,0,0 2,2,3");
        let out = run(&mut s, "elevation south");
        assert_eq!(out.created.len(), 16, "8 outline edges per box");
        assert!(s.doc.layers.contains_key("elevations"));
        for id in &out.created {
            let obj = s.doc.get(*id).unwrap();
            assert_eq!(obj.layer, "elevations");
            let Geometry::Curve(Curve::Polyline { points, closed: false }) = &obj.geometry
            else {
                panic!("expected open polyline, got {:?}", obj.geometry)
            };
            // south elevation flattens onto the y = 0 plane (both boxes' min.y)
            assert!(points.iter().all(|p| p.y.abs() < 1e-9), "{points:?}");
        }
        run(&mut s, "undo");
        assert_eq!(s.doc.len(), 2);
        assert!(!s.doc.layers.contains_key("elevations"));
        run(&mut s, "redo");
        assert!(s.doc.layers.contains_key("elevations"));

        // depth pushes the plane outward along +... no, for south the normal is
        // -Y, so depth moves the plane to more negative y.
        run(&mut s, "undo");
        let out = run(&mut s, "elevation south 1");
        let obj = s.doc.get(out.created[0]).unwrap();
        let Geometry::Curve(Curve::Polyline { points, .. }) = &obj.geometry else {
            unreachable!()
        };
        assert!(points.iter().all(|p| (p.y + 1.0).abs() < 1e-9), "depth offset");

        // empty document errors
        let mut empty = Session::default();
        let err = empty.run(parse("elevation east").unwrap()).unwrap_err();
        assert!(err.to_string().contains("0 objects"), "{err}");
    }

    #[test]
    fn elevation_replay_stability() {
        let mut s = Session::default();
        run(&mut s, "box 0,0,0 2,2,3");
        run(&mut s, "elevation west 0.5");
        let log = s.save_log();
        assert!(matches!(&log[1], Command::Elevation { ids: Some(ids), .. } if !ids.is_empty()));
        let replayed = Session::replay(log.clone()).unwrap();
        assert_eq!(
            s.doc.objects().collect::<Vec<_>>(),
            replayed.doc.objects().collect::<Vec<_>>()
        );
        assert_eq!(
            serde_json::to_string(&log).unwrap(),
            serde_json::to_string(&replayed.save_log()).unwrap()
        );
    }

    #[test]
    fn section_replay_stability() {
        let mut s = Session::default();
        run(&mut s, "box 0,0,0 10,8,3");
        run(&mut s, "box 3,2,-0.5 4,4,4");
        run(&mut s, "difference last 2 last");
        run(&mut s, "name last court");
        run(&mut s, "plan 1.5");
        run(&mut s, "section court 0,4,0 0,1,0");
        run(&mut s, "undo");
        run(&mut s, "redo");

        let log = s.save_log();
        // logged ops carry the minted loop ids
        assert!(matches!(&log[4], Command::Plan { ids: Some(ids), .. } if ids.len() >= 2));
        assert!(matches!(&log[5], Command::Section { ids: Some(ids), .. } if ids.len() >= 2));
        let replayed = Session::replay(log.clone()).unwrap();
        let a: Vec<_> = s.doc.objects().collect();
        let b: Vec<_> = replayed.doc.objects().collect();
        assert_eq!(a, b);
        assert_eq!(
            serde_json::to_string(&log).unwrap(),
            serde_json::to_string(&replayed.save_log()).unwrap()
        );
        // and the file format round-trips byte-identically
        let json1 = crate::io::to_json(&s);
        let json2 = crate::io::to_json(&crate::io::from_json(&json1).unwrap());
        assert_eq!(json1, json2);
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
    fn units_exec_undo_redo_and_dim_message() {
        let mut s = Session::default();
        assert_eq!(s.doc.units, Units::M);
        // dim message respects the document unit
        let out = run(&mut s, "dim 0,0 12ft,0 -2");
        assert!(out.message.contains("3.66 m"), "{}", out.message);

        let out = run(&mut s, "units ftin");
        assert_eq!(s.doc.units, Units::FtIn);
        assert!(out.message.contains("ftin"), "{}", out.message);
        let out = run(&mut s, "dim 0,0 12ft6in,0 -2");
        assert!(out.message.contains("12'-6\""), "{}", out.message);

        run(&mut s, "undo"); // un-dim
        run(&mut s, "undo"); // un-units
        assert_eq!(s.doc.units, Units::M);
        run(&mut s, "redo");
        assert_eq!(s.doc.units, Units::FtIn);
    }

    #[test]
    fn units_replay_stability() {
        let mut s = Session::default();
        run(&mut s, "units ft");
        run(&mut s, "box 0,0,0 12ft,12ft,9ft");
        run(&mut s, "dim 0,0 12ft,0 -2");
        let log = s.save_log();
        let replayed = Session::replay(log.clone()).unwrap();
        assert_eq!(replayed.doc.units, Units::Ft);
        let a: Vec<_> = s.doc.objects().collect();
        let b: Vec<_> = replayed.doc.objects().collect();
        assert_eq!(a, b);
        assert_eq!(
            serde_json::to_string(&log).unwrap(),
            serde_json::to_string(&replayed.save_log()).unwrap()
        );
    }

    #[test]
    fn amend_box_size_rebuilds_downstream_boolean() {
        let mut s = Session::default();
        run(&mut s, "box 0,0,0 5,5,3");
        run(&mut s, "box 1,1,-1 2,2,5");
        run(&mut s, "difference last 2 last");
        assert_eq!(s.doc.len(), 1);
        assert!((mesh_volume(&s) - (75.0 - 12.0)).abs() < 1e-6); // 5*5*3 - 2*2*3

        let out = run(&mut s, "amend 0 box 0,0,0 8,8,3");
        assert!(out.message.contains("amended step 0"), "{}", out.message);
        assert_eq!(s.doc.len(), 1);
        // Bigger slab, same hole: the downstream difference re-resolved.
        assert!((mesh_volume(&s) - (192.0 - 12.0)).abs() < 1e-6); // 8*8*3 - 2*2*3
        let (entries, cursor) = s.history();
        assert_eq!(entries, ["box", "box", "difference"]);
        assert_eq!(cursor, 3);

        // The rebuilt log is still undoable.
        run(&mut s, "undo");
        assert_eq!(s.doc.len(), 2);
        run(&mut s, "redo");
        assert!((mesh_volume(&s) - 180.0).abs() < 1e-6);
    }

    #[test]
    fn failed_amend_restores_prior_state() {
        let mut s = Session::default();
        run(&mut s, "box 0,0,0 5,5,3");
        run(&mut s, "box 1,1,-1 2,2,5");
        run(&mut s, "difference last 2 last");
        let json_before = crate::io::to_json(&s);
        let vol_before = mesh_volume(&s);

        // A line cannot feed the boolean: replay fails at the difference step.
        let err = s.run(parse("amend 1 line 0,0,0 1,0,0").unwrap()).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("step 2") && msg.contains("difference"), "{msg}");

        // Session untouched: same objects, same log, still fully usable.
        assert_eq!(crate::io::to_json(&s), json_before);
        assert!((mesh_volume(&s) - vol_before).abs() < 1e-12);
        run(&mut s, "undo");
        assert_eq!(s.doc.len(), 2);
    }

    #[test]
    fn amend_rejects_bad_step_and_unlogged_command() {
        let mut s = Session::default();
        run(&mut s, "box 0,0,0 1,1,1");
        let err = s.run(parse("amend 3 box 0,0,0 2,2,2").unwrap()).unwrap_err();
        assert!(err.to_string().contains("history has 1 step(s)"), "{err}");
        let err = s.run(parse("amend 0 undo").unwrap()).unwrap_err();
        assert!(err.to_string().contains("not a geometry command"), "{err}");
        assert_eq!(s.doc.len(), 1);
        assert_eq!(s.save_log().len(), 1);
    }

    #[test]
    fn amend_only_touches_the_effective_log() {
        // An undone tail beyond the cursor is dropped by amend, exactly like
        // running any new command would drop it.
        let mut s = Session::default();
        run(&mut s, "box 0,0,0 1,1,1");
        run(&mut s, "box 5,0,0 1,1,1");
        run(&mut s, "undo");
        run(&mut s, "amend 0 box 0,0,0 3,3,3");
        assert_eq!(s.doc.len(), 1);
        assert!((mesh_volume(&s) - 27.0).abs() < 1e-9);
        assert_eq!(s.save_log().len(), 1);
        let err = s.run(Command::Redo).unwrap_err();
        assert_eq!(err, ExecError::NothingToRedo);
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
    fn split_replaces_curve_with_pieces_and_undoes() {
        let mut s = Session::default();
        run(&mut s, "line 0,0 10,0");
        let original = s.doc.objects().next().unwrap().clone();
        let out = run(&mut s, "split last 4,3"); // nearest point on curve = 4,0
        assert_eq!(out.created.len(), 2);
        assert_eq!(s.doc.len(), 2);
        let lens: Vec<f64> = s
            .doc
            .objects()
            .map(|o| {
                let Geometry::Curve(Curve::Line { a, b }) = &o.geometry else { panic!() };
                (*b - *a).length()
            })
            .collect();
        assert_eq!(lens, [4.0, 6.0]);

        run(&mut s, "undo");
        assert_eq!(s.doc.len(), 1);
        assert_eq!(s.doc.objects().next().unwrap(), &original);
        run(&mut s, "redo");
        assert_eq!(s.doc.len(), 2);

        // closed curves and meshes refuse
        let mut s = Session::default();
        run(&mut s, "circle 0,0 2");
        let err = s.run(parse("split last 2,0").unwrap()).unwrap_err();
        assert!(err.to_string().contains("closed"), "{err}");
        run(&mut s, "box 5,0,0 1,1,1");
        let err = s.run(parse("split last 5,0").unwrap()).unwrap_err();
        assert!(err.to_string().contains("curves"), "{err}");
    }

    #[test]
    fn trim_keeps_piece_nearest_keep_point() {
        let mut s = Session::default();
        run(&mut s, "line 0,0 10,0");
        run(&mut s, "name last wall");
        run(&mut s, "line 4,-1 4,1");
        let out = run(&mut s, "trim wall last 0,0"); // keep the left piece
        assert!(out.message.contains("kept 1 of 2"), "{}", out.message);
        assert_eq!(s.doc.len(), 2);
        let kept = s.doc.find_named("wall");
        assert_eq!(kept.len(), 1, "trimmed piece keeps the name");
        let Geometry::Curve(Curve::Line { a, b }) = &s.doc.get(kept[0]).unwrap().geometry
        else {
            panic!()
        };
        assert!(a.distance(DVec3::ZERO) < 1e-9);
        assert!(b.distance(DVec3::new(4.0, 0.0, 0.0)) < 1e-9);

        run(&mut s, "undo");
        let Geometry::Curve(Curve::Line { b, .. }) =
            &s.doc.get(s.doc.find_named("wall")[0]).unwrap().geometry
        else {
            panic!()
        };
        assert!(b.distance(DVec3::new(10.0, 0.0, 0.0)) < 1e-9, "undo restores full line");

        // circle trimmed by a crossing line keeps the arc nearest the keep point
        run(&mut s, "circle 20,0 2");
        run(&mut s, "line 20,-5 20,5");
        run(&mut s, "trim last 2 last 17,0"); // keep the left arc
        let arcs: Vec<_> = s
            .doc
            .objects()
            .filter_map(|o| match &o.geometry {
                Geometry::Curve(c @ Curve::Arc { .. }) => Some(c.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(arcs.len(), 1);
        let Curve::Arc { start, end, .. } = arcs[0] else { panic!() };
        assert!((end - start - std::f64::consts::PI).abs() < 1e-9, "half circle kept");

        // no intersections → error, doc untouched
        let n = s.doc.len();
        run(&mut s, "line 100,0 110,0");
        run(&mut s, "line 100,5 110,5");
        let err = s.run(parse("trim last 2 last 100,0").unwrap()).unwrap_err();
        assert!(err.to_string().contains("do not intersect"), "{err}");
        assert_eq!(s.doc.len(), n + 2);
    }

    #[test]
    fn extend_open_curves_and_undo() {
        let mut s = Session::default();
        run(&mut s, "line 0,0 10,0");
        run(&mut s, "extend last 2");
        let Geometry::Curve(Curve::Line { a, b }) = &s.doc.objects().next().unwrap().geometry
        else {
            panic!()
        };
        assert!(a.distance(DVec3::new(-2.0, 0.0, 0.0)) < 1e-9);
        assert!(b.distance(DVec3::new(12.0, 0.0, 0.0)) < 1e-9);
        run(&mut s, "undo");
        let Geometry::Curve(Curve::Line { a, .. }) = &s.doc.objects().next().unwrap().geometry
        else {
            panic!()
        };
        assert!(a.distance(DVec3::ZERO) < 1e-9);

        // closed curve refuses, doc untouched; negative distance refuses
        run(&mut s, "circle 20,0 2");
        let err = s.run(parse("extend last 1").unwrap()).unwrap_err();
        assert!(err.to_string().contains("open"), "{err}");
        let err = s.run(parse("extend last -1").unwrap()).unwrap_err();
        assert!(err.to_string().contains("positive"), "{err}");
    }

    #[test]
    fn join_consumes_curves_into_polyline() {
        let mut s = Session::default();
        run(&mut s, "line 0,0 4,0");
        run(&mut s, "line 4,0 4,4");
        run(&mut s, "line 4,4 0,4");
        run(&mut s, "line 0,4 0,0");
        let out = run(&mut s, "join last 4");
        assert!(out.message.contains("closed"), "{}", out.message);
        assert_eq!(s.doc.len(), 1);
        let Geometry::Curve(c) = &s.doc.objects().next().unwrap().geometry else { panic!() };
        assert!(c.is_closed());
        // a joined closed square extrudes
        run(&mut s, "extrude last 3");
        assert_eq!(s.doc.len(), 2);
        run(&mut s, "undo"); // un-extrude
        run(&mut s, "undo"); // un-join → four lines restored
        assert_eq!(s.doc.len(), 4);

        // disjoint curves refuse
        run(&mut s, "line 100,0 104,0");
        run(&mut s, "line 200,0 204,0");
        let err = s.run(parse("join last 2").unwrap()).unwrap_err();
        assert!(err.to_string().contains("touch"), "{err}");
    }

    #[test]
    fn fillet_trims_lines_and_adds_arc() {
        let mut s = Session::default();
        run(&mut s, "line -2,0 8,0");
        run(&mut s, "line 0,-2 0,8");
        let out = run(&mut s, "fillet last 2 2");
        assert!(out.message.contains("arc"), "{}", out.message);
        assert_eq!(s.doc.len(), 3); // two trimmed lines + arc
        let arc = s
            .doc
            .objects()
            .find_map(|o| match &o.geometry {
                Geometry::Curve(c @ Curve::Arc { .. }) => Some(c.clone()),
                _ => None,
            })
            .expect("fillet arc present");
        let Curve::Arc { center, radius, .. } = arc else { panic!() };
        assert!(center.distance(DVec3::new(2.0, 2.0, 0.0)) < 1e-9);
        assert!((radius - 2.0).abs() < 1e-9);

        run(&mut s, "undo"); // arc gone, lines restored exactly
        assert_eq!(s.doc.len(), 2);
        let Geometry::Curve(Curve::Line { a, .. }) = &s.doc.objects().next().unwrap().geometry
        else {
            panic!()
        };
        assert!(a.distance(DVec3::new(-2.0, 0.0, 0.0)) < 1e-9);
        run(&mut s, "redo");
        assert_eq!(s.doc.len(), 3);

        // parallel lines refuse; non-lines refuse
        let mut s = Session::default();
        run(&mut s, "line 0,0 5,0");
        run(&mut s, "line 0,1 5,1");
        let err = s.run(parse("fillet last 2 0.5").unwrap()).unwrap_err();
        assert!(err.to_string().contains("parallel"), "{err}");
        run(&mut s, "circle 10,0 1");
        let err = s.run(parse("fillet last 2 0.5").unwrap()).unwrap_err();
        assert!(err.to_string().contains("line"), "{err}");
    }

    #[test]
    fn curve_edit_replay_stable() {
        let mut s = Session::default();
        run(&mut s, "line -2,0 8,0");
        run(&mut s, "line 0,-2 0,8");
        run(&mut s, "fillet last 2 2");
        run(&mut s, "line 20,0 30,0");
        run(&mut s, "split last 24,1");
        run(&mut s, "extend last 0.5");
        run(&mut s, "line 40,0 44,0");
        run(&mut s, "line 44,0 44,4");
        run(&mut s, "join last 2");
        run(&mut s, "line 50,0 60,0");
        run(&mut s, "name last wall");
        run(&mut s, "line 55,-1 55,1");
        run(&mut s, "trim wall last 50,0");
        run(&mut s, "undo");
        run(&mut s, "redo");

        let log = s.save_log();
        let replayed = Session::replay(log.clone()).unwrap();
        let a: Vec<_> = s.doc.objects().collect();
        let b: Vec<_> = replayed.doc.objects().collect();
        assert_eq!(a, b);
        assert_eq!(
            serde_json::to_string(&log).unwrap(),
            serde_json::to_string(&replayed.save_log()).unwrap()
        );
    }

    #[test]
    fn layer_switch_creates_and_assigns_new_objects() {
        let mut s = Session::default();
        run(&mut s, "box 0,0,0 1,1,1");
        assert_eq!(s.doc.objects().next().unwrap().layer, "default");

        let out = run(&mut s, "layer walls");
        assert!(out.message.contains("created"), "{}", out.message);
        assert_eq!(s.doc.current_layer, "walls");
        run(&mut s, "box 5,0,0 1,1,1");
        let layers: Vec<_> = s.doc.objects().map(|o| o.layer.clone()).collect();
        assert_eq!(layers, ["default", "walls"]);

        run(&mut s, "undo"); // un-create second box
        run(&mut s, "undo"); // un-switch: layer removed, current back to default
        assert_eq!(s.doc.current_layer, "default");
        assert!(!s.doc.layers.contains_key("walls"));
        run(&mut s, "redo");
        assert_eq!(s.doc.current_layer, "walls");
        assert!(s.doc.layers.contains_key("walls"));
    }

    #[test]
    fn tolayer_moves_objects_and_undoes() {
        let mut s = Session::default();
        run(&mut s, "box 0,0,0 1,1,1");
        run(&mut s, "circle 5,0,0 1");
        run(&mut s, "tolayer last 2 structure");
        assert!(s.doc.layers.contains_key("structure"));
        assert!(s.doc.objects().all(|o| o.layer == "structure"));
        assert_eq!(s.doc.current_layer, "default", "tolayer does not switch");

        run(&mut s, "undo");
        assert!(s.doc.objects().all(|o| o.layer == "default"));
        assert!(!s.doc.layers.contains_key("structure"), "created layer dropped");
        run(&mut s, "redo");
        assert!(s.doc.objects().all(|o| o.layer == "structure"));
    }

    #[test]
    fn layercolor_hide_show_undo() {
        let mut s = Session::default();
        run(&mut s, "layer walls");
        run(&mut s, "layercolor walls 0.8,0.2,0.1");
        let style = &s.doc.layers["walls"];
        assert_eq!(style.color, Some([0.8, 0.2, 0.1, 1.0]));
        assert!(style.visible);

        run(&mut s, "hide walls");
        assert!(!s.doc.layers["walls"].visible);
        run(&mut s, "show walls");
        assert!(s.doc.layers["walls"].visible);

        run(&mut s, "undo"); // un-show
        assert!(!s.doc.layers["walls"].visible);
        run(&mut s, "undo"); // un-hide
        assert!(s.doc.layers["walls"].visible);
        run(&mut s, "undo"); // un-color
        assert_eq!(s.doc.layers["walls"].color, None);
    }

    #[test]
    fn layer_style_commands_require_existing_layer() {
        let mut s = Session::default();
        for line in ["layercolor ghost 1,0,0", "hide ghost", "show ghost"] {
            let err = s.run(parse(line).unwrap()).unwrap_err();
            assert!(err.to_string().contains("no layer 'ghost'"), "{line}: {err}");
            assert!(err.to_string().contains("layer ghost"), "hint present: {err}");
        }
    }

    #[test]
    fn layerweight_parse_exec_undo_redo() {
        let mut s = Session::default();
        run(&mut s, "layer walls");
        // Default lineweight.
        assert!((s.doc.layers["walls"].lineweight_mm - 0.18).abs() < 1e-9);
        // Set a heavier weight.
        let out = run(&mut s, "layerweight walls 0.35");
        assert!(out.message.contains("0.350"), "{}", out.message);
        assert!((s.doc.layers["walls"].lineweight_mm - 0.35).abs() < 1e-9);
        // Undo restores the default.
        run(&mut s, "undo");
        assert!((s.doc.layers["walls"].lineweight_mm - 0.18).abs() < 1e-9);
        // Redo re-applies.
        run(&mut s, "redo");
        assert!((s.doc.layers["walls"].lineweight_mm - 0.35).abs() < 1e-9);
    }

    #[test]
    fn layerweight_parse_rejects_bad_inputs() {
        assert!(parse("layerweight walls").is_err(), "missing mm arg");
        assert!(parse("layerweight walls 0").is_err(), "zero not allowed");
        assert!(parse("layerweight walls -0.1").is_err(), "negative not allowed");
        // Valid parse.
        assert_eq!(
            parse("layerweight walls 0.18").unwrap(),
            Command::LayerWeight { layer: "walls".into(), mm: 0.18 }
        );
    }

    #[test]
    fn layerweight_requires_existing_layer() {
        let mut s = Session::default();
        let err = s.run(parse("layerweight ghost 0.5").unwrap()).unwrap_err();
        assert!(err.to_string().contains("no layer 'ghost'"), "{err}");
    }

    #[test]
    fn layerweight_replay_stable() {
        let mut s = Session::default();
        run(&mut s, "layer walls");
        run(&mut s, "layerweight walls 0.35");
        run(&mut s, "layer sections");
        run(&mut s, "layerweight sections 0.50");
        let log = s.save_log();
        let replayed = Session::replay(log.clone()).unwrap();
        assert!(
            (replayed.doc.layers["walls"].lineweight_mm - 0.35).abs() < 1e-9
        );
        assert!(
            (replayed.doc.layers["sections"].lineweight_mm - 0.50).abs() < 1e-9
        );
        assert_eq!(
            serde_json::to_string(&log).unwrap(),
            serde_json::to_string(&replayed.save_log()).unwrap(),
            "replay-stable log"
        );
    }

    #[test]
    fn sections_layer_default_lineweight_is_heavier() {
        let mut s = Session::default();
        run(&mut s, "box 0,0,0 4,4,3");
        run(&mut s, "plan 1.5");
        // cut layer heavier than the projected-edge layer.
        assert!((s.doc.layers["sections"].lineweight_mm - CUT_WEIGHT_MM).abs() < 1e-9);
        assert!((s.doc.layers["sections-proj"].lineweight_mm - PROJ_WEIGHT_MM).abs() < 1e-9);
        assert!(
            s.doc.layers["sections"].lineweight_mm > s.doc.layers["sections-proj"].lineweight_mm
        );
    }

    #[test]
    fn hideobj_showobj_undo_redo() {
        let mut s = Session::default();
        run(&mut s, "box 0,0,0 1,1,1");
        run(&mut s, "box 5,0,0 1,1,1");
        let vis = |s: &Session| -> Vec<bool> { s.doc.objects().map(|o| o.visible).collect() };
        assert_eq!(vis(&s), [true, true]);

        let out = run(&mut s, "hideobj last");
        assert_eq!(out.message, "hid 1 object(s)");
        assert_eq!(vis(&s), [true, false]);

        run(&mut s, "showobj all");
        assert_eq!(vis(&s), [true, true]);

        run(&mut s, "undo"); // un-show: second box hidden again
        assert_eq!(vis(&s), [true, false]);
        run(&mut s, "undo"); // un-hide
        assert_eq!(vis(&s), [true, true]);
        run(&mut s, "redo");
        assert_eq!(vis(&s), [true, false]);
        run(&mut s, "redo");
        assert_eq!(vis(&s), [true, true]);
    }

    #[test]
    fn hideobj_replay_stable() {
        let mut s = Session::default();
        run(&mut s, "box 0,0,0 1,1,1");
        run(&mut s, "box 5,0,0 1,1,1");
        run(&mut s, "name last cube");
        run(&mut s, "hideobj cube");
        run(&mut s, "showobj cube");
        run(&mut s, "hideobj last 2");

        let log = s.save_log();
        let replayed = Session::replay(log.clone()).unwrap();
        let a: Vec<_> = s.doc.objects().collect();
        let b: Vec<_> = replayed.doc.objects().collect();
        assert_eq!(a, b);
        assert!(replayed.doc.objects().all(|o| !o.visible));
        assert_eq!(
            serde_json::to_string(&log).unwrap(),
            serde_json::to_string(&replayed.save_log()).unwrap()
        );
    }

    #[test]
    fn boolean_result_keeps_target_layer() {
        let mut s = Session::default();
        run(&mut s, "layer structure");
        run(&mut s, "box 0,0,0 10,10,3");
        run(&mut s, "layer default");
        run(&mut s, "box 3,3,-1 4,4,5");
        run(&mut s, "difference last 2 last");
        assert_eq!(s.doc.objects().next().unwrap().layer, "structure");
    }

    #[test]
    fn layer_replay_stable() {
        let mut s = Session::default();
        run(&mut s, "box 0,0,0 1,1,1");
        run(&mut s, "layer walls");
        run(&mut s, "layercolor walls 0.2,0.6,0.9");
        run(&mut s, "box 5,0,0 1,1,1");
        run(&mut s, "tolayer last 2 slab");
        run(&mut s, "hide slab");
        run(&mut s, "show slab");
        run(&mut s, "hide walls");

        let log = s.save_log();
        let replayed = Session::replay(log.clone()).unwrap();
        let a: Vec<_> = s.doc.objects().collect();
        let b: Vec<_> = replayed.doc.objects().collect();
        assert_eq!(a, b);
        assert_eq!(s.doc.layers, replayed.doc.layers);
        assert_eq!(s.doc.current_layer, replayed.doc.current_layer);
        assert_eq!(
            serde_json::to_string(&log).unwrap(),
            serde_json::to_string(&replayed.save_log()).unwrap()
        );
    }

    #[test]
    fn history_lists_ops_and_cursor() {
        let mut s = Session::default();
        run(&mut s, "box 0,0,0 1,1,1");
        run(&mut s, "move last 1,0,0");
        run(&mut s, "circle 5,0,0 1");
        let (entries, cursor) = s.history();
        assert_eq!(entries, ["box", "move", "circle"]);
        assert_eq!(cursor, 3);

        run(&mut s, "undo");
        let (entries, cursor) = s.history();
        assert_eq!(entries, ["box", "move", "circle"], "undo keeps the list");
        assert_eq!(cursor, 2);

        // undo/redo themselves never appear in history
        run(&mut s, "redo");
        assert_eq!(s.history().0.len(), 3);
    }

    #[test]
    fn jump_reproduces_exact_documents() {
        let mut s = Session::default();
        let lines = [
            "box 0,0,0 5,5,3",
            "move last 1,2,0",
            "circle 10,0,0 2",
            "extrude last 4",
            "delete last",
        ];
        let mut snapshots: Vec<Vec<SceneObject>> = vec![Vec::new()];
        for line in lines {
            run(&mut s, line);
            snapshots.push(s.doc.objects().cloned().collect());
        }

        // jump backwards and forwards, comparing full object state each time
        for step in [2usize, 0, 4, 1, 5, 3] {
            let expected_moves = step.abs_diff(s.history().1);
            let moved = s.jump_to(step).unwrap();
            assert_eq!(moved, expected_moves);
            assert_eq!(s.history().1, step);
            let objs: Vec<_> = s.doc.objects().cloned().collect();
            assert_eq!(objs, snapshots[step], "step {step}");
        }
        // clamped past the end
        assert_eq!(s.jump_to(99).unwrap(), 2);
        assert_eq!(s.history().1, 5);
        // no-op jump
        assert_eq!(s.jump_to(5).unwrap(), 0);
    }

    #[test]
    fn jump_is_replay_stable() {
        let mut s = Session::default();
        run(&mut s, "box 0,0,0 5,5,3");
        run(&mut s, "box 3,3,-1 4,4,5");
        run(&mut s, "difference last 2 last");
        run(&mut s, "circle 10,0,0 2");
        s.jump_to(1).unwrap();
        s.jump_to(3).unwrap(); // redo through the boolean

        let log = s.save_log();
        assert_eq!(log.len(), 3);
        let replayed = Session::replay(log.clone()).unwrap();
        let a: Vec<_> = s.doc.objects().collect();
        let b: Vec<_> = replayed.doc.objects().collect();
        assert_eq!(a, b);
        assert_eq!(
            serde_json::to_string(&log).unwrap(),
            serde_json::to_string(&replayed.save_log()).unwrap()
        );
    }

    #[test]
    fn dim_and_text_create_undo() {
        let mut s = Session::default();
        let out = run(&mut s, "dim 0,0 10,0 0.8");
        assert!(out.message.contains("10.00 m"), "{}", out.message);
        run(&mut s, "text 5,3 living room 0.3");
        assert_eq!(s.doc.len(), 2);
        let anns: Vec<_> = s.doc.objects().collect();
        assert!(matches!(
            &anns[0].geometry,
            Geometry::Annotation(Annotation::LinearDim { offset, .. }) if *offset == 0.8
        ));
        assert!(matches!(
            &anns[1].geometry,
            Geometry::Annotation(Annotation::Text { text, height, .. })
                if text == "living room" && *height == 0.3
        ));
        run(&mut s, "undo");
        run(&mut s, "undo");
        assert_eq!(s.doc.len(), 0);
        run(&mut s, "redo");
        run(&mut s, "redo");
        assert_eq!(s.doc.len(), 2);

        // degenerate dim rejected
        let err = s.run(parse("dim 1,1 1,1").unwrap()).unwrap_err();
        assert!(err.to_string().contains("distinct"), "{err}");
    }

    #[test]
    fn hatch_requires_closed_curve() {
        let mut s = Session::default();
        run(&mut s, "line 0,0 5,0");
        let err = s.run(parse("hatch last").unwrap()).unwrap_err();
        assert!(err.to_string().contains("closed"), "{err}");

        run(&mut s, "box 0,0,0 1,1,1");
        let err = s.run(parse("hatch last").unwrap()).unwrap_err();
        assert!(err.to_string().contains("curve"), "{err}");
    }

    #[test]
    fn hatch_rect_boundary_and_undo() {
        let mut s = Session::default();
        run(&mut s, "rect 0,0,0 10 6");
        run(&mut s, "hatch last lines 45 0.5");
        assert_eq!(s.doc.len(), 2); // boundary curve kept
        let obj = s.doc.objects().last().unwrap();
        let Geometry::Annotation(Annotation::Hatch { boundary, pattern }) = &obj.geometry
        else {
            panic!("expected hatch")
        };
        assert_eq!(boundary.len(), 4);
        assert!(matches!(
            pattern,
            mydrafter_doc::HatchPattern::Lines { angle_deg, spacing }
                if *angle_deg == 45.0 && *spacing == 0.5
        ));
        run(&mut s, "undo");
        assert_eq!(s.doc.len(), 1);

        // zero spacing rejected
        let err = s.run(parse("hatch last lines 45 0").unwrap()).unwrap_err();
        assert!(err.to_string().contains("spacing"), "{err}");
    }

    #[test]
    fn annotations_move_and_delete() {
        let mut s = Session::default();
        run(&mut s, "dim 0,0 10,0 0.5");
        run(&mut s, "move last 0,5,0");
        let obj = s.doc.objects().next().unwrap();
        let Geometry::Annotation(Annotation::LinearDim { a, b, .. }) = &obj.geometry else {
            panic!("expected dim")
        };
        assert_eq!(*a, DVec3::new(0.0, 5.0, 0.0));
        assert_eq!(*b, DVec3::new(10.0, 5.0, 0.0));
        run(&mut s, "delete last");
        assert_eq!(s.doc.len(), 0);
        run(&mut s, "undo");
        assert_eq!(s.doc.len(), 1);
    }

    #[test]
    fn drafting_replay_stable() {
        let mut s = Session::default();
        run(&mut s, "rect 0,0,0 10 6");
        run(&mut s, "hatch last lines 45 0.25");
        run(&mut s, "dim 0,0 10,0 0.8");
        run(&mut s, "text 5,3 living room 0.3");
        run(&mut s, "circle 20,0,0 2");
        run(&mut s, "hatch last solid");
        run(&mut s, "move last 2 0,0,1");

        let log = s.save_log();
        let replayed = Session::replay(log.clone()).unwrap();
        let a: Vec<_> = s.doc.objects().collect();
        let b: Vec<_> = replayed.doc.objects().collect();
        assert_eq!(a, b);
        assert_eq!(
            serde_json::to_string(&log).unwrap(),
            serde_json::to_string(&replayed.save_log()).unwrap()
        );
    }

    #[test]
    fn sheet_create_view_undo_redo() {
        use mydrafter_doc::{PaperSize, ViewDirection};
        let mut s = Session::default();
        let out = run(&mut s, "sheet plan a1");
        assert!(out.message.contains("841x594mm"), "{}", out.message);
        run(&mut s, "sheetview plan top 1:100");
        run(&mut s, "sheetview plan front 50");
        let sheet = s.doc.sheet("plan").unwrap();
        assert_eq!(sheet.paper, PaperSize::A1);
        assert_eq!(sheet.views.len(), 2);
        assert_eq!(sheet.views[1].direction, ViewDirection::Front);
        assert_eq!(sheet.views[1].scale, 50.0);

        run(&mut s, "undo"); // pop front view
        assert_eq!(s.doc.sheet("plan").unwrap().views.len(), 1);
        run(&mut s, "undo"); // pop top view
        run(&mut s, "undo"); // remove sheet
        assert!(s.doc.sheets.is_empty());
        run(&mut s, "redo");
        run(&mut s, "redo");
        run(&mut s, "redo");
        assert_eq!(s.doc.sheet("plan").unwrap().views.len(), 2);

        // duplicate sheet and missing sheet both error with hints
        let err = s.run(parse("sheet plan").unwrap()).unwrap_err();
        assert!(err.to_string().contains("already exists"), "{err}");
        let err = s.run(parse("sheetview ghost top 100").unwrap()).unwrap_err();
        assert!(err.to_string().contains("no sheet 'ghost'"), "{err}");
    }

    #[test]
    fn sheet_replay_stable() {
        let mut s = Session::default();
        run(&mut s, "box 0,0,0 5,5,3");
        run(&mut s, "sheet plan a3");
        run(&mut s, "sheetview plan top 1:100");
        run(&mut s, "sheet detail a4");
        run(&mut s, "sheetview detail persp 1:50");
        run(&mut s, "undo"); // drop the persp view

        let log = s.save_log();
        let replayed = Session::replay(log.clone()).unwrap();
        assert_eq!(s.doc.sheets, replayed.doc.sheets);
        assert_eq!(
            serde_json::to_string(&log).unwrap(),
            serde_json::to_string(&replayed.save_log()).unwrap()
        );
    }

    #[test]
    fn print_writes_vector_pdf() {
        let dir = std::env::temp_dir().join("mydrafter-pdf-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("plan.pdf");
        let _ = std::fs::remove_file(&path);

        let mut s = Session::default();
        run(&mut s, "rect 0,0,0 10 6");
        run(&mut s, "extrude last 3");
        run(&mut s, "circle 5,3,0 1.5");
        run(&mut s, "sheet plan a3");
        run(&mut s, "sheetview plan top 1:100");
        run(&mut s, "sheetview plan persp 1:100");
        let out = run(&mut s, &format!("print plan {}", path.display()));
        assert!(out.message.contains("printed"), "{}", out.message);

        let bytes = std::fs::read(&path).unwrap();
        assert!(bytes.starts_with(b"%PDF"), "PDF header present");
        assert!(bytes.len() > 1024, "nonempty file, got {} bytes", bytes.len());

        // print is not logged: replaying a saved file must not rewrite PDFs
        assert!(s.save_log().iter().all(|c| !matches!(c, Command::Print { .. })));

        // printing before any views / a missing sheet errors cleanly
        run(&mut s, "sheet empty");
        let err = s
            .run(parse(&format!("print empty {}", path.display())).unwrap())
            .unwrap_err();
        assert!(err.to_string().contains("no views"), "{err}");
        let err = s.run(parse("print ghost /tmp/x.pdf").unwrap()).unwrap_err();
        assert!(err.to_string().contains("no sheet"), "{err}");
    }

    #[test]
    fn export_writes_dxf_and_is_not_logged() {
        let dir = std::env::temp_dir().join("mydrafter-dxf-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("model.dxf");
        let _ = std::fs::remove_file(&path);

        let mut s = Session::default();
        run(&mut s, "rect 0,0,0 10 6");
        run(&mut s, "circle 5,3,0 1.5");
        run(&mut s, "box 20,0,0 2,2,2");
        let out = run(&mut s, &format!("export {}", path.display()));
        assert!(out.message.contains("exported DXF"), "{}", out.message);
        assert!(out.message.contains("14 entities"), "{}", out.message); // 1+1+12

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("ENTITIES"));
        assert!(text.contains("EOF"));

        // export is not logged: no Export ops in the save file, nothing to undo
        assert!(s.save_log().iter().all(|c| !matches!(c, Command::Export { .. })));
        assert_eq!(s.history().0.len(), 3);

        // unwritable path errors cleanly, doc untouched
        let err = s
            .run(parse("export /nonexistent-dir/x.dxf").unwrap())
            .unwrap_err();
        assert!(err.to_string().contains("cannot write"), "{err}");
        assert_eq!(s.doc.len(), 3);
    }

    #[test]
    fn measure_distance_in_doc_units() {
        let mut s = Session::default();
        run(&mut s, "box 0,0,0 1,1,1"); // any content; distance ignores it
        let out = run(&mut s, "distance 0,0,0 3,4,0");
        assert!(out.message.contains("distance: 5.00 m"), "{}", out.message);
        assert!(out.message.contains("dx 3.00 m"), "{}", out.message);
        run(&mut s, "units mm");
        let out = run(&mut s, "distance 0,0,0 3,4,0");
        assert!(out.message.contains("5000 mm"), "{}", out.message);
    }

    #[test]
    fn measure_area_curves_and_meshes() {
        let mut s = Session::default();
        // closed 10x6 rect: shoelace = 60
        run(&mut s, "rect 0,0,0 10 6");
        let out = run(&mut s, "area last");
        assert!(out.message.contains("60.00 m²"), "{}", out.message);
        // circle r=2: tessellated shoelace ≈ pi*4
        run(&mut s, "circle 20,0,0 2");
        let out = run(&mut s, "area last");
        let area: f64 = out
            .message
            .split_whitespace()
            .filter_map(|w| w.parse().ok())
            .last()
            .unwrap_or_else(|| panic!("no number in '{}'", out.message));
        // inscribed-polygon tessellation underestimates slightly
        assert!((area - std::f64::consts::PI * 4.0).abs() < 0.1, "{area}");
        // mesh: 2x3x4 box surface = 2*(6+8+12) = 52; multi-target sums
        run(&mut s, "box 30,0,0 2,3,4");
        let out = run(&mut s, "area last");
        assert!(out.message.contains("52.00 m²"), "{}", out.message);
        let out = run(&mut s, "area last 3");
        let total: f64 = out
            .message
            .split_whitespace()
            .filter_map(|w| w.parse::<f64>().ok())
            .last()
            .unwrap();
        assert!((total - (60.0 + std::f64::consts::PI * 4.0 + 52.0)).abs() < 0.1);
        // open curves and annotations refuse with hints
        run(&mut s, "line 40,0 50,0");
        let err = s.run(parse("area last").unwrap()).unwrap_err();
        assert!(err.to_string().contains("open curve"), "{err}");
        run(&mut s, "text 0,0 hi");
        let err = s.run(parse("area last").unwrap()).unwrap_err();
        assert!(err.to_string().contains("annotation"), "{err}");
    }

    #[test]
    fn measure_volume_and_bbox() {
        let mut s = Session::default();
        run(&mut s, "box 0,0,0 5,5,3");
        let out = run(&mut s, "volume last");
        assert!(out.message.contains("75.00 m³"), "{}", out.message);
        // multi-target volumes sum
        run(&mut s, "box 10,0,0 2,2,2");
        let out = run(&mut s, "volume last 2");
        assert!(out.message.contains("83.00 m³"), "{}", out.message);
        // curves refuse with the extrude hint
        run(&mut s, "circle 20,0,0 1");
        let err = s.run(parse("volume last").unwrap()).unwrap_err();
        assert!(err.to_string().contains("extrude it first"), "{err}");

        let out = run(&mut s, "bbox all");
        // the circle spans y -1..1, so the combined min dips below zero
        assert!(out.message.contains("min 0.00,-1.00,0.00"), "{}", out.message);
        assert!(out.message.contains("max 21.00,5.00,3.00"), "{}", out.message);
        assert!(out.message.contains("size 21.00,6.00,3.00"), "{}", out.message);
        assert!(out.message.contains("(m)"), "{}", out.message);
        // bbox respects the doc unit
        run(&mut s, "units cm");
        let out = run(&mut s, "bbox last");
        assert!(out.message.contains("(cm)"), "{}", out.message);
        assert!(out.message.contains("max 2100.00,100.00,0.00"), "{}", out.message);
    }

    #[test]
    fn measure_queries_not_logged_and_leave_doc_untouched() {
        let mut s = Session::default();
        run(&mut s, "box 0,0,0 5,5,3");
        let before: Vec<SceneObject> = s.doc.objects().cloned().collect();
        run(&mut s, "distance 0,0 1,1");
        run(&mut s, "area last");
        run(&mut s, "volume last");
        run(&mut s, "bbox all");
        let after: Vec<SceneObject> = s.doc.objects().cloned().collect();
        assert_eq!(before, after);
        assert_eq!(s.save_log().len(), 1, "queries never enter the op-log");
        assert_eq!(s.history().0, ["box"]);
        // replaying the saved log still reproduces the document
        let replayed = Session::replay(s.save_log()).unwrap();
        let b: Vec<_> = replayed.doc.objects().cloned().collect();
        assert_eq!(after, b);
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

    /// Write a `w`x`h` PNG to a temp path and return it.
    fn temp_png(w: u32, h: u32, tag: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!("mydrafter_underlay_{tag}_{w}x{h}.png"));
        let img = image::RgbaImage::from_pixel(w, h, image::Rgba([200, 100, 50, 255]));
        img.save(&path).unwrap();
        path
    }

    #[test]
    fn underlay_places_with_image_aspect_and_fills_height() {
        let png = temp_png(200, 100, "aspect"); // aspect 2:1
        let mut s = Session::default();
        let out = run(&mut s, &format!("underlay {} 1,2 20", png.display()));
        let u = s.doc.underlay.as_ref().expect("underlay set");
        assert_eq!(u.corner, glam::DVec2::new(1.0, 2.0));
        assert_eq!(u.width, 20.0);
        assert_eq!(u.height, 10.0, "height = width / aspect");
        assert_eq!(u.opacity, 1.0);
        assert!(out.message.contains("20.00 x 10.00"), "{}", out.message);

        // The logged op carries the resolved height so replay needs no file.
        let log = s.save_log();
        assert!(matches!(
            &log[0],
            Command::Underlay { height: Some(h), width: Some(w), .. }
                if (*h - 10.0).abs() < 1e-9 && (*w - 20.0).abs() < 1e-9
        ));
    }

    #[test]
    fn underlay_missing_file_is_a_warning_not_an_error() {
        let mut s = Session::default();
        let out = run(&mut s, "underlay /no/such/file.png 0,0 8");
        let u = s.doc.underlay.as_ref().unwrap();
        assert_eq!(u.width, 8.0);
        assert_eq!(u.height, 8.0, "unreadable image assumed square");
        assert!(out.message.contains("unreadable"), "{}", out.message);
    }

    #[test]
    fn underlay_opacity_and_off_with_undo() {
        let png = temp_png(100, 100, "opac");
        let mut s = Session::default();
        run(&mut s, &format!("underlay {} 0,0 10", png.display()));
        run(&mut s, "underlayopacity 0.3");
        assert_eq!(s.doc.underlay.as_ref().unwrap().opacity, 0.3);

        run(&mut s, "underlayoff");
        assert!(s.doc.underlay.is_none());

        // undo off -> opacity 0.3 underlay is back
        run(&mut s, "undo");
        assert_eq!(s.doc.underlay.as_ref().unwrap().opacity, 0.3);
        // undo opacity -> back to 1.0
        run(&mut s, "undo");
        assert_eq!(s.doc.underlay.as_ref().unwrap().opacity, 1.0);
        // undo placement -> gone
        run(&mut s, "undo");
        assert!(s.doc.underlay.is_none());
        // redo placement
        run(&mut s, "redo");
        assert_eq!(s.doc.underlay.as_ref().unwrap().opacity, 1.0);
    }

    #[test]
    fn underlay_opacity_without_underlay_errors() {
        let mut s = Session::default();
        assert!(s.run(parse("underlayopacity 0.5").unwrap()).is_err());
        assert!(s.run(parse("underlayoff").unwrap()).is_err());
    }

    #[test]
    fn underlay_replaces_and_keeps_opacity() {
        let a = temp_png(200, 100, "a");
        let b = temp_png(100, 200, "b");
        let mut s = Session::default();
        run(&mut s, &format!("underlay {} 0,0 10", a.display()));
        run(&mut s, "underlayopacity 0.5");
        run(&mut s, &format!("underlay {} 0,0 10", b.display()));
        let u = s.doc.underlay.as_ref().unwrap();
        assert_eq!(u.height, 20.0, "new image aspect 1:2");
        assert_eq!(u.opacity, 0.5, "opacity carried across image swap");
    }

    #[test]
    fn underlay_replay_reproduces_placement_without_file() {
        let png = temp_png(300, 100, "replay");
        let mut s = Session::default();
        run(&mut s, &format!("underlay {} 2,3 30", png.display()));
        run(&mut s, "underlayopacity 0.4");
        let before = s.doc.underlay.clone();

        // Delete the file: replay must still reproduce the exact placement.
        std::fs::remove_file(&png).unwrap();
        let log = s.save_log();
        let replayed = Session::replay(log.clone()).unwrap();
        assert_eq!(replayed.doc.underlay, before);
        assert_eq!(
            serde_json::to_string(&log).unwrap(),
            serde_json::to_string(&replayed.save_log()).unwrap(),
            "replay-stable log"
        );
    }

    // ── schedule / sheettable ────────────────────────────────────────────────

    #[test]
    fn schedule_known_doc_expected_numbers() {
        let mut s = Session::default();
        // A 2×3×4 box: volume = 24 m³, surface area = 2*(2*3 + 2*4 + 3*4) = 52 m²
        run(&mut s, "box 0,0,0 2,3,4");
        run(&mut s, "name last cube");
        // A circle radius 1 on layer "arcs": closed XY area = π ≈ 3.14 m²
        run(&mut s, "layer arcs");
        run(&mut s, "circle 0,0,0 1");

        let out = run(&mut s, "schedule");
        // Table contains both objects.
        assert!(
            out.message.contains("cube"),
            "name column missing: {}",
            out.message
        );
        assert!(
            out.message.contains("mesh"),
            "type column missing: {}",
            out.message
        );
        assert!(
            out.message.contains("curve"),
            "curve type missing: {}",
            out.message
        );
        // Volume of the box should appear in the table (~24.00).
        assert!(
            out.message.contains("24.00"),
            "box volume 24 m³ missing: {}",
            out.message
        );

        // Layer filter: only the circle.
        let out2 = run(&mut s, "schedule arcs");
        assert!(out2.message.contains("curve"), "filtered: {}", out2.message);
        assert!(
            !out2.message.contains("cube"),
            "box leaked through layer filter: {}",
            out2.message
        );
        // Area of circle radius 1 ≈ π (tessellated; expect within 2% of π).
        // The schedule table shows it in m²; parse the value from the row.
        let area_val: f64 = {
            let rows = build_schedule_rows(&s.doc, Some("arcs"));
            assert_eq!(rows.len(), 1);
            rows[0].area_m2
        };
        assert!(
            (area_val - std::f64::consts::PI).abs() < 0.1,
            "circle area {area_val} not close to π"
        );
    }

    #[test]
    fn sheettable_places_rows_on_sheet_and_pdf_contains_text() {
        let mut s = Session::default();
        // Box 5×5×3 → volume 75 m³.
        run(&mut s, "box 0,0,0 5,5,3");
        run(&mut s, "name last building");
        run(&mut s, "sheet plan a3");
        run(&mut s, "sheettable plan");

        // Rows are stored on the sheet.
        let tbl = s.doc.sheet("plan").unwrap().table.as_ref().unwrap();
        assert_eq!(tbl.rows.len(), 1, "one object in doc");
        let row = &tbl.rows[0];
        assert_eq!(row.name, "building");
        assert_eq!(row.kind, "mesh");
        assert!((row.volume_m3 - 75.0).abs() < 1e-6, "volume {}", row.volume_m3);

        // PDF output contains row text.
        let sheet = s.doc.sheet("plan").unwrap().clone();
        let (bytes, _) = crate::pdf::sheet_pdf(&s.doc, &sheet);
        let content = String::from_utf8_lossy(&bytes);
        assert!(
            content.contains("building"),
            "PDF missing 'building': (truncated)"
        );
        assert!(
            content.contains("75.00"),
            "PDF missing volume 75.00: (truncated)"
        );
    }

    #[test]
    fn sheettable_undo_clears_table() {
        let mut s = Session::default();
        run(&mut s, "box 0,0,0 1,1,1");
        run(&mut s, "sheet s1 a3");
        run(&mut s, "sheettable s1");
        assert!(s.doc.sheet("s1").unwrap().table.is_some());
        run(&mut s, "undo");
        assert!(s.doc.sheet("s1").unwrap().table.is_none(), "undo must clear table");
    }

    #[test]
    fn sheettable_replay_stability() {
        let mut s = Session::default();
        run(&mut s, "box 0,0,0 3,4,5");
        run(&mut s, "name last block");
        run(&mut s, "sheet lay a3");
        run(&mut s, "sheettable lay");

        let log = s.save_log();
        let replayed = Session::replay(log.clone()).unwrap();
        let orig_tbl = s.doc.sheet("lay").unwrap().table.as_ref().unwrap();
        let rep_tbl = replayed.doc.sheet("lay").unwrap().table.as_ref().unwrap();
        assert_eq!(orig_tbl.rows.len(), rep_tbl.rows.len());
        assert_eq!(orig_tbl.rows[0].name, rep_tbl.rows[0].name);
        assert!((orig_tbl.rows[0].volume_m3 - rep_tbl.rows[0].volume_m3).abs() < 1e-9);
        assert_eq!(
            serde_json::to_string(&log).unwrap(),
            serde_json::to_string(&replayed.save_log()).unwrap(),
            "log must be replay-stable"
        );
    }

    #[test]
    fn sun_exec_sets_document_sun_and_is_logged() {
        let mut s = Session::default();
        assert!(s.doc.sun.is_none(), "default doc has no sun");

        // Set sun; should be logged and bumps generation.
        let g0 = s.doc.generation;
        run(&mut s, "sun 40.71 -74.01 2024-06-21 16:58");
        let sun = s.doc.sun.expect("sun set after command");
        assert!(s.doc.generation > g0);
        // The command uses the NOAA SPA: NY summer solstice noon → ~180° az, ~72.7° alt.
        assert!((sun.azimuth_deg - 180.0).abs() < 0.5, "az={:.2}", sun.azimuth_deg);
        assert!((sun.altitude_deg - 72.7).abs() < 0.5, "alt={:.2}", sun.altitude_deg);

        // Undo removes sun.
        s.run(crate::Command::Undo).unwrap();
        assert!(s.doc.sun.is_none(), "sun cleared after undo");

        // Redo restores sun.
        s.run(crate::Command::Redo).unwrap();
        assert!(s.doc.sun.is_some(), "sun restored after redo");
    }

    #[test]
    fn sunoff_exec_clears_sun_and_is_logged() {
        let mut s = Session::default();
        run(&mut s, "sun 40.71 -74.01 2024-06-21 16:58");
        run(&mut s, "sunoff");
        assert!(s.doc.sun.is_none(), "sunoff clears sun");
        // undo restores sun
        s.run(crate::Command::Undo).unwrap();
        assert!(s.doc.sun.is_some());
    }

    #[test]
    fn sun_command_replay_stability() {
        let mut s = Session::default();
        run(&mut s, "box 0,0,0 5,5,3");
        run(&mut s, "sun 40.71 -74.01 2024-06-21 16:58");
        let log = s.save_log();
        let replayed = Session::replay(log.clone()).unwrap();
        // Replayed document must have the same sun position.
        assert_eq!(s.doc.sun, replayed.doc.sun);
        // Log must be replay-stable (idempotent serialisation).
        assert_eq!(
            serde_json::to_string(&log).unwrap(),
            serde_json::to_string(&replayed.save_log()).unwrap(),
            "sun log must be replay-stable"
        );
    }

    // ---- block definition + instancing ----

    #[test]
    fn block_define_stores_geometry_and_insert_creates_instance() {
        let mut s = Session::default();
        // Create a simple box as source geometry.
        run(&mut s, "box 0,0,0 1,1,2");
        run(&mut s, "block last mytree");
        assert!(s.doc.blocks.contains_key("mytree"), "block definition stored");
        let defs = s.doc.blocks.get("mytree").unwrap();
        assert_eq!(defs.len(), 1, "one geometry in block");

        // Insert an instance.
        run(&mut s, "insert mytree 5,0,0");
        let last = s.doc.objects().last().unwrap();
        assert!(matches!(last.geometry, mydrafter_doc::Geometry::Instance { ref block, .. } if block == "mytree"));
    }

    #[test]
    fn block_insert_with_rotation_and_scale() {
        let mut s = Session::default();
        run(&mut s, "box 0,0,0 1,1,1");
        run(&mut s, "block last door");
        run(&mut s, "insert door 3,0,0 90 2");
        let obj = s.doc.objects().last().unwrap();
        match &obj.geometry {
            mydrafter_doc::Geometry::Instance { block, position, rotation_deg, scale } => {
                assert_eq!(block, "door");
                assert!((position.x - 3.0).abs() < 1e-9);
                assert!((rotation_deg - 90.0).abs() < 1e-9);
                assert!((scale - 2.0).abs() < 1e-9);
            }
            _ => panic!("expected Instance geometry"),
        }
    }

    #[test]
    fn block_define_undo_removes_definition() {
        let mut s = Session::default();
        run(&mut s, "box 0,0,0 1,1,1");
        run(&mut s, "block last myblock");
        assert!(s.doc.blocks.contains_key("myblock"));
        s.run(crate::Command::Undo).unwrap();
        assert!(!s.doc.blocks.contains_key("myblock"), "undo removes block def");
    }

    #[test]
    fn block_insert_undo_removes_instance() {
        let mut s = Session::default();
        run(&mut s, "box 0,0,0 1,1,1");
        run(&mut s, "block last widget");
        run(&mut s, "insert widget 0,5,0");
        let n = s.doc.len();
        s.run(crate::Command::Undo).unwrap();
        assert_eq!(s.doc.len(), n - 1, "undo removes instance");
    }

    #[test]
    fn block_insert_replay_stability() {
        let mut s = Session::default();
        run(&mut s, "box 0,0,0 1,2,3");
        run(&mut s, "block last column");
        run(&mut s, "insert column 0,0,0");
        run(&mut s, "insert column 5,0,0 45 1.5");
        let log = s.save_log();
        let replayed = Session::replay(log.clone()).unwrap();
        assert_eq!(s.doc.len(), replayed.doc.len());
        assert_eq!(
            serde_json::to_string(&log).unwrap(),
            serde_json::to_string(&replayed.save_log()).unwrap(),
            "block log must be replay-stable"
        );
    }

    #[test]
    fn blocks_list_is_not_logged() {
        let mut s = Session::default();
        run(&mut s, "box 0,0,0 1,1,1");
        run(&mut s, "block last b");
        let log_before = s.save_log().len();
        run(&mut s, "blocks");
        assert_eq!(s.save_log().len(), log_before, "blocks list must not be logged");
    }

    #[test]
    fn insert_unknown_block_errors() {
        let mut s = Session::default();
        let result = s.run(crate::parse::parse("insert nosuchblock 0,0,0").unwrap());
        assert!(result.is_err(), "should error on unknown block");
    }

    #[test]
    fn color_set_undo_redo_replay() {
        let mut s = Session::default();
        run(&mut s, "box 0,0,0 1,1,1");
        let id = *s.doc.all_ids().last().unwrap();

        // Set color
        run(&mut s, "color last 1,0,0");
        assert_eq!(s.doc.get(id).unwrap().color, Some([1.0, 0.0, 0.0]));

        // Undo restores None
        run(&mut s, "undo");
        assert_eq!(s.doc.get(id).unwrap().color, None);

        // Redo re-applies
        run(&mut s, "redo");
        assert_eq!(s.doc.get(id).unwrap().color, Some([1.0, 0.0, 0.0]));

        // coloroff clears it
        run(&mut s, "color last off");
        assert_eq!(s.doc.get(id).unwrap().color, None);

        // Replay produces the same result
        let log = s.save_log();
        let s2 = Session::replay(log).unwrap();
        assert_eq!(s2.doc.get(id).unwrap().color, None);
    }

    #[test]
    fn coloroff_undo_restores_previous_color() {
        let mut s = Session::default();
        run(&mut s, "box 0,0,0 1,1,1");
        let id = *s.doc.all_ids().last().unwrap();

        run(&mut s, "color last 0,1,0");
        assert_eq!(s.doc.get(id).unwrap().color, Some([0.0, 1.0, 0.0]));

        run(&mut s, "color last off");
        assert_eq!(s.doc.get(id).unwrap().color, None);

        // Undo coloroff restores the green
        run(&mut s, "undo");
        assert_eq!(s.doc.get(id).unwrap().color, Some([0.0, 1.0, 0.0]));
    }

    #[test]
    fn color_255_scale_accepted() {
        let mut s = Session::default();
        run(&mut s, "box 0,0,0 1,1,1");
        run(&mut s, "color last 255,128,0");
        let id = *s.doc.all_ids().last().unwrap();
        let [r, g, b] = s.doc.get(id).unwrap().color.unwrap();
        assert!((r - 1.0).abs() < 0.005, "r={r}");
        assert!((g - 128.0 / 255.0).abs() < 0.005, "g={g}");
        assert!(b.abs() < 0.005, "b={b}");
    }

    fn curve_of_last(s: &Session) -> Curve {
        let id = *s.doc.all_ids().last().unwrap();
        match &s.doc.get(id).unwrap().geometry {
            Geometry::Curve(c) => c.clone(),
            _ => panic!("not a curve"),
        }
    }

    #[test]
    fn interpcurve_passes_through_points() {
        let mut s = Session::default();
        run(&mut s, "interpcurve 0,0 2,4 6,4 8,0 10,2");
        let Curve::Nurbs { control, weights, knots, degree } = curve_of_last(&s) else {
            panic!("expected nurbs")
        };
        // Endpoints are interpolated exactly (clamped).
        let p0 = kernel_curve::nurbs_point(&control, &weights, &knots, degree, 0.0);
        let p1 = kernel_curve::nurbs_point(&control, &weights, &knots, degree, 1.0);
        assert!(p0.distance(DVec3::new(0.0, 0.0, 0.0)) < 1e-6);
        assert!(p1.distance(DVec3::new(10.0, 2.0, 0.0)) < 1e-6);
    }

    #[test]
    fn helix_radius_and_height_via_command() {
        let mut s = Session::default();
        run(&mut s, "helix 0,0,0 3 12 4");
        let Curve::Polyline { points, .. } = curve_of_last(&s) else { panic!() };
        for p in &points {
            assert!(((p.x * p.x + p.y * p.y).sqrt() - 3.0).abs() < 1e-9);
        }
        let zmax = points.iter().map(|p| p.z).fold(f64::MIN, f64::max);
        assert!((zmax - 12.0).abs() < 1e-9);
    }

    #[test]
    fn setpoint_moves_control_and_undo_replay() {
        let mut s = Session::default();
        run(&mut s, "curve 0,0 2,4 6,4 8,0");
        let id = *s.doc.all_ids().last().unwrap();
        let before = curve_of_last(&s);
        run(&mut s, "setpoint last 1 3,9,0");
        let Curve::Nurbs { control, .. } = curve_of_last(&s) else { panic!() };
        assert!(control[1].distance(DVec3::new(3.0, 9.0, 0.0)) < 1e-12);
        // Undo restores the original geometry exactly.
        s.run(Command::Undo).unwrap();
        assert_eq!(
            match &s.doc.get(id).unwrap().geometry {
                Geometry::Curve(c) => c.clone(),
                _ => panic!(),
            },
            before
        );
        // Redo re-applies.
        s.run(Command::Redo).unwrap();
        let Curve::Nurbs { control, .. } = curve_of_last(&s) else { panic!() };
        assert!(control[1].distance(DVec3::new(3.0, 9.0, 0.0)) < 1e-12);

        // Replay from the op-log reproduces the edited curve.
        let json = crate::io::to_json(&s);
        let loaded = crate::io::from_json(&json).unwrap();
        assert_eq!(crate::io::to_json(&loaded), json, "replay-stable");
    }

    #[test]
    fn setpoint_on_polyline() {
        let mut s = Session::default();
        run(&mut s, "polyline 0,0 5,0 5,5 closed");
        run(&mut s, "setpoint last 2 9,9,0");
        let Curve::Polyline { points, .. } = curve_of_last(&s) else { panic!() };
        assert!(points[2].distance(DVec3::new(9.0, 9.0, 0.0)) < 1e-12);
    }

    #[test]
    fn setpoint_index_out_of_range_errors() {
        let mut s = Session::default();
        run(&mut s, "polyline 0,0 5,0 5,5");
        let err = s.run(parse("setpoint last 9 1,1,0").unwrap()).unwrap_err();
        assert!(err.to_string().contains("out of range"), "{err}");
    }

    #[test]
    fn rebuild_resamples_to_count_and_undo() {
        let mut s = Session::default();
        run(&mut s, "polyline 0,0 10,0");
        let out = run(&mut s, "rebuild last 6");
        assert!(out.message.contains("6 points"), "{}", out.message);
        let Curve::Polyline { points, .. } = curve_of_last(&s) else { panic!() };
        assert_eq!(points.len(), 6);
        // Undo brings back the original 2-point polyline.
        s.run(Command::Undo).unwrap();
        let Curve::Polyline { points, .. } = curve_of_last(&s) else { panic!() };
        assert_eq!(points.len(), 2);
    }

    #[test]
    fn rebuild_closed_curve_stays_closed() {
        let mut s = Session::default();
        run(&mut s, "circle 0,0,0 5");
        run(&mut s, "rebuild last 24");
        let c = curve_of_last(&s);
        assert!(c.is_closed());
        let Curve::Polyline { points, .. } = c else { panic!() };
        assert_eq!(points.len(), 24);
    }
}
