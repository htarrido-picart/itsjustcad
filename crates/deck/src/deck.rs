// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc::UnboundedSender;

use crate::config::{DeckConfig, DeckKind};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    User,
    Assistant,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: Role,
    pub content: String,
}

#[derive(Clone, Debug)]
pub struct ChatRequest {
    pub system: String,
    pub messages: Vec<ChatMessage>,
    pub model: String,
    pub max_tokens: u32,
    pub temperature: f32,
    /// Provider-side conversation handle (claude-code sessions). When set,
    /// the adapter may send only the newest message instead of the full
    /// transcript. HTTP adapters ignore it.
    pub session_id: Option<String>,
    /// Tools the model may use this turn (claude-code cassette only). Empty =
    /// none, keeping the deck a pure text substrate. HTTP adapters ignore it.
    ///
    /// SECURITY (H-1): this is NEVER a bare `Read`. A vision critique sets
    /// `vision_shot_path` instead, from which the claude-code adapter derives a
    /// *path-scoped* `Read(<that one file>)` specifier — the model can open the
    /// screenshot and nothing else (no `decks.json` key exfiltration, no
    /// arbitrary file read via an attacker-controlled scene name in the prompt).
    pub allowed_tools: Vec<String>,
    /// The single screenshot a vision-critique turn is allowed to open
    /// (claude-code cassette only). When set, the adapter grants a Read scoped
    /// to exactly this path and nothing else. `None` = no file access.
    pub vision_shot_path: Option<String>,
    /// Agentic turn budget (claude-code cassette only). 1 for a plain reply; a
    /// tool-using turn (Read a screenshot, then answer) needs 2+. HTTP
    /// adapters ignore it.
    pub max_turns: u32,
    /// Opt-in web search for this turn. Default `false` (offline/sealed stance).
    /// When `true`:
    /// - the anthropic cassette attaches Anthropic's server-side `web_search`
    ///   tool to the request;
    /// - the claude-code cassette adds `WebSearch`/`WebFetch` to `--allowed-tools`.
    ///
    /// Off keeps the tool absent from the request entirely. Other cassettes
    /// (local grammar) ignore it.
    pub web_search: bool,
}

impl ChatRequest {
    /// A plain text turn: no tools, single agentic step. Keeps the tool/turn
    /// defaults in one place so the substrate stays text-only unless a turn
    /// opts in.
    pub fn text(
        system: String,
        messages: Vec<ChatMessage>,
        model: String,
        max_tokens: u32,
        temperature: f32,
        session_id: Option<String>,
    ) -> Self {
        Self {
            system,
            messages,
            model,
            max_tokens,
            temperature,
            session_id,
            allowed_tools: Vec::new(),
            vision_shot_path: None,
            max_turns: 1,
            web_search: false,
        }
    }
}

/// Streaming events from a deck to the UI.
#[derive(Debug)]
pub enum DeckDelta {
    Text(String),
    /// Provider-side session handle to reuse on the next turn.
    Session(String),
    Done,
    Error(String),
}

#[derive(Debug, thiserror::Error)]
pub enum DeckError {
    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("api error {status}: {body}")]
    Api { status: u16, body: String },
    #[error("stream error: {0}")]
    Stream(String),
}

/// One cassette. The app never knows which brain is loaded.
#[async_trait]
pub trait LlmDeck: Send + Sync {
    fn name(&self) -> String;
    /// Stream a chat completion, pushing deltas as they arrive. Always ends by
    /// sending `Done` or `Error` (also on failure paths).
    async fn stream_chat(&self, req: ChatRequest, tx: UnboundedSender<DeckDelta>);
}

pub fn make_deck(config: &DeckConfig) -> Box<dyn LlmDeck> {
    match config.kind {
        DeckKind::OpenaiCompat => Box::new(crate::openai_compat::OpenAiCompatDeck::new(config)),
        DeckKind::Anthropic => Box::new(crate::anthropic::AnthropicDeck::new(config)),
        DeckKind::ClaudeCode => Box::new(crate::claude_code::ClaudeCodeDeck::new(config)),
    }
}
