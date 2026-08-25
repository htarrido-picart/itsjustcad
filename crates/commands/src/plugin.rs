// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! LLM-authorable, user-persisted plugins — the "everything is a plugin" slice.
//!
//! Borrowed from DeepSeek-Harness's plugin ethos: an extension model where new
//! verbs are contributed at runtime rather than baked into the static registry,
//! and the authoritative record is the expanded command stream (their
//! "anything that reaches a model request must be reconstructable from the log"
//! invariant). Here a plugin is a *macro*: a named, parameterised list of
//! command-template lines. Invoking it expands — at execute time — into ordinary
//! commands that flow through the same substrate and get logged individually.
//! Replay never re-expands a plugin; it replays the expanded ops. That keeps old
//! op-logs stable even if a plugin is later edited or deleted (see FORMAT.md).
//!
//! Persistence — TWO on-disk layouts are accepted under
//! `~/.config/itsjustcad/plugins/`, both loaded at startup into a
//! `PluginRegistry` that autosuggest, help, the menu and the deck prompt all
//! consult:
//! * **Manifest (formalized user layout):** one directory per plugin,
//!   `<name>/plugin.json`. This is the documented, user-facing format (see
//!   `docs/plugins.md`) — a self-contained plugin folder a user can drop in.
//! * **Flat (legacy / LLM quick-author):** a single `<name>.plugin.json`
//!   file. Retained so op-logs and older installs keep working.
//!
//! `define()` writes the manifest layout; `load_dir()` reads both.
//!
//! Safety: a plugin is a *declarative macro* — a list of command-template lines
//! composed from existing substrate verbs plus `{param}` substitution. It can
//! NEVER run native code or bypass the op-log: every expanded line is parsed and
//! executed as an ordinary substrate command (logged individually). This is the
//! same hardened posture as the deck.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Write `contents` to `path` with mode 0600 on unix (readable only by owner).
/// On non-unix platforms it falls back to a plain write.
fn write_private(path: &Path, contents: &str) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)?;
        f.write_all(contents.as_bytes())
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, contents)
    }
}

/// Returns `true` when the plugin name is safe to use as a filename component.
/// Mirrors the guard in `define()` — rejects empty, and any `/`, `\`, or `.`.
fn name_is_valid(name: &str) -> bool {
    !name.is_empty() && !name.contains(['/', '\\', '.'])
}

/// One positional parameter of a plugin, with an optional default value.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PluginParam {
    pub name: String,
    #[serde(default)]
    pub default: Option<String>,
}

/// A user/LLM-authored macro. `body` lines are command templates; `{0}`, `{1}`,
/// ... (and `{name}` matching a declared param) are substituted positionally at
/// invocation time.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Plugin {
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// Optional menu group. When set (and non-empty) the plugin verb surfaces
    /// under a "Plugins ▸ <category>" submenu; otherwise it lands directly under
    /// the top-level "Plugins" menu. Purely a UI hint — it never affects how the
    /// plugin lowers to substrate commands.
    #[serde(default)]
    pub category: Option<String>,
    #[serde(default)]
    pub params: Vec<PluginParam>,
    pub body: Vec<String>,
}

impl Plugin {
    /// A one-line usage string mirroring `CommandSpec::usage`, e.g.
    /// `column-grid <nx> <ny>` — declared params become positional slots.
    pub fn usage(&self) -> String {
        let mut s = self.name.clone();
        for p in &self.params {
            s.push_str(&format!(" <{}>", p.name));
        }
        s
    }

    /// A summary line for help/prompt. Falls back to a generic note.
    pub fn summary(&self) -> String {
        if self.description.is_empty() {
            format!("Plugin macro ({} line(s)).", self.body.len())
        } else {
            self.description.clone()
        }
    }

    /// Expand the body against positional `args`, substituting `{i}` and
    /// `{param-name}`. Missing args fall back to the param default; a slot with
    /// neither an arg nor a default is an error.
    pub fn expand(&self, args: &[String]) -> Result<Vec<String>, PluginError> {
        // Resolve the value for each declared param (arg wins over default).
        let mut by_name: BTreeMap<&str, String> = BTreeMap::new();
        let mut positional: Vec<String> = Vec::new();
        for (i, p) in self.params.iter().enumerate() {
            let val = match args.get(i) {
                Some(a) => a.clone(),
                None => p.default.clone().ok_or_else(|| PluginError::MissingArg {
                    plugin: self.name.clone(),
                    param: p.name.clone(),
                })?,
            };
            by_name.insert(p.name.as_str(), val.clone());
            positional.push(val);
        }
        // Extra positional args beyond declared params are still addressable by
        // index ({3} etc.) so a parameterless plugin can be invoked with args.
        for a in args.iter().skip(self.params.len()) {
            positional.push(a.clone());
        }

        let mut out = Vec::with_capacity(self.body.len());
        for line in &self.body {
            out.push(substitute(line, &positional, &by_name));
        }
        Ok(out)
    }

