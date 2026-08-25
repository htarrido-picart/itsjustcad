// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

use async_trait::async_trait;
use eventsource_stream::Eventsource as _;
use futures::StreamExt as _;
use serde_json::{json, Value};
use tokio::sync::mpsc::UnboundedSender;

use crate::config::DeckConfig;
use crate::deck::{ChatRequest, DeckDelta, DeckError, LlmDeck, Role};

pub struct AnthropicDeck {
    name: String,
    base_url: String,
    model: String,
    api_key: Option<String>,
    client: reqwest::Client,
}

impl AnthropicDeck {
    pub fn new(config: &DeckConfig) -> Self {
        Self {
            name: config.name.clone(),
            base_url: config.base_url.trim_end_matches('/').to_string(),
            model: config.model.clone(),
            api_key: config.resolved_key(),
            client: reqwest::Client::new(),
        }
    }

    /// Build the JSON request body. Pure (no I/O) so the web-search gating is
    /// unit-testable without a live endpoint. When `req.web_search` is set, the
    /// Anthropic server-side `web_search` tool is attached; when it is off the
    /// `tools` key is entirely absent (offline/sealed stance).
    fn build_body(&self, req: &ChatRequest) -> Value {
        let messages: Vec<Value> = req
            .messages
            .iter()
            .map(|m| {
                json!({
                    "role": match m.role { Role::User => "user", Role::Assistant => "assistant" },
                    "content": m.content,
                })
            })
            .collect();

        let mut body = json!({
            "model": if req.model.is_empty() { &self.model } else { &req.model },
            "system": req.system,
            "messages": messages,
            "max_tokens": req.max_tokens,
            "temperature": req.temperature,
            "stream": true,
        });
        // OPT-IN web search: only attach the server-side tool when this turn
        // asked for it. Absent otherwise — the request carries no tools.
        if req.web_search {
            body["tools"] = json!([{
                "type": "web_search_20250305",
                "name": "web_search",
            }]);
        }
        body
    }

    async fn stream_inner(
        &self,
        req: ChatRequest,
        tx: &UnboundedSender<DeckDelta>,
    ) -> Result<(), DeckError> {
        let response = self
            .client
            .post(format!("{}/v1/messages", self.base_url))
            .header("x-api-key", self.api_key.clone().unwrap_or_default())
            .header("anthropic-version", "2023-06-01")
            .json(&self.build_body(&req))
            .send()
            .await?;
        if !response.status().is_success() {
            return Err(DeckError::Api {
                status: response.status().as_u16(),
                body: response.text().await.unwrap_or_default(),
            });
        }

        let mut events = response.bytes_stream().eventsource();
        while let Some(event) = events.next().await {
            let event = event.map_err(|e| DeckError::Stream(e.to_string()))?;
            match event.event.as_str() {
                "content_block_delta" => {
                    let value: Value = serde_json::from_str(&event.data)
                        .map_err(|e| DeckError::Stream(e.to_string()))?;
                    if let Some(text) = value["delta"]["text"].as_str()
                        && !text.is_empty()
                    {
                        let _ = tx.send(DeckDelta::Text(text.to_string()));
                    }
                }
                "message_stop" => break,
                "error" => return Err(DeckError::Stream(event.data)),
                _ => {}
            }
        }
        Ok(())
    }
}

#[async_trait]
impl LlmDeck for AnthropicDeck {
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

    fn deck() -> AnthropicDeck {
        AnthropicDeck::new(&DeckConfig {
            name: "claude".into(),
            kind: DeckKind::Anthropic,
            base_url: "https://api.anthropic.com".into(),
            model: "claude-sonnet-4-6".into(),
            api_key: None,
            grammar: false,
        })
    }

    fn req(web_search: bool) -> ChatRequest {
        let mut r = ChatRequest::text("sys".into(), Vec::new(), String::new(), 512, 0.2, None);
        r.web_search = web_search;
        r
    }

    #[test]
    fn web_search_off_attaches_no_tools() {
        // Default sealed stance: the request must carry no `tools` at all.
        let body = deck().build_body(&req(false));
        assert!(
            body.get("tools").is_none(),
            "web search OFF must not attach any tool: {body}"
        );
    }

    #[test]
    fn web_search_on_attaches_server_side_tool() {
        let body = deck().build_body(&req(true));
        let tools = body["tools"].as_array().expect("tools array present");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["name"], "web_search");
        assert_eq!(tools[0]["type"], "web_search_20250305");
    }

    #[test]
    fn serialized_body_only_mentions_web_search_when_enabled() {
        // At the serialization boundary — what actually goes on the wire.
        let off = serde_json::to_string(&deck().build_body(&req(false))).unwrap();
        assert!(!off.contains("web_search"), "off wire leaked web_search: {off}");
        let on = serde_json::to_string(&deck().build_body(&req(true))).unwrap();
        assert!(on.contains("web_search"), "on wire missing web_search: {on}");
    }
}
