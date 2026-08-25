// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

use std::path::PathBuf;

use itsjustcad_commands::plugin::{self, Plugin, PluginRegistry};
use itsjustcad_commands::{parse, registry, Command, Session};

use crate::suggest::{self, Suggestion};

/// Maximum number of autosuggest entries shown at once.
const MAX_SUGGESTIONS: usize = 8;

/// Rhino-style command line: single input row + scrollback, up/down history,
/// plus as-you-type autosuggest popup and usage hints.
pub struct CommandLine {
    input: String,
    history: Vec<String>,
    /// Executed inputs for up-arrow recall.
    recall: Vec<String>,
    recall_pos: Option<usize>,
    focus_next_frame: bool,
    /// Current suggestion list (recomputed every keystroke).
    suggestions: Vec<Suggestion>,
    /// Which suggestion is highlighted; None = none.
    suggest_sel: Option<usize>,
    /// True when the user explicitly dismissed the popup with Esc.
    suggest_dismissed: bool,
    /// The input text that was current when we last built `suggestions`.
    suggest_for: String,
    /// On-disk plugin directory (`~/.config/itsjustcad/plugins`). The registry
    /// itself lives on `Session` so the deck prompt can reach it.
    plugin_dir: PathBuf,
    /// Raw command-line text of every document-changing command, in order —
    /// the source for `plugin save <name> <n>`.
    logged_inputs: Vec<String>,
    /// Plugin verbs cached for autosuggest (name, usage), refreshed after any
    /// define/delete so the popup reflects the current registry.
    plugin_verbs: Vec<(String, String)>,
}

impl Default for CommandLine {
    fn default() -> Self {
        Self {
            input: String::new(),
            history: vec!["ItsJustCAD — type 'help' for commands".to_string()],
            recall: Vec::new(),
            recall_pos: None,
            focus_next_frame: true,
            suggestions: Vec::new(),
            suggest_sel: None,
            suggest_dismissed: false,
            suggest_for: String::new(),
            plugin_dir: plugin::default_dir().unwrap_or_else(|| PathBuf::from(".")),
            logged_inputs: Vec::new(),
            plugin_verbs: Vec::new(),
        }
    }
}

impl CommandLine {
    /// Load persisted plugins from `plugin_dir` into `session.plugins` at
    /// startup. Malformed files are reported to the scrollback but never block
    /// startup.
    pub fn load_plugins(&mut self, session: &mut Session) {
        let (reg, warnings) = PluginRegistry::load_dir(&self.plugin_dir);
        session.plugins = reg;
        for w in warnings {
            self.push_line(format!("plugin load warning: {w}"));
        }
        if !session.plugins.is_empty() {
            let names: Vec<&str> = session.plugins.iter().map(|p| p.name.as_str()).collect();
            self.push_line(format!("plugins: {}", names.join(", ")));
        }
        self.refresh_plugin_verbs(session);
    }

    /// Snapshot plugin (name, usage) pairs for autosuggest after any change.
    fn refresh_plugin_verbs(&mut self, session: &Session) {
        self.plugin_verbs = session
            .plugins
            .iter()
            .map(|p| (p.name.clone(), p.usage()))
            .collect();
        self.suggest_for.clear(); // force a suggestion rebuild
    }

    pub fn push_line(&mut self, line: impl Into<String>) {
        self.history.push(line.into());
        if self.history.len() > 500 {
            self.history.drain(..self.history.len() - 500);
        }
    }

