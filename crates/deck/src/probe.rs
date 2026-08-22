use serde_json::Value;

use crate::config::{DeckConfig, DeckKind};

#[derive(Clone, Debug)]
pub struct ProbeInfo {
    pub detail: String,
    /// Models available on the endpoint (drives the model picker).
    pub models: Vec<String>,
}

/// Check whether a cassette is actually usable before enabling the deck UI:
/// endpoint reachable, key present/valid, model available.
pub async fn probe(config: &DeckConfig) -> Result<ProbeInfo, String> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .map_err(|e| e.to_string())?;
    let base = config.base_url.trim_end_matches('/');

    match config.kind {
        DeckKind::OpenaiCompat => {
            let mut request = client.get(format!("{base}/models"));
            if let Some(key) = config.resolved_key() {
                request = request.bearer_auth(key);
            }
            let response = request.send().await.map_err(|_| {
                format!(
                    "cannot reach {base} — is the server running? (for Ollama: `ollama serve`)"
                )
            })?;
            if !response.status().is_success() {
                return Err(format!(
                    "{base} answered {} — check the API key",
                    response.status()
                ));
            }
            let body: Value = response.json().await.map_err(|e| e.to_string())?;
            let models: Vec<String> = body["data"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|m| m["id"].as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default();
            if !models.is_empty() && !models.iter().any(|m| *m == config.model) {
                return Err(format!(
                    "model '{}' not found on {base}. Available: {}",
                    config.model,
                    models.join(", ")
                ));
            }
            Ok(ProbeInfo {
                detail: format!("ready — {} @ {base}", config.model),
                models,
            })
        }
        DeckKind::Anthropic => {
            let Some(key) = config.resolved_key() else {
                let hint = config.api_key.as_deref().unwrap_or("api_key");
                return Err(format!(
                    "no API key ({hint} not set) — add it to ~/.config/mydrafter/decks.json or export the env var"
                ));
            };
            let response = client
                .get(format!("{base}/v1/models"))
                .header("x-api-key", key)
                .header("anthropic-version", "2023-06-01")
                .send()
                .await
                .map_err(|_| format!("cannot reach {base}"))?;
            if response.status().as_u16() == 401 {
                return Err("API key rejected (401) — check the key".to_string());
            }
            if !response.status().is_success() {
                return Err(format!("{base} answered {}", response.status()));
            }
            let body: Value = response.json().await.map_err(|e| e.to_string())?;
            let models: Vec<String> = body["data"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|m| m["id"].as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default();
            Ok(ProbeInfo {
                detail: format!("ready — {} @ {base}", config.model),
                models,
            })
        }
    }
}
