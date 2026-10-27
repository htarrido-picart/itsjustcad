// SPDX-License-Identifier: AGPL-3.0-only
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

use crate::command::Command;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// Process-global in-memory workdir grant. Populated by [`set`] the moment a
/// workdir is granted, and consulted FIRST by [`get`]. This is what makes the
/// grant work on hosts where the on-disk config (`$HOME/.config/itsjustcad/
/// workdir.txt`) is not writable — notably the iOS app sandbox, whose data
/// container root does not accept a new top-level `.config` directory. On the
/// desktop it is simply a redundant cache of the same path the file holds, so
/// behaviour (including cross-run persistence via the file) is unchanged.
#[cfg(not(test))]
fn mem_workdir() -> &'static Mutex<Option<PathBuf>> {
    static CELL: std::sync::OnceLock<Mutex<Option<PathBuf>>> = std::sync::OnceLock::new();
    CELL.get_or_init(|| Mutex::new(None))
}

/// Unit tests run in parallel threads of one process, so a process-global grant
/// set by one test would leak into every other. Under `cfg(test)` each test
/// thread gets its own cell instead.
#[cfg(test)]
fn mem_workdir() -> &'static Mutex<Option<PathBuf>> {
    thread_local! {
        static CELL: &'static Mutex<Option<PathBuf>> = Box::leak(Box::new(Mutex::new(None)));
    }
    CELL.with(|c| *c)
}

/// Extensions the substrate can import (kept in sync with the `import` dispatch
/// in `exec.rs`). Used to filter the workdir listing to files the deck can
/// actually act on.
pub const IMPORTABLE_EXTS: &[&str] = &[
    "dwg", "dxf", "obj", "stl", "gltf", "glb", "dae", "3dm", "step", "stp", "ifc", "epw",
    "geojson", "json", "las", "laz", "e57",
];

/// The file the granted workdir path is persisted in.
pub fn workdir_config_path() -> Option<PathBuf> {
    crate::config_dir().map(|d| d.join("workdir.txt"))
}