    /// Run one command line through the session, echoing results.
    /// Returns true when the document changed.
    pub fn execute(&mut self, session: &mut Session, line: &str) -> bool {
        let line = line.trim();
        if line.is_empty() {
            return false;
        }
        self.recall.push(line.to_string());
        self.push_line(format!("> {line}"));

        if line == "help" {
            for spec in registry() {
                self.push_line(format!("  {:<28} {}", spec.usage, spec.summary));
            }
            for p in session.plugins.iter() {
                self.push_line(format!("  {:<28} {}", p.usage(), p.summary()));
            }
            self.push_line(format!("  {}", itsjustcad_commands::SELECTOR_HELP));
            return false;
        }

        // `plugin <sub> ...` management commands, then plugin invocation, both
        // handled before the parser so plugin verbs shadow nothing in the
        // static registry (names collide only if a user picks a builtin name,
        // in which case the builtin wins — see run_plugin's guard at define).
        let mut words = line.split_whitespace();
        let verb = words.next().unwrap_or("");
        if verb == "plugin" {
            let rest: Vec<&str> = words.collect();
            return self.plugin_command(session, &rest);
        }
        if !registry().iter().any(|s| s.name == verb) && session.plugins.contains(verb) {
            return self.invoke_plugin(session, verb, line);
        }

        match parse(line) {
            Ok(cmd) => {
                let changes = cmd.is_logged();
                match session.run(cmd) {
                    Ok(outcome) => {
                        self.push_line(outcome.message);
                        if changes {
                            self.logged_inputs.push(line.to_string());
                        }
                        true
                    }
                    Err(e) => {
                        self.push_line(format!("error: {e}"));
                        false
                    }
                }
            }
            Err(e) => {
                self.push_line(format!("error: {e}"));
                false
            }
        }
    }

    /// Invoke a plugin: expand its body against positional args (drop the
    /// plugin verb, keep the rest of the line as args) and run each expanded
    /// line through the substrate. Every expanded op is logged normally, so the
    /// op-log is replay-safe without re-expanding the plugin.
    fn invoke_plugin(&mut self, session: &mut Session, name: &str, line: &str) -> bool {
        let args: Vec<String> = line.split_whitespace().skip(1).map(String::from).collect();
        let Some(plugin) = session.plugins.get(name).cloned() else {
            self.push_line(format!("error: plugin '{name}' not found"));
            return false;
        };
        let body = match plugin.expand(&args) {
            Ok(b) => b,
            Err(e) => {
                self.push_line(format!("error: {e}"));
                return false;
            }
        };
        let mut changed = false;
        for expanded in body {
            self.push_line(format!("  {expanded}"));
            match parse(&expanded) {
                Ok(cmd) => {
                    let logs = cmd.is_logged();
                    match session.run(cmd) {
                        Ok(outcome) => {
                            self.push_line(outcome.message);
                            if logs {
                                self.logged_inputs.push(expanded.clone());
                            }
                            changed = true;
                        }
                        Err(e) => {
                            self.push_line(format!("  error: {e}"));
                        }
                    }
                }
                Err(e) => self.push_line(format!("  error: {e}")),
            }
        }
        changed
    }

    /// Handle `plugin list | delete <name> | save <name> <n> | define <json>`.
    /// Returns whether the document changed (always false — these manage macros).
    fn plugin_command(&mut self, session: &mut Session, rest: &[&str]) -> bool {
        match rest.first().copied() {
            Some("list") => {
                if session.plugins.is_empty() {
                    self.push_line("no plugins defined");
                } else {
                    for p in session.plugins.iter() {
                        self.push_line(format!("  {:<28} {}", p.usage(), p.summary()));
                    }
                }
            }
            Some("delete") => match rest.get(1) {
                Some(name) => match session.plugins.delete(name, &self.plugin_dir) {
                    Ok(()) => {
                        self.push_line(format!("deleted plugin '{name}'"));
                        self.refresh_plugin_verbs(session);
                    }
                    Err(e) => self.push_line(format!("error: {e}")),
                },
                None => self.push_line("usage: plugin delete <name>"),
            },
            Some("save") => {
                let name = rest.get(1);
                let n: Option<usize> = rest.get(2).and_then(|s| s.parse().ok());
                match (name, n) {
                    (Some(name), Some(n)) => self.plugin_save(session, name, n),
                    _ => self.push_line("usage: plugin save <name> <n>"),
                }
            }
            Some("define") => {
                // Everything after `define` is inline JSON (may contain spaces).
                let json = rest[1..].join(" ");
                self.plugin_define(session, &json);
            }
            _ => self.push_line("usage: plugin list | save <name> <n> | define <json> | delete <name>"),
        }
        false
    }

