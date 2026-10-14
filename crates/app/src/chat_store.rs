// SPDX-License-Identifier: AGPL-3.0-only
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
        self.turns.push(Turn {
            role: role.into(),
            content: content.into(),
        });
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
        Self {
            doc_uuid,
            sessions: Vec::new(),
        }
    }

    /// Sort sessions newest-first by `updated`.
    pub fn sort_recent(&mut self) {
        self.sessions.sort_by_key(|a| std::cmp::Reverse(a.updated));
    }

    /// Full-text SEARCH across every turn of every session in THIS document.
    ///
    /// Prefers FTS5 over the live database (fast, scales across many sessions /
    /// documents); falls back to a linear in-memory substring scan when the DB
    /// is unavailable (headless/no-home) or the query isn't FTS-expressible, so
    /// an unsaved in-memory `DocSessions` still searches. Returns one hit per
    /// matching turn with a snippet and the turn's `ord` as `turn_index`. An
    /// empty/whitespace query returns nothing.
    pub fn search(&self, query: &str) -> Vec<SearchHit> {
        let needle = query.trim();
        if needle.is_empty() {
            return Vec::new();
        }
        if let Some(hits) = self.search_fts(needle) {
            return hits;
        }
        self.search_linear(needle)
    }

    /// FTS5-backed search scoped to this document. `None` when the DB is
    /// unavailable or the query produces no FTS syntax (caller falls back).
    fn search_fts(&self, query: &str) -> Option<Vec<SearchHit>> {
        let path = crate::chat_db::db_path()?;
        if !path.exists() {
            return None;
        }
        let conn = crate::chat_db::open_db(&path).ok()?;
        search_fts_conn(&conn, &self.doc_uuid, query).ok().flatten()
    }

    /// FTS5 search against an explicit DB path (test seam / direct DB query).
    /// Returns an empty vec for an empty/whitespace query.
    #[allow(dead_code)] // hermetic test/API seam; production uses search()/search_fts.
    pub fn search_at(
        doc_uuid: &str,
        query: &str,
        db_path: &std::path::Path,
    ) -> rusqlite::Result<Vec<SearchHit>> {
        if query.trim().is_empty() {
            return Ok(Vec::new());
        }
        let conn = crate::chat_db::open_db(db_path)?;
        Ok(search_fts_conn(&conn, doc_uuid, query.trim())?.unwrap_or_default())
    }

    /// Case-insensitive linear substring scan over the in-memory sessions (the
    /// pre-DB behavior; also the fallback when no DB is present).
    fn search_linear(&self, query: &str) -> Vec<SearchHit> {
        let needle = query.to_lowercase();
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

/// Build a safe FTS5 MATCH expression from a free-text query: each whitespace
/// token becomes a double-quoted term (so punctuation / FTS operators in user
/// text can't blow up the parser), ANDed together. Returns `None` when the
/// query has no usable tokens (caller falls back to the linear scan).
fn fts_match_expr(query: &str) -> Option<String> {
    let terms: Vec<String> = query
        .split_whitespace()
        .filter(|t| t.chars().any(|c| c.is_alphanumeric()))
        .map(|t| {
            // Escape embedded double quotes per FTS5 string-literal rules.
            let escaped = t.replace('"', "\"\"");
            format!("\"{escaped}\"")
        })
        .collect();
    if terms.is_empty() {
        None
    } else {
        Some(terms.join(" AND "))
    }
}

/// Build a snippet for an FTS hit by locating the first query term in `content`
/// (case-insensitive) and windowing around it; falls back to a leading preview
/// when no term is found (e.g. a stemmed FTS match). Char-boundary safe.
fn snippet_for(content: &str, query: &str) -> String {
    let hay = content.to_lowercase();
    for term in query.split_whitespace() {
        let t = term.to_lowercase();
        if t.is_empty() {
            continue;
        }
        if let Some(pos) = hay.find(&t) {
            return snippet_around(content, pos, t.len());
        }
    }
    snippet_around(content, 0, 0)
}

/// Run the FTS5 query over one document using an open connection. Returns
/// `Ok(None)` when the query has no usable FTS tokens (caller falls back).
fn search_fts_conn(
    conn: &rusqlite::Connection,
    doc_uuid: &str,
    query: &str,
) -> rusqlite::Result<Option<Vec<SearchHit>>> {
    let Some(match_expr) = fts_match_expr(query) else {
        return Ok(None);
    };
    let mut stmt = conn.prepare(
        "SELECT s.uuid, s.title, m.ord, m.content \
         FROM messages_fts f \
         JOIN messages m ON m.id = f.rowid \
         JOIN sessions s ON s.id = m.session_id \
         WHERE s.doc_uuid = ?1 AND f.content MATCH ?2 \
         ORDER BY s.updated_at DESC, m.ord ASC",
    )?;
    let rows = stmt.query_map(rusqlite::params![doc_uuid, match_expr], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, i64>(2)?,
            r.get::<_, String>(3)?,
        ))
    })?;
    let mut hits = Vec::new();
    for row in rows {
        let (session_id, session_title, ord, content) = row?;
        hits.push(SearchHit {
            session_id,
            session_title,
            turn_index: ord.max(0) as usize,
            snippet: snippet_for(&content, query),
        });
    }
    Ok(Some(hits))
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

