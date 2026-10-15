// SPDX-License-Identifier: AGPL-3.0-only
// Copyright © 2026 Hector Tarrido-Picart

//! Structured agent messages the deck model can emit beyond plain prose and
//! ```draft fences: a clarifying QUESTION (ask-before-guessing) and a numbered
//! PLAN for prolonged multi-step tasks. Both are advertised in the system
//! prompt (`prompt::CLARIFY_HELP` / `prompt::PLAN_HELP`) and backstopped for
//! small local models by the GBNF grammar
//! (`itsjustcad_commands::gbnf::command_grammar`), which admits exactly these
//! shapes at the token level. The parsers here are pure and line-oriented so
//! the app and the tool loop share one message contract.

use serde::{Deserialize, Serialize};

/// Bounded retry budget per plan step: a step whose commands keep erroring is
/// retried (errors fed back) at most this many times before the step is marked
/// failed and the plan stops. Mirrors the app's per-turn error-retry loop.
pub const MAX_STEP_ATTEMPTS: u32 = 3;

/// Parse a clarifying question the model emitted instead of guessing:
/// a line of the form `QUESTION: <one short question>`. Returns the question
/// text of the FIRST such line, or `None`. Prose around the line is tolerated
/// (models preface anyway); an empty question is not a question.
pub fn parse_question(reply: &str) -> Option<String> {
    reply.lines().find_map(|l| {
        let q = l.trim().strip_prefix("QUESTION:")?.trim();
        (!q.is_empty()).then(|| q.to_string())
    })
}

/// One step of a [`Plan`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanStep {
    /// The step's text as the model wrote it ("model the ground slab").
    pub text: String,
    pub status: StepStatus,
    /// How many command rounds this step has consumed (1 = first try; each
    /// error-driven retry adds one). Bounded by [`MAX_STEP_ATTEMPTS`].
    pub attempts: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepStatus {
    Pending,
    Done,
    Failed,
}

/// A numbered plan the model emitted for a prolonged task, plus its execution
/// cursor. Serializable so it persists in the per-document chat session and an
/// interrupted plan resumes after a cancel or an app restart.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Plan {
    pub steps: Vec<PlanStep>,
    /// Index of the step currently being executed (== `steps.len()` when every
    /// step is done).
    pub current: usize,
}

impl Plan {
    pub fn new(texts: Vec<String>) -> Self {
        Self {
            steps: texts
                .into_iter()
                .map(|text| PlanStep { text, status: StepStatus::Pending, attempts: 0 })
                .collect(),
            current: 0,
        }
    }

    /// The step being executed now, if any remain.
    pub fn current_step(&self) -> Option<&PlanStep> {
        self.steps.get(self.current)
    }

    /// True when every step has been executed successfully.
    pub fn is_complete(&self) -> bool {
        self.current >= self.steps.len()
    }

    /// True when the plan stopped on a failed step (retry budget exhausted).
    pub fn has_failed(&self) -> bool {
        self.steps.iter().any(|s| s.status == StepStatus::Failed)
    }

    /// Mark the current step done and advance the cursor.
    pub fn mark_step_done(&mut self) {
        if let Some(s) = self.steps.get_mut(self.current) {
            s.status = StepStatus::Done;
            self.current += 1;
        }
    }

    /// Record one failed attempt on the current step; returns the new attempt
    /// count so callers can compare against [`MAX_STEP_ATTEMPTS`].
    pub fn note_step_error(&mut self) -> u32 {
        match self.steps.get_mut(self.current) {
            Some(s) => {
                s.attempts += 1;
                s.attempts
            }
            None => 0,
        }
    }

    /// Mark the current step failed (retry budget exhausted). The cursor stays
    /// on the failed step so a transcript shows exactly where the plan stopped.
    pub fn mark_step_failed(&mut self) {
        if let Some(s) = self.steps.get_mut(self.current) {
            s.status = StepStatus::Failed;
        }
    }

