// SPDX-License-Identifier: AGPL-3.0-only
// Copyright © 2026 Hector Tarrido-Picart

//! LLM deck: cassette-player providers behind one trait. Ollama, Kimi, Claude,
//! any OpenAI-compatible endpoint. The deck emits plain-text commands in
//! ```draft fenced blocks — the same language humans type — extracted by a
//! streaming state machine and executed by the app against the one Session.

mod agent;
mod anthropic;
mod claude_code;
mod claude_spawn;
mod config;
mod deck;
mod digest;
mod extract;
mod feedback;
mod ffi_prompt;
mod openai_compat;
mod probe;
mod prompt;
mod render_deck;
mod retrieve;
mod selectors;
mod tool_loop;
mod which;

pub use agent::{
    looks_like_draw_request, parse_plan, parse_question, should_force_draft_retry, Plan,
    PlanStep, StepStatus, MAX_STEP_ATTEMPTS,
};
pub use claude_code::{scoped_allowed_tools, turn_allowed_tools};
pub use config::{is_local_url, CloudProvider, DeckConfig, DeckKind, DecksFile};
pub use deck::{
    make_deck, ChatMessage, ChatRequest, DeckDelta, DeckError, ImagePart, LlmDeck, Role,
};
pub use digest::{digest, digest_filtered, geometry_kind, DigestFilter};
pub use extract::{Extractor, ExtractEvent};
pub use feedback::{
    google_form_url, parse_line as parse_feedback_line, platform, FeedbackCategory,
    FeedbackDraft, FieldKind, GoogleForm, TemplateField, BUG_TEMPLATE,
    EXAMPLE_LINE as FEEDBACK_EXAMPLE_LINE, FREQ_OPTIONS, GOOGLE_FORM, GOOGLE_FORM_URL_PREFIX,
    IDEA_TEMPLATE,
};
pub use ffi_prompt::{ffi_system_prompt, FFI_SELECTOR_HELP, FFI_VIEW_HELP};
pub use probe::{probe, warm_model, ProbeInfo, WarmOutcome};
pub use prompt::{
    compact_command_catalog, compact_system_prompt, language_directive, select_prompt,
    system_prompt, terse_adjusted, with_language, PromptChoice, CLARIFY_HELP, PLAN_HELP,
    TERSE_MAX_TOKENS, TERSE_STYLE_HELP, UI_VERB_HELP, VIEW_VERB_HELP,
};
pub use render_deck::{
    local_sd_readiness, make_render_deck, probe_url, render_config_path, resolve_sd_binary,
    resolve_sd_binary_in, sd_search_dirs, test_connection, Automatic1111RenderDeck,
    CloudRenderDeck, ComfyRenderDeck, ControlImages, LocalSdRenderDeck, MockRenderDeck,
    RenderConfig, RenderDeck, RenderDeckError, RenderDecksFile, RenderKind, RenderRequest,
    RenderedImage, UnconfiguredRenderDeck, NO_BACKEND_MESSAGE, NO_SD_BINARY_MESSAGE,
};
pub use retrieve::{estimate_tokens, retrieve, retrieved_block, Match, CORE_VERBS, GATED_VERBS};
pub use selectors::normalize_selectors;
pub use tool_loop::{
    run_plan_loop, run_tool_loop, AgentCassette, LoopOutcome, PlanOutcome, StepDecision,
    ToolCall, ToolDispatch, ToolResult,
};
pub use which::{augmented_path_env, resolve_claude_binary};

/// The user's config dir: [`home_config_dir`] of the home directory.
#[cfg(not(test))]
pub fn config_dir() -> Option<std::path::PathBuf> {
    dirs::home_dir().map(|h| home_config_dir(&h))
}

/// Tests never read or write the developer's real config: they get a throwaway
/// dir under the system temp dir instead, one per test process.
#[cfg(test)]
pub fn config_dir() -> Option<std::path::PathBuf> {
    Some(std::env::temp_dir().join(format!("ijc_test_config_{}", std::process::id())))
}

/// `~/.config/itsjustcad` under `home`, on every platform (CLI-tool convention,
/// XDG default on Linux; never a macOS-only dir — GitHub issue #26).
pub fn home_config_dir(home: &std::path::Path) -> std::path::PathBuf {
    home.join(".config").join("itsjustcad")
}
