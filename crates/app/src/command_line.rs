use mydrafter_commands::{parse, registry, Command, Session};

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
}

impl Default for CommandLine {
    fn default() -> Self {
        Self {
            input: String::new(),
            history: vec!["mydrafter — type 'help' for commands".to_string()],
            recall: Vec::new(),
            recall_pos: None,
            focus_next_frame: true,
            suggestions: Vec::new(),
            suggest_sel: None,
            suggest_dismissed: false,
            suggest_for: String::new(),
        }
    }
}

impl CommandLine {
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
            self.push_line(format!("  {}", mydrafter_commands::SELECTOR_HELP));
            return false;
        }

        match parse(line) {
            Ok(cmd) => match session.run(cmd) {
                Ok(outcome) => {
                    self.push_line(outcome.message);
                    true
                }
                Err(e) => {
                    self.push_line(format!("error: {e}"));
                    false
                }
            },
            Err(e) => {
                self.push_line(format!("error: {e}"));
                false
            }
        }
    }

    /// Run a pre-built command, echoing `line` like a typed input. Used when
    /// the app fills fields the parser cannot (e.g. the viewport camera for
    /// `view save`). Returns true when the document changed.
    pub fn execute_command(&mut self, session: &mut Session, line: &str, cmd: Command) -> bool {
        self.recall.push(line.to_string());
        self.push_line(format!("> {line}"));
        match session.run(cmd) {
            Ok(outcome) => {
                self.push_line(outcome.message);
                true
            }
            Err(e) => {
                self.push_line(format!("error: {e}"));
                false
            }
        }
    }

    /// Pre-fill the command input with `text` without executing it.
    /// Used by the MYDRAFTER_TYPE dev hook to make the autosuggest popup
    /// visible in MYDRAFTER_SHOT screenshots.
    pub fn prefill(&mut self, text: String) {
        self.input = text;
        self.suggest_for.clear(); // force suggestion refresh
    }

    /// Recompute suggestions when `input` has changed.
    fn refresh_suggestions(&mut self, object_names: &[String]) {
        if self.input == self.suggest_for {
            return;
        }
        self.suggest_for = self.input.clone();
        self.suggest_dismissed = false;
        self.suggest_sel = None;
        self.suggestions = suggest::suggestions(&self.input, object_names, MAX_SUGGESTIONS);
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
    /// to populate selector suggestions. Pass `doc.objects().filter_map(|o|
    /// o.name.clone()).collect::<Vec<_>>()` from the caller.
    pub fn ui(&mut self, ui: &mut egui::Ui, object_names: &[String]) -> Option<String> {
        let mut submitted = None;

        egui::ScrollArea::vertical()
            .max_height(96.0)
            .stick_to_bottom(true)
            .show(ui, |ui| {
                for line in &self.history {
                    ui.monospace(line);
                }
            });

        // Recompute suggestions if input changed.
        self.refresh_suggestions(object_names);

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
                        .small()
                        .font(egui::FontId::monospace(10.0)),
                )
                .wrap(),
            );
        }

        // ── Autosuggest popup (above the input row) ──────────────────────
        let show_popup = !self.suggest_dismissed
            && !self.suggestions.is_empty()
            && !show_usage_hint; // hide popup once verb is selected

        if show_popup {
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

        // ── Input row ────────────────────────────────────────────────────
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
        let mut cl = CommandLine::default();
        cl.input = "bo".to_string();
        cl.refresh_suggestions(&[]);
        assert!(!cl.suggestions.is_empty(), "expected suggestions for 'bo'");
        assert!(cl.suggestions.iter().any(|s| s.completion == "box"), "{:?}", cl.suggestions);
    }

    #[test]
    fn refresh_suggestions_clears_on_new_input() {
        let mut cl = CommandLine::default();
        cl.input = "bo".to_string();
        cl.refresh_suggestions(&[]);
        let count_before = cl.suggestions.len();
        cl.input = "zzzz".to_string();
        cl.refresh_suggestions(&[]);
        assert!(cl.suggestions.len() < count_before, "should be fewer (likely 0) for 'zzzz'");
    }

    #[test]
    fn accept_suggestion_completes_verb() {
        let mut cl = CommandLine::default();
        cl.input = "bo".to_string();
        cl.refresh_suggestions(&[]);
        assert!(!cl.suggestions.is_empty());
        cl.suggest_sel = Some(0); // should be 'box'
        cl.accept_suggestion();
        assert_eq!(cl.input, "box ");
    }

    #[test]
    fn suggestions_dismissed_after_accept() {
        let mut cl = CommandLine::default();
        cl.input = "bo".to_string();
        cl.refresh_suggestions(&[]);
        cl.suggest_sel = Some(0);
        cl.accept_suggestion();
        assert!(cl.suggest_dismissed, "popup should be dismissed after accept");
    }
}
