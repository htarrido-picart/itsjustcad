use mydrafter_commands::{parse, registry, Command, Session};

/// Rhino-style command line: single input row + scrollback, up/down history.
pub struct CommandLine {
    input: String,
    history: Vec<String>,
    /// Executed inputs for up-arrow recall.
    recall: Vec<String>,
    recall_pos: Option<usize>,
    focus_next_frame: bool,
}

impl Default for CommandLine {
    fn default() -> Self {
        Self {
            input: String::new(),
            history: vec!["mydrafter — type 'help' for commands".to_string()],
            recall: Vec::new(),
            recall_pos: None,
            focus_next_frame: true,
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

    /// Returns Some(command) when the user pressed Enter.
    pub fn ui(&mut self, ui: &mut egui::Ui) -> Option<String> {
        let mut submitted = None;

        egui::ScrollArea::vertical()
            .max_height(96.0)
            .stick_to_bottom(true)
            .show(ui, |ui| {
                for line in &self.history {
                    ui.monospace(line);
                }
            });

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
                let (up, down) = ui.input(|i| {
                    (
                        i.key_pressed(egui::Key::ArrowUp),
                        i.key_pressed(egui::Key::ArrowDown),
                    )
                });
                if up && !self.recall.is_empty() {
                    let pos = match self.recall_pos {
                        Some(p) if p > 0 => p - 1,
                        Some(p) => p,
                        None => self.recall.len() - 1,
                    };
                    self.recall_pos = Some(pos);
                    self.input = self.recall[pos].clone();
                }
                if down && let Some(p) = self.recall_pos {
                    if p + 1 < self.recall.len() {
                        self.recall_pos = Some(p + 1);
                        self.input = self.recall[p + 1].clone();
                    } else {
                        self.recall_pos = None;
                        self.input.clear();
                    }
                }
            }

            if response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                submitted = Some(std::mem::take(&mut self.input));
                self.recall_pos = None;
                self.focus_next_frame = true;
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
}