    /// Menu group label for this plugin: its declared `category`, or `"Plugins"`
    /// when none is set. Used to build the Plugins menu (see `menu.rs`).
    pub fn menu_category(&self) -> &str {
        match &self.category {
            Some(c) if !c.trim().is_empty() => c.as_str(),
            _ => "Plugins",
        }
    }

    /// Directory this plugin's manifest lives in under `dir` (`<dir>/<name>/`).
    pub fn dir_in(&self, dir: &Path) -> PathBuf {
        dir.join(&self.name)
    }

    /// Manifest path this plugin persists to under `dir`
    /// (`<dir>/<name>/plugin.json`) — the formalized, user-facing layout.
    pub fn manifest_in(&self, dir: &Path) -> PathBuf {
        self.dir_in(dir).join("plugin.json")
    }

    /// Legacy flat path (`<dir>/<name>.plugin.json`). Still read by `load_dir`
    /// and cleaned up by `delete` so older installs keep working.
    pub fn path_in(&self, dir: &Path) -> PathBuf {
        dir.join(format!("{}.plugin.json", self.name))
    }
}

/// Replace `{i}` and `{name}` tokens in one line. Unknown tokens are left as-is
/// (so a literal `{foo}` in a body survives when nothing matches).
fn substitute(line: &str, positional: &[String], by_name: &BTreeMap<&str, String>) -> String {
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        if let Some(close_rel) = rest[open..].find('}') {
            let token = &rest[open + 1..open + close_rel];
            let replaced = if let Ok(idx) = token.parse::<usize>() {
                positional.get(idx).cloned()
            } else {
                by_name.get(token).cloned()
            };
            match replaced {
                Some(v) => out.push_str(&v),
                None => {
                    // Leave the token verbatim.
                    out.push_str(&rest[open..open + close_rel + 1]);
                }
            }
            rest = &rest[open + close_rel + 1..];
        } else {
            // No closing brace — emit the remainder literally.
            out.push_str(&rest[open..]);
            rest = "";
        }
    }
    out.push_str(rest);
    out
}

/// Runtime plugin table. Consulted by autosuggest, help and the deck prompt so
/// LLM-authored verbs are first-class alongside the static registry.
#[derive(Clone, Debug, Default)]
pub struct PluginRegistry {
    plugins: BTreeMap<String, Plugin>,
}

