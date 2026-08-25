// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! Multi-session chat store, keyed by a stable DOCUMENT UUID and kept
//! APP-LOCAL (`~/.config/itsjustcad/chats/<uuid>.json`), private by default.
//! Chats are NEVER auto-written into the shared document — they live beside it,
//! not inside it, so opening someone else's `.itsjustcad` file never leaks your
//! conversations. Attaching/exporting a chat into the document is a separate,
//! explicit opt-in (not done here).
//!
//! Builds on the single-session scaffolding (per-doc "New session") by letting a
//! document own MANY named sessions and by adding full-text SEARCH across them:
//! a linear substring scan returns the matching sessions, a snippet, and the
//! turn index to jump to.

use serde::{Deserialize, Serialize};

/// One turn in a session, mirroring the deck's role/content shape but owned by
/// the app so the store is independent of the deck crate's wire types.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Turn {
    /// "user" or "assistant".
    pub role: String,
    pub content: String,
}

/// A single named conversation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChatSession {
    /// Stable per-session id (uuid string). Distinct from the document uuid.
    pub id: String,
    /// Human-facing title (defaults to the first user line, truncated).
    pub title: String,
    /// Unix seconds of the last update; drives newest-first ordering.
    pub updated: u64,
    pub turns: Vec<Turn>,
}

impl ChatSession {
    /// A fresh, empty session with a random id and a placeholder title.
    pub fn new(now: u64) -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            title: "New chat".to_string(),
            updated: now,
            turns: Vec::new(),
        }
    }

    /// Append a turn and refresh the title/timestamp. The first user turn seeds
    /// the title (truncated) so the session list is scannable.
    pub fn push(&mut self, role: &str, content: &str, now: u64) {
        if self.turns.is_empty() && role == "user" {
            self.title = title_from(content);
        }
        self.turns.push(Turn { role: role.into(), content: content.into() });
        self.updated = now;
    }
}

/// First line of `content`, trimmed to 48 chars, as a session title.
fn title_from(content: &str) -> String {
    let first = content.lines().next().unwrap_or("").trim();
    if first.chars().count() <= 48 {
        first.to_string()
    } else {
        let cut: String = first.chars().take(47).collect();
        format!("{cut}…")
    }
}

/// All sessions belonging to one document, newest-first.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DocSessions {
    /// The document uuid these sessions are keyed to.
    pub doc_uuid: String,
    pub sessions: Vec<ChatSession>,
}

/// One search hit: which session matched, a context snippet, and the turn index
/// to jump to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SearchHit {
    pub session_id: String,
    pub session_title: String,
    pub turn_index: usize,
    pub snippet: String,
}

impl DocSessions {
    pub fn new(doc_uuid: String) -> Self {
        Self { doc_uuid, sessions: Vec::new() }
    }

    /// Sort sessions newest-first by `updated`.
    pub fn sort_recent(&mut self) {
        self.sessions.sort_by_key(|a| std::cmp::Reverse(a.updated));
    }

    /// Full-text SEARCH across every turn of every session: a case-insensitive
    /// linear substring scan. Returns one hit per matching turn (first match per
    /// turn), so the UI can list matching sessions with a snippet and jump to the
    /// exact turn. An empty/whitespace query returns nothing.
    pub fn search(&self, query: &str) -> Vec<SearchHit> {
        let needle = query.trim().to_lowercase();
        if needle.is_empty() {
            return Vec::new();
        }
        let mut hits = Vec::new();
        for session in &self.sessions {
            for (i, turn) in session.turns.iter().enumerate() {
                let hay = turn.content.to_lowercase();
                if let Some(pos) = hay.find(&needle) {
                    hits.push(SearchHit {
                        session_id: session.id.clone(),
                        session_title: session.title.clone(),
                        turn_index: i,
                        snippet: snippet_around(&turn.content, pos, needle.len()),
                    });
                }
            }
        }
        hits
    }

    /// Find a session by id.
    pub fn get(&self, id: &str) -> Option<&ChatSession> {
        self.sessions.iter().find(|s| s.id == id)
    }
}

/// A ±30-char context window around a byte match, on char boundaries, with
/// ellipses when clipped. `pos`/`len` are byte offsets into the lowercased
/// haystack, which shares byte offsets with `content` for the ASCII case and is
/// clamped to char boundaries for safety.
fn snippet_around(content: &str, pos: usize, len: usize) -> String {
    const CTX: usize = 30;
    let start = floor_char_boundary(content, pos.saturating_sub(CTX));
    let end = ceil_char_boundary(content, (pos + len + CTX).min(content.len()));
    let mut out = String::new();
    if start > 0 {
        out.push('…');
    }
    out.push_str(content[start..end].trim());
    if end < content.len() {
        out.push('…');
    }
    out
}

