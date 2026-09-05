// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! Scoped deck **workdir**: a single user-granted folder the deck may list and
//! import files from — NOT the whole filesystem, NOT a shell.
//!
//! The deck (LLM) can drive only bounded verbs. `workdir <path>` grants a folder
//! (persisted to `~/.config/itsjustcad/workdir.txt`); `workdir` / `files` lists
//! the importable files inside it; and `import <name>` resolves a *bare name*
//! against the workdir. Every name is path-traversal guarded: a `..` segment, an
//! absolute path, or anything that would resolve outside the granted folder is
//! refused. This keeps the security invariant — the model gets file access only
//! within one folder a human explicitly chose, and only for import.

use std::path::{Path, PathBuf};

/// Extensions the substrate can import (kept in sync with the `import` dispatch
/// in `exec.rs`). Used to filter the workdir listing to files the deck can
/// actually act on.
pub const IMPORTABLE_EXTS: &[&str] = &[
    "dwg", "dxf", "obj", "stl", "gltf", "glb", "dae", "3dm", "step", "stp", "ifc", "epw",
    "geojson", "json", "las", "laz", "e57",
];

/// The file the granted workdir path is persisted in.
pub fn workdir_config_path() -> Option<PathBuf> {
    dirs_home().map(|h| h.join(".config").join("itsjustcad").join("workdir.txt"))
}

fn dirs_home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// The currently-granted workdir, if any (canonicalized on read so comparisons
/// are stable). `None` when unset or the stored path no longer exists.
pub fn get() -> Option<PathBuf> {
    let raw = std::fs::read_to_string(workdir_config_path()?).ok()?;
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    let p = PathBuf::from(raw);
    // Only report a workdir that still exists and is a directory.
    p.is_dir().then(|| p.canonicalize().unwrap_or(p))
}

/// Grant `path` as the workdir. It must exist and be a directory. Persists the
/// canonical path. Returns the canonical path on success.
pub fn set(path: &str) -> Result<PathBuf, String> {
    let p = PathBuf::from(path);
    if !p.is_dir() {
        return Err(format!("workdir '{path}' is not an existing directory"));
    }
    let canon = p.canonicalize().map_err(|e| format!("cannot resolve '{path}': {e}"))?;
    let cfg = workdir_config_path().ok_or("cannot locate config dir (no $HOME)")?;
    if let Some(parent) = cfg.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("cannot create config dir: {e}"))?;
    }
    std::fs::write(&cfg, canon.to_string_lossy().as_bytes())
        .map_err(|e| format!("cannot persist workdir: {e}"))?;
    Ok(canon)
}

/// List importable files (by name, sorted) in the granted workdir. Errors when
/// no workdir is set. Only top-level files with an [`IMPORTABLE_EXTS`] extension
/// are listed — no recursion, no directories, no dotfiles.
pub fn list_importable() -> Result<(PathBuf, Vec<String>), String> {
    let dir = get().ok_or("no workdir set — grant one with 'workdir <path>'")?;
    list_importable_in(&dir).map(|names| (dir, names))
}

/// Pure lister: importable file names in `dir` (testable without the config).
pub fn list_importable_in(dir: &Path) -> Result<Vec<String>, String> {
    let mut names = Vec::new();
    let entries = std::fs::read_dir(dir).map_err(|e| format!("cannot read workdir: {e}"))?;
    for entry in entries.flatten() {
        if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        let ext = name.rsplit('.').next().unwrap_or_default().to_ascii_lowercase();
        if IMPORTABLE_EXTS.contains(&ext.as_str()) {
            names.push(name);
        }
    }
    names.sort();
    Ok(names)
}

