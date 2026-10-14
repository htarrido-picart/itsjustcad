-- SPDX-License-Identifier: AGPL-3.0-only
-- Copyright © 2026 Hector Tarrido-Picart
--
-- SHARED SOURCE-OF-TRUTH chat/session schema for the embedded SQLite store.
-- Both the Rust desktop app (rusqlite, `bundled` SQLite) and the iOS app
-- (GRDB) open the SAME database file and MUST apply the SAME migrations, in
-- the SAME order, under the SAME identifiers. This file is that contract.
--
-- Migration model (mirrors GRDB's `DatabaseMigrator` EXACTLY):
--   * `grdb_migrations(identifier TEXT NOT NULL PRIMARY KEY)` records every
--     applied migration by a stable string identifier. GRDB uses this exact
--     table name and shape; do not rename it.
--   * Migrations are applied in a fixed order and each is recorded by inserting
--     its identifier. On iOS, register the identifiers below in the SAME order
--     with `migrator.registerMigration("v1_initial") { db in ... }`.
--   * The Rust runner splits this file on the `-- migration:<identifier>`
--     markers below, so each marked block is one GRDB migration. Keep the
--     markers and identifiers byte-for-byte in sync with the iOS registrations.
--
-- Dates: all timestamps are INTEGER unix epoch SECONDS (UTC). GRDB maps this
-- to `Date` via its epoch-INTEGER representation (store `date.timeIntervalSince1970`
-- as an Int). One encoding, both sides — do NOT use SQLite's julianday/text dates.
--
-- Encryption: the database is PLAINTEXT-at-rest for now (matches the app's
-- `chat_encryption` default of OFF). SQLCipher is the planned at-rest layer;
-- when adopted it is a localized change in `open_db` (see chat_db.rs) plus a
-- feature swap, with NO schema change. There is no field-level crypto.

-- migration:v1_initial

-- One named conversation. The app's session id is a uuid STRING; it is kept as
-- a UNIQUE TEXT column while the INTEGER rowid PK (`id`) is the GRDB-friendly
-- primary key and the FK target for messages.
CREATE TABLE sessions (
    id         INTEGER PRIMARY KEY,
    uuid       TEXT    UNIQUE NOT NULL,
    doc_uuid   TEXT    NOT NULL,
    title      TEXT    NOT NULL,
    summary    TEXT    NOT NULL DEFAULT '',
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);

CREATE INDEX idx_sessions_doc_uuid ON sessions(doc_uuid);
CREATE INDEX idx_sessions_uuid     ON sessions(uuid);

-- Turns of a conversation, ordered by `ord`. Deleting a session cascades to
-- its messages.
CREATE TABLE messages (
    id         INTEGER PRIMARY KEY,
    session_id INTEGER NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    ord        INTEGER NOT NULL,
    role       TEXT    NOT NULL,
    content    TEXT    NOT NULL,
    created_at INTEGER NOT NULL
);

CREATE INDEX idx_messages_session_ord ON messages(session_id, ord);

-- FTS5 external-content index over `messages.content` (the GRDB-recommended
-- external-content pattern: the FTS table stores no copy of the text, it points
-- back at `messages` via `content_rowid = messages.id`). Standard FTS5 only.
CREATE VIRTUAL TABLE messages_fts USING fts5(
    content,
    content='messages',
    content_rowid='id'
);

-- Standard external-content sync triggers: keep messages_fts in lockstep with
-- messages. 'delete' rows feed FTS the old value so its index stays consistent.
CREATE TRIGGER messages_ai AFTER INSERT ON messages BEGIN
    INSERT INTO messages_fts(rowid, content) VALUES (new.id, new.content);
END;

CREATE TRIGGER messages_ad AFTER DELETE ON messages BEGIN
    INSERT INTO messages_fts(messages_fts, rowid, content) VALUES ('delete', old.id, old.content);
END;

CREATE TRIGGER messages_au AFTER UPDATE ON messages BEGIN
    INSERT INTO messages_fts(messages_fts, rowid, content) VALUES ('delete', old.id, old.content);
    INSERT INTO messages_fts(rowid, content) VALUES (new.id, new.content);
END;
