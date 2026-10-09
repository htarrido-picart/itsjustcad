// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

use serde::{Deserialize, Serialize};

/// Write `contents` to `path` with mode 0600 on unix so API keys stored in
/// config files are not world-readable on multi-user hosts (M-3 / L-1).
pub(crate) fn write_private(path: &std::path::Path, contents: &str) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)?;
        f.write_all(contents.as_bytes())
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, contents)
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DeckKind {
    /// Ollama, Kimi/Moonshot, DeepSeek, vLLM, OpenAI — one adapter covers all.
    OpenaiCompat,
    Anthropic,
    /// Local `claude` CLI subprocess — Claude subscription auth, no API key.
    ClaudeCode,
}

/// The two first-class cloud providers the "LLM ▸ API Keys…" dialog manages.
/// Each maps to one canonical cassette in `decks.json` (created if absent), so
/// the dialog can set a key / pick a model without the user hand-editing JSON.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CloudProvider {
    /// Claude via the Anthropic API (`x-api-key` auth).
    Anthropic,
    /// ChatGPT via the OpenAI API (served through the OpenAI-compatible adapter).
    OpenAi,
}

impl CloudProvider {
    /// Canonical cassette name written to `decks.json`.
    pub fn cassette_name(self) -> &'static str {
        match self {
            CloudProvider::Anthropic => "claude",
            CloudProvider::OpenAi => "openai",
        }
    }

    /// Adapter kind for this provider.
    pub fn deck_kind(self) -> DeckKind {
        match self {
            CloudProvider::Anthropic => DeckKind::Anthropic,
            CloudProvider::OpenAi => DeckKind::OpenaiCompat,
        }
    }

    /// Canonical API base URL.
    pub fn base_url(self) -> &'static str {
        match self {
            CloudProvider::Anthropic => "https://api.anthropic.com",
            CloudProvider::OpenAi => "https://api.openai.com/v1",
        }
    }

    /// Environment variable the key is read from when the user prefers env
    /// indirection over a stored literal.
    pub fn env_var(self) -> &'static str {
        match self {
            CloudProvider::Anthropic => "ANTHROPIC_API_KEY",
            CloudProvider::OpenAi => "OPENAI_API_KEY",
        }
    }

    /// Fallback model when creating a brand-new cassette (before a probe picks).
    pub fn default_model(self) -> &'static str {
        match self {
            CloudProvider::Anthropic => "claude-sonnet-4-6",
            // Sol = flagship-class at mid price; a sensible default for a CAD
            // assistant. The live /models probe lets the user switch.
            CloudProvider::OpenAi => "gpt-6-sol",
        }
    }

    /// Human-facing label for the dialog row.
    pub fn label(self) -> &'static str {
        match self {
            CloudProvider::Anthropic => "Anthropic (Claude)",
            CloudProvider::OpenAi => "OpenAI (ChatGPT)",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DeckConfig {
    pub name: String,
    pub kind: DeckKind,
    /// e.g. "http://localhost:11434/v1" or "https://api.moonshot.ai/v1" or
    /// "https://api.anthropic.com".
    pub base_url: String,
    pub model: String,
    /// Literal key, or "env:VAR_NAME" to read from the environment.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    /// Opt-in grammar-constrained decoding. When true, the `openai_compat`
    /// cassette attaches a GBNF grammar (derived from the command registry) to
    /// each request so local models can only emit real verbs inside real
    /// ```draft fences. Sent as an extra `grammar` JSON field — llama.cpp's
    /// server honours it; endpoints that don't (OpenAI proper) ignore it.
    /// Leave false for cloud endpoints. Other cassettes ignore this flag.
    #[serde(default)]
    pub grammar: bool,
    /// Terse mode override. `None` = default ON for every cassette — local
    /// models answer faster on fewer tokens, cloud models cost less per turn.
    /// `Some(_)` is the user's explicit choice from the LLM menu toggle.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terse: Option<bool>,
}

impl DeckConfig {
    /// Whether terse mode (caveman-style response budget: style rules + a hard
    /// per-turn max-token cap) applies to this cassette. Explicit user override
    /// wins; otherwise ON for all cassettes, local and cloud alike.
    pub fn terse_enabled(&self) -> bool {
        self.terse.unwrap_or(true)
    }

    /// Whether this cassette can analyze an attached image. Claude (subscription
    /// CLI) and Anthropic API are multimodal; OpenAI-compatible endpoints only
    /// when the selected model is a known vision model. Local grammar cassettes
    /// (llama.cpp text models) are treated as text-only. The image-attach button
    /// in the chat is gated on this so we never send an image to a blind model.
    pub fn supports_vision(&self) -> bool {
        match self.kind {
            DeckKind::ClaudeCode | DeckKind::Anthropic => true,
            DeckKind::OpenaiCompat => {
                // Grammar-constrained local text models can't see images.
                if self.grammar {
                    return false;
                }
                // The real OpenAI cloud only serves frontier models, which are
                // all multimodal (gpt-4o, gpt-4.1, gpt-5.x "Sol", "astra", …).
                // Trust the endpoint rather than chasing each new codename in a
                // substring list that always lags releases — the user picked the
                // model from a live /models probe against this very endpoint.
                if self.base_url.starts_with("https://api.openai.com") {
                    return true;
                }
                // Other OpenAI-compatible endpoints (local servers, gateways)
                // host arbitrary models, so keep allow-listing known vision names.
                let m = self.model.to_ascii_lowercase();
                m.contains("vl")
                    || m.contains("vision")
                    || m.contains("-v")
                    || m.contains("4o") // gpt-4o, gpt-4o-mini, chatgpt-4o
                    || m.contains("gpt-4-turbo")
                    || m.contains("gpt-4.1") // gpt-4.1 family is multimodal
                    || m.contains("llava")
                    || m.contains("gemini")
                    || m.contains("pixtral")
                    || m.contains("qwen2.5-vl")
            }
        }
    }

    pub fn resolved_key(&self) -> Option<String> {
        match self.api_key.as_deref() {
            Some(k) if k.starts_with("env:") => std::env::var(&k[4..]).ok(),
            Some(k) => Some(k.to_string()),
            None => None,
        }
    }
}

/// Returns `true` when `url` resolves to localhost (empty, "localhost", or
/// "127.*" / "[::1]" / "0.0.0.0" host). Used by the local-only filter.
pub fn is_local_url(url: &str) -> bool {
    if url.is_empty() {
        return true; // ClaudeCode (subprocess) has no base_url — always local.
    }
    // Strip scheme and path — we only care about the host part.
    let host_part = url
        .trim_start_matches("http://")
        .trim_start_matches("https://")
        .split('/')
        .next()
        .unwrap_or(url);
    // Strip port.
    let host = if let Some(bracket_end) = host_part.find(']') {
        // IPv6 literal like [::1]:11434
        &host_part[..=bracket_end]
    } else {
        host_part.split(':').next().unwrap_or(host_part)
    };
    matches!(
        host,
        "localhost" | "127.0.0.1" | "0.0.0.0" | "[::1]" | "::1"
    ) || host.starts_with("127.")
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DecksFile {
    pub decks: Vec<DeckConfig>,
    #[serde(default)]
    pub active: usize,
    /// When true: only cassettes with local base_urls are shown; any attempt
    /// to send to a non-local endpoint is blocked. Persisted in decks.json.
    #[serde(default)]
    pub local_only: bool,
}

impl Default for DecksFile {
    fn default() -> Self {
        Self {
            decks: vec![
                DeckConfig {
                    name: "claude-code".into(),
                    kind: DeckKind::ClaudeCode,
                    base_url: String::new(),
                    model: "sonnet".into(),
                    api_key: None,
                    grammar: false,
                    terse: None,
                },
                DeckConfig {
                    name: "ollama".into(),
                    kind: DeckKind::OpenaiCompat,
                    base_url: "http://localhost:11434/v1".into(),
                    model: "qwen3".into(),
                    api_key: None,
                    // Local model — constrain decoding on by default.
                    grammar: true,
                    terse: None,
                },
                DeckConfig {
                    name: "claude".into(),
                    kind: DeckKind::Anthropic,
                    base_url: "https://api.anthropic.com".into(),
                    model: "claude-sonnet-4-6".into(),
                    api_key: Some("env:ANTHROPIC_API_KEY".into()),
                    grammar: false,
                    terse: None,
                },
                DeckConfig {
                    name: "kimi".into(),
                    kind: DeckKind::OpenaiCompat,
                    base_url: "https://api.moonshot.ai/v1".into(),
                    model: "kimi-k2-0905-preview".into(),
                    api_key: Some("env:MOONSHOT_API_KEY".into()),
                    grammar: false,
                    terse: None,
                },
            ],
            active: 0,
            local_only: false,
        }
    }
}

pub fn config_path() -> Option<std::path::PathBuf> {
    // ~/.config/itsjustcad on every platform — CLI-tool convention, greppable.
    Some(dirs::home_dir()?.join(".config").join("itsjustcad").join("decks.json"))
}

impl DecksFile {
    pub fn load_or_default() -> Self {
        config_path()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) {
        if let Some(path) = config_path() {
            let _ = std::fs::create_dir_all(path.parent().expect("has parent"));
            // M-3: 0600 so literal API keys are not world-readable on multi-user hosts.
            let _ = write_private(&path, &serde_json::to_string_pretty(self).expect("serializes"));
        }
    }

    /// Cassettes visible under the current `local_only` setting.
    pub fn visible_decks(&self) -> impl Iterator<Item = (usize, &DeckConfig)> {
        self.decks.iter().enumerate().filter(|(_, d)| {
            !self.local_only || is_local_url(&d.base_url)
        })
    }

    /// Index of the canonical cassette for a cloud provider, if present. Prefers
    /// the canonical name; falls back to kind+endpoint so a hand-edited cassette
    /// under a different name is still recognised (OpenAI is matched on the
    /// `api.openai.com` host so Kimi/Moonshot and other OpenAI-compat endpoints
    /// are never mistaken for it).
    pub fn provider_index(&self, provider: CloudProvider) -> Option<usize> {
        self.decks
            .iter()
            .position(|d| d.name == provider.cassette_name())
            .or_else(|| {
                self.decks.iter().position(|d| {
                    d.kind == provider.deck_kind()
                        && (provider != CloudProvider::OpenAi
                            || d.base_url.starts_with("https://api.openai.com"))
                })
            })
    }

    /// Create or update the canonical cassette for a cloud provider.
    ///
    /// - `api_key`: `Some("env:VAR")` or `Some("<literal>")` to set the key;
    ///   `None` leaves an existing cassette's key untouched (used when the user
    ///   only changes the model).
    /// - `model`: `Some(_)` sets the selected model; `None` keeps the current
    ///   (or the provider default for a freshly created cassette).
    ///
    /// Kind and base_url are re-pinned to the canonical values so a drifted file
    /// is healed. Does NOT persist — call [`DecksFile::save`] after. Returns the
    /// cassette index.
    pub fn set_cloud_provider(
        &mut self,
        provider: CloudProvider,
        api_key: Option<String>,
        model: Option<String>,
    ) -> usize {
        match self.provider_index(provider) {
            Some(i) => {
                if api_key.is_some() {
                    self.decks[i].api_key = api_key;
                }
                if let Some(m) = model {
                    self.decks[i].model = m;
                }
                self.decks[i].kind = provider.deck_kind();
                self.decks[i].base_url = provider.base_url().to_string();
                i
            }
            None => {
                self.decks.push(DeckConfig {
                    name: provider.cassette_name().to_string(),
                    kind: provider.deck_kind(),
                    base_url: provider.base_url().to_string(),
                    model: model.unwrap_or_else(|| provider.default_model().to_string()),
                    api_key,
                    grammar: false,
                    terse: None,
                });
                self.decks.len() - 1
            }
        }
    }

    /// Returns `Err` when `local_only` is on and the active cassette is remote.
    pub fn check_local_only(&self) -> Result<(), String> {
        if !self.local_only {
            return Ok(());
        }
        let config = self.decks.get(self.active).ok_or_else(|| "no deck configured".to_string())?;
        if is_local_url(&config.base_url) {
            Ok(())
        } else {
            Err(format!(
                "local-only mode is on — '{}' ({}) is a remote endpoint",
                config.name, config.base_url
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_url_classification() {
        assert!(is_local_url(""));
        assert!(is_local_url("http://localhost:11434/v1"));
        assert!(is_local_url("http://127.0.0.1:8080"));
        assert!(is_local_url("http://[::1]:5000"));
        assert!(is_local_url("http://0.0.0.0"));
        assert!(is_local_url("http://127.255.0.1/api"));
        assert!(!is_local_url("https://api.anthropic.com"));
        assert!(!is_local_url("https://api.moonshot.ai/v1"));
        assert!(!is_local_url("http://192.168.1.10:11434/v1"));
    }

    #[test]
    fn local_only_filter_hides_remote_decks() {
        let df = DecksFile { local_only: true, ..DecksFile::default() };
        let visible: Vec<&str> = df.visible_decks().map(|(_, d)| d.name.as_str()).collect();
        // claude-code (empty base_url) and ollama (localhost) are local; claude + kimi are not.
        assert!(visible.contains(&"claude-code"), "{visible:?}");
        assert!(visible.contains(&"ollama"), "{visible:?}");
        assert!(!visible.contains(&"claude"), "{visible:?}");
        assert!(!visible.contains(&"kimi"), "{visible:?}");
    }

    #[test]
    fn check_local_only_blocks_remote_active() {
        let mut df = DecksFile { local_only: true, ..DecksFile::default() };
        // Default active is 0 (claude-code, local) → OK.
        assert!(df.check_local_only().is_ok());
        // Switch active to "claude" (index 2, remote) → Err.
        df.active = 2;
        assert!(df.check_local_only().is_err());
    }

    #[test]
    fn vision_capability_gating() {
        let mk = |kind, model: &str, grammar| DeckConfig {
            name: "x".into(),
            kind,
            base_url: String::new(),
            model: model.into(),
            api_key: None,
            grammar,
            terse: None,
        };
        // Claude (CLI + API) is always multimodal.
        assert!(mk(DeckKind::ClaudeCode, "sonnet", false).supports_vision());
        assert!(mk(DeckKind::Anthropic, "claude-sonnet-4-6", false).supports_vision());
        // OpenAI-compat: vision only for known vision models, never for a
        // grammar-constrained local text model.
        assert!(mk(DeckKind::OpenaiCompat, "gpt-4o", false).supports_vision());
        assert!(mk(DeckKind::OpenaiCompat, "qwen2.5-vl-7b", false).supports_vision());
        assert!(!mk(DeckKind::OpenaiCompat, "qwen3", true).supports_vision());
        assert!(!mk(DeckKind::OpenaiCompat, "llama3", false).supports_vision());
    }

    #[test]
    fn terse_defaults_on_for_all_cassettes() {
        let mk = |kind, base_url: &str| DeckConfig {
            name: "x".into(),
            kind,
            base_url: base_url.into(),
            model: "m".into(),
            api_key: None,
            grammar: false,
            terse: None,
        };
        // ON by default everywhere: local (faster inference) AND cloud (cheaper
        // turns — user 2026-09-02: "i want caveman for cloud as well").
        assert!(mk(DeckKind::OpenaiCompat, "http://localhost:11434/v1").terse_enabled());
        assert!(mk(DeckKind::OpenaiCompat, "http://127.0.0.1:8080").terse_enabled());
        assert!(mk(DeckKind::Anthropic, "https://api.anthropic.com").terse_enabled());
        assert!(mk(DeckKind::ClaudeCode, "").terse_enabled());
        assert!(mk(DeckKind::OpenaiCompat, "https://api.moonshot.ai/v1").terse_enabled());
        // Explicit override wins in both directions.
        let mut c = mk(DeckKind::Anthropic, "https://api.anthropic.com");
        c.terse = Some(false);
        assert!(!c.terse_enabled());
        let mut c = mk(DeckKind::OpenaiCompat, "http://localhost:11434/v1");
        c.terse = Some(false);
        assert!(!c.terse_enabled());
    }

    #[test]
    fn terse_field_serde_defaults_none_and_roundtrips() {
        // Old decks.json without the field → None (kind-based default applies).
        let json = r#"{"name":"o","kind":"openai_compat","base_url":"http://localhost:11434/v1","model":"qwen3"}"#;
        let c: DeckConfig = serde_json::from_str(json).unwrap();
        assert_eq!(c.terse, None);
        assert!(c.terse_enabled());
        // Explicit value survives a save/load roundtrip.
        let mut c = c;
        c.terse = Some(false);
        let back: DeckConfig =
            serde_json::from_str(&serde_json::to_string(&c).unwrap()).unwrap();
        assert_eq!(back.terse, Some(false));
    }

    #[test]
    fn provider_index_matches_default_cassettes() {
        let df = DecksFile::default();
        // Default file ships a "claude" (Anthropic) cassette.
        let ai = df.provider_index(CloudProvider::Anthropic);
        assert_eq!(ai.map(|i| df.decks[i].name.as_str()), Some("claude"));
        // No OpenAI cassette in the default (kimi is OpenAI-compat but on
        // moonshot.ai, which must NOT be matched as OpenAI).
        assert_eq!(df.provider_index(CloudProvider::OpenAi), None);
    }

    #[test]
    fn set_cloud_provider_creates_then_updates() {
        let mut df = DecksFile { decks: vec![], active: 0, local_only: false };
        // Create: paste a literal OpenAI key.
        let i = df.set_cloud_provider(
            CloudProvider::OpenAi,
            Some("sk-test".into()),
            None,
        );
        assert_eq!(df.decks.len(), 1);
        assert_eq!(df.decks[i].name, "openai");
        assert_eq!(df.decks[i].kind, DeckKind::OpenaiCompat);
        assert_eq!(df.decks[i].base_url, "https://api.openai.com/v1");
        assert_eq!(df.decks[i].api_key.as_deref(), Some("sk-test"));
        // Default model used when none supplied.
        assert_eq!(df.decks[i].model, "gpt-6-sol");

        // Update model only — key is left untouched (api_key = None).
        let j = df.set_cloud_provider(CloudProvider::OpenAi, None, Some("gpt-4.1".into()));
        assert_eq!(j, i, "updates the same cassette, no duplicate");
        assert_eq!(df.decks.len(), 1);
        assert_eq!(df.decks[i].model, "gpt-4.1");
        assert_eq!(df.decks[i].api_key.as_deref(), Some("sk-test"));

        // Switch to env indirection.
        df.set_cloud_provider(
            CloudProvider::OpenAi,
            Some("env:OPENAI_API_KEY".into()),
            None,
        );
        assert_eq!(df.decks[i].api_key.as_deref(), Some("env:OPENAI_API_KEY"));
    }

    #[test]
    fn set_cloud_provider_heals_drifted_endpoint() {
        // A cassette named "claude" that drifted to the wrong kind/url.
        let mut df = DecksFile {
            decks: vec![DeckConfig {
                name: "claude".into(),
                kind: DeckKind::OpenaiCompat,
                base_url: "http://wrong".into(),
                model: "x".into(),
                api_key: None,
                grammar: false,
                terse: None,
            }],
            active: 0,
            local_only: false,
        };
        df.set_cloud_provider(CloudProvider::Anthropic, Some("k".into()), None);
        assert_eq!(df.decks.len(), 1, "matched by name, not duplicated");
        assert_eq!(df.decks[0].kind, DeckKind::Anthropic);
        assert_eq!(df.decks[0].base_url, "https://api.anthropic.com");
    }

    #[test]
    fn openai_class_models_support_vision() {
        let mk = |model: &str| DeckConfig {
            name: "x".into(),
            kind: DeckKind::OpenaiCompat,
            base_url: "https://api.openai.com/v1".into(),
            model: model.into(),
            api_key: None,
            grammar: false,
            terse: None,
        };
        assert!(mk("gpt-4o").supports_vision());
        assert!(mk("gpt-4o-mini").supports_vision());
        assert!(mk("gpt-4.1").supports_vision());
        assert!(mk("gpt-4-turbo").supports_vision());
        // Unknown future models on the real OpenAI cloud are trusted as
        // multimodal (endpoint-based, not name-based) — codenames we can't
        // predict (e.g. "gpt-5.6-sol", "astra") must still allow image attach.
        assert!(mk("gpt-5.6-sol").supports_vision());
        assert!(mk("astra").supports_vision());

        // But an arbitrary OpenAI-compatible endpoint (e.g. a local gateway)
        // still name-gates: an unknown text model there is treated as blind.
        let local = |model: &str| DeckConfig {
            name: "x".into(),
            kind: DeckKind::OpenaiCompat,
            base_url: "http://localhost:8080/v1".into(),
            model: model.into(),
            api_key: None,
            grammar: false,
            terse: None,
        };
        assert!(!local("some-text-model").supports_vision());
        assert!(local("qwen2.5-vl-7b").supports_vision());
    }

    #[test]
    fn local_only_field_serde_defaults_false() {
        // Files without the field must deserialise with local_only = false.
        let json = r#"{"decks": [], "active": 0}"#;
        let df: DecksFile = serde_json::from_str(json).unwrap();
        assert!(!df.local_only);
    }
}
