use mydrafter_commands::{parse, registry, Session};

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