/// Legacy per-document JSON store path: `~/.config/itsjustcad/chats/<uuid>.json`.
///
/// The live backend is now SQLite ([`crate::chat_db`]); these JSON files are
/// the pre-SQLite format, kept for the ONE-TIME import and left in place as a
/// read-only backup (never written again). The uuid is validated (hex + dashes
/// only) to guard against path traversal — a check still reused by call sites.
pub fn store_path(doc_uuid: &str) -> Option<std::path::PathBuf> {
    // Guard against path traversal: a uuid is hex + dashes only.
    if doc_uuid.is_empty() || !doc_uuid.chars().all(|c| c.is_ascii_hexdigit() || c == '-') {
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
    /// Load this document's sessions from the SQLite store (importing legacy
    /// JSON on first run); see [`Self::load_with`]. The `KeyStore`-generic
    /// variant is retained for API compatibility — encryption is handled by
    /// SQLCipher-later at the DB layer, not here, so the key store is unused in
    /// the DB path.
    pub fn load(doc_uuid: &str) -> Self {
        Self::load_with(doc_uuid, &crate::chat_crypto::OsKeyStore)
    }

    /// Persist to the SQLite store; see [`Self::save_with_opts`]. The `encrypt`
    /// preference is threaded through for API compatibility but is a no-op on
    /// the DB path (plaintext-at-rest until SQLCipher).
    pub fn save(&self) {
        self.save_with_opts(
            &crate::chat_crypto::OsKeyStore,
            crate::app::load_chat_encryption(),
        );
    }

    /// Load this document's sessions from SQLite, newest-first.
    ///
    /// On first open the one-time JSON→DB import runs (see
    /// [`crate::chat_db::import`]-equivalent logic in [`import_legacy_json`]),
    /// after which the legacy JSON files are left as a backup. Any DB error
    /// yields an empty set rather than losing the caller's day. The `KeyStore`
    /// is accepted for signature compatibility and is unused (encryption moved
    /// to the SQLCipher-ready DB seam); tests therefore never touch the real
    /// keychain regardless.
    pub fn load_with<K: crate::chat_crypto::KeyStore>(doc_uuid: &str, _store: &K) -> Self {
        let Some(path) = crate::chat_db::db_path() else {
            return DocSessions::new(doc_uuid.to_string());
        };
        Self::load_at(doc_uuid, &path).unwrap_or_else(|_| DocSessions::new(doc_uuid.to_string()))
    }

    /// Persist to SQLite. `encrypt` is accepted for API compatibility but is a
    /// no-op (plaintext-at-rest; see [`crate::chat_db::open_db`] for the
    /// SQLCipher seam). Best-effort; never writes into the shared document.
    pub fn save_with_opts<K: crate::chat_crypto::KeyStore>(&self, _store: &K, _encrypt: bool) {
        let Some(path) = crate::chat_db::db_path() else {
            return;
        };
        let _ = self.save_at(&path);
    }

    /// Hermetic load against an explicit DB path (test seam). Opens + migrates
    /// the DB, runs the one-time legacy import from the REAL chats directory,
    /// then reads this document's rows.
    pub fn load_at(doc_uuid: &str, db_path: &std::path::Path) -> rusqlite::Result<Self> {
        let legacy = store_path("00000000-0000-0000-0000-000000000000")
            .and_then(|p| p.parent().map(std::path::Path::to_path_buf));
        Self::load_at_with_legacy(doc_uuid, db_path, legacy.as_deref())
    }

    /// Fully hermetic load: explicit DB path AND explicit legacy chats dir
    /// (`None` to skip import). Used by tests so the one-time JSON import is
    /// exercised without touching `~/.config`.
    pub fn load_at_with_legacy(
        doc_uuid: &str,
        db_path: &std::path::Path,
        legacy_dir: Option<&std::path::Path>,
    ) -> rusqlite::Result<Self> {
        let mut conn = crate::chat_db::open_db(db_path)?;
        crate::chat_db::run_migrations(&mut conn)?;
        import_legacy_json(&mut conn, legacy_dir)?;
        read_doc(&conn, doc_uuid)
    }

    /// Hermetic save against an explicit DB path (test seam). Atomically
    /// replaces THIS document's sessions/messages in one transaction.
    pub fn save_at(&self, db_path: &std::path::Path) -> rusqlite::Result<()> {
        let mut conn = crate::chat_db::open_db(db_path)?;
        crate::chat_db::run_migrations(&mut conn)?;
        write_doc(&mut conn, self)
    }
}

/// Read all sessions for one document from the DB, newest-first.
fn read_doc(conn: &rusqlite::Connection, doc_uuid: &str) -> rusqlite::Result<DocSessions> {
    let mut docs = DocSessions::new(doc_uuid.to_string());
    let mut sstmt = conn.prepare(
        "SELECT id, uuid, title, summary, updated_at FROM sessions \
         WHERE doc_uuid = ?1 ORDER BY updated_at DESC, id DESC",
    )?;
    let sessions: Vec<(i64, String, String, String, i64)> = sstmt
        .query_map([doc_uuid], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
        })?
        .collect::<rusqlite::Result<_>>()?;
    let mut mstmt = conn.prepare(
        "SELECT role, content FROM messages WHERE session_id = ?1 ORDER BY ord ASC, id ASC",
    )?;
    for (rowid, uuid, title, summary, updated) in sessions {
        let turns: Vec<Turn> = mstmt
            .query_map([rowid], |r| {
                Ok(Turn {
                    role: r.get(0)?,
                    content: r.get(1)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?;
        docs.sessions.push(ChatSession {
            id: uuid,
            title,
            summary,
            updated: updated.max(0) as u64,
            turns,
        });
    }
    Ok(docs)
}

/// Atomically replace one document's sessions. Sessions absent from `docs` are
/// deleted (cascading their messages + FTS rows); present ones are upserted and
/// their messages fully reinserted in order. One transaction = all-or-nothing.
fn write_doc(conn: &mut rusqlite::Connection, docs: &DocSessions) -> rusqlite::Result<()> {
    let tx = conn.transaction()?;
    // Drop sessions that no longer exist for this document.
    {
        let keep: Vec<String> = docs.sessions.iter().map(|s| s.id.clone()).collect();
        let mut existing = tx.prepare("SELECT uuid FROM sessions WHERE doc_uuid = ?1")?;
        let present: Vec<String> = existing
            .query_map([&docs.doc_uuid], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<_>>()?;
        drop(existing);
        for uuid in present {
            if !keep.contains(&uuid) {
                tx.execute("DELETE FROM sessions WHERE uuid = ?1", [&uuid])?;
            }
        }
    }
    for s in &docs.sessions {
        upsert_session(&tx, &docs.doc_uuid, s)?;
    }
    tx.commit()
}

/// Upsert one session (by uuid) and fully replace its ordered messages. Used by
/// [`write_doc`] and the legacy import. `created_at` is preserved on update.
fn upsert_session(
    tx: &rusqlite::Transaction<'_>,
    doc_uuid: &str,
    s: &ChatSession,
) -> rusqlite::Result<()> {
    let updated = s.updated as i64;
    // Insert or update by the unique uuid; keep the original created_at.
    tx.execute(
        "INSERT INTO sessions(uuid, doc_uuid, title, summary, created_at, updated_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?5) \
         ON CONFLICT(uuid) DO UPDATE SET \
            doc_uuid = excluded.doc_uuid, \
            title = excluded.title, \
            summary = excluded.summary, \
            updated_at = excluded.updated_at",
        rusqlite::params![s.id, doc_uuid, s.title, s.summary, updated],
    )?;
    let rowid: i64 = tx.query_row("SELECT id FROM sessions WHERE uuid = ?1", [&s.id], |r| {
        r.get(0)
    })?;
    // Fully replace messages (cascades FTS via triggers). Simplest correct
    // upsert for conversations that only ever append.
    tx.execute("DELETE FROM messages WHERE session_id = ?1", [rowid])?;
    let mut ins = tx.prepare(
        "INSERT INTO messages(session_id, ord, role, content, created_at) \
         VALUES (?1, ?2, ?3, ?4, ?5)",
    )?;
    for (ord, turn) in s.turns.iter().enumerate() {
        ins.execute(rusqlite::params![
            rowid,
            ord as i64,
            turn.role,
            turn.content,
            updated
        ])?;
    }
    Ok(())
}

/// Marker recording that the one-time legacy JSON→DB import already ran, so it
/// never double-imports (idempotent even if the user later deletes all chats).
const IMPORT_MARKER: &str = "json_import_v1";

/// ONE-TIME import of pre-SQLite JSON chats into the DB. Runs when the
/// `json_import_v1` marker is absent from `grdb_migrations`-sibling bookkeeping;
/// scans `~/.config/itsjustcad/chats/*.json` (the multi-session archives) and
/// imports any the DB doesn't already have, then records the marker. Legacy
/// files are LEFT IN PLACE as a backup. Encrypted legacy stores are opened via
/// `chat_crypto` using the real keychain (best-effort; undecryptable ones are
/// skipped). Idempotent: the marker guards re-runs.
fn import_legacy_json(
    conn: &mut rusqlite::Connection,
    legacy_dir: Option<&std::path::Path>,
) -> rusqlite::Result<()> {
    // Bookkeeping table for non-schema one-time tasks (distinct from GRDB's
    // schema-migration table so it never confuses the shared migrator).
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS app_imports (marker TEXT NOT NULL PRIMARY KEY);",
    )?;
    let done: bool = conn
        .query_row(
            "SELECT 1 FROM app_imports WHERE marker = ?1",
            [IMPORT_MARKER],
            |_| Ok(true),
        )
        .unwrap_or(false);
    if done {
        return Ok(());
    }
    if let Some(dir) = legacy_dir
        && let Ok(entries) = std::fs::read_dir(dir)
    {
        let tx = conn.transaction()?;
        for entry in entries.flatten() {
            let path = entry.path();
            // Only the multi-session archives `<uuid>.json` (skip `.draft.json`).
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if !name.ends_with(".json") || name.ends_with(".draft.json") {
                continue;
            }
            let Ok(bytes) = std::fs::read(&path) else {
                continue;
            };
            // Legacy files may be plaintext JSON or chat_crypto-sealed.
            let Ok(json) = crate::chat_crypto::open(&crate::chat_crypto::OsKeyStore, &bytes) else {
                continue;
            };
            let Ok(legacy) = serde_json::from_slice::<DocSessions>(&json) else {
                continue;
            };
            for s in &legacy.sessions {
                // Skip sessions already present (keeps import idempotent even
                // if the marker were lost).
                let exists: bool = tx
                    .query_row("SELECT 1 FROM sessions WHERE uuid = ?1", [&s.id], |_| {
                        Ok(true)
                    })
                    .unwrap_or(false);
                if exists {
                    continue;
                }
                upsert_session(&tx, &legacy.doc_uuid, s)?;
            }
        }
        tx.commit()?;
    }
    conn.execute(
        "INSERT INTO app_imports(marker) VALUES (?1)",
        [IMPORT_MARKER],
    )?;
    Ok(())
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

    /// Unique temp DB path per test (pid + counter): hermetic + parallel-safe,
    /// never touches `~/.config`. Mirrors the workdir temp-dir+pid convention.
    fn temp_db_path() -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static CTR: AtomicU64 = AtomicU64::new(0);
        let n = CTR.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("ijc-chatstore-{}-{}.db", std::process::id(), n))
    }

    /// RAII temp DB: unique path, cleaned up on drop (incl. WAL/SHM sidecars).
    struct TmpDb(std::path::PathBuf);
    impl TmpDb {
        fn new() -> Self {
            let p = temp_db_path();
            let _ = std::fs::remove_file(&p);
            TmpDb(p)
        }
        fn path(&self) -> &std::path::Path {
            &self.0
        }
    }
    impl Drop for TmpDb {
        fn drop(&mut self) {
            for ext in ["", "-wal", "-shm"] {
                let _ = std::fs::remove_file(format!("{}{ext}", self.0.display()));
            }
        }
    }

    #[test]
    fn search_returns_the_right_session_for_a_query() {
        // Linear (in-memory) scan — the DB-less fallback semantics.
        let docs = doc_with_two_sessions();
        let hits = docs.search_linear("facade");
        assert!(!hits.is_empty(), "expected a hit for 'facade'");
        assert!(
            hits.iter()
                .all(|h| h.session_title.contains("curtain wall")),
            "all facade hits must be in the curtain-wall session: {hits:?}"
        );
        let core = docs.search_linear("core");
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
        let hits = docs.search_linear("GLASS");
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
        assert!(docs.search_linear("helicopter").is_empty());
    }

    #[test]
    fn fts_search_within_and_across_sessions_and_documents() {
        let db = TmpDb::new();
        // Two documents, each with sessions, saved to the same DB.
        let mut doc_a = DocSessions::new("aaaaaaaa-0000-0000-0000-000000000001".into());
        let mut s1 = ChatSession::new(100);
        s1.push("user", "make a five by five office core", 100);
        s1.push("assistant", "Drew the core.", 101);
        let mut s2 = ChatSession::new(200);
        s2.push("user", "add a curtain wall facade", 200);
        s2.push("assistant", "Added a facade of glass panels.", 201);
        doc_a.sessions.push(s1);
        doc_a.sessions.push(s2);
        doc_a.save_at(db.path()).unwrap();

        let mut doc_b = DocSessions::new("bbbbbbbb-0000-0000-0000-000000000002".into());
        let mut s3 = ChatSession::new(300);
        s3.push("user", "a glass helicopter pad on the roof", 300);
        doc_b.sessions.push(s3);
        doc_b.save_at(db.path()).unwrap();

        // Across sessions WITHIN doc_a: "facade" only in s2, "core" only in s1.
        let facade = DocSessions::search_at(&doc_a.doc_uuid, "facade", db.path()).unwrap();
        assert!(!facade.is_empty());
        assert!(
            facade
                .iter()
                .all(|h| h.session_title.contains("curtain wall"))
        );
        let core = DocSessions::search_at(&doc_a.doc_uuid, "core", db.path()).unwrap();
        assert!(core.iter().all(|h| h.session_title.contains("office core")));

        // Search is scoped PER DOCUMENT: "glass" matches in both docs, but each
        // query only returns its own document's hits.
        let glass_a = DocSessions::search_at(&doc_a.doc_uuid, "glass", db.path()).unwrap();
        assert_eq!(glass_a.len(), 1, "doc_a has one 'glass' turn");
        let glass_b = DocSessions::search_at(&doc_b.doc_uuid, "glass", db.path()).unwrap();
        assert_eq!(glass_b.len(), 1, "doc_b has its own 'glass' turn");
        assert_ne!(glass_a[0].session_id, glass_b[0].session_id);

        // No match returns empty; empty query returns empty.
        assert!(
            DocSessions::search_at(&doc_a.doc_uuid, "helicopter", db.path())
                .unwrap()
                .is_empty()
        );
        assert!(
            DocSessions::search_at(&doc_a.doc_uuid, "   ", db.path())
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn db_round_trip_preserves_turns_meta_and_epoch_timestamps() {
        let db = TmpDb::new();
        let mut docs = doc_with_two_sessions();
        docs.set_meta(&docs.sessions[0].id.clone(), "Office core", "A core.");
        docs.save_at(db.path()).unwrap();
        let back = DocSessions::load_at_with_legacy(&docs.doc_uuid, db.path(), None).unwrap();
        // Newest-first ordering, turns + meta preserved, timestamps as epoch ints.
        assert_eq!(back.sessions.len(), 2);
        let a = back
            .get(
                &docs
                    .sessions
                    .iter()
                    .find(|s| s.title == "Office core")
                    .unwrap()
                    .id,
            )
            .unwrap();
        assert_eq!(a.title, "Office core");
        assert_eq!(a.summary, "A core.");
        assert_eq!(a.updated, 101, "updated stored/read as unix seconds");
        assert_eq!(a.turns.len(), 2);
        assert_eq!(a.turns[0].role, "user");
    }

    #[test]
    fn save_at_is_atomic_upsert_and_deletes_removed_sessions() {
        let db = TmpDb::new();
        let docs = doc_with_two_sessions();
        docs.save_at(db.path()).unwrap();
        let keep_id = docs.sessions[1].id.clone();
        // Replace one session's content (upsert) and drop the other.
        let mut replacement = ChatSession::new(500);
        replacement.id = keep_id.clone();
        replacement.push("user", "the continued conversation", 500);
        let mut next = DocSessions::new(docs.doc_uuid.clone());
        next.sessions.push(replacement);
        next.save_at(db.path()).unwrap();

        let back = DocSessions::load_at_with_legacy(&docs.doc_uuid, db.path(), None).unwrap();
        assert_eq!(back.sessions.len(), 1, "removed session must be deleted");
        assert_eq!(back.sessions[0].id, keep_id);
        assert_eq!(
            back.sessions[0].turns[0].content,
            "the continued conversation"
        );
        // Cascade: the dropped session's messages are gone (its old content).
        let gone = DocSessions::search_at(&docs.doc_uuid, "office", db.path()).unwrap();
        assert!(
            gone.is_empty(),
            "messages of deleted session must cascade out"
        );
    }

    #[test]
    fn json_import_is_idempotent_and_preserves_turns_and_meta() {
        let db = TmpDb::new();
        // Stage a legacy JSON chats dir with one plaintext store.
        let legacy = std::env::temp_dir().join(format!(
            "ijc-legacy-{}-{}",
            std::process::id(),
            db.path().file_name().unwrap().to_string_lossy()
        ));
        std::fs::create_dir_all(&legacy).unwrap();
        let mut old = doc_with_two_sessions();
        old.set_meta(&old.sessions[0].id.clone(), "Imported", "Imported summary.");
        let json = serde_json::to_vec_pretty(&old).unwrap();
        std::fs::write(legacy.join(format!("{}.json", old.doc_uuid)), &json).unwrap();
        // A `.draft.json` that MUST be skipped by the import.
        std::fs::write(legacy.join(format!("{}.draft.json", old.doc_uuid)), b"{}").unwrap();

        // First load imports.
        let back =
            DocSessions::load_at_with_legacy(&old.doc_uuid, db.path(), Some(&legacy)).unwrap();
        assert_eq!(back.sessions.len(), 2);
        let imported = back
            .get(
                &old.sessions
                    .iter()
                    .find(|s| s.title == "Imported")
                    .unwrap()
                    .id,
            )
            .unwrap();
        assert_eq!(imported.summary, "Imported summary.");
        assert_eq!(imported.turns.len(), 2);

        // Second load must NOT double-import (idempotent via the marker).
        let again =
            DocSessions::load_at_with_legacy(&old.doc_uuid, db.path(), Some(&legacy)).unwrap();
        assert_eq!(
            again.sessions.len(),
            2,
            "re-import must not duplicate sessions"
        );

        // Legacy files are LEFT IN PLACE as a backup.
        assert!(legacy.join(format!("{}.json", old.doc_uuid)).exists());
        let _ = std::fs::remove_dir_all(&legacy);
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
        s.push(
            "assistant",
            "Drew the core and added a stair to level 2.",
            1,
        );
        s.derive_meta();
        assert_eq!(s.title, "make a five by five office core with a stair");
        assert!(
            s.summary.contains("office core"),
            "summary keeps the ask: {}",
            s.summary
        );
        assert!(
            s.summary.contains('→'),
            "summary joins ask→reply: {}",
            s.summary
        );
        assert!(
            s.summary.contains("stair"),
            "summary previews the reply: {}",
            s.summary
        );
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
        assert_eq!(
            docs.sessions[0].summary, "",
            "missing summary defaults to empty"
        );
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
        assert_eq!(
            docs.sessions[0].turns[0].content,
            "the continued conversation"
        );
        // The OTHER session is preserved.
        assert!(
            docs.sessions
                .iter()
                .any(|s| s.title.contains("curtain wall"))
        );
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
        let hits = docs.search_linear("façade");
        assert_eq!(hits.len(), 1);
        // Must not panic and must contain the match.
        assert!(hits[0].snippet.contains("façade"));
    }

    #[test]
    fn fts_snippet_stays_on_char_boundaries() {
        // Same multibyte content, but exercised through the DB/FTS path to prove
        // snippet_for is char-boundary safe on an FTS hit.
        let db = TmpDb::new();
        let mut docs = DocSessions::new("cccc1111-2222-3333-4444-555566667777".into());
        let mut s = ChatSession::new(7);
        s.push("user", "café — draw a façade wall then résumé the work", 7);
        docs.sessions.push(s);
        docs.save_at(db.path()).unwrap();
        let hits = DocSessions::search_at(&docs.doc_uuid, "façade", db.path()).unwrap();
        assert_eq!(hits.len(), 1);
        assert!(hits[0].snippet.contains("façade"));
    }
}
