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
