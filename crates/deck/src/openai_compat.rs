// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

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
    /// Opt-in grammar-constrained decoding (llama.cpp `grammar` param).
    grammar: bool,
    client: reqwest::Client,
}

impl OpenAiCompatDeck {
    pub fn new(config: &DeckConfig) -> Self {
        Self {
            name: config.name.clone(),
            base_url: config.base_url.trim_end_matches('/').to_string(),
            model: config.model.clone(),
            api_key: config.resolved_key(),
            grammar: config.grammar,
            client: reqwest::Client::new(),
        }
    }

    /// Build the JSON request body. Pure (no I/O) so it can be unit-tested
    /// without a live endpoint. When `grammar` is on, a GBNF grammar derived
    /// from the command registry is attached as an extra `grammar` field:
    /// llama.cpp's server honours it, endpoints that don't (OpenAI) ignore it.
    fn build_body(&self, req: &ChatRequest, messages: Value) -> Value {
        let mut body = json!({
            "model": if req.model.is_empty() { &self.model } else { &req.model },
            "messages": messages,
            "max_tokens": req.max_tokens,
            "temperature": req.temperature,
            "stream": true,
        });
        if self.grammar {
            body["grammar"] = Value::String(itsjustcad_commands::gbnf::command_grammar());
        }
        body
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

        let body = self.build_body(&req, Value::Array(messages));
        let mut request = self
            .client
            .post(format!("{}/chat/completions", self.base_url))
            .json(&body);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::DeckKind;

    fn config(grammar: bool) -> DeckConfig {
        DeckConfig {
            name: "local".into(),
            kind: DeckKind::OpenaiCompat,
            base_url: "http://localhost:11434/v1".into(),
            model: "qwen3".into(),
            api_key: None,
            grammar,
        }
    }

    fn req() -> ChatRequest {
        ChatRequest::text("sys".into(), Vec::new(), String::new(), 512, 0.2, None)
    }

    #[test]
    fn body_carries_grammar_when_flag_is_set() {
        let deck = OpenAiCompatDeck::new(&config(true));
        let body = deck.build_body(&req(), Value::Array(vec![]));
        let grammar = body["grammar"]
            .as_str()
            .expect("grammar field present as string");
        // It is the registry-derived GBNF: has the root rule and real verbs.
        assert!(grammar.contains("root      ::="), "not a GBNF grammar: {grammar}");
        assert!(grammar.contains("\"box\""), "grammar missing a known verb");
        assert_eq!(grammar, itsjustcad_commands::gbnf::command_grammar());
    }

    #[test]
    fn body_omits_grammar_when_flag_is_off() {
        let deck = OpenAiCompatDeck::new(&config(false));
        let body = deck.build_body(&req(), Value::Array(vec![]));
        assert!(
            body.get("grammar").is_none(),
            "grammar must be absent when the flag is off: {body}"
        );
    }

    #[test]
    fn serialized_request_json_contains_grammar_string() {
        // End-to-end at the serialization boundary: what actually goes on the wire.
        let deck = OpenAiCompatDeck::new(&config(true));
        let body = deck.build_body(&req(), Value::Array(vec![]));
        let wire = serde_json::to_string(&body).expect("serializes");
        assert!(wire.contains("\"grammar\""), "wire JSON lacks grammar field");
        assert!(wire.contains("```draft"), "wire JSON lacks the draft fence rule");
    }
}
