// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! LLM deck: cassette-player providers behind one trait. Ollama, Kimi, Claude,
//! any OpenAI-compatible endpoint. The deck emits plain-text commands in
//! ```draft fenced blocks — the same language humans type — extracted by a
//! streaming state machine and executed by the app against the one Session.

mod anthropic;
mod claude_code;
mod claude_spawn;
mod config;
mod deck;
mod digest;
mod extract;
mod openai_compat;
mod probe;
mod prompt;
mod render_deck;
mod tool_loop;
mod which;

pub use claude_code::{scoped_allowed_tools, turn_allowed_tools};
pub use config::{is_local_url, DeckConfig, DeckKind, DecksFile};
pub use deck::{make_deck, ChatMessage, ChatRequest, DeckDelta, DeckError, LlmDeck, Role};
pub use digest::digest;
pub use extract::{Extractor, ExtractEvent};
pub use probe::{probe, warm_model, ProbeInfo, WarmOutcome};
pub use prompt::{system_prompt, UI_VERB_HELP, VIEW_VERB_HELP};
pub use render_deck::{
    make_render_deck, render_config_path, Automatic1111RenderDeck, CloudRenderDeck,
    ComfyRenderDeck, ControlImages, MockRenderDeck, RenderConfig, RenderDeck, RenderDeckError,
    RenderDecksFile, RenderKind, RenderRequest, RenderedImage, UnconfiguredRenderDeck,
    NO_BACKEND_MESSAGE,
};
pub use tool_loop::{
    run_tool_loop, AgentCassette, LoopOutcome, StepDecision, ToolCall, ToolDispatch, ToolResult,
};
pub use which::{augmented_path_env, resolve_claude_binary};
