use async_trait::async_trait;
use eventsource_stream::Eventsource as _;
use futures::StreamExt as _;
use serde_json::{json, Value};
use tokio::sync::mpsc::UnboundedSender;

use crate::config::DeckConfig;
use crate::deck::{ChatRequest, DeckDelta, DeckError, LlmDeck, Role};

/// One adapter covers Ollama, Kimi/Moonshot, DeepSeek, vLLM, OpenAI, ...
pub struct OpenAiCompatDeck {
    name: String,
    base_url: String,
    model: String,
    api_key: Option<String>,
    client: reqwest::Client,
}

impl OpenAiCompatDeck {
    pub fn new(config: &DeckConfig) -> Self {
        Self {
            name: config.name.clone(),
            base_url: config.base_url.trim_end_matches('/').to_string(),
            model: config.model.clone(),
            api_key: config.resolved_key(),
            client: reqwest::Client::new(),
        }
    }

    async fn stream_inner(
        &self,
        req: ChatRequest,
        tx: &UnboundedSender<DeckDelta>,
    ) -> Result<(), DeckError> {
        let mut messages = vec![json!({"role": "system", "content": req.system})];
        messages.extend(req.messages.iter().map(|m| {
            json!({
                "role": match m.role { Role::User => "user", Role::Assistant => "assistant" },
                "content": m.content,
            })
        }));

        let mut request = self
            .client
            .post(format!("{}/chat/completions", self.base_url))
            .json(&json!({
                "model": if req.model.is_empty() { &self.model } else { &req.model },
                "messages": messages,
                "max_tokens": req.max_tokens,
                "temperature": req.temperature,
                "stream": true,
            }));
        if let Some(key) = &self.api_key {
            request = request.bearer_auth(key);
        }

        let response = request.send().await?;
        if !response.status().is_success() {
            return Err(DeckError::Api {
                status: response.status().as_u16(),
                body: response.text().await.unwrap_or_default(),
            });
        }

        let mut events = response.bytes_stream().eventsource();
        while let Some(event) = events.next().await {
            let event = event.map_err(|e| DeckError::Stream(e.to_string()))?;
            if event.data == "[DONE]" {
                break;
            }
            let value: Value =
                serde_json::from_str(&event.data).map_err(|e| DeckError::Stream(e.to_string()))?;
            if let Some(text) = value["choices"][0]["delta"]["content"].as_str()
                && !text.is_empty()
            {
                let _ = tx.send(DeckDelta::Text(text.to_string()));
            }
        }
        Ok(())
    }
}

#[async_trait]
impl LlmDeck for OpenAiCompatDeck {
    fn name(&self) -> String {
        self.name.clone()
    }

    async fn stream_chat(&self, req: ChatRequest, tx: UnboundedSender<DeckDelta>) {
        match self.stream_inner(req, &tx).await {
            Ok(()) => {
                let _ = tx.send(DeckDelta::Done);
            }
            Err(e) => {
                let _ = tx.send(DeckDelta::Error(e.to_string()));
            }
        }
    }
}
