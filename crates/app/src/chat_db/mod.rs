// SPDX-License-Identifier: AGPL-3.0-only
// Copyright © 2026 Hector Tarrido-Picart

//! Embedded SQLite backend for the chat/session store.
//!
//! This is the on-disk home for chats, replacing the per-document JSON files
//! that `chat_store.rs` used to write. There is ONE database for the whole app
//! (`~/.config/itsjustcad/itsjustcad.db`, private 0600), keyed internally by
//! `doc_uuid`. The schema is a SHARED CONTRACT with the iOS GRDB app — see
//! [`schema.sql`](./schema.sql) — so both sides open the same file and apply
//! the same migrations in the same order.
//!
//! Layering:
//! * [`open_db`] is the SINGLE connection-open seam (SQLCipher hook lives here).
//! * [`run_migrations`] applies the `schema.sql` migrations idempotently,
//!   recording each in `grdb_migrations` exactly as GRDB's `DatabaseMigrator`
//!   does.
//! * `chat_store.rs` re-points its public API (`DocSessions::load/save/search/
//!   upsert/set_meta/...`) onto these primitives, so call sites are unchanged.
//!
//! Dates are INTEGER unix epoch seconds throughout (matches `ChatSession.updated`
//! and GRDB's epoch-INTEGER `Date` mapping).
//!
//! Encryption is OFF (plaintext-at-rest), matching the app default. The old
//! `chat_crypto` field encryption is NOT layered on the DB; SQLCipher is the
//! planned replacement (a localized change in [`open_db`]). The legacy
//! encrypted/plaintext JSON files are left in place as a read-only backup.

use std::path::{Path, PathBuf};

use rusqlite::{Connection, OpenFlags};

/// The embedded `schema.sql`, split into ordered GRDB migrations at build time.
const SCHEMA_SQL: &str = include_str!("schema.sql");

/// Ordered migration identifiers. These MUST match the iOS GRDB
/// `registerMigration` calls byte-for-byte and in this order. Each entry is the
/// identifier that follows a `-- migration:<identifier>` marker in `schema.sql`.
pub const MIGRATIONS: &[&str] = &["v1_initial"];

/// App-wide database path: `~/.config/itsjustcad/itsjustcad.db`.
pub fn db_path() -> Option<PathBuf> {
    Some(
        dirs::home_dir()?
            .join(".config")
            .join("itsjustcad")
            .join("itsjustcad.db"),
    )
}

/// THE single connection-open seam.
///
/// Opens (creating if needed) the SQLite database at `path`, enables foreign
/// keys (so `ON DELETE CASCADE` fires), and sets WAL journaling for concurrent
/// reads. On unix the freshly created file is chmod'd to 0600 to match the
/// app's private-file convention.
///
/// SQLCipher-ready seam: to encrypt the database at rest later, this is the
/// ONE place to change — swap the `rusqlite` feature `bundled` →
/// `bundled-sqlcipher` in Cargo.toml and, immediately after opening, run
/// `conn.pragma_update(None, "key", &passphrase)?;` (ideally the key sourced
/// from the OS keychain via `chat_crypto::KeyStore`) BEFORE any other
/// statement. No schema change is required; GRDB opens SQLCipher the same way
/// with its `prepare`/`passphrase` configuration. Encryption is intentionally
/// NOT wired now to avoid the OpenSSL/CI dependency.
pub fn open_db(path: &Path) -> rusqlite::Result<Connection> {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let existed = path.exists();
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_CREATE
            | OpenFlags::SQLITE_OPEN_URI,
    )?;
    // --- SQLCipher key would go HERE (see doc comment above), before any SQL. ---
    if !existed {
        set_private_perms(path);
    }
    conn.pragma_update(None, "foreign_keys", true)?;
    // WAL survives crashes better and lets the iOS reader coexist; harmless for
    // a single-writer desktop app. Ignored on in-memory DBs.
    let _ = conn.pragma_update(None, "journal_mode", "WAL");
    Ok(conn)
}

/// Set 0600 on a freshly created DB file (unix), matching `journal::write_private`.
fn set_private_perms(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
}

