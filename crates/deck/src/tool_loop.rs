// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! Our OWN multi-step agentic loop — the FOSS core that drops the closed
//! `claude` CLI from the critical path. A cassette answers one step at a time:
//! it either asks to call tools or returns a final answer. The loop dispatches
//! each requested tool, feeds the results back, and repeats until the cassette
//! is done (or a hard step budget is hit). The CAD substrate commands are the
//! model's tools (dispatched through a [`ToolDispatch`]), so the same loop can
//! draw geometry, run a scoped read, or — when opted in — search the web.
//!
//! This module is transport-agnostic and synchronous over a trait, which keeps
//! the agentic control flow fully unit-testable with a mock cassette (no
//! network, no subprocess). The HTTP/subprocess cassettes remain the streaming
//! path for plain chat; this loop is what lets a cassette act across steps
//! without relying on any provider's built-in agent runner.

use serde::{Deserialize, Serialize};

use crate::agent::{Plan, MAX_STEP_ATTEMPTS};

/// A single tool invocation the cassette wants performed this step. `id`
/// correlates the request with its [`ToolResult`] on the next step; `name` is
/// the tool (e.g. a CAD verb like `box`, or `web_search`); `input` is the raw
/// argument string the dispatcher interprets.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub input: String,
}

/// The outcome of running one [`ToolCall`], fed back to the cassette so it can
/// decide the next step. `is_error` lets the cassette self-correct (the same
/// error-feedback contract the streaming path already relies on).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolResult {
    pub id: String,
    pub output: String,
    pub is_error: bool,
}

/// What a cassette decides on one step of the loop.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StepDecision {
    /// Call these tools; the loop runs them and calls the cassette again with
    /// the results appended.
    CallTools(Vec<ToolCall>),
    /// The turn is complete; this is the assistant's final prose answer.
    Final(String),
    /// The cassette announced a numbered PLAN for a prolonged task (the
    /// `agent::parse_plan` message form). [`run_plan_loop`] adopts it as the
    /// plan state; emitted again mid-run it REPLANS the remaining steps.
    /// [`run_tool_loop`] (plain, plan-less loop) skips it and keeps stepping.
    Plan(Vec<String>),
    /// The cassette asked a clarifying question (the `agent::parse_question`
    /// form) instead of acting. Both loops end the turn immediately — the
    /// user's answer arrives as the next turn.
    Question(String),
}

/// A cassette that can drive the agentic loop one step at a time. Given the
/// tool results produced since its last decision (empty on the first step), it
/// returns the next [`StepDecision`]. Implementors keep their own conversation
/// state; the loop only shuttles tool results in and decisions out.
pub trait AgentCassette {
    /// Decide the next step. `results` are the outcomes of the tools requested
    /// on the previous step (empty on the first call of a turn).
    fn step(&mut self, results: &[ToolResult]) -> StepDecision;
}

/// Runs the tools a cassette requests. The real implementation parses each
/// `ToolCall` into a substrate `Command` and runs it through the `Session`
/// (geometry, scoped reads); `web_search` routes to the opt-in search backend.
pub trait ToolDispatch {
    /// Execute one tool call, returning its result. Must never panic — a failed
    /// tool becomes a `ToolResult { is_error: true }` so the cassette can
    /// recover on the next step.
    fn dispatch(&mut self, call: &ToolCall) -> ToolResult;
}

/// The transcript of one agentic turn: the tool calls made, their results, and
/// the final answer. Handy for the UI (show what ran) and for tests.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LoopOutcome {
    /// Every tool call the cassette made this turn, in order.
    pub calls: Vec<ToolCall>,
    /// The result of each call, index-aligned with `calls`.
    pub results: Vec<ToolResult>,
    /// The cassette's final answer.
    pub answer: String,
    /// True when the loop stopped because it hit `max_steps` rather than a
    /// `Final` decision (a runaway cassette; the partial work still stands).
    pub truncated: bool,
    /// A clarifying question that ended the turn (`StepDecision::Question`).
    pub question: Option<String>,
}

