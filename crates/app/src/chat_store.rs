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
    /// A 1-2 line summary of the conversation, shown on the session CARD under
    /// the title. Serde-defaults to empty for stores written before this field
    /// existed; [`ChatSession::derive_meta`] fills a fallback from the first
    /// exchange (a lightweight LLM pass may replace it later).
    #[serde(default)]
    pub summary: String,
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
            summary: String::new(),
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

    /// Fallback title + summary from the first exchange, used whenever no LLM
    /// pass has run (headless, tests, offline, or before the session grows).
    /// Title = first user prompt (truncated); summary = the first user prompt
    /// plus a short preview of the first assistant reply. Never calls a model.
    pub fn derive_meta(&mut self) {
        if let Some(first_user) = self.turns.iter().find(|t| t.role == "user") {
            self.title = title_from(&first_user.content);
        }
        self.summary = summary_from_turns(&self.turns);
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

/// Short preview (≤ `max` chars, single line) of a message body.
fn preview(content: &str, max: usize) -> String {
    let flat: String = content.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max {
        flat
    } else {
        let cut: String = flat.chars().take(max.saturating_sub(1)).collect();
        format!("{cut}…")
    }
}

/// Fallback summary: the first user ask, then a preview of the first assistant
/// reply (when present), joined into a 1-2 line blurb.
fn summary_from_turns(turns: &[Turn]) -> String {
    let user = turns.iter().find(|t| t.role == "user");
    let assistant = turns.iter().find(|t| t.role == "assistant");
    match (user, assistant) {
        (Some(u), Some(a)) => {
            format!("{} → {}", preview(&u.content, 60), preview(&a.content, 80))
        }
        (Some(u), None) => preview(&u.content, 120),
        (None, Some(a)) => preview(&a.content, 120),
        (None, None) => String::new(),
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

    /// Insert `session`, or REPLACE the existing one with the same id in place —
    /// then re-sort newest-first. This is the dedup-free archive path: continuing
    /// a loaded session updates it rather than forking a fresh copy. Caller saves.
    pub fn upsert(&mut self, session: ChatSession) {
        if let Some(slot) = self.sessions.iter_mut().find(|s| s.id == session.id) {
            *slot = session;
        } else {
            self.sessions.push(session);
        }
        self.sort_recent();
    }

    /// Refine a stored session's title/summary by id (the async LLM pass). Both
    /// are only applied when non-empty so a partial model reply never blanks a
    /// good deterministic label. No-op when the id is gone. Caller saves.
    pub fn set_meta(&mut self, id: &str, title: &str, summary: &str) {
        if let Some(s) = self.sessions.iter_mut().find(|s| s.id == id) {
            if !title.trim().is_empty() {
                s.title = title.trim().to_string();
            }
            if !summary.trim().is_empty() {
                s.summary = summary.trim().to_string();
            }
        }
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

/// Format a Unix-seconds timestamp as a UTC `YYYY-MM-DD` date for a session
/// card. Pure civil-date arithmetic (no chrono dep) so it is unit-testable and
/// deterministic. Based on Howard Hinnant's days-from-civil algorithm.
pub fn fmt_date(unix: u64) -> String {
    let days = (unix / 86_400) as i64;
    // Shift epoch to 0000-03-01 so leap days land at the end of the era.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
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
    /// Uses the real OS-keychain-backed key store; see [`Self::load_with`].
    pub fn load(doc_uuid: &str) -> Self {
        Self::load_with(doc_uuid, &crate::chat_crypto::OsKeyStore)
    }

    /// Persist to the app-local store; see [`Self::save_with_opts`]. Encryption
    /// follows the user's `chat_encryption` preference (default ON).
    pub fn save(&self) {
        self.save_with_opts(&crate::chat_crypto::OsKeyStore, crate::app::load_chat_encryption());
    }

    /// Load, transparently decrypting an encrypted store or reading a legacy
    /// plaintext one (both distinguished by a file marker). Any read/decrypt
    /// failure yields an empty set rather than losing the caller's day.
    /// `store` is injectable so tests never touch the real keychain.
    pub fn load_with<K: crate::chat_crypto::KeyStore>(doc_uuid: &str, store: &K) -> Self {
        store_path(doc_uuid)
            .and_then(|p| std::fs::read(p).ok())
            .and_then(|bytes| crate::chat_crypto::open(store, &bytes).ok())
            .and_then(|json| serde_json::from_slice(&json).ok())
            .unwrap_or_else(|| DocSessions::new(doc_uuid.to_string()))
    }

    /// Persist to the app-local store, private (0600). `encrypt` gates at-rest
    /// encryption: when `false` the store is written as plaintext JSON
    /// (self-describing, so it still loads); when `true` it is sealed if a
    /// keychain is available (plaintext fallback with a one-time warning
    /// otherwise). Best-effort; NEVER writes into the shared document. `store`
    /// is injectable for tests.
    pub fn save_with_opts<K: crate::chat_crypto::KeyStore>(&self, store: &K, encrypt: bool) {
        let Some(path) = store_path(&self.doc_uuid) else { return };
        let Some(parent) = path.parent() else { return };
        let _ = std::fs::create_dir_all(parent);
        if let Ok(json) = serde_json::to_vec_pretty(self) {
            let blob = if encrypt {
                crate::chat_crypto::seal_for_write(store, &json)
            } else {
                json
            };
            let _ = crate::journal::write_private(&path, &blob);
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
        assert_eq!(docs.sessions[0].updated, 201); // session b (updated 201)
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
    fn derive_meta_builds_fallback_title_and_summary() {
        let mut s = ChatSession::new(0);
        s.push("user", "make a five by five office core with a stair", 0);
        s.push("assistant", "Drew the core and added a stair to level 2.", 1);
        s.derive_meta();
        assert_eq!(s.title, "make a five by five office core with a stair");
        assert!(s.summary.contains("office core"), "summary keeps the ask: {}", s.summary);
        assert!(s.summary.contains('→'), "summary joins ask→reply: {}", s.summary);
        assert!(s.summary.contains("stair"), "summary previews the reply: {}", s.summary);
    }

    #[test]
    fn derive_meta_user_only_summary() {
        let mut s = ChatSession::new(0);
        s.push("user", "add a curtain wall", 0);
        s.derive_meta();
        assert_eq!(s.title, "add a curtain wall");
        assert_eq!(s.summary, "add a curtain wall");
    }

    #[test]
    fn summary_field_serde_defaults_when_absent() {
        // A store written BEFORE `summary` existed: the field is absent from the
        // JSON. Deserialize must succeed and default the summary to "".
        let legacy = r#"{
            "doc_uuid": "11112222-3333-4444-5555-666677778888",
            "sessions": [
                {
                    "id": "aaaa",
                    "title": "old session",
                    "updated": 42,
                    "turns": [{ "role": "user", "content": "hi" }]
                }
            ]
        }"#;
        let docs: DocSessions = serde_json::from_str(legacy).expect("legacy load");
        assert_eq!(docs.sessions.len(), 1);
        assert_eq!(docs.sessions[0].summary, "", "missing summary defaults to empty");
        assert_eq!(docs.sessions[0].title, "old session");
    }

    #[test]
    fn upsert_replaces_by_id_without_dup_and_sorts_newest_first() {
        let mut docs = doc_with_two_sessions();
        let existing_id = docs.sessions[0].id.clone(); // session a (updated 101)
        let before = docs.sessions.len();
        // Upsert a session reusing an existing id but with a newer timestamp.
        let mut updated = ChatSession::new(500);
        updated.id = existing_id.clone();
        updated.push("user", "the continued conversation", 500);
        docs.upsert(updated);
        // No duplicate: same count, exactly one session with that id.
        assert_eq!(docs.sessions.len(), before, "upsert must not add a dup");
        assert_eq!(
            docs.sessions.iter().filter(|s| s.id == existing_id).count(),
            1
        );
        // Replaced in place (new content) and re-sorted newest-first.
        assert_eq!(docs.sessions[0].id, existing_id);
        assert_eq!(docs.sessions[0].updated, 500);
        assert_eq!(docs.sessions[0].turns[0].content, "the continued conversation");
        // The OTHER session is preserved.
        assert!(docs.sessions.iter().any(|s| s.title.contains("curtain wall")));
    }

    #[test]
    fn upsert_pushes_a_fresh_id() {
        let mut docs = doc_with_two_sessions();
        let before = docs.sessions.len();
        let mut fresh = ChatSession::new(999);
        fresh.push("user", "brand new chat", 999);
        docs.upsert(fresh);
        assert_eq!(docs.sessions.len(), before + 1);
        assert_eq!(docs.sessions[0].updated, 999, "newest sorts first");
    }

    #[test]
    fn set_meta_updates_by_id_and_ignores_blanks() {
        let mut docs = doc_with_two_sessions();
        let id = docs.sessions[0].id.clone();
        let old_title = docs.sessions[0].title.clone();
        docs.set_meta(&id, "Office core", "A five-by-five core with a stair.");
        let s = docs.get(&id).unwrap();
        assert_eq!(s.title, "Office core");
        assert_eq!(s.summary, "A five-by-five core with a stair.");
        // Blank title/summary must NOT clobber a good label.
        docs.set_meta(&id, "   ", "");
        let s = docs.get(&id).unwrap();
        assert_eq!(s.title, "Office core", "blank title left the label intact");
        assert_eq!(s.summary, "A five-by-five core with a stair.");
        // A missing id is a no-op (no panic, nothing else changes).
        docs.set_meta("no-such-id", "X", "Y");
        assert_ne!(old_title, docs.get(&id).unwrap().title);
    }

    #[test]
    fn fmt_date_formats_known_epochs() {
        assert_eq!(fmt_date(0), "1970-01-01");
        // 2021-01-01 00:00:00 UTC = 1609459200
        assert_eq!(fmt_date(1_609_459_200), "2021-01-01");
        // 2020-02-29 (leap day) 00:00:00 UTC = 1582934400
        assert_eq!(fmt_date(1_582_934_400), "2020-02-29");
    }

    /// In-memory key store for the encrypt/decrypt round-trip below — never
    /// touches the OS keychain.
    struct MemKey([u8; 32]);
    impl crate::chat_crypto::KeyStore for MemKey {
        fn get_or_create_key(&self) -> Result<[u8; 32], ()> {
            Ok(self.0)
        }
    }

    #[test]
    fn encrypted_store_serialize_round_trip() {
        // Serialize → seal (encrypt) → open (decrypt) → deserialize must recover
        // an identical DocSessions, mirroring the on-disk save/load path without
        // hitting the filesystem or the real keychain.
        let docs = doc_with_two_sessions();
        let key = MemKey([42u8; 32]);
        let json = serde_json::to_vec_pretty(&docs).unwrap();
        let sealed = crate::chat_crypto::seal_for_write(&key, &json);
        assert!(
            crate::chat_crypto::is_encrypted(&sealed),
            "store must be encrypted at rest with a healthy key"
        );
        let opened = crate::chat_crypto::open(&key, &sealed).unwrap();
        let back: DocSessions = serde_json::from_slice(&opened).unwrap();
        assert_eq!(back, docs);
    }

    #[test]
    fn opt_out_writes_plaintext_that_still_opens() {
        // With encryption disabled the serialized bytes are plain JSON (not our
        // encrypted container), yet `open` still recovers them — so a user who
        // turns encryption off loses no sessions and can turn it back on later.
        let docs = doc_with_two_sessions();
        let key = MemKey([7u8; 32]);
        let json = serde_json::to_vec_pretty(&docs).unwrap();
        assert!(
            !crate::chat_crypto::is_encrypted(&json),
            "opt-out must leave the store as plaintext JSON"
        );
        // The self-describing loader reads plaintext regardless of key.
        let opened = crate::chat_crypto::open(&key, &json).unwrap();
        let back: DocSessions = serde_json::from_slice(&opened).unwrap();
        assert_eq!(back, docs);
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
