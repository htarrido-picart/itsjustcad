// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! Binary resolution for spawned CLIs (currently the `claude` Code CLI).
//!
//! A macOS `.app` launched from Finder inherits a MINIMAL `PATH` — typically
//! just `/usr/bin:/bin:/usr/sbin:/sbin`, WITHOUT `/usr/local/bin`,
//! `/opt/homebrew/bin`, or `~/.local/bin`. So a bare `Command::new("claude")`
//! (which resolves against the inherited `PATH`) fails with "not found" even
//! though `claude` is installed — the classic "works in Terminal, broken in the
//! shipped app" bug.
//!
//! [`resolve_claude_binary`] fixes this by probing the well-known install
//! locations directly (absolute paths, no `PATH` needed), falling back to an
//! inherited-`PATH` lookup. [`claude_search_dirs`] exposes the same directories
//! so a spawned child's `PATH` env can be augmented — belt and suspenders for
//! any tool `claude` itself shells out to.

use std::path::PathBuf;

/// The directories we look in for the `claude` binary, in priority order. These
/// are the standard install targets for Homebrew (Intel + Apple Silicon), the
/// official install script (`~/.local/bin`), and Claude Code's own local
/// versioned install (`~/.claude/local`).
///
/// Pure (modulo `$HOME`) so the search set is unit-testable.
pub fn claude_search_dirs() -> Vec<PathBuf> {
    let mut dirs = vec![
        PathBuf::from("/usr/local/bin"),
        PathBuf::from("/opt/homebrew/bin"),
    ];
    if let Some(home) = dirs_home() {
        dirs.push(home.join(".local").join("bin"));
        dirs.push(home.join(".claude").join("local"));
    }
    dirs
}

/// `$HOME` as a path, if set. Split out so tests can reason about the `HOME`
/// dependency without pulling in the `dirs` crate here.
fn dirs_home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// Resolve the absolute path to the `claude` CLI, or `None` if it can't be
/// found anywhere. Checks, in order:
///
/// 1. Each well-known install dir ([`claude_search_dirs`]) for an existing
///    `claude` file — this is what rescues the Finder-launched `.app`.
/// 2. The inherited `PATH` (covers custom installs / dev shells).
///
/// The caller passes the resolved absolute path to `Command::new`, so the spawn
/// never depends on the child inheriting a useful `PATH`.
pub fn resolve_claude_binary() -> Option<PathBuf> {
    resolve_claude_binary_in(&claude_search_dirs(), path_env())
}

/// The inherited `PATH` as a single string, if set.
fn path_env() -> Option<String> {
    std::env::var("PATH").ok()
}

/// Pure core of [`resolve_claude_binary`]: given a set of candidate dirs and an
/// optional `PATH` string, return the first existing `claude` binary. Injected
/// inputs make the resolution rule unit-testable without touching the real env.
pub fn resolve_claude_binary_in(search_dirs: &[PathBuf], path: Option<String>) -> Option<PathBuf> {
    // 1) Well-known absolute locations first — the fix for a stripped PATH.
    for dir in search_dirs {
        let cand = dir.join("claude");
        if cand.is_file() {
            return Some(cand);
        }
    }
    // 2) Fall back to whatever the inherited PATH offers.
    if let Some(path) = path {
        for dir in std::env::split_paths(&path) {
            let cand = dir.join("claude");
            if cand.is_file() {
                return Some(cand);
            }
        }
    }
    None
}

