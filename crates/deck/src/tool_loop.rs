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
        StepDecision::CallTools(_) => outcome.truncated = true,
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
}