    /// Render the plan as a transcript checklist: `[x]` done, `[ ]` pending,
    /// `[!]` failed, with the in-progress step marked by an arrow.
    pub fn checklist(&self) -> String {
        self.steps
            .iter()
            .enumerate()
            .map(|(i, s)| {
                let mark = match s.status {
                    StepStatus::Done => "[x]",
                    StepStatus::Failed => "[!]",
                    StepStatus::Pending => "[ ]",
                };
                let arrow = if i == self.current && s.status == StepStatus::Pending {
                    " ←"
                } else {
                    ""
                };
                format!("{mark} {}. {}{arrow}", i + 1, s.text)
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// Parse a plan message: a `PLAN:` line followed by numbered steps
/// (`1. …` / `2) …`). Returns `None` when there is no `PLAN:` line or no
/// numbered steps under it. Text before `PLAN:` is tolerated; step collection
/// stops at the first non-step line so trailing prose can't inject steps.
pub fn parse_plan(reply: &str) -> Option<Plan> {
    let mut lines = reply.lines().map(str::trim);
    lines.find(|l| *l == "PLAN:" || l.starts_with("PLAN:"))?;
    let mut steps = Vec::new();
    for line in lines {
        if line.is_empty() && steps.is_empty() {
            continue; // blank line(s) between PLAN: and step 1
        }
        let Some(step) = strip_step_number(line) else { break };
        steps.push(step.to_string());
    }
    (!steps.is_empty()).then(|| Plan::new(steps))
}

/// Strip a `N.`/`N)` step prefix, returning the step text.
fn strip_step_number(line: &str) -> Option<&str> {
    let digits = line.chars().take_while(|c| c.is_ascii_digit()).count();
    if digits == 0 {
        return None;
    }
    let rest = line[digits..].strip_prefix(['.', ')'])?.trim();
    (!rest.is_empty()).then_some(rest)
}

/// Decide whether a just-finished chat turn should be given ONE corrective
/// follow-up because the model answered in prose without emitting any `draft`
/// commands for a request that clearly needed drawing/modifying the model.
///
/// This is the fix for the claude-code/sonnet format-non-compliance bug: the
/// model replies "Sheet created with top, front, right, and perspective views…"
/// but never emits the ```draft block, so nothing is drawn. We re-issue a hard
/// instruction to output ONLY the draft commands. Pure + line-oriented so the
/// gating is unit-testable; the caller owns the single-retry cap and the
/// backend scoping.
///
/// Conservative by design — ALL must hold to force a retry:
/// - `had_commands` is false (the turn drew nothing);
/// - the turn was not a clarifying `QUESTION:` and not a `PLAN:` announcement
///   (those are legitimate non-drawing outcomes with their own flows);
/// - no retry has been forced yet this turn (`already_forced` is false);
/// - the user's message reads as an imperative/draw request (see
///   [`looks_like_draw_request`]) rather than genuine Q&A.
pub fn should_force_draft_retry(
    user_message: &str,
    reply: &str,
    had_commands: bool,
    already_forced: bool,
) -> bool {
    if had_commands || already_forced {
        return false;
    }
    // A question or a plan is a legitimate commands-free outcome — never force.
    if parse_question(reply).is_some() || parse_plan(reply).is_some() {
        return false;
    }
    looks_like_draw_request(user_message)
}

/// Heuristic: does the user's message ask to draw or modify the model (as
/// opposed to asking a question / making small talk)? Kept deliberately simple
/// and recall-biased for imperative CAD phrasing, but it still bails on obvious
/// interrogatives so a genuine Q&A turn ("what is a hypar?") is never forced
/// into emitting commands.
pub fn looks_like_draw_request(user_message: &str) -> bool {
    let msg = user_message.trim();
    if msg.is_empty() {
        return false;
    }
    let lower = msg.to_lowercase();

    // Clear interrogatives / help / chit-chat → NOT a draw request. Checked
    // first so "what should I draw?" or "can you explain extrude?" don't force.
    if msg.ends_with('?') {
        return false;
    }
    const QA_PREFIXES: &[&str] = &[
        "what", "why", "how", "who", "when", "where", "which", "explain",
        "describe", "tell me", "is ", "are ", "do ", "does ", "can you explain",
        "could you explain", "help", "list ", "show me the",
    ];
    if QA_PREFIXES.iter().any(|p| lower.starts_with(p)) {
        return false;
    }

    // Imperative CAD verbs / action words anywhere in the message → draw request.
    // These are the words a human uses when they want geometry or view changes.
    const DRAW_WORDS: &[&str] = &[
        "make", "draw", "create", "add", "build", "model", "place", "put",
        "generate", "construct", "extrude", "box", "sphere", "cylinder", "cone",
        "circle", "rect", "polygon", "line", "wall", "slab", "column", "beam",
        "roof", "floor", "stair", "ramp", "sheet", "view", "move", "copy",
        "rotate", "scale", "mirror", "array", "union", "difference", "offset",
        "fillet", "trim", "delete", "erase", "cut", "join", "loft", "sweep",
        "revolve", "pipe", "give me", "set up", "lay out", "sketch",
    ];
    DRAW_WORDS.iter().any(|w| {
        // Word-ish match: allow it at a word boundary so "remake" doesn't count
        // but "make a box" does. Multi-word phrases ("give me") match as-is.
        if w.contains(' ') {
            lower.contains(w)
        } else {
            lower.split(|c: char| !c.is_ascii_alphanumeric()).any(|t| t == *w)
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn question_form_parses() {
        assert_eq!(
            parse_question("QUESTION: Which of the 3 selected objects should grow?").as_deref(),
            Some("Which of the 3 selected objects should grow?")
        );
        // Prose around the line is tolerated (models preface anyway).
        assert_eq!(
            parse_question("The request is ambiguous.\nQUESTION: How tall is the wall?\n")
                .as_deref(),
            Some("How tall is the wall?")
        );
        // Indented form still parses (streamed text may carry whitespace).
        assert_eq!(parse_question("  QUESTION: Units?").as_deref(), Some("Units?"));
    }

    #[test]
    fn non_questions_do_not_parse() {
        assert_eq!(parse_question("Here is a box."), None);
        assert_eq!(parse_question("QUESTION:"), None, "empty question is not a question");
        assert_eq!(parse_question("QUESTION:   "), None);
        // Mid-line mention is chat, not the message form.
        assert_eq!(parse_question("I have a QUESTION: really?"), None);
    }

    #[test]
    fn plan_form_parses_with_numbered_steps() {
        let p = parse_plan(
            "PLAN:\n1. Model the ground slab\n2. Extrude the cores\n3) Cut the courtyard\n",
        )
        .expect("plan parses");
        assert_eq!(p.steps.len(), 3);
        assert_eq!(p.steps[0].text, "Model the ground slab");
        assert_eq!(p.steps[2].text, "Cut the courtyard");
        assert_eq!(p.current, 0);
        assert!(p.steps.iter().all(|s| s.status == StepStatus::Pending));
    }

    #[test]
    fn plan_tolerates_preamble_and_stops_at_trailing_prose() {
        let p = parse_plan(
            "This needs several stages.\nPLAN:\n\n1. First\n2. Second\nI will start now.\n3. Injected",
        )
        .expect("plan parses");
        // Collection stops at the first non-step line — trailing prose can't
        // smuggle extra steps in.
        assert_eq!(p.steps.len(), 2);
    }

    #[test]
    fn non_plans_do_not_parse() {
        assert!(parse_plan("1. just a list\n2. no PLAN header").is_none());
        assert!(parse_plan("PLAN:\nno numbered steps here").is_none());
        assert!(parse_plan("PLAN:").is_none());
        assert!(parse_plan("a box, drawn").is_none());
    }

    #[test]
    fn plan_advances_and_bounds_attempts() {
        let mut p = Plan::new(vec!["a".into(), "b".into()]);
        assert_eq!(p.current_step().unwrap().text, "a");
        p.mark_step_done();
        assert_eq!(p.current, 1);
        assert!(!p.is_complete());
        assert_eq!(p.note_step_error(), 1);
        assert_eq!(p.note_step_error(), 2);
        p.mark_step_failed();
        assert!(p.has_failed());
        assert!(!p.is_complete());
        assert_eq!(p.steps[0].status, StepStatus::Done);
        assert_eq!(p.steps[1].status, StepStatus::Failed);
    }

    #[test]
    fn checklist_renders_status_marks() {
        let mut p = Plan::new(vec!["slab".into(), "cores".into(), "roof".into()]);
        p.mark_step_done();
        p.note_step_error();
        let c = p.checklist();
        assert_eq!(c.lines().count(), 3);
        assert!(c.contains("[x] 1. slab"), "{c}");
        assert!(c.contains("[ ] 2. cores ←"), "{c}");
        assert!(c.contains("[ ] 3. roof"), "{c}");
        p.mark_step_failed();
        assert!(p.checklist().contains("[!] 2. cores"), "{}", p.checklist());
    }

    // ── Forcing-retry gating (claude-code prose-without-commands fix) ────────

    #[test]
    fn prose_turn_on_a_draw_request_forces_one_retry() {
        // THE bug: model said it created the sheet but ran no commands.
        assert!(should_force_draft_retry(
            "make a sheet with four views of a cube",
            "Sheet created with top, front, right, and perspective views of a 5×5×5 cube.",
            /*had_commands*/ false,
            /*already_forced*/ false,
        ));
        // Works for arbitrary draw requests, not just sheets.
        assert!(should_force_draft_retry(
            "draw a 4x4x3 tower",
            "Done — a 4×4×3 tower.",
            false,
            false,
        ));
    }

    #[test]
    fn a_turn_that_ran_commands_is_not_retried() {
        assert!(!should_force_draft_retry(
            "make a box",
            "Here you go.",
            /*had_commands*/ true,
            false,
        ));
    }

    #[test]
    fn a_clarifying_question_turn_is_not_retried() {
        // The clarify flow owns this; forcing would stomp a legitimate question.
        assert!(!should_force_draft_retry(
            "make a tower",
            "QUESTION: How tall should the tower be?",
            false,
            false,
        ));
    }

    #[test]
    fn a_plan_announcement_turn_is_not_retried() {
        assert!(!should_force_draft_retry(
            "model a small office building",
            "PLAN:\n1. Model the slab\n2. Extrude the core",
            false,
            false,
        ));
    }

    #[test]
    fn the_retry_is_capped_at_one() {
        // Once we've already forced a retry this turn, never force again.
        assert!(!should_force_draft_retry(
            "make a sheet of a cube",
            "Created the sheet.",
            false,
            /*already_forced*/ true,
        ));
    }

    #[test]
    fn genuine_qa_turns_are_not_forced() {
        // A plain question with no commands is a normal answer, not a failure.
        for (user, reply) in [
            ("what is a hypar?", "A hyperbolic paraboloid is a doubly-ruled surface."),
            ("how do I extrude a profile?", "Draw a closed profile, then extrude it."),
            ("can you explain the difference command?", "It subtracts one solid from another."),
            ("is Z up in this app?", "Yes, Z is up and the ground plane is z=0."),
        ] {
            assert!(
                !should_force_draft_retry(user, reply, false, false),
                "Q&A turn should not force a retry: {user:?}"
            );
        }
    }

    #[test]
    fn draw_request_detection_is_conservative() {
        // Imperative draw phrasing → yes.
        assert!(looks_like_draw_request("make a sheet with four views of a cube"));
        assert!(looks_like_draw_request("give me two towers 10m apart"));
        assert!(looks_like_draw_request("add a column at the origin"));
        assert!(looks_like_draw_request("top view please")); // "view" is an action word
        // Questions / chit-chat → no.
        assert!(!looks_like_draw_request("what can you do?"));
        assert!(!looks_like_draw_request("how does extrude work"));
        assert!(!looks_like_draw_request("thanks!"));
        assert!(!looks_like_draw_request(""));
        // "remake" must not match on the "make" substring (word-boundary check).
        assert!(!looks_like_draw_request("remake? no"));
    }

    #[test]
    fn plan_serde_roundtrips_for_session_persistence() {
        let mut p = Plan::new(vec!["a".into(), "b".into()]);
        p.mark_step_done();
        p.note_step_error();
        let back: Plan = serde_json::from_str(&serde_json::to_string(&p).unwrap()).unwrap();
        assert_eq!(back, p);
        assert_eq!(back.current, 1);
        assert_eq!(back.steps[1].attempts, 1);
    }
}
