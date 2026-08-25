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

    /// Best-effort fetch of the endpoint's advertised model ids (`GET /models`),
    /// used to enrich a "model not found" turn failure. Never errors — an empty
    /// list just means we couldn't enumerate (the message degrades gracefully).
    async fn list_models(&self) -> Vec<String> {
        let mut request = self.client.get(format!("{}/models", self.base_url));
        if let Some(key) = &self.api_key {
            request = request.bearer_auth(key);
        }
        let Ok(resp) = request.send().await else {
            return Vec::new();
        };
        if !resp.status().is_success() {
            return Vec::new();
        }
        let Ok(body) = resp.json::<Value>().await else {
            return Vec::new();
        };
        body["data"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|m| m["id"].as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default()
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

        let response = request.send().await.map_err(|e| {
            // Connection refused / DNS / TLS: the server isn't reachable. Say so
            // plainly instead of leaking a raw reqwest error — the deck must
            // never fail a turn silently or cryptically.
            DeckError::Stream(format!(
                "can't reach {} — is the runtime/Ollama running? ({e})",
                self.base_url
            ))
        })?;
        if !response.status().is_success() {
            let status = response.status().as_u16();
            let body = response.text().await.unwrap_or_default();
            // A missing model is the single most common silent failure (Ollama
            // 404, vLLM/llama.cpp 400/404). Turn it into an actionable message
            // that lists what the endpoint DOES serve.
            if is_model_missing(status, &body) {
                let available = self.list_models().await;
                return Err(DeckError::Stream(model_not_found_message(
                    &self.model,
                    &self.base_url,
                    &available,
                )));
            }
            return Err(DeckError::Api { status, body });
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

/// Heuristic: does this error status+body indicate the requested model isn't
/// served by the endpoint? Ollama answers 404 with "model ... not found";
/// llama.cpp/vLLM/OpenAI answer 400/404 with a body mentioning the model. Pure
/// so the classification is unit-testable.
fn is_model_missing(status: u16, body: &str) -> bool {
    if status == 404 {
        return true;
    }
    // Some servers use 400 for an unknown model — sniff the body.
    if status == 400 || status == 422 {
        let b = body.to_lowercase();
        return b.contains("model") && (b.contains("not found") || b.contains("does not exist"));
    }
    false
}

/// The actionable "model not found" message, listing what the endpoint serves.
/// Pure so the wording is unit-testable without a live endpoint.
fn model_not_found_message(model: &str, base_url: &str, available: &[String]) -> String {
    if available.is_empty() {
        format!(
            "model '{model}' not found on {base_url} — and no models are listed there. \
             Pull it (e.g. `ollama pull {model}`) or start the right runtime."
        )
    } else {
        format!(
            "model '{model}' not found on {base_url} — available: {}. \
             Pull it or pick one of those.",
            available.join(", ")
        )
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

    // ── model-missing classification (Priority C) ──────────────────────────

    #[test]
    fn ollama_404_is_model_missing() {
        assert!(is_model_missing(
            404,
            r#"{"error":"model 'qwen3' not found, try pulling it first"}"#
        ));
    }

    #[test]
    fn bad_request_mentioning_missing_model_is_model_missing() {
        assert!(is_model_missing(
            400,
            "The model `foo` does not exist or you do not have access"
        ));
        assert!(is_model_missing(422, "model not found"));
    }

    #[test]
    fn generic_400_is_not_model_missing() {
        // A 400 that isn't about a model (e.g. bad params) must NOT be
        // misclassified — it should surface as a plain Api error.
        assert!(!is_model_missing(400, "invalid temperature value"));
        // 500s are server errors, not missing-model.
        assert!(!is_model_missing(500, "model blew up internally"));
        assert!(!is_model_missing(401, "model unauthorized"));
    }

    #[test]
    fn not_found_message_lists_available_models() {
        let msg = model_not_found_message(
            "qwen3",
            "http://localhost:11434/v1",
            &["llama3.2".into(), "phi4".into()],
        );
        assert!(msg.contains("qwen3"), "{msg}");
        assert!(msg.contains("http://localhost:11434/v1"), "{msg}");
        assert!(msg.contains("llama3.2"), "{msg}");
        assert!(msg.contains("phi4"), "{msg}");
    }

    #[test]
    fn not_found_message_handles_empty_list() {
        let msg = model_not_found_message("qwen3", "http://localhost:11434/v1", &[]);
        assert!(msg.contains("qwen3"), "{msg}");
        assert!(msg.contains("no models are listed"), "{msg}");
    }
}