/// Resolve a *bare file name* against the granted workdir, refusing any name
/// that escapes it. Returns the absolute path to an existing file inside the
/// workdir, or a clear error.
///
/// SECURITY: rejects absolute paths, any `..` component, any embedded path
/// separator (the deck references files by NAME, not by path), and — as a
/// belt-and-suspenders check — verifies the canonical result still lives under
/// the canonical workdir.
pub fn resolve_within(name: &str) -> Result<PathBuf, String> {
    let dir = get().ok_or("no workdir set — grant one with 'workdir <path>'")?;
    resolve_within_dir(&dir, name)
}

/// Pure core of [`resolve_within`] against an explicit workdir — testable.
pub fn resolve_within_dir(dir: &Path, name: &str) -> Result<PathBuf, String> {
    let candidate = Path::new(name);
    // A bare name only: no absolute paths, no separators, no traversal.
    if candidate.is_absolute() {
        return Err(format!("'{name}' must be a file name inside the workdir, not an absolute path"));
    }
    if name.contains('/') || name.contains('\\') {
        return Err(format!("'{name}' must be a bare file name (no path separators) inside the workdir"));
    }
    if candidate.components().any(|c| matches!(c, std::path::Component::ParentDir)) {
        return Err(format!("'{name}' escapes the workdir ('..' not allowed)"));
    }
    let full = dir.join(name);
    if !full.is_file() {
        return Err(format!("'{name}' is not a file in the workdir"));
    }
    // Belt and suspenders: the canonical path must still be under the workdir
    // (defends against symlinks pointing outside).
    let canon = full.canonicalize().map_err(|e| format!("cannot resolve '{name}': {e}"))?;
    let dir_canon = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
    if !canon.starts_with(&dir_canon) {
        return Err(format!("'{name}' resolves outside the workdir (symlink escape refused)"));
    }
    Ok(canon)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tmp(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ijc_wd_{tag}_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn lists_only_importable_top_level_files() {
        let dir = tmp("list");
        fs::write(dir.join("site.dxf"), b"x").unwrap();
        fs::write(dir.join("plan.dwg"), b"x").unwrap();
        fs::write(dir.join("notes.txt"), b"x").unwrap(); // not importable
        fs::write(dir.join(".hidden.dxf"), b"x").unwrap(); // dotfile skipped
        fs::create_dir(dir.join("sub")).unwrap(); // dir skipped
        let names = list_importable_in(&dir).unwrap();
        assert_eq!(names, vec!["plan.dwg".to_string(), "site.dxf".to_string()]);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn resolves_a_bare_name_inside_the_workdir() {
        let dir = tmp("resolve");
        fs::write(dir.join("model.dxf"), b"x").unwrap();
        let got = resolve_within_dir(&dir, "model.dxf").unwrap();
        assert!(got.ends_with("model.dxf"));
        assert!(got.starts_with(dir.canonicalize().unwrap()));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn rejects_parent_dir_traversal() {
        let dir = tmp("traverse");
        let err = resolve_within_dir(&dir, "../etc/passwd").unwrap_err();
        assert!(err.contains("path separators") || err.contains("escapes"), "{err}");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn rejects_absolute_path() {
        let dir = tmp("abs");
        let err = resolve_within_dir(&dir, "/etc/passwd").unwrap_err();
        assert!(err.contains("absolute") || err.contains("separators"), "{err}");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn rejects_bare_dotdot_component() {
        let dir = tmp("dotdot");
        // No slash, but a lone ".." — still traversal.
        let err = resolve_within_dir(&dir, "..").unwrap_err();
        assert!(err.contains("escapes") || err.contains("not a file"), "{err}");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn missing_file_in_workdir_is_a_clear_error() {
        let dir = tmp("missing");
        let err = resolve_within_dir(&dir, "nope.dxf").unwrap_err();
        assert!(err.contains("not a file in the workdir"), "{err}");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn set_rejects_non_directory() {
        let dir = tmp("setbad");
        let file = dir.join("f.txt");
        fs::write(&file, b"x").unwrap();
        assert!(set(file.to_str().unwrap()).is_err());
        fs::remove_dir_all(&dir).unwrap();
    }
}
