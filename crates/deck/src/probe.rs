// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

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

/// Whether the configured model is served by the endpoint, given the model ids
/// the endpoint listed. Tolerant of provider suffixes so a configured alias
/// still matches the listed concrete id:
/// - Ollama tags: `qwen3` matches `qwen3:latest`.
/// - Anthropic dated ids: `claude-sonnet-4-6` matches `claude-sonnet-4-6-20250929`.
/// - Version pins: `model` matches `model@q4`.
///
/// An EMPTY list means the endpoint didn't enumerate models (some OpenAI-compat
/// servers don't) — treated as available, the turn itself will surface a bad
/// model. A bare prefix without a separator (`qwen3` vs `qwen30b`) is NOT a
/// match. Pure — the mismatch rule the probe enforces, unit-tested below.
pub fn model_available(configured: &str, available: &[String]) -> bool {
    if available.is_empty() {
        return true;
    }
    available.iter().any(|m| {
        m == configured
            || m.strip_prefix(configured)
                .is_some_and(|rest| rest.starts_with([':', '-', '@']))
    })
}

/// The human message for a configured-model-not-served mismatch: names the
/// model, the endpoint, and what IS available so the fix is obvious.
fn mismatch_error(configured: &str, base: &str, available: &[String]) -> String {
    format!(
        "model '{configured}' not found on {base}. Available: {}",
        available.join(", ")
    )
}

/// Curate the OpenAI cloud `/models` list down to the chat + reasoning families,
/// newest-first, dropping embeddings/tts/audio/image/moderation/etc. `current`
/// is always kept so the active selection stays visible even if it's an older
/// model that would otherwise be filtered out.
fn curate_openai_models(models: &[String], current: &str) -> Vec<String> {
    let mut out: Vec<String> = models
        .iter()
        .filter(|m| is_openai_chat_model(m) || m.as_str() == current)
        .cloned()
        .collect();
    out.sort_by(|a, b| openai_rank(a).cmp(&openai_rank(b)).then_with(|| a.cmp(b)));
    out.dedup();
    out
}

/// Is `id` an OpenAI chat/reasoning model (vs an embedding/tts/image/etc model)?
fn is_openai_chat_model(id: &str) -> bool {
    let m = id.to_ascii_lowercase();
    const DENY: &[&str] = &[
        "embedding", "tts", "whisper", "audio", "realtime", "dall-e", "image",
        "moderation", "search", "transcribe",
    ];
    if DENY.iter().any(|d| m.contains(d)) {
        return false;
    }
    if m.starts_with("gpt-3") {
        return false; // legacy 3.x
    }
    m.starts_with("gpt-")
        || m.starts_with("o1")
        || m.starts_with("o3")
        || m.starts_with("o4")
        || m == "chat-latest"
}