/// Build an augmented `PATH` string that prepends [`claude_search_dirs`] to the
/// current `PATH`, so a spawned child (and anything IT shells out to) can find
/// `claude` and its neighbours even under a stripped Finder `PATH`. Pure over
/// its inputs so the join rule is unit-testable.
pub fn augmented_path(search_dirs: &[PathBuf], current: Option<String>) -> String {
    let mut parts: Vec<PathBuf> = search_dirs.to_vec();
    if let Some(cur) = current {
        parts.extend(std::env::split_paths(&cur));
    }
    // Deduplicate while preserving order (a bloated PATH is harmless but ugly).
    let mut seen = std::collections::HashSet::new();
    parts.retain(|p| seen.insert(p.clone()));
    std::env::join_paths(parts)
        .map(|os| os.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// The augmented `PATH` for the current process env, ready to hand to a child's
/// `.env("PATH", ...)`. Convenience wrapper over [`augmented_path`].
pub fn augmented_path_env() -> String {
    augmented_path(&claude_search_dirs(), path_env())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// A unique temp dir for a test, created fresh.
    fn tmp(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ijc_which_{tag}_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn finds_claude_in_usr_local_bin_under_stripped_path() {
        // Simulate the Finder-launched .app: PATH lacks /usr/local/bin, but the
        // binary lives there. The resolver must still find it by absolute probe.
        let bindir = tmp("usrlocal");
        let claude = bindir.join("claude");
        fs::write(&claude, b"#!/bin/sh\n").unwrap();

        // A stripped PATH that does NOT contain our bindir.
        let stripped = "/usr/bin:/bin:/usr/sbin:/sbin".to_string();
        let got = resolve_claude_binary_in(std::slice::from_ref(&bindir), Some(stripped));
        assert_eq!(got, Some(claude));

        fs::remove_dir_all(&bindir).unwrap();
    }

    #[test]
    fn search_dirs_take_priority_over_path() {
        // Even if PATH has a claude, an earlier search dir wins (absolute probe
        // is authoritative — it's the trustworthy install location).
        let primary = tmp("primary");
        let onpath = tmp("onpath");
        fs::write(primary.join("claude"), b"x").unwrap();
        fs::write(onpath.join("claude"), b"y").unwrap();
        let path = onpath.to_string_lossy().into_owned();
        let got = resolve_claude_binary_in(std::slice::from_ref(&primary), Some(path));
        assert_eq!(got, Some(primary.join("claude")));
        fs::remove_dir_all(&primary).unwrap();
        fs::remove_dir_all(&onpath).unwrap();
    }

    #[test]
    fn falls_back_to_path_when_not_in_search_dirs() {
        let onpath = tmp("fallback");
        let claude = onpath.join("claude");
        fs::write(&claude, b"x").unwrap();
        let path = onpath.to_string_lossy().into_owned();
        // No search dirs hold it; PATH does.
        let got = resolve_claude_binary_in(&[PathBuf::from("/nonexistent-xyz")], Some(path));
        assert_eq!(got, Some(claude));
        fs::remove_dir_all(&onpath).unwrap();
    }

    #[test]
    fn none_when_absent_everywhere() {
        let got = resolve_claude_binary_in(
            &[PathBuf::from("/nonexistent-abc")],
            Some("/also/nonexistent".to_string()),
        );
        assert_eq!(got, None);
    }

    #[test]
    fn none_with_no_path_and_no_dirs() {
        assert_eq!(resolve_claude_binary_in(&[], None), None);
    }

    #[test]
    fn search_dirs_include_the_known_install_locations() {
        // The static locations must always be present regardless of $HOME.
        let dirs = claude_search_dirs();
        assert!(dirs.contains(&PathBuf::from("/usr/local/bin")), "{dirs:?}");
        assert!(dirs.contains(&PathBuf::from("/opt/homebrew/bin")), "{dirs:?}");
    }

    #[test]
    fn augmented_path_prepends_search_dirs() {
        let dirs = vec![PathBuf::from("/opt/homebrew/bin")];
        let out = augmented_path(&dirs, Some("/usr/bin:/bin".to_string()));
        // Homebrew dir comes first, existing PATH is preserved after it.
        assert!(out.starts_with("/opt/homebrew/bin"), "{out}");
        assert!(out.contains("/usr/bin"), "{out}");
        assert!(out.contains("/bin"), "{out}");
    }

    #[test]
    fn augmented_path_dedupes() {
        // A dir already on PATH must not appear twice.
        let dirs = vec![PathBuf::from("/usr/bin")];
        let out = augmented_path(&dirs, Some("/usr/bin:/bin".to_string()));
        let count = std::env::split_paths(&out)
            .filter(|p| p == &PathBuf::from("/usr/bin"))
            .count();
        assert_eq!(count, 1, "duplicate /usr/bin in {out}");
    }

    #[test]
    fn augmented_path_with_no_current_is_just_search_dirs() {
        let dirs = vec![PathBuf::from("/opt/homebrew/bin"), PathBuf::from("/usr/local/bin")];
        let out = augmented_path(&dirs, None);
        assert_eq!(out, "/opt/homebrew/bin:/usr/local/bin");
    }
}