impl PluginRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Load every plugin under `dir` (ignored if the dir is absent). Accepts
    /// BOTH the manifest layout (`<name>/plugin.json`) and the legacy flat
    /// layout (`<name>.plugin.json`). Malformed files are skipped with the error
    /// collected in the return so one bad plugin never blocks the rest.
    pub fn load_dir(dir: &Path) -> (Self, Vec<String>) {
        let mut reg = Self::new();
        let mut warnings = Vec::new();
        let Ok(entries) = std::fs::read_dir(dir) else {
            return (reg, warnings);
        };
        for entry in entries.flatten() {
            let path = entry.path();
            // Manifest layout: a subdirectory holding `plugin.json`.
            let manifest = if path.is_dir() {
                Some(path.join("plugin.json"))
            } else if path.to_string_lossy().ends_with(".plugin.json") {
                // Legacy flat layout.
                Some(path.clone())
            } else {
                None
            };
            let Some(manifest) = manifest else { continue };
            if !manifest.exists() {
                continue;
            }
            match std::fs::read_to_string(&manifest)
                .map_err(|e| e.to_string())
                .and_then(|s| serde_json::from_str::<Plugin>(&s).map_err(|e| e.to_string()))
            {
                Ok(p) => {
                    // H-6: validate name on load — reject traversal names that
                    // slipped past define() (e.g. hand-planted plugin files).
                    if !name_is_valid(&p.name) {
                        warnings.push(format!(
                            "{}: skipped — plugin name {:?} contains unsafe characters",
                            manifest.display(), p.name
                        ));
                        continue;
                    }
                    reg.plugins.insert(p.name.clone(), p);
                }
                Err(e) => warnings.push(format!("{}: {e}", manifest.display())),
            }
        }
        (reg, warnings)
    }

    pub fn get(&self, name: &str) -> Option<&Plugin> {
        self.plugins.get(name)
    }

    pub fn contains(&self, name: &str) -> bool {
        self.plugins.contains_key(name)
    }

    /// Plugins in stable (name-sorted) order.
    pub fn iter(&self) -> impl Iterator<Item = &Plugin> {
        self.plugins.values()
    }

    pub fn is_empty(&self) -> bool {
        self.plugins.is_empty()
    }

    pub fn len(&self) -> usize {
        self.plugins.len()
    }

    /// Register a plugin in memory only (no disk write).
    pub fn insert(&mut self, plugin: Plugin) {
        self.plugins.insert(plugin.name.clone(), plugin);
    }

    /// Register a plugin and persist it to `dir` as `<name>.plugin.json`.
    pub fn define(&mut self, plugin: Plugin, dir: &Path) -> Result<(), PluginError> {
        if plugin.name.is_empty() {
            return Err(PluginError::EmptyName);
        }
        // Guard the on-disk filename against path traversal / separators.
        if !name_is_valid(&plugin.name) {
            return Err(PluginError::BadName(plugin.name.clone()));
        }
        // Write the formalized manifest layout: `<dir>/<name>/plugin.json`.
        let plugin_dir = plugin.dir_in(dir);
        std::fs::create_dir_all(&plugin_dir).map_err(|e| PluginError::Io(e.to_string()))?;
        let json = serde_json::to_string_pretty(&plugin)
            .map_err(|e| PluginError::Io(e.to_string()))?;
        // L-1: write with 0600 so plugin files (which can contain command bodies
        // that reference local paths) are not world-readable on multi-user hosts.
        write_private(&plugin.manifest_in(dir), &json)
            .map_err(|e| PluginError::Io(e.to_string()))?;
        self.plugins.insert(plugin.name.clone(), plugin);
        Ok(())
    }

    /// Remove a plugin from memory and disk.
    pub fn delete(&mut self, name: &str, dir: &Path) -> Result<(), PluginError> {
        // H-6: re-validate name before deriving the disk path — never join a
        // raw attacker-controlled string with dir.
        if !name_is_valid(name) {
            return Err(PluginError::BadName(name.to_string()));
        }
        let Some(plugin) = self.plugins.remove(name) else {
            return Err(PluginError::Unknown(name.to_string()));
        };
        // Re-derive paths from the already-validated in-memory name (same as
        // plugin.name, which passed validate() at define/load time). Remove both
        // the manifest directory and any legacy flat file.
        let manifest_dir = dir.join(&plugin.name);
        if manifest_dir.is_dir() {
            std::fs::remove_dir_all(&manifest_dir).map_err(|e| PluginError::Io(e.to_string()))?;
        }
        let flat = dir.join(format!("{}.plugin.json", plugin.name));
        if flat.exists() {
            std::fs::remove_file(&flat).map_err(|e| PluginError::Io(e.to_string()))?;
        }
        Ok(())
    }
}

