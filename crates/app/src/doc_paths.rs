// SPDX-License-Identifier: AGPL-3.0-only
// Copyright © 2026 Hector Tarrido-Picart

//! Desktop-only `doc_uuid → file path` map, persisted as a small JSON object at
//! `~/.config/itsjustcad/doc_paths.json`. This lets "open this chat" reconstruct
//! the drawing by reopening the `.ijc` file that was last saved/opened for that
//! document uuid (its op-log replays the geometry), with a replay-the-chat
//! fallback when the uuid has no known file.
//!
//! This is NOT part of the shared #9 SQLite `sessions` schema / GRDB contract —
//! it is app-local metadata only, written privately (mode 0600 on unix).
//!
//! Semantics: a doc_uuid maps to exactly ONE path. Saving (or opening) a
//! document re-stamps the current uuid → that path, so the map always holds the
//! LAST-KNOWN path for a uuid (a doc saved to several paths resolves to the most
//! recent one).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// The on-disk map: document uuid → absolute file path (as a string).
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DocPaths {
    #[serde(default)]
    map: BTreeMap<String, String>,
}

impl DocPaths {
    /// Look up the last-known file path for a document uuid.
    pub fn get(&self, uuid: &str) -> Option<PathBuf> {
        self.map.get(uuid).map(PathBuf::from)
    }

    /// Record (or overwrite) the file path for a document uuid. Last write wins,
    /// so a uuid saved to several paths resolves to the most recent one.
    pub fn set(&mut self, uuid: &str, path: &Path) {
        self.map
            .insert(uuid.to_string(), path.to_string_lossy().into_owned());
    }

    /// Parse a map from JSON bytes, tolerating a missing/empty/corrupt file by
    /// returning an empty map (the feature degrades to the replay fallback).
    fn from_bytes(bytes: &[u8]) -> Self {
        serde_json::from_slice(bytes).unwrap_or_default()
    }

    /// Load the map from an explicit path (test seam). A missing or unreadable
    /// file yields an empty map.
    pub fn load_at(path: &Path) -> Self {
        match std::fs::read(path) {
            Ok(bytes) => Self::from_bytes(&bytes),
            Err(_) => Self::default(),
        }
    }

    /// Persist the map to an explicit path (test seam), creating the parent
    /// directory and writing privately (mode 0600 on unix).
    pub fn save_at(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_vec_pretty(self)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        crate::journal::write_private(path, &json)
    }
}

/// Default map path: `~/.config/itsjustcad/doc_paths.json`. `None` only when
/// there is no home directory.
pub fn default_path() -> Option<PathBuf> {
    Some(
        dirs::home_dir()?
            .join(".config")
            .join("itsjustcad")
            .join("doc_paths.json"),
    )
}

/// Load the map from the default path, returning an empty map when there is no
/// home directory or the file is absent/corrupt.
pub fn load() -> DocPaths {
    match default_path() {
        Some(p) => DocPaths::load_at(&p),
        None => DocPaths::default(),
    }
}

/// Record `uuid → path` into the default map (load-mutate-save). Best-effort:
/// a missing home dir or a write failure is silently ignored — the feature just
/// falls back to replay next time.
pub fn record(uuid: &str, path: &Path) {
    let Some(map_path) = default_path() else { return };
    let mut map = DocPaths::load_at(&map_path);
    map.set(uuid, path);
    let _ = map.save_at(&map_path);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "ijc-doc-paths-test-{}-{name}.json",
            std::process::id()
        ))
    }

    #[test]
    fn get_set_round_trips_in_memory() {
        let mut m = DocPaths::default();
        assert_eq!(m.get("u1"), None, "unknown uuid -> None");
        m.set("u1", Path::new("/tmp/a.itsjustcad.json"));
        assert_eq!(m.get("u1"), Some(PathBuf::from("/tmp/a.itsjustcad.json")));
        assert_eq!(m.get("u2"), None);
    }

    #[test]
    fn persist_round_trips_through_disk() {
        let path = temp_path("persist");
        let _ = std::fs::remove_file(&path);
        let mut m = DocPaths::load_at(&path);
        assert_eq!(m, DocPaths::default(), "missing file -> empty map");
        m.set("abc", Path::new("/docs/x.itsjustcad.json"));
        m.save_at(&path).unwrap();
        let reloaded = DocPaths::load_at(&path);
        assert_eq!(reloaded.get("abc"), Some(PathBuf::from("/docs/x.itsjustcad.json")));
        assert_eq!(reloaded.get("missing"), None);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn last_write_wins_for_a_uuid() {
        // A doc saved to two paths resolves to the most recent (last-known) one.
        let mut m = DocPaths::default();
        m.set("u", Path::new("/old/path.json"));
        m.set("u", Path::new("/new/path.json"));
        assert_eq!(m.get("u"), Some(PathBuf::from("/new/path.json")));
    }

    #[test]
    fn corrupt_file_yields_empty_map() {
        let path = temp_path("corrupt");
        std::fs::write(&path, b"not json at all {{{").unwrap();
        let m = DocPaths::load_at(&path);
        assert_eq!(m, DocPaths::default(), "corrupt file degrades to empty");
        let _ = std::fs::remove_file(&path);
    }

    #[cfg(unix)]
    #[test]
    fn saved_file_is_private_0600() {
        use std::os::unix::fs::PermissionsExt;
        let path = temp_path("perms");
        let _ = std::fs::remove_file(&path);
        let mut m = DocPaths::default();
        m.set("u", Path::new("/p.json"));
        m.save_at(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "doc_paths.json must not be world-readable");
        let _ = std::fs::remove_file(&path);
    }
}
