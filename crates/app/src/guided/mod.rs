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
mod curve_edit;
mod offset;
mod reference_hatch;
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
}

/// A value collected for a step (or seeded from a pre-selection).
#[derive(Clone, Debug, PartialEq)]
pub enum Input {
    /// A canonical selector string for the objects the verb acts on (e.g.
    /// `"sel"` for the current selection). Seeded at start, never a viewport
    /// step in v1.
    Objects(String),
    Point(DVec3),
    Num(f64),
    Int(i64),
    Key(String),
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
    }

    fn current_step(&self) -> Option<&'static Step> {
        self.script?.steps.get(self.done.len())
    }

    /// True when the current step expects a picked/typed **point** (so the app
    /// resolves Enter's typed buffer through `precise::resolve_input`).
    pub fn current_is_point(&self) -> bool {
        matches!(self.current_step(), Some(Step::PickPoint { .. }))
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
            Some(Step::Keyword { .. }) => c.is_alphanumeric(),
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
            Some(Step::PickPoint { .. }) => {
                StepResult::Error("pick a point, or type coordinates".into())
            }
            None => StepResult::Error("no active step".into()),
        }
    }

    /// Register a canvas pick (already snap-resolved). No-op with `NeedMore` when
    /// the current step isn't a point step, so stray clicks are harmless.
    pub fn on_click(&mut self, world: DVec3) -> StepResult {
        if !matches!(self.current_step(), Some(Step::PickPoint { .. })) {
            return StepResult::NeedMore;
        }
        self.done.push(Input::Point(world));
        self.input.clear();
        self.maybe_finish()
    }

    /// Ghost geometry to overlay. Offset needs the source curve (not available
    /// to this pure engine), so v1 draws no ghost — kept for parity with
    /// `draw_tool` and future verbs (mirror axis, array footprint, …).
    pub fn preview(&self, _cursor: Option<DVec3>) -> Vec<Vec<DVec3>> {
        Vec::new()
    }

    /// All steps collected → assemble and reset. Otherwise `NeedMore`.
    fn maybe_finish(&mut self) -> StepResult {
        let script = self.script.expect("active during a step commit");
        if self.done.len() < script.steps.len() {
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
}

/// Verb-script registry lookup. `None` → not a guided verb (fall through to the
/// parser).
fn lookup(verb: &str) -> Option<&'static VerbScript> {
    all_scripts().find(|s| s.verb == verb)
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
        assert_eq!(t.try_start("box", Some("sel")), StartResult::NotGuided);
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
}