    /// Capture the last `n` logged command lines as a parameterless plugin body.
    fn plugin_save(&mut self, session: &mut Session, name: &str, n: usize) {
        if self.logged_inputs.is_empty() {
            self.push_line("nothing in history to save");
            return;
        }
        let take = n.min(self.logged_inputs.len());
        let body: Vec<String> = self.logged_inputs[self.logged_inputs.len() - take..].to_vec();
        let plugin = Plugin {
            name: name.to_string(),
            description: format!("Captured from {take} command(s)."),
            params: Vec::new(),
            body,
        };
        match session.plugins.define(plugin, &self.plugin_dir) {
            Ok(()) => {
                self.push_line(format!("saved plugin '{name}' ({take} command(s))"));
                self.refresh_plugin_verbs(session);
            }
            Err(e) => self.push_line(format!("error: {e}")),
        }
    }

    /// Create + persist a plugin from an inline JSON object (the LLM can emit
    /// this mid-conversation to author a macro).
    fn plugin_define(&mut self, session: &mut Session, json: &str) {
        match serde_json::from_str::<Plugin>(json) {
            Ok(plugin) => {
                let name = plugin.name.clone();
                match session.plugins.define(plugin, &self.plugin_dir) {
                    Ok(()) => {
                        self.push_line(format!("defined plugin '{name}'"));
                        self.refresh_plugin_verbs(session);
                    }
                    Err(e) => self.push_line(format!("error: {e}")),
                }
            }
            Err(e) => self.push_line(format!("error: bad plugin JSON: {e}")),
        }
    }

    /// Run a pre-built command, echoing `line` like a typed input. Used when
    /// the app fills fields the parser cannot (e.g. the viewport camera for
    /// `view save`). Returns true when the document changed.
    pub fn execute_command(&mut self, session: &mut Session, line: &str, cmd: Command) -> bool {
        self.recall.push(line.to_string());
        self.push_line(format!("> {line}"));
        let changes = cmd.is_logged();
        match session.run(cmd) {
            Ok(outcome) => {
                self.push_line(outcome.message);
                if changes {
                    self.logged_inputs.push(line.to_string());
                }
                true
            }
            Err(e) => {
                self.push_line(format!("error: {e}"));
                false
            }
        }
    }

    /// Pre-fill the command input with `text` without executing it.
    /// Used by the ITSJUSTCAD_TYPE dev hook to make the autosuggest popup
    /// visible in ITSJUSTCAD_SHOT screenshots.
    pub fn prefill(&mut self, text: String) {
        self.input = text;
        self.suggest_for.clear(); // force suggestion refresh
    }

    /// Recompute suggestions when `input` has changed.
    fn refresh_suggestions(
        &mut self,
        object_names: &[String],
        preset_aliases: &'static [(&'static str, &'static str)],
    ) {
        if self.input == self.suggest_for {
            return;
        }
        self.suggest_for = self.input.clone();
        self.suggest_dismissed = false;
        self.suggest_sel = None;
        self.suggestions = suggest::suggestions(
            &self.input,
            object_names,
            MAX_SUGGESTIONS,
            preset_aliases,
            &self.plugin_verbs,
        );
    }

    /// Accept the currently-highlighted (or first) suggestion, completing the
    /// current token in the input and appending a trailing space.
    fn accept_suggestion(&mut self) {
        let idx = self.suggest_sel.unwrap_or(0);
        if let Some(s) = self.suggestions.get(idx) {
            let completion = s.completion.clone();
            // Replace the last whitespace-delimited token with the completion.
            let new_input = if suggest::verb_is_complete(&self.input) {
                // We're in an argument position: replace last token.
                let trimmed = self.input.trim_end();
                let last_start = trimmed.rfind(' ').map(|p| p + 1).unwrap_or(0);
                format!("{}{} ", &self.input[..last_start], completion)
            } else {
                // Verb position: replace the whole first token.
                format!("{completion} ")
            };
            self.input = new_input;
            // Force suggestion refresh.
            self.suggest_for.clear();
            self.suggest_dismissed = true; // hide popup after accepting
            self.suggestions.clear();
            self.suggest_sel = None;
        }
    }

