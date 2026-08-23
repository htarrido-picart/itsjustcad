use serde::{Deserialize, Serialize};

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

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DecksFile {
    pub decks: Vec<DeckConfig>,
    #[serde(default)]
    pub active: usize,
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
                },
                DeckConfig {
                    name: "ollama".into(),
                    kind: DeckKind::OpenaiCompat,
                    base_url: "http://localhost:11434/v1".into(),
                    model: "qwen3".into(),
                    api_key: None,
                },
                DeckConfig {
                    name: "claude".into(),
                    kind: DeckKind::Anthropic,
                    base_url: "https://api.anthropic.com".into(),
                    model: "claude-sonnet-4-6".into(),
                    api_key: Some("env:ANTHROPIC_API_KEY".into()),
                },
                DeckConfig {
                    name: "kimi".into(),
                    kind: DeckKind::OpenaiCompat,
                    base_url: "https://api.moonshot.ai/v1".into(),
                    model: "kimi-k2-0905-preview".into(),
                    api_key: Some("env:MOONSHOT_API_KEY".into()),
                },
            ],
            active: 0,
        }
    }
}

pub fn config_path() -> Option<std::path::PathBuf> {
    // ~/.config/mydrafter on every platform — CLI-tool convention, greppable.
    Some(dirs::home_dir()?.join(".config").join("mydrafter").join("decks.json"))
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
            let _ = std::fs::write(path, serde_json::to_string_pretty(self).expect("serializes"));
        }
    }
}
