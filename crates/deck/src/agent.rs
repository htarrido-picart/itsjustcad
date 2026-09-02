// SPDX-License-Identifier: AGPL-3.0-or-later
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