    /// Returns Some(command) when the user pressed Enter.
    /// `object_names` — names of named objects in the current document; used
    /// to populate selector suggestions.
    /// `preset_aliases` — active legacy-CAD alias map from `preset::preset_for`.
    pub fn ui(
        &mut self,
        ui: &mut egui::Ui,
        object_names: &[String],
        preset_aliases: &'static [(&'static str, &'static str)],
    ) -> Option<String> {
        // Recompute suggestions if input changed.
        self.refresh_suggestions(object_names, preset_aliases);

        // Bottom-docked layout (under the right panel): history (op-log
        // scrollback) sits ABOVE the input (Rhino/AutoCAD look) and the popup
        // opens upward.
        let history_block = |cl: &Self, ui: &mut egui::Ui| {
            egui::ScrollArea::vertical()
                .id_salt("cmd_history")
                .max_height(80.0)
                .stick_to_bottom(true)
                .show(ui, |ui| {
                    for line in &cl.history {
                        ui.monospace(line);
                    }
                });
        };

        history_block(self, ui);
        self.suggestion_block(ui, true);
        self.input_row(ui)
    }

    /// True when the autosuggest popup should be visible for the current input.
    fn popup_visible(&self) -> bool {
        let verb = suggest::verb_of(self.input.trim_start());
        let show_usage_hint = suggest::verb_is_complete(&self.input) && !verb.is_empty();
        !self.suggest_dismissed && !self.suggestions.is_empty() && !show_usage_hint
    }

    /// Draw the usage hint (when a verb is fully typed) and the autosuggest
    /// popup. `above_input` records whether this block sits above the input row
    /// (bottom-docked) or below it (top-docked) — the content is identical; the
    /// caller controls placement by call order.
    fn suggestion_block(&mut self, ui: &mut egui::Ui, _above_input: bool) {
        // ── Usage hint (shown when verb is fully typed) ──────────────────
        let verb = suggest::verb_of(self.input.trim_start()).to_string();
        let show_usage_hint = suggest::verb_is_complete(&self.input) && !verb.is_empty();
        if show_usage_hint
            && let Some(usage) = suggest::usage_for_verb(&verb)
        {
            ui.add(
                egui::Label::new(
                    egui::RichText::new(usage)
                        .weak()
                        .text_style(egui::TextStyle::Small)
                        .font(egui::FontId::monospace(11.0)),
                )
                .wrap(),
            );
        }

        if self.popup_visible() {
            let sel = self.suggest_sel.unwrap_or(usize::MAX);
            for (i, s) in self.suggestions.iter().enumerate() {
                let label = if let Some(u) = &s.usage {
                    format!("{:<20} {}", s.completion, u)
                } else {
                    s.completion.clone()
                };
                let text = if i == sel {
                    egui::RichText::new(label)
                        .monospace()
                        .strong()
                        .color(ui.visuals().selection.stroke.color)
                } else {
                    egui::RichText::new(label).monospace().weak()
                };
                ui.label(text);
            }
            ui.separator();
        }
    }

