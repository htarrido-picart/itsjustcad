// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! Rhino-style **guided command engine**: type a bare verb (`offset`) and get
//! walked through its steps — pick points, type distances, etc. — in the command
//! line and viewport, then it emits the *same canonical command string* the
//! parser already accepts. Guided runs flow through the identical substrate as
//! typed or LLM commands (op-logged, undoable, replay-stable).
//!
//! This mirrors [`crate::draw_tool`] (the step machine for *creation* verbs) but
//! generalizes the step to a heterogeneous list driven by a per-verb script.
//! Pure logic lives here and is unit-tested; egui/rendering/snap timing stay in
//! `app.rs`, which drives this the same way it drives `draw_tool`.

pub(crate) use crate::draw_tool::{fmt, num};
use glam::DVec3;

// Per-group verb-script modules. Each owns one file and exposes a `SCRIPTS`
// slice folded into `lookup`. `offset` is the locked reference example.
mod annotate;
mod array;
mod boolean;
mod creation;
mod curve_edit;
mod offset;
mod organize;
mod reference_hatch;
mod structure;
mod transform;

/// One prompt-and-collect step in a verb's guided flow.
#[derive(Clone, Copy)]
pub enum Step {
    /// A world point (osnap/ortho/smarttrack aware, resolved by the app layer).
    PickPoint { prompt: &'static str },
    /// A typed number; `default` (if any) is accepted on a bare Enter.
    Number { prompt: &'static str, default: Option<f64> },
    /// A typed whole number (counts, divisions); `default` accepted on Enter.
    // `allow(dead_code)`: consumed by the group modules as they populate; drop
    // the allow once every group is filled in.
    #[allow(dead_code)]
    Integer { prompt: &'static str, default: Option<i64> },
    /// A command-line option switch. The user types the option (or a unique
    /// prefix / first letter); a bare Enter takes `default`.
    #[allow(dead_code)]
    Keyword { prompt: &'static str, options: &'static [&'static str], default: &'static str },
    /// Interactively pick ONE object in the viewport (Rhino's `GetObject.Get`).
    /// The app hit-tests the click, enforces `filter`, and commits the object's
    /// short id as an `Objects("#<id>")` selector token — so the picked object
    /// rides through the parser like any other selector (no parser change). This
    /// is how verb-first and two-role commands (trim, fillet, difference) pick
    /// each role separately.
    SelectObject { prompt: &'static str, filter: ObjFilter },
    /// A free-text token (an object name, block name, layer name). Collects a
    /// single whitespace-free token so the value round-trips through the
    /// whitespace-tokenized parser. Letters/digits plus `_`/`-` are accepted.
    Text { prompt: &'static str },
    /// A variadic list of world points (Rhino's `GetPoints`): each click/typed
    /// coord appends one, Enter finishes once at least `min` are collected.
    /// Modeled on the polyline draw tool.
    PointList { prompt: &'static str, min: usize },
    /// A direction/vector, collected exactly like a [`Step::PickPoint`]: typed
    /// `dx,dy,dz` resolves via precise-input, or a click supplies a point (the
    /// vector from the origin, or relative to the prior pick). Stored as an
    /// [`Input::Point`]; the assembler reads it with `point_at` and formats it
    /// with `fmt`. Powers `load` directions and roller support axes.
    Vector { prompt: &'static str },
    /// A **keyword branch**: the user picks one arm by key (exact or
    /// unique-prefix, case-insensitive), and that arm's `steps` become all the
    /// remaining steps. MUST be the last entry in a script's `steps` (the chosen
    /// arm supplies every following step — no base steps come after a Branch).
    Branch { prompt: &'static str, arms: &'static [BranchArm] },
}

/// One arm of a [`Step::Branch`]: a keyword `key` and the `steps` that run once
/// the user selects it. The selected `key` is committed as an [`Input::Key`] at
/// the branch's own slot, followed by the arm's collected inputs.
#[derive(Clone, Copy)]
pub struct BranchArm {
    pub key: &'static str,
    pub steps: &'static [Step],
}

/// Restricts what a [`Step::SelectObject`] pick will accept (Rhino's
/// `GeometryFilter`). The app matches this against the hit object's geometry.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ObjFilter {
    Any,
    Curve,
    /// A solid/mesh body (boolean operands).
    Solid,
}

/// A value collected for a step (or seeded from a pre-selection).
#[derive(Clone, Debug, PartialEq)]
pub enum Input {
    /// A canonical selector string for the objects the verb acts on (e.g.
    /// `"sel"` for the current selection, or `"#a1b2c3d4"` for a picked object).
    Objects(String),
    Point(DVec3),
    Num(f64),
    Int(i64),
    Key(String),
    /// A free-text token collected by a [`Step::Text`].
    Text(String),
    /// A variadic point list collected by a [`Step::PointList`].
    Points(Vec<DVec3>),
}

/// A verb's guided script: its ordered steps plus a **pure** assembler turning
/// the collected inputs into the exact canonical command string.
pub struct VerbScript {
    pub verb: &'static str,
    pub steps: &'static [Step],
    /// `(seed ++ collected) -> "offset sel 5 3,0"`. Pure and unit-tested.
    pub assemble: fn(&[Input]) -> Result<String, String>,
    /// The verb operates on a pre-selection (noun-verb). When true and nothing
    /// is selected, [`GuidedTool::try_start`] returns [`StartResult::NeedSelection`].
    pub needs_selection: bool,
}

/// Outcome of feeding a pick/number to the tool.
#[derive(Debug, PartialEq)]
pub enum StepResult {
    /// More steps remain; the caller should re-show the prompt.
    NeedMore,
    /// All steps collected — run this canonical command string.
    Emit(String),
    /// The input was invalid; surface the message, stay on the step.
    Error(String),
}

/// Outcome of attempting to start a guided flow from a submitted line.
#[derive(Debug, PartialEq)]
pub enum StartResult {
    /// Guided flow armed; show `prompt()`.
    Started,
    /// A guided verb, but it needs a pre-selection and none is present.
    NeedSelection,
    /// Not a guided verb (or had inline args): fall through to the parser.
    NotGuided,
}

#[derive(Default)]
pub struct GuidedTool {
    script: Option<&'static VerbScript>,
    /// Inputs seeded before any step (e.g. the pre-selection selector).
    seed: Vec<Input>,
    /// One collected input per completed step.
    done: Vec<Input>,
    /// Typed buffer for the current step (numbers, or typed coords for a point).
    input: String,
    /// Points collected so far for an in-progress [`Step::PointList`] step.
    /// Emptied when the step finishes (folded into an [`Input::Points`]).
    list: Vec<DVec3>,
    /// The chosen [`Step::Branch`] arm's steps, once selected. `None` until the
    /// branch is committed; then it supplies every step after the branch slot.
    branch: Option<&'static [Step]>,
}

impl GuidedTool {
    pub fn active(&self) -> bool {
        self.script.is_some()
    }

    /// Start a guided flow if `line` is a bare guided verb. `selection` is the
    /// canonical selector for the current selection (e.g. `Some("sel")`), or
    /// `None` when nothing is selected.
    pub fn try_start(&mut self, line: &str, selection: Option<&str>) -> StartResult {
        let verb = line.trim();
        let Some(script) = lookup(verb) else {
            return StartResult::NotGuided;
        };
        debug_assert_eq!(script.verb, verb, "registry key must match the script's verb");
        self.seed.clear();
        if script.needs_selection {
            match selection {
                Some(sel) if !sel.is_empty() => self.seed.push(Input::Objects(sel.to_string())),
                _ => return StartResult::NeedSelection,
            }
        }
        self.script = Some(script);
        self.done.clear();
        self.input.clear();
        self.list.clear();
        self.branch = None;
        StartResult::Started
    }

    pub fn cancel(&mut self) {
        self.reset();
    }

    fn reset(&mut self) {
        self.script = None;
        self.seed.clear();
        self.done.clear();
        self.input.clear();
        self.list.clear();
        self.branch = None;
    }

    /// Index of the (single, last-if-present) [`Step::Branch`] in the base
    /// script, or `None` when the script has no branch.
    fn branch_index(&self) -> Option<usize> {
        let base = self.script?.steps;
        base.iter().position(|s| matches!(s, Step::Branch { .. }))
    }

    /// The current step, branch-aware. Before the branch slot the base steps
    /// drive; at the branch slot (still unchosen) the `Branch` step itself is
    /// current; once an arm is chosen its steps supply everything after the slot.
    fn current_step(&self) -> Option<&'static Step> {
        let base = self.script?.steps;
        let n = self.done.len();
        match self.branch_index() {
            None => base.get(n),
            Some(bi) => {
                if n < bi {
                    base.get(n)
                } else if n == bi && self.branch.is_none() {
                    base.get(bi) // the Branch step (a keyword prompt)
                } else {
                    // The arm covers positions > bi; its committed Key sits at
                    // done[bi], so the arm's own index is n - bi - 1.
                    self.branch?.get(n - bi - 1)
                }
            }
        }
    }

    /// True when the current step expects a picked/typed **point** (so the app
    /// resolves Enter's typed buffer through `precise::resolve_input`). A
    /// [`Step::Vector`] is collected identically to a point (typed `dx,dy,dz` or
    /// a click), so it also reports true here — the app path is the same.
    pub fn current_is_point(&self) -> bool {
        matches!(self.current_step(), Some(Step::PickPoint { .. } | Step::Vector { .. }))
    }

    /// True when the current step expects an interactive **object pick** (so the
    /// app hit-tests the click to an `ObjectId` instead of a world point).
    pub fn current_wants_object(&self) -> bool {
        matches!(self.current_step(), Some(Step::SelectObject { .. }))
    }

    /// True when the current step collects a free-text token ([`Step::Text`]).
    pub fn current_wants_text(&self) -> bool {
        matches!(self.current_step(), Some(Step::Text { .. }))
    }

    /// True when the current step is a variadic point list ([`Step::PointList`]).
    pub fn current_wants_point_list(&self) -> bool {
        matches!(self.current_step(), Some(Step::PointList { .. }))
    }

    /// The geometry filter for the current [`Step::SelectObject`], if any.
    pub fn current_filter(&self) -> Option<ObjFilter> {
        match self.current_step() {
            Some(Step::SelectObject { filter, .. }) => Some(*filter),
            _ => None,
        }
    }

    /// Commit an interactively picked object by its short id. The id is stored
    /// as a `#`-prefixed selector token (`#a1b2c3d4`), which the parser resolves
    /// via `find_named`'s short-id match — the `#` keeps it a valid selector even
    /// when the id starts with a digit.
    pub fn commit_object(&mut self, short_id: &str) -> StepResult {
        if !matches!(self.current_step(), Some(Step::SelectObject { .. })) {
            return StepResult::Error("not expecting an object pick here".into());
        }
        self.done.push(Input::Objects(format!("#{short_id}")));
        self.input.clear();
        self.maybe_finish()
    }

    /// Commit the current typed buffer to a [`Step::Text`] step. An empty buffer
    /// is rejected (a name is required). The value is a single whitespace-free
    /// token, so it round-trips through the parser.
    pub fn commit_text(&mut self) -> StepResult {
        if !matches!(self.current_step(), Some(Step::Text { .. })) {
            return StepResult::Error("not expecting a name here".into());
        }
        let buf = self.input.trim().to_string();
        if buf.is_empty() {
            return StepResult::Error("type a name".into());
        }
        self.done.push(Input::Text(buf));
        self.input.clear();
        self.maybe_finish()
    }

    /// Append one point to an in-progress [`Step::PointList`] (a click or typed
    /// coord). No-op with `NeedMore` if the current step isn't a point list.
    pub fn push_list_point(&mut self, world: DVec3) -> StepResult {
        if !matches!(self.current_step(), Some(Step::PointList { .. })) {
            return StepResult::NeedMore;
        }
        self.list.push(world);
        self.input.clear();
        StepResult::NeedMore
    }

    /// Finish a [`Step::PointList`] on Enter: if at least `min` points were
    /// collected, fold them into an [`Input::Points`] and advance; otherwise
    /// stay on the step and report how many more are needed.
    pub fn finish_list(&mut self) -> StepResult {
        let Some(Step::PointList { min, .. }) = self.current_step() else {
            return StepResult::Error("not expecting a point list here".into());
        };
        let min = *min;
        if self.list.len() < min {
            return StepResult::Error(format!(
                "pick at least {min} points ({} so far)",
                self.list.len()
            ));
        }
        let pts = std::mem::take(&mut self.list);
        self.done.push(Input::Points(pts));
        self.input.clear();
        self.maybe_finish()
    }

    /// Last picked point among seed+collected — anchor for relative/ortho input.
    pub fn last_point(&self) -> Option<DVec3> {
        self.seed
            .iter()
            .chain(self.done.iter())
            .rev()
            .find_map(|i| match i {
                Input::Point(p) => Some(*p),
                _ => None,
            })
    }

    /// Feed one typed char to the buffer. Returns true when consumed. Keyword
    /// steps accept letters (option names); every other step takes numeric
    /// precise-input material only.
    pub fn push_input(&mut self, c: char) -> bool {
        if !self.active() {
            return false;
        }
        let ok = match self.current_step() {
            // Keyword and Branch are both keyword-like: they take option/arm names.
            Some(Step::Keyword { .. } | Step::Branch { .. }) => c.is_alphanumeric(),
            // A name token: letters/digits plus `_`/`-`, no whitespace (keeps the
            // emitted value a single token for the whitespace-tokenized parser).
            Some(Step::Text { .. }) => c.is_alphanumeric() || c == '_' || c == '-',
            // Object picks are click-only — nothing to type into a buffer.
            Some(Step::SelectObject { .. }) => false,
            _ => crate::precise::accepts_char(c),
        };
        if ok {
            self.input.push(c);
        }
        ok
    }

    pub fn pop_input(&mut self) -> bool {
        self.input.pop().is_some()
    }

    pub fn take_input(&mut self) -> String {
        std::mem::take(&mut self.input)
    }

    pub fn prompt(&self) -> Option<String> {
        let step = self.current_step()?;
        let base = match step {
            Step::Number { prompt, default } => match default {
                Some(d) => format!("{prompt} <{}>:", num(*d)),
                None => format!("{prompt}:"),
            },
            Step::Integer { prompt, default } => match default {
                Some(d) => format!("{prompt} <{d}>:"),
                None => format!("{prompt}:"),
            },
            Step::Keyword { prompt, options, default } => {
                format!("{prompt} ( {} ) <{default}>:", options.join(" / "))
            }
            Step::PickPoint { prompt } => format!("{prompt} (Esc cancels):"),
            Step::Vector { prompt } => format!("{prompt} (Esc cancels):"),
            Step::SelectObject { prompt, .. } => format!("{prompt} (Esc cancels):"),
            Step::Text { prompt } => format!("{prompt}:"),
            Step::PointList { prompt, .. } => {
                format!("{prompt} ({} so far):", self.list.len())
            }
            Step::Branch { prompt, arms } => {
                let keys = arms.iter().map(|a| a.key).collect::<Vec<_>>().join(" / ");
                let first = arms.first().map(|a| a.key).unwrap_or("");
                format!("{prompt} ( {keys} ) <{first}>:")
            }
        };
        Some(if self.input.is_empty() {
            base
        } else {
            format!("{base}  |  typed: {}_", self.input)
        })
    }

    /// Commit a typed buffer to a Number/Integer/Keyword step (the app's Enter
    /// path for every non-point step). An empty buffer accepts the step's
    /// default. Point steps aren't handled here — the app resolves those to a
    /// pick via `precise::resolve_input` + [`Self::on_click`].
    pub fn commit_typed(&mut self, buf: &str) -> StepResult {
        let b = buf.trim();
        match self.current_step() {
            Some(Step::Number { default, .. }) => {
                let v = if b.is_empty() { *default } else { b.parse::<f64>().ok() };
                match v {
                    Some(v) => {
                        self.done.push(Input::Num(v));
                        self.input.clear();
                        self.maybe_finish()
                    }
                    None => StepResult::Error("type a number, or press Enter for the default".into()),
                }
            }
            Some(Step::Integer { default, .. }) => {
                let v = if b.is_empty() { *default } else { b.parse::<i64>().ok() };
                match v {
                    Some(v) => {
                        self.done.push(Input::Int(v));
                        self.input.clear();
                        self.maybe_finish()
                    }
                    None => StepResult::Error(
                        "type a whole number, or press Enter for the default".into(),
                    ),
                }
            }
            Some(Step::Keyword { options, default, .. }) => {
                let chosen = if b.is_empty() {
                    (*default).to_string()
                } else {
                    let lb = b.to_lowercase();
                    match options.iter().find(|o| {
                        let lo = o.to_lowercase();
                        lo == lb || lo.starts_with(&lb)
                    }) {
                        Some(o) => (*o).to_string(),
                        None => {
                            return StepResult::Error(format!(
                                "unknown option '{buf}' — choose {}",
                                options.join(" / ")
                            ));
                        }
                    }
                };
                self.done.push(Input::Key(chosen));
                self.input.clear();
                self.maybe_finish()
            }
            Some(Step::Branch { arms, .. }) => {
                let chosen = if b.is_empty() {
                    match arms.first() {
                        Some(arm) => arm,
                        None => return StepResult::Error("branch has no arms".into()),
                    }
                } else {
                    let lb = b.to_lowercase();
                    match arms.iter().find(|a| {
                        let lk = a.key.to_lowercase();
                        lk == lb || lk.starts_with(&lb)
                    }) {
                        Some(arm) => arm,
                        None => {
                            let keys = arms.iter().map(|a| a.key).collect::<Vec<_>>().join(" / ");
                            return StepResult::Error(format!("unknown option '{buf}' — choose {keys}"));
                        }
                    }
                };
                self.done.push(Input::Key(chosen.key.to_string()));
                self.branch = Some(chosen.steps);
                self.input.clear();
                self.maybe_finish()
            }
            Some(Step::PickPoint { .. } | Step::Vector { .. }) => {
                StepResult::Error("pick a point, or type coordinates".into())
            }
            Some(Step::SelectObject { .. }) => {
                StepResult::Error("click an object to select it".into())
            }
            // Text/PointList have dedicated commit paths (commit_text /
            // finish_list); the app routes their Enter there, not here.
            Some(Step::Text { .. }) => self.commit_text(),
            Some(Step::PointList { .. }) => self.finish_list(),
            None => StepResult::Error("no active step".into()),
        }
    }

    /// Register a canvas pick (already snap-resolved). No-op with `NeedMore` when
    /// the current step isn't a point step, so stray clicks are harmless.
    pub fn on_click(&mut self, world: DVec3) -> StepResult {
        if !matches!(self.current_step(), Some(Step::PickPoint { .. } | Step::Vector { .. })) {
            return StepResult::NeedMore;
        }
        self.done.push(Input::Point(world));
        self.input.clear();
        self.maybe_finish()
    }

    /// Emit immediately if every step is already satisfied — a zero-step verb
    /// on a pre-selection (`area sel`, `volume sel`, `bbox sel`). The app calls
    /// this right after `try_start` returns `Started`: `Some(Emit)` runs the
    /// command now, `None` means there are steps to prompt for first.
    pub fn emit_if_ready(&mut self) -> Option<StepResult> {
        (self.steps_total() == Some(self.done.len())).then(|| self.maybe_finish())
    }

    /// Live ghost geometry to overlay while picking (Rhino-style rubber-band).
    /// Verb-specific ghosts (arc) render their true shape as the cursor moves;
    /// every other point step falls back to connecting the points picked so far
    /// to the cursor — so `distance`/`dim` draw a line to the cursor and
    /// multi-point verbs draw a running polyline. Typed/object steps and a
    /// missing cursor draw nothing.
    pub fn preview(&self, cursor: Option<DVec3>) -> Vec<Vec<DVec3>> {
        let Some(script) = self.script else {
            return Vec::new();
        };
        let Some(cursor) = cursor else {
            return Vec::new();
        };
        let pts: Vec<DVec3> = self
            .done
            .iter()
            .filter_map(|i| match i {
                Input::Point(p) => Some(*p),
                _ => None,
            })
            .collect();
        // Verb-specific live ghosts.
        if script.verb == "arc" {
            return arc_preview(&pts, cursor);
        }
        // A variadic point list draws a running polyline through the points
        // collected so far plus the cursor (Rhino's `GetPoints` rubber-band).
        if matches!(self.current_step(), Some(Step::PointList { .. })) {
            let mut strip = self.list.clone();
            strip.push(cursor);
            return if strip.len() >= 2 { vec![strip] } else { Vec::new() };
        }
        // Generic rubber-band: only while a point step is active.
        if !matches!(self.current_step(), Some(Step::PickPoint { .. })) {
            return Vec::new();
        }
        let mut strip = pts;
        strip.push(cursor);
        // Need at least one prior point to draw a rubber-band to the cursor.
        if strip.len() >= 2 { vec![strip] } else { Vec::new() }
    }

    /// Total steps to collect before the flow completes, branch-aware. Without a
    /// branch it's just `base.len()`. With a branch: once an arm is chosen, it's
    /// `bi + 1` (base steps up to and including the branch's own Key slot) plus
    /// the arm's own steps. Before an arm is chosen the flow can't complete, so
    /// this returns `None` (a sentinel `done.len()` can never reach).
    fn steps_total(&self) -> Option<usize> {
        let base = self.script?.steps;
        match self.branch_index() {
            None => Some(base.len()),
            Some(bi) => self.branch.map(|arm| bi + 1 + arm.len()),
        }
    }

    /// All steps collected → assemble and reset. Otherwise `NeedMore`.
    fn maybe_finish(&mut self) -> StepResult {
        let script = self.script.expect("active during a step commit");
        let total = self.steps_total();
        // No total yet (branch unchosen) or fewer steps done → keep going.
        if total != Some(self.done.len()) {
            return StepResult::NeedMore;
        }
        let mut args = self.seed.clone();
        args.extend(self.done.iter().cloned());
        let result = match (script.assemble)(&args) {
            Ok(cmd) => StepResult::Emit(cmd),
            Err(e) => StepResult::Error(e),
        };
        self.reset();
        result
    }
}

/// Every guided verb, folded from the per-group `SCRIPTS` slices. Verbs must be
/// unique across groups (checked by `registry_verbs_are_unique`).
fn all_scripts() -> impl Iterator<Item = &'static VerbScript> {
    offset::SCRIPTS
        .iter()
        .chain(transform::SCRIPTS)
        .chain(array::SCRIPTS)
        .chain(curve_edit::SCRIPTS)
        .chain(annotate::SCRIPTS)
        .chain(reference_hatch::SCRIPTS)
        .chain(creation::SCRIPTS)
        .chain(boolean::SCRIPTS)
        .chain(organize::SCRIPTS)
        .chain(structure::SCRIPTS)
}

/// Verb-script registry lookup. `None` → not a guided verb (fall through to the
/// parser).
fn lookup(verb: &str) -> Option<&'static VerbScript> {
    all_scripts().find(|s| s.verb == verb)
}

/// A CCW arc/circle as a 48-segment polyline: from `a0` sweeping `sweep`
/// radians about `center` at `radius`.
fn arc_polyline(center: DVec3, radius: f64, a0: f64, sweep: f64) -> Vec<DVec3> {
    const N: usize = 48;
    (0..=N)
        .map(|i| {
            let t = a0 + sweep * (i as f64) / (N as f64);
            center + DVec3::new(radius * t.cos(), radius * t.sin(), 0.0)
        })
        .collect()
}

/// Live ghost for the guided `arc` (center → start point → end point):
/// after the center, a radius ring + radius line to the cursor; after the start
/// point, the CCW arc from the start angle to the cursor angle + radius lines.
fn arc_preview(pts: &[DVec3], cursor: DVec3) -> Vec<Vec<DVec3>> {
    match pts {
        [c] => {
            let r = c.distance(cursor);
            if r < 1e-9 {
                return Vec::new();
            }
            vec![arc_polyline(*c, r, 0.0, std::f64::consts::TAU), vec![*c, cursor]]
        }
        [c, p1] => {
            let r = c.distance(*p1);
            if r < 1e-9 {
                return Vec::new();
            }
            let a0 = (p1.y - c.y).atan2(p1.x - c.x);
            let a1 = (cursor.y - c.y).atan2(cursor.x - c.x);
            let tau = std::f64::consts::TAU;
            let sweep = ((a1 - a0) % tau + tau) % tau; // CCW start→cursor
            vec![arc_polyline(*c, r, a0, sweep), vec![*c, *p1], vec![*c, cursor]]
        }
        _ => Vec::new(),
    }
}

/// Shared assembler helpers for the per-group modules.
// `allow(dead_code)`: not every helper is used until all groups are populated.
#[allow(dead_code)]
mod assemble {
    use super::Input;

    /// The seeded selector (e.g. `"sel"`) at index 0.
    pub fn selector(args: &[Input], verb: &str) -> Result<String, String> {
        match args.first() {
            Some(Input::Objects(s)) => Ok(s.clone()),
            _ => Err(format!("{verb}: no selection")),
        }
    }

    /// An `Objects` selector token at index `i` — a seeded `sel` or an
    /// interactively picked `#<id>` from a [`super::Step::SelectObject`].
    pub fn obj_at(args: &[Input], i: usize, verb: &str) -> Result<String, String> {
        match args.get(i) {
            Some(Input::Objects(s)) => Ok(s.clone()),
            _ => Err(format!("{verb}: missing object at step {i}")),
        }
    }

    pub fn num_at(args: &[Input], i: usize, verb: &str) -> Result<f64, String> {
        match args.get(i) {
            Some(Input::Num(v)) => Ok(*v),
            _ => Err(format!("{verb}: missing number at step {i}")),
        }
    }

    pub fn int_at(args: &[Input], i: usize, verb: &str) -> Result<i64, String> {
        match args.get(i) {
            Some(Input::Int(v)) => Ok(*v),
            _ => Err(format!("{verb}: missing whole number at step {i}")),
        }
    }

    pub fn point_at(args: &[Input], i: usize, verb: &str) -> Result<glam::DVec3, String> {
        match args.get(i) {
            Some(Input::Point(p)) => Ok(*p),
            _ => Err(format!("{verb}: missing point at step {i}")),
        }
    }

    pub fn key_at<'a>(args: &'a [Input], i: usize, verb: &str) -> Result<&'a str, String> {
        match args.get(i) {
            Some(Input::Key(k)) => Ok(k.as_str()),
            _ => Err(format!("{verb}: missing option at step {i}")),
        }
    }

    /// A free-text token at index `i` (a name, block name, layer name).
    pub fn text_at<'a>(args: &'a [Input], i: usize, verb: &str) -> Result<&'a str, String> {
        match args.get(i) {
            Some(Input::Text(s)) => Ok(s.as_str()),
            _ => Err(format!("{verb}: missing name at step {i}")),
        }
    }