/// The currently-granted workdir, if any (canonicalized on read so comparisons
/// are stable). `None` when unset or the stored path no longer exists.
pub fn get() -> Option<PathBuf> {
    // In-memory grant (set this process) wins — authoritative where the on-disk
    // config can't be written (iOS). Still revalidated as an existing directory.
    if let Some(p) = mem_workdir().lock().ok().and_then(|g| g.clone())
        && p.is_dir()
    {
        return Some(p.canonicalize().unwrap_or(p));
    }
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
    // Record the grant in memory FIRST so it is live for this process even if the
    // on-disk persistence below fails (iOS: the sandbox has no writable $HOME
    // config dir). This is what [`get`] consults first.
    if let Ok(mut g) = mem_workdir().lock() {
        *g = Some(canon.clone());
    }
    // Best-effort cross-run persistence to the config file. On hosts where this
    // can't be written (no $HOME, read-only config dir) the grant still stands
    // for the running process via the in-memory cell above — so a write failure
    // here is non-fatal rather than failing the whole grant.
    if let Some(cfg) = workdir_config_path() {
        let persisted = cfg
            .parent()
            .map(std::fs::create_dir_all)
            .transpose()
            .and_then(|_| std::fs::write(&cfg, canon.to_string_lossy().as_bytes()));
        if let Err(e) = persisted {
            eprintln!("[workdir] in-memory grant set; could not persist to {}: {e}", cfg.display());
        }
    }
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

/// Resolve an `underlay` target against the granted workdir, if it is a bare
/// name inside it. Returns the canonical absolute path when `name` names a file
/// that lives inside the granted workdir (no `..`, no separators, no absolute
/// escape, no symlink escape — the same guard as [`resolve_within`]). Returns
/// `None` when there is no workdir OR the name is anything other than a bare
/// in-workdir file (an absolute path, a traversal attempt, or a miss). This is
/// the single source of truth both the gate and the read-time open resolution
/// ([`resolve_for_open`]) consult, so the file the gate *authorizes* is exactly
/// the file a consumer eventually *opens*.
pub fn resolve_underlay(name: &str) -> Option<PathBuf> {
    let dir = get()?;
    resolve_within_dir(&dir, name).ok()
}

/// Read-time twin of the write-time gate: map a STORED underlay path to the real
/// file to open. A bare name (no separators, not absolute) that resolves inside
/// the granted workdir becomes that in-workdir absolute path; anything else — an
/// absolute/relative path the user typed, or a bare name with no workdir / no
/// match — is returned unchanged.
///
/// Why this exists: the stored command and document keep the portable bare NAME
/// (baking the device's absolute sandbox path into the op-log would leak the
/// filesystem layout and break portability across devices). Resolution to the
/// concrete on-device path happens only here, when a consumer actually opens the
/// file. Best-effort and non-fatal — an unresolvable name is handed back as-is so
/// the caller surfaces the usual "image not shown" warning rather than aborting
/// (critical for op-log replay, which must load even when the workdir/file is
/// gone). It reuses [`resolve_underlay`], so it authorizes exactly what the gate
/// authorized — no CWD shadow, no symlink escape.
pub fn resolve_for_open(path: &str) -> String {
    match get() {
        Some(dir) => resolve_for_open_in(&dir, path),
        None => path.to_string(),
    }
}

/// Pure core of [`resolve_for_open`] against an explicit workdir — testable
/// without the persisted config. A bare name that resolves to a file inside
/// `dir` becomes that canonical path; everything else (absolute/relative paths,
/// or a bare name with no in-workdir match) is returned unchanged.
pub fn resolve_for_open_in(dir: &Path, path: &str) -> String {
    let is_bare = !Path::new(path).is_absolute() && !path.contains('/') && !path.contains('\\');
    if is_bare && let Ok(p) = resolve_within_dir(dir, path) {
        return p.to_string_lossy().into_owned();
    }
    path.to_string()
}

/// SECURITY GATE: the ONLY widening of the iOS side-effect refusal. Returns
/// `true` for exactly one case — an [`Command::Underlay`] whose `path` resolves
/// to a file INSIDE the user-granted workdir (canonicalized, traversal- and
/// symlink-guarded via [`resolve_within_dir`]). Every other command, a missing
/// workdir, an absolute/`..`/separator path, or a path that canonicalizes
/// outside the workdir returns `false`, so Import/Export/Print and any
/// underlay-of-an-outside-file stay refused exactly as before.
pub fn allows_underlay(cmd: &Command) -> bool {
    match get() {
        Some(dir) => allows_underlay_in(&dir, cmd),
        None => false,
    }
}

/// Pure core of [`allows_underlay`] against an explicit workdir — testable
/// without touching the persisted config. True ONLY for an [`Command::Underlay`]
/// whose `path` resolves to a file inside `dir` (traversal/symlink-guarded).
pub fn allows_underlay_in(dir: &Path, cmd: &Command) -> bool {
    match cmd {
        Command::Underlay { path, .. } => resolve_within_dir(dir, path).is_ok(),
        _ => false,
    }
}

/// SECURITY GATE: the second (and only other) widening of the iOS side-effect
/// refusal. Allows [`Command::Trace`] when a workdir is granted. Trace takes NO
/// path from the model — it re-reads the document's existing underlay, which
/// could only have been set through the already-gated [`allows_underlay`] path
/// (or typed by a human). So no new filesystem reach is opened: the model can
/// vectorize the image it was already permitted to place, nothing else. Refused
/// when no workdir is set (symmetric with `allows_underlay`).
pub fn allows_trace(cmd: &Command) -> bool {
    matches!(cmd, Command::Trace { .. }) && get().is_some()
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

    // --- scoped underlay gate (allows_underlay_in) ---

    use crate::command::Command;
    use glam::DVec3;

    fn underlay(path: &str) -> Command {
        Command::Underlay { path: path.into(), corner: None, width: None, height: None }
    }

    #[test]
    fn allows_underlay_for_file_inside_the_workdir() {
        let dir = tmp("ul_ok");
        fs::write(dir.join("attachment-1.jpg"), b"x").unwrap();
        assert!(allows_underlay_in(&dir, &underlay("attachment-1.jpg")));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn rejects_underlay_escaping_the_workdir() {
        let dir = tmp("ul_escape");
        // '..' traversal, a separator'd path, and an absolute path are all refused.
        assert!(!allows_underlay_in(&dir, &underlay("../etc/passwd")));
        assert!(!allows_underlay_in(&dir, &underlay("sub/../../../etc/passwd")));
        assert!(!allows_underlay_in(&dir, &underlay("/etc/passwd")));
        // A bare name that doesn't exist in the workdir is also refused.
        assert!(!allows_underlay_in(&dir, &underlay("nope.jpg")));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn rejects_underlay_through_a_symlink_escaping_the_workdir() {
        // The symlink-escape guard was described but never exercised. Plant a
        // real symlink inside the workdir pointing at a file OUTSIDE it: the
        // bare name exists and is a file, but its canonical path leaves the
        // workdir, so both the resolver and the gate must refuse it.
        let dir = tmp("ul_symlink");
        let outside = tmp("ul_symlink_outside");
        fs::write(outside.join("secret.jpg"), b"x").unwrap();
        std::os::unix::fs::symlink(outside.join("secret.jpg"), dir.join("link.jpg")).unwrap();
        assert!(dir.join("link.jpg").is_file(), "symlink should resolve to a file");
        assert!(resolve_within_dir(&dir, "link.jpg").is_err(), "symlink escape must be refused");
        assert!(!allows_underlay_in(&dir, &underlay("link.jpg")));
        fs::remove_dir_all(&dir).unwrap();
        fs::remove_dir_all(&outside).unwrap();
    }

    #[test]
    fn resolve_is_anchored_to_the_workdir_not_a_same_named_sibling() {
        // Regression for the gate/exec divergence: resolution is anchored to the
        // workdir and never selects a same-named file living elsewhere. The gate
        // authorizes `<workdir>/a.jpg`, so exec (which calls this resolver for a
        // bare name) must place exactly that — not a collision reachable by some
        // other path. Here an identically-named file exists in a sibling dir;
        // the resolved path must be the workdir's copy.
        let dir = tmp("ul_anchor");
        let sibling = tmp("ul_anchor_sibling");
        fs::write(dir.join("a.jpg"), b"in-workdir").unwrap();
        fs::write(sibling.join("a.jpg"), b"elsewhere").unwrap();
        let resolved = resolve_within_dir(&dir, "a.jpg").expect("in-workdir file resolves");
        assert_eq!(resolved, dir.canonicalize().unwrap().join("a.jpg"));
        assert!(!resolved.starts_with(sibling.canonicalize().unwrap()));
        fs::remove_dir_all(&dir).unwrap();
        fs::remove_dir_all(&sibling).unwrap();
    }

    #[test]
    fn resolve_for_open_maps_bare_name_to_workdir_but_passes_others_through() {
        let dir = tmp("ul_open");
        fs::write(dir.join("a.jpg"), b"x").unwrap();
        let canon = dir.canonicalize().unwrap();
        // A bare, in-workdir name resolves to its absolute in-workdir path.
        assert_eq!(resolve_for_open_in(&dir, "a.jpg"), canon.join("a.jpg").to_string_lossy());
        // Anything that isn't a bare in-workdir hit is returned verbatim, so the
        // caller opens exactly what was stored (and surfaces the usual warning if
        // it is missing): a bare miss, a traversal attempt, an absolute path.
        assert_eq!(resolve_for_open_in(&dir, "missing.jpg"), "missing.jpg");
        assert_eq!(resolve_for_open_in(&dir, "../a.jpg"), "../a.jpg");
        assert_eq!(resolve_for_open_in(&dir, "/etc/passwd"), "/etc/passwd");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn rejects_import_and_export_even_inside_the_workdir() {
        let dir = tmp("ul_notunderlay");
        fs::write(dir.join("site.dxf"), b"x").unwrap();
        // Not an Underlay → never allowed by the gate exception, even for a
        // file that genuinely lives in the workdir.
        assert!(!allows_underlay_in(&dir, &Command::Import { path: "site.dxf".into() }));
        assert!(!allows_underlay_in(&dir, &Command::Export { path: "out.dxf".into(), cut: None }));
        assert!(!allows_underlay_in(
            &dir,
            &Command::Print { sheet: "S1".into(), path: "out.pdf".into() }
        ));
        // A pure geometry command is likewise not an underlay grant.
        assert!(!allows_underlay_in(
            &dir,
            &Command::Box { id: None, corner: DVec3::ZERO, size: DVec3::ONE }
        ));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn allows_underlay_is_false_when_no_workdir_set() {
        // The global entry point: with no persisted workdir, nothing is allowed.
        // (We don't mutate the real config here; `get()` returning None is the
        // common CI case. If a workdir IS configured, this asserts the command
        // is still rejected because the file won't exist in it.)
        assert!(!allows_underlay(&underlay("definitely-not-a-real-file-xyz.jpg")));
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