    /// The prompt + text input row, with history recall, popup navigation and
    /// suggestion acceptance. Returns Some(line) when the user pressed Enter.
    fn input_row(&mut self, ui: &mut egui::Ui) -> Option<String> {
        let mut submitted = None;
        let show_popup = self.popup_visible();
        ui.horizontal(|ui| {
            ui.monospace(">");
            let response = ui.add(
                egui::TextEdit::singleline(&mut self.input)
                    .desired_width(f32::INFINITY)
                    .font(egui::TextStyle::Monospace)
                    .hint_text("box 0,0,0 5,5,3"),
            );
            if self.focus_next_frame {
                response.request_focus();
                self.focus_next_frame = false;
            }

            if response.has_focus() {
                // Read all relevant keys in one go to avoid multiple borrows.
                let (up, down, tab, right, esc) = ui.input(|i| {
                    (
                        i.key_pressed(egui::Key::ArrowUp),
                        i.key_pressed(egui::Key::ArrowDown),
                        i.key_pressed(egui::Key::Tab),
                        i.key_pressed(egui::Key::ArrowRight),
                        i.key_pressed(egui::Key::Escape),
                    )
                });

                // ── History recall (Up/Down) ─────────────────────────────
                // Only engage history when the popup is not showing, to avoid
                // conflicts between the two navigation modes.
                if !show_popup {
                    if up && !self.recall.is_empty() {
                        let pos = match self.recall_pos {
                            Some(p) if p > 0 => p - 1,
                            Some(p) => p,
                            None => self.recall.len() - 1,
                        };
                        self.recall_pos = Some(pos);
                        self.input = self.recall[pos].clone();
                        self.suggest_for.clear();
                    }
                    if down && let Some(p) = self.recall_pos {
                        if p + 1 < self.recall.len() {
                            self.recall_pos = Some(p + 1);
                            self.input = self.recall[p + 1].clone();
                        } else {
                            self.recall_pos = None;
                            self.input.clear();
                        }
                        self.suggest_for.clear();
                    }
                } else {
                    // ── Popup navigation (Up/Down while popup is visible) ─
                    if up {
                        self.suggest_sel = Some(match self.suggest_sel {
                            Some(0) | None => self.suggestions.len().saturating_sub(1),
                            Some(p) => p - 1,
                        });
                    }
                    if down {
                        self.suggest_sel = Some(match self.suggest_sel {
                            None => 0,
                            Some(p) => (p + 1).min(self.suggestions.len().saturating_sub(1)),
                        });
                    }
                }

                // ── Accept suggestion (Tab or Right at end-of-line) ──────
                let at_end = self.input.len() == self.input.trim_end_matches(' ').len()
                    || !self.input.is_empty();
                if (tab || (right && at_end)) && show_popup {
                    self.accept_suggestion();
                }

                // ── Dismiss popup (Esc) ──────────────────────────────────
                if esc && show_popup {
                    self.suggest_dismissed = true;
                }
                // If the popup wasn't showing, Esc falls through to whatever
                // the draw-tool or canvas handles it as (we don't consume it).
            }

            if response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                submitted = Some(std::mem::take(&mut self.input));
                self.recall_pos = None;
                self.focus_next_frame = true;
                // Clear suggestions on submit.
                self.suggestions.clear();
                self.suggest_for.clear();
                self.suggest_dismissed = false;
            }
        });
        submitted
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_line_caps_history_at_500() {
        let mut cl = CommandLine::default();
        for i in 0..600 {
            cl.push_line(format!("line {i}"));
        }
        assert_eq!(cl.history.len(), 500);
        assert_eq!(cl.history.first().unwrap(), "line 100");
        assert_eq!(cl.history.last().unwrap(), "line 599");
    }

    #[test]
    fn execute_empty_line_is_a_no_op() {
        let mut cl = CommandLine::default();
        let mut session = Session::default();
        assert!(!cl.execute(&mut session, "   "));
        assert!(cl.recall.is_empty());
        assert_eq!(cl.history.len(), 1, "only the banner line");
    }

    #[test]
    fn execute_runs_command_and_records_recall() {
        let mut cl = CommandLine::default();
        let mut session = Session::default();
        assert!(cl.execute(&mut session, "  box 0,0,0 1,1,1  "));
        assert_eq!(session.doc.len(), 1);
        assert_eq!(cl.recall, vec!["box 0,0,0 1,1,1"]);
        assert!(cl.history.iter().any(|l| l == "> box 0,0,0 1,1,1"));
    }

