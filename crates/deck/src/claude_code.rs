use async_trait::async_trait;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt as _, BufReader};
use tokio::sync::mpsc::UnboundedSender;

use crate::config::DeckConfig;
use crate::deck::{ChatMessage, ChatRequest, DeckDelta, DeckError, LlmDeck, Role};

/// Build the `-p` prompt from a transcript. With a resumable session, send only
/// the newest user message (the CLI holds the rest server-side and keeps the
/// prompt cache warm); otherwise flatten the whole transcript into one prompt.
/// Pure so prompt shape is unit-testable without spawning the CLI.
pub(crate) fn build_prompt(messages: &[ChatMessage], has_session: bool) -> String {
    if has_session {
        messages
            .iter()
            .rev()
            .find(|m| m.role == Role::User)
            .map(|m| m.content.clone())
            .unwrap_or_default()
    } else {
        let mut p = String::new();
        for m in messages {
            let tag = match m.role {
                Role::User => "User",
                Role::Assistant => "Assistant",
            };
            p.push_str(&format!("{tag}: {}\n\n", m.content));
        }
        p.push_str("Assistant:");
        p
    }
}

/// Resolve the effective `--allowed-tools` list for a turn.
///
/// SECURITY (H-1): file access is only ever a Read scoped to the single vision
/// screenshot. This helper is the choke point:
/// - Any bare/unscoped `Read` (or `Read()` with no path) in `allowed_tools` is
///   DROPPED — it must never reach the CLI, or an attacker-controlled scene name
///   in the prompt could steer the model to read `decks.json` (API keys) or any
///   other file.
/// - When `vision_shot_path` is set, a single `Read(<abs path>)` is appended,
///   granting access to exactly that one screenshot and nothing else.
///
/// Pure so the scoping rule is unit-testable without spawning the CLI.
pub fn scoped_allowed_tools(allowed: &[String], vision_shot_path: Option<&str>) -> Vec<String> {
    let mut out: Vec<String> = allowed
        .iter()
        .filter(|t| !is_unscoped_read(t))
        .cloned()
        .collect();
    if let Some(path) = vision_shot_path {
        let path = path.trim();
        if !path.is_empty() {
            out.push(format!("Read({path})"));
        }
    }
    out
}

/// True for a Read specifier that is NOT confined to a concrete path: bare
/// `Read`, or `Read()` / `Read( )` with an empty argument. These are forbidden.
fn is_unscoped_read(tool: &str) -> bool {
    let t = tool.trim();
    if t == "Read" {
        return true;
    }
    if let Some(rest) = t.strip_prefix("Read(").and_then(|r| r.strip_suffix(')')) {
        return rest.trim().is_empty();
    }
    false
}

/// Claude via the local `claude` CLI as a hidden subprocess — uses the user's
/// Claude subscription (OAuth), no API key. The CLI is transport only; the
/// command substrate never leaves ItsJustCAD.
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
        let prompt = build_prompt(&req.messages, req.session_id.is_some());

        let model = if req.model.is_empty() { &self.model } else { &req.model };
        // A tool-using turn needs at least 2 agentic steps (call the tool, then
        // answer); clamp so a caller can never starve the answer step.
        let max_turns = req.max_turns.max(1).to_string();
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
            max_turns,
            "--settings".into(),
            r#"{"disableAllHooks":true}"#.into(),
            // Do not load the user's MCP servers (e.g. Serena spawns a
            // dashboard window on every session).
            "--strict-mcp-config".into(),
            "--mcp-config".into(),
            r#"{"mcpServers":{}}"#.into(),
        ];
        // Grant only the tools this turn opted into. Empty keeps the deck a
        // pure text substrate (the default).
        //
        // SECURITY (H-1): file access is NEVER an unscoped `Read`. A vision
        // critique sets `vision_shot_path`; we translate it into a Read scoped
        // to exactly that one file (`Read(<abs path>)`). Any bare `Read` (or
        // other unscoped Read) that somehow reaches `allowed_tools` is dropped —
        // the only way to read a file is the single scoped screenshot path.
        let scoped = scoped_allowed_tools(&req.allowed_tools, req.vision_shot_path.as_deref());
        if !scoped.is_empty() {
            args.push("--allowed-tools".into());
            args.push(scoped.join(","));
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(role: Role, content: &str) -> ChatMessage {
        ChatMessage { role, content: content.into() }
    }

    #[test]
    fn no_session_flattens_whole_transcript() {
        let messages = vec![
            msg(Role::User, "make a box"),
            msg(Role::Assistant, "done"),
            msg(Role::User, "now critique it"),
        ];
        let p = build_prompt(&messages, false);
        assert!(p.contains("User: make a box"));
        assert!(p.contains("Assistant: done"));
        assert!(p.contains("User: now critique it"));
        assert!(p.trim_end().ends_with("Assistant:"));
    }

    #[test]
    fn session_sends_only_newest_user_message() {
        let messages = vec![
            msg(Role::User, "make a box"),
            msg(Role::Assistant, "done"),
            msg(Role::User, "now critique it"),
        ];
        // With a resumable session the CLI holds history, so only the last
        // user turn goes out — no tags, no earlier turns.
        assert_eq!(build_prompt(&messages, true), "now critique it");
    }

    #[test]
    fn session_prompt_empty_when_no_user_turn() {
        let messages = vec![msg(Role::Assistant, "hi")];
        assert!(build_prompt(&messages, true).is_empty());
    }

    // --- SECURITY H-1: file access is only ever a path-scoped Read ---

    #[test]
    fn vision_shot_becomes_a_path_scoped_read() {
        let tools = scoped_allowed_tools(&[], Some("/tmp/itsjustcad-critique.png"));
        assert_eq!(tools, vec!["Read(/tmp/itsjustcad-critique.png)".to_string()]);
    }

    #[test]
    fn no_vision_shot_grants_no_read() {
        // The default text turn: no file access whatsoever.
        assert!(scoped_allowed_tools(&[], None).is_empty());
    }

    #[test]
    fn bare_read_is_dropped_even_if_requested() {
        // Regression for H-1: a bare/unscoped `Read` must NEVER reach the CLI,
        // even if a caller (or a future bug) puts one in `allowed_tools`. Before
        // the fix, `--allowed-tools Read` granted arbitrary file read, letting an
        // attacker-controlled scene name steer the model into reading decks.json.
        let tools = scoped_allowed_tools(&["Read".into()], None);
        assert!(tools.is_empty(), "unscoped Read must be stripped, got {tools:?}");

        // Empty-arg Read() is equally unscoped and must go too.
        assert!(scoped_allowed_tools(&["Read()".into()], None).is_empty());
        assert!(scoped_allowed_tools(&["Read( )".into()], None).is_empty());
    }

    #[test]
    fn bare_read_dropped_but_scoped_shot_still_granted() {
        // A stray bare Read is dropped; the legitimate scoped screenshot remains
        // the ONLY file the model can open.
        let tools = scoped_allowed_tools(&["Read".into()], Some("/tmp/shot.png"));
        assert_eq!(tools, vec!["Read(/tmp/shot.png)".to_string()]);
    }

    #[test]
    fn empty_vision_path_grants_nothing() {
        assert!(scoped_allowed_tools(&[], Some("   ")).is_empty());
    }
}
