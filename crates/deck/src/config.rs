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
}

impl DeckConfig {
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
                },
                DeckConfig {
                    name: "ollama".into(),
                    kind: DeckKind::OpenaiCompat,
                    base_url: "http://localhost:11434/v1".into(),
                    model: "qwen3".into(),
                    api_key: None,
                    // Local model — constrain decoding on by default.
                    grammar: true,
                },
                DeckConfig {
                    name: "claude".into(),
                    kind: DeckKind::Anthropic,
                    base_url: "https://api.anthropic.com".into(),
                    model: "claude-sonnet-4-6".into(),
                    api_key: Some("env:ANTHROPIC_API_KEY".into()),
                    grammar: false,
                },
                DeckConfig {
                    name: "kimi".into(),
                    kind: DeckKind::OpenaiCompat,
                    base_url: "https://api.moonshot.ai/v1".into(),
                    model: "kimi-k2-0905-preview".into(),
                    api_key: Some("env:MOONSHOT_API_KEY".into()),
                    grammar: false,
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
        let mut df = DecksFile::default();
        df.local_only = true;
        let visible: Vec<&str> = df.visible_decks().map(|(_, d)| d.name.as_str()).collect();
        // claude-code (empty base_url) and ollama (localhost) are local; claude + kimi are not.
        assert!(visible.contains(&"claude-code"), "{visible:?}");
        assert!(visible.contains(&"ollama"), "{visible:?}");
        assert!(!visible.contains(&"claude"), "{visible:?}");
        assert!(!visible.contains(&"kimi"), "{visible:?}");
    }

    #[test]
    fn check_local_only_blocks_remote_active() {
        let mut df = DecksFile::default();
        df.local_only = true;
        // Default active is 0 (claude-code, local) → OK.
        assert!(df.check_local_only().is_ok());
        // Switch active to "claude" (index 2, remote) → Err.
        df.active = 2;
        assert!(df.check_local_only().is_err());
    }

    #[test]
    fn local_only_field_serde_defaults_false() {
        // Files without the field must deserialise with local_only = false.
        let json = r#"{"decks": [], "active": 0}"#;
        let df: DecksFile = serde_json::from_str(json).unwrap();
        assert!(!df.local_only);
    }
}