/// Drive `cassette` to completion, dispatching each requested tool through
/// `dispatch`. Stops at the cassette's `Final` decision or after `max_steps`
/// tool-calling rounds (whichever comes first), so a misbehaving cassette can
/// never spin forever. `max_steps` is clamped to at least 1.
///
/// This is the whole agentic control loop, provider-agnostic and side-effecting
/// only through `dispatch` — exactly what lets us own the loop instead of
/// delegating it to a closed CLI's built-in agent.
pub fn run_tool_loop(
    cassette: &mut dyn AgentCassette,
    dispatch: &mut dyn ToolDispatch,
    max_steps: u32,
) -> LoopOutcome {
    let budget = max_steps.max(1);
    let mut outcome = LoopOutcome::default();
    let mut results: Vec<ToolResult> = Vec::new();

    for _ in 0..budget {
        match cassette.step(&results) {
            StepDecision::Final(answer) => {
                outcome.answer = answer;
                return outcome;
            }
            StepDecision::Question(q) => {
                outcome.question = Some(q);
                return outcome;
            }
            // A plan announcement in the plain loop: nothing to execute yet —
            // keep stepping (a plan-aware caller uses `run_plan_loop`).
            StepDecision::Plan(_) => {}
            StepDecision::CallTools(calls) => {
                results = Vec::with_capacity(calls.len());
                for call in &calls {
                    let result = dispatch.dispatch(call);
                    outcome.results.push(result.clone());
                    results.push(result);
                    outcome.calls.push(call.clone());
                }
            }
        }
    }

    // Budget exhausted without a Final: give the cassette one last chance to
    // summarize, but do not run any more tools. If it still asks for tools we
    // mark the turn truncated and return what we have.
    match cassette.step(&results) {
        StepDecision::Final(answer) => outcome.answer = answer,
        StepDecision::Question(q) => outcome.question = Some(q),
        StepDecision::CallTools(_) | StepDecision::Plan(_) => outcome.truncated = true,
    }
    outcome
}

// ── Plan-execute harness ─────────────────────────────────────────────────────

/// The transcript of one plan-execute run. Extends [`LoopOutcome`]'s shape with
/// the plan state, which the caller persists in the chat session so an
/// interrupted (cancelled / crashed) plan resumes on the next run.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PlanOutcome {
    /// The plan as it stands after this run: step statuses, attempt counts and
    /// the cursor. `None` when the cassette never announced a plan.
    pub plan: Option<Plan>,
    /// Every tool call made this run, in order, and its result.
    pub calls: Vec<ToolCall>,
    pub results: Vec<ToolResult>,
    /// The cassette's final answer (the post-verification summary).
    pub answer: String,
    /// A clarifying question that suspended the run awaiting the user.
    pub question: Option<String>,
    /// True when the run stopped because `cancelled()` reported a user cancel.
    /// The plan state is preserved so the run resumes later.
    pub cancelled: bool,
    /// True when the round budget ran out before a `Final`.
    pub truncated: bool,
}