/// Default on-disk location for plugins: `~/.config/itsjustcad/plugins`.
pub fn default_dir() -> Option<PathBuf> {
    Some(
        dirs::home_dir()?
            .join(".config")
            .join("itsjustcad")
            .join("plugins"),
    )
}

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum PluginError {
    #[error("plugin '{0}' not found")]
    Unknown(String),
    #[error("plugin '{plugin}' needs a value for '{param}'")]
    MissingArg { plugin: String, param: String },
    #[error("plugin name cannot be empty")]
    EmptyName,
    #[error("plugin name '{0}' cannot contain '/', '\\' or '.'")]
    BadName(String),
    #[error("plugin i/o error: {0}")]
    Io(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn column_grid() -> Plugin {
        Plugin {
            name: "column-grid".into(),
            description: "Grid of columns".into(),
            category: None,
            params: vec![
                PluginParam { name: "nx".into(), default: Some("5".into()) },
                PluginParam { name: "ny".into(), default: Some("3".into()) },
            ],
            body: vec![
                "box 0,0,0 0.4,0.4,3".into(),
                "array last {0},{1},1 3,4,0".into(),
            ],
        }
    }

    #[test]
    fn expand_substitutes_positional() {
        let p = column_grid();
        let out = p.expand(&["6".into(), "4".into()]).unwrap();
        assert_eq!(out[0], "box 0,0,0 0.4,0.4,3");
        assert_eq!(out[1], "array last 6,4,1 3,4,0");
    }

    #[test]
    fn expand_uses_defaults_when_arg_missing() {
        let p = column_grid();
        let out = p.expand(&[]).unwrap();
        assert_eq!(out[1], "array last 5,3,1 3,4,0");
    }

    #[test]
    fn expand_by_param_name_token() {
        let p = Plugin {
            name: "t".into(),
            description: String::new(),
            category: None,
            params: vec![PluginParam { name: "h".into(), default: Some("3".into()) }],
            body: vec!["extrude last {h}".into()],
        };
        assert_eq!(p.expand(&["9".into()]).unwrap()[0], "extrude last 9");
        assert_eq!(p.expand(&[]).unwrap()[0], "extrude last 3");
    }

    #[test]
    fn expand_missing_required_arg_errors() {
        let p = Plugin {
            name: "t".into(),
            description: String::new(),
            category: None,
            params: vec![PluginParam { name: "h".into(), default: None }],
            body: vec!["extrude last {0}".into()],
        };
        assert_eq!(
            p.expand(&[]),
            Err(PluginError::MissingArg { plugin: "t".into(), param: "h".into() })
        );
    }

    #[test]
    fn unknown_token_left_verbatim() {
        let p = Plugin {
            name: "t".into(),
            description: String::new(),
            category: None,
            params: vec![],
            body: vec!["note {foo} and {9}".into()],
        };
        assert_eq!(p.expand(&[]).unwrap()[0], "note {foo} and {9}");
    }

    #[test]
    fn usage_lists_params() {
        assert_eq!(column_grid().usage(), "column-grid <nx> <ny>");
    }

    #[test]
    fn define_persist_load_roundtrip() {
        let dir = std::env::temp_dir().join(format!("mydrafter-plugtest-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut reg = PluginRegistry::new();
        reg.define(column_grid(), &dir).unwrap();
        // define() writes the formalized manifest layout `<name>/plugin.json`.
        assert!(column_grid().manifest_in(&dir).exists());

        let (loaded, warnings) = PluginRegistry::load_dir(&dir);
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(loaded.get("column-grid"), Some(&column_grid()));

        // Delete removes disk + memory.
        let mut reg2 = loaded.clone();
        reg2.delete("column-grid", &dir).unwrap();
        assert!(!reg2.contains("column-grid"));
        assert!(!column_grid().manifest_in(&dir).exists());
        assert!(!column_grid().dir_in(&dir).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn define_rejects_bad_names() {
        let dir = std::env::temp_dir().join("mydrafter-plugtest-bad");
        let mut reg = PluginRegistry::new();
        let mut p = column_grid();
        p.name = "../evil".into();
        assert_eq!(reg.define(p, &dir), Err(PluginError::BadName("../evil".into())));
    }

    #[test]
    fn load_dir_absent_is_empty() {
        let (reg, warnings) = PluginRegistry::load_dir(Path::new("/no/such/dir/xyz"));
        assert!(reg.is_empty());
        assert!(warnings.is_empty());
    }

    #[test]
    fn extra_positional_args_addressable_by_index() {
        // Parameterless plugin invoked with args — {0} still resolves.
        let p = Plugin {
            name: "t".into(),
            description: String::new(),
            category: None,
            params: vec![],
            body: vec!["box {0} {1}".into()],
        };
        let out = p.expand(&["0,0,0".into(), "5,5,3".into()]).unwrap();
        assert_eq!(out[0], "box 0,0,0 5,5,3");
    }

    // ── H-6 regression tests ────────────────────────────────────────────────

    /// A hand-planted plugin JSON with a traversal name must be silently
    /// skipped (with a warning) on load, never inserted into the registry.
    #[test]
    fn load_dir_rejects_traversal_name() {
        let dir = std::env::temp_dir()
            .join(format!("mydrafter-plugtest-traversal-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // Plant a hostile plugin file with a traversal name in the JSON body.
        let malicious_json = r#"{"name":"../../../../.ssh/authorized_keys","body":["echo pwned"]}"#;
        std::fs::write(dir.join("evil.plugin.json"), malicious_json).unwrap();

        let (reg, warnings) = PluginRegistry::load_dir(&dir);

        // The traversal name must NOT appear in the registry.
        assert!(reg.is_empty(), "traversal plugin was loaded: {:?}", reg.plugins.keys().collect::<Vec<_>>());
        // A warning must have been emitted.
        assert!(!warnings.is_empty(), "expected a warning about the bad name");
        assert!(warnings[0].contains("unsafe characters"), "warning text: {:?}", warnings);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// After loading a legitimate plugin, `delete` with a traversal name must
    /// fail with `BadName` — it must never reach `remove_file` outside the dir.
    #[test]
    fn delete_rejects_traversal_name_before_remove_file() {
        let dir = std::env::temp_dir()
            .join(format!("mydrafter-plugtest-del-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut reg = PluginRegistry::new();
        reg.define(column_grid(), &dir).unwrap();

        // Attempt to delete using a traversal name — must be rejected.
        let result = reg.delete("../../../.ssh/authorized_keys", &dir);
        assert_eq!(result, Err(PluginError::BadName("../../../.ssh/authorized_keys".into())));

        // The legitimate plugin must still be in memory and on disk.
        assert!(reg.contains("column-grid"));
        assert!(column_grid().manifest_in(&dir).exists());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Plugin file written by `define` must have mode 0600 on unix (L-1).
    #[cfg(unix)]
    #[test]
    fn define_writes_private_mode() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir()
            .join(format!("mydrafter-plugtest-mode-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut reg = PluginRegistry::new();
        reg.define(column_grid(), &dir).unwrap();

        let meta = std::fs::metadata(column_grid().manifest_in(&dir)).unwrap();
        let mode = meta.permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "expected 0600, got {mode:o}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── manifest layout + category (formalized plugin API) ───────────────────

    /// A hand-authored manifest directory (`<name>/plugin.json`) — the
    /// documented user layout — loads with its declared category.
    #[test]
    fn load_dir_reads_manifest_layout_with_category() {
        let dir = std::env::temp_dir()
            .join(format!("mydrafter-plugtest-manifest-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("greek-column")).unwrap();
        let json = r#"{
            "name": "greek-column",
            "description": "A fluted Doric column",
            "category": "Classical",
            "params": [{"name": "h", "default": "4"}],
            "body": ["cylinder 0,0,0 0.3 {h}"]
        }"#;
        std::fs::write(dir.join("greek-column").join("plugin.json"), json).unwrap();

        let (reg, warnings) = PluginRegistry::load_dir(&dir);
        assert!(warnings.is_empty(), "{warnings:?}");
        let p = reg.get("greek-column").expect("plugin loaded from manifest");
        assert_eq!(p.menu_category(), "Classical");
        assert_eq!(p.expand(&[]).unwrap()[0], "cylinder 0,0,0 0.3 4");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A plugin with no category falls back to the generic "Plugins" menu group.
    #[test]
    fn menu_category_defaults_to_plugins() {
        assert_eq!(column_grid().menu_category(), "Plugins");
        let mut p = column_grid();
        p.category = Some("   ".into()); // blank → still the default group
        assert_eq!(p.menu_category(), "Plugins");
    }

    /// A malformed manifest is rejected with a clear, path-tagged error and does
    /// not crash the loader or poison the registry.
    #[test]
    fn load_dir_reports_malformed_manifest() {
        let dir = std::env::temp_dir()
            .join(format!("mydrafter-plugtest-bad-manifest-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("broken")).unwrap();
        // Missing the required `body` field + trailing junk.
        std::fs::write(
            dir.join("broken").join("plugin.json"),
            r#"{"name": "broken" oops"#,
        )
        .unwrap();

        let (reg, warnings) = PluginRegistry::load_dir(&dir);
        assert!(reg.is_empty(), "malformed plugin must not load");
        assert_eq!(warnings.len(), 1, "expected exactly one warning");
        assert!(
            warnings[0].contains("plugin.json"),
            "warning should name the file: {:?}",
            warnings
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// serde back-compat: a manifest WITHOUT the newer `category` field (as an
    /// older install would have written) still deserializes, defaulting the
    /// field to `None`. Guards the `#[serde(default)]` on `category`.
    #[test]
    fn pre_category_manifest_still_loads() {
        let legacy = r#"{
            "name": "old",
            "description": "pre-category plugin",
            "params": [],
            "body": ["box 0,0,0 1,1,1"]
        }"#;
        let p: Plugin = serde_json::from_str(legacy).unwrap();
        assert_eq!(p.category, None);
        assert_eq!(p.menu_category(), "Plugins");
        // Round-trips: re-serialize → re-parse is byte-stable and id-stable.
        let json = serde_json::to_string(&p).unwrap();
        let p2: Plugin = serde_json::from_str(&json).unwrap();
        assert_eq!(p, p2);
        assert_eq!(json, serde_json::to_string(&p2).unwrap());
    }
}