/// Newest-first ordering rank for an OpenAI chat model (lower = newer).
fn openai_rank(id: &str) -> u8 {
    let m = id.to_ascii_lowercase();
    if m.starts_with("gpt-6") {
        0
    } else if m.starts_with("gpt-5") || m == "chat-latest" {
        1
    } else if m.starts_with("gpt-4.1") {
        2
    } else if m.starts_with("gpt-4o") {
        3
    } else if m.starts_with("o4") {
        4
    } else if m.starts_with("o3") {
        5
    } else if m.starts_with("o1") {
        6
    } else {
        7
    }
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
            // A Finder-launched .app has a stripped PATH without /usr/local/bin
            // etc., so a bare `claude` is invisible even when installed. Resolve
            // the absolute path from the well-known install locations first.
            let bin = crate::which::resolve_claude_binary().ok_or_else(|| {
                "claude CLI not found — install Claude Code (https://claude.com/claude-code)"
                    .to_string()
            })?;
            let output = tokio::process::Command::new(&bin)
                .arg("--version")
                .env("PATH", crate::which::augmented_path_env())
                .output()
                .await
                .map_err(|e| {
                    format!(
                        "claude CLI at {} failed to run: {e} — reinstall Claude Code \
                         (https://claude.com/claude-code)",
                        bin.display()
                    )
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
            if !model_available(&config.model, &models) {
                return Err(mismatch_error(&config.model, base, &models));
            }
            // The real OpenAI cloud returns 100+ ids (embeddings, tts, audio,
            // image, …). Curate the PICKER list down to the chat/reasoning
            // families, newest-first. Other OpenAI-compatible endpoints (Ollama,
            // gateways) return their own short lists, so leave those untouched.
            let models = if base.contains("api.openai.com") {
                curate_openai_models(&models, &config.model)
            } else {
                models
            };
            Ok(ProbeInfo {
                detail: format!("ready — {} @ {base}", config.model),
                models,
            })
        }
        DeckKind::Anthropic => {
            let Some(key) = config.resolved_key() else {
                let hint = config.api_key.as_deref().unwrap_or("api_key");
                return Err(format!(
                    "no API key ({hint} not set) — add it to ~/.config/itsjustcad/decks.json or export the env var"
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
            // Same mismatch gate as OpenAI-compat: a configured model the API
            // doesn't serve used to fail silently at send time; surface it here.
            if !model_available(&config.model, &models) {
                return Err(mismatch_error(&config.model, base, &models));
            }
            Ok(ProbeInfo {
                detail: format!("ready — {} @ {base}", config.model),
                models,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn list(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn curate_openai_filters_and_orders_newest_first() {
        let raw = list(&[
            "gpt-4o",
            "text-embedding-3-large",
            "gpt-6-sol",
            "whisper-1",
            "gpt-5.6-sol",
            "dall-e-3",
            "gpt-6-astra",
            "gpt-4.1",
            "tts-1",
            "o3",
            "gpt-3.5-turbo",
            "omni-moderation-latest",
            "gpt-4o-realtime-preview",
        ]);
        let out = curate_openai_models(&raw, "gpt-6-sol");
        // Non-chat models are gone; gpt-3.5 (legacy) + realtime dropped.
        for bad in [
            "text-embedding-3-large",
            "whisper-1",
            "dall-e-3",
            "tts-1",
            "gpt-3.5-turbo",
            "omni-moderation-latest",
            "gpt-4o-realtime-preview",
        ] {
            assert!(!out.contains(&bad.to_string()), "{bad} should be filtered");
        }
        // gpt-6 family is first, then gpt-5.x, then 4.1, then 4o, then o3.
        assert_eq!(out.first().map(String::as_str), Some("gpt-6-astra")); // alpha within gpt-6
        assert!(out.contains(&"gpt-6-sol".to_string()));
        let pos = |id: &str| out.iter().position(|m| m == id).unwrap();
        assert!(pos("gpt-6-sol") < pos("gpt-5.6-sol"));
        assert!(pos("gpt-5.6-sol") < pos("gpt-4.1"));
        assert!(pos("gpt-4.1") < pos("gpt-4o"));
        assert!(pos("gpt-4o") < pos("o3"));
    }

    #[test]
    fn curate_openai_keeps_current_selection_even_if_legacy() {
        // A configured legacy model that would normally be filtered is retained.
        let raw = list(&["gpt-6-sol", "gpt-3.5-turbo"]);
        let out = curate_openai_models(&raw, "gpt-3.5-turbo");
        assert!(out.contains(&"gpt-3.5-turbo".to_string()));
    }

    // ── model_available: the probe's mismatch rule ─────────────────────────

    #[test]
    fn exact_match_is_available() {
        assert!(model_available("qwen3", &list(&["llama3", "qwen3"])));
    }

    #[test]
    fn ollama_tag_suffix_matches() {
        // Configured alias vs the concrete tag Ollama lists.
        assert!(model_available("qwen3", &list(&["qwen3:latest"])));
        assert!(model_available("qwen3", &list(&["qwen3:8b-q4"])));
    }

    #[test]
    fn anthropic_dated_id_matches_alias() {
        assert!(model_available(
            "claude-sonnet-4-6",
            &list(&["claude-sonnet-4-6-20250929", "claude-opus-4-2"])
        ));
    }

    #[test]
    fn version_pin_suffix_matches() {
        assert!(model_available("m", &list(&["m@q4"])));
    }

    #[test]
    fn bare_prefix_without_separator_is_not_a_match() {
        // "qwen3" must NOT match "qwen30b" — that is a different model.
        assert!(!model_available("qwen3", &list(&["qwen30b"])));
    }

    #[test]
    fn absent_model_is_unavailable() {
        assert!(!model_available("mistral", &list(&["qwen3:latest", "llama3"])));
    }

    #[test]
    fn empty_list_is_permissive() {
        // Endpoints that don't enumerate models can't be mismatch-checked.
        assert!(model_available("anything", &[]));
    }

    #[test]
    fn mismatch_error_names_model_endpoint_and_alternatives() {
        let msg = mismatch_error("mistral", "http://localhost:8080/v1", &list(&["qwen3", "llama3"]));
        assert!(msg.contains("mistral"), "{msg}");
        assert!(msg.contains("http://localhost:8080/v1"), "{msg}");
        assert!(msg.contains("qwen3, llama3"), "{msg}");
    }
}