/// Parse `schema.sql` into `(identifier, sql)` migration blocks, split on the
/// `-- migration:<identifier>` markers. Validates that the markers match
/// [`MIGRATIONS`] exactly (same identifiers, same order) so a schema edit that
/// drifts from the registered list fails loudly instead of silently skipping.
fn parse_migrations() -> Vec<(&'static str, String)> {
    const MARKER: &str = "-- migration:";
    let mut blocks: Vec<(&'static str, String)> = Vec::new();
    let mut current: Option<(usize, String)> = None;
    for line in SCHEMA_SQL.lines() {
        if let Some(rest) = line.trim().strip_prefix(MARKER) {
            let ident = rest.trim();
            // Resolve to the 'static identifier from MIGRATIONS for cheap compare.
            let idx = blocks.len();
            let known = MIGRATIONS
                .get(idx)
                .copied()
                .unwrap_or_else(|| panic!("schema.sql has more migrations than MIGRATIONS lists"));
            assert_eq!(
                known, ident,
                "schema.sql migration marker '{ident}' at position {idx} does not match MIGRATIONS[{idx}] = '{known}'"
            );
            if let Some((i, body)) = current.take() {
                blocks.push((MIGRATIONS[i], body));
            }
            current = Some((idx, String::new()));
        } else if let Some((_, body)) = current.as_mut() {
            body.push_str(line);
            body.push('\n');
        }
    }
    if let Some((i, body)) = current.take() {
        blocks.push((MIGRATIONS[i], body));
    }
    assert_eq!(
        blocks.len(),
        MIGRATIONS.len(),
        "schema.sql defines {} migrations but MIGRATIONS lists {}",
        blocks.len(),
        MIGRATIONS.len()
    );
    blocks
}

/// Apply every not-yet-applied migration in order, recording each in
/// `grdb_migrations`. Idempotent: running twice applies nothing the second
/// time. The whole run is one transaction so a mid-migration failure rolls back
/// cleanly (no half-applied schema).
pub fn run_migrations(conn: &mut Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS grdb_migrations (identifier TEXT NOT NULL PRIMARY KEY);",
    )?;
    let tx = conn.transaction()?;
    for (ident, sql) in parse_migrations() {
        let applied: bool = tx
            .query_row(
                "SELECT 1 FROM grdb_migrations WHERE identifier = ?1",
                [ident],
                |_| Ok(true),
            )
            .unwrap_or(false);
        if applied {
            continue;
        }
        tx.execute_batch(&sql)?;
        tx.execute(
            "INSERT INTO grdb_migrations(identifier) VALUES (?1)",
            [ident],
        )?;
    }
    tx.commit()
}

/// Open the app database and ensure migrations are applied. Convenience for the
/// store façade; tests use [`open_db`] + [`run_migrations`] with a temp path.
#[allow(dead_code)] // public seam; store opens per-call via open_db today.
pub fn open_app_db() -> rusqlite::Result<Connection> {
    let path = db_path().ok_or_else(|| {
        rusqlite::Error::InvalidPath(PathBuf::from("<no home dir for itsjustcad.db>"))
    })?;
    let mut conn = open_db(&path)?;
    run_migrations(&mut conn)?;
    Ok(conn)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_db() -> Connection {
        // Hermetic, parallel-safe: unique temp path per test (pid + counter),
        // matching the workdir-test convention. Never touches ~/.config.
        use std::sync::atomic::{AtomicU64, Ordering};
        static CTR: AtomicU64 = AtomicU64::new(0);
        let n = CTR.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("ijc-chatdb-{}-{}.db", std::process::id(), n));
        let _ = std::fs::remove_file(&path);
        let mut c = open_db(&path).unwrap();
        run_migrations(&mut c).unwrap();
        c
    }

    #[test]
    fn migrations_match_schema_markers() {
        // parse_migrations asserts marker/MIGRATIONS agreement; this makes the
        // check an explicit test too.
        let blocks = parse_migrations();
        assert_eq!(blocks.len(), MIGRATIONS.len());
        assert_eq!(blocks[0].0, "v1_initial");
        assert!(blocks[0].1.contains("CREATE TABLE sessions"));
    }

    #[test]
    fn migration_runs_idempotently() {
        let mut c = temp_db();
        // Run again: no error, and grdb_migrations has exactly one row per id.
        run_migrations(&mut c).unwrap();
        run_migrations(&mut c).unwrap();
        let rows: i64 = c
            .query_row("SELECT COUNT(*) FROM grdb_migrations", [], |r| r.get(0))
            .unwrap();
        assert_eq!(rows, MIGRATIONS.len() as i64);
        // Schema objects exist.
        let tables: i64 = c
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name IN ('sessions','messages')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(tables, 2);
    }

    #[test]
    fn foreign_keys_cascade_delete_messages() {
        let c = temp_db();
        c.execute(
            "INSERT INTO sessions(uuid, doc_uuid, title, summary, created_at, updated_at) \
             VALUES ('s1','d1','t','',1,1)",
            [],
        )
        .unwrap();
        let sid = c.last_insert_rowid();
        c.execute(
            "INSERT INTO messages(session_id, ord, role, content, created_at) \
             VALUES (?1, 0, 'user', 'hello world', 1)",
            [sid],
        )
        .unwrap();
        c.execute("DELETE FROM sessions WHERE id = ?1", [sid])
            .unwrap();
        let msgs: i64 = c
            .query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0))
            .unwrap();
        assert_eq!(msgs, 0, "messages must cascade-delete with their session");
        // FTS index also cleared via the delete trigger.
        let fts: i64 = c
            .query_row(
                "SELECT COUNT(*) FROM messages_fts WHERE messages_fts MATCH 'hello'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(fts, 0, "FTS row must be removed with the message");
    }

    #[test]
    fn fts_indexes_inserted_content() {
        let c = temp_db();
        c.execute(
            "INSERT INTO sessions(uuid, doc_uuid, title, summary, created_at, updated_at) \
             VALUES ('s1','d1','t','',1,1)",
            [],
        )
        .unwrap();
        let sid = c.last_insert_rowid();
        c.execute(
            "INSERT INTO messages(session_id, ord, role, content, created_at) \
             VALUES (?1, 0, 'user', 'a curtain wall facade', 1)",
            [sid],
        )
        .unwrap();
        let hits: i64 = c
            .query_row(
                "SELECT COUNT(*) FROM messages_fts WHERE messages_fts MATCH 'facade'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(hits, 1);
    }
}