    #[test]
    fn human_typed_export_still_runs_unaffected_by_deck_gate() {
        // The C-2/H-7 gate confines *deck-originated* side-effects only; a human
        // typing export at the command line must write the file as before.
        let mut cl = CommandLine::default();
        let mut session = Session::default();
        cl.execute(&mut session, "box 0,0,0 1,1,1");
        let out = std::env::temp_dir()
            .join(format!("itsjustcad_human_export_{}.csv", std::process::id()));
        let _ = std::fs::remove_file(&out);
        assert!(cl.execute(&mut session, &format!("export {}", out.display())));
        assert!(out.exists(), "human-typed export must write to disk");
        let _ = std::fs::remove_file(&out);
    }

    #[test]
    fn execute_parse_error_returns_false_and_echoes() {
        let mut cl = CommandLine::default();
        let mut session = Session::default();
        assert!(!cl.execute(&mut session, "frobnicate"));
        assert_eq!(session.doc.len(), 0);
        assert!(cl.history.iter().any(|l| l.starts_with("error: ")));
    }

    #[test]
    fn execute_help_lists_every_registry_command() {
        let mut cl = CommandLine::default();
        let mut session = Session::default();
        assert!(!cl.execute(&mut session, "help"));
        for spec in registry() {
            assert!(
                cl.history.iter().any(|l| l.contains(spec.usage)),
                "help missing usage for '{}'",
                spec.name
            );
        }
    }

    #[test]
    fn refresh_suggestions_populates_on_prefix() {
        let mut cl = CommandLine { input: "bo".to_string(), ..CommandLine::default() };
        cl.refresh_suggestions(&[], &[]);
        assert!(!cl.suggestions.is_empty(), "expected suggestions for 'bo'");
        assert!(cl.suggestions.iter().any(|s| s.completion == "box"), "{:?}", cl.suggestions);
    }

    #[test]
    fn refresh_suggestions_clears_on_new_input() {
        let mut cl = CommandLine { input: "bo".to_string(), ..CommandLine::default() };
        cl.refresh_suggestions(&[], &[]);
        let count_before = cl.suggestions.len();
        cl.input = "zzzz".to_string();
        cl.refresh_suggestions(&[], &[]);
        assert!(cl.suggestions.len() < count_before, "should be fewer (likely 0) for 'zzzz'");
    }

    #[test]
    fn accept_suggestion_completes_verb() {
        let mut cl = CommandLine { input: "bo".to_string(), ..CommandLine::default() };
        cl.refresh_suggestions(&[], &[]);
        assert!(!cl.suggestions.is_empty());
        cl.suggest_sel = Some(0); // should be 'box'
        cl.accept_suggestion();
        assert_eq!(cl.input, "box ");
    }

    #[test]
    fn suggestions_dismissed_after_accept() {
        let mut cl = CommandLine { input: "bo".to_string(), ..CommandLine::default() };
        cl.refresh_suggestions(&[], &[]);
        cl.suggest_sel = Some(0);
        cl.accept_suggestion();
        assert!(cl.suggest_dismissed, "popup should be dismissed after accept");
    }

    // ── Plugins ─────────────────────────────────────────────────────────────

    /// A CommandLine whose plugin dir is a fresh temp dir (no real home writes).
    fn plugin_cl(tag: &str) -> (CommandLine, std::path::PathBuf) {
        let dir = std::env::temp_dir()
            .join(format!("mydrafter-cl-plug-{}-{}", std::process::id(), tag));
        let _ = std::fs::remove_dir_all(&dir);
        let cl = CommandLine { plugin_dir: dir.clone(), ..CommandLine::default() };
        (cl, dir)
    }

