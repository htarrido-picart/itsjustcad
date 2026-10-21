// SPDX-License-Identifier: AGPL-3.0-only
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
        Self::with_client(config, reqwest::Client::new())
    }

    /// Like [`Self::new`] with a caller-built HTTP client (e.g. the FFI's
    /// public-only client, see [`crate::public_only_client`]).
    pub fn with_client(config: &DeckConfig, client: reqwest::Client) -> Self {
        Self {
            name: config.name.clone(),
            base_url: config.base_url.trim_end_matches('/').to_string(),
            model: config.model.clone(),
            api_key: config.resolved_key(),
            client,
        }
    }

    /// Build the JSON request body. Pure (no I/O) so the web-search gating is
    /// unit-testable without a live endpoint. When `req.web_search` is set, the
    /// Anthropic server-side `web_search` tool is attached; when it is off the
    /// `tools` key is entirely absent (offline/sealed stance).
    fn build_body(&self, req: &ChatRequest) -> Value {
        // Images (when present) attach to the LAST user message only; every
        // other message keeps a plain-string `content`. With no images this
        // yields the exact pre-vision body (byte-for-byte), since every message
        // takes the string branch.
        let last_user = if req.images.is_empty() {
            None
        } else {
            req.messages
                .iter()
                .rposition(|m| matches!(m.role, Role::User))
        };
        let messages: Vec<Value> = req
            .messages
            .iter()
            .enumerate()
            .map(|(i, m)| {
                let role = match m.role {
                    Role::User => "user",
                    Role::Assistant => "assistant",
                };
                if Some(i) == last_user {
                    // Content-block array: a text block, then one image block
                    // per image (Anthropic base64 `image` source).
                    let mut blocks = vec![json!({"type": "text", "text": m.content})];
                    for img in &req.images {
                        blocks.push(json!({
                            "type": "image",
                            "source": {
                                "type": "base64",
                                "media_type": img.mime,
                                "data": img.data_base64,
                            },
                        }));
                    }
                    json!({ "role": role, "content": blocks })
                } else {
                    json!({ "role": role, "content": m.content })
                }
            })
            .collect();

        // NOTE: `temperature` is intentionally omitted — newer models (e.g.
        // Opus 4.8) reject it with "`temperature` is deprecated for this model",
        // and the API defaults it sensibly when absent. The app exposes no
        // temperature control, so there's nothing to forward.
        let mut body = json!({
            "model": if req.model.is_empty() { &self.model } else { &req.model },
            "system": req.system,
            "messages": messages,
            "max_tokens": req.max_tokens,
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
        let mut usage = (0u64, 0u64);
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
                "message_start" | "message_delta" => {
                    if let Ok(value) = serde_json::from_str::<Value>(&event.data) {
                        accumulate_usage(&value, &mut usage);
                    }
                }
                "message_stop" => break,
                "error" => return Err(DeckError::Stream(event.data)),
                _ => {}
            }
        }
        if usage != (0, 0) {
            let _ = tx.send(DeckDelta::Usage {
                input_tokens: usage.0,
                output_tokens: usage.1,
            });
        }
        Ok(())
    }
}

/// Fold one `message_start` / `message_delta` event's usage into `(input,
/// output)`. Input counts include prompt-cache reads and writes; output is
/// cumulative in `message_delta`, so the latest value wins.
fn accumulate_usage(value: &Value, usage: &mut (u64, u64)) {
    let u = if value["message"]["usage"].is_object() {
        &value["message"]["usage"]
    } else {
        &value["usage"]
    };
    let input: u64 = ["input_tokens", "cache_read_input_tokens", "cache_creation_input_tokens"]
        .iter()
        .filter_map(|k| u[*k].as_u64())
        .sum();
    if input > 0 {
        usage.0 = input;
    }
    if let Some(out) = u["output_tokens"].as_u64() {
        usage.1 = usage.1.max(out);
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
            terse: None,
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
    fn terse_token_cap_reaches_the_wire_body() {
        // Terse mode's hard cap rides the anthropic body's `max_tokens` field.
        let (system, cap) = crate::prompt::terse_adjusted("sys".into(), 4096, true);
        let r = ChatRequest::text(system, Vec::new(), String::new(), cap, 0.2, None);
        let body = deck().build_body(&r);
        assert_eq!(body["max_tokens"], crate::prompt::TERSE_MAX_TOKENS);
        assert!(
            body["system"]
                .as_str()
                .unwrap()
                .contains("## Response style (terse mode)"),
            "terse style section must ride the system prompt"
        );
    }

    fn user(content: &str) -> crate::deck::ChatMessage {
        crate::deck::ChatMessage {
            role: Role::User,
            content: content.into(),
        }
    }
    fn assistant(content: &str) -> crate::deck::ChatMessage {
        crate::deck::ChatMessage {
            role: Role::Assistant,
            content: content.into(),
        }
    }

    #[test]
    fn no_images_is_byte_identical_to_text_path() {
        // The vision change must not perturb the text-only wire body at all.
        let msgs = vec![user("hi"), assistant("hey"), user("look")];
        let text = ChatRequest::text("sys".into(), msgs.clone(), String::new(), 512, 0.2, None);
        let old = serde_json::to_string(&deck().build_body(&text)).unwrap();
        // An explicit empty-images request must serialize identically.
        let empty = text.with_images(Vec::new());
        let new = serde_json::to_string(&deck().build_body(&empty)).unwrap();
        assert_eq!(old, new, "empty images must be byte-identical to text path");
    }

    #[test]
    fn images_attach_as_block_array_on_last_user_message() {
        use crate::deck::ImagePart;
        let msgs = vec![user("first"), assistant("reply"), user("last")];
        let req =
            ChatRequest::text("sys".into(), msgs, String::new(), 512, 0.2, None).with_images(vec![
                ImagePart {
                    mime: "image/png".into(),
                    data_base64: "AAAA".into(),
                },
            ]);
        let body = deck().build_body(&req);
        let m = body["messages"].as_array().unwrap();
        // Earlier messages stay plain strings.
        assert!(m[0]["content"].is_string());
        assert!(m[1]["content"].is_string());
        // Last user message became a content-block array: text + image.
        let blocks = m[2]["content"]
            .as_array()
            .expect("block array on last user msg");
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0]["type"], "text");
        assert_eq!(blocks[0]["text"], "last");
        assert_eq!(blocks[1]["type"], "image");
        assert_eq!(blocks[1]["source"]["type"], "base64");
        assert_eq!(blocks[1]["source"]["media_type"], "image/png");
        assert_eq!(blocks[1]["source"]["data"], "AAAA");
    }

    #[test]
    fn serialized_body_only_mentions_web_search_when_enabled() {
        // At the serialization boundary — what actually goes on the wire.
        let off = serde_json::to_string(&deck().build_body(&req(false))).unwrap();
        assert!(
            !off.contains("web_search"),
            "off wire leaked web_search: {off}"
        );
        let on = serde_json::to_string(&deck().build_body(&req(true))).unwrap();
        assert!(
            on.contains("web_search"),
            "on wire missing web_search: {on}"
        );
    }

    #[test]
    fn usage_folds_message_start_and_delta() {
        let mut u = (0, 0);
        accumulate_usage(
            &serde_json::json!({"type":"message_start","message":{"usage":{"input_tokens":1200,"cache_read_input_tokens":300,"output_tokens":1}}}),
            &mut u,
        );
        accumulate_usage(&serde_json::json!({"type":"message_delta","usage":{"output_tokens":87}}), &mut u);
        assert_eq!(u, (1500, 87));
    }
}