/// Drive a plan-execute run (Claude-Code/DeepSeek-style): the cassette first
/// announces a `PLAN` (unless `resume` already carries one from a previous,
/// interrupted run), then executes it step by step — each round of tool calls
/// belongs to the CURRENT step, whose outcome advances the cursor:
///
/// - every tool in the round succeeded → the step is done, cursor advances;
/// - any tool errored → one attempt is burned; the errors are fed back so the
///   cassette can retry, at most [`MAX_STEP_ATTEMPTS`] times per step before
///   the step is marked failed and the run stops (bounded self-correction);
/// - a mid-run `Plan` decision replans: the remaining steps are replaced.
///
/// The run ends at the cassette's `Final` answer (its post-verification
/// summary), on a `Question` (awaiting the user), on user cancel (checked
/// before every round via `cancelled`, preserving plan state for resume), on a
/// failed step, or after `max_rounds` rounds — whichever comes first.
pub fn run_plan_loop(
    cassette: &mut dyn AgentCassette,
    dispatch: &mut dyn ToolDispatch,
    resume: Option<Plan>,
    max_rounds: u32,
    cancelled: &dyn Fn() -> bool,
) -> PlanOutcome {
    let budget = max_rounds.max(1);
    let mut outcome = PlanOutcome { plan: resume, ..PlanOutcome::default() };
    let mut results: Vec<ToolResult> = Vec::new();

    for _ in 0..budget {
        if cancelled() {
            outcome.cancelled = true;
            return outcome; // plan state preserved → resumable
        }
        match cassette.step(&results) {
            StepDecision::Final(answer) => {
                outcome.answer = answer;
                return outcome;
            }
            StepDecision::Question(q) => {
                outcome.question = Some(q);
                return outcome;
            }
            StepDecision::Plan(texts) => match &mut outcome.plan {
                // First announcement: adopt the plan.
                None => outcome.plan = Some(Plan::new(texts)),
                // Mid-run announcement: REPLAN — completed steps stand, the
                // remaining ones are replaced by the new tail.
                Some(plan) => {
                    plan.steps.truncate(plan.current);
                    plan.steps.extend(Plan::new(texts).steps);
                }
            },
            StepDecision::CallTools(calls) => {
                results = Vec::with_capacity(calls.len());
                let mut any_error = false;
                for call in &calls {
                    let result = dispatch.dispatch(call);
                    any_error |= result.is_error;
                    outcome.results.push(result.clone());
                    results.push(result);
                    outcome.calls.push(call.clone());
                }
                if let Some(plan) = &mut outcome.plan {
                    if any_error {
                        // Burn one attempt; past the budget the step fails and
                        // the run stops — bounded, never a spin.
                        if plan.note_step_error() >= MAX_STEP_ATTEMPTS {
                            plan.mark_step_failed();
                            return outcome;
                        }
                    } else {
                        plan.mark_step_done();
                    }
                }
            }
        }
    }

    // Round budget exhausted: one last chance to summarize, no more tools.
    match cassette.step(&results) {
        StepDecision::Final(answer) => outcome.answer = answer,
        StepDecision::Question(q) => outcome.question = Some(q),
        _ => outcome.truncated = true,
    }
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scripted cassette: a queue of decisions to hand back, one per step.
    /// This mocks a real model deciding to call a tool, then answer.
    struct ScriptedCassette {
        script: std::collections::VecDeque<StepDecision>,
        /// Records the tool results it was fed on each step (to assert the loop
        /// actually threaded results back in).
        seen_results: Vec<Vec<ToolResult>>,
    }

    impl ScriptedCassette {
        fn new(steps: Vec<StepDecision>) -> Self {
            Self {
                script: steps.into_iter().collect(),
                seen_results: Vec::new(),
            }
        }
    }

    impl AgentCassette for ScriptedCassette {
        fn step(&mut self, results: &[ToolResult]) -> StepDecision {
            self.seen_results.push(results.to_vec());
            self.script
                .pop_front()
                .unwrap_or_else(|| StepDecision::Final("(exhausted script)".into()))
        }
    }

    /// A dispatcher that echoes tool inputs, recording what it ran. `fail_on`
    /// names a tool it should report as an error, to exercise the error path.
    struct RecordingDispatch {
        ran: Vec<ToolCall>,
        fail_on: Option<String>,
    }

    impl ToolDispatch for RecordingDispatch {
        fn dispatch(&mut self, call: &ToolCall) -> ToolResult {
            self.ran.push(call.clone());
            let is_error = self.fail_on.as_deref() == Some(call.name.as_str());
            ToolResult {
                id: call.id.clone(),
                output: if is_error {
                    format!("{} failed", call.name)
                } else {
                    format!("ran {} {}", call.name, call.input)
                },
                is_error,
            }
        }
    }

    #[test]
    fn loop_calls_a_tool_then_finishes() {
        // THE core test: a cassette that first asks to draw a box, then — after
        // seeing the tool result — returns a final answer. The loop must run the
        // tool exactly once and end with the final answer.
        let mut cassette = ScriptedCassette::new(vec![
            StepDecision::CallTools(vec![ToolCall {
                id: "c1".into(),
                name: "box".into(),
                input: "0,0,0 5,5,3".into(),
            }]),
            StepDecision::Final("Drew a 5×5×3 box.".into()),
        ]);
        let mut dispatch = RecordingDispatch { ran: Vec::new(), fail_on: None };

        let outcome = run_tool_loop(&mut cassette, &mut dispatch, 8);

        // Tool ran once, with the right args.
        assert_eq!(dispatch.ran.len(), 1);
        assert_eq!(dispatch.ran[0].name, "box");
        assert_eq!(dispatch.ran[0].input, "0,0,0 5,5,3");
        // The loop recorded the call and its result, and the final answer.
        assert_eq!(outcome.calls.len(), 1);
        assert_eq!(outcome.results.len(), 1);
        assert!(!outcome.results[0].is_error);
        assert_eq!(outcome.answer, "Drew a 5×5×3 box.");
        assert!(!outcome.truncated);
        // The cassette was fed the tool result on its second step.
        assert_eq!(cassette.seen_results[0].len(), 0, "first step sees nothing");
        assert_eq!(cassette.seen_results[1].len(), 1, "second step sees the result");
        assert_eq!(cassette.seen_results[1][0].output, "ran box 0,0,0 5,5,3");
    }

    #[test]
    fn loop_threads_error_results_so_cassette_can_recover() {
        // The cassette asks for a bad tool, gets an error, then corrects and
        // finishes — exercising the self-correction contract across steps.
        let mut cassette = ScriptedCassette::new(vec![
            StepDecision::CallTools(vec![ToolCall {
                id: "c1".into(),
                name: "bogus".into(),
                input: "x".into(),
            }]),
            StepDecision::CallTools(vec![ToolCall {
                id: "c2".into(),
                name: "box".into(),
                input: "0,0,0 1,1,1".into(),
            }]),
            StepDecision::Final("Fixed and drew it.".into()),
        ]);
        let mut dispatch = RecordingDispatch {
            ran: Vec::new(),
            fail_on: Some("bogus".into()),
        };

        let outcome = run_tool_loop(&mut cassette, &mut dispatch, 8);

        assert_eq!(outcome.calls.len(), 2);
        assert!(outcome.results[0].is_error, "first tool errored");
        assert!(!outcome.results[1].is_error, "second tool ok");
        assert_eq!(outcome.answer, "Fixed and drew it.");
        // The error result reached the cassette on its second step.
        assert!(cassette.seen_results[1][0].is_error);
    }

    #[test]
    fn multiple_tools_in_one_step_all_run_in_order() {
        let mut cassette = ScriptedCassette::new(vec![
            StepDecision::CallTools(vec![
                ToolCall { id: "a".into(), name: "box".into(), input: "0,0,0 1,1,1".into() },
                ToolCall { id: "b".into(), name: "box".into(), input: "3,0,0 1,1,1".into() },
            ]),
            StepDecision::Final("Two boxes.".into()),
        ]);
        let mut dispatch = RecordingDispatch { ran: Vec::new(), fail_on: None };
        let outcome = run_tool_loop(&mut cassette, &mut dispatch, 8);
        assert_eq!(dispatch.ran.len(), 2);
        assert_eq!(outcome.calls.len(), 2);
        // Both results were fed back on the next step.
        assert_eq!(cassette.seen_results[1].len(), 2);
    }

    #[test]
    fn runaway_cassette_is_truncated_at_the_budget() {
        // A cassette that always asks for another tool must be stopped by the
        // step budget rather than spinning forever.
        struct Greedy;
        impl AgentCassette for Greedy {
            fn step(&mut self, _r: &[ToolResult]) -> StepDecision {
                StepDecision::CallTools(vec![ToolCall {
                    id: "x".into(),
                    name: "box".into(),
                    input: "0,0,0 1,1,1".into(),
                }])
            }
        }
        let mut dispatch = RecordingDispatch { ran: Vec::new(), fail_on: None };
        let outcome = run_tool_loop(&mut Greedy, &mut dispatch, 3);
        assert!(outcome.truncated, "runaway loop must be flagged truncated");
        assert_eq!(dispatch.ran.len(), 3, "exactly the budget's worth of tools ran");
        assert!(outcome.answer.is_empty());
    }

    #[test]
    fn immediate_final_runs_no_tools() {
        let mut cassette = ScriptedCassette::new(vec![StepDecision::Final("Hi.".into())]);
        let mut dispatch = RecordingDispatch { ran: Vec::new(), fail_on: None };
        let outcome = run_tool_loop(&mut cassette, &mut dispatch, 8);
        assert!(dispatch.ran.is_empty());
        assert_eq!(outcome.answer, "Hi.");
        assert!(outcome.calls.is_empty());
    }

    #[test]
    fn question_ends_the_plain_loop_awaiting_the_user() {
        let mut cassette = ScriptedCassette::new(vec![StepDecision::Question(
            "which object?".into(),
        )]);
        let mut dispatch = RecordingDispatch { ran: Vec::new(), fail_on: None };
        let outcome = run_tool_loop(&mut cassette, &mut dispatch, 8);
        assert_eq!(outcome.question.as_deref(), Some("which object?"));
        assert!(dispatch.ran.is_empty(), "a question turn runs no tools");
        assert!(!outcome.truncated);
    }

    // ── plan-execute harness ────────────────────────────────────────────────

    fn call(id: &str, name: &str, input: &str) -> ToolCall {
        ToolCall { id: id.into(), name: name.into(), input: input.into() }
    }

    fn never_cancelled() -> impl Fn() -> bool {
        || false
    }

    #[test]
    fn plan_loop_announces_then_advances_steps_on_success() {
        // THE core plan test: PLAN(2 steps) → step-1 tools ok → step-2 tools ok
        // → Final summary. Both steps end Done, the answer is the summary.
        let mut cassette = ScriptedCassette::new(vec![
            StepDecision::Plan(vec!["model the slab".into(), "extrude the core".into()]),
            StepDecision::CallTools(vec![call("a", "box", "0,0,0 10,10,0.3")]),
            StepDecision::CallTools(vec![call("b", "box", "4,4,0 2,2,9")]),
            StepDecision::Final("Slab + core drawn; 2 objects verified.".into()),
        ]);
        let mut dispatch = RecordingDispatch { ran: Vec::new(), fail_on: None };
        let outcome =
            run_plan_loop(&mut cassette, &mut dispatch, None, 16, &never_cancelled());
        let plan = outcome.plan.expect("plan adopted");
        assert!(plan.is_complete());
        assert!(plan.steps.iter().all(|s| s.status == crate::agent::StepStatus::Done));
        assert_eq!(outcome.answer, "Slab + core drawn; 2 objects verified.");
        assert_eq!(dispatch.ran.len(), 2);
        assert!(!outcome.truncated && !outcome.cancelled);
    }

    #[test]
    fn plan_loop_bounds_retries_then_fails_the_step() {
        // Step 1 keeps erroring: the loop feeds errors back MAX_STEP_ATTEMPTS
        // times, then marks the step Failed and stops — never a spin.
        let mut script = vec![StepDecision::Plan(vec!["impossible step".into()])];
        for i in 0..10 {
            script.push(StepDecision::CallTools(vec![call(
                &format!("c{i}"),
                "bogus",
                "x",
            )]));
        }
        let mut cassette = ScriptedCassette::new(script);
        let mut dispatch =
            RecordingDispatch { ran: Vec::new(), fail_on: Some("bogus".into()) };
        let outcome =
            run_plan_loop(&mut cassette, &mut dispatch, None, 32, &never_cancelled());
        let plan = outcome.plan.expect("plan adopted");
        assert!(plan.has_failed());
        assert_eq!(plan.steps[0].attempts, MAX_STEP_ATTEMPTS);
        assert_eq!(
            dispatch.ran.len(),
            MAX_STEP_ATTEMPTS as usize,
            "exactly the retry budget's worth of attempts ran"
        );
        // Error results were threaded back for the retries.
        assert!(cassette.seen_results[2][0].is_error);
    }

    #[test]
    fn plan_loop_resumes_from_a_persisted_plan() {
        // A previous run finished step 1 then was interrupted; the persisted
        // plan round-trips serde (chat-session persistence) and the resumed run
        // continues at step 2 WITHOUT the cassette re-announcing a plan.
        let mut prior = Plan::new(vec!["slab".into(), "core".into()]);
        prior.mark_step_done();
        let revived: Plan =
            serde_json::from_str(&serde_json::to_string(&prior).unwrap()).unwrap();
        assert_eq!(revived.current, 1);

        let mut cassette = ScriptedCassette::new(vec![
            StepDecision::CallTools(vec![call("a", "box", "4,4,0 2,2,9")]),
            StepDecision::Final("Core drawn.".into()),
        ]);
        let mut dispatch = RecordingDispatch { ran: Vec::new(), fail_on: None };
        let outcome = run_plan_loop(
            &mut cassette,
            &mut dispatch,
            Some(revived),
            16,
            &never_cancelled(),
        );
        let plan = outcome.plan.expect("plan kept");
        assert!(plan.is_complete(), "resumed run completed the remaining step");
        assert_eq!(plan.steps[0].status, crate::agent::StepStatus::Done);
        assert_eq!(plan.steps[1].status, crate::agent::StepStatus::Done);
        assert_eq!(dispatch.ran.len(), 1, "only the remaining step's tools ran");
        assert_eq!(outcome.answer, "Core drawn.");
    }

    #[test]
    fn plan_loop_user_cancel_preserves_plan_state() {
        // Cancel flips true after the first round of tools: the loop must stop
        // BEFORE the next round, flag cancelled, and keep the plan resumable.
        use std::cell::Cell;
        let rounds = Cell::new(0u32);
        let cancelled = || rounds.get() >= 2; // checked per round: plan, tools, ⏹
        let mut cassette = ScriptedCassette::new(vec![
            StepDecision::Plan(vec!["slab".into(), "core".into()]),
            StepDecision::CallTools(vec![call("a", "box", "0,0,0 10,10,0.3")]),
            StepDecision::CallTools(vec![call("b", "box", "4,4,0 2,2,9")]),
            StepDecision::Final("never reached".into()),
        ]);
        struct CountingDispatch<'a>(RecordingDispatch, &'a Cell<u32>);
        impl ToolDispatch for CountingDispatch<'_> {
            fn dispatch(&mut self, c: &ToolCall) -> ToolResult {
                self.1.set(self.1.get() + 1);
                self.0.dispatch(c)
            }
        }
        let mut dispatch =
            CountingDispatch(RecordingDispatch { ran: Vec::new(), fail_on: None }, &rounds);
        // Round counting via dispatched tools: after the first step's tool the
        // cancel predicate reports true (rounds>=2 counts plan+tool rounds).
        rounds.set(1); // the plan-announcement round has "happened"
        let outcome =
            run_plan_loop(&mut cassette, &mut dispatch, None, 16, &cancelled);
        assert!(outcome.cancelled, "user cancel must be honoured mid-plan");
        assert!(outcome.answer.is_empty());
        let plan = outcome.plan.expect("plan preserved for resume");
        assert!(!plan.is_complete(), "unfinished steps remain");
        assert_eq!(plan.steps[0].status, crate::agent::StepStatus::Done);
        assert_eq!(plan.steps[1].status, crate::agent::StepStatus::Pending);
        assert_eq!(dispatch.0.ran.len(), 1, "no tools ran after the cancel");
    }

    #[test]
    fn plan_loop_midrun_replan_replaces_remaining_steps() {
        // After step 1 the cassette replans the tail; done steps stand.
        let mut cassette = ScriptedCassette::new(vec![
            StepDecision::Plan(vec!["slab".into(), "wrong step".into()]),
            StepDecision::CallTools(vec![call("a", "box", "0,0,0 10,10,0.3")]),
            StepDecision::Plan(vec!["better step".into(), "roof".into()]),
            StepDecision::CallTools(vec![call("b", "box", "0,0,3 10,10,0.2")]),
            StepDecision::CallTools(vec![call("c", "box", "0,0,6 10,10,0.2")]),
            StepDecision::Final("done".into()),
        ]);
        let mut dispatch = RecordingDispatch { ran: Vec::new(), fail_on: None };
        let outcome =
            run_plan_loop(&mut cassette, &mut dispatch, None, 16, &never_cancelled());
        let plan = outcome.plan.expect("plan kept");
        assert_eq!(plan.steps.len(), 3, "1 done + 2 replanned");
        assert_eq!(plan.steps[0].text, "slab");
        assert_eq!(plan.steps[1].text, "better step");
        assert_eq!(plan.steps[2].text, "roof");
        assert!(plan.is_complete());
        assert_eq!(outcome.answer, "done");
    }

    #[test]
    fn plan_loop_round_budget_bounds_a_runaway_plan() {
        struct GreedyPlanner(bool);
        impl AgentCassette for GreedyPlanner {
            fn step(&mut self, _r: &[ToolResult]) -> StepDecision {
                if !self.0 {
                    self.0 = true;
                    return StepDecision::Plan(vec!["forever".into(); 3]);
                }
                StepDecision::CallTools(vec![ToolCall {
                    id: "x".into(),
                    name: "box".into(),
                    input: "0,0,0 1,1,1".into(),
                }])
            }
        }
        let mut dispatch = RecordingDispatch { ran: Vec::new(), fail_on: None };
        let outcome = run_plan_loop(
            &mut GreedyPlanner(false),
            &mut dispatch,
            None,
            4,
            &never_cancelled(),
        );
        assert!(outcome.truncated, "runaway plan must hit the round budget");
        assert!(dispatch.ran.len() <= 4);
    }
}
