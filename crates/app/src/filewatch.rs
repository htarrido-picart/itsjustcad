// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! External-change watching for the open document file.
//!
//! Cloud-synced folders (Dropbox, iCloud Drive, network shares) can rewrite
//! the file under the app. We snapshot `(mtime, len)` at open/save and poll it
//! cheaply on a throttle; a divergence raises [`ExternalChange`] so the app
//! can offer a reload instead of silently clobbering the newer file on the
//! next save. Sibling *conflict copies* ("… (conflicted copy …)", "name 2.…")
//! left behind by sync services are detected at open/save time and surfaced.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

/// What happened to the watched file outside this app.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExternalChange {
    /// mtime or size diverged from our last save/open snapshot.
    Modified,
    /// The file is gone (moved/deleted/renamed by another process).
    Deleted,
}

/// Snapshot of the open document file, polled on a throttle.
pub struct FileWatch {
    path: PathBuf,
    mtime: Option<SystemTime>,
    len: u64,
    last_poll: Option<Instant>,
}

impl FileWatch {
    /// Snapshot `path` as the known-good on-disk state (call at open/save).
    pub fn new(path: &Path) -> Self {
        let (mtime, len) = stat(path);
        Self { path: path.to_path_buf(), mtime, len, last_poll: None }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Re-snapshot the current on-disk state (call after "Keep mine" so the
    /// same change is not re-reported every poll).
    pub fn refresh(&mut self) {
        let (mtime, len) = stat(&self.path);
        self.mtime = mtime;
        self.len = len;
    }

    /// Throttled check: at most one `stat` per `interval`. Returns `Some`
    /// only when the on-disk state diverges from the snapshot.
    pub fn poll(&mut self, interval: Duration) -> Option<ExternalChange> {
        let now = Instant::now();
        if let Some(last) = self.last_poll
            && now.duration_since(last) < interval
        {
            return None;
        }
        self.last_poll = Some(now);
        self.check()
    }

    /// Unthrottled comparison of the on-disk state to the snapshot.
    pub fn check(&self) -> Option<ExternalChange> {
        let (mtime, len) = stat(&self.path);
        if mtime.is_none() && self.mtime.is_some() {
            return Some(ExternalChange::Deleted);
        }
        (mtime != self.mtime || len != self.len).then_some(ExternalChange::Modified)
    }
}

fn stat(path: &Path) -> (Option<SystemTime>, u64) {
    match std::fs::metadata(path) {
        Ok(m) => (m.modified().ok(), m.len()),
        Err(_) => (None, 0),
    }
}

/// Split a file name at its FIRST dot so compound extensions survive:
/// `"model.itsjustcad.json"` → `("model", ".itsjustcad.json")`.
fn split_name(name: &str) -> (&str, &str) {
    match name.find('.') {
        Some(i) if i > 0 => name.split_at(i),
        _ => (name, ""),
    }
}

/// Whether `candidate` looks like a sync-service conflict copy of `original`
/// (both bare file names). Recognized shapes, same extension chain required:
///
/// - Dropbox: `model (<host>'s conflicted copy <date>).itsjustcad.json` —
///   anything between base and extension containing "conflicted copy".
/// - iCloud / generic dedup: `model 2.itsjustcad.json` (base + space + digits).
pub fn is_conflict_sibling(original: &str, candidate: &str) -> bool {
    if candidate == original {
        return false;
    }
    let (base, ext) = split_name(original);
    let Some(rest) = candidate.strip_prefix(base) else {
        return false;
    };
    let Some(mid) = rest.strip_suffix(ext) else {
        return false;
    };
    // Dropbox-style: " (... conflicted copy ...)"
    if mid.to_ascii_lowercase().contains("conflicted copy") {
        return true;
    }
    // Numbered duplicate: " 2", " 3", …
    mid.strip_prefix(' ')
        .is_some_and(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()))
}

/// Conflict copies of `path` sitting next to it, sorted by name.
pub fn conflict_siblings(path: &Path) -> Vec<PathBuf> {
    let (Some(dir), Some(name)) = (path.parent(), path.file_name().and_then(|n| n.to_str()))
    else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<PathBuf> = entries
        .flatten()
        .filter(|e| {
            e.file_name()
                .to_str()
                .is_some_and(|c| is_conflict_sibling(name, c))
        })
        .map(|e| e.path())
        .collect();
    out.sort();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "ijc-filewatch-{tag}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn unchanged_file_reports_nothing() {
        let d = tmpdir("same");
        let f = d.join("doc.itsjustcad.json");
        std::fs::write(&f, b"{}").unwrap();
        let w = FileWatch::new(&f);
        assert_eq!(w.check(), None);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn size_change_is_modified_and_refresh_clears() {
        let d = tmpdir("mod");
        let f = d.join("doc.itsjustcad.json");
        std::fs::write(&f, b"{}").unwrap();
        let mut w = FileWatch::new(&f);
        std::fs::write(&f, b"{\"ops\":[]}").unwrap(); // longer content
        assert_eq!(w.check(), Some(ExternalChange::Modified));
        w.refresh();
        assert_eq!(w.check(), None);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn deleted_file_is_reported() {
        let d = tmpdir("del");
        let f = d.join("doc.itsjustcad.json");
        std::fs::write(&f, b"{}").unwrap();
        let w = FileWatch::new(&f);
        std::fs::remove_file(&f).unwrap();
        assert_eq!(w.check(), Some(ExternalChange::Deleted));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn poll_is_throttled() {
        let d = tmpdir("throttle");
        let f = d.join("doc.itsjustcad.json");
        std::fs::write(&f, b"{}").unwrap();
        let mut w = FileWatch::new(&f);
        // First poll stats; second within the window does not (even though the
        // file changed in between, the throttle suppresses the stat).
        assert_eq!(w.poll(Duration::from_secs(3600)), None);
        std::fs::write(&f, b"changed!!").unwrap();
        assert_eq!(w.poll(Duration::from_secs(3600)), None);
        // Zero interval bypasses the throttle.
        assert_eq!(w.poll(Duration::ZERO), Some(ExternalChange::Modified));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn conflict_name_matching() {
        let orig = "model.itsjustcad.json";
        assert!(is_conflict_sibling(
            orig,
            "model (Hector's MacBook Pro's conflicted copy 2026-09-01).itsjustcad.json"
        ));
        assert!(is_conflict_sibling(orig, "model (conflicted copy).itsjustcad.json"));
        assert!(is_conflict_sibling(orig, "model 2.itsjustcad.json"));
        assert!(is_conflict_sibling(orig, "model 12.itsjustcad.json"));
        // Not conflicts:
        assert!(!is_conflict_sibling(orig, orig)); // itself
        assert!(!is_conflict_sibling(orig, "model.json")); // different ext chain
        assert!(!is_conflict_sibling(orig, "model-b.itsjustcad.json")); // plain sibling
        assert!(!is_conflict_sibling(orig, "model two.itsjustcad.json")); // not digits
        assert!(!is_conflict_sibling(orig, "other (conflicted copy).itsjustcad.json"));
    }

    #[test]
    fn conflict_siblings_scans_directory() {
        let d = tmpdir("scan");
        let f = d.join("doc.itsjustcad.json");
        std::fs::write(&f, b"{}").unwrap();
        std::fs::write(d.join("doc 2.itsjustcad.json"), b"{}").unwrap();
        std::fs::write(
            d.join("doc (A's conflicted copy 2026-01-01).itsjustcad.json"),
            b"{}",
        )
        .unwrap();
        std::fs::write(d.join("unrelated.itsjustcad.json"), b"{}").unwrap();
        let got = conflict_siblings(&f);
        assert_eq!(got.len(), 2, "{got:?}");
        let _ = std::fs::remove_dir_all(&d);
    }
}