    #[test]
    fn define_persist_invoke_cycle() {
        let (mut cl, dir) = plugin_cl("cycle");
        let mut session = Session::default();
        // LLM-style inline define with a positional param + default.
        let json = r#"{"name":"tinybox","description":"a box","params":[{"name":"s","default":"2"}],"body":["box 0,0,0 {0},{0},{0}"]}"#;
        cl.execute(&mut session, &format!("plugin define {json}"));
        assert!(session.plugins.contains("tinybox"));
        // Persisted to disk.
        assert!(dir.join("tinybox.plugin.json").exists());
        // Autosuggest cache updated.
        assert!(cl.plugin_verbs.iter().any(|(n, _)| n == "tinybox"));

        // Invoke with an arg → expands and creates geometry.
        let changed = cl.execute(&mut session, "tinybox 3");
        assert!(changed);
        assert_eq!(session.doc.len(), 1);
        // The expanded op — not the plugin call — is what got logged.
        assert_eq!(cl.logged_inputs.last().unwrap(), "box 0,0,0 3,3,3");

        // A fresh registry loading the same dir sees the plugin (persistence).
        let (loaded, _) = PluginRegistry::load_dir(&dir);
        assert!(loaded.contains("tinybox"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn invoke_uses_default_when_arg_omitted() {
        let (mut cl, dir) = plugin_cl("default");
        let mut session = Session::default();
        let json = r#"{"name":"defbox","params":[{"name":"s","default":"2"}],"body":["box 0,0,0 {0},{0},{0}"]}"#;
        cl.execute(&mut session, &format!("plugin define {json}"));
        cl.execute(&mut session, "defbox");
        assert_eq!(cl.logged_inputs.last().unwrap(), "box 0,0,0 2,2,2");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_from_history_roundtrip() {
        let (mut cl, dir) = plugin_cl("save");
        let mut session = Session::default();
        cl.execute(&mut session, "box 0,0,0 1,1,1");
        cl.execute(&mut session, "box 2,0,0 1,1,1");
        assert_eq!(cl.logged_inputs.len(), 2);
        cl.execute(&mut session, "plugin save twoboxes 2");
        assert!(session.plugins.contains("twoboxes"));
        let p = session.plugins.get("twoboxes").unwrap();
        assert_eq!(p.body, vec!["box 0,0,0 1,1,1", "box 2,0,0 1,1,1"]);

        // Invoking the saved plugin re-creates the same two boxes.
        let mut session2 = Session::default();
        let (loaded, _) = PluginRegistry::load_dir(&dir);
        session2.plugins = loaded;
        cl.execute(&mut session2, "twoboxes");
        assert_eq!(session2.doc.len(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn delete_removes_plugin() {
        let (mut cl, dir) = plugin_cl("del");
        let mut session = Session::default();
        let json = r#"{"name":"gone","body":["box 0,0,0 1,1,1"]}"#;
        cl.execute(&mut session, &format!("plugin define {json}"));
        assert!(session.plugins.contains("gone"));
        cl.execute(&mut session, "plugin delete gone");
        assert!(!session.plugins.contains("gone"));
        assert!(!dir.join("gone.plugin.json").exists());
        assert!(!cl.plugin_verbs.iter().any(|(n, _)| n == "gone"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn autosuggest_includes_plugin_names() {
        let (mut cl, dir) = plugin_cl("suggest");
        let mut session = Session::default();
        let json = r#"{"name":"colgrid","body":["box 0,0,0 1,1,1"]}"#;
        cl.execute(&mut session, &format!("plugin define {json}"));
        cl.input = "col".to_string();
        cl.refresh_suggestions(&[], &[]);
        assert!(
            cl.suggestions.iter().any(|s| s.completion == "colgrid"),
            "{:?}",
            cl.suggestions
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn builtin_name_is_not_shadowed_by_plugin() {
        // A plugin named after a builtin must not intercept the builtin verb.
        let (mut cl, dir) = plugin_cl("shadow");
        let mut session = Session::default();
        // Force a plugin called "box" straight into the registry (define guards
        // the filename, not the name-vs-builtin clash — invocation guards it).
        session.plugins.insert(Plugin {
            name: "box".into(),
            description: String::new(),
            params: vec![],
            body: vec!["circle 0,0,0 99".into()],
        });
        cl.execute(&mut session, "box 0,0,0 1,1,1");
        // The builtin box ran (a mesh box), not the plugin's circle.
        assert_eq!(session.doc.len(), 1);
        assert_eq!(cl.logged_inputs.last().unwrap(), "box 0,0,0 1,1,1");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