    /// A variadic point list at index `i`.
    pub fn points_at<'a>(
        args: &'a [Input],
        i: usize,
        verb: &str,
    ) -> Result<&'a [glam::DVec3], String> {
        match args.get(i) {
            Some(Input::Points(p)) => Ok(p.as_slice()),
            _ => Err(format!("{verb}: missing points at step {i}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_verb_with_selection_starts_and_walks_to_emit() {
        let mut t = GuidedTool::default();
        assert_eq!(t.try_start("offset", Some("sel")), StartResult::Started);
        assert!(t.active());
        // Step 1: distance. Prompt shows the default.
        assert_eq!(t.prompt().unwrap(), "Offset distance <1>:");
        assert!(t.current_is_point() == false);
        assert_eq!(t.commit_typed("5"), StepResult::NeedMore);
        // Step 2: side point.
        assert!(t.current_is_point());
        assert!(t.prompt().unwrap().starts_with("Side to offset toward"));
        assert_eq!(
            t.on_click(DVec3::new(3.0, 0.0, 0.0)),
            StepResult::Emit("offset sel 5 3,0".into())
        );
        // Finishing resets the tool.
        assert!(!t.active());
    }

    #[test]
    fn no_selection_reports_need_selection() {
        let mut t = GuidedTool::default();
        assert_eq!(t.try_start("offset", None), StartResult::NeedSelection);
        assert!(!t.active(), "must not arm without a selection");
        assert_eq!(t.try_start("offset", Some("")), StartResult::NeedSelection);
    }

    #[test]
    fn verb_with_inline_args_is_not_guided() {
        let mut t = GuidedTool::default();
        // Only a BARE verb starts guided; args fall through to the parser.
        assert_eq!(t.try_start("offset last 0.2", Some("sel")), StartResult::NotGuided);
        // A verb that isn't in any group's registry is never guided.
        assert_eq!(t.try_start("sphere", Some("sel")), StartResult::NotGuided);
    }

    #[test]
    fn default_accepted_on_bare_enter() {
        let mut t = GuidedTool::default();
        t.try_start("offset", Some("sel"));
        // An empty Enter buffer accepts the distance step's default (1.0).
        assert_eq!(t.commit_typed(""), StepResult::NeedMore);
        assert_eq!(
            t.on_click(DVec3::new(-2.0, 0.0, 0.0)),
            StepResult::Emit("offset sel 1 -2,0".into())
        );
    }

    #[test]
    fn clicks_on_a_number_step_are_ignored() {
        let mut t = GuidedTool::default();
        t.try_start("offset", Some("sel"));
        // First step is Number; a stray click must not advance it.
        assert_eq!(t.on_click(DVec3::new(9.0, 9.0, 0.0)), StepResult::NeedMore);
        assert!(!t.current_is_point(), "still on the distance step");
    }

    #[test]
    fn side_point_z_is_preserved_in_the_command() {
        let mut t = GuidedTool::default();
        t.try_start("offset", Some("sel"));
        t.commit_typed("2.5");
        // A 3-D side point keeps z (fmt drops z only when ~0).
        assert_eq!(
            t.on_click(DVec3::new(1.0, 2.0, 3.0)),
            StepResult::Emit("offset sel 2.5 1,2,3".into())
        );
    }

    #[test]
    fn buffer_edits_and_clears_across_runs() {
        let mut t = GuidedTool::default();
        assert!(!t.push_input('5'), "inactive tool consumes nothing");
        t.try_start("offset", Some("sel"));
        for c in "5.2".chars() {
            assert!(t.push_input(c));
        }
        assert!(!t.push_input('x'), "letters are not numeric input");
        assert!(t.prompt().unwrap().contains("typed: 5.2_"));
        assert!(t.pop_input());
        assert_eq!(t.take_input(), "5.");
        // Cancel clears everything; a fresh start has no stale buffer.
        t.cancel();
        assert!(!t.active());
        t.try_start("offset", Some("sel"));
        assert_eq!(t.take_input(), "");
    }

    #[test]
    fn last_point_tracks_the_side_pick() {
        let mut t = GuidedTool::default();
        t.try_start("offset", Some("sel"));
        assert_eq!(t.last_point(), None, "no point picked yet");
        t.commit_typed("1");
        assert_eq!(t.last_point(), None, "distance is not a point");
    }

    #[test]
    fn registry_verbs_are_unique() {
        // A verb defined in two groups would make `lookup` order-dependent.
        let mut seen = std::collections::HashSet::new();
        for s in all_scripts() {
            assert!(seen.insert(s.verb), "duplicate guided verb: {}", s.verb);
        }
    }

    #[test]
    fn commit_typed_parses_number_default_and_error() {
        let mut t = GuidedTool::default();
        t.try_start("offset", Some("sel"));
        // Empty buffer → default (1.0), typed → override, garbage → error.
        assert_eq!(t.commit_typed(""), StepResult::NeedMore);
        assert_eq!(t.last_point(), None);
        // Back to a fresh run to test override + error.
        t.cancel();
        t.try_start("offset", Some("sel"));
        assert!(matches!(t.commit_typed("abc"), StepResult::Error(_)));
        assert_eq!(t.commit_typed("7"), StepResult::NeedMore);
        assert_eq!(
            t.on_click(DVec3::new(1.0, 0.0, 0.0)),
            StepResult::Emit("offset sel 7 1,0".into())
        );
    }

    // A throwaway keyword+integer script exercising the generic step kinds the
    // groups rely on (the real verbs live in the per-group modules).
    static KW_TEST: VerbScript = VerbScript {
        verb: "__kwtest",
        needs_selection: false,
        steps: &[
            Step::Integer { prompt: "Count", default: Some(3) },
            Step::Keyword { prompt: "Type", options: &["Sharp", "Round"], default: "Sharp" },
        ],
        assemble: |args| {
            let n = super::assemble::int_at(args, 0, "kw")?;
            let k = super::assemble::key_at(args, 1, "kw")?;
            Ok(format!("kw {n} {k}"))
        },
    };

    #[test]
    fn integer_and_keyword_steps_walk_and_assemble() {
        let mut t = GuidedTool { script: Some(&KW_TEST), ..Default::default() };
        assert_eq!(t.prompt().unwrap(), "Count <3>:");
        // Integer: bare Enter takes the default.
        assert_eq!(t.commit_typed(""), StepResult::NeedMore);
        assert_eq!(t.prompt().unwrap(), "Type ( Sharp / Round ) <Sharp>:");
        // Keyword: a unique prefix resolves the option.
        assert_eq!(t.commit_typed("ro"), StepResult::Emit("kw 3 Round".into()));
    }

    #[test]
    fn keyword_step_accepts_letters_and_rejects_unknown() {
        let mut t = GuidedTool { script: Some(&KW_TEST), ..Default::default() };
        t.commit_typed("5"); // integer step
        // Letters feed the keyword buffer (numeric-only rejects them elsewhere).
        assert!(t.push_input('r'));
        assert_eq!(t.take_input(), "r");
        assert!(matches!(t.commit_typed("xyz"), StepResult::Error(_)));
    }

    // A throwaway two-role object-pick script (like trim/fillet/difference).
    static OBJ_TEST: VerbScript = VerbScript {
        verb: "__objtest",
        needs_selection: false,
        steps: &[
            Step::SelectObject { prompt: "First curve", filter: ObjFilter::Curve },
            Step::SelectObject { prompt: "Second curve", filter: ObjFilter::Curve },
        ],
        assemble: |args| {
            let a = super::assemble::obj_at(args, 0, "obj")?;
            let b = super::assemble::obj_at(args, 1, "obj")?;
            Ok(format!("obj {a} {b}"))
        },
    };

    #[test]
    fn select_object_steps_walk_and_emit_hash_ids() {
        let mut t = GuidedTool { script: Some(&OBJ_TEST), ..Default::default() };
        assert!(t.current_wants_object());
        assert_eq!(t.current_filter(), Some(ObjFilter::Curve));
        assert!(t.prompt().unwrap().starts_with("First curve"));
        // Object steps are click-only: typing and point-clicks don't advance.
        assert!(!t.push_input('a'), "object step takes no typed buffer");
        assert_eq!(t.on_click(DVec3::ZERO), StepResult::NeedMore);
        // A pick commits the short id as a #-prefixed selector token.
        assert_eq!(t.commit_object("a1b2c3d4"), StepResult::NeedMore);
        assert_eq!(
            t.commit_object("00ffee11"),
            StepResult::Emit("obj #a1b2c3d4 #00ffee11".into())
        );
        assert!(!t.active());
    }

    #[test]
    fn commit_object_rejects_when_not_an_object_step() {
        let mut t = GuidedTool::default();
        t.try_start("offset", Some("sel")); // first step is a Number
        assert!(matches!(t.commit_object("a1b2c3d4"), StepResult::Error(_)));
        assert!(!t.current_wants_object());
    }

    #[test]
    fn point_step_preview_rubber_bands_to_cursor() {
        let mut t = GuidedTool::default();
        t.try_start("distance", None); // two point steps
        let cursor = DVec3::new(5.0, 0.0, 0.0);
        // First point step: nothing picked yet → no rubber-band to draw.
        assert!(t.preview(Some(cursor)).is_empty());
        t.on_click(DVec3::ZERO);
        // Second point step: a live line from the first point to the cursor.
        assert_eq!(t.preview(Some(cursor)), vec![vec![DVec3::ZERO, cursor]]);
        // No cursor → nothing.
        assert!(t.preview(None).is_empty());
    }

    #[test]
    fn typed_and_object_steps_have_no_preview() {
        // A Number step (offset distance) draws no ghost.
        let mut t = GuidedTool::default();
        t.try_start("offset", Some("sel"));
        assert!(t.preview(Some(DVec3::new(1.0, 1.0, 0.0))).is_empty());
    }

    // A throwaway script exercising the Text and PointList step kinds (the real
    // verbs live in the per-group modules).
    static TXT_LIST_TEST: VerbScript = VerbScript {
        verb: "__txtlist",
        needs_selection: false,
        steps: &[
            Step::Text { prompt: "Name" },
            Step::PointList { prompt: "Pick points", min: 2 },
        ],
        assemble: |args| {
            let name = super::assemble::text_at(args, 0, "tl")?;
            let pts = super::assemble::points_at(args, 1, "tl")?;
            let joined = pts.iter().map(|p| fmt(*p)).collect::<Vec<_>>().join(" ");
            Ok(format!("tl {name} {joined}"))
        },
    };

    #[test]
    fn text_step_collects_a_name_and_rejects_empty() {
        let mut t = GuidedTool { script: Some(&TXT_LIST_TEST), ..Default::default() };
        assert!(t.current_wants_text());
        assert_eq!(t.prompt().unwrap(), "Name:");
        // Empty buffer is refused; the step stays put.
        assert!(matches!(t.commit_text(), StepResult::Error(_)));
        assert!(t.current_wants_text(), "still on the name step");
        // Letters, digits, `_` and `-` feed the buffer; whitespace does not.
        for c in "widget-1".chars() {
            assert!(t.push_input(c));
        }
        assert!(!t.push_input(' '), "no spaces in a name token");
        assert_eq!(t.commit_text(), StepResult::NeedMore);
        assert!(t.current_wants_point_list());
    }

    #[test]
    fn point_list_finishes_at_min_and_refuses_below() {
        let mut t = GuidedTool { script: Some(&TXT_LIST_TEST), ..Default::default() };
        // Skip past the text step.
        for c in "n".chars() {
            t.push_input(c);
        }
        t.commit_text();
        assert!(t.current_wants_point_list());
        assert_eq!(t.prompt().unwrap(), "Pick points (0 so far):");
        // One point is below min(2): finishing is refused.
        assert_eq!(t.push_list_point(DVec3::new(0.0, 0.0, 0.0)), StepResult::NeedMore);
        assert!(matches!(t.finish_list(), StepResult::Error(_)));
        assert_eq!(t.prompt().unwrap(), "Pick points (1 so far):");
        // A second point reaches min → Enter emits.
        assert_eq!(t.push_list_point(DVec3::new(1.0, 0.0, 0.0)), StepResult::NeedMore);
        assert_eq!(t.finish_list(), StepResult::Emit("tl n 0,0 1,0".into()));
        assert!(!t.active());
    }

    // A throwaway branching script: arm "a" is a short one-number path, arm "b"
    // is a longer two-number path. Exercises branch routing + step counting.
    static BRANCH_TEST: VerbScript = VerbScript {
        verb: "__branchtest",
        needs_selection: false,
        steps: &[
            Step::Number { prompt: "Lead", default: Some(1.0) },
            Step::Branch {
                prompt: "Mode",
                arms: &[
                    BranchArm { key: "alpha", steps: &[Step::Number { prompt: "A1", default: None }] },
                    BranchArm {
                        key: "beta",
                        steps: &[
                            Step::Number { prompt: "B1", default: None },
                            Step::Number { prompt: "B2", default: None },
                        ],
                    },
                ],
            },
        ],
        assemble: |args| {
            let lead = super::assemble::num_at(args, 0, "br")?;
            let key = super::assemble::key_at(args, 1, "br")?;
            let rest = args[2..]
                .iter()
                .map(|i| match i {
                    Input::Num(v) => num(*v).to_string(),
                    _ => "?".into(),
                })
                .collect::<Vec<_>>()
                .join(" ");
            Ok(format!("br {} {key} {rest}", num(lead)))
        },
    };

    #[test]
    fn branch_prompt_lists_arms_and_routes_short_arm() {
        let mut t = GuidedTool { script: Some(&BRANCH_TEST), ..Default::default() };
        // Base step before the branch.
        assert_eq!(t.commit_typed("3"), StepResult::NeedMore);
        // The Branch step prompts with its arm keys and the first as default.
        assert_eq!(t.prompt().unwrap(), "Mode ( alpha / beta ) <alpha>:");
        assert!(!t.current_is_point());
        // Pick the short arm; it has one remaining step (not done yet).
        assert_eq!(t.commit_typed("alpha"), StepResult::NeedMore);
        assert_eq!(t.prompt().unwrap(), "A1:");
        assert_eq!(t.commit_typed("7"), StepResult::Emit("br 3 alpha 7".into()));
        assert!(!t.active());
    }

    #[test]
    fn branch_routes_long_arm_and_counts_all_steps() {
        let mut t = GuidedTool { script: Some(&BRANCH_TEST), ..Default::default() };
        t.commit_typed("3");
        // Unique-prefix, case-insensitive arm match.
        assert_eq!(t.commit_typed("BE"), StepResult::NeedMore);
        assert_eq!(t.prompt().unwrap(), "B1:");
        assert_eq!(t.commit_typed("5"), StepResult::NeedMore);
        assert_eq!(t.prompt().unwrap(), "B2:");
        assert_eq!(t.commit_typed("6"), StepResult::Emit("br 3 beta 5 6".into()));
    }

    #[test]
    fn branch_bare_enter_takes_first_arm_and_rejects_unknown() {
        let mut t = GuidedTool { script: Some(&BRANCH_TEST), ..Default::default() };
        t.commit_typed("1");
        assert!(matches!(t.commit_typed("zzz"), StepResult::Error(_)));
        // Bare Enter selects the first arm (alpha).
        assert_eq!(t.commit_typed(""), StepResult::NeedMore);
        assert_eq!(t.commit_typed("9"), StepResult::Emit("br 1 alpha 9".into()));
    }

    // A throwaway script with a Vector step (typed direction → Input::Point).
    static VEC_TEST: VerbScript = VerbScript {
        verb: "__vectest",
        needs_selection: false,
        steps: &[Step::Vector { prompt: "Direction" }],
        assemble: |args| {
            let v = super::assemble::point_at(args, 0, "vec")?;
            Ok(format!("vec {}", fmt(v)))
        },
    };

    #[test]
    fn vector_step_is_point_like_and_emits_a_vector() {
        let mut t = GuidedTool { script: Some(&VEC_TEST), ..Default::default() };
        // A Vector is collected like a point: current_is_point is true, and a
        // click/typed-coord (fed via on_click by the app) supplies the vector.
        assert!(t.current_is_point());
        assert!(t.prompt().unwrap().starts_with("Direction"));
        // Typed coords go through commit_typed → rejected (the app resolves them
        // to on_click); a resolved point commits and emits.
        assert!(matches!(t.commit_typed("0,0,-1"), StepResult::Error(_)));
        assert_eq!(
            t.on_click(DVec3::new(0.0, 0.0, -1.0)),
            StepResult::Emit("vec 0,0,-1".into())
        );
    }

    #[test]
    fn point_list_preview_shows_running_polyline() {
        let mut t = GuidedTool { script: Some(&TXT_LIST_TEST), ..Default::default() };
        t.push_input('n');
        t.commit_text();
        let cursor = DVec3::new(5.0, 0.0, 0.0);
        // No points yet → nothing to rubber-band.
        assert!(t.preview(Some(cursor)).is_empty());
        t.push_list_point(DVec3::ZERO);
        // One point + cursor → a live line.
        assert_eq!(t.preview(Some(cursor)), vec![vec![DVec3::ZERO, cursor]]);
        t.push_list_point(DVec3::new(1.0, 1.0, 0.0));
        // Two points + cursor → a running strip.
        assert_eq!(
            t.preview(Some(cursor)),
            vec![vec![DVec3::ZERO, DVec3::new(1.0, 1.0, 0.0), cursor]]
        );
    }
}
