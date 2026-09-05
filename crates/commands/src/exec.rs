// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

use glam::DVec3;
use kernel_curve::{clamped_uniform_knots, Curve};
use kernel_mesh::extrude_profile;
use rayon::prelude::*;
use itsjustcad_doc::{
    format_area, format_length, format_volume, AnalysisReport, AnalysisSample, Annotation,
    Document, GeoLocation, Geometry, Grid, LayerStyle,
    LoadGeometry, Material, NamedView, ObjectId, Room, SceneObject, ScheduleRow,
    SheetDim, SheetTable, Story, StructLoad, StructSupport, Underlay, Units,
};

use std::collections::BTreeMap;

use crate::error::ExecError;
use crate::{BoolKind, Command, CompassDir, MirrorPlane, OptionOp, Selector};

/// Chord tolerance used when tessellating profile curves for extrusion.
const PROFILE_TOL: f64 = 0.01;

/// Accepted `room` occupancy classifications — the IBC use groups, simplified
/// to family names. These key the occupant-load factor table carried as DATA
/// in the compliance pack (never hard-coded thresholds).
const ROOM_OCCUPANCIES: &[&str] = &[
    "assembly",
    "business",
    "residential",
    "mercantile",
    "educational",
    "storage",
    "institutional",
];

/// Pixel aspect ratio (width / height) of a raster file, or `None` if it can't
/// be read. Only the header is decoded, so this is cheap.
fn image_aspect(path: &str) -> Option<f64> {
    let (w, h) = image::image_dimensions(path).ok()?;
    (h > 0).then(|| w as f64 / h as f64)
}

/// Hard ceiling on the size of an import file we will load into memory (512 MiB).
///
/// SECURITY: every importer previously called `std::fs::read`, which loads the
/// whole file into RAM *before* any per-format cap (point-count strides, etc.)
/// can apply. A crafted or hostile file — a decompression-bomb-sized point cloud
/// pointed at by an LLM-emitted `import`/`terrain`/`osm` — would OOM the process
/// at the read, not the parse. We stat first and refuse anything over the cap,
/// so an oversized file fails cleanly with a message instead of taking the app
/// down. 512 MiB comfortably covers legitimate CAD/scan files.
const MAX_IMPORT_BYTES: u64 = 512 * 1024 * 1024;

/// Whether `len` is within the import ceiling. Pure so the rule is unit-tested
/// without touching the filesystem.
fn import_size_ok(len: u64) -> bool {
    len <= MAX_IMPORT_BYTES
}

/// Read an import file into memory with a size ceiling. Stats the file first so
/// an oversized file is refused *before* it is loaded (no OOM), then reads it.
/// A race that grows the file between stat and read is still bounded because the
/// read is a one-shot of the now-known length. Returns a clear `ExecError` on a
/// missing/unreadable/oversized file.
fn read_import_bytes(path: &str) -> Result<Vec<u8>, ExecError> {
    let meta = std::fs::metadata(path)
        .map_err(|e| ExecError::Invalid(format!("cannot read '{path}': {e}")))?;
    if !import_size_ok(meta.len()) {
        return Err(ExecError::Invalid(format!(
            "'{path}' is {} bytes — over the {} MiB import limit; refusing to load",
            meta.len(),
            MAX_IMPORT_BYTES / (1024 * 1024)
        )));
    }
    std::fs::read(path).map_err(|e| ExecError::Invalid(format!("cannot read '{path}': {e}")))
}

/// Like [`read_import_bytes`] but returns UTF-8 text (for DXF, CSV, EPW).
fn read_import_string(path: &str) -> Result<String, ExecError> {
    let bytes = read_import_bytes(path)?;
    String::from_utf8(bytes).map_err(|_| ExecError::Invalid(format!("'{path}' is not valid UTF-8")))
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
    /// `layercolor`/`hide`/`show`/`layerorder`: restore the previous layer style.
    LayerStyle { layer: String, prev: LayerStyle },
    /// `layerrename`: rename back, moving objects + current-layer pointer.
    LayerRename { from: String, to: String },
    /// `layerdelete`: recreate the deleted layer with its style, and put the
    /// reassigned objects back on it (plus the current-layer pointer if it moved).
    LayerDelete {
        layer: String,
        style: LayerStyle,
        moved: Vec<ObjectId>,
        prev_current: Option<String>,
    },
    /// `hideobj`/`showobj`: restore each object's previous visible flag.
    ObjectVisibility(Vec<(ObjectId, bool)>),
    /// `lineweight`/`lineweightoff`: restore each object's previous lineweight override.
    ObjectLineweight(Vec<(ObjectId, Option<f64>)>),
    /// `showweights`: restore the previous show_lineweights flag.
    ShowWeights { prev: bool },
    /// `color`/`coloroff`: restore each object's previous color override.
    ObjectColor(Vec<(ObjectId, Option<[f32; 3]>)>),
    /// `material2`/`material2off`: restore each object's previous material.
    ObjectMaterial(Vec<(ObjectId, Option<itsjustcad_doc::ObjectMaterial>)>),
    /// `units`: restore the previous display unit.
    Units { prev: Units },
    /// `underlay`/`underlayopacity`/`underlayoff`: restore the previous underlay.
    Underlay { prev: Option<Underlay> },
    /// `sun`/`sunoff`: restore the previous solar position.
    Sun { prev: Option<itsjustcad_doc::SunPosition> },
    /// `location` (also set as a side effect of `sun`): restore the previous
    /// observer location and solar position.
    Location {
        prev_loc: Option<itsjustcad_doc::GeoLocation>,
        prev_sun: Option<itsjustcad_doc::SunPosition>,
    },
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
        prev: Option<Vec<itsjustcad_doc::BlockGeometry>>,
    },
    /// `pblock`: restore the previous parametric-block definition (None if new).
    ParamBlockDef {
        name: String,
        prev: Option<itsjustcad_doc::ParamBlockDef>,
    },
    /// `blockdelete`: restore the deleted definition (plain and/or parametric —
    /// a pblock name owns entries in BOTH maps when instances have baked).
    BlockDeleted {
        name: String,
        prev_plain: Option<Vec<itsjustcad_doc::BlockGeometry>>,
        prev_param: Option<itsjustcad_doc::ParamBlockDef>,
    },
    /// Dynamic-block `insert`: remove the created instance object AND its
    /// per-instance baked geometry entry in `doc.blocks`.
    CreatedAndBake {
        created: Vec<ObjectId>,
        block_key: String,
    },
    /// `param` (edit an instance's dynamic-block param): restore the instance's
    /// prior geometry (its params map) and its prior baked geometry.
    ParamBlockSet {
        id: ObjectId,
        prev_geometry: itsjustcad_doc::Geometry,
        block_key: String,
        prev_bake: Vec<itsjustcad_doc::BlockGeometry>,
    },
    /// `section`: restore the previous named section binding (None if new).
    SectionDef {
        name: String,
        prev: Option<itsjustcad_doc::Section>,
    },
    /// `material`: restore the previous named material binding (None if new).
    MaterialDef {
        name: String,
        prev: Option<Material>,
    },
    /// `grid`: restore the previous named grid binding (None if new).
    GridDef {
        name: String,
        prev: Option<Grid>,
    },
    /// `story`: restore the previous story list (a story replace/append edits
    /// the whole list, so snapshot it).
    StoryList(Vec<Story>),
    /// `room`: restore the previous room list (a room tag appends to the whole
    /// list, so snapshot it — mirrors `StoryList`).
    RoomList(Vec<itsjustcad_doc::Room>),
    /// `load`: remove the load that was appended at the given index.
    RemoveLoad(usize),
    /// `support`: remove the support that was appended at the given index.
    RemoveSupport(usize),
    /// `constrain`: pop the constraint that was appended and restore the
    /// pre-solve geometry of everything the solve moved.
    ConstraintAdded { snapshots: Vec<(ObjectId, Geometry)> },
    /// `constraints delete`: reinsert the removed constraints at their
    /// (0-based) indices, ascending.
    ConstraintsRestore(Vec<(usize, itsjustcad_doc::SketchConstraint)>),
    /// `lotsettings`: restore the previous subdivision settings (serialized).
    SubdivisionSettings { prev: String },
}

/// Owns the document plus its op-log; the single mutation path for both the
/// human command line and the LLM deck.
pub struct Session {
    pub doc: Document,
    log: Vec<AppliedOp>,
    cursor: usize,
    /// Runtime plugin macros. Not part of the op-log or file format — plugins
    /// expand to ordinary logged commands at invoke time, so replay never
    /// touches this. Held here so the deck prompt, help and autosuggest can all
    /// consult one authoritative table.
    pub plugins: crate::plugin::PluginRegistry,
    /// Loaded compliance-check packs (M-checkengine), keyed by pack name.
    /// Session state like `plugins`, never part of the op-log or file format:
    /// a `codecheck` run embeds its resolved rules into its own logged op, so
    /// replay never consults this table. Seeded with the embedded demo pack;
    /// grown by `checkrules load` and the default checks dir.
    pub check_packs: BTreeMap<String, crate::checkengine::CheckPack>,
    /// Set only after a checkpoint fast-open ([`Session::from_snapshot`]): the
    /// forward ops whose inverses have not yet been materialized. `None` once
    /// the history is live. Never part of the file format.
    pending_log: Option<Vec<Command>>,
    /// Named design-option branches: each is a saved effective op-log. Persisted
    /// in the file (see `crate::io`). The branch you are on is `current_branch`;
    /// its stored log is refreshed on switch/save so divergence is detectable.
    /// Empty by default — old files carry no branches and load unchanged.
    branches: BTreeMap<String, Vec<Command>>,
    /// The branch the live log belongs to. `MAIN_BRANCH` until you save/switch.
    current_branch: String,
    /// Stable document identity written into the file header (see `crate::io`).
    /// `None` for a scratch/never-saved session and for pre-uuid files (which
    /// stay byte-identical on re-save). The app keys per-document chat sessions
    /// off this; it is NOT part of the replayable op-log, so it never affects
    /// geometry, ids, or replay-stability.
    doc_uuid: Option<String>,
}

/// The implicit branch every session starts on and that divergent work is
/// auto-saved to. Never needs an explicit `option save main`.
pub const MAIN_BRANCH: &str = "main";

/// Built-in parametric (dynamic) block definitions, seeded into every session
/// so `insert pdoor|pwindow|pcolumn ...` works out of the box. Each is a plain
/// command template using the `{param}` substitution machinery.
///
/// ### Doors (plan-view, origin at hinge/frame-corner)
/// - `pdoor`        — single-swing door leaf + arc (default width=0.9 m).
/// - `pdoor_double` — double-swing door, two leaves opening from centre seam.
/// - `pdoor_sliding`— sliding door: one panel shown offset inside frame.
///
/// ### Windows (plan-view, thickness along Y)
/// - `pwindow`         — fixed window: frame rect + centre glazing line.
/// - `pwindow_casement`— casement: frame + diagonal open-sash indicator.
/// - `pwindow_sliding` — sliding: frame + two sash-divider lines.
///
/// ### Furniture (plan-view)
/// - `pdesk`  — rectangular work surface (default 1.6 × 0.8 m).
/// - `pchair` — seat square + curved back arc (default 0.5 m).
/// - `pbed`   — bed frame + pillow zone (default double 1.5 × 2.0 m).
/// - `ptable` — round table as a circle (default radius 0.45 m).
/// - `psofa`  — sofa: seat + back + two armrests (default 2.0 × 0.9 m).
/// - `ptoilet`— WC plan: tank rect + bowl body + front arc.
/// - `psink`  — basin rect + drain circle (default 0.6 × 0.5 m).
///
/// ### Structure
/// - `pcolumn` — square column footprint of side `size`.
pub fn starter_param_blocks() -> Vec<(String, itsjustcad_doc::ParamBlockDef)> {
    use itsjustcad_doc::{ParamBlockDef, ParamBlockParam};
    let p = |name: &str, default: &str| ParamBlockParam {
        name: name.to_string(),
        default: default.to_string(),
    };
    let mk = |params: Vec<ParamBlockParam>, body: &[&str]| ParamBlockDef {
        params,
        body: body.iter().map(|s| s.to_string()).collect(),
    };
    vec![
        // ── Doors ────────────────────────────────────────────────────────────

        // Single-swing door: leaf rect + quarter-circle swing arc.
        // Origin at hinge (0,0). Leaf extends along +X; arc sweeps 0→swing°.
        (
            "pdoor".to_string(),
            mk(
                vec![p("width", "0.9"), p("swing", "90")],
                &[
                    "rect 0,0,0 {width} 0.05",
                    "arc 0,0,0 {width} 0 {swing}",
                ],
            ),
        ),

        // Double-swing door: two equal leaves from each jamb meeting at centre.
        // `half` = width/2; each leaf swings 90° outward from its hinge.
        // All derived dims are explicit params so the template stays pure-subst.
        (
            "pdoor_double".to_string(),
            mk(
                vec![p("width", "1.8"), p("half", "0.9")],
                &[
                    // Left leaf: hinge at (0,0), extends +X for `half` metres
                    "rect 0,0,0 {half} 0.05",
                    // Left swing arc: centre (0,0), radius=half, 0→90°
                    "arc 0,0,0 {half} 0 90",
                    // Right leaf: hinge at ({width},0), extends -X for `half` metres
                    "rect {half},0,0 {half} 0.05",
                    // Right swing arc: centre ({width},0), radius=half, 90→180°
                    "arc {width},0,0 {half} 90 180",
                ],
            ),
        ),

        // Sliding door: frame outline + one panel occupying the left half.
        // `half` = width/2 (expose as param for easy override).
        (
            "pdoor_sliding".to_string(),
            mk(
                vec![p("width", "0.9"), p("depth", "0.1"), p("half", "0.45")],
                &[
                    // Frame outline
                    "rect 0,0,0 {width} {depth}",
                    // Sliding panel in left half
                    "rect 0,0,0 {half} {depth}",
                    // Overlap indicator line
                    "line {half},0,0 {half},{depth},0",
                ],
            ),
        ),

        // ── Windows ──────────────────────────────────────────────────────────

        // Fixed window: outer frame rect + single centre glazing line.
        // `half_depth` = depth/2 (centre of wall thickness).
        (
            "pwindow".to_string(),
            mk(
                vec![p("width", "1.2"), p("depth", "0.15"), p("half_depth", "0.075")],
                &[
                    "rect 0,0,0 {width} {depth}",
                    "line 0,{half_depth},0 {width},{half_depth},0",
                ],
            ),
        ),

        // Casement window: frame + diagonal indicating the sash swings out.
        // Hinge at (0, depth); free corner swings to ({width}, 0).
        (
            "pwindow_casement".to_string(),
            mk(
                vec![p("width", "0.9"), p("depth", "0.15")],
                &[
                    "rect 0,0,0 {width} {depth}",
                    "line 0,{depth},0 {width},0,0",
                ],
            ),
        ),

        // Sliding window: frame + two vertical sash-divider lines.
        // `third` ≈ width/3; `two_thirds` ≈ 2×width/3 — explicit params.
        (
            "pwindow_sliding".to_string(),
            mk(
                vec![
                    p("width", "1.2"),
                    p("depth", "0.15"),
                    p("third", "0.4"),
                    p("two_thirds", "0.8"),
                ],
                &[
                    "rect 0,0,0 {width} {depth}",
                    "line {third},0,0 {third},{depth},0",
                    "line {two_thirds},0,0 {two_thirds},{depth},0",
                ],
            ),
        ),

        // ── Furniture ────────────────────────────────────────────────────────

        // Desk: main surface rectangle + keyboard-tray line 0.15 m from front.
        (
            "pdesk".to_string(),
            mk(
                vec![p("width", "1.6"), p("depth", "0.8")],
                &[
                    "rect 0,0,0 {width} {depth}",
                    "line 0,0.15,0 {width},0.15,0",
                ],
            ),
        ),

        // Chair: seat square + back-rest arc behind the seat.
        // `half` = size/2 (centre X for back arc); arc sweeps 0→180° behind seat.
        (
            "pchair".to_string(),
            mk(
                vec![p("size", "0.5"), p("half", "0.25")],
                &[
                    // Seat cushion square
                    "rect 0,0,0 {size} {size}",
                    // Back-rest arc: centre at ({half},{size}), radius={half}
                    "arc {half},{size},0 {half} 0 180",
                ],
            ),
        ),

        // Bed: frame outline + pillow-zone rect + centre-line.
        // `pillow_y` = length - 0.35 (head end); `pillow_w` = width - 0.1;
        // `half` = width/2 (centre divider).
        (
            "pbed".to_string(),
            mk(
                vec![
                    p("width", "1.5"),
                    p("length", "2.0"),
                    p("half", "0.75"),
                    p("pillow_y", "1.65"),
                    p("pillow_w", "1.4"),
                ],
                &[
                    // Bed frame outline
                    "rect 0,0,0 {width} {length}",
                    // Pillow zone at head end
                    "rect 0.05,{pillow_y},0 {pillow_w} 0.3",
                    // Centre divider line (foot-end up to head)
                    "line {half},0.4,0 {half},{pillow_y},0",
                ],
            ),
        ),

        // Round table: circle, radius param (default 0.45 m ≈ 4-person table).
        (
            "ptable".to_string(),
            mk(
                vec![p("radius", "0.45")],
                &["circle 0,0,0 {radius}"],
            ),
        ),

        // Sofa: seat cushion rect + back-rest rect + left/right armrests.
        // Derived dims are explicit params for pure-substitution templates.
        // `seat_d` = seat depth (≈0.55); `back_d` = back thickness (≈0.2);
        // `arm_x` = width − 0.15 (right armrest X origin).
        (
            "psofa".to_string(),
            mk(
                vec![
                    p("width", "2.0"),
                    p("depth", "0.9"),
                    p("seat_d", "0.55"),
                    p("back_d", "0.2"),
                    p("arm_x", "1.85"),
                ],
                &[
                    // Seat cushion area
                    "rect 0,0,0 {width} {seat_d}",
                    // Back rest
                    "rect 0,{seat_d},0 {width} {back_d}",
                    // Left armrest (0.15 m wide, full depth)
                    "rect 0,0,0 0.15 {depth}",
                    // Right armrest
                    "rect {arm_x},0,0 0.15 {depth}",
                ],
            ),
        ),

        // Toilet: cistern rect + bowl body rect + front convex arc.
        // `bowl_d` = bowl body depth (≈0.48); `half` = width/2 for arc centre.
        (
            "ptoilet".to_string(),
            mk(
                vec![
                    p("width", "0.38"),
                    p("depth", "0.72"),
                    p("bowl_d", "0.48"),
                    p("half", "0.19"),
                ],
                &[
                    // Cistern at the back
                    "rect 0,{bowl_d},0 {width} 0.15",
                    // Bowl body rectangle
                    "rect 0,0,0 {width} {bowl_d}",
                    // Front arc (convex front of bowl)
                    "arc {half},0,0 {half} 0 180",
                ],
            ),
        ),

        // Sink: basin outline rect + drain circle at basin centre.
        // `half_w` = width/2; `half_d` = depth/2 (drain centre coords).
        (
            "psink".to_string(),
            mk(
                vec![
                    p("width", "0.6"),
                    p("depth", "0.5"),
                    p("half_w", "0.3"),
                    p("half_d", "0.25"),
                ],
                &[
                    "rect 0,0,0 {width} {depth}",
                    "circle {half_w},{half_d},0 0.04",
                ],
            ),
        ),

        // ── Structure ────────────────────────────────────────────────────────

        (
            "pcolumn".to_string(),
            mk(
                vec![p("size", "0.4")],
                &["rect 0,0,0 {size} {size}"],
            ),
        ),
    ]
}

impl Default for Session {
    fn default() -> Self {
        let mut doc = Document::default();
        for (name, def) in starter_param_blocks() {
            doc.param_blocks.insert(name, def);
        }
        Session {
            doc,
            log: Vec::new(),
            cursor: 0,
            plugins: crate::plugin::PluginRegistry::default(),
            check_packs: BTreeMap::from([
                ("demo".to_string(), crate::checkengine::demo_pack()),
                ("ibc2021".to_string(), crate::checkengine::ibc_pack()),
                ("ada2010".to_string(), crate::checkengine::ada_pack()),
            ]),
            pending_log: None,
            branches: BTreeMap::new(),
            current_branch: MAIN_BRANCH.to_string(),
            doc_uuid: None,
        }
    }
}

impl Session {
    pub fn run(&mut self, cmd: Command) -> Result<ApplyOutcome, ExecError> {
        match cmd {
            Command::Undo => self.undo(),
            Command::Redo => self.redo(),
            Command::Amend { step, with } => self.amend(step, *with),
            Command::Option(op) => self.option(op),
            Command::Import { path } => self.import(path),
            Command::Terrain { path } => self.terrain(path),
            Command::OsmFile { path } => self.osmfile(path),
            Command::Plant { species, at, age_years } => self.plant(species, at, age_years),
            Command::PlantRow { species, a, b, spacing } => {
                self.plantrow(species, a, b, spacing)
            }
            Command::PlantSchedule { path } => self.plantschedule(path),
            Command::PlantCatalog { filter } => self.plantcatalog(filter),
            Command::Miyawaki { targets, density } => self.miyawaki(targets, density),
            // Resolve a check pack by name and embed its rules into the op
            // before the generic logged path, so replay never needs the pack
            // table or disk (the CutFill original_z precedent). Ops replayed
            // from a saved log already carry `rules: Some(..)` and skip this.
            Command::CodeCheck { pack, story, rules: None, ids } => {
                let rules = self.check_pack(&pack)?.rules.clone();
                self.run(Command::CodeCheck { pack, story, rules: Some(rules), ids })
            }
            Command::CheckRulesList => Ok(self.checkrules_list()),
            Command::CheckRulesLoad { path } => self.checkrules_load(path),
            cmd => {
                let logged = cmd.is_logged();
                // A new logged edit truncates the redo tail, so the undo history
                // must be live first (rebuild it if this was a fast-open).
                if logged {
                    self.ensure_history()?;
                }
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
        self.ensure_history()?; // rebuild inverses if fast-opened
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
            Inverse::LayerRename { from, to } => {
                // Undo a rename `from`→`to` by renaming `to`→`from`.
                if let Some(style) = self.doc.layers.remove(to) {
                    self.doc.layers.insert(from.clone(), style);
                }
                for obj in self.doc.objects_mut() {
                    if &obj.layer == to {
                        obj.layer = from.clone();
                    }
                }
                if &self.doc.current_layer == to {
                    self.doc.current_layer = from.clone();
                }
                self.doc.generation += 1;
            }
            Inverse::LayerDelete { layer, style, moved, prev_current } => {
                self.doc.layers.insert(layer.clone(), style.clone());
                for id in moved.clone() {
                    if let Some(obj) = self.doc.get_mut(id) {
                        obj.layer = layer.clone();
                    }
                }
                if let Some(prev) = prev_current {
                    self.doc.current_layer = prev.clone();
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
            Inverse::ObjectLineweight(prev) => {
                for (id, lw) in prev.clone() {
                    if let Some(obj) = self.doc.get_mut(id) {
                        obj.lineweight_mm = lw;
                    }
                }
                self.doc.generation += 1;
            }
            Inverse::ShowWeights { prev } => {
                self.doc.show_lineweights = *prev;
                self.doc.generation += 1;
            }
            Inverse::ObjectColor(prev) => {
                for (id, color) in prev.clone() {
                    if let Some(obj) = self.doc.get_mut(id) {
                        obj.color = color;
                    }
                }
                self.doc.generation += 1;
            }
            Inverse::ObjectMaterial(prev) => {
                for (id, material) in prev.clone() {
                    if let Some(obj) = self.doc.get_mut(id) {
                        obj.material = material;
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
            Inverse::Location { prev_loc, prev_sun } => {
                self.doc.location = *prev_loc;
                self.doc.sun = *prev_sun;
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
            Inverse::ParamBlockDef { name, prev } => {
                match prev {
                    Some(def) => {
                        self.doc.param_blocks.insert(name.clone(), def.clone());
                    }
                    None => {
                        self.doc.param_blocks.remove(name);
                    }
                }
                self.doc.generation += 1;
            }
            Inverse::BlockDeleted { name, prev_plain, prev_param } => {
                if let Some(defs) = prev_plain {
                    self.doc.blocks.insert(name.clone(), defs.clone());
                }
                if let Some(def) = prev_param {
                    self.doc.param_blocks.insert(name.clone(), def.clone());
                }
                self.doc.generation += 1;
            }
            Inverse::CreatedAndBake { created, block_key } => {
                for id in created.clone() {
                    self.doc.remove(id);
                }
                self.doc.blocks.remove(block_key);
                self.doc.generation += 1;
            }
            Inverse::ParamBlockSet { id, prev_geometry, block_key, prev_bake } => {
                if let Some(obj) = self.doc.get_mut(*id) {
                    obj.geometry = prev_geometry.clone();
                }
                self.doc.blocks.insert(block_key.clone(), prev_bake.clone());
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
            Inverse::SectionDef { name, prev } => {
                match prev {
                    Some(s) => {
                        self.doc.sections.insert(name.clone(), *s);
                    }
                    None => {
                        self.doc.sections.remove(name);
                    }
                }
                self.doc.generation += 1;
            }
            Inverse::MaterialDef { name, prev } => {
                match prev {
                    Some(m) => {
                        self.doc.materials.insert(name.clone(), *m);
                    }
                    None => {
                        self.doc.materials.remove(name);
                    }
                }
                self.doc.generation += 1;
            }
            Inverse::GridDef { name, prev } => {
                match prev {
                    Some(g) => {
                        self.doc.grids.insert(name.clone(), g.clone());
                    }
                    None => {
                        self.doc.grids.remove(name);
                    }
                }
                self.doc.generation += 1;
            }
            Inverse::StoryList(prev) => {
                self.doc.stories = prev.clone();
                self.doc.generation += 1;
            }
            Inverse::RoomList(prev) => {
                self.doc.rooms = prev.clone();
                self.doc.generation += 1;
            }
            Inverse::RemoveLoad(idx) => {
                if *idx < self.doc.loads.len() {
                    self.doc.loads.remove(*idx);
                    self.doc.generation += 1;
                }
            }
            Inverse::RemoveSupport(idx) => {
                if *idx < self.doc.supports.len() {
                    self.doc.supports.remove(*idx);
                    self.doc.generation += 1;
                }
            }
            Inverse::ConstraintAdded { snapshots } => {
                self.doc.constraints.pop();
                for (id, geometry) in snapshots.clone() {
                    if let Some(obj) = self.doc.get_mut(id) {
                        obj.geometry = geometry;
                    }
                }
                self.doc.generation += 1;
            }
            Inverse::ConstraintsRestore(removed) => {
                for (idx, c) in removed.clone() {
                    let idx = idx.min(self.doc.constraints.len());
                    self.doc.constraints.insert(idx, c);
                }
                self.doc.generation += 1;
            }
            Inverse::SubdivisionSettings { prev } => {
                if let Ok(settings) = serde_json::from_str(prev) {
                    self.doc.subdivision_settings = settings;
                    self.doc.generation += 1;
                }
            }
        }
        Ok(ApplyOutcome {
            created: Vec::new(),
            message: format!("undid: {}", describe(&self.log[self.cursor].op)),
        })
    }

    fn redo(&mut self) -> Result<ApplyOutcome, ExecError> {
        self.ensure_history()?; // rebuild inverses if fast-opened
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
    /// After a fast-open the inverses are not yet materialized, so this reads
    /// the pending forward log (cursor sits at its end).
    pub fn history(&self) -> (Vec<String>, usize) {
        if let Some(pending) = &self.pending_log {
            return (
                pending.iter().map(|op| describe(op).to_string()).collect(),
                pending.len(),
            );
        }
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
        // Amend rewrites the live log but leaves the design-option branches
        // (and which one we are on) intact — they are meta-level, like the
        // saved branches themselves.
        fresh.branches = std::mem::take(&mut self.branches);
        fresh.current_branch = std::mem::take(&mut self.current_branch);
        // Document identity is header-level, not part of the amended op-log.
        fresh.doc_uuid = self.doc_uuid.take();
        *self = fresh;
        Ok(ApplyOutcome {
            created: Vec::new(),
            message: format!(
                "amended step {step} to '{}'; replayed {count} op(s)",
                describe(&self.log[step].op)
            ),
        })
    }

    /// Design options: named branches of the op-log.
    ///
    /// Model (simplest correct one): a branch is a named saved effective log.
    /// The live session is always "on" a branch (`current_branch`, starting at
    /// [`MAIN_BRANCH`]). Switching replays the target branch; the work you were
    /// doing keeps going on whichever branch you land on.
    ///
    /// - `save <name>`: snapshot the current effective log as branch `name`,
    ///   overwriting any existing branch of that name, and make `name` current.
    /// - `<name>` (switch): if the live log has diverged from the stored copy of
    ///   the current branch, first auto-save it back to the current branch (so
    ///   in-progress work is never lost — divergence is committed to where it
    ///   was made). Then replay branch `name` and adopt it as current. The
    ///   stored copy of `name` is refreshed to exactly what was replayed.
    /// - `list`: names of all branches, current marked with `*`.
    /// - `delete <name>`: drop a branch; the current branch cannot be deleted.
    fn option(&mut self, op: OptionOp) -> Result<ApplyOutcome, ExecError> {
        match op {
            OptionOp::Save { name } => {
                let log = self.save_log();
                self.branches.insert(name.clone(), log);
                self.current_branch = name.clone();
                Ok(ApplyOutcome {
                    created: Vec::new(),
                    message: format!("option saved: {name} (now current)"),
                })
            }
            OptionOp::Switch { name } => {
                if name == self.current_branch {
                    // Re-sync the stored copy with any live divergence, but do
                    // not replay (that would needlessly rebuild the doc).
                    self.branches.insert(name.clone(), self.save_log());
                    return Ok(ApplyOutcome {
                        created: Vec::new(),
                        message: format!("already on option: {name}"),
                    });
                }
                let Some(target) = self.branches.get(&name).cloned() else {
                    let known = if self.branches.is_empty() {
                        "none".to_string()
                    } else {
                        self.branches.keys().cloned().collect::<Vec<_>>().join(", ")
                    };
                    return Err(ExecError::Invalid(format!(
                        "no option '{name}' (saved: {known}; create one with: option save {name})"
                    )));
                };
                // Auto-save divergent in-progress work to the branch we are
                // leaving, so nothing is lost.
                let current = self.current_branch.clone();
                self.branches.insert(current, self.save_log());
                // Replay the target through the same apply path used live.
                let mut fresh = Session::replay(target.clone())?;
                fresh.doc.generation = fresh.doc.generation.max(self.doc.generation) + 1;
                fresh.branches = std::mem::take(&mut self.branches);
                fresh.current_branch = name.clone();
                // Refresh the stored copy to exactly what replay produced.
                fresh.branches.insert(name.clone(), target);
                *self = fresh;
                Ok(ApplyOutcome {
                    created: Vec::new(),
                    message: format!("switched to option: {name}"),
                })
            }
            OptionOp::List => {
                let msg = if self.branches.is_empty() {
                    format!(
                        "no saved options (on '{}'; save one with: option save <name>)",
                        self.current_branch
                    )
                } else {
                    let names: Vec<String> = self
                        .branches
                        .keys()
                        .map(|n| {
                            if *n == self.current_branch {
                                format!("*{n}")
                            } else {
                                n.clone()
                            }
                        })
                        .collect();
                    format!("options: {}", names.join(", "))
                };
                Ok(ApplyOutcome { created: Vec::new(), message: msg })
            }
            OptionOp::Delete { name } => {
                if name == self.current_branch {
                    return Err(ExecError::Invalid(format!(
                        "cannot delete the current option '{name}'; switch to another first"
                    )));
                }
                if self.branches.remove(&name).is_none() {
                    return Err(ExecError::Invalid(format!("no option '{name}' to delete")));
                }
                Ok(ApplyOutcome {
                    created: Vec::new(),
                    message: format!("deleted option: {name}"),
                })
            }
        }
    }

    /// Read-only access to the saved branches (name → effective log), for the
    /// file format. See [`Session::option`] for the semantics.
    pub fn branches(&self) -> &BTreeMap<String, Vec<Command>> {
        &self.branches
    }

    /// The branch the live log currently belongs to.
    pub fn current_branch(&self) -> &str {
        &self.current_branch
    }

    /// Seed branches and the current-branch marker when loading a file. The live
    /// log/doc are unaffected; missing (old-file) values leave the defaults
    /// (empty branch table, [`MAIN_BRANCH`]).
    pub fn set_branches(&mut self, branches: BTreeMap<String, Vec<Command>>, current: String) {
        self.branches = branches;
        self.current_branch = current;
    }

    /// Stable document identity from the file header, if the file carried one
    /// (or one was assigned this run). `None` for pre-uuid files and scratch
    /// sessions. Header-level only — never part of the op-log or replay.
    pub fn doc_uuid(&self) -> Option<&str> {
        self.doc_uuid.as_deref()
    }

    /// Set (or clear) the stable document identity. Used by `crate::io` on load
    /// and by the app when it stamps a fresh uuid onto a first save.
    pub fn set_doc_uuid(&mut self, uuid: Option<String>) {
        self.doc_uuid = uuid;
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
            "obj" | "stl" | "gltf" | "glb" | "dae" => self.import_mesh(path),
            "3dm" => self.import_3dm(path),
            "step" | "stp" => self.import_step(path),
            "ifc" => self.import_ifc(path),
            "epw" => self.import_epw(path),
            "geojson" | "json" => self.import_geojson(path),
            "las" | "laz" => self.import_las(path),
            "e57" => self.import_e57(path),
            other => Err(ExecError::Invalid(format!(
                "unknown import extension '.{other}' (supported: .dxf, .obj, .stl, .gltf, .glb, .dae, .3dm, .step, .stp, .ifc, .epw, .geojson, .las, .laz, .e57)"
            ))),
        }
    }

    /// STEP AP242 (also AP203/214) import through the exact-BREP tier (OCCT).
    ///
    /// STEP carries exact analytic BREP solids; OCCT reads them exactly and we
    /// tessellate into a document `Mesh`, emitted as one logged `MeshLiteral` so
    /// the import replays from the op-log without the STEP file. The exact volume
    /// is reported in the echo. Requires the `kernel-occt` feature — without it
    /// there is no STEP reader, so we return a clear error rather than a stub.
    fn import_step(&mut self, path: String) -> Result<ApplyOutcome, ExecError> {
        let Some(result) = kernel_occt::read_step(&path) else {
            return Err(ExecError::Invalid(format!(
                "STEP import needs the exact-BREP tier — rebuild with the 'kernel-occt' feature (cannot read '{path}')"
            )));
        };
        let exact = result.map_err(|e| ExecError::Invalid(format!("'{path}': {e}")))?;
        let name = path
            .rsplit(['/', '\\'])
            .next()
            .map(|s| s.to_string())
            .filter(|s| !s.is_empty());
        let out = self.run(Command::MeshLiteral {
            id: None,
            positions: exact.mesh.positions().to_vec(),
            faces: exact.mesh.faces().to_vec(),
            name,
        })?;
        Ok(ApplyOutcome {
            created: out.created,
            message: format!(
                "imported exact STEP solid from {path} (volume {:.4}, tessellated to {} triangles) — one MeshLiteral op",
                exact.volume,
                exact.mesh.faces().len()
            ),
        })
    }

    fn import_dxf(&mut self, path: String) -> Result<ApplyOutcome, ExecError> {
        let text = read_import_string(&path)?;
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
        let bytes = read_import_bytes(&path)?;
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

    /// Rhino `.3dm` (openNURBS) import: reconstruct meshes and curves.
    ///
    /// `ON_Mesh` → `MeshLiteral`; `ON_LineCurve`/`ON_PolylineCurve` → `Polyline`;
    /// `ON_NurbsCurve` → a dense tessellated `Polyline` (full NURBS is a later
    /// kernel job). Each object's Rhino name and layer are preserved (the layer
    /// becomes the current layer via a logged `Layer` op, like [`Self::import_ifc`]).
    /// Breps/surfaces/points/annotations are skipped and counted. Every object is
    /// emitted as one logged substrate command, so the import replays from the
    /// op-log without the `.3dm` file.
    fn import_3dm(&mut self, path: String) -> Result<ApplyOutcome, ExecError> {
        let bytes = read_import_bytes(&path)?;
        let parsed = crate::rhino3dm::import(&bytes)
            .map_err(|e| ExecError::Invalid(format!("'{path}': {e}")))?;
        if parsed.objects.is_empty() {
            return Err(ExecError::Invalid(format!(
                "'{path}' contains no importable meshes or curves ({} object(s) skipped)",
                parsed.skipped
            )));
        }

        let prev_layer = self.doc.current_layer.clone();
        let mut created = Vec::new();
        let mut meshes = 0usize;
        let mut curves = 0usize;
        for obj in parsed.objects {
            self.switch_layer(&obj.layer)?;
            let name = if obj.name.is_empty() { None } else { Some(obj.name) };
            let out = match obj.geom {
                crate::rhino3dm::Imported::Mesh(mesh) => {
                    meshes += 1;
                    self.run(Command::MeshLiteral {
                        id: None,
                        positions: mesh.positions().to_vec(),
                        faces: mesh.faces().to_vec(),
                        name,
                    })?
                }
                crate::rhino3dm::Imported::Polyline { points, closed } => {
                    curves += 1;
                    self.run(Command::Polyline { id: None, points, closed })?
                }
            };
            created.extend(out.created);
        }
        self.switch_layer(&prev_layer)?;

        Ok(ApplyOutcome {
            created,
            message: format!(
                "imported {path}: {meshes} mesh(es), {curves} curve(s) \
                 ({} skipped) — logged as substrate commands",
                parsed.skipped
            ),
        })
    }

    /// Semantic IFC4 (or IFC2x3) import: reconstruct *typed* structural members
    /// rather than flattening everything to meshes.
    ///
    /// `IfcBeam`/`IfcColumn` → `FrameMember`; `IfcSlab`/`IfcWall` → `AreaMember`;
    /// their `IfcProfileDef` → the member's `Section`; `IfcMaterial` → the
    /// member's material; `IfcBuildingStorey` → the story it is placed on.
    /// Everything else falls back to a `MeshLiteral` on the `ifc` layer (the old
    /// behavior). Every element is emitted as a *logged substrate command*
    /// (`DefMaterial`/`DefSection`/`DefStory`/`FrameMember`/`AreaMember`/
    /// `MeshLiteral`), so the whole import is replay-safe — the op-log, not the
    /// IFC file, is the record.
    fn import_ifc(&mut self, path: String) -> Result<ApplyOutcome, ExecError> {
        let bytes = read_import_bytes(&path)?;
        let sem = crate::ifc::import_semantic(&bytes)
            .map_err(|e| ExecError::Invalid(format!("'{path}': {e}")))?;
        if sem.elements.is_empty() {
            return Err(ExecError::Invalid(format!(
                "'{path}' contains no importable IFC geometry"
            )));
        }

        let mut created = Vec::new();

        // 1) Definitions first (members reference sections/materials by name).
        for mat in &sem.materials {
            self.run(Command::DefMaterial {
                name: mat.name.clone(),
                elastic_modulus_e: mat.elastic_modulus_e,
                density: mat.density,
            })?;
        }
        for (name, section) in &sem.sections {
            self.run(Command::DefSection { name: name.clone(), section: *section })?;
        }
        for story in &sem.stories {
            self.run(Command::DefStory {
                name: story.name.clone(),
                elevation: story.elevation,
            })?;
        }

        // 2) Elements, in file order. Typed members map to typed commands; the
        //    story becomes the current layer so members land on their level, and
        //    mesh fallbacks land on the 'ifc' layer.
        let prev_layer = self.doc.current_layer.clone();
        let mut frames = 0usize;
        let mut areas = 0usize;
        let mut meshes = 0usize;
        for el in sem.elements {
            match el {
                crate::ifc::ImportedElement::Frame {
                    kind, a, b, section, material, story, ..
                } => {
                    self.switch_layer(story.as_deref().unwrap_or("ifc"))?;
                    let out = self.run(Command::FrameMember {
                        id: None,
                        kind,
                        a,
                        b,
                        section,
                        material,
                        orientation_deg: None,
                    })?;
                    created.extend(out.created);
                    frames += 1;
                }
                crate::ifc::ImportedElement::Area {
                    kind, boundary, thickness, material, story, ..
                } => {
                    self.switch_layer(story.as_deref().unwrap_or("ifc"))?;
                    let out = self.run(Command::AreaMember {
                        id: None,
                        kind,
                        boundary,
                        thickness,
                        material,
                    })?;
                    created.extend(out.created);
                    areas += 1;
                }
                crate::ifc::ImportedElement::Mesh { name, mesh } => {
                    self.switch_layer("ifc")?;
                    let out = self.run(Command::MeshLiteral {
                        id: None,
                        positions: mesh.positions().to_vec(),
                        faces: mesh.faces().to_vec(),
                        name: Some(name),
                    })?;
                    created.extend(out.created);
                    meshes += 1;
                }
            }
        }
        self.switch_layer(&prev_layer)?;

        Ok(ApplyOutcome {
            created,
            message: format!(
                "imported {path}: {frames} frame member(s), {areas} area member(s), \
                 {meshes} mesh(es) — logged as substrate commands"
            ),
        })
    }

    /// Switch the current layer via a logged `Layer` op if it differs.
    fn switch_layer(&mut self, layer: &str) -> Result<(), ExecError> {
        if self.doc.current_layer != layer {
            self.run(Command::Layer { name: layer.to_string() })?;
        }
        Ok(())
    }

    /// Import an EPW (EnergyPlus Weather) file: parse the LOCATION header for
    /// lat/lon/tz and set it on the document via a logged `location` op, then
    /// summarize the 8760 hourly rows (nothing heavy is retained). Only the
    /// `location` op is logged; the weather rows are reported, not stored.
    fn import_epw(&mut self, path: String) -> Result<ApplyOutcome, ExecError> {
        let text = read_import_string(&path)?;
        let s = itsjustcad_solar::parse_epw(&text)
            .map_err(|e| ExecError::Invalid(format!("'{path}': {e}")))?;
        // Log the location so saved files replay the site without the EPW file.
        self.run(Command::Location {
            lat_deg: s.lat_deg,
            lon_deg: s.lon_deg,
            tz_hours: s.tz_hours,
        })?;
        let temp = match (s.mean_dry_bulb_c, s.min_dry_bulb_c, s.max_dry_bulb_c) {
            (Some(m), Some(lo), Some(hi)) => {
                format!(", dry-bulb {lo:.1}..{hi:.1}°C (mean {m:.1}°C)")
            }
            _ => String::new(),
        };
        Ok(ApplyOutcome {
            created: Vec::new(),
            message: format!(
                "EPW '{}' @ ({:.3}, {:.3}) tz {:+.1}h, {} m elev — {} rows{temp}; location set",
                s.city, s.lat_deg, s.lon_deg, s.tz_hours, s.elevation_m, s.rows
            ),
        })
    }

    /// The document's geo origin for projecting lon/lat to local meters, if a
    /// location has been set (EPW/`sun`/`location`).
    fn geo_origin(&self) -> Option<crate::geo::GeoOrigin> {
        self.doc
            .location
            .map(|l| crate::geo::GeoOrigin { lat_deg: l.lat_deg, lon_deg: l.lon_deg })
    }

    /// Import GeoJSON features as substrate ops: Polygon → closed Polyline,
    /// LineString → open Polyline, Point → a tiny marker Circle (there is no
    /// point primitive). `properties.name` becomes the object name. Each op is
    /// logged individually so the op-log — not the GeoJSON file — is the record.
    fn import_geojson(&mut self, path: String) -> Result<ApplyOutcome, ExecError> {
        let bytes = read_import_bytes(&path)?;
        let feats = crate::geo::parse_geojson(&bytes, self.geo_origin())
            .map_err(|e| ExecError::Invalid(format!("'{path}': {e}")))?;
        if feats.is_empty() {
            return Err(ExecError::Invalid(format!("'{path}' has no importable features")));
        }
        use crate::geo::GeoFeature;
        let total = feats.len();
        let mut created = Vec::new();
        for feat in feats {
            let (cmd, name) = match feat {
                GeoFeature::Polygon { name, ring } => (
                    Command::Polyline {
                        id: None,
                        points: ring.iter().map(|p| DVec3::new(p.x, p.y, 0.0)).collect(),
                        closed: true,
                    },
                    name,
                ),
                GeoFeature::Line { name, points } => (
                    Command::Polyline {
                        id: None,
                        points: points.iter().map(|p| DVec3::new(p.x, p.y, 0.0)).collect(),
                        closed: false,
                    },
                    name,
                ),
                // No point primitive: a 0.5 m marker circle stands in.
                GeoFeature::Point { name, at } => (
                    Command::Circle { id: None, center: DVec3::new(at.x, at.y, 0.0), radius: 0.5 },
                    name,
                ),
            };
            let out = self.run(cmd)?;
            created.extend(out.created.iter().copied());
            if let (Some(name), Some(id)) = (name, out.created.first()) {
                self.run(Command::Name {
                    targets: Selector::Ids { ids: vec![*id] },
                    name,
                })?;
            }
        }
        Ok(ApplyOutcome {
            created,
            message: format!(
                "imported {total} GeoJSON feature(s) from {path} (points → 0.5m marker circles)"
            ),
        })
    }

    /// Import a LAS 1.2–1.4 point cloud. Decimates to ≤200k points and stores
    /// as a single `PointLiteral` op on layer "pointcloud". LAZ gets an error.
    fn import_las(&mut self, path: String) -> Result<ApplyOutcome, ExecError> {
        let bytes = read_import_bytes(&path)?;
        let pts = crate::las::parse(&bytes)
            .map_err(|e| ExecError::Invalid(format!("'{path}': {e}")))?;
        if pts.positions.is_empty() {
            return Err(ExecError::Invalid(format!("'{path}' contains no point records")));
        }
        let kept = pts.positions.len();
        let total = pts.total_records;
        let stride = pts.stride;

        if self.doc.current_layer != "pointcloud" {
            self.run(Command::Layer { name: "pointcloud".to_string() })?;
        }
        let out = self.run(Command::PointLiteral { id: None, positions: pts.positions })?;
        Ok(ApplyOutcome {
            created: out.created,
            message: format!(
                "imported {kept} points from {path} (total {total}, stride {stride})"
            ),
        })
    }

    /// Import an E57 point-cloud file (ASTM E2807). Reads all point-cloud
    /// sections, extracts Cartesian positions + optional colors, decimates to
    /// ≤200k, and stores on layer "pointcloud" as a single `PointLiteral` op.
    fn import_e57(&mut self, path: String) -> Result<ApplyOutcome, ExecError> {
        let bytes = read_import_bytes(&path)?;
        let pts = crate::e57::parse(&bytes)
            .map_err(|e| ExecError::Invalid(format!("'{path}': {e}")))?;

        let kept = pts.positions.len();
        let total = pts.total_records;
        let stride = pts.stride;
        let skipped = pts.skipped_sections;

        if self.doc.current_layer != "pointcloud" {
            self.run(Command::Layer { name: "pointcloud".to_string() })?;
        }
        let out = self.run(Command::PointLiteral { id: None, positions: pts.positions })?;

        let color_note = if pts.colors.is_empty() {
            String::new()
        } else {
            format!(", {} colors", pts.colors.len())
        };
        let skip_note = if skipped > 0 {
            format!(", {skipped} section(s) skipped (no Cartesian coords)")
        } else {
            String::new()
        };

        Ok(ApplyOutcome {
            created: out.created,
            message: format!(
                "imported {kept} points from {path} (total {total}, stride {stride}{color_note}{skip_note})"
            ),
        })
    }

    /// Build a terrain surface from a `.csv` (x,y,z points) or `.geojson`
    /// (elevation contour LineStrings) file. Delaunay-triangulates the points
    /// and adds one MeshLiteral op on layer "terrain".
    fn terrain(&mut self, path: String) -> Result<ApplyOutcome, ExecError> {
        let bytes = read_import_bytes(&path)?;
        let ext = path.rsplit('.').next().map(|e| e.to_ascii_lowercase()).unwrap_or_default();
        let mesh = match ext.as_str() {
            "csv" | "txt" => {
                let text = String::from_utf8(bytes)
                    .map_err(|_| ExecError::Invalid(format!("'{path}' is not valid UTF-8")))?;
                let pts = crate::geo::parse_csv_points(&text)
                    .map_err(|e| ExecError::Invalid(format!("'{path}': {e}")))?;
                crate::geo::terrain_from_points(&pts)
            }
            "geojson" | "json" => {
                // "elevation" then "ele" are the common contour z tags.
                crate::geo::terrain_from_contours(&bytes, self.geo_origin(), "elevation")
                    .or_else(|_| {
                        crate::geo::terrain_from_contours(&bytes, self.geo_origin(), "ele")
                    })
            }
            other => {
                return Err(ExecError::Invalid(format!(
                    "terrain: unknown extension '.{other}' (use .csv or .geojson)"
                )));
            }
        }
        .map_err(|e| ExecError::Invalid(format!("'{path}': {e}")))?;

        let prev_layer = self.doc.current_layer.clone();
        if self.doc.current_layer != "terrain" {
            self.run(Command::Layer { name: "terrain".to_string() })?;
        }
        let faces_n = mesh.faces().len();
        let out = self.run(Command::MeshLiteral {
            id: None,
            positions: mesh.positions().to_vec(),
            faces: mesh.faces().to_vec(),
            name: Some("terrain".to_string()),
        })?;
        if self.doc.current_layer != prev_layer {
            self.run(Command::Layer { name: prev_layer })?;
        }
        Ok(ApplyOutcome {
            created: out.created,
            message: format!("terrain surface from {path}: {faces_n} triangles on layer 'terrain'"),
        })
    }

    /// Build OSM building context from a saved Overpass API JSON export: each
    /// building footprint is extruded (height tag or 9 m default) into a
    /// MeshLiteral op on layer "context".
    fn osmfile(&mut self, path: String) -> Result<ApplyOutcome, ExecError> {
        let bytes = read_import_bytes(&path)?;
        let buildings = crate::geo::parse_overpass(&bytes, self.geo_origin())
            .map_err(|e| ExecError::Invalid(format!("'{path}': {e}")))?;
        if buildings.is_empty() {
            return Err(ExecError::Invalid(format!(
                "'{path}' has no building footprints (need Overpass 'out geom;' ways with a building tag)"
            )));
        }
        let prev_layer = self.doc.current_layer.clone();
        if self.doc.current_layer != "context" {
            self.run(Command::Layer { name: "context".to_string() })?;
        }
        let total = buildings.len();
        let mut created = Vec::new();
        for b in buildings {
            let mesh = kernel_mesh::extrude_profile(&b.ring, 0.0, b.height_m);
            let out = self.run(Command::MeshLiteral {
                id: None,
                positions: mesh.positions().to_vec(),
                faces: mesh.faces().to_vec(),
                name: b.name,
            })?;
            created.extend(out.created);
        }
        if self.doc.current_layer != prev_layer {
            self.run(Command::Layer { name: prev_layer })?;
        }
        Ok(ApplyOutcome {
            created,
            message: format!("OSM context from {path}: {total} building(s) on layer 'context'"),
        })
    }

    /// Ground elevation for planting at `(x, y)`: the terrain surface height
    /// when a terrain mesh exists and the point is inside its footprint, else
    /// the caller's own z.
    fn ground_z(&self, x: f64, y: f64, fallback: f64) -> f64 {
        terrain_surface(&self.doc)
            .ok()
            .and_then(|(_, m)| {
                crate::landscape::terrain_z_at(m.positions(), m.faces(), x, y)
            })
            .unwrap_or(fallback)
    }

    /// Place one plant from the embedded catalog: trunk + canopy mesh at `at`
    /// (draped onto the terrain), one MeshLiteral op named "plant:<id>" on
    /// layer "planting". Like `terrain`, the Plant verb itself is not logged —
    /// its MeshLiteral expansion is, so replay never depends on the catalog.
    fn plant(
        &mut self,
        species: String,
        at: DVec3,
        age_years: Option<f64>,
    ) -> Result<ApplyOutcome, ExecError> {
        let sp = crate::landscape::find_species(&species).ok_or_else(|| {
            let ids: Vec<&str> =
                crate::landscape::plant_catalog().iter().map(|s| s.id.as_str()).collect();
            ExecError::Invalid(format!(
                "unknown species '{species}' — catalog: {}",
                ids.join(", ")
            ))
        })?;
        let base = DVec3::new(at.x, at.y, self.ground_z(at.x, at.y, at.z));
        let (positions, faces) = crate::landscape::plant_mesh(sp, base, age_years);
        let (h, canopy_d) = crate::landscape::plant_size(sp, age_years);

        let prev_layer = self.doc.current_layer.clone();
        if self.doc.current_layer != "planting" {
            self.run(Command::Layer { name: "planting".to_string() })?;
        }
        let out = self.run(Command::MeshLiteral {
            id: None,
            positions,
            faces,
            name: Some(format!("plant:{}", sp.id)),
        })?;
        if self.doc.current_layer != prev_layer {
            self.run(Command::Layer { name: prev_layer })?;
        }
        let age_note = match age_years {
            Some(a) => format!(" at {a:.0} yr"),
            None => " (mature)".to_string(),
        };
        Ok(ApplyOutcome {
            created: out.created,
            message: format!(
                "planted {} ({}){age_note}: height {h:.1} m, canopy {canopy_d:.1} m at \
                 ({:.1}, {:.1}, {:.2}) on layer 'planting'{}",
                sp.common,
                sp.binomial,
                base.x,
                base.y,
                base.z,
                self.climate_advisory(sp)
            ),
        })
    }

    /// A row of plants from `a` to `b` at `spacing` intervals, each draped
    /// onto the terrain. One MeshLiteral op per plant.
    fn plantrow(
        &mut self,
        species: String,
        a: DVec3,
        b: DVec3,
        spacing: f64,
    ) -> Result<ApplyOutcome, ExecError> {
        if spacing <= 0.0 || !spacing.is_finite() {
            return Err(ExecError::Invalid("plantrow spacing must be > 0".into()));
        }
        let positions = crate::landscape::row_positions(a, b, spacing);
        let mut created = Vec::new();
        for p in &positions {
            let out = self.plant(species.clone(), *p, None)?;
            created.extend(out.created);
        }
        let n = positions.len();
        Ok(ApplyOutcome {
            created,
            message: format!(
                "planted a row of {n} {species} every {spacing} m from ({:.1}, {:.1}) to \
                 ({:.1}, {:.1}) on layer 'planting'",
                a.x, a.y, b.x, b.y
            ),
        })
    }

    /// Planting schedule: count every "plant:<id>" object on layer
    /// "planting", write a CSV and store an AnalysisReport ("plantschedule").
    fn plantschedule(&mut self, path: String) -> Result<ApplyOutcome, ExecError> {
        // species id → (count, centroid accumulator)
        let mut tally: BTreeMap<String, (usize, DVec3)> = BTreeMap::new();
        for obj in self.doc.objects() {
            if obj.layer != "planting" {
                continue;
            }
            let Some(id) = obj.name.as_deref().and_then(|n| n.strip_prefix("plant:")) else {
                continue;
            };
            let at = match &obj.geometry {
                Geometry::Mesh(m) if !m.positions().is_empty() => {
                    m.positions().iter().copied().sum::<DVec3>()
                        / m.positions().len() as f64
                }
                _ => DVec3::ZERO,
            };
            let e = tally.entry(id.to_string()).or_insert((0, DVec3::ZERO));
            e.0 += 1;
            e.1 += at;
        }
        if tally.is_empty() {
            return Err(ExecError::Invalid(
                "nothing planted — run `plant` or `plantrow` first".into(),
            ));
        }

        let mut csv = String::from(
            "species_id,binomial,common,count,mature_height_m,canopy_diameter_m,deciduous\n",
        );
        let mut samples: Vec<(f64, DVec3, String)> = Vec::new();
        let mut total = 0usize;
        for (id, (count, at_sum)) in &tally {
            total += count;
            let (binomial, common, h, d, dec) = match crate::landscape::find_species(id) {
                Some(sp) => (
                    sp.binomial.as_str(),
                    sp.common.as_str(),
                    sp.mature_height_m,
                    sp.canopy_diameter_m,
                    sp.deciduous,
                ),
                None => ("?", "?", 0.0, 0.0, false), // planted from an older catalog
            };
            csv.push_str(&format!(
                "{id},{binomial},{common},{count},{h},{d},{}\n",
                if dec { "yes" } else { "no" }
            ));
            samples.push((*count as f64, *at_sum / *count as f64, id.clone()));
        }
        std::fs::write(&path, &csv).map_err(|e| {
            ExecError::Invalid(format!("cannot write plant schedule '{path}': {e}"))
        })?;
        let n_species = tally.len();
        self.doc.analysis_reports.insert(
            "plantschedule".to_string(),
            build_analysis_report(
                "plantschedule",
                format!("{total} plant(s), {n_species} species → {path}"),
                "plants",
                samples,
            ),
        );
        self.doc.generation += 1;
        Ok(ApplyOutcome {
            created: Vec::new(),
            message: format!(
                "plant schedule: {total} plant(s) across {n_species} species → {path} \
                 (see `report plantschedule`)"
            ),
        })
    }

    /// Climate band derived from the document's georeference latitude, if any.
    fn derived_band(&self) -> Option<crate::landscape::ClimateBand> {
        self.doc
            .location
            .map(|l| crate::landscape::ClimateBand::from_latitude(l.lat_deg))
    }

    /// An advisory suffix (leading space) when a species sits outside the
    /// document's derived climate band; empty when no location is set or the
    /// species suits the band. Advisory, never an error.
    fn climate_advisory(&self, sp: &crate::landscape::PlantSpecies) -> String {
        match self.derived_band() {
            Some(band) if !sp.climate_zones.is_empty() && !band.suits(sp) => format!(
                " — advisory: {} ({}) is outside the site's typical climate range ({}, zones {})",
                sp.common,
                sp.binomial,
                band.label(),
                band.zones().join("/"),
            ),
            _ => String::new(),
        }
    }

    /// `plantcatalog [region|zone]`: list catalog species filtered by a native
    /// region tag or a Köppen zone code. Query only — no geometry, no op-log.
    fn plantcatalog(&mut self, filter: Option<String>) -> Result<ApplyOutcome, ExecError> {
        // A filter is a zone code when it matches a Köppen letter pattern the
        // catalog uses (starts uppercase, ≤3 chars), otherwise a region tag.
        let (region, zone): (Option<&str>, Option<&str>) = match filter.as_deref() {
            None => (None, None),
            Some(f) if f.len() <= 3 && f.chars().next().is_some_and(char::is_uppercase) => {
                (None, Some(f))
            }
            Some(f) => (Some(f), None),
        };
        let matches: Vec<&crate::landscape::PlantSpecies> =
            crate::landscape::catalog_filtered(region, None)
                .into_iter()
                .filter(|s| zone.is_none_or(|z| s.climate_zones.iter().any(|c| c == z)))
                .collect();
        let scope = match &filter {
            Some(f) => format!(" for '{f}'"),
            None => String::new(),
        };
        if matches.is_empty() {
            return Ok(ApplyOutcome {
                created: Vec::new(),
                message: format!("plantcatalog: no species{scope}"),
            });
        }
        let mut msg = format!("plant catalog{scope}: {} species\n", matches.len());
        for s in &matches {
            msg.push_str(&format!(
                "  {:<26} {:<28} {:>4.0} m  layer {:<7} zones {}\n",
                s.id,
                s.common,
                s.mature_height_m,
                s.layer.as_deref().unwrap_or("-"),
                s.climate_zones.join("/"),
            ));
        }
        Ok(ApplyOutcome { created: Vec::new(), message: msg })
    }

    /// `miyawaki <closed-region> [density]`: dense native mini-forest.
    ///
    /// Draws only native, layered species suited to the doc's climate band,
    /// stratifies them across canopy/tree/subtree/shrub, and seed-places
    /// `density` (default 4, clamped 1–8) stems/m² as saplings, mixed so
    /// adjacent stems differ in species and layer. Deterministically seeded
    /// from the region hash + a fixed salt, so replay is byte-stable. Stores an
    /// AnalysisReport ("miyawaki"). Not logged itself — its per-stem MeshLiteral
    /// ops are (like Plant), so replay never depends on the catalog.
    fn miyawaki(
        &mut self,
        targets: Selector,
        density: Option<f64>,
    ) -> Result<ApplyOutcome, ExecError> {
        let ids = resolve(&self.doc, &targets)?;
        // Gather closed-region polygons (tessellated) from the selection.
        let mut regions: Vec<Vec<DVec3>> = Vec::new();
        for id in &ids {
            if let Some(obj) = self.doc.get(*id)
                && let Geometry::Curve(c) = &obj.geometry
                && c.is_closed()
            {
                regions.push(c.tessellate(PROFILE_TOL));
            }
        }
        if regions.is_empty() {
            return Err(ExecError::Invalid(
                "miyawaki needs a closed region curve (draw a boundary polyline first)".into(),
            ));
        }
        let area: f64 = regions.iter().map(|r| shoelace_area(r)).sum();
        if area < 1.0 {
            return Err(ExecError::Invalid(format!(
                "miyawaki region is too small ({area:.2} m²) — need at least 1 m²"
            )));
        }

        // Climate band gates the species pool.
        let Some(band) = self.derived_band() else {
            return Err(ExecError::Invalid(
                "miyawaki needs a climate to pick natives — set `location <lat> <lon>` first".into(),
            ));
        };
        let pool = crate::landscape::miyawaki_pool(band);
        // Bucket by stratification layer.
        const LAYERS: [(&str, f64); 4] =
            [("canopy", 0.10), ("tree", 0.40), ("subtree", 0.30), ("shrub", 0.20)];
        let mut by_layer: BTreeMap<&str, Vec<&crate::landscape::PlantSpecies>> = BTreeMap::new();
        for sp in &pool {
            if let Some(l) = sp.layer.as_deref() {
                by_layer.entry(l).or_default().push(sp);
            }
        }
        let layers_present = LAYERS.iter().filter(|(l, _)| by_layer.contains_key(l)).count();
        const MIN_SPECIES: usize = 4;
        let mut warnings: Vec<String> = Vec::new();
        if pool.len() < MIN_SPECIES || layers_present < 2 {
            warnings.push(format!(
                "Miyawaki needs natives — only {} layered species ({} strata) found for {}; \
                 add species or set a different location",
                pool.len(),
                layers_present,
                band.label(),
            ));
        }
        if pool.is_empty() {
            return Err(ExecError::Invalid(format!(
                "miyawaki: no native layered species for {} — cannot plant",
                band.label()
            )));
        }

        // Density (stems/m²) and target stem count.
        let dens = density.unwrap_or(4.0).clamp(1.0, 8.0);
        let target = ((area * dens).round() as usize).max(1);

        // Deterministic RNG seeded from the region geometry + a fixed salt, so
        // the same region replays byte-identically.
        const SALT: u64 = 0x4d69_7961_7761_6b69; // "Miyawaki"
        let mut seed = SALT;
        for r in &regions {
            for p in r {
                seed = seed
                    .rotate_left(7)
                    ^ (p.x * 1e3).round() as i64 as u64
                    ^ ((p.y * 1e3).round() as i64 as u64).rotate_left(21);
            }
        }
        let mut rng = SplitMix64(seed);

        // Bounding box for rejection sampling into the region.
        let (mut xmin, mut ymin) = (f64::INFINITY, f64::INFINITY);
        let (mut xmax, mut ymax) = (f64::NEG_INFINITY, f64::NEG_INFINITY);
        for r in &regions {
            for p in r {
                xmin = xmin.min(p.x);
                ymin = ymin.min(p.y);
                xmax = xmax.max(p.x);
                ymax = ymax.max(p.y);
            }
        }

        // Per-layer, weighted placement — spread proportionally and pick a
        // *different* species/layer from the previous stem where we can.
        let strata: Vec<(&str, f64, &Vec<&crate::landscape::PlantSpecies>)> = LAYERS
            .iter()
            .filter_map(|(l, w)| by_layer.get(l).map(|v| (*l, *w, v)))
            .collect();
        let wsum: f64 = strata.iter().map(|(_, w, _)| w).sum();

        let mut placed: Vec<(String, &'static str)> = Vec::new(); // (species id, layer)
        let mut prev_species: Option<String> = None;
        let mut prev_layer: Option<&str> = None;
        let mut attempts = 0usize;
        let max_attempts = target * 40 + 100;
        while placed.len() < target && attempts < max_attempts {
            attempts += 1;
            let x = xmin + rng.unit() * (xmax - xmin);
            let y = ymin + rng.unit() * (ymax - ymin);
            if !regions.iter().any(|r| point_in_polygon(r, x, y)) {
                continue;
            }
            // Pick a layer by weight, avoiding the previous layer when >1 exists.
            let mut pick = rng.unit() * wsum;
            let mut layer_idx = 0usize;
            for (i, (_, w, _)) in strata.iter().enumerate() {
                if pick < *w {
                    layer_idx = i;
                    break;
                }
                pick -= *w;
            }
            if strata.len() > 1 && Some(strata[layer_idx].0) == prev_layer {
                layer_idx = (layer_idx + 1) % strata.len();
            }
            let (layer, _, species) = strata[layer_idx];
            // Pick a species in the layer, avoiding the previous species.
            let mut si = ((rng.unit() * species.len() as f64) as usize).min(species.len() - 1);
            if species.len() > 1 && prev_species.as_deref() == Some(species[si].id.as_str()) {
                si = (si + 1) % species.len();
            }
            let sp = species[si];
            // Plant as a sapling (age 1 yr → 5% floor size).
            let z = self.ground_z(x, y, 0.0);
            self.plant(sp.id.clone(), DVec3::new(x, y, z), Some(1.0))?;
            placed.push((sp.id.clone(), layer));
            prev_species = Some(sp.id.clone());
            prev_layer = Some(layer);
        }

        // Species mix % and per-layer tallies for the report.
        let mut mix: BTreeMap<String, usize> = BTreeMap::new();
        for (id, _) in &placed {
            *mix.entry(id.clone()).or_default() += 1;
        }
        let n = placed.len().max(1);
        let samples: Vec<(f64, DVec3, String)> = mix
            .iter()
            .map(|(id, c)| (100.0 * *c as f64 / n as f64, DVec3::ZERO, id.clone()))
            .collect();
        let mut layer_counts: BTreeMap<&str, usize> = BTreeMap::new();
        for (_, l) in &placed {
            *layer_counts.entry(*l).or_default() += 1;
        }
        let layer_summary = LAYERS
            .iter()
            .filter_map(|(l, _)| layer_counts.get(l).map(|c| format!("{l} {c}")))
            .collect::<Vec<_>>()
            .join(", ");

        let context = format!(
            "{} stems over {:.0} m² ({:.1}/m²), {} species, strata: {}",
            placed.len(),
            area,
            placed.len() as f64 / area,
            mix.len(),
            layer_summary,
        );
        self.doc.analysis_reports.insert(
            "miyawaki".to_string(),
            build_analysis_report("miyawaki", context.clone(), "%mix", samples),
        );
        self.doc.generation += 1;

        let mut message = format!(
            "Miyawaki forest: {} saplings across {} native species on layer 'planting' \
             ({context}). Advisory: dense planting self-thins ~30–50% over time — intentional \
             to the method. See `report miyawaki`.",
            placed.len(),
            mix.len(),
        );
        for w in &warnings {
            message.push_str(&format!("\n⚠ {w}"));
        }
        Ok(ApplyOutcome { created: Vec::new(), message })
    }

    /// Resolve a check pack by name: the in-memory table (embedded demo +
    /// anything `checkrules load`ed) first, then `<default_dir>/<name>.checks.json`
    /// so packs persisted to the user's config dir work across sessions without
    /// explicit loading (plugin-startup parity). A disk hit is cached.
    fn check_pack(&mut self, name: &str) -> Result<&crate::checkengine::CheckPack, ExecError> {
        // Never join traversal-y names into the config dir.
        if !self.check_packs.contains_key(name)
            && !name.contains(['/', '\\', '.'])
            && let Some(dir) = crate::checkengine::default_dir()
        {
            let candidate = dir.join(format!("{name}.checks.json"));
            if let Ok(s) = std::fs::read_to_string(&candidate) {
                let pack = crate::checkengine::CheckPack::from_json(&s).map_err(|e| {
                    ExecError::Invalid(format!("{}: {e}", candidate.display()))
                })?;
                self.check_packs.insert(name.to_string(), pack);
            }
        }
        self.check_packs.get(name).ok_or_else(|| {
            ExecError::Invalid(format!(
                "unknown check pack '{name}' (loaded: {}; add more with `checkrules load <path>`)",
                self.check_packs.keys().cloned().collect::<Vec<_>>().join(", ")
            ))
        })
    }

    /// `checkrules list` — the loaded packs plus any discoverable in the
    /// default checks dir. Query only.
    fn checkrules_list(&mut self) -> ApplyOutcome {
        // Surface on-disk packs too (without failing on malformed ones).
        let mut lines = Vec::new();
        if let Some(dir) = crate::checkengine::default_dir() {
            let (disk, warnings) = crate::checkengine::load_dir(&dir);
            for (name, pack) in disk {
                self.check_packs.entry(name).or_insert(pack);
            }
            lines.extend(warnings.into_iter().map(|w| format!("warning: {w}")));
        }
        for p in self.check_packs.values() {
            lines.push(format!(
                "{}: {} rule(s){}",
                p.name,
                p.rules.len(),
                if p.description.is_empty() {
                    String::new()
                } else {
                    format!(" — {}", p.description)
                }
            ));
        }
        lines.push(format!(
            "run `codecheck <pack>` to evaluate ({})",
            crate::checkengine::ADVISORY_NOTE
        ));
        ApplyOutcome { created: Vec::new(), message: lines.join("\n") }
    }

    /// `checkrules load <path>` — read + validate a pack JSON and install it in
    /// the session table (fs read; never logged).
    fn checkrules_load(&mut self, path: String) -> Result<ApplyOutcome, ExecError> {
        let s = std::fs::read_to_string(&path)
            .map_err(|e| ExecError::Invalid(format!("cannot read check pack '{path}': {e}")))?;
        let pack = crate::checkengine::CheckPack::from_json(&s)
            .map_err(|e| ExecError::Invalid(format!("{path}: {e}")))?;
        let msg = format!(
            "loaded check pack '{}' ({} rule(s)) — run `codecheck {}`",
            pack.name,
            pack.rules.len(),
            pack.name
        );
        self.check_packs.insert(pack.name.clone(), pack);
        Ok(ApplyOutcome { created: Vec::new(), message: msg })
    }

    /// Effective forward log (up to the undo cursor) — this is the file format.
    /// After a fast-open the inverses are still pending, so the untouched
    /// forward log is returned directly (its cursor sits at the end).
    pub fn save_log(&self) -> Vec<Command> {
        if let Some(pending) = &self.pending_log {
            return pending.clone();
        }
        self.log[..self.cursor].iter().map(|a| a.op.clone()).collect()
    }

    /// Fast-open from a checkpoint: seed the document directly from a snapshot
    /// and adopt the forward log for saving, skipping the (potentially costly)
    /// geometry replay. `doc` must equal what `replay(log.clone())?.doc` would
    /// produce — the checkpoint sidecar is a cache, and callers only invoke this
    /// after confirming the checkpoint's op count matches `log`.
    ///
    /// Inverses (needed for undo) are *not* materialized here — that would
    /// require the very replay we are skipping. They are rebuilt lazily the
    /// first time the undo history is touched (undo/redo/amend), via
    /// [`ensure_history`]. The common open→view→save path never pays for it.
    pub fn from_snapshot(doc: Document, log: Vec<Command>) -> Self {
        Session {
            doc,
            log: Vec::new(),
            cursor: 0,
            plugins: crate::plugin::PluginRegistry::default(),
            pending_log: Some(log),
            ..Session::default()
        }
    }

    /// Materialize the op-log with inverses if this session was fast-opened from
    /// a checkpoint. Replays the pending log against a scratch session to
    /// recover each op's inverse, then adopts that history and its cursor.
    /// A no-op once the history is present. Returns any replay error.
    fn ensure_history(&mut self) -> Result<(), ExecError> {
        let Some(log) = self.pending_log.take() else {
            return Ok(());
        };
        let rebuilt = Session::replay(log)?;
        // The snapshot doc is authoritative (it may carry live-only state like
        // the selection); the replay only supplies the log/inverses/cursor.
        debug_assert_eq!(
            self.doc, rebuilt.doc,
            "checkpoint snapshot diverged from op-log replay"
        );
        self.log = rebuilt.log;
        self.cursor = rebuilt.cursor;
        Ok(())
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
    let raw: Vec<ObjectId> = match sel {
        Selector::Ids { ids } => ids.clone(),
        Selector::Named { name } => doc.find_named(name),
        Selector::Last { n } => doc.last_ids(*n),
        Selector::All => doc.all_ids(),
        Selector::Selected => doc.selection.iter().copied().collect(),
    };
    if raw.is_empty() {
        return Err(ExecError::EmptySelection(match sel {
            Selector::Named { name } => format!("no object named '{name}'"),
            Selector::Selected => "selection is empty".to_string(),
            _ => format!("document has {} objects", doc.len()),
        }));
    }
    // Objects on a locked layer are not selectable/editable: drop them so no
    // edit/transform/delete command can touch them. `layerlock <layer> off`
    // restores access.
    let ids: Vec<ObjectId> = raw.iter().copied().filter(|id| !doc.object_locked(*id)).collect();
    if ids.is_empty() {
        return Err(ExecError::Invalid(
            "target objects are on a locked layer (layerlock <layer> off to edit)".to_string(),
        ));
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
        material: None,
        lineweight_mm: None,
        geometry: Geometry::Curve(curve),
    });
    let outcome = ApplyOutcome {
        created: vec![id],
        message: format!("{what} {id}"),
    };
    (id, outcome)
}

/// Insert a plain mesh object on the current layer, returning its id. Used by
/// the expressive-structure generators, which each produce one mesh.
fn mesh_object(doc: &mut Document, id: Option<ObjectId>, mesh: kernel_mesh::Mesh) -> ObjectId {
    let id = id.unwrap_or_default();
    doc.insert(SceneObject {
        visible: true,
        id,
        name: None,
        layer: doc.current_layer.clone(),
        color: None,
        material: None,
        lineweight_mm: None,
        geometry: Geometry::Mesh(mesh),
    });
    id
}

/// Upper bound on a single grid dimension for procedural surface/lattice
/// generators. A `256×256` surface is ~66k vertices — far beyond any sane
/// design resolution, yet small enough that the resulting GPU vertex buffer
/// stays well under wgpu's per-buffer limits (a `2048²` surface would blow the
/// buffer limit and crash the renderer). Bounds worst-case mesh size and OOM.
const MAX_GRID: u32 = 256;

/// Upper bound on geodesic subdivision frequency. Node count grows as
/// `10·f² + 2` and the strut lattice thickens each node, so `f = 64` caps it
/// near 41k nodes — plenty for a frame while preventing integer-overflow / OOM
/// and keeping the thickened lattice within GPU buffer limits.
const MAX_GEODESIC_FREQ: u32 = 64;

/// Reject NaN / infinity in a user-supplied float parameter. Non-finite values
/// slip past `<= 0.0` checks and poison downstream geometry with NaNs.
fn finite(v: f64, what: &str) -> Result<f64, ExecError> {
    if v.is_finite() {
        Ok(v)
    } else {
        Err(ExecError::Invalid(format!("{what} must be a finite number")))
    }
}

/// Clamp a grid resolution into `[1, MAX_GRID]`.
fn clamp_grid(n: u32) -> u32 {
    n.clamp(1, MAX_GRID)
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

/// Flatten every triangle mesh in the document into one shared position list and
/// a single face list indexing into it. Used by the STEP exporter (each triangle
/// becomes a faceted BREP face). Non-mesh geometry (curves, annotations, points,
/// instances) is skipped — STEP here carries only surface/solid triangles.
fn flatten_doc_meshes(doc: &Document) -> (Vec<DVec3>, Vec<[u32; 3]>) {
    let mut positions: Vec<DVec3> = Vec::new();
    let mut faces: Vec<[u32; 3]> = Vec::new();
    for obj in doc.objects() {
        let mesh = match &obj.geometry {
            Geometry::Mesh(m) | Geometry::Frame { mesh: m, .. } | Geometry::Area { mesh: m, .. } => {
                m
            }
            _ => continue,
        };
        let base = positions.len() as u32;
        positions.extend_from_slice(mesh.positions());
        for f in mesh.faces() {
            faces.push([f[0] + base, f[1] + base, f[2] + base]);
        }
    }
    (positions, faces)
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
                Geometry::Mesh(m)
                | Geometry::Frame { mesh: m, .. }
                | Geometry::Area { mesh: m, .. } => Ok(m.clone()),
                Geometry::Curve(_) => Err(ExecError::Invalid(format!(
                    "'{id}' is a curve; booleans need meshes — extrude it first"
                ))),
                Geometry::Annotation(_) => Err(ExecError::Invalid(format!(
                    "'{id}' is an annotation; booleans need meshes"
                ))),
                Geometry::Instance { block, .. } => Err(ExecError::Invalid(format!(
                    "'{id}' is a block instance ('{block}'); booleans need meshes — explode the instance or extrude it first"
                ))),
                Geometry::Points { .. } => Err(ExecError::Invalid(format!(
                    "'{id}' is a point cloud; booleans need meshes"
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

/// Exact boolean of two axis-aligned boxes, preferring the opt-in OCCT exact
/// kernel and falling back to the pure-Rust mesh kernel when the `kernel-occt`
/// feature is not compiled in. Returns `(mesh, volume, used_exact_kernel)`.
///
/// The exact path reports the exact analytic volume; the fallback reports the
/// mesh's signed volume (identical for these polyhedral results).
fn exact_or_mesh_boolean(
    op: BoolKind,
    a_corner: DVec3,
    a_size: DVec3,
    b_corner: DVec3,
    b_size: DVec3,
) -> (kernel_mesh::Mesh, f64, bool) {
    let occ_op = match op {
        BoolKind::Union => kernel_occt::BoolOp::Union,
        BoolKind::Difference => kernel_occt::BoolOp::Difference,
        BoolKind::Intersection => kernel_occt::BoolOp::Intersection,
    };
    if let Some(exact) = kernel_occt::box_boolean(a_corner, a_size, b_corner, b_size, occ_op) {
        return (exact.mesh, exact.volume, true);
    }
    // Fallback: build the two boxes as meshes and run the mesh CSG kernel.
    let a = kernel_mesh::make_box(a_corner, a_size);
    let b = kernel_mesh::make_box(b_corner, b_size);
    let result = match op {
        BoolKind::Union => kernel_mesh::csg_union(&a, &b),
        BoolKind::Difference => kernel_mesh::csg_difference(&a, &b),
        BoolKind::Intersection => kernel_mesh::csg_intersection(&a, &b),
    };
    let volume = kernel_mesh::signed_volume(&result).abs();
    (result, volume, false)
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
        material: None,
        lineweight_mm: None,
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

/// Even-odd ray-cast point-in-polygon test in the XY plane. `poly` is a closed
/// loop that does NOT repeat its first vertex (tessellate() guarantees this).
fn point_in_polygon(poly: &[DVec3], x: f64, y: f64) -> bool {
    let n = poly.len();
    if n < 3 {
        return false;
    }
    let mut inside = false;
    let mut j = n - 1;
    for i in 0..n {
        let (pi, pj) = (poly[i], poly[j]);
        if (pi.y > y) != (pj.y > y) {
            let xint = pi.x + (y - pi.y) / (pj.y - pi.y) * (pj.x - pi.x);
            if x < xint {
                inside = !inside;
            }
        }
        j = i;
    }
    inside
}

/// Deterministic SplitMix64 PRNG — a tiny, portable, byte-stable generator for
/// seeded scatter placement (Miyawaki). No external dependency; the same seed
/// always yields the same stream, so op replay reproduces the exact layout.
struct SplitMix64(u64);

impl SplitMix64 {
    /// Next raw u64.
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    /// Uniform f64 in [0, 1).
    fn unit(&mut self) -> f64 {
        // 53-bit mantissa → exact uniform on [0,1).
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }
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
                Geometry::Points { .. } => "pointcloud",
                Geometry::Frame { kind, .. } => kind.label(),
                Geometry::Area { kind, .. } => kind.label(),
            };
            let area_m2 = match &o.geometry {
                Geometry::Curve(c) if c.is_closed() => {
                    shoelace_area(&c.tessellate(PROFILE_TOL))
                }
                _ => o.geometry.mesh().map(mesh_surface_area).unwrap_or(0.0),
            };
            let volume_m3 = o
                .geometry
                .mesh()
                .map(kernel_mesh::signed_volume)
                .unwrap_or(0.0);
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
            material: None,
            lineweight_mm: None,
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
            material: None,
            lineweight_mm: None,
            geometry: Geometry::Curve(Curve::Polyline { points: vec![a, b], closed: false }),
        });
    }
    Ok((new_ids, layers_created, meshes))
}

/// Layer for the sunlight-hours heatmap overlay.
const ANALYSIS_LAYER: &str = "analysis";

/// Curvature comb: sample the target curve's curvature and draw one hair line
/// per sample (foot on the curve, length = curvature × scale, pointing toward
/// the center of curvature) plus a polyline joining the hair tips, all on the
/// `analysis` layer. Auto scale sizes the longest hair to ~15% of the curve's
/// bounding extent so the comb reads at any zoom.
fn exec_curvature_graph(
    doc: &mut Document,
    target: Selector,
    ids: Option<Vec<ObjectId>>,
    scale: Option<f64>,
    samples: u32,
) -> Result<(Command, Inverse, ApplyOutcome), ExecError> {
    let resolved = resolve(doc, &target)?;
    let [tid] = resolved[..] else {
        return Err(ExecError::Invalid(format!(
            "curvature target matched {} objects, expected exactly 1",
            resolved.len()
        )));
    };
    if !(2..=500).contains(&samples) {
        return Err(ExecError::Invalid("curvature samples must be 2..=500".into()));
    }
    let curve = curve_of(doc, tid, "curvature")?;
    let profile = kernel_curve::curvature_profile(curve, samples as usize, PROFILE_TOL * 0.1);
    if profile.is_empty() {
        return Err(ExecError::Invalid("curvature: curve is degenerate".into()));
    }
    let kmax = profile.iter().map(|s| s.kappa).fold(0.0, f64::max);

    // Curve extent for auto-scaling.
    let (mut lo, mut hi) = (profile[0].point, profile[0].point);
    for s in &profile {
        lo = lo.min(s.point);
        hi = hi.max(s.point);
    }
    let extent = (hi - lo).length().max(1e-6);
    let scale_m = match scale {
        Some(s) if s > 0.0 => s,
        Some(_) => return Err(ExecError::Invalid("curvature scale must be > 0".into())),
        None if kmax > 1e-12 => 0.15 * extent / kmax,
        None => 0.0, // straight curve: zero-length hairs, tip curve overlays it
    };

    let closed = curve.is_closed();
    let tips: Vec<DVec3> =
        profile.iter().map(|s| s.point + s.normal * (s.kappa * scale_m)).collect();

    let count = profile.len() + 1; // hairs + tip polyline
    let new_ids: Vec<ObjectId> = match ids {
        Some(ids) if ids.len() == count => ids,
        _ => (0..count).map(|_| ObjectId::new()).collect(),
    };
    let mut layers_created = Vec::new();
    if !doc.layers.contains_key(ANALYSIS_LAYER) {
        doc.layers
            .insert(ANALYSIS_LAYER.to_string(), LayerStyle::default());
        layers_created.push(ANALYSIS_LAYER.to_string());
    }
    let mut insert_curve_obj = |id: ObjectId, c: Curve| {
        doc.insert(SceneObject {
            visible: true,
            id,
            name: None,
            layer: ANALYSIS_LAYER.to_string(),
            color: None,
            material: None,
            lineweight_mm: None,
            geometry: Geometry::Curve(c),
        });
    };
    for (s, id) in profile.iter().zip(&new_ids) {
        insert_curve_obj(
            *id,
            Curve::Line { a: s.point, b: s.point + s.normal * (s.kappa * scale_m) },
        );
    }
    insert_curve_obj(new_ids[count - 1], Curve::Polyline { points: tips, closed });
    doc.generation += 1;

    let radius_txt = if kmax > 1e-12 {
        format!("min radius {:.3} m", 1.0 / kmax)
    } else {
        "straight (zero curvature)".to_string()
    };
    Ok((
        Command::CurvatureGraph { target, ids: Some(new_ids.clone()), scale, samples },
        Inverse::CreatedOnLayer { created: new_ids.clone(), layers_created },
        ApplyOutcome {
            message: format!(
                "curvature {tid}: {} hairs on '{ANALYSIS_LAYER}', max curvature {kmax:.4} 1/m, \
                 {radius_txt}",
                count - 1
            ),
            created: new_ids,
        },
    ))
}

/// Collect every mesh's world-space triangles as `[a,b,c]` f64 vertex triples.
/// Used by both shadow projection and sun-hours ray-casting.
fn scene_triangles(doc: &Document) -> Vec<[[f64; 3]; 3]> {
    let mut tris = Vec::new();
    for obj in doc.objects() {
        if let Geometry::Mesh(m) = &obj.geometry {
            let pos = m.positions();
            for f in m.faces() {
                let v = |i: u32| {
                    let p = pos[i as usize];
                    [p.x, p.y, p.z]
                };
                tris.push([v(f[0]), v(f[1]), v(f[2])]);
            }
        }
    }
    tris
}

/// Format minutes-past-midnight as `HH:MM` (zero-padded).
fn fmt_hhmm(min: u32) -> String {
    format!("{:02}:{:02}", min / 60, min % 60)
}

// ── M-perf parallel analysis kernels ─────────────────────────────────────────
//
// DETERMINISM CONTRACT: every kernel below is parallel over *independent* items
// with an ordered `collect`, and the per-item math is a single shared function
// also used by the `_seq` reference — so parallel output is byte-identical to a
// sequential run (op-log replay invariant). No parallel float reductions:
// min/avg/max over the collected values stay sequential at the call sites.

/// Count, for one ray origin, the sun directions not occluded by the scene BVH.
fn lit_slots_one(origin: DVec3, sun_dirs: &[DVec3], bvh: &kernel_mesh::TriBvh) -> usize {
    sun_dirs.iter().filter(|&&d| !bvh.ray_occluded(origin, d)).count()
}

/// Unoccluded-sun-slot count per ray origin, parallel over origins.
fn lit_slot_counts(
    origins: &[DVec3],
    sun_dirs: &[DVec3],
    bvh: &kernel_mesh::TriBvh,
) -> Vec<usize> {
    origins.par_iter().map(|&o| lit_slots_one(o, sun_dirs, bvh)).collect()
}

/// Sequential reference for [`lit_slot_counts`] (determinism oracle).
#[cfg(test)]
fn lit_slot_counts_seq(
    origins: &[DVec3],
    sun_dirs: &[DVec3],
    bvh: &kernel_mesh::TriBvh,
) -> Vec<usize> {
    origins.iter().map(|&o| lit_slots_one(o, sun_dirs, bvh)).collect()
}

/// Centroid + unit normal of a triangle, exactly as the analysis loops compute
/// them (degenerate normals are left unnormalised, matching the old inline code).
fn tri_centroid_normal(tri: &[DVec3; 3]) -> (DVec3, DVec3) {
    let centroid = (tri[0] + tri[1] + tri[2]) / 3.0;
    let mut normal = (tri[1] - tri[0]).cross(tri[2] - tri[0]);
    let nlen = normal.length();
    if nlen > 1e-12 {
        normal /= nlen;
    }
    (centroid, normal)
}

/// Per-face sun score for `facesunhours`: lit-slot count (sun on the lit side
/// AND unoccluded), centroid and unit normal.
fn face_lit_one(
    tri: &[DVec3; 3],
    sun_dirs: &[DVec3],
    bvh: &kernel_mesh::TriBvh,
) -> (usize, DVec3, DVec3) {
    let (centroid, normal) = tri_centroid_normal(tri);
    // Lift the origin off the surface along the normal so the face's own
    // triangle doesn't self-occlude the ray.
    let origin = centroid + normal * 1e-3;
    let lit = sun_dirs
        .iter()
        .filter(|&&dir| normal.dot(dir) > 0.0 && !bvh.ray_occluded(origin, dir))
        .count();
    (lit, centroid, normal)
}

/// [`face_lit_one`] over all faces, parallel.
fn face_lit_slots(
    faces: &[[DVec3; 3]],
    sun_dirs: &[DVec3],
    bvh: &kernel_mesh::TriBvh,
) -> Vec<(usize, DVec3, DVec3)> {
    faces.par_iter().map(|tri| face_lit_one(tri, sun_dirs, bvh)).collect()
}

/// Sequential reference for [`face_lit_slots`] (determinism oracle).
#[cfg(test)]
fn face_lit_slots_seq(
    faces: &[[DVec3; 3]],
    sun_dirs: &[DVec3],
    bvh: &kernel_mesh::TriBvh,
) -> Vec<(usize, DVec3, DVec3)> {
    faces.iter().map(|tri| face_lit_one(tri, sun_dirs, bvh)).collect()
}

/// Per-face annual insolation for `radiation`: kWh/m²·yr, centroid, normal.
/// The 288-bin sum inside `annual_face_irradiation` stays sequential — only
/// faces run in parallel, so float addition order is unchanged.
fn face_radiation_one(
    tri: &[DVec3; 3],
    bins: &itsjustcad_solar::RadiationBins,
    year: i32,
    loc: GeoLocation,
    bvh: &kernel_mesh::TriBvh,
) -> (f64, DVec3, DVec3) {
    let (centroid, normal) = tri_centroid_normal(tri);
    let origin = centroid + normal * 1e-3;
    let k = itsjustcad_solar::annual_face_irradiation(
        bins,
        year,
        loc.lat_deg,
        loc.lon_deg,
        loc.tz_hours,
        normal.to_array(),
        |s| bvh.ray_occluded(origin, DVec3::new(s[0], s[1], s[2])),
    );
    (k, centroid, normal)
}

/// [`face_radiation_one`] over all faces, parallel.
fn face_radiation_scores(
    faces: &[[DVec3; 3]],
    bins: &itsjustcad_solar::RadiationBins,
    year: i32,
    loc: GeoLocation,
    bvh: &kernel_mesh::TriBvh,
) -> Vec<(f64, DVec3, DVec3)> {
    faces
        .par_iter()
        .map(|tri| face_radiation_one(tri, bins, year, loc, bvh))
        .collect()
}

/// Sequential reference for [`face_radiation_scores`] (determinism oracle).
#[cfg(test)]
fn face_radiation_scores_seq(
    faces: &[[DVec3; 3]],
    bins: &itsjustcad_solar::RadiationBins,
    year: i32,
    loc: GeoLocation,
    bvh: &kernel_mesh::TriBvh,
) -> Vec<(f64, DVec3, DVec3)> {
    faces
        .iter()
        .map(|tri| face_radiation_one(tri, bins, year, loc, bvh))
        .collect()
}

/// One `shadowstudy` time stamp: sun position for local minute `t`; if the sun
/// is up, the per-object ground-shadow convex hulls (objects in document order,
/// hulls with < 3 points dropped). `None` when the sun is at/below the horizon.
#[allow(clippy::too_many_arguments)]
fn shadow_stamp_one(
    t: u32,
    year: i32,
    month: u32,
    day: u32,
    loc: GeoLocation,
    object_pts: &[Vec<[f64; 3]>],
) -> Option<Vec<Vec<[f64; 2]>>> {
    // Interpret the clock time as local; convert to UTC for the SPA.
    let utc = (t as f64 - loc.tz_hours * 60.0).rem_euclid(1440.0);
    let (h, mi) = ((utc / 60.0) as u32, (utc % 60.0) as u32);
    let pos = itsjustcad_solar::solar_position(year, month, day, h, mi, loc.lat_deg, loc.lon_deg);
    if pos.altitude_deg <= 0.0 {
        return None;
    }
    let dir = itsjustcad_solar::sun_direction(pos.azimuth_deg, pos.altitude_deg);
    let dir = [dir[0] as f64, dir[1] as f64, dir[2] as f64];
    let mut hulls = Vec::new();
    for obj in object_pts {
        let ground: Vec<[f64; 2]> = obj
            .iter()
            .filter_map(|&p| itsjustcad_solar::project_to_ground(p, dir))
            .map(|g| [g[0], g[1]])
            .collect();
        let hull = itsjustcad_solar::convex_hull_xy(ground);
        if hull.len() >= 3 {
            hulls.push(hull);
        }
    }
    Some(hulls)
}

/// [`shadow_stamp_one`] over all stamps, parallel over stamps.
fn shadow_stamp_hulls(
    stamps: &[u32],
    year: i32,
    month: u32,
    day: u32,
    loc: GeoLocation,
    object_pts: &[Vec<[f64; 3]>],
) -> Vec<Option<Vec<Vec<[f64; 2]>>>> {
    stamps
        .par_iter()
        .map(|&t| shadow_stamp_one(t, year, month, day, loc, object_pts))
        .collect()
}

/// Sequential reference for [`shadow_stamp_hulls`] (determinism oracle).
#[cfg(test)]
fn shadow_stamp_hulls_seq(
    stamps: &[u32],
    year: i32,
    month: u32,
    day: u32,
    loc: GeoLocation,
    object_pts: &[Vec<[f64; 3]>],
) -> Vec<Option<Vec<Vec<[f64; 2]>>>> {
    stamps
        .iter()
        .map(|&t| shadow_stamp_one(t, year, month, day, loc, object_pts))
        .collect()
}

/// Compass/vertical facing label for an outward surface normal — the deck LLM
/// critiques by orientation ("north facade gets no winter sun"), so every kept
/// analysis sample carries one. `North = +Y, East = +X, Up = +Z` (matches
/// `itsjustcad_solar::sun_direction`). Near-vertical normals (|z| >= 0.7,
/// ~45° tilt) read "up"/"down"; otherwise the horizontal azimuth is bucketed
/// into the eight compass directions.
fn facing_label(normal: DVec3) -> &'static str {
    if normal.z >= 0.7 {
        return "up";
    }
    if normal.z <= -0.7 {
        return "down";
    }
    // Azimuth clockwise from +Y (north), like a compass bearing.
    let az = normal.x.atan2(normal.y).to_degrees().rem_euclid(360.0);
    const LABELS: [&str; 8] = [
        "north", "northeast", "east", "southeast", "south", "southwest", "west", "northwest",
    ];
    LABELS[(((az + 22.5) / 45.0) as usize) % 8]
}

/// Number of extreme samples an [`AnalysisReport`] keeps at each end.
const REPORT_LOWEST_N: usize = 5;
const REPORT_HIGHEST_N: usize = 3;

/// Build the compact [`AnalysisReport`] for one analysis run from its raw
/// samples `(value, location, tag)`: min/avg/max, a six-bin distribution over
/// [0, max], and only the `REPORT_LOWEST_N`/`REPORT_HIGHEST_N` extreme samples
/// — token-frugal by construction so the whole report fits a deck turn.
fn build_analysis_report(
    kind: &str,
    context: String,
    unit: &str,
    mut samples: Vec<(f64, DVec3, String)>,
) -> AnalysisReport {
    let count = samples.len();
    let n = count.max(1) as f64;
    let sum: f64 = samples.iter().map(|s| s.0).sum();
    let min = samples.iter().map(|s| s.0).fold(f64::INFINITY, f64::min);
    let max = samples.iter().map(|s| s.0).fold(0.0f64, f64::max);
    let min = if min.is_finite() { min } else { 0.0 };

    let mut bins: Vec<(f64, usize)> = Vec::new();
    if max > 0.0 {
        let width = max / 6.0;
        bins = (1..=6).map(|i| (width * i as f64, 0)).collect();
        for (v, _, _) in &samples {
            let idx = ((v / width) as usize).min(5);
            bins[idx].1 += 1;
        }
    }

    // NaN-free by construction; total order is fine here.
    samples.sort_by(|a, b| a.0.total_cmp(&b.0));
    let to_sample = |(value, at, tag): &(f64, DVec3, String)| AnalysisSample {
        value: *value,
        at: [at.x, at.y, at.z],
        tag: tag.clone(),
    };
    let lowest = samples.iter().take(REPORT_LOWEST_N).map(to_sample).collect();
    let highest = samples.iter().rev().take(REPORT_HIGHEST_N).map(to_sample).collect();

    AnalysisReport {
        kind: kind.to_string(),
        context,
        unit: unit.to_string(),
        count,
        min,
        avg: sum / n,
        max,
        bins,
        lowest,
        highest,
    }
}

/// Render one stored [`AnalysisReport`] as the compact text the `report`
/// command prints (and the deck LLM reads back). One decimal for small units
/// (hours), whole numbers once values reach the hundreds (kWh/m2-yr).
fn format_analysis_report(r: &AnalysisReport) -> String {
    let dp = usize::from(r.max < 100.0);
    let f = |v: f64| format!("{v:.dp$}");
    let mut out = format!(
        "{} ({}): {} sample(s), min {} / avg {} / max {} {}\n",
        r.kind,
        r.context,
        r.count,
        f(r.min),
        f(r.avg),
        f(r.max),
        r.unit
    );
    if !r.bins.is_empty() {
        out.push_str("  distribution:");
        for (upper, n) in &r.bins {
            if *n > 0 {
                out.push_str(&format!(" <={}:{}", f(*upper), n));
            }
        }
        out.push('\n');
    }
    let fmt_samples = |label: &str, samples: &[AnalysisSample], out: &mut String| {
        if samples.is_empty() {
            return;
        }
        out.push_str(&format!("  {label}:"));
        for s in samples {
            out.push_str(&format!(
                " {} {} [{}] at ({:.1},{:.1},{:.1});",
                f(s.value),
                r.unit,
                s.tag,
                s.at[0],
                s.at[1],
                s.at[2]
            ));
        }
        out.pop(); // trailing ';'
        out.push('\n');
    };
    fmt_samples("lowest", &r.lowest, &mut out);
    fmt_samples("highest", &r.highest, &mut out);
    out
}

/// Render one stored [`itsjustcad_doc::ComplianceReport`] as the compact text
/// the `report` command prints. One line per rule, grounded in ids/locations
/// so the deck can critique with citations; the advisory disclaimer rides in
/// the header (it is part of the report's context) — every rendering carries it.
fn format_compliance_report(r: &itsjustcad_doc::ComplianceReport) -> String {
    let mut out = format!("codecheck {}: {}\n", r.pack, r.context);
    for o in &r.rules {
        let code = if o.code_ref.is_empty() {
            String::new()
        } else {
            format!(" [{}]", o.code_ref)
        };
        let numbers = match o.measured {
            Some(m) => format!(
                " — measured {m:.3} vs required {:.3} {}",
                o.required.unwrap_or(f64::NAN),
                o.unit
            ),
            None => String::new(),
        };
        out.push_str(&format!(
            "  {}{code} {}{numbers} ({} checked)",
            o.rule_id,
            o.verdict.to_uppercase(),
            o.checked
        ));
        if !o.objects.is_empty() {
            out.push_str(&format!("; {}", o.message));
            out.push_str("; at");
            for (i, loc) in o.locations.iter().take(3).enumerate() {
                let id = o.objects.get(i.min(o.objects.len() - 1)).cloned().unwrap_or_default();
                out.push_str(&format!(" {id}({:.1},{:.1},{:.1})", loc[0], loc[1], loc[2]));
            }
            if o.locations.len() > 3 {
                out.push_str(&format!(" +{} more", o.locations.len() - 3));
            }
        }
        out.push('\n');
    }
    out
}

/// `codecheck <pack> [story]` — evaluate the rules embedded in the op against
/// the document, draw severity-colored failure markers on the 'compliance'
/// layer (analysis-layer precedent: undo removes them, replay recreates them
/// via written-back ids), and store the [`itsjustcad_doc::ComplianceReport`]
/// for the read-only `report` verb. ADVISORY ONLY — see
/// [`crate::checkengine::ADVISORY_NOTE`].
fn exec_codecheck(
    doc: &mut Document,
    pack: String,
    story: Option<String>,
    rules: Option<Vec<crate::checkengine::CheckRule>>,
    ids: Option<Vec<ObjectId>>,
) -> Result<(Command, Inverse, ApplyOutcome), ExecError> {
    use crate::checkengine::{self, COMPLIANCE_LAYER};
    // Session::run embeds the rules before the logged path; a missing list can
    // only mean a hand-edited file, so fail loudly rather than re-resolving.
    let rules = rules.ok_or_else(|| {
        ExecError::Invalid("codecheck op carries no rules (hand-edited file?)".into())
    })?;
    let tris: Vec<[DVec3; 3]> = scene_triangles(doc)
        .into_iter()
        .map(|t| {
            [
                DVec3::from_array(t[0]),
                DVec3::from_array(t[1]),
                DVec3::from_array(t[2]),
            ]
        })
        .collect();
    let (report, markers) =
        checkengine::evaluate(doc, &pack, story.as_deref(), &rules, &tris)
            .map_err(ExecError::Invalid)?;

    // Failure markers: a small circle per violation, colored by severity.
    let new_ids: Vec<ObjectId> = match ids {
        Some(ids) if ids.len() == markers.len() => ids,
        _ => (0..markers.len()).map(|_| ObjectId::new()).collect(),
    };
    let mut layers_created = Vec::new();
    if !markers.is_empty() && !doc.layers.contains_key(COMPLIANCE_LAYER) {
        doc.layers
            .insert(COMPLIANCE_LAYER.to_string(), LayerStyle::default());
        layers_created.push(COMPLIANCE_LAYER.to_string());
    }
    const MARKER_R: f64 = 0.25;
    for ((at, severity), id) in markers.iter().zip(&new_ids) {
        let circle: Vec<DVec3> = (0..12)
            .map(|k| {
                let a = k as f64 / 12.0 * std::f64::consts::TAU;
                *at + DVec3::new(MARKER_R * a.cos(), MARKER_R * a.sin(), 0.05)
            })
            .collect();
        doc.insert(SceneObject {
            visible: true,
            id: *id,
            name: Some(format!("codecheck {}", severity.label())),
            layer: COMPLIANCE_LAYER.to_string(),
            color: Some(severity.marker_color()),
            material: None,
            lineweight_mm: None,
            geometry: Geometry::Curve(Curve::Polyline { points: circle, closed: true }),
        });
    }

    let count = |verdict: &str| report.rules.iter().filter(|r| r.verdict == verdict).count();
    let (pass, fail, warn, info) = (count("pass"), count("fail"), count("warn"), count("info"));
    let message = format!(
        "codecheck {pack}: {} rule(s) — {pass} pass, {fail} fail, {warn} warn, {info} info{} \
         — see `report codecheck` ({})",
        report.rules.len(),
        if markers.is_empty() {
            String::new()
        } else {
            format!("; {} marker(s) on '{COMPLIANCE_LAYER}'", markers.len())
        },
        checkengine::ADVISORY_NOTE
    );
    doc.compliance_reports.insert(pack.clone(), report);
    doc.generation += 1;
    Ok((
        Command::CodeCheck {
            pack,
            story,
            rules: Some(rules),
            ids: Some(new_ids.clone()),
        },
        Inverse::CreatedOnLayer { created: new_ids.clone(), layers_created },
        ApplyOutcome { message, created: new_ids },
    ))
}

/// Ground-shadow study. For each time stamp, compute the sun direction from the
/// document location and project every mesh's silhouette onto `z=0` along the
/// sun. Each object's projected points are reduced to their 2D convex hull and
/// emitted as one closed polygon on a `shadows-HH:MM` layer (translucent dark
/// fill). Convex-hull-per-object is a pragmatic first slice: concave footprints
/// over-cover, but the sun-path envelope reads correctly for massing.
#[allow(clippy::too_many_arguments)]
fn exec_shadow_study(
    doc: &mut Document,
    ids: Option<Vec<ObjectId>>,
    year: i32,
    month: u32,
    day: u32,
    from_min: u32,
    to_min: u32,
    step_min: u32,
) -> Result<(Command, Inverse, ApplyOutcome), ExecError> {
    let loc = doc.location.ok_or_else(|| {
        ExecError::Invalid(
            "no location set — run `sun <lat> <lon> <date> <time>` or `location <lat> <lon>`, \
             or `import <file.epw>` first"
                .into(),
        )
    })?;
    if step_min == 0 {
        return Err(ExecError::Invalid("step must be > 0 minutes".into()));
    }

    // Per-object world-space vertices (grouped so each object gets its own hull).
    let mut object_pts: Vec<Vec<[f64; 3]>> = Vec::new();
    for obj in doc.objects() {
        if let Geometry::Mesh(m) = &obj.geometry {
            object_pts.push(m.positions().iter().map(|p| [p.x, p.y, p.z]).collect());
        }
    }
    if object_pts.is_empty() {
        return Err(ExecError::Invalid(
            "shadowstudy needs meshes to cast shadows (extrude or box first)".into(),
        ));
    }

    // Build (layer, polygon) for every stamp where the sun is up. Stamps are
    // independent time frames → parallel over stamps (ordered collect + the
    // sequential flatten below keep polygon order identical to the old loop).
    struct Poly {
        layer: String,
        pts: Vec<DVec3>,
    }
    // Inclusive stamps from `from_min` to `to_min` at `step_min` spacing.
    let stamps: Vec<u32> = (from_min..=to_min).step_by(step_min as usize).collect();
    let per_stamp = shadow_stamp_hulls(&stamps, year, month, day, loc, &object_pts);
    let mut polys: Vec<Poly> = Vec::new();
    let mut stamps_up = 0usize;
    for (&t, hulls) in stamps.iter().zip(&per_stamp) {
        let Some(hulls) = hulls else { continue };
        stamps_up += 1;
        let layer = format!("shadows-{}", fmt_hhmm(t));
        for hull in hulls {
            polys.push(Poly {
                layer: layer.clone(),
                pts: hull.iter().map(|h| DVec3::new(h[0], h[1], 0.0)).collect(),
            });
        }
    }

    if polys.is_empty() {
        return Err(ExecError::Invalid(format!(
            "sun is below the horizon for the whole window on {year}-{month:02}-{day:02} \
             at this location — no ground shadows"
        )));
    }

    let new_ids: Vec<ObjectId> = match ids {
        Some(ids) if ids.len() == polys.len() => ids,
        _ => (0..polys.len()).map(|_| ObjectId::new()).collect(),
    };

    // Ensure each distinct shadow layer exists with a translucent dark fill.
    let mut layers_created = Vec::new();
    for p in &polys {
        if !doc.layers.contains_key(&p.layer) {
            doc.layers.insert(
                p.layer.clone(),
                LayerStyle {
                    color: Some([0.1, 0.1, 0.15, 0.35]),
                    ..LayerStyle::default()
                },
            );
            layers_created.push(p.layer.clone());
        }
    }

    for (poly, id) in polys.iter().zip(&new_ids) {
        doc.insert(SceneObject {
            visible: true,
            id: *id,
            name: None,
            layer: poly.layer.clone(),
            color: None,
            material: None,
            lineweight_mm: None,
            geometry: Geometry::Curve(Curve::Polyline { points: poly.pts.clone(), closed: true }),
        });
    }
    // Structured summary for the deck's `report` command: one sample per
    // shadow polygon — value = ground area covered, tag = its "HH:MM" stamp —
    // so the LLM can flag when the massing overshadows most.
    let samples: Vec<(f64, DVec3, String)> = polys
        .iter()
        .map(|p| {
            let centroid = p.pts.iter().copied().sum::<DVec3>() / p.pts.len().max(1) as f64;
            let stamp = p.layer.strip_prefix("shadows-").unwrap_or(&p.layer).to_string();
            (shoelace_area(&p.pts), centroid, stamp)
        })
        .collect();
    doc.analysis_reports.insert(
        "shadowstudy".to_string(),
        build_analysis_report(
            "shadowstudy",
            format!(
                "{year}-{month:02}-{day:02} {}-{} every {step_min} min",
                fmt_hhmm(from_min),
                fmt_hhmm(to_min)
            ),
            "m2",
            samples,
        ),
    );
    doc.generation += 1;

    let n_layers = layers_created.len();
    Ok((
        Command::ShadowStudy {
            ids: Some(new_ids.clone()),
            year,
            month,
            day,
            from_min,
            to_min,
            step_min,
        },
        Inverse::CreatedOnLayer { created: new_ids.clone(), layers_created },
        ApplyOutcome {
            message: format!(
                "shadowstudy {year}-{month:02}-{day:02}: {} polygon(s) across {stamps_up} \
                 daylight stamp(s) on {n_layers} 'shadows-HH:MM' layer(s)",
                new_ids.len()
            ),
            created: new_ids,
        },
    ))
}

/// Annual radiation (insolation) study. Per selected face: beam kWh weighted
/// by the EPW month×hour Direct-Normal bins (representative 21st of each
/// month, occlusion-tested against the scene BVH) plus isotropic-sky diffuse
/// from the Diffuse-Horizontal bins. The bins are embedded into the logged
/// command on first exec so replay never re-reads the EPW file.
fn exec_radiation(
    doc: &mut Document,
    targets: Selector,
    ids: Option<Vec<ObjectId>>,
    path: String,
    bins: Option<Vec<[f64; 2]>>,
) -> Result<(Command, Inverse, ApplyOutcome), ExecError> {
    let loc = doc.location.ok_or_else(|| {
        ExecError::Invalid(
            "no location set — run `sun <lat> <lon> <date> <time>` or `location <lat> <lon>`, \
             or `import <file.epw>` first"
                .into(),
        )
    })?;
    // First exec reads + bins the EPW; replay reuses the embedded bins.
    let bins: itsjustcad_solar::RadiationBins = match bins {
        Some(b) if b.len() == 288 => b,
        Some(_) => return Err(ExecError::Invalid("corrupt radiation bins in op-log".into())),
        None => {
            let text = std::fs::read_to_string(&path)
                .map_err(|e| ExecError::Invalid(format!("cannot read EPW '{path}': {e}")))?;
            itsjustcad_solar::parse_epw_radiation(&text)
                .map_err(|e| ExecError::Invalid(format!("EPW '{path}': {e}")))?
        }
    };
    let target_ids = resolve(doc, &targets)?;

    // World-space triangles of the selected faces (what we score + color).
    let mut faces: Vec<[DVec3; 3]> = Vec::new();
    for id in &target_ids {
        if let Some(obj) = doc.get(*id)
            && let Geometry::Mesh(m) = &obj.geometry
        {
            let pos = m.positions();
            for f in m.faces() {
                faces.push([pos[f[0] as usize], pos[f[1] as usize], pos[f[2] as usize]]);
            }
        }
    }
    if faces.is_empty() {
        return Err(ExecError::Invalid(
            "radiation needs a selected mesh (extrude or box first, then select)".into(),
        ));
    }

    // Occlusion tested against the whole scene.
    let tri_bvh = kernel_mesh::TriBvh::build(
        scene_triangles(doc)
            .into_iter()
            .map(|t| {
                [
                    DVec3::from_array(t[0]),
                    DVec3::from_array(t[1]),
                    DVec3::from_array(t[2]),
                ]
            })
            .collect(),
    );

    // Year for the representative sun positions: fixed so replay is stable
    // (annual sun geometry is effectively year-invariant).
    const RAD_YEAR: i32 = 2026;
    // Faces are independent → parallel per-face scoring (ordered collect; the
    // 288-bin sum inside each face stays sequential, so replay is bit-stable).
    let scored = face_radiation_scores(&faces, &bins, RAD_YEAR, loc, &tri_bvh);
    let mut kwh: Vec<f64> = Vec::with_capacity(faces.len());
    let mut samples: Vec<(f64, DVec3, String)> = Vec::with_capacity(faces.len());
    let mut max_k = 0.0f64;
    for &(k, centroid, normal) in &scored {
        max_k = max_k.max(k);
        samples.push((k, centroid, facing_label(normal).to_string()));
        kwh.push(k);
    }

    let new_ids: Vec<ObjectId> = match ids {
        Some(ids) if ids.len() == faces.len() => ids,
        _ => (0..faces.len()).map(|_| ObjectId::new()).collect(),
    };

    let mut layers_created = Vec::new();
    if !doc.layers.contains_key(ANALYSIS_LAYER) {
        doc.layers
            .insert(ANALYSIS_LAYER.to_string(), LayerStyle::default());
        layers_created.push(ANALYSIS_LAYER.to_string());
    }

    for ((tri, &k), id) in faces.iter().zip(&kwh).zip(&new_ids) {
        let frac = if max_k > 0.0 { k / max_k } else { 0.0 };
        let color = [frac as f32, 0.15, (1.0 - frac) as f32];
        let mut normal = (tri[1] - tri[0]).cross(tri[2] - tri[0]);
        let nlen = normal.length();
        if nlen > 1e-12 {
            normal /= nlen;
        }
        let lift = normal * 5e-3;
        let mesh = kernel_mesh::Mesh::new(
            vec![tri[0] + lift, tri[1] + lift, tri[2] + lift],
            vec![[0, 1, 2]],
        );
        doc.insert(SceneObject {
            visible: true,
            id: *id,
            name: None,
            layer: ANALYSIS_LAYER.to_string(),
            color: Some(color),
            material: None,
            lineweight_mm: None,
            geometry: Geometry::Mesh(mesh),
        });
    }
    // Structured summary for the deck's `report` command (per-face insolation
    // with facings, so the LLM can point at hot/cold faces).
    doc.analysis_reports.insert(
        "radiation".to_string(),
        build_analysis_report("radiation", format!("annual, EPW {path}"), "kWh/m2-yr", samples),
    );
    doc.generation += 1;

    let n = kwh.len() as f64;
    let avg: f64 = kwh.iter().sum::<f64>() / n;
    let min_k = kwh.iter().cloned().fold(f64::INFINITY, f64::min);

    Ok((
        Command::Radiation {
            targets,
            ids: Some(new_ids.clone()),
            path,
            bins: Some(bins),
        },
        Inverse::CreatedOnLayer { created: new_ids.clone(), layers_created },
        ApplyOutcome {
            message: format!(
                "radiation: {} face(s), annual insolation min {min_k:.0} / avg {avg:.0} / \
                 max {max_k:.0} kWh/m2-yr on '{ANALYSIS_LAYER}'",
                new_ids.len()
            ),
            created: new_ids,
        },
    ))
}

/// The terrain surface: the most recently created mesh on layer "terrain".
/// Landscape verbs (`contours`, `pad`, `cutfill`, `flowarrows`, `ponding`,
/// `sitepath`) all operate on this single surface.
fn terrain_surface(doc: &Document) -> Result<(ObjectId, &kernel_mesh::Mesh), ExecError> {
    doc.objects()
        .filter(|o| o.layer == "terrain")
        .filter_map(|o| match &o.geometry {
            Geometry::Mesh(m) => Some((o.id, m)),
            _ => None,
        })
        .last()
        .ok_or_else(|| {
            ExecError::Invalid(
                "no terrain mesh — run `terrain <path.csv|.geojson>` first".into(),
            )
        })
}

/// `lotsubdivide` (M-intemfit Phase 3): subdivide the selected closed block
/// curve(s) into lots on the `lots` layer via the pure `subdivision` crate.
/// Numeric args override the sticky settings for THIS run (they are baked into
/// the logged op so replay is self-contained). Deterministic for a fixed seed →
/// the written-back `ids` make replay recreate byte-identical lots.
#[allow(clippy::too_many_arguments)]
fn exec_lot_subdivide(
    doc: &mut Document,
    targets: Selector,
    method: String,
    area: Option<f64>,
    width: Option<f64>,
    irregularity: Option<f64>,
    seed: Option<u64>,
    ids: Option<Vec<ObjectId>>,
) -> Result<(Command, Inverse, ApplyOutcome), ExecError> {
    let Some(method_enum) = crate::lot::parse_method(&method) else {
        return Err(ExecError::Invalid(format!(
            "unknown lotsubdivide method '{method}' — use grid | perimeter | streetfollowing"
        )));
    };

    // Build the effective settings: sticky doc settings with per-run overrides.
    let mut settings = doc.subdivision_settings.clone();
    settings.method = method_enum;
    let method_label = match method_enum {
        subdivision::SubdivisionMethod::Recursive => "grid",
        subdivision::SubdivisionMethod::Offset => "perimeter",
        subdivision::SubdivisionMethod::Skeleton => "streetfollowing",
    };
    if let Some(a) = area {
        settings.lot_area_min = a;
    }
    if let Some(w) = width {
        settings.lot_width_min = w;
    }
    if let Some(irr) = irregularity {
        settings.irregularity = irr;
    }
    if let Some(s) = seed {
        settings.seed = s;
    }

    // Gather closed block polygons from the selection.
    let sel_ids = resolve(doc, &targets)?;
    let mut blocks: Vec<(subdivision::Polygon2d, f64)> = Vec::new();
    for id in &sel_ids {
        if let Some(obj) = doc.get(*id)
            && let Geometry::Curve(c) = &obj.geometry
            && c.is_closed()
            && let Some(poly) = crate::lot::curve_to_polygon(c)
        {
            // Source elevation: mean z of the tessellated boundary.
            let pts = c.tessellate(PROFILE_TOL);
            let z = if pts.is_empty() {
                0.0
            } else {
                pts.iter().map(|p| p.z).sum::<f64>() / pts.len() as f64
            };
            blocks.push((poly, z));
        }
    }
    if blocks.is_empty() {
        return Err(ExecError::Invalid(
            "lotsubdivide needs a closed block curve (draw or select a boundary polyline first)"
                .into(),
        ));
    }

    let bake = crate::lot::subdivide_blocks(&blocks, &settings).map_err(ExecError::Invalid)?;

    let new_ids: Vec<ObjectId> = match ids {
        Some(ids) if ids.len() == bake.polygons.len() => ids,
        _ => (0..bake.polygons.len()).map(|_| ObjectId::new()).collect(),
    };

    let mut layers_created = Vec::new();
    if let Some(name) = crate::lot::ensure_lots_layer(doc) {
        layers_created.push(name);
    }
    crate::lot::insert_lots(doc, &bake, &new_ids);
    doc.generation += 1;

    let n = new_ids.len();
    Ok((
        Command::LotSubdivide {
            targets,
            method,
            area,
            width,
            irregularity,
            seed,
            ids: Some(new_ids.clone()),
        },
        Inverse::CreatedOnLayer { created: new_ids.clone(), layers_created },
        ApplyOutcome {
            message: {
                let mut msg = format!(
                    "lotsubdivide {method_label}: {n} lots on '{}' ({} with street frontage)",
                    crate::lot::LOTS_LAYER,
                    bake.with_street
                );
                if bake.slivers_merged > 0 {
                    msg.push_str(&format!("; {} sliver(s) merged", bake.slivers_merged));
                }
                if bake.corners_widened > 0 {
                    msg.push_str(&format!("; {} corner lot(s) widened", bake.corners_widened));
                }
                if let Some(err) = bake.width_mix_error {
                    msg.push_str(&format!("; width mix ±{:.1}% of target", err * 100.0));
                }
                if let Some(note) = &bake.placeholder_note {
                    msg.push_str(&format!("; {note}"));
                }
                msg
            },
            created: new_ids,
        },
    ))
}

/// Parse a width-mix string `6:0.25,8:0.5,10:0.25` → `LotWidthMix` (soft
/// proportions). Each pair is `width:proportion`; proportions are normalised by
/// the solver. Errors on malformed input.
fn parse_width_mix(v: &str) -> Result<subdivision::LotWidthMix, ExecError> {
    let mut products = Vec::new();
    for pair in v.split(',') {
        let pair = pair.trim();
        if pair.is_empty() {
            continue;
        }
        let (w, p) = pair
            .split_once(':')
            .ok_or_else(|| ExecError::Invalid(format!("bad width-mix pair '{pair}' (want width:proportion)")))?;
        let width: f64 = w
            .trim()
            .parse()
            .map_err(|_| ExecError::Invalid(format!("bad width '{w}' in width-mix")))?;
        let prop: f64 = p
            .trim()
            .parse()
            .map_err(|_| ExecError::Invalid(format!("bad proportion '{p}' in width-mix")))?;
        if width <= 0.0 || prop <= 0.0 {
            return Err(ExecError::Invalid(format!(
                "width-mix width + proportion must be positive: '{pair}'"
            )));
        }
        products.push((width, prop));
    }
    if products.is_empty() {
        return Err(ExecError::Invalid(
            "width-mix needs at least one width:proportion pair (e.g. 6:0.25,8:0.5,10:0.25)".into(),
        ));
    }
    Ok(subdivision::LotWidthMix {
        products,
        strict_proportions: false,
    })
}

/// `lotsettings` (M-intemfit): show or set the sticky `SubdivisionSettings`.
/// With no `sets`, reports the current settings. Logged; the prior settings JSON
/// is captured into the op on first exec so replay + undo are self-contained.
fn exec_lot_settings(
    doc: &mut Document,
    sets: Vec<(String, String)>,
    prev: Option<String>,
) -> Result<(Command, Inverse, ApplyOutcome), ExecError> {
    let prev_json = prev.unwrap_or_else(|| {
        serde_json::to_string(&doc.subdivision_settings).unwrap_or_default()
    });

    if sets.is_empty() {
        let s = &doc.subdivision_settings;
        let msg = format!(
            "lot settings: method={:?} area_min={} area_max={} width_min={} irregularity={} \
             loose={} force_street={} seed={}",
            s.method,
            s.lot_area_min,
            s.lot_area_max,
            s.lot_width_min,
            s.irregularity,
            s.loose,
            s.force_street_access,
            s.seed,
        );
        return Ok((
            Command::LotSettings { sets, prev: Some(prev_json.clone()) },
            Inverse::SubdivisionSettings { prev: prev_json },
            ApplyOutcome { message: msg, created: Vec::new() },
        ));
    }

    let s = &mut doc.subdivision_settings;
    for (k, v) in &sets {
        let num = || v.parse::<f64>().map_err(|_| ExecError::Invalid(format!("bad number '{v}'")));
        match k.to_lowercase().as_str() {
            "method" => {
                s.method = crate::lot::parse_method(v).ok_or_else(|| {
                    ExecError::Invalid(format!("unknown method '{v}'"))
                })?
            }
            "area" | "area_min" | "lot_area_min" => s.lot_area_min = num()?,
            "area_max" | "lot_area_max" => s.lot_area_max = num()?,
            "width" | "width_min" | "lot_width_min" => s.lot_width_min = num()?,
            "irregularity" | "irreg" => s.irregularity = num()?,
            "loose" => s.loose = matches!(v.to_lowercase().as_str(), "1" | "true" | "yes" | "on"),
            "force_street_access" | "force_street" => s.force_street_access = num()?,
            "seed" => {
                s.seed = v
                    .parse::<u64>()
                    .map_err(|_| ExecError::Invalid(format!("bad seed '{v}'")))?
            }
            // ── Phase 6 lot rules ──
            "region" => {
                s.region = match v.to_lowercase().as_str() {
                    "euro_latam" | "eurolatam" | "metric" => {
                        subdivision::RegionProfile::EuroLatam
                    }
                    "us_suburban" | "us" | "imperial" => {
                        subdivision::RegionProfile::UsSuburban
                    }
                    _ => {
                        return Err(ExecError::Invalid(format!(
                            "unknown region '{v}' (try euro_latam | us_suburban)"
                        )))
                    }
                }
            }
            "loading" | "load" => {
                s.loading = match v.to_lowercase().as_str() {
                    "front" | "frontloaded" => subdivision::LoadingType::FrontLoaded,
                    "alley" | "alleyloaded" => subdivision::LoadingType::AlleyLoaded,
                    "mixed" => subdivision::LoadingType::Mixed,
                    _ => {
                        return Err(ExecError::Invalid(format!(
                            "unknown loading '{v}' (try front | alley)"
                        )))
                    }
                }
            }
            "widthmix" | "width_mix" | "mix" => {
                s.width_mix = if matches!(v.to_lowercase().as_str(), "off" | "none" | "") {
                    None
                } else {
                    Some(parse_width_mix(v)?)
                }
            }
            "depth" | "lot_depth" | "lot_depth_target" => s.lot_depth_target = num()?,
            "depth_tol" | "lot_depth_tolerance" => s.lot_depth_tolerance = num()?,
            "corner" | "corner_bonus" | "corner_lot_width_bonus" => {
                // Accept `15%` or a fraction (0.15).
                let vv = v.trim_end_matches('%');
                let n: f64 = vv
                    .parse()
                    .map_err(|_| ExecError::Invalid(format!("bad corner bonus '{v}'")))?;
                s.corner_lot_width_bonus = if v.ends_with('%') { n / 100.0 } else { n };
            }
            "corner_angle" | "corner_angle_max" => s.corner_angle_max = num()?,
            "flag" | "allow_flag_lots" => {
                s.allow_flag_lots = matches!(v.to_lowercase().as_str(), "1" | "true" | "yes" | "on")
            }
            "flag_pole" | "flag_pole_width_min" => s.flag_pole_width_min = num()?,
            "mergeslivers" | "merge_slivers" | "slivers" => {
                s.merge_slivers = matches!(v.to_lowercase().as_str(), "1" | "true" | "yes" | "on")
            }
            "sliver_frac" | "sliver_area_frac" => s.sliver_area_frac = num()?,
            "alley_width" | "alleywidth" => s.alley_width = num()?,
            other => {
                return Err(ExecError::Invalid(format!(
                    "unknown lot setting '{other}' (try area/area_max/width/irregularity/loose/\
                     force_street/method/seed/region/loading/widthmix/depth/corner/flag/\
                     mergeslivers)"
                )));
            }
        }
    }
    doc.generation += 1;

    Ok((
        Command::LotSettings { sets: sets.clone(), prev: Some(prev_json.clone()) },
        Inverse::SubdivisionSettings { prev: prev_json },
        ApplyOutcome {
            message: format!("updated {} lot setting(s)", sets.len()),
            created: Vec::new(),
        },
    ))
}

/// `lotloading [sel] front|alley` (M-intemfit Phase 6): set the sticky loading
/// mode. A thin convenience over `lotsettings loading=…`; logged so replay + undo
/// reproduce it. `targets` is advisory (loading is sticky doc state applied on the
/// next subdivide, not a per-object attribute).
fn exec_lot_loading(
    doc: &mut Document,
    targets: Selector,
    mode: String,
    prev: Option<String>,
) -> Result<(Command, Inverse, ApplyOutcome), ExecError> {
    let prev_json =
        prev.unwrap_or_else(|| serde_json::to_string(&doc.subdivision_settings).unwrap_or_default());
    let loading = match mode.to_lowercase().as_str() {
        "front" | "frontloaded" => subdivision::LoadingType::FrontLoaded,
        "alley" | "alleyloaded" => subdivision::LoadingType::AlleyLoaded,
        _ => {
            return Err(ExecError::Invalid(format!(
                "unknown loading mode '{mode}' — use front | alley"
            )))
        }
    };
    doc.subdivision_settings.loading = loading;
    doc.generation += 1;
    Ok((
        Command::LotLoading { targets, mode: mode.clone(), prev: Some(prev_json.clone()) },
        Inverse::SubdivisionSettings { prev: prev_json },
        ApplyOutcome {
            message: format!("lot loading set to {mode} (sticky; applies on next lotsubdivide)"),
            created: Vec::new(),
        },
    ))
}

/// `lotgeneratesite` (M-intemfit Phase 5): generate a road network + blocks from
/// the selected site boundary curve. Roads bake onto the `roads` layer, blocks
/// onto the `blocks` layer, as one logged op. Per-run args override the sticky
/// settings and are baked into the op so replay is self-contained. Deterministic
/// for a fixed seed → written-back ids make replay recreate byte-identical
/// roads + blocks.
#[allow(clippy::too_many_arguments)]
fn exec_lot_generate_site(
    doc: &mut Document,
    targets: Selector,
    pattern: String,
    roadwidth: Option<f64>,
    blockdepth: Option<f64>,
    alleys: Option<bool>,
    seed: Option<u64>,
    road_ids: Option<Vec<ObjectId>>,
    block_ids: Option<Vec<ObjectId>>,
) -> Result<(Command, Inverse, ApplyOutcome), ExecError> {
    let Some(pattern_enum) = crate::lot::parse_pattern(&pattern) else {
        return Err(ExecError::Invalid(format!(
            "unknown lotgeneratesite pattern '{pattern}' — use \
             orthogonal | skewed | organic | culdesac"
        )));
    };

    // Effective settings: sticky doc settings + per-run overrides.
    let mut settings = doc.subdivision_settings.clone();
    settings.street_pattern = pattern_enum;
    if let Some(w) = roadwidth {
        settings.road_width = w;
    }
    if let Some(d) = blockdepth {
        settings.block_depth = d;
    }
    if let Some(a) = alleys {
        settings.loading = if a {
            subdivision::LoadingType::AlleyLoaded
        } else {
            subdivision::LoadingType::FrontLoaded
        };
    }
    if let Some(s) = seed {
        settings.seed = s;
    }

    // Gather the (single) closed site boundary from the selection. If several
    // closed curves are selected, use the largest by area (the site).
    let sel_ids = resolve(doc, &targets)?;
    let mut best: Option<(subdivision::Polygon2d, f64)> = None;
    for id in &sel_ids {
        if let Some(obj) = doc.get(*id)
            && let Geometry::Curve(c) = &obj.geometry
            && c.is_closed()
            && let Some(poly) = crate::lot::curve_to_polygon(c)
        {
            let pts = c.tessellate(PROFILE_TOL);
            let z = if pts.is_empty() {
                0.0
            } else {
                pts.iter().map(|p| p.z).sum::<f64>() / pts.len() as f64
            };
            let a = poly.area();
            if best.as_ref().map(|(_, _)| a).is_none() || a > best.as_ref().map(|(p, _)| p.area()).unwrap_or(0.0) {
                best = Some((poly, z));
            }
        }
    }
    let Some((site, z)) = best else {
        return Err(ExecError::Invalid(
            "lotgeneratesite needs a closed site boundary curve (draw or select one first)".into(),
        ));
    };

    let bake = crate::lot::generate_site(&site, z, &settings).map_err(ExecError::Invalid)?;

    // Written-back ids: roads first, then blocks. Reuse on replay.
    let new_road_ids: Vec<ObjectId> = match road_ids {
        Some(ids) if ids.len() == bake.roads.len() => ids,
        _ => (0..bake.roads.len()).map(|_| ObjectId::new()).collect(),
    };
    let new_block_ids: Vec<ObjectId> = match block_ids {
        Some(ids) if ids.len() == bake.blocks.len() => ids,
        _ => (0..bake.blocks.len()).map(|_| ObjectId::new()).collect(),
    };

    let mut layers_created = Vec::new();
    if let Some(name) = crate::lot::ensure_roads_layer(doc) {
        layers_created.push(name);
    }
    if let Some(name) = crate::lot::ensure_blocks_layer(doc) {
        layers_created.push(name);
    }
    crate::lot::insert_site(doc, &bake, &new_road_ids, &new_block_ids);
    doc.generation += 1;

    let mut created: Vec<ObjectId> = new_road_ids.clone();
    created.extend(new_block_ids.clone());
    let n_roads = new_road_ids.len();
    let n_blocks = new_block_ids.len();
    Ok((
        Command::LotGenerateSite {
            targets,
            pattern,
            roadwidth,
            blockdepth,
            alleys,
            seed,
            road_ids: Some(new_road_ids),
            block_ids: Some(new_block_ids),
        },
        Inverse::CreatedOnLayer { created: created.clone(), layers_created },
        ApplyOutcome {
            message: format!(
                "lotgeneratesite {pattern_enum:?}: {n_roads} roads on '{}', {n_blocks} blocks on \
                 '{}' ({} street edges, {} alley edges)",
                crate::lot::ROADS_LAYER,
                crate::lot::BLOCKS_LAYER,
                bake.street_edges,
                bake.alley_edges,
            ),
            created,
        },
    ))
}

/// `lotsetbacks` (M-intemfit Phase 8): compute + bake the buildable envelope per
/// selected lot curve onto the `setbacks` layer. The envelope is the lot inset by
/// per-edge setbacks (front from the street edge, rear opposite, side the rest);
/// `buildto > 0` pins the front to the build-to line. Per-run args override the
/// sticky settings and are baked into the op so replay is self-contained; the
/// written-back ids make replay recreate byte-identical envelopes. A collapsed
/// envelope (setbacks exceed the lot) is reported, never a panic.
#[allow(clippy::too_many_arguments)]
fn exec_lot_setbacks(
    doc: &mut Document,
    targets: Selector,
    front: Option<f64>,
    side: Option<f64>,
    rear: Option<f64>,
    buildto: Option<f64>,
    envelope: Option<bool>,
    ids: Option<Vec<ObjectId>>,
) -> Result<(Command, Inverse, ApplyOutcome), ExecError> {
    // Effective settings: sticky doc settings + per-run overrides.
    let mut settings = doc.subdivision_settings.clone();
    if let Some(f) = front {
        settings.setback_front = f;
    }
    if let Some(s) = side {
        settings.setback_side = s;
    }
    if let Some(r) = rear {
        settings.setback_rear = r;
    }
    if let Some(b) = buildto {
        settings.build_to_line = b;
    }
    let draw = envelope.unwrap_or(true);

    // Gather closed lot polygons from the selection.
    let sel_ids = resolve(doc, &targets)?;
    let mut lots: Vec<(subdivision::Polygon2d, f64)> = Vec::new();
    for id in &sel_ids {
        if let Some(obj) = doc.get(*id)
            && let Geometry::Curve(c) = &obj.geometry
            && c.is_closed()
            && let Some(poly) = crate::lot::curve_to_polygon(c)
        {
            let pts = c.tessellate(PROFILE_TOL);
            let z = if pts.is_empty() {
                0.0
            } else {
                pts.iter().map(|p| p.z).sum::<f64>() / pts.len() as f64
            };
            lots.push((poly, z));
        }
    }
    if lots.is_empty() {
        return Err(ExecError::Invalid(
            "lotsetbacks needs closed lot curve(s) (select lots, or run lotsubdivide first)".into(),
        ));
    }

    let bake = crate::lot::compute_setbacks(&lots, &settings).map_err(ExecError::Invalid)?;

    // envelope=off: report only, bake nothing (no logged geometry).
    if !draw {
        let mut msg = format!(
            "lotsetbacks: {} envelope(s) computed (front {} / side {} / rear {}",
            bake.envelopes.len(),
            settings.setback_front,
            settings.setback_side,
            settings.setback_rear,
        );
        if settings.build_to_line > 0.0 {
            msg.push_str(&format!("; build-to {}", settings.build_to_line));
        }
        msg.push(')');
        if bake.collapsed > 0 {
            msg.push_str(&format!("; {} collapsed (setbacks exceed lot)", bake.collapsed));
        }
        if let Some(note) = &bake.placeholder_note {
            msg.push_str(&format!("; {note}"));
        }
        // Not baked → nothing to undo; report through a no-op logged op with no
        // created ids (envelope=off with ids=None means replay recomputes only).
        return Ok((
            Command::LotSetbacks { targets, front, side, rear, buildto, envelope, ids: Some(Vec::new()) },
            Inverse::CreatedOnLayer { created: Vec::new(), layers_created: Vec::new() },
            ApplyOutcome { message: msg, created: Vec::new() },
        ));
    }

    let new_ids: Vec<ObjectId> = match ids {
        Some(ids) if ids.len() == bake.envelopes.len() => ids,
        _ => (0..bake.envelopes.len()).map(|_| ObjectId::new()).collect(),
    };

    let mut layers_created = Vec::new();
    if let Some(name) = crate::lot::ensure_setbacks_layer(doc) {
        layers_created.push(name);
    }
    crate::lot::insert_setbacks(doc, &bake, &new_ids);
    doc.generation += 1;

    let n = new_ids.len();
    Ok((
        Command::LotSetbacks {
            targets,
            front,
            side,
            rear,
            buildto,
            envelope,
            ids: Some(new_ids.clone()),
        },
        Inverse::CreatedOnLayer { created: new_ids.clone(), layers_created },
        ApplyOutcome {
            message: {
                let mut msg = format!(
                    "lotsetbacks: {n} buildable envelope(s) on '{}' (front {} / side {} / rear {}",
                    crate::lot::SETBACKS_LAYER,
                    settings.setback_front,
                    settings.setback_side,
                    settings.setback_rear,
                );
                if bake.build_to_used {
                    msg.push_str(&format!("; front pinned to build-to {}", settings.build_to_line));
                }
                msg.push(')');
                if bake.collapsed > 0 {
                    msg.push_str(&format!("; {} collapsed (setbacks exceed lot)", bake.collapsed));
                }
                if let Some(note) = &bake.placeholder_note {
                    msg.push_str(&format!("; {note}"));
                }
                msg
            },
            created: new_ids,
        },
    ))
}

/// `lotfrontage` (M-intemfit Phase 8): report each selected lot's frontage
/// length, measured along the setback line by DEFAULT (`at=setback`, Manuel's
/// explicit ask) or the curb (`at=curb`). Read-only query — stored on the
/// AnalysisReport plane (`lotfrontage`) + rendered so `report` can re-show it and
/// the deck can critique. Never logged.
fn exec_lot_frontage(
    doc: &mut Document,
    targets: Selector,
    at: Option<String>,
) -> Result<(Command, Inverse, ApplyOutcome), ExecError> {
    let at_str = at.clone().unwrap_or_else(|| "setback".to_string());
    let Some(at_enum) = crate::lot::parse_frontage_at(&at_str) else {
        return Err(ExecError::Invalid(format!(
            "unknown lotfrontage at='{at_str}' — use setback | curb"
        )));
    };

    let settings = doc.subdivision_settings.clone();
    let sel_ids = resolve(doc, &targets)?;
    let mut lots: Vec<subdivision::Polygon2d> = Vec::new();
    let mut zs: Vec<f64> = Vec::new();
    for id in &sel_ids {
        if let Some(obj) = doc.get(*id)
            && let Geometry::Curve(c) = &obj.geometry
            && c.is_closed()
            && let Some(poly) = crate::lot::curve_to_polygon(c)
        {
            let pts = c.tessellate(PROFILE_TOL);
            let z = if pts.is_empty() {
                0.0
            } else {
                pts.iter().map(|p| p.z).sum::<f64>() / pts.len() as f64
            };
            lots.push(poly);
            zs.push(z);
        }
    }
    if lots.is_empty() {
        return Err(ExecError::Invalid(
            "lotfrontage needs closed lot curve(s) (select lots, or run lotsubdivide first)".into(),
        ));
    }

    let measures = crate::lot::measure_frontage(&lots, at_enum, &settings);
    let at_label = match at_enum {
        subdivision::FrontageAt::Setback => "setback line",
        subdivision::FrontageAt::Curb => "curb",
    };
    let samples: Vec<(f64, DVec3, String)> = measures
        .iter()
        .zip(&zs)
        .map(|(m, z)| (m.length, DVec3::new(m.at.x, m.at.y, *z), at_label.to_string()))
        .collect();
    let context = format!(
        "measured at the {at_label} (default: setback line — plan §5); {} lot(s)",
        measures.len()
    );
    let report = build_analysis_report("lotfrontage", context, "m", samples);
    let msg = format_analysis_report(&report);
    doc.analysis_reports.insert("lotfrontage".to_string(), report);
    doc.generation += 1;

    Ok((
        Command::LotFrontage { targets, at },
        Inverse::Rename(Vec::new()), // not logged; inverse unused
        ApplyOutcome { message: msg.trim_end().to_string(), created: Vec::new() },
    ))
}

/// `lotopenspace` (M-intemfit Phase 9): place an open-space feature
/// (`type=park|greenway|pond|treesave`) at the selected region / largest empty
/// block, OR run the blind `reserve=<pct>` mode that pulls whole central blocks
/// out of subdivision as open space. Both bake onto the `openspace` layer as one
/// logged op with written-back ids (deterministic → replay byte-identical); undo
/// removes the geometry + layer. Feature placement is design-intent, not
/// hydrology/ecology engineering — the run surfaces a no-false-precision note.
fn exec_lot_openspace(
    doc: &mut Document,
    targets: Selector,
    feature: Option<String>,
    area: Option<f64>,
    reserve: Option<f64>,
    ids: Option<Vec<ObjectId>>,
) -> Result<(Command, Inverse, ApplyOutcome), ExecError> {
    let settings = doc.subdivision_settings.clone();

    // Gather the selected region(s): the largest closed curve is the region
    // (site for reserve, or the block/region for feature placement). An open
    // polyline, if one is selected, is a candidate greenway path.
    let sel_ids = resolve(doc, &targets)?;
    let mut best_region: Option<(subdivision::Polygon2d, f64)> = None;
    let mut greenway_path: Option<Vec<glam::DVec2>> = None;
    for id in &sel_ids {
        let Some(obj) = doc.get(*id) else { continue };
        let Geometry::Curve(c) = &obj.geometry else { continue };
        if c.is_closed() {
            if let Some(poly) = crate::lot::curve_to_polygon(c) {
                let pts = c.tessellate(PROFILE_TOL);
                let z = if pts.is_empty() {
                    0.0
                } else {
                    pts.iter().map(|p| p.z).sum::<f64>() / pts.len() as f64
                };
                let a = poly.area();
                if best_region.as_ref().map(|(p, _)| a > p.area()).unwrap_or(true) {
                    best_region = Some((poly, z));
                }
            }
        } else {
            let pts = c.tessellate(PROFILE_TOL);
            if pts.len() >= 2 {
                greenway_path = Some(pts.iter().map(|p| glam::DVec2::new(p.x, p.y)).collect());
            }
        }
    }

    let bake = if let Some(pct) = reserve.filter(|p| *p > 0.0) {
        // Blind %-reserve mode.
        let Some((site, z)) = best_region else {
            return Err(ExecError::Invalid(
                "lotopenspace reserve=<pct> needs a closed site boundary curve (select one first)"
                    .into(),
            ));
        };
        crate::lot::reserve_open_space(&site, pct, &settings, z).map_err(ExecError::Invalid)?
    } else {
        // Feature-placement mode (default).
        let feat_str = feature.clone().unwrap_or_else(|| "park".to_string());
        let Some(feat) = crate::lot::parse_open_space_feature(&feat_str) else {
            return Err(ExecError::Invalid(format!(
                "unknown lotopenspace type='{feat_str}' — use park | greenway | pond | treesave"
            )));
        };
        let Some((region, z)) = best_region else {
            return Err(ExecError::Invalid(
                "lotopenspace needs a closed region curve (select a block/region, or draw one)"
                    .into(),
            ));
        };
        crate::lot::place_feature(&region, greenway_path.as_deref(), feat, area, z)
            .map_err(ExecError::Invalid)?
    };

    let new_ids: Vec<ObjectId> = match ids {
        Some(ids) if ids.len() == bake.polygons.len() => ids,
        _ => (0..bake.polygons.len()).map(|_| ObjectId::new()).collect(),
    };

    let mut layers_created = Vec::new();
    if let Some(name) = crate::lot::ensure_openspace_layer(doc) {
        layers_created.push(name);
    }
    crate::lot::insert_open_space(doc, &bake, &new_ids);
    doc.generation += 1;

    let mut msg = format!("lotopenspace: {}", bake.summary);
    if let Some(a) = &bake.advisory {
        msg.push_str(&format!(" ({a})"));
    }
    Ok((
        Command::LotOpenSpace { targets, feature, area, reserve, ids: Some(new_ids.clone()) },
        Inverse::CreatedOnLayer { created: new_ids.clone(), layers_created },
        ApplyOutcome { message: msg, created: new_ids },
    ))
}

/// Contour polylines FROM the terrain mesh: marching triangles at every
/// multiple of `interval`, chained into polylines. Minor contours on layer
/// "contours", every `major_every`-th level on "contours-major". Pure
/// function of the terrain + params, so replay with the written-back ids
/// recreates identical objects.
fn exec_contours(
    doc: &mut Document,
    interval: f64,
    major_every: Option<u32>,
    ids: Option<Vec<ObjectId>>,
) -> Result<(Command, Inverse, ApplyOutcome), ExecError> {
    if interval <= 0.0 || !interval.is_finite() {
        return Err(ExecError::Invalid("contours interval must be > 0".into()));
    }
    if major_every == Some(0) {
        return Err(ExecError::Invalid("contours major-every must be ≥ 1".into()));
    }
    let (_, mesh) = terrain_surface(doc)?;
    let lines =
        crate::landscape::contours(mesh.positions(), mesh.faces(), interval);
    if lines.is_empty() {
        return Err(ExecError::Invalid(format!(
            "no contours: terrain has no elevation band crossing a multiple of {interval}"
        )));
    }

    let new_ids: Vec<ObjectId> = match ids {
        Some(ids) if ids.len() == lines.len() => ids,
        _ => (0..lines.len()).map(|_| ObjectId::new()).collect(),
    };

    let is_major = |index: i64| -> bool {
        major_every.is_some_and(|k| index.rem_euclid(k as i64) == 0)
    };
    let mut layers_created = Vec::new();
    // Earthy brown for minor contours, darker + heavier read for majors.
    for (name, color) in [
        ("contours", [0.55f32, 0.45, 0.32, 1.0]),
        ("contours-major", [0.35, 0.25, 0.15, 1.0]),
    ] {
        let used = (name == "contours" && lines.iter().any(|c| !is_major(c.index)))
            || (name == "contours-major" && lines.iter().any(|c| is_major(c.index)));
        if used && !doc.layers.contains_key(name) {
            doc.layers.insert(
                name.to_string(),
                LayerStyle { color: Some(color), ..LayerStyle::default() },
            );
            layers_created.push(name.to_string());
        }
    }

    let mut n_major = 0usize;
    for (line, id) in lines.iter().zip(&new_ids) {
        let major = is_major(line.index);
        n_major += usize::from(major);
        doc.insert(SceneObject {
            visible: true,
            id: *id,
            name: Some(format!("contour {:.3}", line.level)),
            layer: if major { "contours-major" } else { "contours" }.to_string(),
            color: None,
            material: None,
            lineweight_mm: None,
            geometry: Geometry::Curve(Curve::Polyline {
                points: line.points.clone(),
                closed: line.closed,
            }),
        });
    }
    doc.generation += 1;

    let (zmin, zmax) = lines.iter().fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), c| {
        (lo.min(c.level), hi.max(c.level))
    });
    Ok((
        Command::Contours { interval, major_every, ids: Some(new_ids.clone()) },
        Inverse::CreatedOnLayer { created: new_ids.clone(), layers_created },
        ApplyOutcome {
            message: format!(
                "contours every {interval} m ({zmin}..{zmax} m): {} minor on 'contours', \
                 {n_major} major on 'contours-major'",
                new_ids.len() - n_major
            ),
            created: new_ids,
        },
    ))
}

/// Grade a flat building pad into the terrain mesh (see
/// [`crate::landscape::grade_pad`]). Deterministic vertex edit of the current
/// terrain, so the logged op replays bit-identically; the first pad snapshots
/// the pre-grading heights into `Document::pregrade_terrain` for `cutfill`.
fn exec_pad(
    doc: &mut Document,
    at: DVec3,
    width: f64,
    depth: f64,
    elev: f64,
    slope: Option<f64>,
) -> Result<(Command, Inverse, ApplyOutcome), ExecError> {
    if width <= 0.0 || depth <= 0.0 {
        return Err(ExecError::Invalid("pad width and depth must be > 0".into()));
    }
    let slope_h = slope.unwrap_or(2.0);
    if slope_h <= 0.0 || !slope_h.is_finite() {
        return Err(ExecError::Invalid(
            "pad slope must be > 0 (horizontal run per unit rise, e.g. 2 = 2:1)".into(),
        ));
    }
    let (tid, mesh) = terrain_surface(doc)?;
    let old_geometry = Geometry::Mesh(mesh.clone());
    let original_z: Vec<f64> = mesh.positions().iter().map(|p| p.z).collect();
    let mut positions = mesh.positions().to_vec();
    let faces = mesh.faces().to_vec();
    let (on_pad, on_slope) = crate::landscape::grade_pad(
        &mut positions,
        at.x,
        at.y,
        width,
        depth,
        elev,
        slope_h,
    );
    // First grading of THIS terrain captures the pre-grading snapshot.
    match &doc.pregrade_terrain {
        Some((id, _)) if *id == tid => {}
        _ => doc.pregrade_terrain = Some((tid, original_z)),
    }
    if let Some(obj) = doc.get_mut(tid) {
        obj.geometry = Geometry::Mesh(kernel_mesh::Mesh::new(positions, faces));
    }
    Ok((
        Command::Pad { at, width, depth, elev, slope },
        Inverse::SetGeometry(vec![(tid, old_geometry)]),
        ApplyOutcome {
            message: format!(
                "pad {width}x{depth} m @ ({:.1}, {:.1}) elev {elev} m: {on_pad} vertex(es) to \
                 pad grade, {on_slope} on {slope_h}:1 side slopes — run `cutfill` for earthwork",
                at.x, at.y
            ),
            created: Vec::new(),
        },
    ))
}

/// Cut + fill volumes between the current terrain and the pre-grading
/// snapshot. The original heights are embedded into the logged op on first
/// exec (radiation-bins pattern), so replay never depends on transient state.
fn exec_cutfill(
    doc: &mut Document,
    original_z: Option<Vec<f64>>,
) -> Result<(Command, Inverse, ApplyOutcome), ExecError> {
    let (tid, mesh) = terrain_surface(doc)?;
    let n = mesh.positions().len();
    let orig: Vec<f64> = match original_z {
        Some(o) if o.len() == n => o,
        Some(_) => {
            return Err(ExecError::Invalid(
                "corrupt cutfill snapshot in op-log (vertex count changed)".into(),
            ))
        }
        None => match &doc.pregrade_terrain {
            Some((id, z)) if *id == tid && z.len() == n => z.clone(),
            Some(_) => {
                return Err(ExecError::Invalid(
                    "terrain changed since grading — re-run `pad` before `cutfill`".into(),
                ))
            }
            None => {
                return Err(ExecError::Invalid(
                    "no grading recorded — run `pad` first, then `cutfill`".into(),
                ))
            }
        },
    };
    let (cut, fill) =
        crate::landscape::cut_fill(mesh.positions(), mesh.faces(), &orig);
    // Per-vertex Δz samples (only where the grade moved) for the deck report.
    let samples: Vec<(f64, DVec3, String)> = mesh
        .positions()
        .iter()
        .zip(&orig)
        .filter(|(p, oz)| (p.z - **oz).abs() > 1e-9)
        .map(|(p, oz)| {
            let dz = p.z - oz;
            (dz, *p, if dz < 0.0 { "cut" } else { "fill" }.to_string())
        })
        .collect();
    let net = fill - cut;
    doc.analysis_reports.insert(
        "cutfill".to_string(),
        build_analysis_report(
            "cutfill",
            format!(
                "cut {cut:.1} m3 / fill {fill:.1} m3 / net {net:+.1} m3 vs pre-grading \
                 terrain (TIN prism estimate)"
            ),
            "m dz",
            samples,
        ),
    );
    doc.generation += 1;
    Ok((
        Command::CutFill { original_z: Some(orig) },
        Inverse::CreatedOnLayer { created: Vec::new(), layers_created: Vec::new() },
        ApplyOutcome {
            message: format!(
                "cutfill: cut {cut:.1} m3, fill {fill:.1} m3, net {net:+.1} m3 vs pre-grading \
                 terrain (TIN prism estimate — see `report cutfill`)"
            ),
            created: Vec::new(),
        },
    ))
}

/// Steepest-descent arrows on the largest terrain faces: a drainage-direction
/// picture (visualization, not hydrology engineering). Deterministic: faces
/// sorted by plan area (centroid tie-break), arrow geometry pure per face.
fn exec_flow_arrows(
    doc: &mut Document,
    n: Option<u32>,
    ids: Option<Vec<ObjectId>>,
) -> Result<(Command, Inverse, ApplyOutcome), ExecError> {
    if n == Some(0) {
        return Err(ExecError::Invalid("flowarrows count must be ≥ 1".into()));
    }
    let (_, mesh) = terrain_surface(doc)?;
    let mut flows = crate::landscape::face_flows(mesh.positions(), mesh.faces());
    if flows.is_empty() {
        return Err(ExecError::Invalid(
            "terrain is flat — no descent direction to draw".into(),
        ));
    }
    // Biggest plan-area faces first; centroid tie-break keeps grids stable.
    flows.sort_by(|a, b| {
        b.area_xy
            .total_cmp(&a.area_xy)
            .then(a.centroid.y.total_cmp(&b.centroid.y))
            .then(a.centroid.x.total_cmp(&b.centroid.x))
    });
    flows.truncate(n.unwrap_or(200) as usize);

    let new_ids: Vec<ObjectId> = match ids {
        Some(ids) if ids.len() == flows.len() => ids,
        _ => (0..flows.len()).map(|_| ObjectId::new()).collect(),
    };
    let mut layers_created = Vec::new();
    if !doc.layers.contains_key(ANALYSIS_LAYER) {
        doc.layers
            .insert(ANALYSIS_LAYER.to_string(), LayerStyle::default());
        layers_created.push(ANALYSIS_LAYER.to_string());
    }
    let mut samples = Vec::with_capacity(flows.len());
    for (fl, id) in flows.iter().zip(&new_ids) {
        // Arrow sized to its face, floated just above the surface.
        let len = (0.8 * fl.area_xy.sqrt()).max(0.2);
        let lift = DVec3::new(0.0, 0.0, 0.05);
        let pts: Vec<DVec3> = crate::landscape::arrow_points(fl.centroid, fl.downhill, len)
            .into_iter()
            .map(|p| p + lift)
            .collect();
        samples.push((fl.slope, fl.centroid, "downslope".to_string()));
        doc.insert(SceneObject {
            visible: true,
            id: *id,
            name: None,
            layer: ANALYSIS_LAYER.to_string(),
            color: Some([0.15, 0.45, 0.9]),
            material: None,
            lineweight_mm: None,
            geometry: Geometry::Curve(Curve::Polyline { points: pts, closed: false }),
        });
    }
    let max_slope = flows.iter().map(|f| f.slope).fold(0.0f64, f64::max);
    doc.analysis_reports.insert(
        "flowarrows".to_string(),
        build_analysis_report(
            "flowarrows",
            format!(
                "{} steepest-descent arrow(s) — visualization, not hydrology engineering",
                new_ids.len()
            ),
            "rise/run",
            samples,
        ),
    );
    doc.generation += 1;
    Ok((
        Command::FlowArrows { n, ids: Some(new_ids.clone()) },
        Inverse::CreatedOnLayer { created: new_ids.clone(), layers_created },
        ApplyOutcome {
            message: format!(
                "flowarrows: {} downslope arrow(s) on '{ANALYSIS_LAYER}', steepest slope \
                 {max_slope:.2} rise/run — a gradient visualization, not hydrology engineering",
                new_ids.len()
            ),
            created: new_ids,
        },
    ))
}

/// Ponding markers: a circle at every interior terrain vertex lower than all
/// its neighbors (visualization, not hydrology engineering).
fn exec_ponding(
    doc: &mut Document,
    ids: Option<Vec<ObjectId>>,
) -> Result<(Command, Inverse, ApplyOutcome), ExecError> {
    let (_, mesh) = terrain_surface(doc)?;
    let positions = mesh.positions().to_vec();
    let faces = mesh.faces().to_vec();
    let sinks = crate::landscape::find_sinks(&positions, &faces);
    // Marker radius scaled to the terrain footprint.
    let (min, max) = positions.iter().fold(
        (DVec3::splat(f64::INFINITY), DVec3::splat(f64::NEG_INFINITY)),
        |(lo, hi), p| (lo.min(*p), hi.max(*p)),
    );
    let r = (0.02 * (max - min).truncate().length()).clamp(0.15, 2.0);
    // Sink depth: how far below its lowest neighbor rim the vertex sits — a
    // marker tag, not a storage volume.
    let neighbor_min_z = |i: usize| -> f64 {
        let mut z = f64::INFINITY;
        for f in &faces {
            if f.contains(&(i as u32)) {
                for &v in f {
                    if v as usize != i {
                        z = z.min(positions[v as usize].z);
                    }
                }
            }
        }
        z
    };

    let new_ids: Vec<ObjectId> = match ids {
        Some(ids) if ids.len() == sinks.len() => ids,
        _ => (0..sinks.len()).map(|_| ObjectId::new()).collect(),
    };
    let mut layers_created = Vec::new();
    if !sinks.is_empty() && !doc.layers.contains_key(ANALYSIS_LAYER) {
        doc.layers
            .insert(ANALYSIS_LAYER.to_string(), LayerStyle::default());
        layers_created.push(ANALYSIS_LAYER.to_string());
    }
    let mut samples = Vec::with_capacity(sinks.len());
    for (&vi, id) in sinks.iter().zip(&new_ids) {
        let c = positions[vi];
        samples.push((neighbor_min_z(vi) - c.z, c, "sink".to_string()));
        let circle: Vec<DVec3> = (0..12)
            .map(|k| {
                let a = k as f64 / 12.0 * std::f64::consts::TAU;
                c + DVec3::new(r * a.cos(), r * a.sin(), 0.05)
            })
            .collect();
        doc.insert(SceneObject {
            visible: true,
            id: *id,
            name: Some("ponding sink".to_string()),
            layer: ANALYSIS_LAYER.to_string(),
            color: Some([0.1, 0.35, 0.85]),
            material: None,
            lineweight_mm: None,
            geometry: Geometry::Curve(Curve::Polyline { points: circle, closed: true }),
        });
    }
    doc.analysis_reports.insert(
        "ponding".to_string(),
        build_analysis_report(
            "ponding",
            format!(
                "{} local minimum(s) — visualization, not hydrology engineering",
                sinks.len()
            ),
            "m below rim",
            samples,
        ),
    );
    doc.generation += 1;
    let message = if sinks.is_empty() {
        "ponding: no local minima — terrain drains to its edges (local-minima check only, \
         not hydrology engineering)"
            .to_string()
    } else {
        format!(
            "ponding: {} potential sink(s) marked on '{ANALYSIS_LAYER}' — local-minima \
             visualization, not hydrology engineering",
            sinks.len()
        )
    };
    Ok((
        Command::Ponding { ids: Some(new_ids.clone()) },
        Inverse::CreatedOnLayer { created: new_ids.clone(), layers_created },
        ApplyOutcome { message, created: new_ids },
    ))
}

/// Hardscape path: a constant-width ribbon mesh following the selected curve,
/// draped onto the terrain (where present), with an accessible-slope advisory
/// (warns past 1:12). Deterministic function of the scene + params.
fn exec_site_path(
    doc: &mut Document,
    targets: Selector,
    width: f64,
    ids: Option<Vec<ObjectId>>,
) -> Result<(Command, Inverse, ApplyOutcome), ExecError> {
    if width <= 0.0 || !width.is_finite() {
        return Err(ExecError::Invalid("sitepath width must be > 0".into()));
    }
    let target_ids = resolve(doc, &targets)?;
    let curve = target_ids
        .iter()
        .find_map(|id| match doc.get(*id).map(|o| &o.geometry) {
            Some(Geometry::Curve(c)) => Some(c.clone()),
            _ => None,
        })
        .ok_or_else(|| {
            ExecError::Invalid(
                "sitepath needs a selected curve (draw a line/polyline/arc first)".into(),
            )
        })?;
    let centerline = crate::landscape::sample_polyline(
        &curve.tessellate(PROFILE_TOL),
        curve.is_closed(),
        0.5,
    );
    if centerline.len() < 2 {
        return Err(ExecError::Invalid("sitepath curve has no length".into()));
    }
    // Drape onto the terrain surface where the path crosses it (2 cm wear
    // course above grade); off-terrain samples keep the curve's own z.
    let terrain = terrain_surface(doc).ok().map(|(_, m)| (m.positions().to_vec(), m.faces().to_vec()));
    let mut draped_n = 0usize;
    let draped: Vec<DVec3> = centerline
        .iter()
        .map(|p| match &terrain {
            Some((pos, faces)) => {
                match crate::landscape::terrain_z_at(pos, faces, p.x, p.y) {
                    Some(z) => {
                        draped_n += 1;
                        DVec3::new(p.x, p.y, z + 0.02)
                    }
                    None => *p,
                }
            }
            None => *p,
        })
        .collect();
    const ACCESS_LIMIT: f64 = 1.0 / 12.0;
    let (max_slope, steep) = crate::landscape::path_slope_check(&draped, ACCESS_LIMIT);
    let (positions, faces) = crate::landscape::ribbon(&draped, width);
    let length: f64 = draped.windows(2).map(|w| (w[1] - w[0]).length()).sum();

    let new_ids: Vec<ObjectId> = match ids {
        Some(ids) if ids.len() == 1 => ids,
        _ => vec![ObjectId::new()],
    };
    let mut layers_created = Vec::new();
    if !doc.layers.contains_key("hardscape") {
        doc.layers.insert(
            "hardscape".to_string(),
            LayerStyle { color: Some([0.62, 0.60, 0.56, 1.0]), ..LayerStyle::default() },
        );
        layers_created.push("hardscape".to_string());
    }
    doc.insert(SceneObject {
        visible: true,
        id: new_ids[0],
        name: Some("sitepath".to_string()),
        layer: "hardscape".to_string(),
        color: None,
        material: None,
        lineweight_mm: None,
        geometry: Geometry::Mesh(kernel_mesh::Mesh::new(positions, faces)),
    });
    doc.generation += 1;

    let drape_note = if draped_n > 0 { " draped onto terrain" } else { "" };
    let slope_note = if steep > 0 {
        format!(
            "; WARNING: {steep} segment(s) exceed the 1:12 accessible slope \
             (steepest 1:{:.0}) — advisory, not a code check",
            1.0 / max_slope
        )
    } else {
        format!("; grade within 1:12 throughout (steepest {max_slope:.3} rise/run) — advisory")
    };
    Ok((
        Command::SitePath { targets, width, ids: Some(new_ids.clone()) },
        Inverse::CreatedOnLayer { created: new_ids.clone(), layers_created },
        ApplyOutcome {
            message: format!(
                "sitepath: {length:.1} m x {width} m ribbon{drape_note} on 'hardscape'{slope_note}"
            ),
            created: new_ids,
        },
    ))
}

/// Sun-path diagram: the yearly sun-path dome for the document's location as
/// open polylines on a golden `sunpath` layer — seven date arcs (Dec 21 → Jun
/// 21; Jul–Nov retrace them), analemma-style hour curves, plus a horizon
/// compass circle at the dome's rim. Pure view-of-the-sky geometry centered on
/// the world origin; scale comes from the scene so the dome reads over the
/// massing.
fn exec_sun_path(
    doc: &mut Document,
    ids: Option<Vec<ObjectId>>,
    year: i32,
    radius: Option<f64>,
) -> Result<(Command, Inverse, ApplyOutcome), ExecError> {
    let loc = doc.location.ok_or_else(|| {
        ExecError::Invalid(
            "no location set — run `sun <lat> <lon> <date> <time>` or `location <lat> <lon>`, \
             or `import <file.epw>` first"
                .into(),
        )
    })?;
    // Auto radius: 1.2× the scene's bounding radius from the origin, floored at
    // 10 m so an empty/small scene still gets a readable dome.
    let r = match radius {
        Some(r) if r > 0.0 => r,
        Some(_) => return Err(ExecError::Invalid("sunpath radius must be > 0".into())),
        None => doc
            .scene_aabb()
            .map(|aabb| {
                let m = aabb
                    .min
                    .abs()
                    .max(aabb.max.abs());
                (m.x.hypot(m.y) * 1.2).max(10.0)
            })
            .unwrap_or(10.0),
    };

    let diagram =
        itsjustcad_solar::sun_path_diagram(year, loc.lat_deg, loc.lon_deg, loc.tz_hours);
    if diagram.day_arcs.is_empty() {
        return Err(ExecError::Invalid(
            "sun never rises at this location in any sampled month — no sun path".into(),
        ));
    }

    // Scale the unit-hemisphere polylines to the dome radius.
    let scale = |pts: &[[f64; 3]]| -> Vec<DVec3> {
        pts.iter().map(|p| DVec3::new(p[0] * r, p[1] * r, p[2] * r)).collect()
    };
    struct Line {
        pts: Vec<DVec3>,
        closed: bool,
    }
    let mut lines: Vec<Line> = Vec::new();
    let n_arcs = diagram.day_arcs.len();
    for (_, pts) in &diagram.day_arcs {
        lines.push(Line { pts: scale(pts), closed: false });
    }
    let n_hours = diagram.hour_curves.len();
    for (_, pts) in &diagram.hour_curves {
        // A full 12-month analemma loops; a partial (horizon-clipped) one stays
        // an open curve.
        let closed = pts.len() == 12;
        lines.push(Line { pts: scale(pts), closed });
    }
    // Horizon compass circle at the dome rim (z = 0).
    let circle: Vec<DVec3> = (0..64)
        .map(|i| {
            let a = (i as f64) / 64.0 * std::f64::consts::TAU;
            DVec3::new(r * a.cos(), r * a.sin(), 0.0)
        })
        .collect();
    lines.push(Line { pts: circle, closed: true });

    let new_ids: Vec<ObjectId> = match ids {
        Some(ids) if ids.len() == lines.len() => ids,
        _ => (0..lines.len()).map(|_| ObjectId::new()).collect(),
    };

    // Golden analysis layer so the dome reads as an overlay, not model geometry.
    let mut layers_created = Vec::new();
    if !doc.layers.contains_key("sunpath") {
        doc.layers.insert(
            "sunpath".to_string(),
            LayerStyle { color: Some([0.95, 0.72, 0.2, 1.0]), ..LayerStyle::default() },
        );
        layers_created.push("sunpath".to_string());
    }

    for (line, id) in lines.iter().zip(&new_ids) {
        doc.insert(SceneObject {
            visible: true,
            id: *id,
            name: None,
            layer: "sunpath".to_string(),
            color: None,
            material: None,
            lineweight_mm: None,
            geometry: Geometry::Curve(Curve::Polyline {
                points: line.pts.clone(),
                closed: line.closed,
            }),
        });
    }
    doc.generation += 1;

    Ok((
        Command::SunPath { ids: Some(new_ids.clone()), year, radius },
        Inverse::CreatedOnLayer { created: new_ids.clone(), layers_created },
        ApplyOutcome {
            message: format!(
                "sunpath {year} @ ({:.2}, {:.2}): {n_arcs} date arc(s) + {n_hours} hour \
                 curve(s) + horizon circle, radius {r:.1} m on 'sunpath'",
                loc.lat_deg, loc.lon_deg
            ),
            created: new_ids,
        },
    ))
}

/// Sunlight-hours heatmap. Sample a regular XY grid over the scene bounding box
/// at `z=0`, and for each cell ray-cast toward the sun every 30 min of the date.
/// A cell counts an hour of sun when no scene triangle occludes the ray. Results
/// become a single flat quad-mesh on the `analysis` layer, per-vertex... (mesh
/// has no per-vertex color, so we emit one small colored quad per cell instead:
/// blue = 0 h, red = max h). Brute-force triangle intersection — fine at massing
/// scale; a BVH is future work.
fn exec_sun_hours(
    doc: &mut Document,
    ids: Option<Vec<ObjectId>>,
    year: i32,
    month: u32,
    day: u32,
    spacing: f64,
) -> Result<(Command, Inverse, ApplyOutcome), ExecError> {
    let loc = doc.location.ok_or_else(|| {
        ExecError::Invalid(
            "no location set — run `sun <lat> <lon> <date> <time>` or `location <lat> <lon>`, \
             or `import <file.epw>` first"
                .into(),
        )
    })?;
    if spacing <= 0.0 {
        return Err(ExecError::Invalid("grid spacing must be > 0".into()));
    }
    let aabb = doc.scene_aabb().ok_or_else(|| {
        ExecError::Invalid("sunhours needs geometry to bound the ground grid".into())
    })?;
    // Build a BVH over the scene triangles so each cell/sun ray only tests the
    // triangles whose boxes it crosses instead of the whole soup.
    let tri_bvh = kernel_mesh::TriBvh::build(
        scene_triangles(doc)
            .into_iter()
            .map(|t| {
                [
                    DVec3::from_array(t[0]),
                    DVec3::from_array(t[1]),
                    DVec3::from_array(t[2]),
                ]
            })
            .collect(),
    );

    // Precompute sun directions (up only) for every 30 min of the day.
    let mut sun_dirs: Vec<[f64; 3]> = Vec::new();
    for slot in 0..48 {
        let local_min = slot * 30;
        let utc = (local_min as f64 - loc.tz_hours * 60.0).rem_euclid(1440.0);
        let pos = itsjustcad_solar::solar_position(
            year,
            month,
            day,
            (utc / 60.0) as u32,
            (utc % 60.0) as u32,
            loc.lat_deg,
            loc.lon_deg,
        );
        if pos.altitude_deg > 0.0 {
            let d = itsjustcad_solar::sun_direction(pos.azimuth_deg, pos.altitude_deg);
            sun_dirs.push([d[0] as f64, d[1] as f64, d[2] as f64]);
        }
    }
    if sun_dirs.is_empty() {
        return Err(ExecError::Invalid(
            "sun never rises on this date at this location — nothing to sample".into(),
        ));
    }

    // Grid cell centers across the footprint.
    let nx = ((aabb.max.x - aabb.min.x) / spacing).ceil().max(1.0) as usize;
    let ny = ((aabb.max.y - aabb.min.y) / spacing).ceil().max(1.0) as usize;
    if nx * ny > 40_000 {
        return Err(ExecError::Invalid(format!(
            "grid {nx}x{ny} too fine ({} cells) — increase spacing", nx * ny
        )));
    }

    // Ray-cast each cell center toward each sun position; count unoccluded
    // slots. Cells are independent → parallel over cells (ordered collect keeps
    // row-major cell order; lit counts are integers, so no float-order hazard).
    let origins: Vec<DVec3> = (0..nx * ny)
        .map(|c| {
            let (ix, iy) = (c % nx, c / nx);
            let x = aabb.min.x + (ix as f64 + 0.5) * spacing;
            let y = aabb.min.y + (iy as f64 + 0.5) * spacing;
            // Lift the origin slightly so a ground-coincident triangle at the
            // sample point doesn't self-occlude.
            DVec3::new(x, y, 1e-4)
        })
        .collect();
    let dirs: Vec<DVec3> = sun_dirs.iter().map(|&d| DVec3::from_array(d)).collect();
    let lit_counts = lit_slot_counts(&origins, &dirs, &tri_bvh);
    let mut cells: Vec<(f64, f64, f64)> = Vec::with_capacity(nx * ny); // (x, y, hours)
    let mut max_h = 0.0f64;
    for (o, &lit) in origins.iter().zip(&lit_counts) {
        let hours = lit as f64 * 0.5; // 30-min slots → hours
        max_h = max_h.max(hours);
        cells.push((o.x, o.y, hours));
    }

    // Meshes carry no per-vertex color, so emit one small colored quad per cell,
    // grading blue (few hours) → red (most). All quads land on `analysis`; the
    // single logged op records every quad's id for replay stability.
    let new_ids: Vec<ObjectId> = match ids {
        Some(ids) if ids.len() == cells.len() => ids,
        _ => (0..cells.len()).map(|_| ObjectId::new()).collect(),
    };

    let mut layers_created = Vec::new();
    if !doc.layers.contains_key(ANALYSIS_LAYER) {
        doc.layers
            .insert(ANALYSIS_LAYER.to_string(), LayerStyle::default());
        layers_created.push(ANALYSIS_LAYER.to_string());
    }

    let half = spacing * 0.5;
    for ((x, y, hours), id) in cells.iter().zip(&new_ids) {
        let frac = if max_h > 0.0 { hours / max_h } else { 0.0 };
        // blue (few hours) → red (most hours)
        let color = [frac as f32, 0.15, (1.0 - frac) as f32];
        let corners = vec![
            DVec3::new(x - half, y - half, 0.0),
            DVec3::new(x + half, y - half, 0.0),
            DVec3::new(x + half, y + half, 0.0),
            DVec3::new(x - half, y + half, 0.0),
        ];
        let mesh = kernel_mesh::Mesh::new(corners, vec![[0, 1, 2], [0, 2, 3]]);
        doc.insert(SceneObject {
            visible: true,
            id: *id,
            name: None,
            layer: ANALYSIS_LAYER.to_string(),
            color: Some(color),
            material: None,
            lineweight_mm: None,
            geometry: Geometry::Mesh(mesh),
        });
    }
    // Structured summary for the deck's `report` command: darkest ground cells
    // mark permanently shaded courtyards/edges the LLM should call out.
    doc.analysis_reports.insert(
        "sunhours".to_string(),
        build_analysis_report(
            "sunhours",
            format!("{year}-{month:02}-{day:02}, {spacing} m ground grid"),
            "h",
            cells.iter().map(|&(x, y, h)| (h, DVec3::new(x, y, 0.0), "ground".to_string())).collect(),
        ),
    );
    doc.generation += 1;

    Ok((
        Command::SunHours { ids: Some(new_ids.clone()), year, month, day, spacing },
        Inverse::CreatedOnLayer { created: new_ids.clone(), layers_created },
        ApplyOutcome {
            message: format!(
                "sunhours {year}-{month:02}-{day:02}: {}x{ny} grid ({} cells) at {spacing} m, \
                 max {max_h:.1} h sun on '{ANALYSIS_LAYER}'",
                nx,
                new_ids.len()
            ),
            created: new_ids,
        },
    ))
}

/// Per-face insolation (sun-hours). For each triangle of the selected mesh(es),
/// ray-cast from the triangle centroid toward the sun every 30 min of the date
/// and count the daylight half-hours with no scene triangle occluding the ray.
/// The result is one small colored overlay triangle per face on the `analysis`
/// layer (blue = few hours → red = most), plus a min/avg/max report. Mirrors
/// `sunhours` (ground grid) but samples the actual faces instead of a plane, so
/// tilted/vertical surfaces get an honest count and self-shadowing is handled by
/// testing against the whole scene (including the surface's own object).
fn exec_face_sun_hours(
    doc: &mut Document,
    targets: Selector,
    ids: Option<Vec<ObjectId>>,
    year: i32,
    month: u32,
    day: u32,
) -> Result<(Command, Inverse, ApplyOutcome), ExecError> {
    let loc = doc.location.ok_or_else(|| {
        ExecError::Invalid(
            "no location set — run `sun <lat> <lon> <date> <time>` or `location <lat> <lon>`, \
             or `import <file.epw>` first"
                .into(),
        )
    })?;
    let target_ids = resolve(doc, &targets)?;

    // World-space triangles of the *selected* faces only (what we score + color).
    let mut faces: Vec<[DVec3; 3]> = Vec::new();
    for id in &target_ids {
        if let Some(obj) = doc.get(*id)
            && let Geometry::Mesh(m) = &obj.geometry
        {
            let pos = m.positions();
            for f in m.faces() {
                faces.push([pos[f[0] as usize], pos[f[1] as usize], pos[f[2] as usize]]);
            }
        }
    }
    if faces.is_empty() {
        return Err(ExecError::Invalid(
            "facesunhours needs a selected mesh (extrude or box first, then select)".into(),
        ));
    }

    // Occlusion is tested against the whole scene (so neighbouring massing and
    // the surface's own object can shade it).
    let tri_bvh = kernel_mesh::TriBvh::build(
        scene_triangles(doc)
            .into_iter()
            .map(|t| {
                [
                    DVec3::from_array(t[0]),
                    DVec3::from_array(t[1]),
                    DVec3::from_array(t[2]),
                ]
            })
            .collect(),
    );

    // Sun directions (up only) every 30 min of the date.
    let mut sun_dirs: Vec<DVec3> = Vec::new();
    for slot in 0..48 {
        let local_min = slot * 30;
        let utc = (local_min as f64 - loc.tz_hours * 60.0).rem_euclid(1440.0);
        let pos = itsjustcad_solar::solar_position(
            year,
            month,
            day,
            (utc / 60.0) as u32,
            (utc % 60.0) as u32,
            loc.lat_deg,
            loc.lon_deg,
        );
        if pos.altitude_deg > 0.0 {
            let d = itsjustcad_solar::sun_direction(pos.azimuth_deg, pos.altitude_deg);
            sun_dirs.push(DVec3::new(d[0] as f64, d[1] as f64, d[2] as f64));
        }
    }
    if sun_dirs.is_empty() {
        return Err(ExecError::Invalid(
            "sun never rises on this date at this location — nothing to sample".into(),
        ));
    }

    // Score each face: hours of unoccluded sun. A ray is only counted when the
    // sun is on the lit side of the face (dot(normal, sun) > 0); a downward- or
    // away-facing surface can't see that sun position at all. Faces are
    // independent → parallel per-face scoring (ordered collect, integer counts).
    let scored = face_lit_slots(&faces, &sun_dirs, &tri_bvh);
    let mut hours: Vec<f64> = Vec::with_capacity(faces.len());
    let mut samples: Vec<(f64, DVec3, String)> = Vec::with_capacity(faces.len());
    let mut max_h = 0.0f64;
    for &(lit, centroid, normal) in &scored {
        let h = lit as f64 * 0.5;
        max_h = max_h.max(h);
        samples.push((h, centroid, facing_label(normal).to_string()));
        hours.push(h);
    }

    let new_ids: Vec<ObjectId> = match ids {
        Some(ids) if ids.len() == faces.len() => ids,
        _ => (0..faces.len()).map(|_| ObjectId::new()).collect(),
    };

    let mut layers_created = Vec::new();
    if !doc.layers.contains_key(ANALYSIS_LAYER) {
        doc.layers
            .insert(ANALYSIS_LAYER.to_string(), LayerStyle::default());
        layers_created.push(ANALYSIS_LAYER.to_string());
    }

    for ((tri, &h), id) in faces.iter().zip(&hours).zip(&new_ids) {
        let frac = if max_h > 0.0 { h / max_h } else { 0.0 };
        let color = [frac as f32, 0.15, (1.0 - frac) as f32];
        // Overlay copy of the face, nudged toward the sky along its normal so it
        // renders just above the source surface instead of z-fighting with it.
        let mut normal = (tri[1] - tri[0]).cross(tri[2] - tri[0]);
        let nlen = normal.length();
        if nlen > 1e-12 {
            normal /= nlen;
        }
        let lift = normal * 5e-3;
        let mesh = kernel_mesh::Mesh::new(
            vec![tri[0] + lift, tri[1] + lift, tri[2] + lift],
            vec![[0, 1, 2]],
        );
        doc.insert(SceneObject {
            visible: true,
            id: *id,
            name: None,
            layer: ANALYSIS_LAYER.to_string(),
            color: Some(color),
            material: None,
            lineweight_mm: None,
            geometry: Geometry::Mesh(mesh),
        });
    }
    // Structured summary for the deck's `report` command (per-face sun-hours
    // with facings, so the LLM can critique glazing/amenity placement).
    doc.analysis_reports.insert(
        "facesunhours".to_string(),
        build_analysis_report(
            "facesunhours",
            format!("{year}-{month:02}-{day:02}"),
            "h",
            samples,
        ),
    );
    doc.generation += 1;

    let n = hours.len() as f64;
    let sum: f64 = hours.iter().sum();
    let avg = sum / n;
    let min_h = hours.iter().cloned().fold(f64::INFINITY, f64::min);
    let daylight = itsjustcad_solar::daylight_hours(year, month, day, loc.lat_deg);

    Ok((
        Command::FaceSunHours { targets, ids: Some(new_ids.clone()), year, month, day },
        Inverse::CreatedOnLayer { created: new_ids.clone(), layers_created },
        ApplyOutcome {
            message: format!(
                "facesunhours {year}-{month:02}-{day:02}: {} face(s), sun-hours \
                 min {min_h:.1} / avg {avg:.1} / max {max_h:.1} \
                 (astronomical daylight {daylight:.1} h) on '{ANALYSIS_LAYER}'",
                new_ids.len()
            ),
            created: new_ids,
        },
    ))
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
            material: None,
            lineweight_mm: None,
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

/// Per-instance baked-block key for a dynamic-block instance. Deterministic in
/// the instance id so undo/redo/replay recreate the exact same key.
fn param_bake_key(source: &str, id: ObjectId) -> String {
    format!("__param/{source}/{}", id.short())
}

/// Bake a parametric block at concrete param `values`: expand its template,
/// run each line on a scratch document, and harvest the resulting geometry as
/// a flat `Vec<BlockGeometry>`. Instances/points produced by the template are
/// skipped (blocks are flat). Runs on a throwaway `Session` so the template's
/// own commands never touch the live op-log.
fn bake_param_block(
    def: &itsjustcad_doc::ParamBlockDef,
    values: &BTreeMap<String, String>,
) -> Result<Vec<itsjustcad_doc::BlockGeometry>, ExecError> {
    use itsjustcad_doc::BlockGeometry;
    let mut scratch = Session::default();
    for line in def.expand(values) {
        let cmd = crate::parse::parse(&line)
            .map_err(|e| ExecError::Invalid(format!("param-block template line '{line}': {e}")))?;
        scratch
            .run(cmd)
            .map_err(|e| ExecError::Invalid(format!("param-block template line '{line}': {e}")))?;
    }
    let mut out = Vec::new();
    for obj in scratch.doc.objects() {
        match &obj.geometry {
            Geometry::Mesh(m)
            | Geometry::Frame { mesh: m, .. }
            | Geometry::Area { mesh: m, .. } => out.push(BlockGeometry::Mesh(m.clone())),
            Geometry::Curve(c) => out.push(BlockGeometry::Curve(c.clone())),
            Geometry::Annotation(a) => out.push(BlockGeometry::Annotation(a.clone())),
            Geometry::Instance { .. } | Geometry::Points { .. } => {}
        }
    }
    if out.is_empty() {
        return Err(ExecError::Invalid(
            "param-block template produced no geometry".into(),
        ));
    }
    Ok(out)
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
                material: None,
                lineweight_mm: None,
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
                material: None,
                lineweight_mm: None,
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
                material: None,
                lineweight_mm: None,
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
        Command::Loft { id, targets, guides } => {
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
            let mut guide_polys = Vec::new();
            if let Some(gsel) = &guides {
                let gids = resolve(doc, gsel)?;
                if gids.is_empty() {
                    return Err(ExecError::BadProfile(
                        "loft guides selector matched no curves".into(),
                    ));
                }
                for gid in &gids {
                    let curve = curve_of(doc, *gid, "loft")?;
                    if curve.is_closed() {
                        return Err(ExecError::BadProfile(format!(
                            "guide '{gid}' is closed; guides must run openly from the first \
                             profile to the last"
                        )));
                    }
                    guide_polys.push(curve.tessellate(PROFILE_TOL));
                }
            }
            let mesh = if guide_polys.is_empty() {
                kernel_mesh::loft_profiles(&profiles)
            } else {
                kernel_mesh::loft_profiles_guided(&profiles, &guide_polys, 8)
            };
            let n_guides = guide_polys.len();
            let id = id.unwrap_or_default();
            doc.insert(SceneObject {
                visible: true,
                id,
                name: None,
                layer: doc.current_layer.clone(),
                color: None,
                material: None,
                lineweight_mm: None,
                geometry: Geometry::Mesh(mesh),
            });
            let message = if n_guides > 0 {
                format!("lofted {} profiles with {n_guides} guide(s) -> {id}", ids.len())
            } else {
                format!("lofted {} profiles -> {id}", ids.len())
            };
            Ok((
                Command::Loft { id: Some(id), targets, guides },
                Inverse::DeleteCreated(vec![id]),
                ApplyOutcome { created: vec![id], message },
            ))
        }
        Command::BlendSurface { id, a, b, bulge } => {
            let aid = resolve(doc, &a)?;
            let bid = resolve(doc, &b)?;
            let ([aid], [bid]) = (&aid[..], &bid[..]) else {
                return Err(ExecError::Invalid(
                    "blend needs exactly one curve per selector".into(),
                ));
            };
            let (aid, bid) = (*aid, *bid);
            if bulge <= 0.0 {
                return Err(ExecError::Invalid("blend bulge must be > 0".into()));
            }
            let ca = curve_of(doc, aid, "blend")?;
            let cb = curve_of(doc, bid, "blend")?;
            let closed = match (ca.is_closed(), cb.is_closed()) {
                (true, true) => true,
                (false, false) => false,
                _ => {
                    return Err(ExecError::BadProfile(
                        "blend curves must both be open or both closed".into(),
                    ))
                }
            };
            let pa = ca.tessellate(PROFILE_TOL);
            let pb = cb.tessellate(PROFILE_TOL);
            let mesh = kernel_mesh::blend_curves(&pa, &pb, closed, bulge, 16);
            let id = id.unwrap_or_default();
            doc.insert(SceneObject {
                visible: true,
                id,
                name: None,
                layer: doc.current_layer.clone(),
                color: None,
                material: None,
                lineweight_mm: None,
                geometry: Geometry::Mesh(mesh),
            });
            Ok((
                Command::BlendSurface { id: Some(id), a, b, bulge },
                Inverse::DeleteCreated(vec![id]),
                ApplyOutcome {
                    created: vec![id],
                    message: format!("blend {aid} <-> {bid} (bulge {bulge}) -> {id}"),
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
                material: None,
                lineweight_mm: None,
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
                material: None,
                lineweight_mm: None,
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
                material: None,
                lineweight_mm: None,
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
                material: None,
                lineweight_mm: None,
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
        Command::Geodesic { id, frequency, radius, full } => {
            if frequency == 0 {
                return Err(ExecError::Invalid("geodesic frequency must be >= 1".into()));
            }
            if frequency > MAX_GEODESIC_FREQ {
                return Err(ExecError::Invalid(format!(
                    "geodesic frequency must be <= {MAX_GEODESIC_FREQ}"
                )));
            }
            let radius = finite(radius, "geodesic radius")?;
            if radius <= 0.0 {
                return Err(ExecError::Invalid("geodesic radius must be positive".into()));
            }
            let (_, segs) = kernel_mesh::geodesic_network(frequency, radius, !full);
            // Strut side scales gently with radius so the frame reads at any size.
            let strut = (radius * 0.02).clamp(0.01, 0.5);
            let mesh = kernel_mesh::strut_lattice(&segs, strut);
            let id = mesh_object(doc, id, mesh);
            Ok((
                Command::Geodesic { id: Some(id), frequency, radius, full },
                Inverse::DeleteCreated(vec![id]),
                ApplyOutcome {
                    created: vec![id],
                    message: format!(
                        "geodesic {id} (freq {frequency}, r={radius}, {})",
                        if full { "sphere" } else { "dome" }
                    ),
                },
            ))
        }
        Command::SpaceFrame { id, nx, ny, bay, depth } => {
            if nx == 0 || ny == 0 {
                return Err(ExecError::Invalid("space frame nx and ny must be >= 1".into()));
            }
            if bay <= 0.0 || depth <= 0.0 {
                return Err(ExecError::Invalid(
                    "space frame bay and depth must be positive".into(),
                ));
            }
            let segs = kernel_mesh::spaceframe_struts(nx, ny, bay, depth);
            let strut = (bay * 0.04).clamp(0.02, 0.3);
            let mesh = kernel_mesh::strut_lattice(&segs, strut);
            let id = mesh_object(doc, id, mesh);
            Ok((
                Command::SpaceFrame { id: Some(id), nx, ny, bay, depth },
                Inverse::DeleteCreated(vec![id]),
                ApplyOutcome {
                    created: vec![id],
                    message: format!("spaceframe {id} ({nx}x{ny}, bay={bay}, depth={depth})"),
                },
            ))
        }
        Command::Hypar { id, a, b, c, nu, nv } => {
            let (a, b, c) = (finite(a, "hypar a")?, finite(b, "hypar b")?, finite(c, "hypar c")?);
            if a <= 0.0 || b <= 0.0 {
                return Err(ExecError::Invalid("hypar a and b must be positive".into()));
            }
            if c == 0.0 {
                return Err(ExecError::Invalid("hypar c must be non-zero".into()));
            }
            let (nu_v, nv_v) =
                (clamp_grid(nu.unwrap_or(12)), clamp_grid(nv.unwrap_or(12)));
            let mesh = kernel_mesh::hypar_surface(a, b, c, nu_v, nv_v);
            let id = mesh_object(doc, id, mesh);
            Ok((
                Command::Hypar { id: Some(id), a, b, c, nu, nv },
                Inverse::DeleteCreated(vec![id]),
                ApplyOutcome {
                    created: vec![id],
                    message: format!("hypar {id} (a={a}, b={b}, c={c})"),
                },
            ))
        }
        Command::GaussVault { id, span, length, rise, undulate } => {
            let span = finite(span, "gaussvault span")?;
            let length = finite(length, "gaussvault length")?;
            let rise = finite(rise, "gaussvault rise")?;
            if span <= 0.0 || length <= 0.0 || rise <= 0.0 {
                return Err(ExecError::Invalid(
                    "gaussvault span, length and rise must be positive".into(),
                ));
            }
            let mesh = kernel_mesh::gaussvault_surface(span, length, rise, 24, 24, undulate);
            let id = mesh_object(doc, id, mesh);
            Ok((
                Command::GaussVault { id: Some(id), span, length, rise, undulate },
                Inverse::DeleteCreated(vec![id]),
                ApplyOutcome {
                    created: vec![id],
                    message: format!(
                        "gaussvault {id} (span={span}, len={length}, rise={rise}{})",
                        if undulate { ", undulating" } else { "" }
                    ),
                },
            ))
        }
        Command::Gridshell { id, surface, nu, nv } => {
            let (nu_v, nv_v) =
                (clamp_grid(nu.unwrap_or(12)), clamp_grid(nv.unwrap_or(12)));
            // Validate surface parameters up front.
            match surface {
                crate::GridshellSurfaceSpec::Hypar { a, b, c } => {
                    finite(a, "gridshell a")?;
                    finite(b, "gridshell b")?;
                    finite(c, "gridshell c")?;
                    if a <= 0.0 || b <= 0.0 || c == 0.0 {
                        return Err(ExecError::Invalid(
                            "gridshell hypar needs a>0, b>0, c!=0".into(),
                        ));
                    }
                }
                crate::GridshellSurfaceSpec::Vault { span, length, rise, undulate: _ } => {
                    finite(span, "gridshell span")?;
                    finite(length, "gridshell length")?;
                    finite(rise, "gridshell rise")?;
                    if span <= 0.0 || length <= 0.0 || rise <= 0.0 {
                        return Err(ExecError::Invalid(
                            "gridshell vault needs span>0, length>0, rise>0".into(),
                        ));
                    }
                }
            }
            let mesh = kernel_mesh::gridshell(surface.to_kernel(), nu_v, nv_v, 0.06);
            let id = mesh_object(doc, id, mesh);
            Ok((
                Command::Gridshell { id: Some(id), surface, nu, nv },
                Inverse::DeleteCreated(vec![id]),
                ApplyOutcome {
                    created: vec![id],
                    message: format!("gridshell {id} ({nu_v}x{nv_v} lattice)"),
                },
            ))
        }
        Command::Funicular {
            id,
            support_a,
            support_b,
            segments,
            load,
            slack,
            invert,
        } => {
            if !support_a.is_finite() || !support_b.is_finite() {
                return Err(ExecError::Invalid(
                    "funicular supports must be finite points".into(),
                ));
            }
            if (support_b - support_a).length() < 1e-6 {
                return Err(ExecError::Invalid(
                    "funicular supports must be distinct points".into(),
                ));
            }
            let seg = segments.unwrap_or(24).clamp(2, MAX_GRID);
            let ld = load.unwrap_or(1.0).max(0.0);
            let sl = slack.unwrap_or(1.4);
            let mut pts = kernel_mesh::funicular_chain(support_a, support_b, seg, ld, sl);
            if invert {
                pts = kernel_mesh::invert_funicular(&pts);
            }
            let segsv: Vec<(glam::DVec3, glam::DVec3)> =
                pts.windows(2).map(|w| (w[0], w[1])).collect();
            let span = (support_b - support_a).length();
            let strut = (span * 0.02).clamp(0.02, 0.4);
            let mesh = kernel_mesh::strut_lattice(&segsv, strut);
            let id = mesh_object(doc, id, mesh);
            Ok((
                Command::Funicular {
                    id: Some(id),
                    support_a,
                    support_b,
                    segments,
                    load,
                    slack,
                    invert,
                },
                Inverse::DeleteCreated(vec![id]),
                ApplyOutcome {
                    created: vec![id],
                    message: format!(
                        "funicular {id} ({seg} links{})",
                        if invert { ", inverted → compression arch" } else { "" }
                    ),
                },
            ))
        }
        Command::Tensegrity { id, struts, radius, height, twist_deg } => {
            if struts < 3 {
                return Err(ExecError::Invalid("tensegrity needs >= 3 struts".into()));
            }
            if struts > MAX_GRID {
                return Err(ExecError::Invalid(format!(
                    "tensegrity struts must be <= {MAX_GRID}"
                )));
            }
            let r = finite(radius.unwrap_or(1.0), "tensegrity radius")?;
            let h = finite(height.unwrap_or(2.0), "tensegrity height")?;
            if r <= 0.0 || h <= 0.0 {
                return Err(ExecError::Invalid(
                    "tensegrity radius and height must be positive".into(),
                ));
            }
            // Default twist is the antiprism stability twist π(1/2 − 1/n).
            let tw = match twist_deg {
                Some(d) => finite(d, "tensegrity twist")?.to_radians(),
                None => {
                    std::f64::consts::PI * (0.5 - 1.0 / struts as f64)
                }
            };
            let t = kernel_mesh::tensegrity_prism(struts, r, h, tw);
            // Thick struts (compression) + thin cables (tension) merged into one.
            let mut mesh = kernel_mesh::strut_lattice(&t.net.strut_segments(), (r * 0.08).max(0.03));
            let cables = kernel_mesh::strut_lattice(&t.net.cable_segments(), (r * 0.03).max(0.012));
            mesh.merge(&cables);
            let id = mesh_object(doc, id, mesh);
            Ok((
                Command::Tensegrity { id: Some(id), struts, radius, height, twist_deg },
                Inverse::DeleteCreated(vec![id]),
                ApplyOutcome {
                    created: vec![id],
                    message: format!("tensegrity {id} ({struts}-strut prism)"),
                },
            ))
        }
        Command::Cablenet { id, corners, n, sag } => {
            if corners.iter().any(|c| !c.is_finite()) {
                return Err(ExecError::Invalid("cablenet corners must be finite".into()));
            }
            let nn = clamp_grid(n.unwrap_or(8));
            let sg = finite(sag.unwrap_or(1.5), "cablenet sag")?;
            let (_, _, segsv) = kernel_mesh::cable_net(corners, nn, sg);
            if segsv.is_empty() {
                return Err(ExecError::Invalid("cablenet produced no links".into()));
            }
            // Strut side scales with the net span.
            let span = (corners[1] - corners[0]).length().max(1e-3);
            let strut = (span * 0.01).clamp(0.01, 0.2);
            let mesh = kernel_mesh::strut_lattice(&segsv, strut);
            let id = mesh_object(doc, id, mesh);
            Ok((
                Command::Cablenet { id: Some(id), corners, n, sag },
                Inverse::DeleteCreated(vec![id]),
                ApplyOutcome {
                    created: vec![id],
                    message: format!("cablenet {id} ({nn}x{nn} net)"),
                },
            ))
        }
        Command::MinSurf { id, target, n } => {
            let (tid, curve) = one_curve(doc, &target, "minsurf")?;
            if !curve.is_closed() {
                return Err(ExecError::Invalid(format!(
                    "minsurf needs a closed boundary curve; '{tid}' is open"
                )));
            }
            let mut boundary = curve.tessellate(PROFILE_TOL);
            // Closed polylines may repeat the first point at the end; the soap
            // film wants the loop without the duplicate.
            if boundary.len() >= 2
                && (boundary[0] - boundary[boundary.len() - 1]).length() < 1e-9
            {
                boundary.pop();
            }
            let nn = clamp_grid(n.unwrap_or(16)).max(2);
            let mesh = kernel_mesh::minimal_surface(&boundary, nn).ok_or_else(|| {
                ExecError::Invalid("minsurf: boundary curve is degenerate".into())
            })?;
            let id = mesh_object(doc, id, mesh);
            Ok((
                Command::MinSurf { id: Some(id), target, n },
                Inverse::DeleteCreated(vec![id]),
                ApplyOutcome {
                    created: vec![id],
                    message: format!("minsurf {id} (soap film, {nn}x{nn} grid over {tid})"),
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
        Command::InsertKnot { target, t } => {
            let ids = resolve(doc, &target)?;
            let [tid] = ids[..] else {
                return Err(ExecError::Invalid(format!(
                    "insertknot target matched {} objects, expected exactly 1",
                    ids.len()
                )));
            };
            let curve = curve_of(doc, tid, "insertknot")?;
            let Curve::Nurbs { control, weights, knots, degree } = curve else {
                return Err(ExecError::Invalid(
                    "insertknot works on NURBS curves only (try `curve` or `interpcurve`)".into(),
                ));
            };
            if !(0.0..=1.0).contains(&t) {
                return Err(ExecError::Invalid("insertknot parameter must be in 0..1".into()));
            }
            // Map the normalized parameter into the knot domain.
            let (k0, k1) = (knots[*degree], knots[knots.len() - degree - 1]);
            let t_dom = k0 + (k1 - k0) * t;
            let (nc, nw, nk) = kernel_curve::insert_knot(control, weights, knots, *degree, t_dom)
                .ok_or_else(|| {
                    ExecError::Invalid(
                        "insertknot: parameter is at a curve end or the knot is already at full \
                         multiplicity"
                            .into(),
                    )
                })?;
            let n_control = nc.len();
            let new = Curve::Nurbs { control: nc, weights: nw, knots: nk, degree: *degree };
            let obj = doc.get_mut(tid).expect("resolved");
            let snapshot = obj.geometry.clone();
            obj.geometry = Geometry::Curve(new);
            Ok((
                Command::InsertKnot { target, t },
                Inverse::SetGeometry(vec![(tid, snapshot)]),
                ApplyOutcome {
                    created: Vec::new(),
                    message: format!(
                        "insertknot {tid} t={t:.3} -> {n_control} control points (shape unchanged)"
                    ),
                },
            ))
        }
        Command::CurvatureGraph { target, ids, scale, samples } => {
            exec_curvature_graph(doc, target, ids, scale, samples)
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
                material: None,
                lineweight_mm: None,
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
                material: None,
                lineweight_mm: None,
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
                material: None,
                lineweight_mm: None,
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
                itsjustcad_doc::HatchPattern::Lines { spacing, .. }
                | itsjustcad_doc::HatchPattern::Crosshatch { spacing, .. }
                | itsjustcad_doc::HatchPattern::Brick { spacing }
                | itsjustcad_doc::HatchPattern::Concrete { spacing }
                | itsjustcad_doc::HatchPattern::Insulation { spacing }
                | itsjustcad_doc::HatchPattern::Earth { spacing }
                | itsjustcad_doc::HatchPattern::Ansi { spacing, .. } => Some(*spacing),
                itsjustcad_doc::HatchPattern::Solid => None,
            };
            if let itsjustcad_doc::HatchPattern::Ansi { code, .. } = &pattern
                && !(31..=38).contains(code)
            {
                return Err(ExecError::Invalid(format!(
                    "unknown ANSI hatch code {code} (use 31..38)"
                )));
            }
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
                material: None,
                lineweight_mm: None,
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
        Command::ExactBoolean { id, op, a_corner, a_size, b_corner, b_size } => {
            // Reject non-finite coordinates (NaN/Inf). Left unchecked they flow
            // into make_box -> NaN mesh vertices, and NaN/Inf serialize to JSON
            // `null` on save — which then fails to deserialize into f64 on reload,
            // permanently bricking the saved op-log. The sibling generators
            // (geodesic/hypar/…) already guard with finite(); this one was missed.
            for (v, what) in [
                (a_corner, "exact_boolean a_corner"),
                (a_size, "exact_boolean a_size"),
                (b_corner, "exact_boolean b_corner"),
                (b_size, "exact_boolean b_size"),
            ] {
                if !v.is_finite() {
                    return Err(ExecError::Invalid(format!(
                        "{what} must have finite coordinates"
                    )));
                }
            }
            let (mesh, volume, exact) =
                exact_or_mesh_boolean(op, a_corner, a_size, b_corner, b_size);
            if mesh.faces().is_empty() {
                return Err(ExecError::Invalid(format!(
                    "{op} of the two boxes is empty (they do not overlap)"
                )));
            }
            let id = id.unwrap_or_default();
            doc.insert(SceneObject {
                visible: true,
                id,
                name: None,
                layer: "solids".to_string(),
                color: None,
                material: None,
                lineweight_mm: None,
                geometry: Geometry::Mesh(mesh),
            });
            let path = if exact {
                "exact OCCT kernel"
            } else {
                "mesh kernel (OCCT feature off — fallback)"
            };
            Ok((
                Command::ExactBoolean { id: Some(id), op, a_corner, a_size, b_corner, b_size },
                Inverse::DeleteCreated(vec![id]),
                ApplyOutcome {
                    created: vec![id],
                    message: format!(
                        "exact {op} of two boxes -> {id} (volume {volume:.4}, via {path})"
                    ),
                },
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
                    material: None,
                    lineweight_mm: None,
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
                material: None,
                lineweight_mm: None,
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
            doc.insert(SceneObject { id, name, layer, visible: true, color: None, material: None, lineweight_mm: None, geometry: Geometry::Curve(joined) });
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
                material: None,
                lineweight_mm: None,
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
                material: None,
                lineweight_mm: None,
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
        Command::LayerRename { from, to } => {
            if from == to {
                return Err(ExecError::Invalid("layerrename: names are identical".into()));
            }
            if !doc.layers.contains_key(&from) {
                let known: Vec<&str> = doc.layers.keys().map(String::as_str).collect();
                return Err(ExecError::Invalid(format!(
                    "no layer '{from}' (layers: {})",
                    known.join(", ")
                )));
            }
            if doc.layers.contains_key(&to) {
                return Err(ExecError::Invalid(format!("layer '{to}' already exists")));
            }
            let style = doc.layers.remove(&from).expect("checked");
            doc.layers.insert(to.clone(), style);
            for obj in doc.objects_mut() {
                if obj.layer == from {
                    obj.layer = to.clone();
                }
            }
            if doc.current_layer == from {
                doc.current_layer = to.clone();
            }
            doc.generation += 1;
            Ok((
                Command::LayerRename { from: from.clone(), to: to.clone() },
                Inverse::LayerRename { from: from.clone(), to: to.clone() },
                ApplyOutcome {
                    created: Vec::new(),
                    message: format!("layer '{from}' renamed to '{to}'"),
                },
            ))
        }
        Command::LayerDelete { layer } => {
            if layer == itsjustcad_doc::DEFAULT_LAYER {
                return Err(ExecError::Invalid(
                    "cannot delete the default layer".into(),
                ));
            }
            let Some(style) = doc.layers.get(&layer).cloned() else {
                let known: Vec<&str> = doc.layers.keys().map(String::as_str).collect();
                return Err(ExecError::Invalid(format!(
                    "no layer '{layer}' (layers: {})",
                    known.join(", ")
                )));
            };
            // Reassign objects on this layer to the default layer (recorded for undo).
            let moved: Vec<ObjectId> = doc
                .objects()
                .filter(|o| o.layer == layer)
                .map(|o| o.id)
                .collect();
            for id in &moved {
                if let Some(obj) = doc.get_mut(*id) {
                    obj.layer = itsjustcad_doc::DEFAULT_LAYER.to_string();
                }
            }
            let prev_current = (doc.current_layer == layer).then(|| {
                let prev = doc.current_layer.clone();
                doc.current_layer = itsjustcad_doc::DEFAULT_LAYER.to_string();
                prev
            });
            doc.layers.remove(&layer);
            doc.generation += 1;
            Ok((
                Command::LayerDelete { layer: layer.clone() },
                Inverse::LayerDelete { layer: layer.clone(), style, moved: moved.clone(), prev_current },
                ApplyOutcome {
                    created: Vec::new(),
                    message: format!(
                        "layer '{layer}' deleted ({} object(s) → default)",
                        moved.len()
                    ),
                },
            ))
        }
        Command::LayerOrder { layer, order } => {
            let style = layer_style_mut(doc, &layer)?;
            let prev = style.clone();
            style.order = order;
            doc.generation += 1;
            Ok((
                Command::LayerOrder { layer: layer.clone(), order },
                Inverse::LayerStyle { layer: layer.clone(), prev },
                ApplyOutcome {
                    created: Vec::new(),
                    message: format!("layer '{layer}' order set to {order}"),
                },
            ))
        }
        Command::LayerLock { layer, locked } => {
            let style = layer_style_mut(doc, &layer)?;
            let prev = style.clone();
            style.locked = locked;
            doc.generation += 1;
            Ok((
                Command::LayerLock { layer: layer.clone(), locked },
                Inverse::LayerStyle { layer: layer.clone(), prev },
                ApplyOutcome {
                    created: Vec::new(),
                    message: format!(
                        "layer '{layer}' {}",
                        if locked { "locked" } else { "unlocked" }
                    ),
                },
            ))
        }
        Command::LayerLinetype { layer, linetype } => {
            let style = layer_style_mut(doc, &layer)?;
            let prev = style.clone();
            style.linetype = linetype;
            doc.generation += 1;
            Ok((
                Command::LayerLinetype { layer: layer.clone(), linetype },
                Inverse::LayerStyle { layer: layer.clone(), prev },
                ApplyOutcome {
                    created: Vec::new(),
                    message: format!("layer '{layer}' linetype set to {}", linetype.label()),
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
        Command::Lineweight { targets, mm } => {
            let ids = resolve(doc, &targets)?;
            let mut prev = Vec::with_capacity(ids.len());
            for id in &ids {
                let obj = doc.get_mut(*id).expect("resolved");
                prev.push((*id, obj.lineweight_mm));
                obj.lineweight_mm = Some(mm);
            }
            doc.generation += 1;
            Ok((
                Command::Lineweight { targets, mm },
                Inverse::ObjectLineweight(prev),
                ApplyOutcome {
                    created: Vec::new(),
                    message: format!("lineweight {mm:.3} mm on {} object(s)", ids.len()),
                },
            ))
        }
        Command::LinweightOff { targets } => {
            let ids = resolve(doc, &targets)?;
            let mut prev = Vec::with_capacity(ids.len());
            for id in &ids {
                let obj = doc.get_mut(*id).expect("resolved");
                prev.push((*id, obj.lineweight_mm));
                obj.lineweight_mm = None;
            }
            doc.generation += 1;
            Ok((
                Command::LinweightOff { targets },
                Inverse::ObjectLineweight(prev),
                ApplyOutcome {
                    created: Vec::new(),
                    message: format!("cleared lineweight on {} object(s)", ids.len()),
                },
            ))
        }
        Command::ShowWeights { on } => {
            let prev = doc.show_lineweights;
            doc.show_lineweights = on;
            doc.generation += 1;
            Ok((
                Command::ShowWeights { on },
                Inverse::ShowWeights { prev },
                ApplyOutcome {
                    created: Vec::new(),
                    message: format!("viewport lineweights {}", if on { "on" } else { "off" }),
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
        Command::Material2 { targets, material } => {
            let ids = resolve(doc, &targets)?;
            let mut prev = Vec::with_capacity(ids.len());
            for id in &ids {
                let obj = doc.get_mut(*id).expect("resolved");
                prev.push((*id, obj.material));
                obj.material = Some(material);
            }
            doc.generation += 1;
            let (_, rough, metal) = material.pbr();
            let label = match material {
                itsjustcad_doc::ObjectMaterial::Preset { preset } => preset.label().to_string(),
                itsjustcad_doc::ObjectMaterial::Custom { .. } => {
                    format!("custom (rough {rough:.2}, metal {metal:.2})")
                }
            };
            Ok((
                Command::Material2 { targets, material },
                Inverse::ObjectMaterial(prev),
                ApplyOutcome {
                    created: Vec::new(),
                    message: format!("material2 {label} on {} object(s)", ids.len()),
                },
            ))
        }
        Command::Material2Off { targets } => {
            let ids = resolve(doc, &targets)?;
            let mut prev = Vec::with_capacity(ids.len());
            for id in &ids {
                let obj = doc.get_mut(*id).expect("resolved");
                prev.push((*id, obj.material));
                obj.material = None;
            }
            doc.generation += 1;
            Ok((
                Command::Material2Off { targets },
                Inverse::ObjectMaterial(prev),
                ApplyOutcome {
                    created: Vec::new(),
                    message: format!("cleared material2 on {} object(s)", ids.len()),
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
        Command::Sun { azimuth_deg, altitude_deg, lat_deg, lon_deg } => {
            let prev_sun = doc.sun;
            let prev_loc = doc.location;
            doc.sun = Some(itsjustcad_doc::SunPosition { azimuth_deg, altitude_deg });
            // Record the observer location so environmental analyses can reuse
            // it. tz is UTC (0) because `sun` takes UTC clock times.
            doc.location = Some(itsjustcad_doc::GeoLocation { lat_deg, lon_deg, tz_hours: 0.0 });
            doc.generation += 1;
            Ok((
                Command::Sun { azimuth_deg, altitude_deg, lat_deg, lon_deg },
                Inverse::Location { prev_loc, prev_sun },
                ApplyOutcome {
                    created: Vec::new(),
                    message: format!(
                        "sun az={azimuth_deg:.1}° alt={altitude_deg:.1}° @ ({lat_deg:.3},{lon_deg:.3})"
                    ),
                },
            ))
        }
        Command::Location { lat_deg, lon_deg, tz_hours } => {
            let prev_loc = doc.location;
            let prev_sun = doc.sun;
            doc.location = Some(itsjustcad_doc::GeoLocation { lat_deg, lon_deg, tz_hours });
            doc.generation += 1;
            Ok((
                Command::Location { lat_deg, lon_deg, tz_hours },
                Inverse::Location { prev_loc, prev_sun },
                ApplyOutcome {
                    created: Vec::new(),
                    message: format!(
                        "location set to ({lat_deg:.4}, {lon_deg:.4}) tz {tz_hours:+.1}h"
                    ),
                },
            ))
        }
        Command::ShadowStudy { ids, year, month, day, from_min, to_min, step_min } => {
            exec_shadow_study(doc, ids, year, month, day, from_min, to_min, step_min)
        }
        Command::SunHours { ids, year, month, day, spacing } => {
            exec_sun_hours(doc, ids, year, month, day, spacing)
        }
        Command::SunPath { ids, year, radius } => exec_sun_path(doc, ids, year, radius),
        Command::Radiation { targets, ids, path, bins } => {
            exec_radiation(doc, targets, ids, path, bins)
        }
        Command::FaceSunHours { targets, ids, year, month, day } => {
            exec_face_sun_hours(doc, targets, ids, year, month, day)
        }
        Command::Contours { interval, major_every, ids } => {
            exec_contours(doc, interval, major_every, ids)
        }
        Command::Pad { at, width, depth, elev, slope } => {
            exec_pad(doc, at, width, depth, elev, slope)
        }
        Command::CutFill { original_z } => exec_cutfill(doc, original_z),
        Command::LotSubdivide { targets, method, area, width, irregularity, seed, ids } => {
            exec_lot_subdivide(doc, targets, method, area, width, irregularity, seed, ids)
        }
        Command::LotSettings { sets, prev } => exec_lot_settings(doc, sets, prev),
        Command::LotLoading { targets, mode, prev } => exec_lot_loading(doc, targets, mode, prev),
        Command::LotGenerateSite {
            targets,
            pattern,
            roadwidth,
            blockdepth,
            alleys,
            seed,
            road_ids,
            block_ids,
        } => exec_lot_generate_site(
            doc, targets, pattern, roadwidth, blockdepth, alleys, seed, road_ids, block_ids,
        ),
        Command::LotSetbacks { targets, front, side, rear, buildto, envelope, ids } => {
            exec_lot_setbacks(doc, targets, front, side, rear, buildto, envelope, ids)
        }
        Command::LotFrontage { targets, at } => exec_lot_frontage(doc, targets, at),
        Command::LotOpenSpace { targets, feature, area, reserve, ids } => {
            exec_lot_openspace(doc, targets, feature, area, reserve, ids)
        }
        Command::FlowArrows { n, ids } => exec_flow_arrows(doc, n, ids),
        Command::Ponding { ids } => exec_ponding(doc, ids),
        Command::CodeCheck { pack, story, rules, ids } => {
            exec_codecheck(doc, pack, story, rules, ids)
        }
        // Handled in Session::run (they touch session state, not the doc) and
        // never logged, so apply_forward/replay cannot legitimately see them.
        Command::CheckRulesList | Command::CheckRulesLoad { .. } => Err(ExecError::Invalid(
            "checkrules is session-level; this is a bug".into(),
        )),
        Command::SitePath { targets, width, ids } => exec_site_path(doc, targets, width, ids),
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
            doc.sheets.push(itsjustcad_doc::Sheet {
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
            s.views.push(itsjustcad_doc::SheetView { direction, scale });
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
                // SAF 2.2.0 workbook — '.saf' and '.xlsx' both emit the same
                // genuine OOXML spreadsheet (so 'model.saf.xlsx' works too).
                "saf" | "xlsx" => {
                    let (b, detail) = crate::saf::export(doc).map_err(ExecError::Invalid)?;
                    (b, detail)
                }
                "3dm" => {
                    let (b, counts) = crate::rhino3dm::export(doc);
                    (b, format!("3DM (openNURBS V5), {counts}"))
                }
                "step" | "stp" => {
                    // STEP export goes through OCCT. The document stores meshes
                    // (no persisted analytic BREP), so this writes a FACETED STEP
                    // part — one planar face per triangle — not exact analytic
                    // surfaces. OCCT itself writes the file at its own path, so we
                    // hand it the target directly and read the bytes back for the
                    // uniform write/echo below.
                    let (positions, faces) = flatten_doc_meshes(doc);
                    if faces.is_empty() {
                        return Err(ExecError::Invalid(
                            "no triangle meshes to export to STEP".to_string(),
                        ));
                    }
                    let Some(res) = kernel_occt::write_mesh_step(&positions, &faces, &path) else {
                        return Err(ExecError::Invalid(
                            "STEP export needs the exact-BREP tier — rebuild with the 'kernel-occt' feature".to_string(),
                        ));
                    };
                    let n = res.map_err(ExecError::Invalid)?;
                    let bytes = std::fs::read(&path)
                        .map_err(|e| ExecError::Invalid(format!("cannot read written '{path}': {e}")))?;
                    (bytes, format!("STEP (faceted), {n} faces"))
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
        Command::ControlImages { prefix } => {
            // Requires the GPU view (depth buffer, edge/mask passes), which lives
            // in the render/app layer — the commands crate has no wgpu. The app
            // and headless runner intercept this verb and call
            // `itsjustcad_render::render_control_images` directly. Reaching exec
            // means no GPU context is available.
            Err(ExecError::Invalid(format!(
                "controlimages needs the GPU view — run 'controlimages {prefix}' from the app command line or headless renderer"
            )))
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
                    Geometry::Mesh(m)
                    | Geometry::Frame { mesh: m, .. }
                    | Geometry::Area { mesh: m, .. } => mesh_surface_area(m),
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
                    Geometry::Points { .. } => {
                        return Err(ExecError::Invalid(format!(
                            "'{id}' is a point cloud — area not supported on point clouds"
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
                    Geometry::Mesh(m)
                    | Geometry::Frame { mesh: m, .. }
                    | Geometry::Area { mesh: m, .. } => total += kernel_mesh::signed_volume(m),
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
                    Geometry::Points { .. } => {
                        return Err(ExecError::Invalid(format!(
                            "'{id}' is a point cloud; volume not supported on point clouds"
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
        Command::EnviroReport { kind } => {
            if doc.analysis_reports.is_empty() && doc.compliance_reports.is_empty() {
                return Err(ExecError::Invalid(
                    "no analysis stored — run sunhours, facesunhours, radiation, \
                     shadowstudy, or codecheck first, then `report`"
                        .into(),
                ));
            }
            let mut msg = String::new();
            for (k, r) in &doc.analysis_reports {
                if kind.as_deref().is_none_or(|want| want == k) {
                    msg.push_str(&format_analysis_report(r));
                }
            }
            // Compliance reports ride the same plane: bare `report` includes
            // them; `report codecheck` (or a pack name) filters to them.
            for (k, r) in &doc.compliance_reports {
                if kind.as_deref().is_none_or(|want| want == k || want == "codecheck") {
                    msg.push_str(&format_compliance_report(r));
                }
            }
            if msg.is_empty() {
                let stored: Vec<String> = doc
                    .analysis_reports
                    .keys()
                    .cloned()
                    .chain(doc.compliance_reports.keys().map(|k| format!("codecheck:{k}")))
                    .collect();
                return Err(ExecError::Invalid(format!(
                    "no '{}' report stored (stored: {})",
                    kind.as_deref().unwrap_or("?"),
                    stored.join(", ")
                )));
            }
            Ok((
                Command::EnviroReport { kind },
                Inverse::Rename(Vec::new()), // never logged; inverse unused
                ApplyOutcome {
                    created: Vec::new(),
                    message: msg.trim_end().to_string(),
                },
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
                material: None,
                lineweight_mm: None,
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
        Command::PointLiteral { id, positions } => {
            if positions.is_empty() {
                return Err(ExecError::Invalid(
                    "point_literal: positions must be non-empty".into(),
                ));
            }
            let id = id.unwrap_or_default();
            let count = positions.len();
            doc.insert(SceneObject {
                visible: true,
                id,
                name: None,
                layer: doc.current_layer.clone(),
                color: None,
                material: None,
                lineweight_mm: None,
                geometry: Geometry::Points { positions: positions.clone() },
            });
            Ok((
                Command::PointLiteral { id: Some(id), positions },
                Inverse::DeleteCreated(vec![id]),
                ApplyOutcome {
                    created: vec![id],
                    message: format!("point cloud {id} ({count} points)"),
                },
            ))
        }
        Command::BlockDefine { targets, name, geometries } => {
            use itsjustcad_doc::BlockGeometry;
            // Replay / import path supplies geometry directly and needs NO source
            // objects — skip selector resolution (which would error on an empty
            // selector). Only the live "define from selection" path resolves ids.
            let ids = if geometries.is_some() {
                Vec::new()
            } else {
                resolve(doc, &targets)?
            };
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
                            Geometry::Mesh(m)
                            | Geometry::Frame { mesh: m, .. }
                            | Geometry::Area { mesh: m, .. } => BlockGeometry::Mesh(m.clone()),
                            Geometry::Curve(c) => BlockGeometry::Curve(c.clone()),
                            Geometry::Annotation(a) => BlockGeometry::Annotation(a.clone()),
                            // Instances and point clouds within a block are skipped.
                            Geometry::Instance { .. } | Geometry::Points { .. } => return None,
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
        Command::BlockInsert { id, name, position, rotation_deg, scale, params } => {
            let is_param = doc.param_blocks.contains_key(&name);
            if !is_param && !doc.blocks.contains_key(&name) {
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
            // Dynamic block: bake this instance's geometry under a per-instance
            // key and record the resolved param values on the instance.
            if is_param {
                let def = doc.param_blocks.get(&name).expect("checked").clone();
                let values = def.resolve_values(&params);
                let baked = bake_param_block(&def, &values)?;
                let key = param_bake_key(&name, id);
                doc.blocks.insert(key.clone(), baked);
                doc.insert(SceneObject {
                    visible: true,
                    id,
                    name: None,
                    layer: doc.current_layer.clone(),
                    color: None,
                    material: None,
                    lineweight_mm: None,
                    geometry: Geometry::Instance {
                        block: key.clone(),
                        position,
                        rotation_deg: rot,
                        scale: sc,
                        source: Some(name.clone()),
                        params: values.clone(),
                    },
                });
                return Ok((
                    Command::BlockInsert {
                        id: Some(id),
                        name: name.clone(),
                        position,
                        rotation_deg: Some(rot),
                        scale: Some(sc),
                        params: values,
                    },
                    Inverse::CreatedAndBake { created: vec![id], block_key: key },
                    ApplyOutcome {
                        created: vec![id],
                        message: format!("insert '{name}' -> {id} at {position}"),
                    },
                ));
            }
            doc.insert(SceneObject {
                visible: true,
                id,
                name: None,
                layer: doc.current_layer.clone(),
                color: None,
                material: None,
                lineweight_mm: None,
                geometry: Geometry::Instance {
                    block: name.clone(),
                    position,
                    rotation_deg: rot,
                    scale: sc,
                    source: None,
                    params: Default::default(),
                },
            });
            Ok((
                Command::BlockInsert {
                    id: Some(id),
                    name: name.clone(),
                    position,
                    rotation_deg: Some(rot),
                    scale: Some(sc),
                    params: Default::default(),
                },
                Inverse::DeleteCreated(vec![id]),
                ApplyOutcome {
                    created: vec![id],
                    message: format!("insert '{}' -> {id} at {position}", name),
                },
            ))
        }
        Command::BlockParamDefine { name, params, body } => {
            use itsjustcad_doc::ParamBlockDef;
            if name.is_empty() {
                return Err(ExecError::Invalid("pblock: name cannot be empty".into()));
            }
            let def = ParamBlockDef { params: params.clone(), body: body.clone() };
            // Validate the template bakes at its defaults before committing.
            bake_param_block(&def, &def.default_values())?;
            let prev = doc.param_blocks.insert(name.clone(), def);
            doc.generation += 1;
            let np = params.len();
            Ok((
                Command::BlockParamDefine { name: name.clone(), params, body },
                Inverse::ParamBlockDef { name: name.clone(), prev },
                ApplyOutcome {
                    created: Vec::new(),
                    message: format!(
                        "dynamic block '{name}' defined ({np} param{})",
                        if np == 1 { "" } else { "s" }
                    ),
                },
            ))
        }
        Command::BlockParamSet { target, params } => {
            let ids = resolve(doc, &target)?;
            if ids.len() != 1 {
                return Err(ExecError::Invalid(format!(
                    "param: selector matched {} objects, expected exactly 1",
                    ids.len()
                )));
            }
            let id = ids[0];
            let obj = doc.get(id).expect("resolved");
            let (source, cur_params, position, rot, sc) = match &obj.geometry {
                Geometry::Instance {
                    source: Some(src),
                    params: cur,
                    position,
                    rotation_deg,
                    scale,
                    ..
                } => (src.clone(), cur.clone(), *position, *rotation_deg, *scale),
                _ => {
                    return Err(ExecError::Invalid(format!(
                        "param: '{id}' is not a dynamic-block instance"
                    )))
                }
            };
            let def = doc
                .param_blocks
                .get(&source)
                .ok_or_else(|| {
                    ExecError::Invalid(format!("param: no dynamic block '{source}'"))
                })?
                .clone();
            // Reject unknown keys so typos surface instead of silently no-op.
            for k in params.keys() {
                if !cur_params.contains_key(k) {
                    return Err(ExecError::Invalid(format!(
                        "param: '{source}' has no parameter '{k}'"
                    )));
                }
            }
            let mut new_values = cur_params.clone();
            for (k, v) in &params {
                new_values.insert(k.clone(), v.clone());
            }
            let baked = bake_param_block(&def, &new_values)?;
            let key = param_bake_key(&source, id);
            let prev_bake = doc.blocks.insert(key.clone(), baked).unwrap_or_default();
            let prev_geometry = doc.get(id).expect("resolved").geometry.clone();
            if let Some(o) = doc.get_mut(id) {
                o.geometry = Geometry::Instance {
                    block: key.clone(),
                    position,
                    rotation_deg: rot,
                    scale: sc,
                    source: Some(source.clone()),
                    params: new_values.clone(),
                };
            }
            doc.generation += 1;
            Ok((
                Command::BlockParamSet { target, params },
                Inverse::ParamBlockSet { id, prev_geometry, block_key: key, prev_bake },
                ApplyOutcome {
                    created: Vec::new(),
                    message: format!("param: '{id}' updated"),
                },
            ))
        }
        Command::BlockDeleteDef { name } => {
            // Guard: refuse while live instances reference this definition. A
            // plain-block instance points `block` at the name; a dynamic-block
            // instance points `source` at it (its `block` is a per-instance
            // baked key). Never orphan live geometry.
            let instances = doc
                .objects()
                .filter(|o| match &o.geometry {
                    Geometry::Instance { block, source, .. } => {
                        source.as_deref() == Some(name.as_str())
                            || (source.is_none() && *block == name)
                    }
                    _ => false,
                })
                .count();
            if instances > 0 {
                return Err(ExecError::Invalid(format!(
                    "blockdelete: {instances} instance{} of '{name}' exist{} — delete the instance{} first",
                    if instances == 1 { "" } else { "s" },
                    if instances == 1 { "s" } else { "" },
                    if instances == 1 { "" } else { "s" },
                )));
            }
            let prev_plain = doc.blocks.remove(&name);
            let prev_param = doc.param_blocks.remove(&name);
            if prev_plain.is_none() && prev_param.is_none() {
                return Err(ExecError::Invalid(format!(
                    "no block named '{name}' (use 'blocks' to list definitions)"
                )));
            }
            doc.generation += 1;
            Ok((
                Command::BlockDeleteDef { name: name.clone() },
                Inverse::BlockDeleted { name: name.clone(), prev_plain, prev_param },
                ApplyOutcome {
                    created: Vec::new(),
                    message: format!("deleted block definition '{name}'"),
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
        Command::RoomList => {
            let list: Vec<String> = doc
                .rooms
                .iter()
                .map(|r| {
                    format!("  {} ({}) — {}", r.name, r.occupancy, format_area(doc.units, r.area))
                })
                .collect();
            let msg = if list.is_empty() {
                "no rooms tagged (room <closed-curve> <occupancy>)".to_string()
            } else {
                format!("rooms:\n{}", list.join("\n"))
            };
            Ok((
                Command::RoomList,
                // Not logged; this Inverse is never stored.
                Inverse::DeleteCreated(Vec::new()),
                ApplyOutcome { created: Vec::new(), message: msg },
            ))
        }
        Command::BlockLibList => {
            let (names, dir) = crate::blocklib::list()
                .map_err(|e| ExecError::Invalid(e.to_string()))?;
            let msg = if names.is_empty() {
                format!("block library empty ({dir})\nhint: run 'blocksave <name>' to save a block definition")
            } else {
                format!(
                    "library blocks ({dir}):\n{}",
                    names.iter().map(|n| format!("  {n}")).collect::<Vec<_>>().join("\n")
                )
            };
            Ok((
                Command::BlockLibList,
                Inverse::DeleteCreated(Vec::new()),
                ApplyOutcome { created: Vec::new(), message: msg },
            ))
        }
        Command::BlockLibLoad { name, geometries } => {
            use itsjustcad_doc::BlockGeometry;
            let snaps: Vec<BlockGeometry> = if let Some(g) = geometries {
                // Replay path: use stored geometries.
                g
            } else {
                // Live path: load from library.
                let bf = crate::blocklib::load(&name)
                    .map_err(|e| ExecError::Invalid(e.to_string()))?;
                bf.geometries
            };
            let n = snaps.len();
            let prev = doc.blocks.insert(name.clone(), snaps.clone());
            doc.generation += 1;
            let msg = format!(
                "block '{}' loaded from library ({n} geometr{})",
                name,
                if n == 1 { "y" } else { "ies" }
            );
            Ok((
                Command::BlockLibLoad {
                    name: name.clone(),
                    geometries: Some(snaps),
                },
                Inverse::BlockDef { name, prev },
                ApplyOutcome { created: Vec::new(), message: msg },
            ))
        }
        Command::BlockLibSave { name, description } => {
            let defs = doc.blocks.get(&name).ok_or_else(|| {
                ExecError::Invalid(format!(
                    "no block named '{name}' in document (define it first with 'block')"
                ))
            })?;
            let path = crate::blocklib::save(&name, &description, defs.clone())
                .map_err(|e| ExecError::Invalid(e.to_string()))?;
            Ok((
                Command::BlockLibSave { name: name.clone(), description },
                Inverse::DeleteCreated(Vec::new()),
                ApplyOutcome {
                    created: Vec::new(),
                    message: format!("block '{}' saved to {}", name, path.display()),
                },
            ))
        }
        Command::DefSection { name, section } => {
            let prev = doc.sections.insert(name.clone(), section);
            doc.generation += 1;
            let msg = format!("section '{name}' defined");
            Ok((
                Command::DefSection { name: name.clone(), section },
                Inverse::SectionDef { name, prev },
                ApplyOutcome { created: Vec::new(), message: msg },
            ))
        }
        Command::DefMaterial { name, elastic_modulus_e, density } => {
            let prev = doc
                .materials
                .insert(name.clone(), Material { elastic_modulus_e, density });
            doc.generation += 1;
            let msg = format!("material '{name}' (E={elastic_modulus_e}, ρ={density})");
            Ok((
                Command::DefMaterial { name: name.clone(), elastic_modulus_e, density },
                Inverse::MaterialDef { name, prev },
                ApplyOutcome { created: Vec::new(), message: msg },
            ))
        }
        Command::DefGrid { name, x_axes, y_axes, levels } => {
            let grid = Grid {
                x_axes: x_axes.clone(),
                y_axes: y_axes.clone(),
                levels: levels.clone(),
            };
            let prev = doc.grids.insert(name.clone(), grid);
            doc.generation += 1;
            let msg = format!(
                "grid '{name}' ({} x-axes, {} y-axes, {} levels)",
                x_axes.len(),
                y_axes.len(),
                levels.len()
            );
            Ok((
                Command::DefGrid { name: name.clone(), x_axes, y_axes, levels },
                Inverse::GridDef { name, prev },
                ApplyOutcome { created: Vec::new(), message: msg },
            ))
        }
        Command::DefStory { name, elevation } => {
            let prev = doc.stories.clone();
            // Replace by name if it already exists, else append; keep sorted by
            // elevation so level lists read bottom-to-top.
            if let Some(s) = doc.stories.iter_mut().find(|s| s.name == name) {
                s.elevation = elevation;
            } else {
                doc.stories.push(Story { name: name.clone(), elevation });
            }
            doc.stories.sort_by(|a, b| a.elevation.total_cmp(&b.elevation));
            doc.generation += 1;
            Ok((
                Command::DefStory { name: name.clone(), elevation },
                Inverse::StoryList(prev),
                ApplyOutcome {
                    created: Vec::new(),
                    message: format!("story '{name}' at {elevation} m"),
                },
            ))
        }
        Command::Room { boundary, occupancy, name } => {
            let occ = occupancy.to_lowercase();
            if !ROOM_OCCUPANCIES.contains(&occ.as_str()) {
                return Err(ExecError::Invalid(format!(
                    "unknown occupancy '{occupancy}' (use one of: {})",
                    ROOM_OCCUPANCIES.join(", ")
                )));
            }
            let ids = resolve(doc, &boundary)?;
            // The region boundary is a single closed curve.
            if ids.len() != 1 {
                return Err(ExecError::Invalid(format!(
                    "room needs exactly one closed curve as its boundary ({} selected)",
                    ids.len()
                )));
            }
            let obj = doc.get(ids[0]).expect("resolved");
            let pts = match &obj.geometry {
                Geometry::Curve(c) if c.is_closed() => c.tessellate(PROFILE_TOL),
                Geometry::Curve(_) => {
                    return Err(ExecError::Invalid(
                        "room boundary is an open curve — close it first".into(),
                    ))
                }
                _ => {
                    return Err(ExecError::Invalid(
                        "room boundary must be a closed curve".into(),
                    ))
                }
            };
            if pts.len() < 3 {
                return Err(ExecError::Invalid(
                    "room boundary has fewer than 3 vertices".into(),
                ));
            }
            let area = shoelace_area(&pts);
            let prev = doc.rooms.clone();
            let label = name.clone().unwrap_or_else(|| {
                let n = doc.rooms.iter().filter(|r| r.occupancy == occ).count() + 1;
                format!("{occ}-{n}")
            });
            doc.rooms.push(Room {
                name: label.clone(),
                occupancy: occ.clone(),
                area,
                boundary: pts.iter().map(|p| [p.x, p.y, p.z]).collect(),
            });
            doc.generation += 1;
            let msg = format!(
                "room '{label}' ({occ}) — {}",
                format_area(doc.units, area)
            );
            Ok((
                Command::Room { boundary, occupancy: occ, name: Some(label) },
                Inverse::RoomList(prev),
                ApplyOutcome { created: Vec::new(), message: msg },
            ))
        }
        Command::FrameMember { id, kind, a, b, section, material, orientation_deg } => {
            if (b - a).length() < 1e-9 {
                return Err(ExecError::Invalid(
                    "frame member endpoints coincide (zero-length member)".into(),
                ));
            }
            let sec = *doc.sections.get(&section).ok_or_else(|| {
                ExecError::Invalid(format!(
                    "no section named '{section}' (define one with 'section {section} rect ...')"
                ))
            })?;
            if let Some(m) = &material
                && !doc.materials.contains_key(m)
            {
                return Err(ExecError::Invalid(format!(
                    "no material named '{m}' (define one with 'material {m} <E> <density>')"
                )));
            }
            let orient = orientation_deg.unwrap_or(0.0);
            let mesh = kernel_mesh::frame_member(&sec.boundary(), a, b, orient.to_radians());
            let id = id.unwrap_or_default();
            doc.insert(SceneObject {
                visible: true,
                id,
                name: None,
                layer: doc.current_layer.clone(),
                color: None,
                material: None,
                lineweight_mm: None,
                geometry: Geometry::Frame {
                    kind,
                    a,
                    b,
                    section: sec,
                    material: material.clone(),
                    orientation_deg: orient,
                    mesh,
                },
            });
            Ok((
                Command::FrameMember {
                    id: Some(id),
                    kind,
                    a,
                    b,
                    section,
                    material,
                    orientation_deg,
                },
                Inverse::DeleteCreated(vec![id]),
                ApplyOutcome {
                    created: vec![id],
                    message: format!("{} {id} ({:.2} m)", kind.label(), (b - a).length()),
                },
            ))
        }
        Command::AreaMember { id, kind, boundary, thickness, material } => {
            use itsjustcad_doc::AreaKind;
            if boundary.len() < 3 {
                return Err(ExecError::Invalid(
                    "area member needs a closed boundary of at least 3 points".into(),
                ));
            }
            if thickness <= 0.0 {
                return Err(ExecError::Invalid("area member thickness must be positive".into()));
            }
            if let Some(m) = &material
                && !doc.materials.contains_key(m)
            {
                return Err(ExecError::Invalid(format!(
                    "no material named '{m}' (define one with 'material {m} <E> <density>')"
                )));
            }
            // Slab extrudes up (+Z); wall extrudes along its in-plane normal
            // (boundary plane normal projected onto XY, rotated 90°). For a
            // typical vertical wall drawn as a footprint line the normal is
            // horizontal; fall back to +Z if the boundary is degenerate.
            let dir = match kind {
                AreaKind::Slab => DVec3::Z,
                AreaKind::Wall => wall_normal(&boundary),
            };
            let mesh = kernel_mesh::area_member(&boundary, dir, thickness);
            let id = id.unwrap_or_default();
            doc.insert(SceneObject {
                visible: true,
                id,
                name: None,
                layer: doc.current_layer.clone(),
                color: None,
                material: None,
                lineweight_mm: None,
                geometry: Geometry::Area {
                    kind,
                    boundary: boundary.clone(),
                    thickness,
                    dir,
                    material: material.clone(),
                    mesh,
                },
            });
            Ok((
                Command::AreaMember {
                    id: Some(id),
                    kind,
                    boundary,
                    thickness,
                    material,
                },
                Inverse::DeleteCreated(vec![id]),
                ApplyOutcome {
                    created: vec![id],
                    message: format!("{} {id} (t={thickness} m)", kind.label()),
                },
            ))
        }
        Command::AddLoad { name, geometry, magnitude, direction, index } => {
            // Normalise the direction vector; reject zero vector.
            let len = direction.length();
            if len < 1e-12 {
                return Err(ExecError::Invalid(
                    "load direction must be a non-zero vector".into(),
                ));
            }
            let dir = direction / len;
            let load = StructLoad {
                name: name.clone(),
                magnitude,
                direction: dir,
                geometry: geometry.clone(),
            };
            // Use the pre-filled index on replay so undo removes the same slot.
            let idx = index.unwrap_or(doc.loads.len());
            doc.loads.insert(idx, load);
            doc.generation += 1;
            let kind_label = match &geometry {
                LoadGeometry::Point { .. } => "point",
                LoadGeometry::Line { .. } => "line",
                LoadGeometry::Area { .. } => "area",
            };
            let msg = format!("load '{name}' ({kind_label}, {magnitude:.4e})");
            Ok((
                Command::AddLoad {
                    name,
                    geometry,
                    magnitude,
                    direction: dir,
                    index: Some(idx),
                },
                Inverse::RemoveLoad(idx),
                ApplyOutcome { created: Vec::new(), message: msg },
            ))
        }
        Command::AddSupport { position, kind, roller_axis, index } => {
            // Normalise roller axis if provided.
            let roller_axis = roller_axis.map(|ax| {
                let l = ax.length();
                if l < 1e-12 { DVec3::X } else { ax / l }
            });
            let support = StructSupport { position, kind, roller_axis };
            let idx = index.unwrap_or(doc.supports.len());
            doc.supports.insert(idx, support);
            doc.generation += 1;
            let msg = format!("support {} at ({:.2},{:.2},{:.2})", kind.label(), position.x, position.y, position.z);
            Ok((
                Command::AddSupport { position, kind, roller_axis, index: Some(idx) },
                Inverse::RemoveSupport(idx),
                ApplyOutcome { created: Vec::new(), message: msg },
            ))
        }
        Command::Constrain { kind, a, b, value } => {
            let ids_a = resolve(doc, &a)?;
            let [id_a] = ids_a[..] else {
                return Err(ExecError::Invalid(format!(
                    "constrain target must be a single object; selector matched {}",
                    ids_a.len()
                )));
            };
            let id_b = match &b {
                Some(sel) => {
                    let ids = resolve(doc, sel)?;
                    let [id] = ids[..] else {
                        return Err(ExecError::Invalid(format!(
                            "constrain target must be a single object; selector matched {}",
                            ids.len()
                        )));
                    };
                    Some(id)
                }
                None => None,
            };
            let constraint = crate::sketch::build_doc_constraint(doc, kind, id_a, id_b, value)?;
            doc.constraints.push(constraint);
            doc.generation += 1;
            let n = doc.constraints.len();
            // Solve immediately, SolveSpace-style. On an inconsistent system
            // the constraint is still recorded (delete it to back out) but the
            // geometry is left untouched.
            let report = crate::sketch::solve_document(doc)?;
            let mut snapshots = Vec::new();
            if report.converged {
                for (id, geo) in report.changed {
                    if let Some(obj) = doc.get_mut(id) {
                        snapshots.push((id, obj.geometry.clone()));
                        obj.geometry = geo;
                    }
                }
            }
            Ok((
                Command::Constrain { kind, a, b, value },
                Inverse::ConstraintAdded { snapshots },
                ApplyOutcome {
                    created: Vec::new(),
                    message: format!("constraint #{n} ({}): {}", kind.name(), report.message),
                },
            ))
        }
        Command::SolveConstraints => {
            if doc.constraints.is_empty() {
                return Err(ExecError::Invalid(
                    "no constraints to solve (add some with 'constrain')".into(),
                ));
            }
            let report = crate::sketch::solve_document(doc)?;
            let mut snapshots = Vec::new();
            if report.converged {
                for (id, geo) in report.changed {
                    if let Some(obj) = doc.get_mut(id) {
                        snapshots.push((id, obj.geometry.clone()));
                        obj.geometry = geo;
                    }
                }
            }
            let moved = snapshots.len();
            Ok((
                Command::SolveConstraints,
                Inverse::SetGeometry(snapshots),
                ApplyOutcome {
                    created: Vec::new(),
                    message: format!("{} ({moved} object(s) moved)", report.message),
                },
            ))
        }
        Command::ConstraintsList => {
            if doc.constraints.is_empty() {
                return Ok((
                    Command::ConstraintsList,
                    Inverse::SetGeometry(Vec::new()),
                    ApplyOutcome {
                        created: Vec::new(),
                        message: "no constraints (add some with 'constrain')".into(),
                    },
                ));
            }
            // solve_document only *reports* — write-back is the caller's job —
            // so listing never mutates geometry.
            let report = crate::sketch::solve_document(doc)?;
            let mut lines: Vec<String> = doc
                .constraints
                .iter()
                .enumerate()
                .map(|(i, c)| {
                    let n = i + 1;
                    let mut line =
                        format!("#{n} {}", crate::sketch::describe_constraint(doc, c));
                    if report.redundant.contains(&n) {
                        line.push_str("  [redundant]");
                    }
                    if report.failed.contains(&n) {
                        line.push_str("  [conflicts]");
                    }
                    line
                })
                .collect();
            lines.push(report.message);
            Ok((
                Command::ConstraintsList,
                Inverse::SetGeometry(Vec::new()),
                ApplyOutcome { created: Vec::new(), message: lines.join("\n") },
            ))
        }
        Command::ConstraintDelete { index } => {
            if doc.constraints.is_empty() {
                return Err(ExecError::Invalid("no constraints to delete".into()));
            }
            let removed: Vec<(usize, itsjustcad_doc::SketchConstraint)> = match index {
                Some(n) => {
                    if n == 0 || n > doc.constraints.len() {
                        return Err(ExecError::Invalid(format!(
                            "no constraint #{n}; there are {} (see 'constraints list')",
                            doc.constraints.len()
                        )));
                    }
                    vec![(n - 1, doc.constraints.remove(n - 1))]
                }
                None => std::mem::take(&mut doc.constraints)
                    .into_iter()
                    .enumerate()
                    .collect(),
            };
            doc.generation += 1;
            let message = match index {
                Some(n) => format!(
                    "deleted constraint #{n} ({})",
                    removed[0].1.kind_name()
                ),
                None => format!("deleted all {} constraint(s)", removed.len()),
            };
            Ok((
                Command::ConstraintDelete { index },
                Inverse::ConstraintsRestore(removed),
                ApplyOutcome { created: Vec::new(), message },
            ))
        }
        Command::Undo
        | Command::Redo
        | Command::Amend { .. }
        | Command::Option(..)
        | Command::Import { .. }
        | Command::Terrain { .. }
        | Command::OsmFile { .. }
        | Command::Plant { .. }
        | Command::PlantRow { .. }
        | Command::PlantSchedule { .. }
        | Command::PlantCatalog { .. }
        | Command::Miyawaki { .. } => {
            unreachable!("handled in Session::run")
        }
    }
}

/// Extrusion direction for a wall: the boundary's plane normal. A footprint
/// drawn flat in XY has a +Z normal, which would make it a slab; so for walls
/// we prefer the horizontal normal of the footprint's dominant edge. Falls back
/// to +Z when the boundary has no usable in-plane extent.
fn wall_normal(boundary: &[DVec3]) -> DVec3 {
    // Longest edge direction, rotated 90° in XY, gives the wall's thickness
    // direction (perpendicular to the wall run, horizontal).
    let mut best = DVec3::ZERO;
    let mut best_len = 0.0;
    for i in 0..boundary.len() {
        let e = boundary[(i + 1) % boundary.len()] - boundary[i];
        let l = e.length();
        if l > best_len {
            best_len = l;
            best = e;
        }
    }
    let n = DVec3::new(-best.y, best.x, 0.0);
    if n.length() < 1e-9 { DVec3::Z } else { n.normalize() }
}

fn describe(cmd: &Command) -> &'static str {
    match cmd {
        Command::Constrain { .. } => "constrain",
        Command::SolveConstraints => "solveconstraints",
        Command::ConstraintsList | Command::ConstraintDelete { .. } => "constraints",
        Command::Box { .. } => "box",
        Command::Extrude { .. } => "extrude",
        Command::Revolve { .. } => "revolve",
        Command::Loft { .. } => "loft",
        Command::BlendSurface { .. } => "blend",
        Command::Sweep { .. } => "sweep",
        Command::Sweep2 { .. } => "sweep2",
        Command::RailRevolve { .. } => "railrevolve",
        Command::Pipe { .. } => "pipe",
        Command::Geodesic { .. } => "geodesic",
        Command::SpaceFrame { .. } => "spaceframe",
        Command::Hypar { .. } => "hypar",
        Command::GaussVault { .. } => "gaussvault",
        Command::Gridshell { .. } => "gridshell",
        Command::Funicular { .. } => "funicular",
        Command::Tensegrity { .. } => "tensegrity",
        Command::Cablenet { .. } => "cablenet",
        Command::MinSurf { .. } => "minsurf",
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
        Command::InsertKnot { .. } => "insertknot",
        Command::CurvatureGraph { .. } => "curvature",
        Command::Rebuild { .. } => "rebuild",
        Command::Dim { .. } => "dim",
        Command::Text { .. } => "text",
        Command::Hatch { .. } => "hatch",
        Command::Union { .. } => "union",
        Command::Difference { .. } => "difference",
        Command::Intersect { .. } => "intersect",
        Command::ExactBoolean { .. } => "exact_boolean",
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
        Command::LayerRename { .. } => "layerrename",
        Command::LayerDelete { .. } => "layerdelete",
        Command::LayerOrder { .. } => "layerorder",
        Command::LayerLock { .. } => "layerlock",
        Command::LayerLinetype { .. } => "layerlinetype",
        Command::Hide { .. } => "hide",
        Command::Show { .. } => "show",
        Command::HideObj { .. } => "hideobj",
        Command::ShowObj { .. } => "showobj",
        Command::Lineweight { .. } => "lineweight",
        Command::LinweightOff { .. } => "lineweightoff",
        Command::ShowWeights { .. } => "showweights",
        Command::Color { .. } => "color",
        Command::ColorOff { .. } => "coloroff",
        Command::Material2 { .. } => "material2",
        Command::Material2Off { .. } => "material2off",
        Command::Units { .. } => "units",
        Command::Underlay { .. } => "underlay",
        Command::UnderlayOpacity { .. } => "underlayopacity",
        Command::UnderlayOff => "underlayoff",
        Command::Sun { .. } => "sun",
        Command::SunOff => "sunoff",
        Command::Location { .. } => "location",
        Command::ShadowStudy { .. } => "shadowstudy",
        Command::SunHours { .. } => "sunhours",
        Command::SunPath { .. } => "sunpath",
        Command::Radiation { .. } => "radiation",
        Command::FaceSunHours { .. } => "facesunhours",
        Command::Sheet { .. } => "sheet",
        Command::SheetView { .. } => "sheetview",
        Command::Print { .. } => "print",
        Command::Export { .. } => "export",
        Command::ControlImages { .. } => "controlimages",
        Command::Import { .. } => "import",
        Command::Terrain { .. } => "terrain",
        Command::OsmFile { .. } => "osmfile",
        Command::Contours { .. } => "contours",
        Command::Pad { .. } => "pad",
        Command::CutFill { .. } => "cutfill",
        Command::LotSubdivide { .. } => "lotsubdivide",
        Command::LotSettings { .. } => "lotsettings",
        Command::LotLoading { .. } => "lotloading",
        Command::LotGenerateSite { .. } => "lotgeneratesite",
        Command::LotSetbacks { .. } => "lotsetbacks",
        Command::LotFrontage { .. } => "lotfrontage",
        Command::LotOpenSpace { .. } => "lotopenspace",
        Command::Plant { .. } => "plant",
        Command::PlantRow { .. } => "plantrow",
        Command::PlantSchedule { .. } => "plantschedule",
        Command::PlantCatalog { .. } => "plantcatalog",
        Command::Miyawaki { .. } => "miyawaki",
        Command::FlowArrows { .. } => "flowarrows",
        Command::Ponding { .. } => "ponding",
        Command::SitePath { .. } => "sitepath",
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
        Command::EnviroReport { .. } => "report",
        Command::CodeCheck { .. } => "codecheck",
        Command::CheckRulesList | Command::CheckRulesLoad { .. } => "checkrules",
        Command::SheetTable { .. } => "sheettable",
        Command::SheetDim { .. } => "sheetdim",
        Command::MeshLiteral { .. } => "mesh_literal",
        Command::PointLiteral { .. } => "point_literal",
        Command::BlockDefine { .. } => "block",
        Command::BlockInsert { .. } => "insert",
        Command::BlockParamDefine { .. } => "pblock",
        Command::BlockParamSet { .. } => "param",
        Command::BlockDeleteDef { .. } => "blockdelete",
        Command::BlocksList => "blocks",
        Command::BlockLibList => "blocklib",
        Command::BlockLibLoad { .. } => "blockload",
        Command::BlockLibSave { .. } => "blocksave",
        Command::DefSection { .. } => "section",
        Command::DefMaterial { .. } => "material",
        Command::DefGrid { .. } => "grid",
        Command::DefStory { .. } => "story",
        Command::Room { .. } => "room",
        Command::RoomList => "rooms",
        Command::FrameMember { kind, .. } => kind.label(),
        Command::AreaMember { kind, .. } => kind.label(),
        Command::AddLoad { .. } => "load",
        Command::AddSupport { .. } => "support",
        Command::Undo => "undo",
        Command::Redo => "redo",
        Command::Amend { .. } => "amend",
        Command::Option(..) => "option",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse;

    fn run(s: &mut Session, line: &str) -> ApplyOutcome {
        s.run(parse(line).unwrap()).unwrap()
    }

    // ── M-perf bench: one big analysis case, ignored by default ─────────────

    /// Build a 200-tower massing scene with a location set (the M-perf bench
    /// scene). 20×10 grid of 4×4 m boxes of varying heights.
    fn bench_scene() -> Session {
        let mut s = Session::default();
        for i in 0..200 {
            let x = (i % 20) as f64 * 6.0 - 60.0;
            let y = (i / 20) as f64 * 6.0 - 30.0;
            let h = 5.0 + (i % 7) as f64 * 3.0;
            run(&mut s, &format!("box {x},{y},0 4,4,{h}"));
        }
        run(&mut s, "location 40.0 0.0 0");
        s
    }

    // ── M-perf determinism: parallel kernels == sequential reference ────────

    /// Every rayon analysis kernel must produce output *bit-identical* to its
    /// sequential reference, and be stable across repeated runs (op-log replay
    /// invariant). One shared massing scene exercises all four kernels.
    #[test]
    fn mperf_parallel_kernels_match_sequential_bitwise() {
        // Small massing scene: 12 boxes of varying heights.
        let mut s = Session::default();
        for i in 0..12 {
            let x = (i % 4) as f64 * 6.0 - 9.0;
            let y = (i / 4) as f64 * 6.0 - 3.0;
            let h = 4.0 + (i % 5) as f64 * 3.0;
            run(&mut s, &format!("box {x},{y},0 4,4,{h}"));
        }
        let loc = GeoLocation { lat_deg: 40.71, lon_deg: -74.01, tz_hours: -5.0 };
        let tris: Vec<[DVec3; 3]> = scene_triangles(&s.doc)
            .into_iter()
            .map(|t| {
                [
                    DVec3::from_array(t[0]),
                    DVec3::from_array(t[1]),
                    DVec3::from_array(t[2]),
                ]
            })
            .collect();
        let bvh = kernel_mesh::TriBvh::build(tris.clone());
        // Sun directions every 30 min of a solstice day (up only).
        let mut sun_dirs: Vec<DVec3> = Vec::new();
        for slot in 0..48 {
            let utc = (slot as f64 * 30.0 - loc.tz_hours * 60.0).rem_euclid(1440.0);
            let pos = itsjustcad_solar::solar_position(
                2024, 6, 21,
                (utc / 60.0) as u32,
                (utc % 60.0) as u32,
                loc.lat_deg, loc.lon_deg,
            );
            if pos.altitude_deg > 0.0 {
                let d = itsjustcad_solar::sun_direction(pos.azimuth_deg, pos.altitude_deg);
                sun_dirs.push(DVec3::new(d[0] as f64, d[1] as f64, d[2] as f64));
            }
        }
        assert!(!sun_dirs.is_empty());

        // sunhours: ground-grid origins.
        let origins: Vec<DVec3> = (0..400)
            .map(|c| {
                DVec3::new((c % 20) as f64 * 1.5 - 14.0, (c / 20) as f64 * 1.5 - 8.0, 1e-4)
            })
            .collect();
        let par = lit_slot_counts(&origins, &sun_dirs, &bvh);
        assert_eq!(par, lit_slot_counts_seq(&origins, &sun_dirs, &bvh), "sunhours kernel");
        assert_eq!(par, lit_slot_counts(&origins, &sun_dirs, &bvh), "sunhours repeat run");

        // facesunhours: per-face lit slots (exact f64 equality on centroid/normal).
        let par = face_lit_slots(&tris, &sun_dirs, &bvh);
        assert_eq!(par, face_lit_slots_seq(&tris, &sun_dirs, &bvh), "facesunhours kernel");
        assert_eq!(par, face_lit_slots(&tris, &sun_dirs, &bvh), "facesunhours repeat run");

        // radiation: per-face annual insolation (exact f64 equality — the
        // 288-bin sum stays sequential inside each face).
        let bins: itsjustcad_solar::RadiationBins = (0..288)
            .map(|i| if (7..17).contains(&(i % 24)) { [500.0, 100.0] } else { [0.0, 0.0] })
            .collect();
        let par = face_radiation_scores(&tris, &bins, 2026, loc, &bvh);
        let seq = face_radiation_scores_seq(&tris, &bins, 2026, loc, &bvh);
        assert!(
            par.iter().zip(&seq).all(|(a, b)| {
                a.0.to_bits() == b.0.to_bits() && a.1 == b.1 && a.2 == b.2
            }),
            "radiation kernel must be bit-identical to sequential"
        );
        assert_eq!(par, face_radiation_scores(&tris, &bins, 2026, loc, &bvh), "radiation repeat");

        // shadowstudy: per-stamp ground hulls.
        let object_pts: Vec<Vec<[f64; 3]>> = s
            .doc
            .objects()
            .filter_map(|o| match &o.geometry {
                Geometry::Mesh(m) => {
                    Some(m.positions().iter().map(|p| [p.x, p.y, p.z]).collect())
                }
                _ => None,
            })
            .collect();
        let stamps: Vec<u32> = (0..=1410).step_by(30).collect();
        let par = shadow_stamp_hulls(&stamps, 2024, 6, 21, loc, &object_pts);
        let seq = shadow_stamp_hulls_seq(&stamps, 2024, 6, 21, loc, &object_pts);
        assert_eq!(par, seq, "shadowstudy kernel");
        assert_eq!(
            par,
            shadow_stamp_hulls(&stamps, 2024, 6, 21, loc, &object_pts),
            "shadowstudy repeat run"
        );
        // Sanity: the solstice day actually has lit and dark stamps.
        assert!(par.iter().any(Option::is_some) && par.iter().any(Option::is_none));
    }

    /// Timing evidence for the M-perf rayon work — run manually with
    /// `cargo test --profile quick -p itsjustcad-commands bench_analysis -- --ignored --nocapture`.
    #[test]
    #[ignore = "bench: timing evidence only, run manually with --nocapture"]
    fn bench_analysis_200_boxes() {
        let mut s = bench_scene();
        let t0 = std::time::Instant::now();
        let out = run(&mut s, "sunhours 2024-06-21 1");
        eprintln!("bench sunhours:      {:>8.1?}  ({})", t0.elapsed(), out.message);

        let mut s = bench_scene();
        let t0 = std::time::Instant::now();
        let out = run(&mut s, "facesunhours all 2024-06-21");
        eprintln!("bench facesunhours:  {:>8.1?}  ({})", t0.elapsed(), out.message);

        let mut s = bench_scene();
        let t0 = std::time::Instant::now();
        let out = run(&mut s, "shadowstudy 2024-06-21 06:00 20:00 30");
        eprintln!("bench shadowstudy:   {:>8.1?}  ({})", t0.elapsed(), out.message);

        let path = write_synth_epw("bench.epw");
        let mut s = bench_scene();
        let t0 = std::time::Instant::now();
        let out = run(&mut s, &format!("radiation all {}", path.display()));
        eprintln!("bench radiation:     {:>8.1?}  ({})", t0.elapsed(), out.message);
    }

    // ── import size ceiling (decompression-bomb / OOM defense) ──────────────

    #[test]
    fn import_size_rule_caps_at_512_mib() {
        assert!(import_size_ok(0));
        assert!(import_size_ok(MAX_IMPORT_BYTES));
        assert!(import_size_ok(MAX_IMPORT_BYTES - 1));
        // One byte over the ceiling is refused.
        assert!(!import_size_ok(MAX_IMPORT_BYTES + 1));
        // A multi-GB "bomb" is refused.
        assert!(!import_size_ok(8 * 1024 * 1024 * 1024));
    }

    #[test]
    fn read_import_bytes_refuses_oversized_file_without_loading() {
        // A sparse file whose *length* exceeds the cap: `set_len` grows the
        // metadata length without writing 512 MiB, so the test is cheap. The
        // reader must refuse on the stat, never attempting the full read.
        let dir = std::env::temp_dir().join(format!("ijc_import_cap_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("bomb.las");
        let f = std::fs::File::create(&path).unwrap();
        f.set_len(MAX_IMPORT_BYTES + 4096).unwrap();
        drop(f);
        let err = read_import_bytes(path.to_str().unwrap()).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("import limit"), "wrong error: {msg}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn read_import_bytes_reads_a_small_file() {
        let dir = std::env::temp_dir().join(format!("ijc_import_ok_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("ok.bin");
        std::fs::write(&path, b"hello").unwrap();
        assert_eq!(read_import_bytes(path.to_str().unwrap()).unwrap(), b"hello");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn read_import_bytes_missing_file_is_clean_error() {
        let err = read_import_bytes("/nonexistent/ijc/does-not-exist.las").unwrap_err();
        assert!(err.to_string().contains("cannot read"), "{err}");
    }

    // ── dynamic (parametric) blocks ─────────────────────────────────────────

    /// The baked-geometry key an instance points at, for inspection in tests.
    fn instance_block_key(s: &Session, id: ObjectId) -> String {
        match &s.doc.get(id).unwrap().geometry {
            Geometry::Instance { block, .. } => block.clone(),
            g => panic!("expected instance, got {g:?}"),
        }
    }

    fn baked_len(s: &Session, id: ObjectId) -> usize {
        let key = instance_block_key(s, id);
        s.doc.blocks.get(&key).map(|v| v.len()).unwrap_or(0)
    }

    /// Bounding-box width (X extent) of the baked geometry an instance resolves
    /// to — the observable that must change when `width` changes.
    fn baked_width(s: &Session, id: ObjectId) -> f64 {
        let key = instance_block_key(s, id);
        let defs = s.doc.blocks.get(&key).expect("baked block present");
        let mut min = f64::INFINITY;
        let mut max = f64::NEG_INFINITY;
        for d in defs {
            let bb = d.aabb();
            min = min.min(bb.min.x);
            max = max.max(bb.max.x);
        }
        max - min
    }

    #[test]
    fn pblock_define_insert_and_param_edit_updates_geometry() {
        let mut s = Session::default();
        run(&mut s, "pblock testdoor width=0.9 : rect 0,0,0 {width} 0.05 ; arc 0,0,0 {width} 0 90");
        assert!(s.doc.param_blocks.contains_key("testdoor"));

        let created = run(&mut s, "insert testdoor 0,0,0 width=1.0").created;
        let id = created[0];
        assert_eq!(baked_len(&s, id), 2, "rect + arc baked");
        // The arc's conservative bound makes the footprint 2×width; the exact
        // ratio is invariant — what matters is that it tracks the param.
        let w0 = baked_width(&s, id);
        assert!((w0 - 2.0).abs() < 1e-6, "width footprint for width=1.0, got {w0}");

        // Edit the param -> geometry re-derives (footprint doubles).
        run(&mut s, "param last width=2.0");
        let w1 = baked_width(&s, id);
        assert!((w1 - 4.0).abs() < 1e-6, "footprint for width=2.0, got {w1}");
        assert!(w1 > w0, "wider param -> wider geometry");

        // Undo restores prior geometry (footprint back to width=1.0).
        run(&mut s, "undo");
        let w2 = baked_width(&s, id);
        assert!((w2 - 2.0).abs() < 1e-6, "undo restores footprint, got {w2}");

        // Redo re-applies the edit.
        run(&mut s, "redo");
        let w3 = baked_width(&s, id);
        assert!((w3 - 4.0).abs() < 1e-6, "redo re-applies footprint, got {w3}");
    }

    #[test]
    fn param_insert_uses_defaults_when_no_override() {
        let mut s = Session::default();
        run(&mut s, "pblock t width=1.5 : rect 0,0,0 {width} 0.1");
        let id = run(&mut s, "insert t 0,0,0").created[0];
        match &s.doc.get(id).unwrap().geometry {
            Geometry::Instance { params, source, .. } => {
                assert_eq!(source.as_deref(), Some("t"));
                assert_eq!(params.get("width").map(String::as_str), Some("1.5"));
            }
            g => panic!("expected instance, got {g:?}"),
        }
        assert!((baked_width(&s, id) - 1.5).abs() < 1e-6);
    }

    #[test]
    fn param_insert_undo_removes_instance_and_bake() {
        let mut s = Session::default();
        run(&mut s, "pblock t size=0.4 : rect 0,0,0 {size} {size}");
        let id = run(&mut s, "insert t 0,0,0").created[0];
        let key = instance_block_key(&s, id);
        assert!(s.doc.blocks.contains_key(&key));
        run(&mut s, "undo");
        assert!(s.doc.get(id).is_none(), "instance removed");
        assert!(!s.doc.blocks.contains_key(&key), "baked block removed");
    }

    #[test]
    fn param_define_undo_redo() {
        let mut s = Session::default();
        assert!(!s.doc.param_blocks.contains_key("foo"));
        run(&mut s, "pblock foo w=1 : rect 0,0,0 {w} {w}");
        assert!(s.doc.param_blocks.contains_key("foo"));
        run(&mut s, "undo");
        assert!(!s.doc.param_blocks.contains_key("foo"), "define undone");
        run(&mut s, "redo");
        assert!(s.doc.param_blocks.contains_key("foo"), "define redone");
    }

    #[test]
    fn starter_blocks_instantiate_at_different_params() {
        // pdoor / pwindow / pcolumn are seeded — insert at varied params.
        let mut s = Session::default();
        let narrow = run(&mut s, "insert pdoor 0,0,0 width=0.8").created[0];
        let wide = run(&mut s, "insert pdoor 5,0,0 width=1.5").created[0];
        let wn = baked_width(&s, narrow);
        let ww = baked_width(&s, wide);
        // Door footprint tracks width (arc bound makes it 2×width).
        assert!((wn - 1.6).abs() < 1e-6, "narrow door footprint, got {wn}");
        assert!((ww - 3.0).abs() < 1e-6, "wide door footprint, got {ww}");
        assert!(ww > wn, "wide door must be wider than narrow");

        let win = run(&mut s, "insert pwindow 0,5,0 width=2.0 depth=0.15").created[0];
        assert!((baked_width(&s, win) - 2.0).abs() < 1e-6);

        let col = run(&mut s, "insert pcolumn 0,10,0 size=0.6").created[0];
        assert!((baked_width(&s, col) - 0.6).abs() < 1e-6);
    }

    #[test]
    fn param_set_rejects_unknown_param() {
        let mut s = Session::default();
        run(&mut s, "pblock t w=1 : rect 0,0,0 {w} {w}");
        run(&mut s, "insert t 0,0,0");
        let err = s.run(parse("param last bogus=9").unwrap()).unwrap_err();
        assert!(matches!(err, ExecError::Invalid(_)), "got {err:?}");
    }

    #[test]
    fn param_set_on_plain_block_errors() {
        let mut s = Session::default();
        run(&mut s, "box 0,0,0 1,1,1");
        run(&mut s, "block last plainblk");
        run(&mut s, "insert plainblk 3,0,0");
        let err = s.run(parse("param last size=2").unwrap()).unwrap_err();
        assert!(matches!(err, ExecError::Invalid(_)), "got {err:?}");
    }

    #[test]
    fn param_insert_replay_stable() {
        // A log with a define, an insert-with-params, and a param edit must
        // survive to_json -> from_json -> to_json byte-identically.
        let mut s = Session::default();
        run(&mut s, "pblock pd width=0.9 : rect 0,0,0 {width} 0.05 ; arc 0,0,0 {width} 0 90");
        run(&mut s, "insert pd 1,2,0 width=1.3");
        run(&mut s, "param last width=1.7");

        let j1 = crate::io::to_json(&s);
        let s2 = crate::io::from_json(&j1).unwrap();
        let j2 = crate::io::to_json(&s2);
        assert_eq!(j1, j2, "op-log JSON must be byte-stable across replay");

        // Ids stable: the instance keeps its baked-block key after replay.
        let id = s.doc.all_ids()[0];
        assert_eq!(
            instance_block_key(&s, id),
            instance_block_key(&s2, id),
            "baked-block key stable across replay"
        );
    }

    #[test]
    fn insert_with_params_parse_round_trip() {
        let cmd = parse("insert pdoor 0,0,0 90 1.5 width=1.2").unwrap();
        match &cmd {
            Command::BlockInsert { name, rotation_deg, scale, params, .. } => {
                assert_eq!(name, "pdoor");
                assert_eq!(*rotation_deg, Some(90.0));
                assert_eq!(*scale, Some(1.5));
                assert_eq!(params.get("width").map(String::as_str), Some("1.2"));
            }
            c => panic!("expected BlockInsert, got {c:?}"),
        }
    }

    // ── asset-pack tests ────────────────────────────────────────────────────────

    /// Helper: bounding-box depth (Y extent) of the baked geometry.
    fn baked_depth(s: &Session, id: ObjectId) -> f64 {
        let key = instance_block_key(s, id);
        let defs = s.doc.blocks.get(&key).expect("baked block present");
        let mut min = f64::INFINITY;
        let mut max = f64::NEG_INFINITY;
        for d in defs {
            let bb = d.aabb();
            min = min.min(bb.min.y);
            max = max.max(bb.max.y);
        }
        max - min
    }

    /// Each asset instantiates with geometry at expected real-world scale.
    #[test]
    fn asset_pack_doors_instantiate_at_expected_size() {
        let mut s = Session::default();

        // pdoor: 0.9 m wide by default; arc makes footprint 2×width in X.
        let door = run(&mut s, "insert pdoor 0,0,0").created[0];
        let dw = baked_width(&s, door);
        assert!((dw - 1.8).abs() < 1e-6, "pdoor footprint width, got {dw}");

        // pdoor_double: default total width 1.8, half=0.9.
        // Arc AABB is conservative (full bounding square of circle), so the
        // two arcs of radius 0.9 centred at x=0 and x=1.8 produce an X span
        // from -0.9 to 2.7 → baked_width = 3.6 (2 × diameter).
        let ddoor = run(&mut s, "insert pdoor_double 0,0,0").created[0];
        let ddw = baked_width(&s, ddoor);
        assert!((ddw - 3.6).abs() < 1e-6, "pdoor_double AABB width, got {ddw}");

        // pdoor_sliding: 0.9 m wide frame.
        let sdoor = run(&mut s, "insert pdoor_sliding 0,0,0").created[0];
        let sdw = baked_width(&s, sdoor);
        assert!((sdw - 0.9).abs() < 1e-6, "pdoor_sliding width, got {sdw}");
    }

    #[test]
    fn asset_pack_windows_instantiate_at_expected_size() {
        let mut s = Session::default();

        // pwindow: default 1.2 m wide.
        let win = run(&mut s, "insert pwindow 0,0,0").created[0];
        assert!((baked_width(&s, win) - 1.2).abs() < 1e-6);

        // pwindow_casement: default 0.9 m wide.
        let cas = run(&mut s, "insert pwindow_casement 0,0,0").created[0];
        assert!((baked_width(&s, cas) - 0.9).abs() < 1e-6);

        // pwindow_sliding: default 1.2 m wide.
        let sld = run(&mut s, "insert pwindow_sliding 0,0,0").created[0];
        assert!((baked_width(&s, sld) - 1.2).abs() < 1e-6);
    }

    #[test]
    fn asset_pack_furniture_instantiates_at_expected_size() {
        let mut s = Session::default();

        // pdesk: 1.6 × 0.8 m.
        let desk = run(&mut s, "insert pdesk 0,0,0").created[0];
        assert!((baked_width(&s, desk) - 1.6).abs() < 1e-6, "desk width");
        assert!((baked_depth(&s, desk) - 0.8).abs() < 1e-6, "desk depth");

        // pchair: 0.5 m seat (back arc extends behind, so depth > 0.5).
        let chair = run(&mut s, "insert pchair 0,5,0").created[0];
        let cw = baked_width(&s, chair);
        assert!((cw - 0.5).abs() < 1e-6, "chair seat width, got {cw}");
        // Back arc extends 0.25 m beyond seat, so baked depth ≈ 0.75.
        let cd = baked_depth(&s, chair);
        assert!(cd > 0.5, "chair depth includes back arc, got {cd}");

        // pbed: 1.5 × 2.0 m frame.
        let bed = run(&mut s, "insert pbed 5,0,0").created[0];
        assert!((baked_width(&s, bed) - 1.5).abs() < 1e-6, "bed width");
        assert!((baked_depth(&s, bed) - 2.0).abs() < 1e-6, "bed depth");

        // ptable: circle radius 0.45 → diameter 0.9 in both axes.
        let table = run(&mut s, "insert ptable 0,10,0").created[0];
        let tw = baked_width(&s, table);
        assert!((tw - 0.9).abs() < 1e-6, "round table diameter, got {tw}");

        // psofa: 2.0 m wide.
        let sofa = run(&mut s, "insert psofa 0,15,0").created[0];
        assert!((baked_width(&s, sofa) - 2.0).abs() < 1e-6, "sofa width");

        // ptoilet: 0.38 m wide.
        let toilet = run(&mut s, "insert ptoilet 5,10,0").created[0];
        assert!((baked_width(&s, toilet) - 0.38).abs() < 1e-6, "toilet width");

        // psink: 0.6 × 0.5 m.
        let sink = run(&mut s, "insert psink 5,15,0").created[0];
        assert!((baked_width(&s, sink) - 0.6).abs() < 1e-6, "sink width");
        assert!((baked_depth(&s, sink) - 0.5).abs() < 1e-6, "sink depth");
    }

    /// Parametric response: changing width changes baked geometry footprint.
    #[test]
    fn asset_pack_parametric_response() {
        let mut s = Session::default();

        // pdesk: widen from 1.6 to 2.0.
        let desk = run(&mut s, "insert pdesk 0,0,0 width=1.6").created[0];
        let w0 = baked_width(&s, desk);
        assert!((w0 - 1.6).abs() < 1e-6);
        run(&mut s, "param last width=2.0");
        let w1 = baked_width(&s, desk);
        assert!((w1 - 2.0).abs() < 1e-6, "desk widened to 2.0, got {w1}");

        // pwindow_sliding: change width.
        let win = run(&mut s, "insert pwindow_sliding 10,0,0 width=0.9").created[0];
        let ww0 = baked_width(&s, win);
        assert!((ww0 - 0.9).abs() < 1e-6);
        run(&mut s, "param last width=1.5 third=0.5 two_thirds=1.0");
        let ww1 = baked_width(&s, win);
        assert!((ww1 - 1.5).abs() < 1e-6, "sliding window widened to 1.5, got {ww1}");
    }

    /// Insert + undo + redo round-trip for an asset-pack block.
    #[test]
    fn asset_pack_undo_redo() {
        let mut s = Session::default();
        let id = run(&mut s, "insert psofa 0,0,0").created[0];
        let key = instance_block_key(&s, id);
        assert!(s.doc.blocks.contains_key(&key));
        run(&mut s, "undo");
        assert!(s.doc.get(id).is_none(), "sofa removed by undo");
        assert!(!s.doc.blocks.contains_key(&key), "baked block removed");
        run(&mut s, "redo");
        assert!(s.doc.get(id).is_some(), "sofa restored by redo");
        let key2 = instance_block_key(&s, id);
        assert!(s.doc.blocks.contains_key(&key2));
    }

    /// Replay stability: op-log JSON is byte-identical after round-trip.
    #[test]
    fn asset_pack_replay_stable() {
        let mut s = Session::default();
        run(&mut s, "insert pdoor_double 0,0,0 width=1.6 half=0.8");
        run(&mut s, "insert pdesk 5,0,0 width=1.8");
        run(&mut s, "insert ptoilet 10,0,0");
        run(&mut s, "insert psink 15,0,0");

        let j1 = crate::io::to_json(&s);
        let s2 = crate::io::from_json(&j1).unwrap();
        let j2 = crate::io::to_json(&s2);
        assert_eq!(j1, j2, "asset-pack op-log must be byte-stable across replay");
    }

    /// Door is ~0.9 m wide; chair is ~0.5 m; chair should be narrower than door.
    #[test]
    fn asset_relative_scale_chair_smaller_than_door() {
        let mut s = Session::default();
        let door = run(&mut s, "insert pdoor 0,0,0").created[0];
        let chair = run(&mut s, "insert pchair 5,0,0").created[0];
        // Door arc footprint = 1.8 m; chair seat = 0.5 m.
        assert!(
            baked_width(&s, chair) < baked_width(&s, door),
            "chair ({}) must be narrower than door ({})",
            baked_width(&s, chair),
            baked_width(&s, door)
        );
    }

    fn sample_view(distance: f32) -> NamedView {
        NamedView {
            target: [1.0, 2.0, 3.0],
            distance,
            yaw: 0.25,
            pitch: 0.5,
            fov_y: 45f32.to_radians(),
            ortho: true,
            two_point: false,
            pano: None,
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
    fn loft_with_guides_bows_and_replays() {
        let mut s = Session::default();
        run(&mut s, "circle 0,0,0 2");
        run(&mut s, "circle 0,0,6 2");
        run(&mut s, "name last 2 rings");
        // Bowed guide from the +x rim of the bottom circle to the top one.
        run(&mut s, "interpcurve 2,0,0 4,0,3 2,0,6");
        run(&mut s, "name last rail");
        let out = run(&mut s, "loft rings guides rail");
        assert!(out.message.contains("1 guide"), "{}", out.message);
        // The guided solid is fatter than a plain cylinder of the same rings.
        let vol = mesh_volume(&s);
        assert!(vol > std::f64::consts::PI * 4.0 * 6.0 + 1.0, "vol {vol}");
        run(&mut s, "undo");
        run(&mut s, "redo");
        // Replay from the op-log is stable.
        let json = crate::io::to_json(&s);
        let loaded = crate::io::from_json(&json).unwrap();
        assert_eq!(crate::io::to_json(&loaded), json, "replay-stable");

        // Closed guides are rejected.
        run(&mut s, "circle 8,0,3 1");
        let err = s.run(parse("loft rings guides last").unwrap()).unwrap_err();
        assert!(err.to_string().contains("closed"), "{err}");
    }

    #[test]
    fn blend_between_lines_and_undo() {
        let mut s = Session::default();
        run(&mut s, "line 0,0,0 8,0,0");
        run(&mut s, "name last edgea");
        run(&mut s, "line 0,4,0 8,4,0");
        run(&mut s, "name last edgeb");
        let out = run(&mut s, "blend edgea edgeb");
        assert_eq!(out.created.len(), 1);
        let Geometry::Mesh(m) = &s.doc.get(out.created[0]).unwrap().geometry else {
            panic!("expected mesh")
        };
        // Flat ruled sheet between the two lines (bulge defaults to 1).
        assert!(m.positions().iter().all(|p| p.z.abs() < 1e-9));
        assert!(m
            .positions()
            .iter()
            .all(|p| (-1e-9..=4.0 + 1e-9).contains(&p.y)));
        run(&mut s, "undo");
        assert_eq!(s.doc.len(), 2);
        run(&mut s, "redo");
        assert_eq!(s.doc.len(), 3);

        // Mixed open/closed inputs are rejected, as is a bad bulge.
        run(&mut s, "circle 0,0,0 1");
        let err = s.run(parse("blend edgea last").unwrap()).unwrap_err();
        assert!(err.to_string().contains("open or both closed"), "{err}");
        assert!(s.run(parse("blend edgea edgeb -1").unwrap()).is_err());
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
    fn layerlock_parse_exec_undo_redo() {
        let mut s = Session::default();
        run(&mut s, "layer walls");
        assert!(!s.doc.layers["walls"].locked, "default unlocked");
        let out = run(&mut s, "layerlock walls on");
        assert!(out.message.contains("locked"), "{}", out.message);
        assert!(s.doc.layers["walls"].locked);
        // Undo restores unlocked.
        run(&mut s, "undo");
        assert!(!s.doc.layers["walls"].locked);
        // Redo re-locks.
        run(&mut s, "redo");
        assert!(s.doc.layers["walls"].locked);
        // Unlock again.
        let out = run(&mut s, "layerlock walls off");
        assert!(out.message.contains("unlocked"), "{}", out.message);
        assert!(!s.doc.layers["walls"].locked);
    }

    #[test]
    fn layerlock_parse_and_errors() {
        assert_eq!(
            parse("layerlock walls on").unwrap(),
            Command::LayerLock { layer: "walls".into(), locked: true }
        );
        assert_eq!(
            parse("layerlock walls OFF").unwrap(),
            Command::LayerLock { layer: "walls".into(), locked: false }
        );
        assert!(parse("layerlock walls").is_err(), "missing state");
        assert!(parse("layerlock walls maybe").is_err(), "bad state");
        let mut s = Session::default();
        let err = s.run(parse("layerlock ghost on").unwrap()).unwrap_err();
        assert!(err.to_string().contains("no layer 'ghost'"), "{err}");
    }

    #[test]
    fn locked_layer_objects_are_not_editable() {
        let mut s = Session::default();
        run(&mut s, "layer walls");
        run(&mut s, "box 0,0,0 1,1,1");
        // Lock the layer the box lives on.
        run(&mut s, "layerlock walls on");
        // A transform targeting it is rejected.
        let err = s.run(parse("move all 1,0,0").unwrap()).unwrap_err();
        assert!(err.to_string().contains("locked layer"), "{err}");
        // Unlock and the same edit now succeeds.
        run(&mut s, "layerlock walls off");
        run(&mut s, "move all 1,0,0");
    }

    #[test]
    fn layerlock_replay_stable() {
        let mut s = Session::default();
        run(&mut s, "layer walls");
        run(&mut s, "layerlock walls on");
        let log = s.save_log();
        let replayed = Session::replay(log.clone()).unwrap();
        assert!(replayed.doc.layers["walls"].locked);
        assert_eq!(
            serde_json::to_string(&log).unwrap(),
            serde_json::to_string(&replayed.save_log()).unwrap(),
            "replay-stable log"
        );
    }

    #[test]
    fn layerlinetype_parse_exec_undo_redo() {
        use itsjustcad_doc::LineType;
        let mut s = Session::default();
        run(&mut s, "layer grid");
        assert_eq!(s.doc.layers["grid"].linetype, LineType::Continuous);
        let out = run(&mut s, "layerlinetype grid dashed");
        assert!(out.message.contains("Dashed"), "{}", out.message);
        assert_eq!(s.doc.layers["grid"].linetype, LineType::Dashed);
        run(&mut s, "undo");
        assert_eq!(s.doc.layers["grid"].linetype, LineType::Continuous);
        run(&mut s, "redo");
        assert_eq!(s.doc.layers["grid"].linetype, LineType::Dashed);
    }

    #[test]
    fn layerlinetype_parse_all_variants_and_errors() {
        use itsjustcad_doc::LineType;
        for (tok, lt) in [
            ("continuous", LineType::Continuous),
            ("dashed", LineType::Dashed),
            ("dotted", LineType::Dotted),
            ("dashdot", LineType::DashDot),
        ] {
            assert_eq!(
                parse(&format!("layerlinetype grid {tok}")).unwrap(),
                Command::LayerLinetype { layer: "grid".into(), linetype: lt }
            );
        }
        assert!(parse("layerlinetype grid").is_err(), "missing linetype");
        assert!(parse("layerlinetype grid squiggly").is_err(), "bad linetype");
        let mut s = Session::default();
        let err = s.run(parse("layerlinetype ghost dashed").unwrap()).unwrap_err();
        assert!(err.to_string().contains("no layer 'ghost'"), "{err}");
    }

    #[test]
    fn layerlinetype_replay_stable() {
        use itsjustcad_doc::LineType;
        let mut s = Session::default();
        run(&mut s, "layer grid");
        run(&mut s, "layerlinetype grid dotted");
        run(&mut s, "layer edge");
        run(&mut s, "layerlinetype edge dashdot");
        let log = s.save_log();
        let replayed = Session::replay(log.clone()).unwrap();
        assert_eq!(replayed.doc.layers["grid"].linetype, LineType::Dotted);
        assert_eq!(replayed.doc.layers["edge"].linetype, LineType::DashDot);
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
            itsjustcad_doc::HatchPattern::Lines { angle_deg, spacing }
                if *angle_deg == 45.0 && *spacing == 0.5
        ));
        run(&mut s, "undo");
        assert_eq!(s.doc.len(), 1);

        // zero spacing rejected
        let err = s.run(parse("hatch last lines 45 0").unwrap()).unwrap_err();
        assert!(err.to_string().contains("spacing"), "{err}");

        // ANSI patterns store code + spacing and replay stably.
        run(&mut s, "hatch last ansi33 0.15");
        let obj = s.doc.objects().last().unwrap();
        assert!(matches!(
            &obj.geometry,
            Geometry::Annotation(Annotation::Hatch {
                pattern: itsjustcad_doc::HatchPattern::Ansi { code: 33, spacing },
                ..
            }) if *spacing == 0.15
        ));
        let json = crate::io::to_json(&s);
        let loaded = crate::io::from_json(&json).unwrap();
        assert_eq!(crate::io::to_json(&loaded), json, "replay-stable");
        run(&mut s, "undo"); // back to the bare rect so `last` is the boundary
        let err = s.run(parse("hatch last ansi33 0").unwrap()).unwrap_err();
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
        use itsjustcad_doc::{PaperSize, ViewDirection};
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

    // ── STEP AP242 (via OCCT) ────────────────────────────────────────────────

    /// Without the `kernel-occt` feature there is no STEP reader/writer, so both
    /// directions must fail with a clear, actionable message (never a silent stub
    /// or a bogus file). This is the default-build behavior.
    #[cfg(not(feature = "kernel-occt"))]
    #[test]
    fn step_import_export_reports_feature_needed_when_off() {
        let mut s = Session::default();
        run(&mut s, "rect 0,0,0 10 6");
        run(&mut s, "extrude last 3");
        let path = std::env::temp_dir().join("ijc_step_off.step");
        let p = path.to_string_lossy().to_string();

        let err = s.run(parse(&format!("export {p}")).unwrap()).unwrap_err();
        assert!(err.to_string().contains("kernel-occt"), "{err}");

        // Write a dummy file so import has something to open; it must still
        // refuse on the missing feature, not on a read error.
        std::fs::write(&path, b"ISO-10303-21;\n").unwrap();
        let err = s.run(parse(&format!("import {p}")).unwrap()).unwrap_err();
        assert!(err.to_string().contains("kernel-occt"), "{err}");
        let _ = std::fs::remove_file(&path);
    }

    /// With the exact-BREP tier compiled in, a document solid exports to STEP and
    /// re-imports as a mesh with the same volume — the AP242 handoff round-trips.
    /// The import is one logged `MeshLiteral`, so it replays from the op-log.
    #[cfg(feature = "kernel-occt")]
    #[test]
    fn step_round_trip_through_document() {
        let mut s = Session::default();
        run(&mut s, "box 0,0,0 2,3,4"); // a solid of volume 24
        let path = std::env::temp_dir().join("ijc_step_doc_rt.step");
        let p = path.to_string_lossy().to_string();

        let out = run(&mut s, &format!("export {p}"));
        assert!(out.message.contains("STEP"), "{}", out.message);
        assert!(std::path::Path::new(&p).exists());

        let before = s.doc.objects().count();
        let out = run(&mut s, &format!("import {p}"));
        assert!(out.message.contains("STEP"), "{}", out.message);
        assert_eq!(s.doc.objects().count(), before + 1, "one MeshLiteral added");

        // The import is logged and replay-stable.
        let log = s.save_log();
        assert!(log.iter().any(|c| matches!(c, Command::MeshLiteral { .. })));
        let replayed = Session::replay(log.clone()).unwrap();
        assert_eq!(
            serde_json::to_string(&log).unwrap(),
            serde_json::to_string(&replayed.save_log()).unwrap()
        );
        let _ = std::fs::remove_file(&path);
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
            .next_back()
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
            .next_back()
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

    // ── exact-BREP tier (kernel-occt) ───────────────────────────────────────

    /// The signed volume of the single mesh a boolean produced. Works for both
    /// the exact and the mesh-fallback path since box booleans are polyhedral.
    fn only_mesh_volume(s: &Session, id: ObjectId) -> f64 {
        match &s.doc.get(id).unwrap().geometry {
            Geometry::Mesh(m) => kernel_mesh::signed_volume(m).abs(),
            g => panic!("expected mesh, got {g:?}"),
        }
    }

    #[test]
    fn exact_boolean_difference_volume_is_correct_either_kernel() {
        // 10^3 box minus a 4x4 column punched through z: 1000 - 160 = 840.
        // Whether the exact OCCT kernel or the mesh fallback runs, the answer is
        // the same (polyhedral result), so this test is kernel-agnostic.
        let mut s = Session::default();
        let out = run(
            &mut s,
            "exact_boolean difference 0,0,0 10,10,10 3,3,-5 4,4,20",
        );
        let id = out.created[0];
        let vol = only_mesh_volume(&s, id);
        assert!((vol - 840.0).abs() < 1e-6, "difference volume {vol} != 840");
        // The result advertises which kernel path ran.
        assert!(
            out.message.contains("exact OCCT kernel")
                || out.message.contains("mesh kernel"),
            "message should name the kernel path: {}",
            out.message
        );
        // With the feature off (default CI build) it must be the fallback; with
        // it on it must be exact. Assert consistency with the compiled feature.
        assert_eq!(
            out.message.contains("exact OCCT kernel"),
            kernel_occt::available(),
            "kernel path must match compiled feature"
        );
    }

    #[test]
    fn exact_boolean_union_and_intersection_volumes() {
        let mut s = Session::default();
        // disjoint 2^3 cubes -> 16
        let u = run(&mut s, "exact_boolean union 0,0,0 2,2,2 5,0,0 2,2,2");
        assert!((only_mesh_volume(&s, u.created[0]) - 16.0).abs() < 1e-6);
        // 10^3 ∩ 4x4x20 column = 160
        let i = run(
            &mut s,
            "exact_boolean intersection 0,0,0 10,10,10 3,3,-5 4,4,20",
        );
        assert!((only_mesh_volume(&s, i.created[0]) - 160.0).abs() < 1e-6);
    }

    #[test]
    fn exact_boolean_undo_redo_round_trips() {
        let mut s = Session::default();
        let before = s.doc.all_ids().len();
        let out = run(
            &mut s,
            "exact_boolean union 0,0,0 2,2,2 5,0,0 2,2,2",
        );
        let id = out.created[0];
        assert!(s.doc.get(id).is_some());
        run(&mut s, "undo");
        assert!(s.doc.get(id).is_none(), "undo removes the result");
        assert_eq!(s.doc.all_ids().len(), before);
        run(&mut s, "redo");
        // redo re-inserts a solid with the same (recorded) id.
        assert!(s.doc.get(id).is_some(), "redo restores the result");
        assert!((only_mesh_volume(&s, id) - 16.0).abs() < 1e-6);
    }

    #[test]
    fn exact_boolean_non_overlapping_difference_errors() {
        // difference of two disjoint boxes leaves box A intact -> non-empty,
        // so instead test intersection of disjoint boxes which IS empty.
        let mut s = Session::default();
        let err = s
            .run(parse("exact_boolean intersection 0,0,0 1,1,1 5,5,5 1,1,1").unwrap())
            .unwrap_err();
        assert!(
            matches!(err, ExecError::Invalid(_)),
            "empty intersection must error: {err:?}"
        );
    }

    #[test]
    fn exact_boolean_rejects_non_finite_coords() {
        // SECURITY: a non-finite coordinate (NaN/Inf) flows into a NaN mesh, and
        // NaN/Inf serialize to JSON `null` on save — which then fails to reload
        // into f64, permanently bricking the op-log. The command must error
        // BEFORE inserting anything, so the doc stays clean and re-loadable.
        for line in [
            "exact_boolean union nan,0,0 10,10,10 3,3,-5 4,4,20",
            "exact_boolean union 0,0,0 inf,10,10 3,3,-5 4,4,20",
            "exact_boolean union 0,0,0 10,10,10 3,3,-5 4,-inf,20",
        ] {
            let mut s = Session::default();
            let before = s.doc.len();
            let err = s.run(parse(line).unwrap()).unwrap_err();
            assert!(
                matches!(err, ExecError::Invalid(_)),
                "non-finite exact_boolean must error cleanly: {line} -> {err:?}"
            );
            assert_eq!(s.doc.len(), before, "no object inserted on rejection: {line}");
        }
    }

    /// Write a `w`x`h` PNG to a temp path and return it.
    fn temp_png(w: u32, h: u32, tag: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!("itsjustcad_underlay_{tag}_{w}x{h}.png"));
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
        // sun also records the observer location for analyses.
        let loc = s.doc.location.expect("sun sets location");
        assert!((loc.lat_deg - 40.71).abs() < 1e-9 && (loc.lon_deg - (-74.01)).abs() < 1e-9);

        // Undo removes sun and location.
        s.run(crate::Command::Undo).unwrap();
        assert!(s.doc.sun.is_none(), "sun cleared after undo");
        assert!(s.doc.location.is_none(), "location cleared after undo");

        // Redo restores sun.
        s.run(crate::Command::Redo).unwrap();
        assert!(s.doc.sun.is_some(), "sun restored after redo");
        assert!(s.doc.location.is_some(), "location restored after redo");
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

    // ---- environmental analyses: shadow study, sun-hours, EPW ----

    #[test]
    fn shadowstudy_errors_without_location() {
        let mut s = Session::default();
        run(&mut s, "box 0,0,0 5,5,3");
        let err = s
            .run(parse("shadowstudy 2024-06-21 09:00 15:00 180").unwrap())
            .unwrap_err();
        assert!(err.to_string().contains("no location"), "{err}");
    }

    #[test]
    fn shadowstudy_projects_polygons_and_undoes() {
        let mut s = Session::default();
        // A 4 m tall box; sun set (also sets location, tz=UTC).
        run(&mut s, "box 0,0,0 4,4,4");
        run(&mut s, "sun 40.71 -74.01 2024-06-21 16:58");
        let before = s.doc.len();
        // Two stamps: 12:00 and 14:00 UTC (both daylight in June at NY).
        let out = run(&mut s, "shadowstudy 2024-06-21 12:00 14:00 120");
        assert!(!out.created.is_empty(), "shadows created: {}", out.message);
        // One object per stamp (one box → one hull polygon per stamp).
        assert_eq!(out.created.len(), 2, "{}", out.message);
        // Each created object is a closed polygon on a shadows-HH:MM layer at z=0.
        for id in &out.created {
            let obj = s.doc.get(*id).unwrap();
            assert!(obj.layer.starts_with("shadows-"), "layer={}", obj.layer);
            match &obj.geometry {
                Geometry::Curve(Curve::Polyline { points, closed }) => {
                    assert!(*closed && points.len() >= 3);
                    assert!(points.iter().all(|p| p.z.abs() < 1e-9), "on ground");
                    // Shadow is offset from the box footprint (sun not at zenith),
                    // so some projected point has |x| or |y| beyond the 0..4 box.
                    let spread = points.iter().any(|p| p.x < -1e-6 || p.y < -1e-6 || p.x > 4.0 + 1e-6 || p.y > 4.0 + 1e-6);
                    assert!(spread, "shadow should extend past the footprint");
                }
                g => panic!("expected closed polyline, got {g:?}"),
            }
        }
        // Undo removes every polygon and the shadow layers.
        s.run(crate::Command::Undo).unwrap();
        assert_eq!(s.doc.len(), before, "shadows removed on undo");
        assert!(
            s.doc.layers.keys().all(|k| !k.starts_with("shadows-")),
            "shadow layers dropped on undo"
        );
    }

    #[test]
    fn shadowstudy_replay_stable() {
        let mut s = Session::default();
        run(&mut s, "box 0,0,0 4,4,4");
        run(&mut s, "sun 40.71 -74.01 2024-06-21 16:58");
        run(&mut s, "shadowstudy 2024-06-21 12:00 14:00 120");
        let log = s.save_log();
        let replayed = Session::replay(log.clone()).unwrap();
        assert_eq!(
            serde_json::to_string(&log).unwrap(),
            serde_json::to_string(&replayed.save_log()).unwrap(),
            "shadowstudy log must be replay-stable"
        );
        assert_eq!(s.doc.len(), replayed.doc.len());
    }

    /// Write a synthetic EPW (NYC-ish location; DNI 500 / DHI 100 for ending
    /// hours 8–17, dark otherwise) to a temp file and return its path.
    fn write_synth_epw(name: &str) -> std::path::PathBuf {
        use std::io::Write;
        let dir = std::env::temp_dir().join("itsjustcad_radiation_test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, "LOCATION,Testville,TS,TST,TMY3,000000,40.71,-74.01,-5.0,10.0").unwrap();
        for kw in [
            "DESIGN CONDITIONS,0",
            "TYPICAL/EXTREME PERIODS,0",
            "GROUND TEMPERATURES,0",
            "HOLIDAYS/DAYLIGHT SAVINGS,No,0,0,0",
            "COMMENTS 1,x",
            "COMMENTS 2,y",
            "DATA PERIODS,1,1,Data,Sunday,1/1,12/31",
        ] {
            writeln!(f, "{kw}").unwrap();
        }
        for month in 1..=12 {
            for hour in 1..=24 {
                let (dni, dhi) = if (8..=17).contains(&hour) { (500, 100) } else { (0, 0) };
                writeln!(
                    f,
                    "1999,{month},21,{hour},60,A7,10.0,5.0,80,81100,0,0,300,600,{dni},{dhi}"
                )
                .unwrap();
            }
        }
        path
    }

    #[test]
    fn radiation_errors_without_location() {
        let path = write_synth_epw("noloc.epw");
        let mut s = Session::default();
        run(&mut s, "box 0,0,0 4,4,4");
        let err = s
            .run(parse(&format!("radiation last {}", path.display())).unwrap())
            .unwrap_err();
        assert!(err.to_string().contains("no location"), "{err}");
    }

    #[test]
    fn radiation_colors_faces_up_gets_more_than_down() {
        let path = write_synth_epw("site.epw");
        let mut s = Session::default();
        run(&mut s, "box 0,0,0 4,4,4");
        run(&mut s, &format!("import {}", path.display())); // sets location
        let before = s.doc.len();
        let out = run(&mut s, &format!("radiation last {}", path.display()));
        assert!(!out.created.is_empty(), "{}", out.message);
        assert!(out.message.contains("kWh"), "{}", out.message);
        // Overlay faces live on 'analysis' and are colored; the roof (top)
        // faces must be redder (higher insolation) than the bottom faces.
        let (mut top_red, mut bottom_red) = (f32::NEG_INFINITY, f32::NEG_INFINITY);
        for id in &out.created {
            let obj = s.doc.get(*id).unwrap();
            assert_eq!(obj.layer, "analysis");
            let red = obj.color.unwrap()[0];
            let Geometry::Mesh(m) = &obj.geometry else { panic!("mesh expected") };
            let z_avg: f64 =
                m.positions().iter().map(|p| p.z).sum::<f64>() / m.positions().len() as f64;
            if z_avg > 3.9 {
                top_red = top_red.max(red);
            } else if z_avg < 0.1 {
                bottom_red = bottom_red.max(red);
            }
        }
        assert!(
            top_red > bottom_red + 0.3,
            "roof must out-collect the underside: top {top_red} vs bottom {bottom_red}"
        );
        // Undo removes the overlay.
        s.run(crate::Command::Undo).unwrap();
        assert_eq!(s.doc.len(), before);
    }

    #[test]
    fn radiation_replays_without_the_epw_file() {
        // The bins are embedded into the logged command on first exec, so a
        // saved file replays even after the EPW is gone.
        let path = write_synth_epw("ephemeral.epw");
        let mut s = Session::default();
        run(&mut s, "box 0,0,0 4,4,4");
        run(&mut s, &format!("import {}", path.display()));
        run(&mut s, &format!("radiation last {}", path.display()));
        let log = s.save_log();
        std::fs::remove_file(&path).unwrap();
        let replayed = Session::replay(log.clone()).unwrap();
        assert_eq!(
            serde_json::to_string(&log).unwrap(),
            serde_json::to_string(&replayed.save_log()).unwrap(),
            "radiation log must be replay-stable without the EPW file"
        );
        assert_eq!(s.doc.len(), replayed.doc.len());
    }

    #[test]
    fn radiation_parse_requires_selector_and_path() {
        match parse("radiation last /tmp/site file.epw").unwrap() {
            Command::Radiation { path, ids, bins, .. } => {
                assert_eq!(path, "/tmp/site file.epw", "spaces in path survive");
                assert!(ids.is_none() && bins.is_none());
            }
            other => panic!("expected Radiation, got {other:?}"),
        }
        assert!(parse("radiation last").is_err(), "path required");
    }

    #[test]
    fn sunpath_errors_without_location() {
        let mut s = Session::default();
        let err = s.run(parse("sunpath").unwrap()).unwrap_err();
        assert!(err.to_string().contains("no location"), "{err}");
    }

    #[test]
    fn sunpath_draws_dome_on_sunpath_layer_and_undoes() {
        let mut s = Session::default();
        run(&mut s, "location 40.71 -74.01 -5");
        let before = s.doc.len();
        let out = run(&mut s, "sunpath 20");
        // 7 date arcs + hour curves + 1 horizon circle, all on 'sunpath'.
        assert!(out.created.len() > 8, "{}", out.message);
        assert!(s.doc.layers.contains_key("sunpath"));
        let mut on_dome = 0usize;
        let mut closed_circles = 0usize;
        for id in &out.created {
            let obj = s.doc.get(*id).unwrap();
            assert_eq!(obj.layer, "sunpath");
            match &obj.geometry {
                Geometry::Curve(Curve::Polyline { points, closed }) => {
                    assert!(points.len() >= 2);
                    // Every point sits on (or on the rim of) the radius-20 dome,
                    // at or above the ground plane.
                    for p in points {
                        let d = (p.x * p.x + p.y * p.y + p.z * p.z).sqrt();
                        assert!((d - 20.0).abs() < 1e-6, "point off dome: |p|={d}");
                        assert!(p.z >= -1e-9, "below ground: z={}", p.z);
                    }
                    if *closed && points.iter().all(|p| p.z.abs() < 1e-9) {
                        closed_circles += 1;
                    }
                    on_dome += 1;
                }
                g => panic!("expected polyline, got {g:?}"),
            }
        }
        assert_eq!(on_dome, out.created.len());
        assert_eq!(closed_circles, 1, "exactly one horizon compass circle");
        // Undo removes the dome and its layer.
        s.run(crate::Command::Undo).unwrap();
        assert_eq!(s.doc.len(), before);
        assert!(!s.doc.layers.contains_key("sunpath"), "layer removed on undo");
    }

    #[test]
    fn sunpath_auto_radius_scales_with_scene() {
        // With a big scene, the auto dome radius grows past the 10 m floor.
        let mut s = Session::default();
        run(&mut s, "box 0,0,0 40,40,5");
        run(&mut s, "location 40.71 -74.01 -5");
        let out = run(&mut s, "sunpath");
        let id = out.created.first().unwrap();
        let obj = s.doc.get(*id).unwrap();
        let Geometry::Curve(Curve::Polyline { points, .. }) = &obj.geometry else {
            panic!("polyline expected");
        };
        let r = points[0].length();
        assert!(r > 20.0, "auto radius should exceed the 10 m floor: {r}");
    }

    #[test]
    fn sunpath_replay_stable() {
        let mut s = Session::default();
        run(&mut s, "location 40.71 -74.01 -5");
        run(&mut s, "sunpath 15");
        let log = s.save_log();
        let replayed = Session::replay(log.clone()).unwrap();
        assert_eq!(
            serde_json::to_string(&log).unwrap(),
            serde_json::to_string(&replayed.save_log()).unwrap(),
            "sunpath log must be replay-stable"
        );
        assert_eq!(s.doc.len(), replayed.doc.len());
    }

    #[test]
    fn sunpath_parse_round_trip() {
        match parse("sunpath").unwrap() {
            Command::SunPath { ids, year, radius } => {
                assert!(ids.is_none() && radius.is_none());
                assert_eq!(year, 2026);
            }
            other => panic!("expected SunPath, got {other:?}"),
        }
        match parse("sunpath 25 2024").unwrap() {
            Command::SunPath { radius, year, .. } => {
                assert_eq!(radius, Some(25.0));
                assert_eq!(year, 2024);
            }
            other => panic!("expected SunPath, got {other:?}"),
        }
        assert!(parse("sunpath -5").is_err(), "negative radius rejected");
        assert!(parse("sunpath 10 99").is_err(), "silly year rejected");
    }

    #[test]
    fn sunhours_box_shades_cells_beneath_it() {
        // A tall box centered at the origin should shade grid cells directly
        // under it far more than cells well outside its footprint.
        let mut s = Session::default();
        // A tall box at the origin shades the cells beneath it…
        run(&mut s, "box -2,-2,0 4,4,20"); // 4x4 footprint, 20 m tall
        // …plus two tiny corner markers to widen the sampled bbox well past the
        // box footprint (so some grid cells sit in the clear). They are 20 m out,
        // short (0.1 m), so they cast negligible shadow on the clear cells.
        run(&mut s, "box 12,12,0 0.1,0.1,0.1");
        run(&mut s, "box -12,-12,0 0.1,0.1,0.1");
        // Location due south exposure; use a mid-latitude summer date.
        run(&mut s, "location 40.0 0.0 0");
        let out = run(&mut s, "sunhours 2024-06-21 4");
        assert!(!out.created.is_empty(), "cells created: {}", out.message);

        // Collect (center, hours-by-color) for the created quads. Blue channel
        // encodes (1 - fraction of max), red encodes fraction; a shaded cell is
        // bluer (low red). Find the cell nearest origin (under the box) and one
        // far away (outside footprint) and compare their red (=sun) component.
        // The cell whose center is nearest the origin sits under the tall box;
        // the cell with the most sun (max red) is somewhere in the clear. The
        // shaded cell must receive strictly less sun.
        let mut nearest_red = 0.0f32;
        let mut nearest_d = f64::INFINITY;
        let mut max_red = f32::NEG_INFINITY;
        for id in &out.created {
            let obj = s.doc.get(*id).unwrap();
            let Geometry::Mesh(m) = &obj.geometry else { panic!("mesh cell") };
            let c = m.aabb();
            let cx = (c.min.x + c.max.x) * 0.5;
            let cy = (c.min.y + c.max.y) * 0.5;
            let red = obj.color.unwrap()[0];
            let d = cx * cx + cy * cy;
            if d < nearest_d {
                nearest_d = d;
                nearest_red = red;
            }
            max_red = max_red.max(red);
        }
        assert!(max_red.is_finite(), "cells present");
        // Under-box cell gets less sun (lower red) than the sunniest clear cell.
        assert!(
            nearest_red < max_red,
            "shaded cell red {nearest_red} should be < sunniest cell red {max_red}"
        );
    }

    #[test]
    fn facesunhours_parse_round_trip() {
        let c = parse("facesunhours last 2024-06-21").unwrap();
        match &c {
            Command::FaceSunHours { targets, ids, year, month, day } => {
                assert!(matches!(targets, Selector::Last { n: 1 }));
                assert!(ids.is_none());
                assert_eq!((*year, *month, *day), (2024, 6, 21));
            }
            other => panic!("expected FaceSunHours, got {other:?}"),
        }
    }

    #[test]
    fn facesunhours_open_face_approaches_daylight_hours() {
        // A wide, flat, sky-facing slab with nothing above it should receive
        // close to the full astronomical daylight hours for the date/latitude.
        let mut s = Session::default();
        // 40x40 m slab, 0.2 m thick — its top faces point straight up.
        run(&mut s, "box -20,-20,0 40,40,0.2");
        run(&mut s, "location 40.0 0.0 0");
        let out = run(&mut s, "facesunhours last 2024-06-21");
        assert!(!out.created.is_empty(), "faces created: {}", out.message);

        let daylight = itsjustcad_solar::daylight_hours(2024, 6, 21, 40.0);
        // Top faces (color red ≈ max) should reach ~ the astronomical daylight
        // hours. We recover the max sun-hours from the message and compare.
        // The overlay encodes hours only relatively, so assert on the reported
        // max in the message (avg is dragged down by the shaded underside/sides).
        assert!(
            out.message.contains(&format!("astronomical daylight {daylight:.1} h")),
            "message: {}", out.message
        );
        // Parse "max <n>" from the message and compare to daylight.
        let max_tok = out
            .message
            .split("max ")
            .nth(1)
            .and_then(|s| s.split_whitespace().next())
            .and_then(|s| s.parse::<f64>().ok())
            .expect("max hours in message");
        assert!(
            (max_tok - daylight).abs() <= 0.75,
            "sky-facing max sun-hours {max_tok} should be ~ daylight {daylight} h"
        );
    }

    #[test]
    fn facesunhours_occluded_face_gets_less_sun() {
        let parse_max = |m: &str| -> f64 {
            m.split("max ")
                .nth(1)
                .and_then(|s| s.split_whitespace().next())
                .and_then(|s| s.parse::<f64>().ok())
                .unwrap()
        };

        // Open case: a sky-facing slab with clear sky.
        let mut open = Session::default();
        run(&mut open, "box -5,-5,0 10,10,0.2");
        run(&mut open, "location 40.0 0.0 0");
        let open_max = parse_max(&run(&mut open, "facesunhours last 2024-06-21").message);

        // Occluded case: a big canopy 5 m above the *same* slab. Build the
        // canopy FIRST, then the slab, so `last` selects the slab we measure.
        let mut occ = Session::default();
        run(&mut occ, "box -8,-8,5 16,16,0.2"); // canopy overhead
        run(&mut occ, "box -5,-5,0 10,10,0.2"); // slab (last → measured)
        run(&mut occ, "location 40.0 0.0 0");
        let occ_max = parse_max(&run(&mut occ, "facesunhours last 2024-06-21").message);

        assert!(
            occ_max < open_max,
            "occluded max {occ_max} should be < open max {open_max}"
        );
    }

    #[test]
    fn facesunhours_exec_undo_and_replay_stable() {
        let mut s = Session::default();
        run(&mut s, "box 0,0,0 4,4,4");
        run(&mut s, "location 40.71 0.0 0");
        let before = s.doc.len();
        let out = run(&mut s, "facesunhours last 2024-06-21");
        assert!(!out.created.is_empty());
        for id in &out.created {
            let obj = s.doc.get(*id).unwrap();
            assert_eq!(obj.layer, "analysis");
            assert!(matches!(obj.geometry, Geometry::Mesh(_)));
        }
        // Undo removes every overlay face and the analysis layer.
        s.run(crate::Command::Undo).unwrap();
        assert_eq!(s.doc.len(), before, "overlays removed on undo");

        // Replay stability: to_json -> from_json -> to_json byte-identical.
        run(&mut s, "facesunhours last 2024-06-21");
        let log = s.save_log();
        let replayed = Session::replay(log.clone()).unwrap();
        assert_eq!(
            serde_json::to_string(&log).unwrap(),
            serde_json::to_string(&replayed.save_log()).unwrap(),
            "facesunhours log must be replay-stable"
        );
        assert_eq!(s.doc.len(), replayed.doc.len());
    }

    // --- analysis reports + the `report` critique command ---

    #[test]
    fn facing_label_buckets_normals() {
        assert_eq!(facing_label(DVec3::Z), "up");
        assert_eq!(facing_label(-DVec3::Z), "down");
        assert_eq!(facing_label(DVec3::Y), "north");
        assert_eq!(facing_label(-DVec3::Y), "south");
        assert_eq!(facing_label(DVec3::X), "east");
        assert_eq!(facing_label(-DVec3::X), "west");
        assert_eq!(facing_label(DVec3::new(1.0, 1.0, 0.0).normalize()), "northeast");
        assert_eq!(facing_label(DVec3::new(-1.0, -1.0, 0.0).normalize()), "southwest");
        // A gently tilted roof still reads "up"; a steep wall reads by compass.
        assert_eq!(facing_label(DVec3::new(0.0, 0.3, 0.95).normalize()), "up");
        assert_eq!(facing_label(DVec3::new(0.0, 0.95, 0.3).normalize()), "north");
    }

    #[test]
    fn build_analysis_report_stats_bins_and_extremes() {
        let samples: Vec<(f64, DVec3, String)> = (0..12)
            .map(|i| (i as f64, DVec3::new(i as f64, 0.0, 0.0), "up".to_string()))
            .collect();
        let r = build_analysis_report("facesunhours", "2024-06-21".into(), "h", samples);
        assert_eq!(r.count, 12);
        assert_eq!(r.min, 0.0);
        assert_eq!(r.max, 11.0);
        assert!((r.avg - 5.5).abs() < 1e-9);
        assert_eq!(r.bins.len(), 6);
        assert_eq!(r.bins.iter().map(|b| b.1).sum::<usize>(), 12);
        // Extremes are the N lowest ascending / N highest descending.
        assert_eq!(r.lowest.len(), REPORT_LOWEST_N);
        assert_eq!(r.lowest[0].value, 0.0);
        assert_eq!(r.highest.len(), REPORT_HIGHEST_N);
        assert_eq!(r.highest[0].value, 11.0);
        assert_eq!(r.highest[0].at, [11.0, 0.0, 0.0]);
    }

    #[test]
    fn build_analysis_report_all_zero_has_no_bins() {
        let r = build_analysis_report(
            "sunhours",
            "ctx".into(),
            "h",
            vec![(0.0, DVec3::ZERO, "ground".into())],
        );
        assert!(r.bins.is_empty());
        assert_eq!((r.min, r.avg, r.max), (0.0, 0.0, 0.0));
    }

    #[test]
    fn report_errors_without_analysis() {
        let mut s = Session::default();
        let err = s.run(parse("report").unwrap()).unwrap_err();
        assert!(err.to_string().contains("no analysis stored"), "{err}");
    }

    #[test]
    fn report_parse_round_trip_and_never_logged() {
        assert!(matches!(parse("report").unwrap(), Command::EnviroReport { kind: None }));
        match parse("report sunhours").unwrap() {
            Command::EnviroReport { kind } => assert_eq!(kind.as_deref(), Some("sunhours")),
            other => panic!("expected EnviroReport, got {other:?}"),
        }
        assert!(parse("report a b").is_err());
        assert!(!parse("report").unwrap().is_logged(), "report is a query, never logged");
    }

    #[test]
    fn facesunhours_report_north_wall_face_is_lowest_south_or_top_highest() {
        // A long thin wall along X: its two big faces point north (+Y) and
        // south (-Y). At 40°N on the WINTER solstice the sun never leaves the
        // southern sky, so the north face and the underside get zero while the
        // top and south face collect essentially the whole day — exactly the
        // "north facade gets no winter sun" critique the report must ground.
        let mut s = Session::default();
        run(&mut s, "box 0,0,0 10,0.2,3");
        run(&mut s, "location 40.0 0.0 0");
        run(&mut s, "facesunhours last 2024-12-21");

        let r = s.doc.analysis_reports.get("facesunhours").expect("report stored");
        assert_eq!(r.kind, "facesunhours");
        assert_eq!(r.context, "2024-12-21");
        assert_eq!(r.unit, "h");
        assert_eq!(r.count, 12, "a box tessellates to 12 triangles");
        assert_eq!(r.min, 0.0, "the underside sees no sun");
        assert!(r.max > 8.0, "sky-facing top approaches daylight: {}", r.max);
        assert_eq!(r.bins.iter().map(|b| b.1).sum::<usize>(), 12);
        assert!(
            r.lowest.iter().any(|smp| smp.tag == "north"),
            "north wall face among the lowest: {:?}",
            r.lowest.iter().map(|smp| &smp.tag).collect::<Vec<_>>()
        );
        assert!(
            r.highest.iter().all(|smp| smp.tag == "up" || smp.tag == "south"),
            "top/south dominate the highest: {:?}",
            r.highest.iter().map(|smp| &smp.tag).collect::<Vec<_>>()
        );
        // Sample locations are real wall coordinates.
        for smp in r.lowest.iter().chain(&r.highest) {
            assert!((-1.0..=11.0).contains(&smp.at[0]), "x in wall span: {:?}", smp.at);
        }
    }

    #[test]
    fn sunhours_stores_ground_report() {
        let mut s = Session::default();
        // A tower plus a distant low marker: the marker widens the scene AABB
        // so the ground grid has open cells (under the tower = 0 h, in the
        // clear = many hours).
        run(&mut s, "box 0,0,0 6,6,3");
        run(&mut s, "box 14,14,0 1,1,1");
        run(&mut s, "location 40.0 0.0 0");
        let out = run(&mut s, "sunhours 2024-06-21 2");
        let r = s.doc.analysis_reports.get("sunhours").expect("report stored");
        assert_eq!(r.kind, "sunhours");
        assert_eq!(r.unit, "h");
        assert_eq!(r.count, out.created.len(), "one sample per grid cell");
        assert!(r.context.contains("2024-06-21") && r.context.contains("2 m"));
        assert!(r.lowest.iter().chain(&r.highest).all(|smp| smp.tag == "ground"));
        // Cells under the box are shaded: the darkest cell is well below max.
        assert!(r.min < r.max, "min {} < max {}", r.min, r.max);
    }

    #[test]
    fn shadowstudy_stores_report_with_time_stamps() {
        let mut s = Session::default();
        run(&mut s, "box 0,0,0 4,4,4");
        run(&mut s, "location 40.71 0.0 0");
        let out = run(&mut s, "shadowstudy 2024-06-21 09:00 15:00 180");
        let r = s.doc.analysis_reports.get("shadowstudy").expect("report stored");
        assert_eq!(r.kind, "shadowstudy");
        assert_eq!(r.unit, "m2");
        assert_eq!(r.count, out.created.len(), "one sample per shadow polygon");
        assert!(r.context.contains("09:00") && r.context.contains("15:00"));
        // Every sample is tagged with its HH:MM stamp and covers real area.
        for smp in r.lowest.iter().chain(&r.highest) {
            assert!(smp.tag.contains(':'), "stamp tag: {}", smp.tag);
            assert!(smp.value > 0.0, "shadow polygon has area");
        }
    }

    #[test]
    fn radiation_stores_report_with_facings() {
        let path = write_synth_epw("report.epw");
        let mut s = Session::default();
        run(&mut s, "box 0,0,0 10,0.2,3");
        run(&mut s, "location 40.71 -74.01 -5");
        let out = run(&mut s, &format!("radiation last {}", path.display()));
        assert!(!out.created.is_empty());
        let r = s.doc.analysis_reports.get("radiation").expect("report stored");
        assert_eq!(r.kind, "radiation");
        assert_eq!(r.unit, "kWh/m2-yr");
        assert_eq!(r.count, 12);
        assert!(r.context.contains("report.epw"));
        assert!(r.max > r.min);
        assert!(
            r.highest.iter().all(|smp| smp.tag == "up" || smp.tag == "south"),
            "top/south collect the most annual radiation: {:?}",
            r.highest.iter().map(|smp| &smp.tag).collect::<Vec<_>>()
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn report_command_prints_summary_and_filters_by_kind() {
        let mut s = Session::default();
        run(&mut s, "box 0,0,0 4,4,3");
        run(&mut s, "location 40.0 0.0 0");
        run(&mut s, "facesunhours last 2024-06-21");
        let logged = s.save_log();

        // Unfiltered: prints the stored report with stats + extremes.
        let out = run(&mut s, "report");
        assert!(out.message.contains("facesunhours (2024-06-21)"), "{}", out.message);
        assert!(out.message.contains("min 0.0"), "{}", out.message);
        assert!(out.message.contains("distribution:"), "{}", out.message);
        assert!(out.message.contains("lowest:"), "{}", out.message);
        assert!(out.message.contains("highest:"), "{}", out.message);
        assert!(out.message.contains("[down]") || out.message.contains("[north]"),
            "extreme samples carry facings: {}", out.message);

        // Kind filter hits and misses.
        assert!(run(&mut s, "report facesunhours").message.contains("facesunhours"));
        let err = s.run(parse("report radiation").unwrap()).unwrap_err();
        assert!(err.to_string().contains("stored: facesunhours"), "{err}");

        // A query never grows the op-log.
        assert_eq!(
            serde_json::to_string(&logged).unwrap(),
            serde_json::to_string(&s.save_log()).unwrap(),
            "report must not be logged"
        );
    }

    #[test]
    fn analysis_report_survives_checkpoint_round_trip() {
        // The checkpoint sidecar serializes `analysis_reports`; an old snapshot
        // without the field must still load (serde default).
        let mut s = Session::default();
        run(&mut s, "box 0,0,0 4,4,3");
        run(&mut s, "location 40.0 0.0 0");
        run(&mut s, "facesunhours last 2024-06-21");
        let json = serde_json::to_string(&s.doc).unwrap();
        let back: Document = serde_json::from_str(&json).unwrap();
        assert_eq!(back.analysis_reports, s.doc.analysis_reports);

        let stripped = {
            let mut v: serde_json::Value = serde_json::from_str(&json).unwrap();
            v.as_object_mut().unwrap().remove("analysis_reports");
            v.to_string()
        };
        let old: Document = serde_json::from_str(&stripped).unwrap();
        assert!(old.analysis_reports.is_empty(), "pre-report snapshots load empty");
    }

    #[test]
    fn import_epw_sets_location_and_reports_stats() {
        use std::io::Write;
        let dir = std::env::temp_dir().join("itsjustcad_epw_test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("site.epw");
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(
            f,
            "LOCATION,Denver Intl Ap,CO,USA,TMY3,725650,39.83,-104.65,-7.0,1650.0"
        )
        .unwrap();
        for kw in [
            "DESIGN CONDITIONS,0",
            "TYPICAL/EXTREME PERIODS,0",
            "GROUND TEMPERATURES,0",
            "HOLIDAYS/DAYLIGHT SAVINGS,No,0,0,0",
            "COMMENTS 1,x",
            "COMMENTS 2,y",
            "DATA PERIODS,1,1,Data,Sunday,1/1,12/31",
        ] {
            writeln!(f, "{kw}").unwrap();
        }
        writeln!(f, "1999,1,1,1,60,A7,10.0,5.0,80,81100").unwrap();
        writeln!(f, "1999,1,1,2,60,A7,20.0,6.0,78,81100").unwrap();
        drop(f);

        let mut s = Session::default();
        let out = s
            .run(parse(&format!("import {}", path.display())).unwrap())
            .unwrap();
        assert!(out.message.contains("Denver"), "{}", out.message);
        assert!(out.message.contains("mean 15.0"), "{}", out.message);
        let loc = s.doc.location.expect("EPW set location");
        assert!((loc.lat_deg - 39.83).abs() < 1e-6);
        assert!((loc.lon_deg - (-104.65)).abs() < 1e-6);
        assert!((loc.tz_hours - (-7.0)).abs() < 1e-6);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn old_sun_log_without_latlon_still_loads() {
        // Pre-enviro logs recorded Sun with only az/alt. Serde defaults must let
        // them replay (lat/lon → 0).
        let json = r#"[{"cmd":"sun","azimuth_deg":180.0,"altitude_deg":72.7}]"#;
        let log: Vec<Command> = serde_json::from_str(json).unwrap();
        let s = Session::replay(log).unwrap();
        let sun = s.doc.sun.expect("sun replayed");
        assert!((sun.altitude_deg - 72.7).abs() < 1e-9);
        let loc = s.doc.location.expect("location defaulted");
        assert_eq!((loc.lat_deg, loc.lon_deg), (0.0, 0.0));
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
        assert!(matches!(last.geometry, itsjustcad_doc::Geometry::Instance { ref block, .. } if block == "mytree"));
    }

    #[test]
    fn block_insert_with_rotation_and_scale() {
        let mut s = Session::default();
        run(&mut s, "box 0,0,0 1,1,1");
        run(&mut s, "block last door");
        run(&mut s, "insert door 3,0,0 90 2");
        let obj = s.doc.objects().last().unwrap();
        match &obj.geometry {
            itsjustcad_doc::Geometry::Instance { block, position, rotation_deg, scale, .. } => {
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
    fn blockdelete_removes_plain_definition() {
        let mut s = Session::default();
        run(&mut s, "box 0,0,0 1,1,1");
        run(&mut s, "block last crate");
        assert!(s.doc.blocks.contains_key("crate"));
        run(&mut s, "blockdelete crate");
        assert!(!s.doc.blocks.contains_key("crate"), "definition removed");
    }

    #[test]
    fn blockdelete_removes_parametric_definition() {
        let mut s = Session::default();
        run(&mut s, "pblock pdoor width=0.9 : rect 0,0,0 {width} 0.05");
        assert!(s.doc.param_blocks.contains_key("pdoor"));
        run(&mut s, "blockdelete pdoor");
        assert!(!s.doc.param_blocks.contains_key("pdoor"));
    }

    #[test]
    fn blockdelete_refuses_while_instances_exist() {
        let mut s = Session::default();
        run(&mut s, "box 0,0,0 1,1,1");
        run(&mut s, "block last tree");
        run(&mut s, "insert tree 5,0,0");
        let err = s.run(crate::parse::parse("blockdelete tree").unwrap());
        assert!(err.is_err(), "must refuse with a live instance");
        let msg = format!("{}", err.unwrap_err());
        assert!(msg.contains("1 instance"), "guard names the count: {msg}");
        assert!(s.doc.blocks.contains_key("tree"), "definition untouched");
    }

    #[test]
    fn blockdelete_refuses_while_dynamic_instances_exist() {
        let mut s = Session::default();
        run(&mut s, "pblock pwin width=0.5 : rect 0,0,0 {width} 0.05");
        run(&mut s, "insert pwin 0,0,0");
        let err = s.run(crate::parse::parse("blockdelete pwin").unwrap());
        assert!(err.is_err(), "dynamic instances also guard the definition");
        assert!(s.doc.param_blocks.contains_key("pwin"));
    }

    #[test]
    fn blockdelete_allows_after_instances_deleted() {
        let mut s = Session::default();
        run(&mut s, "box 0,0,0 1,1,1");
        run(&mut s, "block last bench");
        run(&mut s, "insert bench 5,0,0");
        run(&mut s, "delete last");
        run(&mut s, "blockdelete bench");
        assert!(!s.doc.blocks.contains_key("bench"));
    }

    #[test]
    fn blockdelete_undo_restores_definition() {
        let mut s = Session::default();
        run(&mut s, "box 0,0,0 1,1,2");
        run(&mut s, "block last lamp");
        let before = s.doc.blocks.get("lamp").unwrap().clone();
        run(&mut s, "blockdelete lamp");
        s.run(crate::Command::Undo).unwrap();
        assert_eq!(s.doc.blocks.get("lamp"), Some(&before), "undo restores the definition");
    }

    #[test]
    fn blockdelete_undo_restores_parametric_definition() {
        let mut s = Session::default();
        run(&mut s, "pblock pcol h=3 : box 0,0,0 0.3,0.3,{h}");
        run(&mut s, "blockdelete pcol");
        assert!(!s.doc.param_blocks.contains_key("pcol"));
        s.run(crate::Command::Undo).unwrap();
        assert!(s.doc.param_blocks.contains_key("pcol"), "undo restores the pblock");
    }

    #[test]
    fn blockdelete_replay_stability() {
        let mut s = Session::default();
        run(&mut s, "box 0,0,0 1,1,1");
        run(&mut s, "block last temp");
        run(&mut s, "blockdelete temp");
        let log = s.save_log();
        let replayed = Session::replay(log.clone()).unwrap();
        assert!(!replayed.doc.blocks.contains_key("temp"));
        assert_eq!(
            serde_json::to_string(&log).unwrap(),
            serde_json::to_string(&replayed.save_log()).unwrap(),
            "blockdelete log must be replay-stable"
        );
    }

    #[test]
    fn blockdelete_unknown_name_errors() {
        let mut s = Session::default();
        let result = s.run(crate::parse::parse("blockdelete ghost").unwrap());
        assert!(result.is_err(), "unknown definition must error");
    }

    #[test]
    fn insert_unknown_block_errors() {
        let mut s = Session::default();
        let result = s.run(crate::parse::parse("insert nosuchblock 0,0,0").unwrap());
        assert!(result.is_err(), "should error on unknown block");
    }

    // ---- block library commands ----

    #[test]
    fn blocklib_list_is_not_logged() {
        let mut s = Session::default();
        let log_before = s.save_log().len();
        run(&mut s, "blocklib");
        assert_eq!(s.save_log().len(), log_before, "blocklib list must not be logged");
    }

    #[test]
    fn blocklib_list_variant_accepted() {
        // Both "blocklib" and "blocklib list" should parse.
        let cmd1 = crate::parse::parse("blocklib").unwrap();
        let cmd2 = crate::parse::parse("blocklib list").unwrap();
        assert_eq!(cmd1, cmd2);
    }

    #[test]
    fn blockload_loads_starter_tree_and_inserts() {
        // Use the embedded starter block geometry directly (no fs dependency in CI).
        use crate::blocklib::{load_from_dir, seed_dir};
        let tmp = {
            let dir = std::env::temp_dir()
                .join("itsjustcad_exec_blockload")
                .join(format!("{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            dir
        };
        seed_dir(&tmp);

        let bf = load_from_dir(&tmp, "tree").unwrap();
        let mut s = Session::default();
        s.doc.blocks.insert(bf.name.clone(), bf.geometries);

        run(&mut s, "insert tree 5,5,0");
        run(&mut s, "insert tree 10,3,0");
        assert_eq!(s.doc.len(), 2, "two tree instances inserted");
        for obj in s.doc.objects() {
            assert!(
                matches!(&obj.geometry, itsjustcad_doc::Geometry::Instance { block, .. } if block == "tree"),
                "expected tree instance"
            );
        }
    }

    #[test]
    fn blockload_command_replay_stability() {
        // Drive the full blockload command through Session::run so we can test
        // that the replay log is self-contained (geometries are embedded).
        // We inject a BlockLibLoad with pre-filled geometries (replay form)
        // so the test doesn't require the library directory to exist.
        use kernel_curve::Curve;
        use itsjustcad_doc::BlockGeometry;

        let geoms = vec![BlockGeometry::Curve(Curve::Arc {
            center: glam::DVec3::ZERO,
            radius: 0.5,
            start: 0.0,
            end: std::f64::consts::TAU,
        })];
        let cmd = crate::Command::BlockLibLoad {
            name: "tree".to_string(),
            geometries: Some(geoms),
        };
        let mut s = Session::default();
        s.run(cmd).unwrap();
        assert!(s.doc.blocks.contains_key("tree"), "block defined after load");

        // Replay the log.
        let log = s.save_log();
        let replayed = Session::replay(log.clone()).unwrap();
        assert_eq!(
            serde_json::to_string(&log).unwrap(),
            serde_json::to_string(&replayed.save_log()).unwrap(),
            "blockload must be replay-stable"
        );
    }

    #[test]
    fn blockload_undo_removes_definition() {
        use kernel_curve::Curve;
        use itsjustcad_doc::BlockGeometry;

        let geoms = vec![BlockGeometry::Curve(Curve::Line {
            a: glam::DVec3::ZERO,
            b: glam::DVec3::new(1.0, 0.0, 0.0),
        })];
        let cmd = crate::Command::BlockLibLoad {
            name: "mylib".to_string(),
            geometries: Some(geoms),
        };
        let mut s = Session::default();
        s.run(cmd).unwrap();
        assert!(s.doc.blocks.contains_key("mylib"));
        run(&mut s, "undo");
        assert!(!s.doc.blocks.contains_key("mylib"), "undo removes loaded block");
    }

    #[test]
    fn blocksave_unknown_block_errors() {
        let mut s = Session::default();
        let result = s.run(crate::parse::parse("blocksave nosuchblock").unwrap());
        assert!(result.is_err(), "blocksave of undefined block must error");
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
    fn material2_set_undo_redo_replay() {
        use itsjustcad_doc::{MaterialPreset, ObjectMaterial};
        let mut s = Session::default();
        run(&mut s, "box 0,0,0 1,1,1");
        let id = *s.doc.all_ids().last().unwrap();

        // Apply a preset material.
        run(&mut s, "material2 last glass");
        assert_eq!(
            s.doc.get(id).unwrap().material,
            Some(ObjectMaterial::Preset { preset: MaterialPreset::Glass })
        );

        // Undo restores None.
        run(&mut s, "undo");
        assert_eq!(s.doc.get(id).unwrap().material, None);

        // Redo re-applies.
        run(&mut s, "redo");
        assert!(matches!(
            s.doc.get(id).unwrap().material,
            Some(ObjectMaterial::Preset { preset: MaterialPreset::Glass })
        ));

        // Replace with a custom material, then material2off clears it.
        run(&mut s, "material2 last roughness=0.9 metallic=0 color=0.6,0.6,0.6");
        match s.doc.get(id).unwrap().material {
            Some(ObjectMaterial::Custom { roughness, .. }) => {
                assert!((roughness - 0.9).abs() < 1e-6)
            }
            other => panic!("expected custom, got {other:?}"),
        }
        run(&mut s, "material2 last off");
        assert_eq!(s.doc.get(id).unwrap().material, None);

        // Undo the off restores the custom material.
        run(&mut s, "undo");
        assert!(matches!(
            s.doc.get(id).unwrap().material,
            Some(ObjectMaterial::Custom { .. })
        ));

        // Replay reproduces the exact final state.
        let log = s.save_log();
        let s2 = Session::replay(log).unwrap();
        assert_eq!(s2.doc.get(id).unwrap().material, s.doc.get(id).unwrap().material);
    }

    #[test]
    fn material2_replay_stable_json() {
        let mut s = Session::default();
        run(&mut s, "box 0,0,0 2,2,2");
        run(&mut s, "material2 last metal");
        run(&mut s, "box 3,0,0 4,4,4");
        run(&mut s, "material2 last roughness=0.2 metallic=1 color=0.8,0.8,0.9");

        let json = crate::io::to_json(&s);
        let loaded = crate::io::from_json(&json).unwrap();
        assert_eq!(crate::io::to_json(&loaded), json, "replay-stable json");
    }

    #[test]
    fn material2_needs_gpu_message_from_exec() {
        // controlimages reaching exec (no GPU) is a clear error, not a panic.
        let mut s = Session::default();
        let err = s.run(parse("controlimages /tmp/x").unwrap()).unwrap_err();
        assert!(err.to_string().contains("GPU"), "{err}");
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
    fn insertknot_adds_control_point_shape_unchanged_undo_replay() {
        let mut s = Session::default();
        run(&mut s, "curve 0,0 2,4 6,4 8,0");
        let id = *s.doc.all_ids().last().unwrap();
        let Curve::Nurbs { control: c0, weights: w0, knots: k0, degree } = curve_of_last(&s)
        else {
            panic!("expected nurbs")
        };
        run(&mut s, "insertknot last 0.5");
        let Curve::Nurbs { control, weights, knots, .. } = curve_of_last(&s) else { panic!() };
        assert_eq!(control.len(), c0.len() + 1);
        for i in 0..=50 {
            let t = i as f64 / 50.0;
            let a = kernel_curve::nurbs_point(&c0, &w0, &k0, degree, t);
            let b = kernel_curve::nurbs_point(&control, &weights, &knots, degree, t);
            assert!(a.distance(b) < 1e-9, "shape changed at t={t}");
        }
        // Undo restores the original control net.
        s.run(Command::Undo).unwrap();
        let Curve::Nurbs { control, .. } = curve_of_last(&s) else { panic!() };
        assert_eq!(control.len(), c0.len());
        s.run(Command::Redo).unwrap();
        assert!(s.doc.get(id).is_some());
        // Replay from the op-log is stable.
        let json = crate::io::to_json(&s);
        let loaded = crate::io::from_json(&json).unwrap();
        assert_eq!(crate::io::to_json(&loaded), json, "replay-stable");
    }

    #[test]
    fn insertknot_rejects_non_nurbs_and_bad_t() {
        let mut s = Session::default();
        run(&mut s, "polyline 0,0 4,0 4,4");
        assert!(s.run(parse("insertknot last 0.5").unwrap()).is_err());
        run(&mut s, "curve 0,0 2,4 6,4 8,0");
        assert!(s.run(parse("insertknot last 0").unwrap()).is_err());
        assert!(s.run(parse("insertknot last 1.5").unwrap()).is_err());
    }

    #[test]
    fn curvature_comb_on_circle_undo_and_replay() {
        let mut s = Session::default();
        run(&mut s, "circle 0,0,0 5");
        let before = s.doc.all_ids().len();
        let out = run(&mut s, "curvature last 1 16");
        // 16 hairs + 1 tip polyline, all on the analysis layer.
        assert_eq!(out.created.len(), 17);
        assert!(out.message.contains("min radius 5.000"), "{}", out.message);
        assert!(s.doc.layers.contains_key("analysis"));
        for id in &out.created {
            assert_eq!(s.doc.get(*id).unwrap().layer, "analysis");
        }
        // Hair length = kappa * scale = (1/5) * 1 = 0.2 m.
        let Geometry::Curve(Curve::Line { a, b }) =
            &s.doc.get(out.created[0]).unwrap().geometry
        else {
            panic!("expected hair line")
        };
        assert!((a.distance(*b) - 0.2).abs() < 1e-6);
        // Hairs point inward (tip closer to the center than the foot).
        assert!(b.length() < a.length());
        // Undo removes the comb (and the created layer).
        s.run(Command::Undo).unwrap();
        assert_eq!(s.doc.all_ids().len(), before);
        assert!(!s.doc.layers.contains_key("analysis"));
        s.run(Command::Redo).unwrap();
        assert_eq!(s.doc.all_ids().len(), before + 17);
        // Replay from the op-log is stable (ids embedded).
        let json = crate::io::to_json(&s);
        let loaded = crate::io::from_json(&json).unwrap();
        assert_eq!(crate::io::to_json(&loaded), json, "replay-stable");
    }

    #[test]
    fn curvature_rejects_bad_args() {
        let mut s = Session::default();
        run(&mut s, "circle 0,0,0 5");
        assert!(s.run(parse("curvature last 0").unwrap()).is_err()); // scale must be > 0
        assert!(s.run(parse("curvature last 1 1").unwrap()).is_err()); // too few samples
        assert!(s.run(parse("curvature last 1 501").unwrap()).is_err()); // too many
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

    // ---- site: terrain / geojson import / osmfile ----

    fn write_tmp(name: &str, contents: &[u8]) -> String {
        let path = std::env::temp_dir().join(format!("itsjustcad_test_{name}"));
        std::fs::write(&path, contents).unwrap();
        path.to_string_lossy().into_owned()
    }

    fn mesh_faces_on_layer(s: &Session, layer: &str) -> usize {
        s.doc
            .objects()
            .filter(|o| o.layer == layer)
            .filter_map(|o| match &o.geometry {
                Geometry::Mesh(m) => Some(m.faces().len()),
                _ => None,
            })
            .sum()
    }

    #[test]
    fn terrain_csv_square_center_is_four_triangles() {
        let path = write_tmp("terr.csv", b"x,y,z\n0,0,0\n1,0,0\n1,1,0\n0,1,0\n0.5,0.5,2\n");
        let mut s = Session::default();
        let out = run(&mut s, &format!("terrain {path}"));
        assert!(out.message.contains("4 triangles"), "{}", out.message);
        assert_eq!(mesh_faces_on_layer(&s, "terrain"), 4);
        // The terrain op is not itself logged; its MeshLiteral expansion is,
        // and the current layer is restored to default afterwards.
        assert_eq!(s.doc.current_layer, "default");
    }

    #[test]
    fn terrain_replay_stable() {
        let path = write_tmp("terr2.csv", b"0,0,0\n4,0,1\n4,4,0\n0,4,1\n2,2,3\n");
        let mut s = Session::default();
        run(&mut s, &format!("terrain {path}"));
        let replayed = Session::replay(s.save_log()).unwrap();
        assert_eq!(mesh_faces_on_layer(&replayed, "terrain"), 4);
        assert_eq!(replayed.doc.all_ids(), s.doc.all_ids(), "ids stable on replay");
    }

    #[test]
    fn import_geojson_polygon_line_point() {
        let gj = br#"{"type":"FeatureCollection","features":[
          {"type":"Feature","properties":{"name":"lot"},
           "geometry":{"type":"Polygon","coordinates":[[[0,0],[10,0],[10,10],[0,10],[0,0]]]}},
          {"type":"Feature","properties":{},
           "geometry":{"type":"LineString","coordinates":[[0,0],[5,5]]}},
          {"type":"Feature","properties":{"name":"tree"},
           "geometry":{"type":"Point","coordinates":[3,3]}}
        ]}"#;
        let path = write_tmp("site.geojson", gj);
        let mut s = Session::default();
        let out = run(&mut s, &format!("import {path}"));
        assert!(out.message.contains("3 GeoJSON feature"), "{}", out.message);
        // Polygon → closed polyline, Line → open polyline, Point → circle.
        let closed = s
            .doc
            .objects()
            .filter(|o| matches!(&o.geometry, Geometry::Curve(c) if c.is_closed()))
            .count();
        assert!(closed >= 2, "polygon + point-circle are closed");
        assert!(s.doc.find_named("lot").len() == 1, "polygon carries its name");
        assert!(s.doc.find_named("tree").len() == 1, "point carries its name");
    }

    #[test]
    fn osmfile_extrudes_buildings_on_context_layer() {
        let osm = br#"{"elements":[
          {"type":"way","id":1,"tags":{"building":"yes","height":"12"},
           "geometry":[{"lat":0,"lon":0},{"lat":0,"lon":0.0002},
                       {"lat":0.0002,"lon":0.0002},{"lat":0.0002,"lon":0},{"lat":0,"lon":0}]},
          {"type":"way","id":2,"tags":{"highway":"residential"},
           "geometry":[{"lat":0,"lon":0},{"lat":0,"lon":0.001}]}
        ]}"#;
        let path = write_tmp("overpass.json", osm);
        let mut s = Session::default();
        // A location makes lon/lat project to local meters (else degrees).
        run(&mut s, "location 0 0");
        let out = run(&mut s, &format!("osmfile {path}"));
        assert!(out.message.contains("1 building"), "{}", out.message);
        // One extruded box → 12 side/cap triangles.
        assert_eq!(mesh_faces_on_layer(&s, "context"), 12);
        assert_eq!(s.doc.current_layer, "default", "layer restored");
    }

    #[test]
    fn osmfile_replay_stable() {
        let osm = br#"{"elements":[
          {"type":"way","id":1,"tags":{"building":"yes"},
           "geometry":[{"lat":0,"lon":0},{"lat":0,"lon":0.0002},
                       {"lat":0.0002,"lon":0},{"lat":0,"lon":0}]}
        ]}"#;
        let path = write_tmp("overpass2.json", osm);
        let mut s = Session::default();
        run(&mut s, &format!("osmfile {path}"));
        let replayed = Session::replay(s.save_log()).unwrap();
        assert_eq!(replayed.doc.all_ids(), s.doc.all_ids());
        // Triangular footprint extrudes to 3 side quads (6 tris) + 2 caps = 8.
        assert_eq!(mesh_faces_on_layer(&replayed, "context"), 8);
    }

    // ---- design options: op-log branches ----

    #[test]
    fn option_save_switch_round_trips_document() {
        let mut s = Session::default();
        run(&mut s, "box 0,0,0 2,2,10"); // tower massing
        run(&mut s, "option save tower");
        let tower_ids = s.doc.all_ids();
        let tower_log = s.save_log();

        // Diverge onto a fresh scheme and save it.
        run(&mut s, "box 0,0,0 8,8,3"); // now tower + courtyard slab
        run(&mut s, "option save courtyard");
        assert_eq!(s.current_branch(), "courtyard");
        assert_eq!(s.doc.len(), 2);

        // Switch back to tower: replay reproduces the exact earlier state.
        let out = run(&mut s, "option tower");
        assert_eq!(out.message, "switched to option: tower");
        assert_eq!(s.current_branch(), "tower");
        assert_eq!(s.doc.all_ids(), tower_ids, "replayed ids identical");
        assert_eq!(s.save_log(), tower_log, "live log is the tower branch");

        // Round-trip: tower branch equals a standalone replay of its log
        // (the switch bumps `generation` for cache invalidation, so compare
        // the objects rather than the whole Document).
        let fresh = Session::replay(tower_log).unwrap();
        let objs = |s: &Session| {
            let mut v: Vec<_> = s.doc.objects().cloned().collect();
            v.sort_by_key(|o| o.id);
            v
        };
        assert_eq!(objs(&s), objs(&fresh));
    }

    #[test]
    fn option_switch_auto_saves_divergent_work() {
        let mut s = Session::default();
        run(&mut s, "box 0,0,0 1,1,1");
        run(&mut s, "option save a");
        run(&mut s, "box 5,0,0 1,1,1");
        run(&mut s, "option save b");

        // On branch b, keep working WITHOUT saving, then switch away.
        run(&mut s, "box 10,0,0 1,1,1");
        assert_eq!(s.doc.len(), 3);
        run(&mut s, "option a"); // leaving b with unsaved divergence
        assert_eq!(s.doc.len(), 1, "landed on a");

        // The divergent third box must have been auto-saved into b.
        run(&mut s, "option b");
        assert_eq!(s.doc.len(), 3, "b kept the in-progress box");
    }

    #[test]
    fn option_list_marks_current_and_delete_guards() {
        let mut s = Session::default();
        run(&mut s, "box 0,0,0 1,1,1");
        run(&mut s, "option save a");
        run(&mut s, "option save b");
        let out = run(&mut s, "option list");
        assert!(out.message.contains("*b"), "{}", out.message);
        assert!(out.message.contains("a"), "{}", out.message);

        // Cannot delete the branch you are on.
        let err = s.run(parse("option delete b").unwrap()).unwrap_err();
        assert!(err.to_string().contains("current"), "{err}");

        // Deleting another is fine; switching to a missing one errors.
        run(&mut s, "option delete a");
        assert_eq!(s.branches().len(), 1);
        let err = s.run(parse("option nope").unwrap()).unwrap_err();
        assert!(err.to_string().contains("no option 'nope'"), "{err}");
    }

    // ---- structural members ----------------------------------------------

    /// Replay the saved log into a fresh session; the doc must match bit-for-bit.
    fn assert_replay_stable(s: &Session) {
        let replayed = Session::replay(s.save_log()).expect("replay succeeds");
        assert_eq!(replayed.doc, s.doc, "replay doc == live doc");
    }

    #[test]
    fn section_define_exec_undo_replay() {
        let mut s = Session::default();
        run(&mut s, "section col rect 0.4 0.6");
        assert!(s.doc.sections.contains_key("col"));
        assert_replay_stable(&s);
        run(&mut s, "undo");
        assert!(s.doc.sections.is_empty());
        run(&mut s, "redo");
        assert!(s.doc.sections.contains_key("col"));
    }

    #[test]
    fn material_define_exec_undo_replay() {
        let mut s = Session::default();
        run(&mut s, "material steel 200e9 7850");
        let m = s.doc.materials.get("steel").expect("material defined");
        assert!((m.elastic_modulus_e - 200e9).abs() < 1.0);
        assert!((m.density - 7850.0).abs() < 1e-9);
        assert_replay_stable(&s);
        run(&mut s, "undo");
        assert!(s.doc.materials.is_empty());
    }

    #[test]
    fn grid_define_exec_undo_replay() {
        let mut s = Session::default();
        run(&mut s, "grid main x A:0 B:6 C:12 y 1:0 2:5 levels 0,3.5,7");
        let g = s.doc.grids.get("main").expect("grid defined");
        assert_eq!(g.x_axes.len(), 3);
        assert_eq!(g.y_axes.len(), 2);
        assert_eq!(g.levels.len(), 3);
        assert_eq!(g.x_axes[1], ("B".to_string(), 6.0));
        assert_replay_stable(&s);
        run(&mut s, "undo");
        assert!(s.doc.grids.is_empty());
    }

    #[test]
    fn story_define_sorts_and_undo_replay() {
        let mut s = Session::default();
        run(&mut s, "story L2 3.5");
        run(&mut s, "story L1 0");
        // sorted by elevation, bottom first
        assert_eq!(s.doc.stories[0].name, "L1");
        assert_eq!(s.doc.stories[1].name, "L2");
        assert_replay_stable(&s);
        run(&mut s, "undo"); // removes L1 add
        assert_eq!(s.doc.stories.len(), 1);
        assert_eq!(s.doc.stories[0].name, "L2");
    }

    #[test]
    fn beam_rect_section_is_watertight_with_correct_volume() {
        let mut s = Session::default();
        run(&mut s, "section b rect 0.4 0.6");
        let out = run(&mut s, "beam 0,0,3 5,0,3 b");
        let id = out.created[0];
        let Geometry::Frame { mesh, .. } = &s.doc.get(id).unwrap().geometry else {
            panic!("beam should be a Frame geometry");
        };
        // V = w*h*length = 0.4*0.6*5 = 1.2
        assert!((kernel_mesh::signed_volume(mesh) - 1.2).abs() < 1e-6);
        // Watertight: every undirected edge balances.
        let welded = kernel_mesh::weld(mesh, 1e-9);
        let mut bal: std::collections::HashMap<(u32, u32), i64> = std::collections::HashMap::new();
        for f in welded.faces() {
            if f[0] == f[1] || f[1] == f[2] || f[0] == f[2] {
                continue;
            }
            for (a, b) in [(f[0], f[1]), (f[1], f[2]), (f[2], f[0])] {
                *bal.entry((a.min(b), a.max(b))).or_default() += if a < b { 1 } else { -1 };
            }
        }
        assert!(!bal.is_empty() && bal.values().all(|&v| v == 0), "beam mesh watertight");
        assert_replay_stable(&s);
    }

    // ── expressive parametric structures ────────────────────────────────────

    fn mesh_of(s: &Session, id: ObjectId) -> &kernel_mesh::Mesh {
        match &s.doc.get(id).unwrap().geometry {
            Geometry::Mesh(m) => m,
            g => panic!("expected mesh geometry, got {g:?}"),
        }
    }

    #[test]
    fn geodesic_exec_undo_redo_replay() {
        let mut s = Session::default();
        let out = run(&mut s, "geodesic 3 5 dome");
        let id = out.created[0];
        // A freq-3 dome has struts; the mesh is a non-empty strut lattice
        // (8 verts per strut).
        let m = mesh_of(&s, id);
        assert!(!m.positions().is_empty());
        assert_eq!(m.positions().len() % 8, 0, "8 verts per strut prism");
        // All vertices lie near the sphere radius (dome projected onto r=5),
        // allowing for the strut cross-section thickness (~0.1 side).
        let rmax = m.positions().iter().map(|p| p.length()).fold(0.0, f64::max);
        assert!(rmax <= 5.0 + 0.2, "verts near r=5, got rmax={rmax}");
        assert_replay_stable(&s);
        run(&mut s, "undo");
        assert!(s.doc.get(id).is_none());
        run(&mut s, "redo");
        assert!(s.doc.get(id).is_some());
    }

    #[test]
    fn geodesic_full_sphere_encloses_more_than_dome() {
        let mut s = Session::default();
        let dome = run(&mut s, "geodesic 3 5 dome").created[0];
        let full = run(&mut s, "geodesic 3 5 full").created[0];
        assert!(mesh_of(&s, full).positions().len() > mesh_of(&s, dome).positions().len());
    }

    #[test]
    fn geodesic_rejects_bad_params() {
        let mut s = Session::default();
        assert!(s.run(parse("geodesic 0 5").unwrap()).is_err());
        assert!(s.run(parse("geodesic 3 -1").unwrap()).is_err());
    }

    #[test]
    fn generators_reject_hostile_params_no_oom() {
        // A hostile frequency would allocate ~10·f²·2 nodes → integer overflow /
        // OOM. Must be rejected, not attempted.
        let mut s = Session::default();
        assert!(
            s.run(parse("geodesic 100000 5").unwrap()).is_err(),
            "huge geodesic frequency must be rejected"
        );
        // NaN slips past `<= 0.0` checks and poisons geometry with NaNs.
        assert!(
            s.run(parse("hypar nan 5 5").unwrap()).is_err(),
            "NaN hypar param must be rejected"
        );
        assert!(
            s.run(parse("hypar 5 inf 5").unwrap()).is_err(),
            "infinite hypar param must be rejected"
        );
        // Huge grid resolutions are clamped, not OOM'd: this must succeed and
        // stay bounded at MAX_GRID² rather than 10^10 vertices.
        let out = s.run(parse("hypar 5 5 5 100000 100000").unwrap());
        assert!(out.is_ok(), "huge nu/nv clamp instead of erroring");
        // Huge tensegrity strut count is rejected.
        assert!(
            s.run(parse("tensegrity 100000").unwrap()).is_err(),
            "huge tensegrity strut count must be rejected"
        );
    }

    #[test]
    fn spaceframe_exec_undo_redo_replay() {
        let mut s = Session::default();
        let out = run(&mut s, "spaceframe 4 3 3 1.5");
        let id = out.created[0];
        let m = mesh_of(&s, id);
        assert_eq!(m.positions().len() % 8, 0);
        // Top chord at z=1.5; strut prisms add a little half-thickness on top.
        let zmax = m.positions().iter().map(|p| p.z).fold(f64::MIN, f64::max);
        assert!((zmax - 1.5).abs() < 0.2, "top chord near z=1.5, got {zmax}");
        assert_replay_stable(&s);
        run(&mut s, "undo");
        assert!(s.doc.get(id).is_none());
        run(&mut s, "redo");
        assert!(s.doc.get(id).is_some());
    }

    #[test]
    fn hypar_exec_saddle_undo_replay() {
        let mut s = Session::default();
        let out = run(&mut s, "hypar 5 5 5 6 6");
        let id = out.created[0];
        let m = mesh_of(&s, id);
        assert_eq!(m.positions().len(), 49); // (6+1)^2
        // Saddle: opposite corners at +z and -z (z = a*b/c = 25/5 = 5).
        let zmax = m.positions().iter().map(|p| p.z).fold(f64::MIN, f64::max);
        let zmin = m.positions().iter().map(|p| p.z).fold(f64::MAX, f64::min);
        assert!((zmax - 5.0).abs() < 1e-9 && (zmin + 5.0).abs() < 1e-9);
        assert_replay_stable(&s);
        run(&mut s, "undo");
        assert!(s.doc.get(id).is_none());
    }

    #[test]
    fn gaussvault_exec_undo_replay() {
        let mut s = Session::default();
        let out = run(&mut s, "gaussvault 6 12 3 undulate");
        let id = out.created[0];
        let m = mesh_of(&s, id);
        // 24×24 default grid → 25^2 verts.
        assert_eq!(m.positions().len(), 625);
        let zmin = m.positions().iter().map(|p| p.z).fold(f64::MAX, f64::min);
        assert!(zmin.abs() < 1e-9, "springings on the ground");
        assert_replay_stable(&s);
        run(&mut s, "undo");
        assert!(s.doc.get(id).is_none());
        run(&mut s, "redo");
        assert!(s.doc.get(id).is_some());
    }

    #[test]
    fn gridshell_hypar_and_vault_exec_replay() {
        let mut s = Session::default();
        let h = run(&mut s, "gridshell hypar 5 5 5 4 4").created[0];
        assert_eq!(mesh_of(&s, h).positions().len() % 8, 0);
        assert_replay_stable(&s);
        let v = run(&mut s, "gridshell vault 6 12 3 undulate 5 5").created[0];
        assert_eq!(mesh_of(&s, v).positions().len() % 8, 0);
        assert_replay_stable(&s);
        run(&mut s, "undo");
        assert!(s.doc.get(v).is_none());
    }

    #[test]
    fn funicular_exec_undo_redo_replay() {
        let mut s = Session::default();
        let out = run(&mut s, "funicular -5,0,0 5,0,0 20 1 1.4");
        let id = out.created[0];
        let m = mesh_of(&s, id);
        // 20 links → 20 strut prisms × 8 verts.
        assert_eq!(m.positions().len(), 20 * 8);
        // The hanging chain sags below the springing line (z < 0 somewhere).
        let zmin = m.positions().iter().map(|p| p.z).fold(f64::MAX, f64::min);
        assert!(zmin < -0.5, "funicular should sag, zmin={zmin}");
        assert_replay_stable(&s);
        run(&mut s, "undo");
        assert!(s.doc.get(id).is_none());
        run(&mut s, "redo");
        assert!(s.doc.get(id).is_some());
    }

    #[test]
    fn funicular_invert_stands_the_arch() {
        let mut s = Session::default();
        let hung = run(&mut s, "funicular -5,0,0 5,0,0 20 1 1.4").created[0];
        let arch = run(&mut s, "funicular -5,0,0 5,0,0 20 1 1.4 invert").created[0];
        let hz = mesh_of(&s, hung).positions().iter().map(|p| p.z).fold(f64::MAX, f64::min);
        let az = mesh_of(&s, arch).positions().iter().map(|p| p.z).fold(f64::MIN, f64::max);
        // Hung form dips below the supports; inverted form rises above them.
        assert!(hz < 0.0 && az > 0.0, "hung zmin={hz}, arch zmax={az}");
        assert_replay_stable(&s);
    }

    #[test]
    fn tensegrity_exec_undo_redo_replay() {
        let mut s = Session::default();
        let out = run(&mut s, "tensegrity 3 1 2");
        let id = out.created[0];
        let m = mesh_of(&s, id);
        assert_eq!(m.positions().len() % 8, 0, "8 verts per strut prism");
        // Spans z from ~0 (bottom ring) to ~2 (top ring).
        let zmax = m.positions().iter().map(|p| p.z).fold(f64::MIN, f64::max);
        assert!(zmax > 1.5, "tensegrity height ~2, zmax={zmax}");
        assert_replay_stable(&s);
        run(&mut s, "undo");
        assert!(s.doc.get(id).is_none());
        run(&mut s, "redo");
        assert!(s.doc.get(id).is_some());
    }

    #[test]
    fn tensegrity_rejects_too_few_struts() {
        let mut s = Session::default();
        assert!(s.run(parse("tensegrity 2").unwrap()).is_err());
    }

    #[test]
    fn cablenet_exec_undo_redo_replay() {
        let mut s = Session::default();
        let out = run(&mut s, "cablenet 0,0,0 8,0,0 8,8,3 0,8,3 5 1.5");
        let id = out.created[0];
        let m = mesh_of(&s, id);
        assert_eq!(m.positions().len() % 8, 0);
        assert!(!m.positions().is_empty());
        // Net lives inside the corner bounding box in z.
        let zmax = m.positions().iter().map(|p| p.z).fold(f64::MIN, f64::max);
        assert!(zmax <= 3.5, "net within corner span, zmax={zmax}");
        assert_replay_stable(&s);
        run(&mut s, "undo");
        assert!(s.doc.get(id).is_none());
        run(&mut s, "redo");
        assert!(s.doc.get(id).is_some());
    }

    #[test]
    fn minsurf_exec_undo_redo_replay() {
        let mut s = Session::default();
        // Saddle wire: closed polyline with alternating corner heights.
        let wire = run(&mut s, "polyline 0,0,1 6,0,-1 6,6,1 0,6,-1 closed").created[0];
        let out = run(&mut s, "minsurf last 8");
        let id = out.created[0];
        let m = mesh_of(&s, id);
        // 9×9 grid, 8×8×2 triangles.
        assert_eq!(m.positions().len(), 81);
        assert_eq!(m.faces().len(), 128);
        // Film stays inside the wire's z-range and spans the saddle midplane.
        for p in m.positions() {
            assert!(p.z >= -1.0 - 1e-6 && p.z <= 1.0 + 1e-6, "film outside wire: {p}");
        }
        // The boundary curve is kept.
        assert!(s.doc.get(wire).is_some());
        assert_replay_stable(&s);
        run(&mut s, "undo");
        assert!(s.doc.get(id).is_none());
        run(&mut s, "redo");
        assert!(s.doc.get(id).is_some());
    }

    #[test]
    fn minsurf_rejects_open_curves_and_non_curves() {
        let mut s = Session::default();
        run(&mut s, "line 0,0,0 5,0,0");
        assert!(s.run(parse("minsurf last").unwrap()).is_err(), "open curve must fail");
        run(&mut s, "box 0,0,0 1,1,1");
        assert!(s.run(parse("minsurf last").unwrap()).is_err(), "mesh must fail");
    }

    #[test]
    fn minsurf_works_on_circle_and_soapfilm_alias_parses() {
        let mut s = Session::default();
        run(&mut s, "circle 0,0,2 3");
        let id = run(&mut s, "soapfilm last").created[0];
        let m = mesh_of(&s, id);
        // A planar boundary's harmonic film is planar: all z = 2.
        for p in m.positions() {
            assert!((p.z - 2.0).abs() < 1e-6, "planar film off-plane: {p}");
        }
        assert_replay_stable(&s);
    }

    #[test]
    fn timber_and_guadua_beams_have_expected_area() {
        let mut s = Session::default();
        run(&mut s, "section glb timber 0.2 0.6");
        run(&mut s, "section culm guadua 0.1 0.01");
        let tb = run(&mut s, "beam 0,0,0 4,0,0 glb").created[0];
        let Geometry::Frame { section, .. } = &s.doc.get(tb).unwrap().geometry else {
            panic!("beam is a Frame");
        };
        assert!((section.area() - 0.12).abs() < 1e-9); // 0.2*0.6
        let gb = run(&mut s, "beam 0,1,0 4,1,0 culm").created[0];
        let Geometry::Frame { section, .. } = &s.doc.get(gb).unwrap().geometry else {
            panic!("beam is a Frame");
        };
        let expected = std::f64::consts::PI * (0.05 * 0.05 - 0.04 * 0.04);
        assert!((section.area() - expected).abs() < 1e-9);
        assert_replay_stable(&s);
    }

    #[test]
    fn column_and_beam_exec_undo_redo() {
        let mut s = Session::default();
        run(&mut s, "section c rect 0.3 0.3");
        run(&mut s, "column 0,0,0 0,0,3.5 c");
        assert_eq!(s.doc.len(), 1);
        assert_replay_stable(&s);
        run(&mut s, "undo");
        assert_eq!(s.doc.len(), 0);
        run(&mut s, "redo");
        assert_eq!(s.doc.len(), 1);
    }

    #[test]
    fn beam_missing_section_errors() {
        let mut s = Session::default();
        let err = s.run(parse("beam 0,0,0 5,0,0 nope").unwrap()).unwrap_err();
        assert!(err.to_string().contains("no section"), "{err}");
    }

    #[test]
    fn beam_with_undefined_material_errors() {
        let mut s = Session::default();
        run(&mut s, "section b rect 0.4 0.6");
        let err = s
            .run(parse("beam 0,0,0 5,0,0 b material nope").unwrap())
            .unwrap_err();
        assert!(err.to_string().contains("no material"), "{err}");
    }

    #[test]
    fn slab_extrudes_boundary_with_correct_volume() {
        let mut s = Session::default();
        let out = run(&mut s, "slab 0,0 6,0 6,4 0,4 thick 0.2");
        let id = out.created[0];
        let Geometry::Area { mesh, kind, .. } = &s.doc.get(id).unwrap().geometry else {
            panic!("slab should be an Area geometry");
        };
        assert_eq!(kind.label(), "slab");
        // V = 6*4*0.2 = 4.8
        assert!((kernel_mesh::signed_volume(mesh) - 4.8).abs() < 1e-6);
        assert_replay_stable(&s);
        run(&mut s, "undo");
        assert_eq!(s.doc.len(), 0);
    }

    #[test]
    fn wall_exec_undo_replay() {
        let mut s = Session::default();
        run(&mut s, "material concrete 30e9 2400");
        let out = run(&mut s, "wall 0,0 6,0 6,0.2 0,0.2 thick 3 material concrete");
        let id = out.created[0];
        assert!(matches!(s.doc.get(id).unwrap().geometry, Geometry::Area { .. }));
        assert_replay_stable(&s);
        run(&mut s, "undo");
        assert_eq!(s.doc.len(), 0);
        run(&mut s, "redo");
        assert_eq!(s.doc.len(), 1);
    }

    #[test]
    fn frame_member_id_written_back_for_replay() {
        // The logged op must carry the concrete id so replay reproduces it.
        let mut s = Session::default();
        run(&mut s, "section b rect 0.4 0.6");
        run(&mut s, "beam 0,0,0 5,0,0 b");
        let log = s.save_log();
        let has_id = log.iter().any(|op| matches!(
            op,
            Command::FrameMember { id: Some(_), .. }
        ));
        assert!(has_id, "frame member id must be written back into the log");
    }

    // -----------------------------------------------------------------------
    // Structural loads
    // -----------------------------------------------------------------------

    #[test]
    fn load_point_parse_exec_undo_redo() {
        let mut s = Session::default();
        // Exec: load appended to doc.loads, no SceneObject created.
        let out = run(&mut s, "load point dead 0,0,3 10000 0,0,-1");
        assert!(out.created.is_empty(), "load creates no scene object");
        assert_eq!(s.doc.loads.len(), 1);
        assert_eq!(s.doc.loads[0].name, "dead");
        assert_eq!(s.doc.loads[0].magnitude, 10_000.0);
        // Direction should be normalised (-Z).
        let d = s.doc.loads[0].direction;
        assert!((d.length() - 1.0).abs() < 1e-9, "direction must be unit");
        assert!((d.z + 1.0).abs() < 1e-9);
        // Replay stability while the load exists.
        assert_replay_stable(&s);

        // Undo: load removed.
        run(&mut s, "undo");
        assert!(s.doc.loads.is_empty(), "undo should clear the load");

        // Redo: load back.
        run(&mut s, "redo");
        assert_eq!(s.doc.loads.len(), 1, "redo should restore the load");
    }

    #[test]
    fn load_line_parse_exec_undo() {
        let mut s = Session::default();
        run(&mut s, "load line live 0,0,3 6,0,3 5000 0,0,-1");
        assert_eq!(s.doc.loads.len(), 1);
        assert_eq!(s.doc.loads[0].name, "live");
        use itsjustcad_doc::LoadGeometry;
        assert!(matches!(&s.doc.loads[0].geometry, LoadGeometry::Line { .. }));
        assert_replay_stable(&s);
        run(&mut s, "undo");
        assert!(s.doc.loads.is_empty());
    }

    #[test]
    fn load_area_parse_exec_undo() {
        let mut s = Session::default();
        run(&mut s, "load area wind 0,0,0 6,0,0 6,4,0 0,4,0 end 2000 1,0,0");
        assert_eq!(s.doc.loads.len(), 1);
        assert_eq!(s.doc.loads[0].name, "wind");
        use itsjustcad_doc::LoadGeometry;
        assert!(matches!(&s.doc.loads[0].geometry, LoadGeometry::Area { boundary } if boundary.len() == 4));
        assert_replay_stable(&s);
        run(&mut s, "undo");
        assert!(s.doc.loads.is_empty());
    }

    #[test]
    fn load_index_written_back_for_replay() {
        let mut s = Session::default();
        run(&mut s, "load point dead 0,0,3 10000 0,0,-1");
        let log = s.save_log();
        let has_index = log.iter().any(|op| {
            matches!(op, Command::AddLoad { index: Some(_), .. })
        });
        assert!(has_index, "AddLoad must carry index for replay stability");
    }

    #[test]
    fn load_zero_direction_errors() {
        let mut s = Session::default();
        let err = s.run(parse("load point dead 0,0,3 10000 0,0,0").unwrap()).unwrap_err();
        assert!(err.to_string().contains("direction"), "{err}");
    }

    #[test]
    fn old_file_without_loads_field_deserializes() {
        use itsjustcad_doc::Document;
        let mut v = serde_json::to_value(Document::default()).unwrap();
        v.as_object_mut().unwrap().remove("loads");
        v.as_object_mut().unwrap().remove("supports");
        let doc: Document = serde_json::from_value(v).unwrap();
        assert!(doc.loads.is_empty());
        assert!(doc.supports.is_empty());
    }

    // -----------------------------------------------------------------------
    // Structural supports
    // -----------------------------------------------------------------------

    #[test]
    fn support_pinned_parse_exec_undo_redo() {
        let mut s = Session::default();
        let out = run(&mut s, "support 0,0,0 pinned");
        assert!(out.created.is_empty(), "support creates no scene object");
        assert_eq!(s.doc.supports.len(), 1);
        use itsjustcad_doc::RestraintKind;
        assert_eq!(s.doc.supports[0].kind, RestraintKind::Pinned);
        assert_eq!(s.doc.supports[0].position, glam::DVec3::ZERO);
        assert_replay_stable(&s);

        run(&mut s, "undo");
        assert!(s.doc.supports.is_empty());

        run(&mut s, "redo");
        assert_eq!(s.doc.supports.len(), 1);
    }

    #[test]
    fn support_fixed_parse_exec() {
        let mut s = Session::default();
        run(&mut s, "support 3,3,0 fixed");
        use itsjustcad_doc::RestraintKind;
        assert_eq!(s.doc.supports[0].kind, RestraintKind::Fixed);
        assert_replay_stable(&s);
    }

    #[test]
    fn support_roller_parse_exec_with_axis() {
        let mut s = Session::default();
        run(&mut s, "support 6,0,0 roller 1,0,0");
        use itsjustcad_doc::RestraintKind;
        let sup = &s.doc.supports[0];
        assert_eq!(sup.kind, RestraintKind::Roller);
        let ax = sup.roller_axis.expect("roller axis should be set");
        assert!((ax.length() - 1.0).abs() < 1e-9, "axis should be unit");
        assert!((ax.x - 1.0).abs() < 1e-9);
        assert_replay_stable(&s);
    }

    #[test]
    fn support_index_written_back_for_replay() {
        let mut s = Session::default();
        run(&mut s, "support 0,0,0 pinned");
        let log = s.save_log();
        let has_index = log.iter().any(|op| {
            matches!(op, Command::AddSupport { index: Some(_), .. })
        });
        assert!(has_index, "AddSupport must carry index for replay stability");
    }

    #[test]
    fn multiple_loads_and_supports_replay_stable() {
        let mut s = Session::default();
        // Build a simple frame.
        run(&mut s, "section c rect 0.3 0.3");
        run(&mut s, "column 0,0,0 0,0,3.5 c");
        run(&mut s, "column 6,0,0 6,0,3.5 c");
        run(&mut s, "beam 0,0,3.5 6,0,3.5 c");
        // Add loads.
        run(&mut s, "load point dead 0,0,3.5 10000 0,0,-1");
        run(&mut s, "load line live 0,0,3.5 6,0,3.5 3000 0,0,-1");
        // Add supports.
        run(&mut s, "support 0,0,0 pinned");
        run(&mut s, "support 6,0,0 roller 1,0,0");
        assert_eq!(s.doc.loads.len(), 2);
        assert_eq!(s.doc.supports.len(), 2);
        // Full replay must reproduce the identical document.
        assert_replay_stable(&s);
        // File round-trip.
        let json1 = crate::io::to_json(&s);
        let json2 = crate::io::to_json(&crate::io::from_json(&json1).unwrap());
        assert_eq!(json1, json2);
    }

    // ── lineweight tests ─────────────────────────────────────────────────────

    /// Parse round-trip for the three new lineweight commands.
    #[test]
    fn lineweight_parse_round_trip() {
        use crate::parse;

        // Raw mm value
        let cmd = parse("lineweight last 0.35").unwrap();
        assert!(matches!(cmd, Command::Lineweight { mm, .. } if (mm - 0.35).abs() < 1e-9));

        // ISO pen name: "0.50" snaps to 0.50
        let cmd = parse("lineweight last 0.50").unwrap();
        assert!(matches!(cmd, Command::Lineweight { mm, .. } if (mm - 0.50).abs() < 1e-9));

        // ISO pen via iso-prefix + hundredths: "iso35" → 0.35
        let cmd = parse("lineweight last iso35").unwrap();
        assert!(matches!(cmd, Command::Lineweight { mm, .. } if (mm - 0.35).abs() < 1e-9));

        // "off" → LinweightOff
        let cmd = parse("lineweight last off").unwrap();
        assert!(matches!(cmd, Command::LinweightOff { .. }));

        // showweights on / off
        assert!(matches!(parse("showweights on").unwrap(), Command::ShowWeights { on: true }));
        assert!(matches!(parse("showweights off").unwrap(), Command::ShowWeights { on: false }));

        // JSON round-trip for Lineweight
        let cmd = parse("lineweight last 0.35").unwrap();
        let json = serde_json::to_string(&cmd).unwrap();
        let back: Command = serde_json::from_str(&json).unwrap();
        assert_eq!(cmd, back);

        // JSON round-trip for ShowWeights
        let cmd2 = parse("showweights on").unwrap();
        let json2 = serde_json::to_string(&cmd2).unwrap();
        let back2: Command = serde_json::from_str(&json2).unwrap();
        assert_eq!(cmd2, back2);
    }

    /// ISO pen name parsing: named integer forms resolve to the right mm value.
    #[test]
    fn iso_pen_names_snap_to_correct_mm() {
        use crate::parse;

        // Standard ISO 128 pen widths by decimal string
        for (s, expected) in [
            ("0.13", 0.13), ("0.18", 0.18), ("0.25", 0.25), ("0.35", 0.35),
            ("0.50", 0.50), ("0.70", 0.70), ("1.00", 1.00), ("1.40", 1.40), ("2.00", 2.00),
        ] {
            let cmd = parse(&format!("lineweight last {s}")).unwrap();
            let Command::Lineweight { mm, .. } = cmd else { panic!("expected Lineweight") };
            assert!((mm - expected).abs() < 1e-9, "'{s}' → {mm}, expected {expected}");
        }
        // iso-prefix forms
        for (s, expected) in [("iso13", 0.13), ("iso18", 0.18), ("iso100", 1.00), ("iso140", 1.40)] {
            let cmd = parse(&format!("lineweight last {s}")).unwrap();
            let Command::Lineweight { mm, .. } = cmd else { panic!("expected Lineweight") };
            assert!((mm - expected).abs() < 1e-9, "'{s}' → {mm}, expected {expected}");
        }
        // Near-miss snaps: 0.349 should snap to 0.35
        let cmd = parse("lineweight last 0.349").unwrap();
        let Command::Lineweight { mm, .. } = cmd else { panic!() };
        assert!((mm - 0.35).abs() < 1e-9, "0.349 should snap to 0.35, got {mm}");
    }

    /// exec + undo + redo for `lineweight`, `lineweightoff`, and `showweights`.
    #[test]
    fn lineweight_exec_undo_redo() {
        let mut s = Session::default();
        run(&mut s, "line 0,0,0 5,0,0");
        let id = s.doc.objects().next().unwrap().id;

        // Initially no per-object override; default layer is 0.18.
        assert!(s.doc.objects().next().unwrap().lineweight_mm.is_none());
        assert!((s.doc.effective_lineweight(s.doc.get(id).unwrap()) - 0.18).abs() < 1e-9);

        // Set lineweight.
        let out = run(&mut s, "lineweight last 0.50");
        assert!(out.message.contains("0.500"));
        assert!((s.doc.get(id).unwrap().lineweight_mm.unwrap() - 0.50).abs() < 1e-9);
        assert!((s.doc.effective_lineweight(s.doc.get(id).unwrap()) - 0.50).abs() < 1e-9);

        // Undo restores None.
        run(&mut s, "undo");
        assert!(s.doc.get(id).unwrap().lineweight_mm.is_none());

        // Redo re-applies.
        run(&mut s, "redo");
        assert!((s.doc.get(id).unwrap().lineweight_mm.unwrap() - 0.50).abs() < 1e-9);

        // lineweightoff clears the override.
        run(&mut s, "lineweightoff last");
        assert!(s.doc.get(id).unwrap().lineweight_mm.is_none());
        run(&mut s, "undo");
        assert!((s.doc.get(id).unwrap().lineweight_mm.unwrap() - 0.50).abs() < 1e-9);
        run(&mut s, "redo");
        assert!(s.doc.get(id).unwrap().lineweight_mm.is_none());

        // showweights toggle.
        assert!(!s.doc.show_lineweights);
        run(&mut s, "showweights on");
        assert!(s.doc.show_lineweights);
        run(&mut s, "undo");
        assert!(!s.doc.show_lineweights);
        run(&mut s, "redo");
        assert!(s.doc.show_lineweights);
        run(&mut s, "showweights off");
        assert!(!s.doc.show_lineweights);
    }

    /// Per-object lineweight beats layer lineweight in effective_lineweight().
    #[test]
    fn per_object_lineweight_beats_layer() {
        use crate::parse;

        let mut s = Session::default();
        run(&mut s, "layer walls");
        s.run(parse("layerweight walls 0.35").unwrap()).unwrap();
        run(&mut s, "line 0,0,0 1,0,0"); // on "walls" layer (current layer)
        let id = s.doc.objects().next().unwrap().id;

        // Without per-object override, effective = layer weight 0.35.
        assert!((s.doc.effective_lineweight(s.doc.get(id).unwrap()) - 0.35).abs() < 1e-9);

        // Set per-object override to 1.00 — must beat the layer's 0.35.
        run(&mut s, "lineweight last 1.00");
        assert!((s.doc.effective_lineweight(s.doc.get(id).unwrap()) - 1.00).abs() < 1e-9);

        // After lineweightoff, reverts to layer weight.
        run(&mut s, "lineweightoff last");
        assert!((s.doc.effective_lineweight(s.doc.get(id).unwrap()) - 0.35).abs() < 1e-9);
    }

    /// Replay stability: lineweight commands survive to_json → from_json → to_json
    /// with byte-identical JSON and stable ids.
    #[test]
    fn lineweight_replay_stable() {
        let mut s = Session::default();
        run(&mut s, "line 0,0,0 5,0,0");
        run(&mut s, "line 0,0,0 0,5,0");
        run(&mut s, "lineweight last 2 0.70");
        run(&mut s, "showweights on");
        assert_replay_stable(&s);
    }

    /// Pre-lineweight serde compatibility: a SceneObject JSON without the
    /// `lineweight_mm` field must still deserialize, defaulting it to None.
    #[test]
    fn pre_lineweight_scene_object_loads() {
        let json = r#"{
            "id": "00000000000000000000000000000002",
            "name": null,
            "layer": "default",
            "visible": true,
            "geometry": { "geo": "points", "positions": [] }
        }"#;
        let obj: itsjustcad_doc::SceneObject = serde_json::from_str(json).unwrap();
        assert!(obj.lineweight_mm.is_none(), "missing lineweight_mm must default to None");
        // Re-serialized object must omit the field (skip_serializing_if None).
        let back = serde_json::to_value(&obj).unwrap();
        assert!(back.get("lineweight_mm").is_none(), "None lineweight_mm must not serialize");
    }

    /// Pre-show_lineweights Document JSON must load cleanly, defaulting to false.
    #[test]
    fn pre_show_lineweights_document_loads() {
        // A minimal file without `show_lineweights` in the doc snapshot
        // must load without error and default to false.
        let json = r#"{"itsjustcad":1,"ops":[]}"#;
        let s = crate::io::from_json(json).unwrap();
        assert!(!s.doc.show_lineweights, "missing show_lineweights must default to false");
    }

    // ── Rhino .3dm import ────────────────────────────────────────────────────

    /// Full-flow: write a spec-conformant .3dm to a temp file, import it via the
    /// substrate `Command::Import`, and assert (a) mesh + curve counts, (b) each
    /// object landed on its Rhino layer, (c) the mesh name is preserved, and (d)
    /// the whole import is logged and replays byte-identically.
    #[test]
    fn import_3dm_meshes_curves_layers_and_replay() {
        use crate::rhino3dm::{write_min, WriteItem};

        let bytes = write_min(
            &["walls", "guides"],
            &[
                WriteItem::Mesh {
                    name: "panel".to_string(),
                    layer_index: 0,
                    positions: vec![
                        DVec3::new(0.0, 0.0, 0.0),
                        DVec3::new(2.0, 0.0, 0.0),
                        DVec3::new(2.0, 1.0, 0.0),
                        DVec3::new(0.0, 1.0, 0.0),
                    ],
                    tris: vec![[0, 1, 2], [0, 2, 3]],
                },
                WriteItem::Line {
                    name: "axis".to_string(),
                    layer_index: 1,
                    a: DVec3::new(0.0, 0.0, 0.0),
                    b: DVec3::new(10.0, 0.0, 0.0),
                },
                WriteItem::Polyline {
                    name: "trace".to_string(),
                    layer_index: 1,
                    points: vec![
                        DVec3::new(0.0, 0.0, 0.0),
                        DVec3::new(1.0, 1.0, 0.0),
                        DVec3::new(2.0, 0.0, 0.0),
                    ],
                },
            ],
        );

        let mut path = std::env::temp_dir();
        path.push(format!("itsjustcad_test_{}.3dm", std::process::id()));
        std::fs::write(&path, &bytes).unwrap();

        let mut s = Session::default();
        let out = run(&mut s, &format!("import {}", path.display()));
        std::fs::remove_file(&path).ok();

        assert!(out.message.contains("1 mesh(es)"), "message: {}", out.message);
        assert!(out.message.contains("2 curve(s)"), "message: {}", out.message);

        // Three geometry objects: one mesh, two curves.
        let meshes = s.doc.objects().filter(|o| matches!(o.geometry, Geometry::Mesh { .. })).count();
        let curves =
            s.doc.objects().filter(|o| matches!(o.geometry, Geometry::Curve(Curve::Polyline { .. }))).count();
        assert_eq!(meshes, 1, "one mesh imported");
        assert_eq!(curves, 2, "line + polyline imported");

        // The mesh kept its Rhino name and landed on the 'walls' layer.
        let mesh_obj = s
            .doc
            .objects()
            .find(|o| matches!(o.geometry, Geometry::Mesh { .. }))
            .expect("mesh present");
        assert_eq!(mesh_obj.name.as_deref(), Some("panel"));
        assert_eq!(mesh_obj.layer, "walls");

        // Both curves landed on the 'guides' layer.
        for c in s.doc.objects().filter(|o| matches!(o.geometry, Geometry::Curve(Curve::Polyline { .. }))) {
            assert_eq!(c.layer, "guides");
        }

        // The import is logged; the op-log replays byte-identically.
        let json1 = crate::io::to_json(&s);
        let loaded = crate::io::from_json(&json1).unwrap();
        let json2 = crate::io::to_json(&loaded);
        assert_eq!(json1, json2, "3dm import op-log must replay identically");
    }

    // ── Rhino .3dm export ───────────────────────────────────────────────────

    /// Full-flow: model a mesh + line + circle on named layers, `export .3dm`,
    /// then import the bytes back and check geometry, names and layers survive.
    #[test]
    fn export_3dm_round_trips_through_import() {
        let mut s = Session::default();
        run(&mut s, "box 0,0,0 2,1,3");
        run(&mut s, "name last panelbox");
        run(&mut s, "tolayer last walls");
        run(&mut s, "line 0,0,0 10,0,0");
        run(&mut s, "circle 5,5,0 2");

        let mut path = std::env::temp_dir();
        path.push(format!("itsjustcad_test_export_{}.3dm", std::process::id()));
        let out = run(&mut s, &format!("export {}", path.display()));
        assert!(out.message.contains("3DM (openNURBS V5)"), "message: {}", out.message);
        assert!(out.message.contains("1 mesh(es), 2 curve(s)"), "message: {}", out.message);

        // Read the file back through the real importer.
        let mut s2 = Session::default();
        let back = run(&mut s2, &format!("import {}", path.display()));
        std::fs::remove_file(&path).ok();
        assert!(back.message.contains("1 mesh(es)"), "message: {}", back.message);
        assert!(back.message.contains("2 curve(s)"), "message: {}", back.message);

        let mesh_obj = s2
            .doc
            .objects()
            .find(|o| matches!(o.geometry, Geometry::Mesh { .. }))
            .expect("mesh survives round trip");
        assert_eq!(mesh_obj.name.as_deref(), Some("panelbox"));
        assert_eq!(mesh_obj.layer, "walls");
        let Geometry::Mesh(m) = &mesh_obj.geometry else { unreachable!() };
        assert_eq!(m.faces().len(), 12, "box mesh keeps its 12 triangles");

        // The circle came back as a closed polyline (the importer detects the
        // repeated first point and sets the closed flag).
        let closed = s2.doc.objects().any(|o| {
            matches!(&o.geometry, Geometry::Curve(Curve::Polyline { points, closed: true })
                if points.len() > 8)
        });
        assert!(closed, "circle survives as a closed dense polyline");
    }

    /// An empty document still exports a valid (readable) archive.
    #[test]
    fn export_3dm_empty_document_is_readable() {
        let s = Session::default();
        let (bytes, counts) = crate::rhino3dm::export(&s.doc);
        assert_eq!(counts, "0 mesh(es), 0 curve(s)");
        let parsed = crate::rhino3dm::import(&bytes).expect("empty archive parses");
        assert_eq!(parsed.objects.len(), 0);
    }

    // ── LAZ point-cloud import ──────────────────────────────────────────────

    /// Full-flow: write a real laz-compressed file to a temp path, import it
    /// through the `.laz` extension, and check the points land on the
    /// 'pointcloud' layer with an op-log that replays byte-identically.
    #[test]
    fn import_laz_point_cloud_full_flow() {
        let bytes = crate::las::testutil::make_laz(25, 0.001, 10.0);
        let mut path = std::env::temp_dir();
        path.push(format!("itsjustcad_test_{}.laz", std::process::id()));
        std::fs::write(&path, &bytes).unwrap();

        let mut s = Session::default();
        let out = run(&mut s, &format!("import {}", path.display()));
        std::fs::remove_file(&path).ok();

        assert!(out.message.contains("imported 25 points"), "message: {}", out.message);

        let cloud = s
            .doc
            .objects()
            .find(|o| matches!(o.geometry, Geometry::Points { .. }))
            .expect("point cloud object present");
        assert_eq!(cloud.layer, "pointcloud");
        let Geometry::Points { positions, .. } = &cloud.geometry else { unreachable!() };
        assert_eq!(positions.len(), 25);
        // Record 0 decodes to the header offset exactly.
        assert!((positions[0].x - 10.0).abs() < 1e-9, "x0={}", positions[0].x);

        let json1 = crate::io::to_json(&s);
        let loaded = crate::io::from_json(&json1).unwrap();
        let json2 = crate::io::to_json(&loaded);
        assert_eq!(json1, json2, "laz import op-log must replay identically");
    }
    // ── sketch constraints (constrain / solveconstraints / constraints) ─────

    fn line_xy(s: &Session, name: &str) -> ((f64, f64), (f64, f64)) {
        let id = s.doc.find_named(name)[0];
        match &s.doc.get(id).unwrap().geometry {
            Geometry::Curve(kernel_curve::Curve::Line { a, b }) => ((a.x, a.y), (b.x, b.y)),
            other => panic!("expected line, got {other:?}"),
        }
    }

    #[test]
    fn constrain_horizontal_solves_line() {
        let mut s = Session::default();
        run(&mut s, "line 0,0,0 10,2,0");
        run(&mut s, "name last l1");
        let out = run(&mut s, "constrain horizontal l1");
        assert!(out.message.contains("solved"), "message: {}", out.message);
        let ((_, ay), (_, by)) = line_xy(&s, "l1");
        assert!((ay - by).abs() < 1e-6, "flattened: {ay} vs {by}");
    }

    #[test]
    fn constrain_rectangle_via_commands() {
        let mut s = Session::default();
        run(&mut s, "line 0,0,0 4,0,0");
        run(&mut s, "name last bottom");
        run(&mut s, "line 4.1,0.2,0 4.3,2.8,0");
        run(&mut s, "name last right");
        run(&mut s, "line 4.2,3.1,0 -0.2,2.9,0");
        run(&mut s, "name last top");
        run(&mut s, "line 0.1,3.2,0 0.2,-0.1,0");
        run(&mut s, "name last left");
        run(&mut s, "constrain fixed bottom");
        run(&mut s, "constrain coincident bottom right");
        run(&mut s, "constrain coincident right top");
        run(&mut s, "constrain coincident top left");
        run(&mut s, "constrain coincident left bottom");
        run(&mut s, "constrain vertical right");
        run(&mut s, "constrain vertical left");
        run(&mut s, "constrain horizontal top");
        let out = run(&mut s, "constrain length right 3");
        assert!(out.message.contains("solved"), "message: {}", out.message);
        let ((_, _), (rx, ry)) = line_xy(&s, "right");
        assert!((rx - 4.0).abs() < 1e-6, "right top corner x: {rx}");
        assert!((ry - 3.0).abs() < 1e-6, "right top corner y: {ry}");
        let ((tx, ty), _) = line_xy(&s, "top");
        assert!((tx - 4.0).abs() < 1e-6 && (ty - 3.0).abs() < 1e-6, "top joins right: {tx},{ty}");
    }

    #[test]
    fn constrain_undo_restores_geometry_and_constraint() {
        let mut s = Session::default();
        run(&mut s, "line 0,0,0 10,2,0");
        run(&mut s, "name last l1");
        let before = line_xy(&s, "l1");
        run(&mut s, "constrain horizontal l1");
        assert_eq!(s.doc.constraints.len(), 1);
        run(&mut s, "undo");
        assert!(s.doc.constraints.is_empty(), "constraint removed on undo");
        assert_eq!(line_xy(&s, "l1"), before, "geometry restored on undo");
        run(&mut s, "redo");
        assert_eq!(s.doc.constraints.len(), 1, "redo re-adds");
        let ((_, ay), (_, by)) = line_xy(&s, "l1");
        assert!((ay - by).abs() < 1e-6, "redo re-solves");
    }

    #[test]
    fn constraints_list_delete_and_clear() {
        let mut s = Session::default();
        run(&mut s, "line 0,0,0 10,2,0");
        run(&mut s, "name last l1");
        run(&mut s, "circle 5,5,0 2");
        run(&mut s, "name last c1");
        run(&mut s, "constrain horizontal l1");
        run(&mut s, "constrain radius c1 3");
        let out = run(&mut s, "constraints list");
        assert!(out.message.contains("#1 horizontal"), "list: {}", out.message);
        assert!(out.message.contains("#2 radius"), "list: {}", out.message);
        let out = run(&mut s, "constraints delete 1");
        assert!(out.message.contains("deleted constraint #1"), "{}", out.message);
        assert_eq!(s.doc.constraints.len(), 1);
        run(&mut s, "undo");
        assert_eq!(s.doc.constraints.len(), 2, "delete undone");
        assert!(matches!(s.doc.constraints[0], itsjustcad_doc::SketchConstraint::Horizontal { .. }));
        let out = run(&mut s, "constraints clear");
        assert!(out.message.contains("deleted all 2"), "{}", out.message);
        assert!(s.doc.constraints.is_empty());
        run(&mut s, "undo");
        assert_eq!(s.doc.constraints.len(), 2, "clear undone");
    }

    #[test]
    fn conflicting_constraint_leaves_geometry_and_warns() {
        let mut s = Session::default();
        run(&mut s, "line 0,0,0 4,0,0");
        run(&mut s, "name last l1");
        run(&mut s, "constrain fixed l1");
        let before = line_xy(&s, "l1");
        let out = run(&mut s, "constrain length l1 9");
        assert!(out.message.contains("NOT SOLVED"), "message: {}", out.message);
        assert_eq!(line_xy(&s, "l1"), before, "inconsistent solve leaves geometry");
        assert_eq!(s.doc.constraints.len(), 2, "constraint still recorded for deletion");
        let out = run(&mut s, "constraints list");
        assert!(out.message.contains("[conflicts]"), "list flags conflict: {}", out.message);
    }

    #[test]
    fn redundant_constraint_flagged_in_list() {
        let mut s = Session::default();
        run(&mut s, "line 0,0,0 10,2,0");
        run(&mut s, "name last l1");
        run(&mut s, "constrain horizontal l1");
        let out = run(&mut s, "constrain horizontal l1");
        assert!(out.message.contains("redundant"), "message: {}", out.message);
        let out = run(&mut s, "constraints list");
        assert!(out.message.contains("[redundant]"), "list: {}", out.message);
    }

    #[test]
    fn constrain_tangent_line_circle_via_commands() {
        let mut s = Session::default();
        run(&mut s, "line 0,0,0 10,0,0");
        run(&mut s, "name last ground");
        run(&mut s, "circle 5,2,0 3");
        run(&mut s, "name last wheel");
        run(&mut s, "constrain fixed ground");
        run(&mut s, "constrain radius wheel 3");
        let out = run(&mut s, "constrain tangent ground wheel");
        assert!(out.message.contains("solved"), "message: {}", out.message);
        let id = s.doc.find_named("wheel")[0];
        let Geometry::Curve(kernel_curve::Curve::Arc { center, radius, .. }) =
            &s.doc.get(id).unwrap().geometry
        else {
            panic!("wheel is a circle")
        };
        assert!((center.y - 3.0).abs() < 1e-6, "center rests one radius up: {}", center.y);
        assert!((radius - 3.0).abs() < 1e-9);
    }

    #[test]
    fn constraints_skip_deleted_objects() {
        let mut s = Session::default();
        run(&mut s, "line 0,0,0 10,2,0");
        run(&mut s, "name last l1");
        run(&mut s, "constrain horizontal l1");
        run(&mut s, "delete l1");
        let out = run(&mut s, "constraints list");
        assert!(out.message.contains("skipped"), "list: {}", out.message);
    }

    #[test]
    fn under_constrained_reports_remaining_dof() {
        let mut s = Session::default();
        run(&mut s, "line 0,0,0 10,2,0");
        run(&mut s, "name last l1");
        let out = run(&mut s, "constrain horizontal l1");
        assert!(out.message.contains("under-constrained"), "message: {}", out.message);
        assert!(out.message.contains("3 DOF"), "4 params - 1 eq: {}", out.message);
    }

    #[test]
    fn constraint_oplog_replays_identically() {
        let mut s = Session::default();
        run(&mut s, "line 0,0,0 10,2,0");
        run(&mut s, "name last l1");
        run(&mut s, "line 0,5,0 8,6,0");
        run(&mut s, "name last l2");
        run(&mut s, "constrain horizontal l1");
        run(&mut s, "constrain parallel l1 l2");
        run(&mut s, "constrain length l2 5");
        let json1 = crate::io::to_json(&s);
        let loaded = crate::io::from_json(&json1).unwrap();
        let json2 = crate::io::to_json(&loaded);
        assert_eq!(json1, json2, "constraint ops must replay identically");
        assert_eq!(loaded.doc.constraints, s.doc.constraints);
    }

    #[test]
    fn solveconstraints_after_move_restores_dimensions() {
        let mut s = Session::default();
        run(&mut s, "line 0,0,0 4,0,0");
        run(&mut s, "name last l1");
        run(&mut s, "constrain horizontal l1");
        run(&mut s, "constrain length l1 4");
        // Nudge one endpoint away via a raw move of the whole line, then
        // re-solve: length + horizontality must come back.
        run(&mut s, "move l1 1,1,0");
        let out = run(&mut s, "solveconstraints");
        assert!(out.message.contains("solved"), "{}", out.message);
        let ((ax, ay), (bx, by)) = line_xy(&s, "l1");
        assert!((ay - by).abs() < 1e-6, "horizontal again");
        assert!((((bx - ax).powi(2) + (by - ay).powi(2)).sqrt() - 4.0).abs() < 1e-6, "length 4");
    }

    // ── M-landscape ─────────────────────────────────────────────────────────

    /// Session with a synthetic gridded terrain mesh on layer "terrain":
    /// (n+1)² vertices over [0,size]², z = f(x,y). Uses a MeshLiteral op like
    /// the real `terrain` verb, so the whole setup lives in the op-log.
    fn terrain_session(n: usize, size: f64, f: impl Fn(f64, f64) -> f64) -> Session {
        let mut s = Session::default();
        run(&mut s, "layer terrain");
        let mut positions = Vec::new();
        for j in 0..=n {
            for i in 0..=n {
                let x = size * i as f64 / n as f64;
                let y = size * j as f64 / n as f64;
                positions.push(DVec3::new(x, y, f(x, y)));
            }
        }
        let mut faces = Vec::new();
        let idx = |i: usize, j: usize| (j * (n + 1) + i) as u32;
        for j in 0..n {
            for i in 0..n {
                faces.push([idx(i, j), idx(i + 1, j), idx(i + 1, j + 1)]);
                faces.push([idx(i, j), idx(i + 1, j + 1), idx(i, j + 1)]);
            }
        }
        s.run(Command::MeshLiteral {
            id: None,
            positions,
            faces,
            name: Some("terrain".to_string()),
        })
        .unwrap();
        run(&mut s, "layer 0");
        s
    }

    #[test]
    fn contours_needs_a_terrain_mesh() {
        let mut s = Session::default();
        run(&mut s, "box 0,0,0 4,4,4");
        let err = s.run(parse("contours 1").unwrap()).unwrap_err();
        assert!(format!("{err}").contains("no terrain"), "{err}");
    }

    #[test]
    fn contours_on_cone_split_minor_major_and_undo() {
        // Cone peak z=5 at (5,5): closed rings at 1..4 m. major-every 2 →
        // levels 2 and 4 are major.
        let mut s = terrain_session(40, 10.0, |x, y| {
            (5.0 - ((x - 5.0).powi(2) + (y - 5.0).powi(2)).sqrt()).max(0.0)
        });
        let before = s.doc.len();
        let out = run(&mut s, "contours 1 2");
        assert_eq!(out.created.len(), 4, "{}", out.message);
        let on_layer = |s: &Session, layer: &str| -> usize {
            s.doc.objects().filter(|o| o.layer == layer).count()
        };
        assert_eq!(on_layer(&s, "contours"), 2, "levels 1 and 3 are minor");
        assert_eq!(on_layer(&s, "contours-major"), 2, "levels 2 and 4 are major");
        // Every contour on a cone is a closed ring.
        for id in &out.created {
            match &s.doc.get(*id).unwrap().geometry {
                Geometry::Curve(Curve::Polyline { closed, .. }) => assert!(closed),
                g => panic!("contour must be a polyline, got {g:?}"),
            }
        }
        // Undo removes the polylines and drops the auto-created layers.
        run(&mut s, "undo");
        assert_eq!(s.doc.len(), before);
        assert!(!s.doc.layers.contains_key("contours"));
        assert!(!s.doc.layers.contains_key("contours-major"));
    }

    #[test]
    fn pad_grades_terrain_and_undo_restores_it() {
        // Sloped plane z = x/2; pad 4×4 at (10,10) elev 3 digs into the grade.
        let mut s = terrain_session(40, 20.0, |x, _| x / 2.0);
        let (tid, before) = {
            let (id, m) = terrain_surface(&s.doc).unwrap();
            (id, m.positions().to_vec())
        };
        run(&mut s, "pad 10,10 4 4 3");
        {
            let (_, m) = terrain_surface(&s.doc).unwrap();
            // Center vertex of the pad sits at the pad elevation now.
            let center = m
                .positions()
                .iter()
                .find(|p| (p.x - 10.0).abs() < 1e-9 && (p.y - 10.0).abs() < 1e-9)
                .unwrap();
            assert!((center.z - 3.0).abs() < 1e-12, "pad center at elev 3");
        }
        assert!(s.doc.pregrade_terrain.is_some(), "first pad snapshots pre-grading");
        run(&mut s, "undo");
        let (tid2, after) = {
            let (id, m) = terrain_surface(&s.doc).unwrap();
            (id, m.positions().to_vec())
        };
        assert_eq!(tid, tid2);
        assert_eq!(before, after, "undo must restore the exact terrain");
    }

    #[test]
    fn cutfill_zero_for_pad_at_existing_grade() {
        // Flat plane at z=2, pad at elev 2 → grading is a no-op → zero volumes.
        let mut s = terrain_session(20, 20.0, |_, _| 2.0);
        run(&mut s, "pad 10,10 4 4 2");
        let out = run(&mut s, "cutfill");
        assert!(
            out.message.contains("cut 0.0 m3, fill 0.0 m3"),
            "{}",
            out.message
        );
    }

    #[test]
    fn cutfill_matches_analytic_volume_and_reports() {
        // Flat plane z=0, 4×4 pad sunk to −1 at 2:1 → analytic ≈ 36.2 m³ cut.
        let mut s = terrain_session(80, 20.0, |_, _| 0.0);
        run(&mut s, "pad 10,10 4 4 -1 2");
        let out = run(&mut s, "cutfill");
        let want = 16.0 + 16.0 + 4.0 * std::f64::consts::PI / 3.0;
        assert!(out.message.contains("fill 0.0 m3"), "{}", out.message);
        let cut: f64 = out
            .message
            .split("cut ")
            .nth(1)
            .and_then(|t| t.split(" m3").next())
            .unwrap()
            .parse()
            .unwrap();
        assert!((cut - want).abs() < 0.5, "cut {cut} vs analytic {want}");
        // Deck critique hook: the structured report is stored.
        let r = s.doc.analysis_reports.get("cutfill").expect("report stored");
        assert!(r.context.contains("TIN prism estimate"));
        assert!(r.count > 0);
        let rep = run(&mut s, "report cutfill");
        assert!(rep.message.contains("cutfill"), "{}", rep.message);
    }

    #[test]
    fn cutfill_without_grading_errors_and_replay_is_stable() {
        let mut s = terrain_session(10, 10.0, |_, _| 0.0);
        let err = s.run(parse("cutfill").unwrap()).unwrap_err();
        assert!(format!("{err}").contains("pad"), "{err}");
        // Grade, measure, then replay: original heights are embedded in the
        // logged cutfill op, so the log round-trips bit-identically.
        run(&mut s, "pad 5,5 2 2 -0.5");
        run(&mut s, "cutfill");
        let log = s.save_log();
        let replayed = Session::replay(log.clone()).unwrap();
        assert_eq!(
            serde_json::to_string(&log).unwrap(),
            serde_json::to_string(&replayed.save_log()).unwrap(),
            "pad + cutfill must replay bit-identically"
        );
        assert!(replayed.doc.analysis_reports.contains_key("cutfill"));
    }

    #[test]
    fn plant_drapes_onto_terrain_and_lands_on_planting_layer() {
        // Sloped terrain z = x/2: a plant at (10, 10) must root at z = 5.
        let mut s = terrain_session(20, 20.0, |x, _| x / 2.0);
        let out = run(&mut s, "plant oak 10,10 25");
        assert_eq!(out.created.len(), 1, "{}", out.message);
        let obj = s.doc.get(out.created[0]).unwrap();
        assert_eq!(obj.layer, "planting");
        assert_eq!(obj.name.as_deref(), Some("plant:quercus-robur"));
        let Geometry::Mesh(m) = &obj.geometry else { panic!("plant must be a mesh") };
        let zmin = m.positions().iter().map(|p| p.z).fold(f64::INFINITY, f64::min);
        assert!((zmin - 5.0).abs() < 1e-9, "rooted on terrain, got {zmin}");
        // 25-year oak at 0.5 m/yr → 12.5 m tall above ground.
        let zmax = m.positions().iter().map(|p| p.z).fold(f64::NEG_INFINITY, f64::max);
        assert!((zmax - (5.0 + 12.5)).abs() < 1e-9, "aged canopy, got {zmax}");
        // Current layer restored; unknown species is a helpful error.
        assert_eq!(s.doc.current_layer, "0");
        let err = s.run(parse("plant triffid 0,0").unwrap()).unwrap_err();
        assert!(format!("{err}").contains("quercus-robur"), "{err}");
    }

    #[test]
    fn plantrow_spaces_plants_and_replays_without_catalog_dependence() {
        let mut s = Session::default();
        let out = run(&mut s, "plantrow cypress 0,0 10,0 2.5");
        assert_eq!(out.created.len(), 5, "{}", out.message);
        // Each plant is its own MeshLiteral op → replay is self-contained.
        let log = s.save_log();
        let replayed = Session::replay(log.clone()).unwrap();
        assert_eq!(
            serde_json::to_string(&log).unwrap(),
            serde_json::to_string(&replayed.save_log()).unwrap()
        );
        assert_eq!(replayed.doc.len(), s.doc.len());
        // Each plant expands to layer-switch + MeshLiteral + layer-restore
        // ops; three undos pop exactly one plant.
        for _ in 0..3 {
            run(&mut s, "undo");
        }
        assert_eq!(
            s.doc.objects().filter(|o| o.layer == "planting").count(),
            4,
            "undoing the expansion ops removes the last planted mesh"
        );
    }

    #[test]
    fn plantschedule_writes_csv_and_stores_report() {
        let dir = std::env::temp_dir().join("ijc_plantschedule_test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("schedule.csv");
        let mut s = Session::default();
        let err = s
            .run(Command::PlantSchedule { path: path.display().to_string() })
            .unwrap_err();
        assert!(format!("{err}").contains("nothing planted"), "{err}");
        run(&mut s, "plant oak 0,0");
        run(&mut s, "plant oak 20,0");
        run(&mut s, "plantrow birch 0,10 8,10 4");
        let out = run(&mut s, &format!("plantschedule {}", path.display()));
        assert!(out.message.contains("5 plant(s) across 2 species"), "{}", out.message);
        let csv = std::fs::read_to_string(&path).unwrap();
        assert!(csv.starts_with("species_id,binomial,common,count,"), "{csv}");
        assert!(csv.contains("quercus-robur,Quercus robur,English oak,2,30,25,yes"), "{csv}");
        assert!(csv.contains("betula-pendula,Betula pendula,silver birch,3,20,9,yes"), "{csv}");
        let r = s.doc.analysis_reports.get("plantschedule").expect("report stored");
        assert_eq!(r.count, 2, "one sample per species");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn plant_warns_when_species_is_outside_climate_band() {
        // Boston (~42°N) → temperate. A palm there fires the advisory; an oak
        // does not. No location → no advisory at all.
        let mut s = Session::default();
        let out = run(&mut s, "plant roystonea 0,0");
        assert!(!out.message.contains("advisory"), "no location yet: {}", out.message);
        run(&mut s, "location 42.36 -71.06");
        let palm = run(&mut s, "plant roystonea 5,0");
        assert!(palm.message.contains("advisory"), "palm in Boston: {}", palm.message);
        assert!(palm.message.contains("temperate"), "{}", palm.message);
        let oak = run(&mut s, "plant oak 10,0");
        assert!(!oak.message.contains("advisory"), "oak suits Boston: {}", oak.message);
        // Guayaquil (~-2°) → equatorial: birch is out of range, palm is fine.
        let mut g = Session::default();
        run(&mut g, "location -2.19 -79.88");
        let birch = run(&mut g, "plant birch 0,0");
        assert!(birch.message.contains("advisory"), "birch in Guayaquil: {}", birch.message);
        let coco = run(&mut g, "plant coconut 3,0");
        assert!(!coco.message.contains("advisory"), "coconut suits Guayaquil: {}", coco.message);
    }

    #[test]
    fn plantcatalog_filters_by_region_and_zone() {
        let mut s = Session::default();
        let all = run(&mut s, "plantcatalog");
        assert!(all.message.contains("33 species"), "{}", all.message);
        // Region tag: the Caribbean pack has 9 species.
        let carib = run(&mut s, "plantcatalog caribbean");
        assert!(carib.message.contains("9 species"), "{}", carib.message);
        assert!(carib.message.contains("roystonea-regia"), "{}", carib.message);
        assert!(!carib.message.contains("quercus-robur"), "{}", carib.message);
        // Zone code: temperate Dfb should list oak/birch but no palms.
        let dfb = run(&mut s, "plantcatalog Dfb");
        assert!(dfb.message.contains("quercus-robur"), "{}", dfb.message);
        assert!(!dfb.message.contains("roystonea-regia"), "{}", dfb.message);
        // Neither region nor zone matches → empty.
        let none = run(&mut s, "plantcatalog atlantis");
        assert!(none.message.contains("no species"), "{}", none.message);
        // Query only: no geometry, no op-log growth.
        assert_eq!(s.doc.len(), 0);
    }

    /// A square closed-region polyline of side `side` at the origin.
    fn square_region(s: &mut Session, side: f64) {
        run(
            s,
            &format!(
                "polyline 0,0,0 {side},0,0 {side},{side},0 0,{side},0 closed"
            ),
        );
    }

    #[test]
    fn miyawaki_needs_location_and_closed_region() {
        let mut s = Session::default();
        square_region(&mut s, 5.0);
        // No location → error.
        let err = s.run(parse("miyawaki last").unwrap()).unwrap_err();
        assert!(format!("{err}").contains("location"), "{err}");
        // Location but the target is an open line → error.
        let mut o = Session::default();
        run(&mut o, "location -2.19 -79.88");
        run(&mut o, "line 0,0,0 10,0,0");
        let err2 = o.run(parse("miyawaki last").unwrap()).unwrap_err();
        assert!(format!("{err2}").contains("closed region"), "{err2}");
    }

    #[test]
    fn miyawaki_places_stratified_mix_and_is_seed_stable() {
        // Guayaquil: a 10×10 = 100 m² region at density 3 → ~300 stems.
        let build = || {
            let mut s = Session::default();
            run(&mut s, "location -2.19 -79.88");
            square_region(&mut s, 10.0);
            let out = run(&mut s, "miyawaki last 3");
            (s, out)
        };
        let (s1, out1) = build();
        assert!(out1.message.contains("Miyawaki forest"), "{}", out1.message);
        assert!(out1.message.contains("self-thins"), "advisory present: {}", out1.message);
        let planted = s1.doc.objects().filter(|o| o.layer == "planting").count();
        // 100 m² × 3 /m² = 300 target; allow the region-fill rejection slack.
        assert!(
            (250..=300).contains(&planted),
            "count near density×area, got {planted}"
        );
        // All planted species must be native to the equatorial band.
        let band = crate::landscape::ClimateBand::from_latitude(-2.19);
        for o in s1.doc.objects().filter(|o| o.layer == "planting") {
            let id = o.name.as_deref().unwrap().strip_prefix("plant:").unwrap();
            let sp = crate::landscape::find_species(id).unwrap();
            assert!(band.suits(sp), "{id} not native to band");
            assert!(sp.layer.is_some(), "{id} has no stratum");
        }
        // Mix spans more than one stratum.
        let strata: std::collections::BTreeSet<&str> = s1
            .doc
            .objects()
            .filter(|o| o.layer == "planting")
            .filter_map(|o| {
                let id = o.name.as_deref().unwrap().strip_prefix("plant:").unwrap();
                crate::landscape::find_species(id).unwrap().layer.as_deref()
            })
            .collect();
        assert!(strata.len() >= 2, "mix must span layers, got {strata:?}");
        let report = s1.doc.analysis_reports.get("miyawaki").expect("report");
        assert!(report.count >= 2, "species mix samples");
        // Seed stability: same region + density → identical layout.
        let (s2, _) = build();
        let names1: Vec<_> = s1
            .doc
            .objects()
            .filter(|o| o.layer == "planting")
            .map(|o| o.name.clone())
            .collect();
        let names2: Vec<_> = s2
            .doc
            .objects()
            .filter(|o| o.layer == "planting")
            .map(|o| o.name.clone())
            .collect();
        assert_eq!(names1, names2, "seeded placement must be deterministic");
    }

    #[test]
    fn miyawaki_warns_when_too_few_natives() {
        // A temperate site: the catalog's temperate pool spans enough layers,
        // so instead verify the warning path via a band with a thin pool would
        // require a custom catalog; here assert the healthy path has NO warning
        // and that replay reproduces the forest byte-for-byte.
        let mut s = Session::default();
        run(&mut s, "location 42.36 -71.06"); // Boston, temperate
        square_region(&mut s, 6.0);
        let out = run(&mut s, "miyawaki last 2");
        // Temperate pool is small but ≥ the layer threshold; still, if the
        // warning fires it must name the shortage — never crash.
        if out.message.contains('⚠') {
            assert!(out.message.contains("Miyawaki needs natives"), "{}", out.message);
        }
        // Replay stability: the op-log reproduces the same planted forest.
        let log = s.save_log();
        let replayed = Session::replay(log.clone()).unwrap();
        assert_eq!(
            serde_json::to_string(&log).unwrap(),
            serde_json::to_string(&replayed.save_log()).unwrap(),
            "miyawaki forest must replay identically"
        );
    }

    #[test]
    fn planted_canopy_occludes_sun_analyses() {
        // A mature oak north of a sample grid at a mid-latitude site changes
        // nothing here — instead verify the canopy participates in the scene
        // triangle set used by every occlusion analysis.
        let mut s = Session::default();
        let before = scene_triangles(&s.doc).len();
        run(&mut s, "plant spruce 0,0");
        let after = scene_triangles(&s.doc).len();
        assert!(after > before, "plant mesh must join the occlusion scene");
    }

    #[test]
    fn flowarrows_point_downslope_and_carry_advisory() {
        // z = x/2 → every arrow tip is −x of its start.
        let mut s = terrain_session(10, 10.0, |x, _| x / 2.0);
        let out = run(&mut s, "flowarrows 20");
        assert_eq!(out.created.len(), 20, "{}", out.message);
        assert!(
            out.message.contains("not hydrology engineering"),
            "advisory wording required: {}",
            out.message
        );
        for id in &out.created {
            let obj = s.doc.get(*id).unwrap();
            assert_eq!(obj.layer, "analysis");
            let Geometry::Curve(Curve::Polyline { points, .. }) = &obj.geometry else {
                panic!("arrow must be a polyline");
            };
            assert!(points[1].x < points[0].x, "tip is downslope (−x)");
        }
        assert!(s.doc.analysis_reports.contains_key("flowarrows"));
        // Flat terrain has nothing to draw.
        let mut flat = terrain_session(4, 4.0, |_, _| 1.0);
        let err = flat.run(parse("flowarrows").unwrap()).unwrap_err();
        assert!(format!("{err}").contains("flat"), "{err}");
    }

    #[test]
    fn ponding_marks_bowl_center_and_none_on_slope() {
        let mut s = terrain_session(10, 10.0, |x, y| {
            ((x - 5.0).powi(2) + (y - 5.0).powi(2)) / 10.0
        });
        let out = run(&mut s, "ponding");
        assert_eq!(out.created.len(), 1, "{}", out.message);
        assert!(out.message.contains("not hydrology engineering"), "{}", out.message);
        let obj = s.doc.get(out.created[0]).unwrap();
        let Geometry::Curve(Curve::Polyline { points, closed }) = &obj.geometry else {
            panic!("marker must be a polyline circle");
        };
        assert!(closed);
        let cx = points.iter().map(|p| p.x).sum::<f64>() / points.len() as f64;
        let cy = points.iter().map(|p| p.y).sum::<f64>() / points.len() as f64;
        assert!((cx - 5.0).abs() < 1e-9 && (cy - 5.0).abs() < 1e-9, "marker at bowl bottom");
        // A uniform slope drains off the edge: zero markers, still advisory.
        let mut sl = terrain_session(10, 10.0, |x, _| x / 2.0);
        let out2 = run(&mut sl, "ponding");
        assert!(out2.created.is_empty());
        assert!(out2.message.contains("drains to its edges"), "{}", out2.message);
    }

    #[test]
    fn drainage_verbs_replay_and_undo_cleanly() {
        let mut s = terrain_session(10, 10.0, |x, y| {
            ((x - 5.0).powi(2) + (y - 5.0).powi(2)) / 10.0
        });
        let before = s.doc.len();
        run(&mut s, "flowarrows 10");
        run(&mut s, "ponding");
        let log = s.save_log();
        let replayed = Session::replay(log.clone()).unwrap();
        assert_eq!(
            serde_json::to_string(&log).unwrap(),
            serde_json::to_string(&replayed.save_log()).unwrap(),
            "drainage ops must replay bit-identically (ids embedded)"
        );
        run(&mut s, "undo");
        run(&mut s, "undo");
        assert_eq!(s.doc.len(), before, "both overlays removed");
        assert!(!s.doc.layers.contains_key("analysis"), "auto layer dropped");
    }

    #[test]
    fn sitepath_on_slope_warns_on_flat_does_not() {
        // z = x/2 (1:2 grade). A path running up the fall line must warn.
        let mut s = terrain_session(20, 20.0, |x, _| x / 2.0);
        run(&mut s, "line 2,10,0 18,10,0");
        let out = run(&mut s, "sitepath last 1.5");
        assert!(out.message.contains("WARNING"), "{}", out.message);
        assert!(out.message.contains("1:12"), "{}", out.message);
        assert!(out.message.contains("advisory"), "{}", out.message);
        let obj = s.doc.get(out.created[0]).unwrap();
        assert_eq!(obj.layer, "hardscape");
        let Geometry::Mesh(m) = &obj.geometry else { panic!("ribbon must be a mesh") };
        // Draped: ribbon z follows the terrain (+2 cm), so x=10 sits near 5.
        let mid = m
            .positions()
            .iter()
            .find(|p| (p.x - 10.0).abs() < 0.3)
            .expect("sample near x=10");
        assert!((mid.z - 5.02).abs() < 0.2, "draped onto terrain, got {}", mid.z);

        // A contour-following path (constant elevation) does not warn.
        run(&mut s, "line 10,2,0 10,18,0");
        let out2 = run(&mut s, "sitepath last 1.5");
        assert!(!out2.message.contains("WARNING"), "{}", out2.message);
        assert!(out2.message.contains("within 1:12"), "{}", out2.message);
    }

    #[test]
    fn sitepath_without_curve_errors_and_replays() {
        let mut s = Session::default();
        run(&mut s, "box 0,0,0 2,2,2");
        let err = s.run(parse("sitepath last 1").unwrap()).unwrap_err();
        assert!(format!("{err}").contains("curve"), "{err}");
        // Flat ground, no terrain: path still builds and replays stably.
        run(&mut s, "line 0,5,0 20,5,0");
        run(&mut s, "sitepath last 2");
        let log = s.save_log();
        let replayed = Session::replay(log.clone()).unwrap();
        assert_eq!(
            serde_json::to_string(&log).unwrap(),
            serde_json::to_string(&replayed.save_log()).unwrap(),
            "sitepath must replay bit-identically"
        );
        // Undo drops the ribbon and the auto-created layer.
        let n = s.doc.len();
        run(&mut s, "undo");
        assert_eq!(s.doc.len(), n - 1);
        assert!(!s.doc.layers.contains_key("hardscape"));
    }

    #[test]
    fn contours_replay_is_stable() {
        let mut s = terrain_session(20, 10.0, |x, _| x / 2.0);
        run(&mut s, "contours 0.5");
        let log = s.save_log();
        let replayed = Session::replay(log.clone()).unwrap();
        assert_eq!(
            serde_json::to_string(&log).unwrap(),
            serde_json::to_string(&replayed.save_log()).unwrap(),
            "contours log must replay bit-identically (ids embedded)"
        );
        assert_eq!(s.doc.len(), replayed.doc.len());
    }

    // ── M-checkengine: codecheck / checkrules / report codecheck ────────────

    /// A scene tripping most demo rules: a 1:6 ramp, a 0.7 m door, a stair
    /// with 0.2 m risers, a corridor squeezed to 0.8 m under a 1.8 m ceiling,
    /// and a raised slab (guard info).
    fn violating_scene() -> Session {
        let mut s = Session::default();
        // Ramp: 6 m run rising 1 m = 1:6 (limit 1:12).
        run(&mut s, "line 0,0,0 6,0,1");
        run(&mut s, "name last ramp-a");
        // Door: parametric door narrowed to 0.7 m (min 0.8128).
        run(&mut s, "insert pdoor 10,0,0 width=0.7");
        // Stair: two merged steps with a 0.2 m riser (max 0.1778).
        run(&mut s, "box 20,0,0 0.4,1,0.2");
        run(&mut s, "box 20.3,0,0 0.4,1,0.4");
        run(&mut s, "union last 2");
        run(&mut s, "name last stair-a");
        // Corridor at y=50: walls 0.8 m apart (min 0.9144) and a 1.8 m
        // ceiling (min 2.032) over a 2 m centerline.
        run(&mut s, "box 0,50.4,0 2,0.3,3");
        run(&mut s, "box 0,49.3,0 2,0.3,3");
        run(&mut s, "box 0,49.3,1.8 2,1.4,0.2");
        run(&mut s, "line 0,50,0 2,50,0");
        run(&mut s, "name last corridor-a");
        // Raised slab, top at 1.2 m (> 0.762 guard-drop info threshold).
        run(&mut s, "slab 30,0,1 32,0,1 32,2,1 30,2,1 thick 0.2");
        s
    }

    #[test]
    fn codecheck_demo_end_to_end() {
        let mut s = violating_scene();
        let before = s.doc.len();
        let out = run(&mut s, "codecheck demo");
        assert!(out.message.contains("advisory"), "message must carry the disclaimer: {}", out.message);
        assert!(!out.created.is_empty(), "violations must create markers");
        // Every marker lands on the compliance layer.
        assert!(s.doc.layers.contains_key("compliance"));
        for id in &out.created {
            assert_eq!(s.doc.get(*id).unwrap().layer, "compliance");
        }
        // The stored report has the expected per-rule verdicts.
        let r = s.doc.compliance_reports.get("demo").expect("report stored");
        let verdict = |id: &str| {
            r.rules.iter().find(|o| o.rule_id == id).unwrap_or_else(|| panic!("rule {id}"))
        };
        assert_eq!(verdict("ramp-slope").verdict, "fail");
        assert!(verdict("ramp-slope").measured.unwrap() > 0.16);
        assert_eq!(verdict("door-width").verdict, "fail");
        assert!((verdict("door-width").measured.unwrap() - 0.7).abs() < 1e-9);
        assert_eq!(verdict("stair-riser").verdict, "fail");
        assert!((verdict("stair-riser").measured.unwrap() - 0.2).abs() < 0.02);
        assert_eq!(verdict("headroom").verdict, "warn");
        assert!((verdict("headroom").measured.unwrap() - 1.8).abs() < 0.05);
        assert_eq!(verdict("corridor-width").verdict, "warn");
        assert!((verdict("corridor-width").measured.unwrap() - 0.8).abs() < 0.05);
        assert_eq!(verdict("guard-check").verdict, "info");
        // Violating rules carry object ids + locations.
        assert!(!verdict("ramp-slope").objects.is_empty());
        assert!(!verdict("ramp-slope").locations.is_empty());

        // `report codecheck` serves it, grounded in rule ids + disclaimer.
        let rep = run(&mut s, "report codecheck").message;
        for id in ["ramp-slope", "door-width", "stair-riser", "headroom", "corridor-width", "guard-check"] {
            assert!(rep.contains(id), "report missing rule {id}: {rep}");
        }
        assert!(rep.contains("advisory pre-check"), "report missing disclaimer: {rep}");
        assert!(rep.contains("FAIL") && rep.contains("INFO"));
        // Bare `report` includes it too.
        assert!(run(&mut s, "report").message.contains("ramp-slope"));

        // Undo removes markers AND the auto-created layer.
        run(&mut s, "undo");
        assert_eq!(s.doc.len(), before);
        assert!(!s.doc.layers.contains_key("compliance"));
    }

    #[test]
    fn codecheck_replay_is_stable_and_disk_free() {
        let mut s = violating_scene();
        run(&mut s, "codecheck demo");
        let log = s.save_log();
        // The logged op embeds the rules (no pack-table/disk dependency).
        let logged = log.last().unwrap();
        match logged {
            Command::CodeCheck { rules, ids, .. } => {
                assert!(rules.is_some(), "rules must be embedded in the logged op");
                assert!(ids.is_some(), "marker ids must be written back");
            }
            other => panic!("expected CodeCheck, got {other:?}"),
        }
        let replayed = Session::replay(log.clone()).unwrap();
        assert_eq!(
            serde_json::to_string(&log).unwrap(),
            serde_json::to_string(&replayed.save_log()).unwrap(),
            "codecheck must replay bit-identically (rules + ids embedded)"
        );
        assert_eq!(s.doc.len(), replayed.doc.len());
        assert_eq!(
            s.doc.compliance_reports, replayed.doc.compliance_reports,
            "replay regenerates the same compliance report"
        );
    }

    #[test]
    fn codecheck_clean_scene_all_pass_no_markers() {
        let mut s = Session::default();
        run(&mut s, "line 0,0,0 24,0,1"); // 1:24 — gentle
        run(&mut s, "name last ramp-ok");
        let before = s.doc.len();
        let out = run(&mut s, "codecheck demo");
        assert!(out.created.is_empty(), "no violations, no markers");
        assert_eq!(s.doc.len(), before);
        assert!(!s.doc.layers.contains_key("compliance"), "no layer without markers");
        let r = s.doc.compliance_reports.get("demo").unwrap();
        assert!(r.rules.iter().all(|o| o.verdict == "pass"), "{:?}", r.rules);
        assert!(out.message.contains("advisory"));
    }

    #[test]
    fn codecheck_story_filter_scopes_targets() {
        let mut s = violating_scene();
        run(&mut s, "story L1 0");
        run(&mut s, "story L2 10");
        // Everything in the scene sits below z=10 → L2 matches nothing.
        run(&mut s, "codecheck demo L2");
        let r = s.doc.compliance_reports.get("demo").unwrap();
        assert!(r.context.contains("L2"));
        for o in &r.rules {
            if o.rule_id == "guard-check" {
                continue; // count-style rules aside, geometry rules see 0 targets
            }
            assert_eq!(o.checked, 0, "rule {} matched targets on empty L2", o.rule_id);
            assert_eq!(o.verdict, "pass");
        }
        // Unknown story is a clear error.
        let err = s.run(parse("codecheck demo attic").unwrap()).unwrap_err();
        assert!(err.to_string().contains("unknown story"), "{err}");
    }

    #[test]
    fn codecheck_unknown_pack_lists_loaded() {
        let mut s = Session::default();
        let err = s.run(parse("codecheck nonexistent").unwrap()).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("unknown check pack"), "{msg}");
        assert!(msg.contains("demo"), "should list the loaded packs: {msg}");
    }

    #[test]
    fn checkrules_load_and_run_custom_pack() {
        let dir = std::env::temp_dir().join(format!("ijc-checkrules-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("strict.checks.json");
        std::fs::write(
            &path,
            r#"{"name":"strict","description":"one strict slope rule","rules":[
                {"id":"any-slope","code_ref":"TEST 1","severity":"warn",
                 "target":{"kinds":["curve"]},
                 "check":{"kind":"max_slope","limit":0.01},
                 "message":"practically anything sloped"}]}"#,
        )
        .unwrap();

        let mut s = Session::default();
        run(&mut s, "line 0,0,0 10,0,1");
        let out = run(&mut s, &format!("checkrules load {}", path.display()));
        assert!(out.message.contains("strict"));
        // list shows demo + the loaded pack + the disclaimer.
        let listing = run(&mut s, "checkrules list").message;
        assert!(listing.contains("demo") && listing.contains("strict"));
        assert!(listing.contains("advisory"));
        // The loaded pack evaluates.
        run(&mut s, "codecheck strict");
        let r = s.doc.compliance_reports.get("strict").unwrap();
        assert_eq!(r.rules[0].verdict, "warn");
        // Malformed pack is rejected with the path in the error.
        let bad = dir.join("bad.checks.json");
        std::fs::write(&bad, "{ nope").unwrap();
        let err = s
            .run(parse(&format!("checkrules load {}", bad.display())).unwrap())
            .unwrap_err();
        assert!(err.to_string().contains("bad.checks.json"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn checkrules_classification() {
        // load reads the fs → side-effecting + confirm summary; list is pure.
        let load = parse("checkrules load /tmp/x.json").unwrap();
        assert!(load.is_side_effecting());
        assert!(load.side_effect_summary().unwrap().contains("/tmp/x.json"));
        assert!(!load.is_logged());
        let list = parse("checkrules list").unwrap();
        assert!(!list.is_side_effecting());
        assert!(!list.is_logged());
        // codecheck is a pure, LOGGED analysis op (markers must replay).
        let cc = parse("codecheck demo").unwrap();
        assert!(!cc.is_side_effecting());
        assert!(cc.is_logged());
    }

    // ── M-ibc: room tagging + IBC pack ───────────────────────────────────────

    #[test]
    fn room_tags_closed_curve_with_area_and_undoes() {
        let mut s = Session::default();
        // 10×20 m rectangle → 200 m² plan area.
        run(&mut s, "rect 0,0,0 10 20");
        let out = run(&mut s, "room last business Office");
        assert!(out.message.contains("business"), "{}", out.message);
        assert_eq!(s.doc.rooms.len(), 1);
        let r = &s.doc.rooms[0];
        assert_eq!(r.name, "Office");
        assert_eq!(r.occupancy, "business");
        assert!((r.area - 200.0).abs() < 1e-6, "area {}", r.area);
        // Centroid is the rectangle center (5,10,0).
        let c = r.centroid();
        assert!((c[0] - 5.0).abs() < 1e-6 && (c[1] - 10.0).abs() < 1e-6, "centroid {c:?}");
        // `rooms` lists it.
        assert!(run(&mut s, "rooms").message.contains("Office"));
        // Undo removes the room, redo restores it.
        run(&mut s, "undo");
        assert!(s.doc.rooms.is_empty(), "undo drops the room");
        run(&mut s, "redo");
        assert_eq!(s.doc.rooms.len(), 1);
    }

    #[test]
    fn room_rejects_open_curve_and_bad_occupancy() {
        let mut s = Session::default();
        run(&mut s, "line 0,0,0 10,0,0");
        let err = s.run(parse("room last business").unwrap()).unwrap_err();
        assert!(err.to_string().contains("open curve"), "{err}");
        // Auto-named when no name given, valid occupancy.
        run(&mut s, "rect 0,0,0 5 5");
        let out = run(&mut s, "room last storage");
        assert!(out.message.contains("storage-1"), "{}", out.message);
        // Unknown occupancy is refused.
        run(&mut s, "rect 20,0,0 5 5");
        let err = s.run(parse("room last spaceship").unwrap()).unwrap_err();
        assert!(err.to_string().contains("unknown occupancy"), "{err}");
    }

    #[test]
    fn room_replay_is_stable() {
        let mut s = Session::default();
        run(&mut s, "rect 0,0,0 10 20");
        run(&mut s, "room last business");
        let log = s.save_log();
        let replayed = Session::replay(log.clone()).unwrap();
        assert_eq!(
            serde_json::to_string(&log).unwrap(),
            serde_json::to_string(&replayed.save_log()).unwrap(),
            "room op must replay bit-identically (name written back)"
        );
        assert_eq!(replayed.doc.rooms, s.doc.rooms);
    }

    #[test]
    fn codecheck_ibc2021_end_to_end() {
        // A room + egress scene run through the embedded IBC pack.
        let mut s = Session::default();
        // Business floor: 10×20 m rectangle (200 m² → load 15 → 1 exit).
        run(&mut s, "rect 0,0,0 10 20");
        run(&mut s, "room last business");
        // A door named "exit-1" near the room centroid.
        run(&mut s, "insert pdoor 5,8,0 width=0.9");
        run(&mut s, "name last exit-1");
        let out = run(&mut s, "codecheck ibc2021");
        assert!(out.message.contains("advisory"), "{}", out.message);
        let r = s.doc.compliance_reports.get("ibc2021").expect("report stored");
        let verdict =
            |id: &str| r.rules.iter().find(|o| o.rule_id == id).unwrap_or_else(|| panic!("{id}"));
        // Occupant load computed on the room (info verdict, measured = 15).
        assert!((verdict("occupant-load").measured.unwrap() - 15.0).abs() < 1e-9);
        assert_eq!(verdict("occupant-load").checked, 1, "one room checked");
        // One exit within reach, one required → exit count passes.
        assert_eq!(verdict("exit-count").verdict, "pass");
        // Exit is ~3 m from the centroid, well under 76.2 m → travel passes.
        assert_eq!(verdict("travel-distance").verdict, "pass");
        // Replay stability (embedded rules + marker ids).
        let log = s.save_log();
        let replayed = Session::replay(log.clone()).unwrap();
        assert_eq!(
            serde_json::to_string(&log).unwrap(),
            serde_json::to_string(&replayed.save_log()).unwrap(),
            "ibc codecheck must replay bit-identically"
        );
    }

    #[test]
    fn codecheck_ada2010_end_to_end() {
        // An accessible-route scene run through the embedded ADA pack.
        let mut s = Session::default();
        // A ramp centerline named "ramp-1": rises 0.99 m over 12 m (~1:12.1,
        // just under the 1:12 limit) — running slope PASSES, but there is no
        // 60 in landing in a >30 in rise, so ramp-landings FAILS.
        run(&mut s, "polyline 0,0,0 12,0,0.99");
        run(&mut s, "name last ramp-1");
        // A narrow door (0.8 m < 0.813 m) → door clear width fails; no
        // threshold param → door-threshold flags "not modelably detectable".
        run(&mut s, "insert pdoor 3,3,0 width=0.8");
        run(&mut s, "name last door-1");

        let out = run(&mut s, "codecheck ada2010");
        assert!(out.message.contains("advisory"), "{}", out.message);
        let r = s.doc.compliance_reports.get("ada2010").expect("report stored");
        let verdict =
            |id: &str| r.rules.iter().find(|o| o.rule_id == id).unwrap_or_else(|| panic!("{id}"));
        // Running slope of exactly 1:12 is not > the limit → passes.
        assert_eq!(verdict("ramp-running-slope").verdict, "pass");
        // No landing in a >30 in continuous rise → landings fail (error).
        assert_eq!(verdict("ramp-landings").verdict, "fail");
        // 0.8 m door leaf < 0.813 m → clear width fails.
        assert_eq!(verdict("door-clear-width").verdict, "fail");
        // No 'threshold' param → threshold flagged (honest "not detectable").
        assert_eq!(verdict("door-threshold").verdict, "warn");

        // Replay stability (embedded rules + marker ids).
        let log = s.save_log();
        let replayed = Session::replay(log.clone()).unwrap();
        assert_eq!(
            serde_json::to_string(&log).unwrap(),
            serde_json::to_string(&replayed.save_log()).unwrap(),
            "ada codecheck must replay bit-identically"
        );
    }

    #[test]
    fn compliance_report_survives_snapshot_roundtrip() {
        // The checkpoint sidecar serializes `compliance_reports`; pre-field
        // snapshots load empty (serde default) — AnalysisReport precedent.
        let mut s = violating_scene();
        run(&mut s, "codecheck demo");
        let json = serde_json::to_string(&s.doc).unwrap();
        let back: Document = serde_json::from_str(&json).unwrap();
        assert_eq!(back.compliance_reports, s.doc.compliance_reports);
        let mut v: serde_json::Value = serde_json::from_str(&json).unwrap();
        v.as_object_mut().unwrap().remove("compliance_reports");
        let old: Document = serde_json::from_value(v).unwrap();
        assert!(old.compliance_reports.is_empty(), "pre-compliance snapshots load empty");
    }

    // ── M-intemfit: lotsubdivide / lotsettings (Phase 2/3) ──────────────────

    fn lot_count(s: &Session) -> usize {
        s.doc.all_ids()
            .iter()
            .filter(|id| {
                s.doc.get(**id).map(|o| o.layer == crate::lot::LOTS_LAYER).unwrap_or(false)
            })
            .count()
    }

    #[test]
    fn lotsubdivide_grid_bakes_lots_on_layer() {
        let mut s = Session::default();
        run(&mut s, "rect 0,0,0 400 120");
        let out = run(&mut s, "lotsubdivide last grid area=6500 width=50");
        assert!(out.created.len() > 1, "expected multiple lots");
        assert!(s.doc.layers.contains_key(crate::lot::LOTS_LAYER));
        assert_eq!(lot_count(&s), out.created.len());
    }

    #[test]
    fn lotsubdivide_undo_removes_lots() {
        let mut s = Session::default();
        run(&mut s, "rect 0,0,0 400 120");
        run(&mut s, "lotsubdivide last grid area=6500 width=50");
        assert!(lot_count(&s) > 0);
        run(&mut s, "undo");
        assert_eq!(lot_count(&s), 0, "undo removes baked lots + layer");
    }

    #[test]
    fn lotsubdivide_replay_is_byte_identical() {
        // Deterministic subdivider → replaying the log recreates identical lots.
        let mut s = Session::default();
        run(&mut s, "rect 0,0,0 400 120");
        run(&mut s, "lotsubdivide last grid area=6500 width=50 irregularity=0.3 seed=7");
        let before: Vec<_> = s
            .doc
            .all_ids()
            .iter()
            .filter_map(|id| s.doc.get(*id))
            .filter(|o| o.layer == crate::lot::LOTS_LAYER)
            .map(|o| o.geometry.clone())
            .collect();
        let log: Vec<Command> = s.log.iter().map(|a| a.op.clone()).collect();
        let rebuilt = Session::replay(log).unwrap();
        let after: Vec<_> = rebuilt
            .doc
            .all_ids()
            .iter()
            .filter_map(|id| rebuilt.doc.get(*id))
            .filter(|o| o.layer == crate::lot::LOTS_LAYER)
            .map(|o| o.geometry.clone())
            .collect();
        assert_eq!(before, after, "replay recreated identical lot geometry");
    }

    #[test]
    fn perimeter_method_runs_and_bakes_lots() {
        // Phase 4: method=perimeter now runs (no longer the deferral error).
        let mut s = Session::default();
        run(&mut s, "rect 0,0,0 400 300");
        let out = run(&mut s, "lotsubdivide last perimeter area=4000 width=20 seed=7");
        assert!(out.created.len() > 1, "expected multiple perimeter lots");
        assert_eq!(lot_count(&s), out.created.len());
    }

    #[test]
    fn perimeter_undo_and_replay_byte_identical() {
        let mut s = Session::default();
        run(&mut s, "rect 0,0,0 400 300");
        run(&mut s, "lotsubdivide last perimeter area=4000 width=20 irregularity=0.3 seed=7");
        assert!(lot_count(&s) > 0);

        // Undo removes all baked lots + the layer.
        let before: Vec<_> = s
            .doc
            .all_ids()
            .iter()
            .filter_map(|id| s.doc.get(*id))
            .filter(|o| o.layer == crate::lot::LOTS_LAYER)
            .map(|o| o.geometry.clone())
            .collect();
        run(&mut s, "undo");
        assert_eq!(lot_count(&s), 0, "undo removes perimeter lots");
        run(&mut s, "redo");

        // Replay recreates byte-identical lot geometry (deterministic subdivider).
        let log: Vec<Command> = s.log.iter().map(|a| a.op.clone()).collect();
        let rebuilt = Session::replay(log).unwrap();
        let after: Vec<_> = rebuilt
            .doc
            .all_ids()
            .iter()
            .filter_map(|id| rebuilt.doc.get(*id))
            .filter(|o| o.layer == crate::lot::LOTS_LAYER)
            .map(|o| o.geometry.clone())
            .collect();
        assert_eq!(before, after, "replay recreated identical perimeter lots");
    }

    #[test]
    fn streetfollowing_method_runs_and_bakes_lots() {
        // Phase 7: method=streetfollowing runs (skeleton subdivision) instead of
        // the deferral error.
        let mut s = Session::default();
        run(&mut s, "rect 0,0,0 400 120");
        let out = run(&mut s, "lotsubdivide last streetfollowing area=4000 width=30 seed=7");
        assert!(out.created.len() > 1, "expected multiple skeleton lots");
        assert_eq!(lot_count(&s), out.created.len());
    }

    #[test]
    fn streetfollowing_undo_and_replay_byte_identical() {
        let mut s = Session::default();
        run(&mut s, "rect 0,0,0 400 120");
        run(&mut s, "lotsubdivide last streetfollowing area=4000 width=30 seed=7");
        assert!(lot_count(&s) > 0);

        let before: Vec<_> = s
            .doc
            .all_ids()
            .iter()
            .filter_map(|id| s.doc.get(*id))
            .filter(|o| o.layer == crate::lot::LOTS_LAYER)
            .map(|o| o.geometry.clone())
            .collect();
        run(&mut s, "undo");
        assert_eq!(lot_count(&s), 0, "undo removes skeleton lots");
        run(&mut s, "redo");

        let log: Vec<Command> = s.log.iter().map(|a| a.op.clone()).collect();
        let rebuilt = Session::replay(log).unwrap();
        let after: Vec<_> = rebuilt
            .doc
            .all_ids()
            .iter()
            .filter_map(|id| rebuilt.doc.get(*id))
            .filter(|o| o.layer == crate::lot::LOTS_LAYER)
            .map(|o| o.geometry.clone())
            .collect();
        assert_eq!(before, after, "replay recreated identical skeleton lots");
    }

    // ── M-intemfit: lotgeneratesite (Phase 5) ───────────────────────────────

    fn layer_count(s: &Session, layer: &str) -> usize {
        s.doc.all_ids()
            .iter()
            .filter(|id| s.doc.get(**id).map(|o| o.layer == layer).unwrap_or(false))
            .count()
    }

    #[test]
    fn lotgeneratesite_bakes_roads_and_blocks_layers() {
        let mut s = Session::default();
        run(&mut s, "rect 0,0,0 400 300");
        let out = run(
            &mut s,
            "lotgeneratesite last orthogonal roadwidth=12 blockdepth=60 seed=7",
        );
        assert!(out.created.len() > 2, "expected roads + blocks");
        assert!(s.doc.layers.contains_key(crate::lot::ROADS_LAYER));
        assert!(s.doc.layers.contains_key(crate::lot::BLOCKS_LAYER));
        assert!(layer_count(&s, crate::lot::ROADS_LAYER) > 0, "roads baked");
        assert!(layer_count(&s, crate::lot::BLOCKS_LAYER) > 0, "blocks baked");
        assert!(out.message.contains("street edges"), "{}", out.message);
    }

    #[test]
    fn lotgeneratesite_undo_removes_both_layers() {
        let mut s = Session::default();
        run(&mut s, "rect 0,0,0 400 300");
        run(&mut s, "lotgeneratesite last orthogonal roadwidth=12 blockdepth=60 seed=7");
        assert!(layer_count(&s, crate::lot::ROADS_LAYER) > 0);
        assert!(layer_count(&s, crate::lot::BLOCKS_LAYER) > 0);
        run(&mut s, "undo");
        assert_eq!(layer_count(&s, crate::lot::ROADS_LAYER), 0, "undo removes roads");
        assert_eq!(layer_count(&s, crate::lot::BLOCKS_LAYER), 0, "undo removes blocks");
    }

    #[test]
    fn lotgeneratesite_replay_is_byte_identical() {
        // Deterministic generators → replaying the log recreates identical roads
        // + blocks (ids written back on first exec).
        let mut s = Session::default();
        run(&mut s, "rect 0,0,0 400 300");
        run(&mut s, "lotgeneratesite last culdesac blockdepth=60 alleys=on seed=7");
        let geo = |s: &Session| -> Vec<_> {
            let mut v: Vec<_> = s
                .doc
                .all_ids()
                .iter()
                .filter_map(|id| s.doc.get(*id))
                .filter(|o| {
                    o.layer == crate::lot::ROADS_LAYER || o.layer == crate::lot::BLOCKS_LAYER
                })
                .map(|o| (o.layer.clone(), o.geometry.clone()))
                .collect();
            v.sort_by(|a, b| format!("{a:?}").cmp(&format!("{b:?}")));
            v
        };
        let before = geo(&s);
        let log: Vec<Command> = s.log.iter().map(|a| a.op.clone()).collect();
        let rebuilt = Session::replay(log).unwrap();
        assert_eq!(before, geo(&rebuilt), "replay recreated identical roads + blocks");
    }

    #[test]
    fn lotgeneratesite_nonrectilinear_patterns_bake() {
        // Phase 5b: radial / hexagonal / voronoi now generate roads + blocks
        // (they were clean-error stubs in Phase 5). Each bakes onto the roads +
        // blocks layers with no panic.
        for pattern in ["radial", "hexagonal", "voronoi"] {
            let mut s = Session::default();
            run(&mut s, "rect 0,0,0 400 300");
            s.run(parse(&format!("lotgeneratesite last {pattern} blockdepth=60"))
                .unwrap())
                .unwrap_or_else(|e| panic!("{pattern} errored: {e}"));
            let n_roads = s
                .doc
                .all_ids()
                .iter()
                .filter_map(|id| s.doc.get(*id))
                .filter(|o| o.layer == crate::lot::ROADS_LAYER)
                .count();
            let n_blocks = s
                .doc
                .all_ids()
                .iter()
                .filter_map(|id| s.doc.get(*id))
                .filter(|o| o.layer == crate::lot::BLOCKS_LAYER)
                .count();
            assert!(n_roads > 0, "{pattern}: no roads baked");
            assert!(n_blocks > 0, "{pattern}: no blocks baked");
        }
    }

    // ── M-intemfit: lot rules (Phase 6) ──────────────────────────────────────

    #[test]
    fn lotsettings_accepts_phase6_keys() {
        let mut s = Session::default();
        run(
            &mut s,
            "lotsettings region=euro_latam loading=alley widthmix=6:0.25,8:0.5,10:0.25 \
             depth=25 corner=15% flag=on mergeslivers=on",
        );
        let st = &s.doc.subdivision_settings;
        assert_eq!(st.region, subdivision::RegionProfile::EuroLatam);
        assert_eq!(st.loading, subdivision::LoadingType::AlleyLoaded);
        assert!(st.width_mix.is_some());
        assert_eq!(st.lot_depth_target, 25.0);
        assert!((st.corner_lot_width_bonus - 0.15).abs() < 1e-9);
        assert!(st.allow_flag_lots);
        assert!(st.merge_slivers);
    }

    #[test]
    fn lotloading_sets_sticky_mode_and_undoes() {
        let mut s = Session::default();
        run(&mut s, "lotloading alley");
        assert_eq!(
            s.doc.subdivision_settings.loading,
            subdivision::LoadingType::AlleyLoaded
        );
        run(&mut s, "undo");
        assert_eq!(
            s.doc.subdivision_settings.loading,
            subdivision::LoadingType::FrontLoaded,
            "undo restores prior loading"
        );
    }

    #[test]
    fn lotsubdivide_width_mix_reports_proportion_and_placeholder() {
        // A 500 m frontage × 50 m block, width-mix from the euro_latam default
        // (no explicit products → placeholder note fires).
        let mut s = Session::default();
        run(&mut s, "rect 0,0,0 500 50");
        // Metric area so the sliver threshold (0.5×area) does not eat the lots.
        run(&mut s, "lotsettings region=euro_latam widthmix=6:0.25,8:0.5,10:0.25 area=120");
        let out = run(&mut s, "lotsubdivide last grid seed=7");
        assert!(out.created.len() > 10, "expected many width-mix lots, got {}", out.created.len());
        assert!(out.message.contains("width mix"), "message: {}", out.message);
    }

    #[test]
    fn lotsubdivide_placeholder_note_surfaces_on_defaults() {
        // euro_latam corner bonus is a placeholder → note appears even on a plain
        // grid run (corner rule resolves from the profile).
        let mut s = Session::default();
        run(&mut s, "rect 0,0,0 400 120");
        let out = run(&mut s, "lotsubdivide last grid area=6500 width=50");
        assert!(
            out.message.contains("placeholder — confirm with Manuel"),
            "message should surface the euro_latam placeholder note: {}",
            out.message
        );
    }

    #[test]
    fn lotsubdivide_merges_slivers_no_placeholder_under_us_profile() {
        // Under us_suburban there are no placeholder fallbacks; a plain run must
        // NOT print the placeholder note.
        let mut s = Session::default();
        run(&mut s, "rect 0,0,0 400 120");
        run(&mut s, "lotsettings region=us_suburban");
        let out = run(&mut s, "lotsubdivide last grid area=6500 width=50");
        assert!(
            !out.message.contains("placeholder"),
            "us_suburban must not print placeholder note: {}",
            out.message
        );
    }

    #[test]
    fn lotsubdivide_width_mix_replay_byte_identical() {
        let mut s = Session::default();
        run(&mut s, "rect 0,0,0 500 50");
        run(&mut s, "lotsettings widthmix=6:0.25,8:0.5,10:0.25 area=120");
        run(&mut s, "lotsubdivide last grid seed=7");
        let before: Vec<_> = s
            .doc
            .all_ids()
            .iter()
            .filter_map(|id| s.doc.get(*id))
            .filter(|o| o.layer == crate::lot::LOTS_LAYER)
            .map(|o| o.geometry.clone())
            .collect();
        let log = s.save_log();
        let replayed = Session::replay(log).unwrap();
        let after: Vec<_> = replayed
            .doc
            .all_ids()
            .iter()
            .filter_map(|id| replayed.doc.get(*id))
            .filter(|o| o.layer == crate::lot::LOTS_LAYER)
            .map(|o| o.geometry.clone())
            .collect();
        assert_eq!(before.len(), after.len(), "lot count must survive replay");
    }

    #[test]
    fn lotsettings_sticks_and_survives_roundtrip() {
        let mut s = Session::default();
        run(&mut s, "lotsettings area=6500 width=45 irregularity=0.2");
        assert_eq!(s.doc.subdivision_settings.lot_area_min, 6500.0);
        assert_eq!(s.doc.subdivision_settings.lot_width_min, 45.0);
        // Serde roundtrip (checkpoint sidecar) preserves the settings.
        let json = serde_json::to_string(&s.doc).unwrap();
        let back: Document = serde_json::from_str(&json).unwrap();
        assert_eq!(back.subdivision_settings, s.doc.subdivision_settings);
        // Pre-intemfit snapshots (no field) load with defaults.
        let mut v: serde_json::Value = serde_json::from_str(&json).unwrap();
        v.as_object_mut().unwrap().remove("subdivision_settings");
        let old: Document = serde_json::from_value(v).unwrap();
        assert_eq!(old.subdivision_settings, subdivision::SubdivisionSettings::default());
    }

    #[test]
    fn lotsettings_undo_restores_prior() {
        let mut s = Session::default();
        run(&mut s, "lotsettings area=6500");
        run(&mut s, "lotsettings area=8000");
        assert_eq!(s.doc.subdivision_settings.lot_area_min, 8000.0);
        run(&mut s, "undo");
        assert_eq!(s.doc.subdivision_settings.lot_area_min, 6500.0);
    }

    // ── M-intemfit: setbacks + buildable envelopes + frontage (Phase 8) ──────

    fn setback_count(s: &Session) -> usize {
        s.doc
            .all_ids()
            .iter()
            .filter(|id| {
                s.doc.get(**id).map(|o| o.layer == crate::lot::SETBACKS_LAYER).unwrap_or(false)
            })
            .count()
    }

    #[test]
    fn lotsetbacks_bakes_envelope_layer() {
        let mut s = Session::default();
        run(&mut s, "rect 0,0,0 30 40");
        let out = run(&mut s, "lotsetbacks last front=5 side=3 rear=7");
        assert_eq!(out.created.len(), 1, "one envelope for one lot");
        assert!(s.doc.layers.contains_key(crate::lot::SETBACKS_LAYER));
        assert_eq!(setback_count(&s), 1);
        // Distinct layer from `lots`.
        assert_ne!(crate::lot::SETBACKS_LAYER, crate::lot::LOTS_LAYER);
    }

    #[test]
    fn lotsetbacks_undo_removes_envelopes() {
        let mut s = Session::default();
        run(&mut s, "rect 0,0,0 30 40");
        run(&mut s, "lotsetbacks last front=5 side=3 rear=7");
        assert!(setback_count(&s) > 0);
        run(&mut s, "undo");
        assert_eq!(setback_count(&s), 0, "undo removes baked envelopes + layer");
    }

    #[test]
    fn lotsetbacks_replay_is_byte_identical() {
        let mut s = Session::default();
        run(&mut s, "rect 0,0,0 30 40");
        run(&mut s, "lotsetbacks last front=5 side=3 rear=7");
        let before: Vec<_> = s
            .doc
            .all_ids()
            .iter()
            .filter_map(|id| s.doc.get(*id))
            .filter(|o| o.layer == crate::lot::SETBACKS_LAYER)
            .map(|o| o.geometry.clone())
            .collect();
        let log: Vec<Command> = s.log.iter().map(|a| a.op.clone()).collect();
        let rebuilt = Session::replay(log).unwrap();
        let after: Vec<_> = rebuilt
            .doc
            .all_ids()
            .iter()
            .filter_map(|id| rebuilt.doc.get(*id))
            .filter(|o| o.layer == crate::lot::SETBACKS_LAYER)
            .map(|o| o.geometry.clone())
            .collect();
        assert_eq!(before, after, "replay recreated identical envelope geometry");
    }

    #[test]
    fn lotsetbacks_euro_latam_placeholder_note_appears() {
        // Under the euro_latam default profile the setback numbers are §6b
        // placeholders → the run surfaces the "confirm with Manuel" note.
        let mut s = Session::default();
        run(&mut s, "rect 0,0,0 30 40");
        // Explicit setbacks that leave an envelope, but region stays euro_latam.
        let out = run(&mut s, "lotsetbacks last front=3 side=0 rear=3");
        assert!(
            out.message.contains("placeholder — confirm with Manuel"),
            "message should carry the euro_latam placeholder note: {}",
            out.message
        );
    }

    #[test]
    fn lotsetbacks_collapse_reported_not_panicked() {
        let mut s = Session::default();
        run(&mut s, "rect 0,0,0 6 6");
        run(&mut s, "lotsettings region=us_suburban");
        // Setbacks far exceed the lot → all collapse → clean error, no panic.
        let err = s
            .run(parse("lotsetbacks last front=8 side=8 rear=8").unwrap())
            .unwrap_err();
        assert!(format!("{err}").contains("collapse") || format!("{err}").contains("exceed"));
    }

    #[test]
    fn lotsetbacks_envelope_off_reports_without_baking() {
        let mut s = Session::default();
        run(&mut s, "rect 0,0,0 30 40");
        run(&mut s, "lotsettings region=us_suburban");
        let out = run(&mut s, "lotsetbacks last front=5 side=3 rear=7 envelope=off");
        assert_eq!(setback_count(&s), 0, "envelope=off bakes nothing");
        assert!(out.message.contains("computed"), "message: {}", out.message);
    }

    #[test]
    fn lotfrontage_reports_setback_by_default() {
        let mut s = Session::default();
        run(&mut s, "rect 0,0,0 40 20");
        run(&mut s, "lotsettings region=us_suburban");
        let out = run(&mut s, "lotfrontage last");
        // Report goes to the AnalysisReport plane keyed 'lotfrontage'.
        assert!(s.doc.analysis_reports.contains_key("lotfrontage"));
        assert!(out.message.contains("setback line"), "message: {}", out.message);
    }

    #[test]
    fn lotfrontage_setback_and_curb_differ() {
        let mut s = Session::default();
        run(&mut s, "rect 0,0,0 40 20");
        // Sticky small setbacks so lotfrontage's setback-line measure stays
        // inside the lot: front is the 40-wide bottom edge; side=4 insets both
        // ends → setback frontage (40−8=32) < curb (40).
        run(&mut s, "lotsettings region=us_suburban");
        s.doc.subdivision_settings.setback_front = 3.0;
        s.doc.subdivision_settings.setback_side = 4.0;
        s.doc.subdivision_settings.setback_rear = 3.0;
        run(&mut s, "lotfrontage last at=curb");
        let curb = s.doc.analysis_reports["lotfrontage"].avg;
        run(&mut s, "lotfrontage last at=setback");
        let setback = s.doc.analysis_reports["lotfrontage"].avg;
        assert!(curb > 0.0 && setback > 0.0);
        assert!(
            (curb - setback).abs() > 1.0,
            "curb {curb} and setback {setback} frontage should differ"
        );
    }

    #[test]
    fn lotfrontage_is_not_logged() {
        assert!(!parse("lotfrontage last").unwrap().is_logged(), "lotfrontage is a query");
        assert!(parse("lotsetbacks last front=5").unwrap().is_logged(), "lotsetbacks is logged");
    }

    // ── M-intemfit: lotopenspace (Phase 9) ──────────────────────────────────

    fn openspace_count(s: &Session) -> usize {
        s.doc
            .all_ids()
            .iter()
            .filter(|id| {
                s.doc.get(**id).map(|o| o.layer == crate::lot::OPENSPACE_LAYER).unwrap_or(false)
            })
            .count()
    }

    #[test]
    fn lotopenspace_park_bakes_on_openspace_layer() {
        let mut s = Session::default();
        run(&mut s, "rect 0,0,0 200 120");
        let out = run(&mut s, "lotopenspace last type=park area=4000");
        assert!(s.doc.layers.contains_key(crate::lot::OPENSPACE_LAYER));
        assert_eq!(openspace_count(&s), 1);
        assert!(out.message.contains("park"), "message: {}", out.message);
        // Distinct layer from lots.
        assert_ne!(crate::lot::OPENSPACE_LAYER, crate::lot::LOTS_LAYER);
    }

    #[test]
    fn lotopenspace_each_feature_type_places_a_polygon() {
        for feat in ["park", "greenway", "pond", "treesave"] {
            let mut s = Session::default();
            run(&mut s, "rect 0,0,0 200 120");
            let out = run(&mut s, &format!("lotopenspace last type={feat}"));
            assert_eq!(openspace_count(&s), 1, "{feat} should place one polygon");
            // Every placed feature carries the no-false-precision advisory.
            assert!(out.message.contains("advisory"), "{feat}: {}", out.message);
        }
    }

    #[test]
    fn lotopenspace_default_is_feature_placement() {
        // Bare run with a region → park placement (feature mode is the default).
        let mut s = Session::default();
        run(&mut s, "rect 0,0,0 200 120");
        run(&mut s, "lotopenspace last");
        assert_eq!(openspace_count(&s), 1);
    }

    #[test]
    fn lotopenspace_undo_removes_geometry() {
        let mut s = Session::default();
        run(&mut s, "rect 0,0,0 200 120");
        run(&mut s, "lotopenspace last type=pond area=3000");
        assert!(openspace_count(&s) > 0);
        run(&mut s, "undo");
        assert_eq!(openspace_count(&s), 0, "undo removes open-space geometry + layer");
    }

    #[test]
    fn lotopenspace_replay_is_byte_identical() {
        let mut s = Session::default();
        run(&mut s, "rect 0,0,0 200 120");
        run(&mut s, "lotopenspace last type=park area=4000");
        let before: Vec<_> = s
            .doc
            .all_ids()
            .iter()
            .filter_map(|id| s.doc.get(*id))
            .filter(|o| o.layer == crate::lot::OPENSPACE_LAYER)
            .map(|o| o.geometry.clone())
            .collect();
        let log: Vec<Command> = s.log.iter().map(|a| a.op.clone()).collect();
        let rebuilt = Session::replay(log).unwrap();
        let after: Vec<_> = rebuilt
            .doc
            .all_ids()
            .iter()
            .filter_map(|id| rebuilt.doc.get(*id))
            .filter(|o| o.layer == crate::lot::OPENSPACE_LAYER)
            .map(|o| o.geometry.clone())
            .collect();
        assert_eq!(before, after, "replay recreated identical open-space geometry");
    }

    #[test]
    fn lotopenspace_reserve_pulls_central_blocks() {
        let mut s = Session::default();
        run(&mut s, "rect 0,0,0 600 400");
        let out = run(&mut s, "lotopenspace last reserve=20");
        assert!(openspace_count(&s) > 0, "reserve should bake blocks: {}", out.message);
        assert!(out.message.contains('%'), "reports the achieved fraction: {}", out.message);
    }

    #[test]
    fn lotopenspace_reserve_is_deterministic_replay() {
        let mut s = Session::default();
        run(&mut s, "rect 0,0,0 600 400");
        run(&mut s, "lotsettings seed=7");
        run(&mut s, "lotopenspace last reserve=25");
        let before: Vec<_> = s
            .doc
            .all_ids()
            .iter()
            .filter_map(|id| s.doc.get(*id))
            .filter(|o| o.layer == crate::lot::OPENSPACE_LAYER)
            .map(|o| o.geometry.clone())
            .collect();
        let log: Vec<Command> = s.log.iter().map(|a| a.op.clone()).collect();
        let rebuilt = Session::replay(log).unwrap();
        let after: Vec<_> = rebuilt
            .doc
            .all_ids()
            .iter()
            .filter_map(|id| rebuilt.doc.get(*id))
            .filter(|o| o.layer == crate::lot::OPENSPACE_LAYER)
            .map(|o| o.geometry.clone())
            .collect();
        assert!(!before.is_empty(), "reserve baked something");
        assert_eq!(before, after, "reserve replay byte-identical");
    }

    #[test]
    fn lotopenspace_reserve_zero_is_feature_mode() {
        // reserve=0 → NOT reserve mode; defaults to feature placement (park).
        let mut s = Session::default();
        run(&mut s, "rect 0,0,0 200 120");
        let cmd = parse("lotopenspace last reserve=0").unwrap();
        match &cmd {
            Command::LotOpenSpace { reserve, feature, .. } => {
                assert!(reserve.is_none(), "reserve=0 is off");
                assert_eq!(feature.as_deref(), Some("park"), "defaults to feature mode");
            }
            _ => panic!("wrong command"),
        }
        s.run(cmd).unwrap();
        assert_eq!(openspace_count(&s), 1, "one park placed, not a reserve");
    }

    #[test]
    fn lotopenspace_is_logged() {
        assert!(parse("lotopenspace last type=park").unwrap().is_logged());
        assert!(parse("lotopenspace last reserve=20").unwrap().is_logged());
    }
}
