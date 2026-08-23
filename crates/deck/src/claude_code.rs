use async_trait::async_trait;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt as _, BufReader};
use tokio::sync::mpsc::UnboundedSender;

use crate::config::DeckConfig;
use crate::deck::{ChatRequest, DeckDelta, DeckError, LlmDeck, Role};

/// Claude via the local `claude` CLI as a hidden subprocess — uses the user's
/// Claude subscription (OAuth), no API key. The CLI is transport only; the
/// command substrate never leaves mydrafter.
pub struct ClaudeCodeDeck {
    name: String,
    model: String,
}

impl ClaudeCodeDeck {
    pub fn new(config: &DeckConfig) -> Self {
        Self {
            name: config.name.clone(),
            model: config.model.clone(),
        }
    }

    async fn stream_inner(
        &self,
        req: ChatRequest,
        tx: &UnboundedSender<DeckDelta>,
    ) -> Result<(), DeckError> {
        // With a session to resume, send only the newest message — the CLI
        // holds the conversation server-side (and keeps the prompt cache warm).
        // Otherwise flatten the transcript into one prompt.
        let prompt = if req.session_id.is_some() {
            req.messages
                .iter()
                .rev()
                .find(|m| m.role == Role::User)
                .map(|m| m.content.clone())
                .unwrap_or_default()
        } else {
            let mut p = String::new();
            for m in &req.messages {
                let tag = match m.role {
                    Role::User => "User",
                    Role::Assistant => "Assistant",
                };
                p.push_str(&format!("{tag}: {}\n\n", m.content));
            }
            p.push_str("Assistant:");
            p
        };

        let model = if req.model.is_empty() { &self.model } else { &req.model };
        let mut args: Vec<String> = vec![
            "-p".into(),
            prompt,
            "--system-prompt".into(),
            req.system.clone(),
            "--model".into(),
            model.clone(),
            "--output-format".into(),
            "stream-json".into(),
            "--verbose".into(),
            "--include-partial-messages".into(),
            "--max-turns".into(),
            "1".into(),
            "--settings".into(),
            r#"{"disableAllHooks":true}"#.into(),
            // Do not load the user's MCP servers (e.g. Serena spawns a
            // dashboard window on every session).
            "--strict-mcp-config".into(),
            "--mcp-config".into(),
            r#"{"mcpServers":{}}"#.into(),
        ];
        if let Some(session) = &req.session_id {
            args.push("--resume".into());
            args.push(session.clone());
        }
        let mut child = tokio::process::Command::new("claude")
            .args(&args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| DeckError::Stream(format!("cannot launch claude CLI: {e}")))?;

        let stdout = child.stdout.take().expect("piped stdout");
        let mut lines = BufReader::new(stdout).lines();
        let mut got_result = false;
        while let Ok(Some(line)) = lines.next_line().await {
            let Ok(value) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            match value["type"].as_str() {
                Some("system") if value["subtype"].as_str() == Some("init") => {
                    if let Some(sid) = value["session_id"].as_str() {
                        let _ = tx.send(DeckDelta::Session(sid.to_string()));
                    }
                }
                Some("stream_event") => {
                    let delta = &value["event"]["delta"];
                    // Forward text; skip thinking deltas.
                    if delta["type"].as_str() == Some("text_delta")
                        && let Some(text) = delta["text"].as_str()
                        && !text.is_empty()
                    {
                        let _ = tx.send(DeckDelta::Text(text.to_string()));
                    }
                }
                Some("result") => {
                    got_result = true;
                    if value["is_error"].as_bool() == Some(true) {
                        let msg = value["result"].as_str().unwrap_or("unknown CLI error");
                        return Err(DeckError::Stream(msg.to_string()));
                    }
                    break;
                }
                _ => {}
            }
        }
        let _ = child.wait().await;
        if !got_result {
            return Err(DeckError::Stream(
                "claude CLI ended without a result event".into(),
            ));
        }
        Ok(())
    }
}

#[async_trait]
impl LlmDeck for ClaudeCodeDeck {
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
