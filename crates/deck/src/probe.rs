use serde_json::Value;

use crate::config::{DeckConfig, DeckKind};

#[derive(Clone, Debug)]
pub struct ProbeInfo {
    pub detail: String,
    /// Models available on the endpoint (drives the model picker).
    pub models: Vec<String>,
}

/// Ollama root URL (its native API lives beside the /v1 OpenAI-compat shim).
fn ollama_root(base_url: &str) -> String {
    base_url
        .trim_end_matches('/')
        .trim_end_matches("/v1")
        .to_string()
}

/// Preload the model into memory (Ollama `keep_alive`), so the first prompt
/// doesn't silently pay a 30-60s cold-load tax. No-op for non-Ollama
/// endpoints (cloud APIs have no load phase).
pub async fn warm_model(config: &DeckConfig) -> Result<WarmOutcome, String> {
    if config.kind != DeckKind::OpenaiCompat {
        return Ok(WarmOutcome::NotApplicable);
    }
    let root = ollama_root(&config.base_url);
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(300))
        .build()
        .map_err(|e| e.to_string())?;

    // Already resident? (Ollama /api/ps)
    if let Ok(response) = client.get(format!("{root}/api/ps")).send().await
        && response.status().is_success()
        && let Ok(body) = response.json::<Value>().await
    {
        let loaded = body["models"]
            .as_array()
            .is_some_and(|a| a.iter().any(|m| m["name"].as_str() == Some(&config.model)));
        if loaded {
            return Ok(WarmOutcome::Warm);
        }
    } else {
        // /api/ps missing — not an Ollama server; nothing to warm.
        return Ok(WarmOutcome::NotApplicable);
    }

    // Empty prompt + keep_alive loads the model and pins it for 30 minutes.
    let response = client
        .post(format!("{root}/api/generate"))
        .json(&serde_json::json!({ "model": config.model, "keep_alive": "30m" }))
        .send()
        .await
        .map_err(|e| format!("warm-up failed: {e}"))?;
    if !response.status().is_success() {
        return Err(format!("warm-up failed: {}", response.status()));
    }
    Ok(WarmOutcome::Warm)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WarmOutcome {
    Warm,
    NotApplicable,
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
        DeckKind::ClaudeCode => {
            let output = tokio::process::Command::new("claude")
                .arg("--version")
                .output()
                .await
                .map_err(|_| {
                    "claude CLI not found — install Claude Code (https://claude.com/claude-code)"
                        .to_string()
                })?;
            if !output.status.success() {
                return Err("claude CLI errored on --version".to_string());
            }
            let version = String::from_utf8_lossy(&output.stdout).trim().to_string();
            Ok(ProbeInfo {
                detail: format!("ready — {} via {version} (subscription)", config.model),
                models: vec!["sonnet".into(), "opus".into(), "haiku".into()],
            })
        }
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
            if !models.is_empty() && !models.contains(&config.model) {
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
