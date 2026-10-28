// SPDX-License-Identifier: AGPL-3.0-only
// Copyright © 2026 Hector Tarrido-Picart

//! `~` expansion for filesystem-path arguments.
//!
//! Expansion happens once, at parse time ([`crate::parse`]), so the command
//! line / history keeps the text exactly as typed while the parsed
//! [`Command`] — the thing executed and recorded in the op-log — carries the
//! resolved absolute path. Replaying an op-log therefore never depends on the
//! replaying process's `$HOME`.
//!
//! Only a leading `~` or `~/` is expanded; `~user` forms and a `~` anywhere
//! else are left untouched. With no home directory (e.g. a sandbox without
//! one) the path is returned unchanged.

use crate::command::{Command, SheetSetOp};
use std::path::Path;

/// Expand a leading `~` / `~/` in `path` against `home`. Pure: the home
/// directory is injected so it is unit-testable without touching `$HOME`.
pub fn expand_tilde_with(path: &str, home: Option<&Path>) -> String {
    let rest = match path.strip_prefix('~') {
        Some("") => "",
        Some(r) if r.starts_with('/') => &r[1..],
        _ => return path.to_string(),
    };
    match home {
        Some(h) if rest.is_empty() => h.to_string_lossy().into_owned(),
        Some(h) => h.join(rest).to_string_lossy().into_owned(),
        None => path.to_string(),
    }
}

/// [`expand_tilde_with`] against the current user's home directory.
pub fn expand_tilde(path: &str) -> String {
    expand_tilde_with(path, dirs::home_dir().as_deref())
}

impl Command {
    /// Expand a leading `~` in every filesystem-path argument this command
    /// carries (see the module docs). Non-path commands are unchanged.
    pub fn expand_tilde_paths(&mut self, home: Option<&Path>) {
        let x = |p: &mut String| *p = expand_tilde_with(p, home);
        match self {
            Command::Export { path, .. }
            | Command::Print { path, .. }
            | Command::Import { path }
            | Command::Terrain { path }
            | Command::OsmFile { path }
            | Command::PlantSchedule { path }
            | Command::Underlay { path, .. }
            | Command::HatchPat { path }
            | Command::CheckRulesLoad { path }
            | Command::XrefAttach { path, .. }
            | Command::Radiation { path, .. }
            | Command::ControlImages { prefix: path } => x(path),
            Command::Climate {
                path: Some(path), ..
            }
            | Command::DataExtract {
                path: Some(path), ..
            }
            | Command::Workdir { path: Some(path) }
            | Command::SheetSet(SheetSetOp::Publish { path: Some(path) }) => x(path),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn expands_only_a_leading_tilde() {
        let home = PathBuf::from("/home/u");
        let h = Some(home.as_path());
        assert_eq!(expand_tilde_with("~", h), "/home/u");
        assert_eq!(expand_tilde_with("~/", h), "/home/u");
        assert_eq!(expand_tilde_with("~/x", h), "/home/u/x");
        assert_eq!(
            expand_tilde_with("~/Desktop/part.dxf", h),
            "/home/u/Desktop/part.dxf"
        );
        assert_eq!(expand_tilde_with("a/~/b", h), "a/~/b");
        assert_eq!(expand_tilde_with("/abs/~/p", h), "/abs/~/p");
        assert_eq!(expand_tilde_with("rel.dxf", h), "rel.dxf");
        assert_eq!(expand_tilde_with("~user/x", h), "~user/x");
        assert_eq!(expand_tilde_with("~x", h), "~x");
    }

    #[test]
    fn no_home_leaves_path_unchanged() {
        assert_eq!(expand_tilde_with("~/x", None), "~/x");
    }

    #[test]
    fn expands_path_fields_of_commands() {
        let home = PathBuf::from("/home/u");
        let mut c = Command::Export {
            path: "~/a.dxf".into(),
            cut: None,
        };
        c.expand_tilde_paths(Some(&home));
        assert_eq!(
            c,
            Command::Export {
                path: "/home/u/a.dxf".into(),
                cut: None
            }
        );
        let mut c = Command::Climate {
            path: Some("~/w.epw".into()),
            data: None,
        };
        c.expand_tilde_paths(Some(&home));
        assert_eq!(
            c,
            Command::Climate {
                path: Some("/home/u/w.epw".into()),
                data: None
            }
        );
    }
}