fn floor_char_boundary(s: &str, mut i: usize) -> usize {
    if i >= s.len() {
        return s.len();
    }
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn ceil_char_boundary(s: &str, mut i: usize) -> usize {
    if i >= s.len() {
        return s.len();
    }
    while i < s.len() && !s.is_char_boundary(i) {
        i += 1;
    }
    i
}

/// App-local path for a document's chat store: `~/.config/itsjustcad/chats/<uuid>.json`.
pub fn store_path(doc_uuid: &str) -> Option<std::path::PathBuf> {
    // Guard against path traversal: a uuid is hex + dashes only.
    if doc_uuid.is_empty()
        || !doc_uuid.chars().all(|c| c.is_ascii_hexdigit() || c == '-')
    {
        return None;
    }
    Some(
        dirs::home_dir()?
            .join(".config")
            .join("itsjustcad")
            .join("chats")
            .join(format!("{doc_uuid}.json")),
    )
}

impl DocSessions {
    /// Load this document's sessions from the app-local store, or an empty set.
    pub fn load(doc_uuid: &str) -> Self {
        store_path(doc_uuid)
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_else(|| DocSessions::new(doc_uuid.to_string()))
    }

    /// Persist to the app-local store, private (0600). Best-effort — a write
    /// failure is logged, not fatal. NEVER writes into the shared document.
    pub fn save(&self) {
        let Some(path) = store_path(&self.doc_uuid) else { return };
        let Some(parent) = path.parent() else { return };
        let _ = std::fs::create_dir_all(parent);
        if let Ok(json) = serde_json::to_string_pretty(self) {
            let _ = crate::journal::write_private(&path, json.as_bytes());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc_with_two_sessions() -> DocSessions {
        let mut docs = DocSessions::new("11112222-3333-4444-5555-666677778888".into());
        let mut a = ChatSession::new(100);
        a.push("user", "make a five by five office core", 100);
        a.push("assistant", "Drew the core.", 101);
        let mut b = ChatSession::new(200);
        b.push("user", "add a curtain wall facade to the north side", 200);
        b.push("assistant", "Added a facade of glass panels.", 201);
        docs.sessions.push(a);
        docs.sessions.push(b);
        docs
    }

    #[test]
    fn search_returns_the_right_session_for_a_query() {
        let docs = doc_with_two_sessions();
        // "facade" only appears in session b.
        let hits = docs.search("facade");
        assert!(!hits.is_empty(), "expected a hit for 'facade'");
        assert!(
            hits.iter().all(|h| h.session_title.contains("curtain wall")),
            "all facade hits must be in the curtain-wall session: {hits:?}"
        );
        // "core" only appears in session a (in both its turns).
        let core = docs.search("core");
        assert!(!core.is_empty());
        assert!(
            core.iter().all(|h| h.session_title.contains("office core")),
            "all 'core' hits must be in session a: {core:?}"
        );
        assert_eq!(core[0].turn_index, 0, "first 'core' hit is the user turn");
    }

    #[test]
    fn search_is_case_insensitive_and_snippets_the_match() {
        let docs = doc_with_two_sessions();
        let hits = docs.search("GLASS");
        assert_eq!(hits.len(), 1);
        assert!(
            hits[0].snippet.to_lowercase().contains("glass"),
            "snippet must include the match: {:?}",
            hits[0].snippet
        );
    }

    #[test]
    fn empty_query_matches_nothing() {
        let docs = doc_with_two_sessions();
        assert!(docs.search("").is_empty());
        assert!(docs.search("   ").is_empty());
    }

    #[test]
    fn no_match_returns_empty() {
        let docs = doc_with_two_sessions();
        assert!(docs.search("helicopter").is_empty());
    }

    #[test]
    fn title_seeded_from_first_user_turn() {
        let mut s = ChatSession::new(0);
        s.push("user", "a very short one", 0);
        assert_eq!(s.title, "a very short one");
        // An assistant-first session keeps the placeholder.
        let mut a = ChatSession::new(0);
        a.push("assistant", "hi", 0);
        assert_eq!(a.title, "New chat");
    }

    #[test]
    fn sort_recent_orders_newest_first() {
        let mut docs = doc_with_two_sessions();
        docs.sort_recent();
        assert_eq!(docs.sessions[0].updated, 201.max(200)); // session b (updated 201)
        assert!(docs.sessions[0].updated >= docs.sessions[1].updated);
    }

    #[test]
    fn store_path_rejects_traversal_and_bad_uuids() {
        assert!(store_path("../../etc/passwd").is_none());
        assert!(store_path("").is_none());
        assert!(store_path("has/slash").is_none());
        assert!(store_path("11112222-3333-4444-5555-666677778888").is_some());
    }

    #[test]
    fn round_trips_through_json() {
        let docs = doc_with_two_sessions();
        let json = serde_json::to_string(&docs).unwrap();
        let back: DocSessions = serde_json::from_str(&json).unwrap();
        assert_eq!(back, docs);
        assert_eq!(back.doc_uuid, docs.doc_uuid);
    }

    #[test]
    fn multibyte_content_snippet_stays_on_char_boundaries() {
        let mut docs = DocSessions::new("aaaa1111-2222-3333-4444-555566667777".into());
        let mut s = ChatSession::new(0);
        // Non-ASCII around the match to exercise char-boundary clamping.
        s.push("user", "café — draw a façade wall then résumé the work", 0);
        docs.sessions.push(s);
        let hits = docs.search("façade");
        assert_eq!(hits.len(), 1);
        // Must not panic and must contain the match.
        assert!(hits[0].snippet.contains("façade"));
    }
}
