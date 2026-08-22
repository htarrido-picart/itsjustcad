/// Streaming extractor: splits model output into chat text and command lines
/// from ```draft fenced blocks, emitting each command the moment its newline
/// arrives — that is what makes geometry appear live while tokens stream.
#[derive(Default)]
pub struct Extractor {
    buffer: String,
    in_fence: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ExtractEvent {
    /// Chat prose (may be a partial line; forward to the transcript as-is).
    Chat(String),
    /// One complete command line from inside a draft fence.
    Command(String),
}

impl Extractor {
    pub fn push(&mut self, chunk: &str) -> Vec<ExtractEvent> {
        self.buffer.push_str(chunk);
        let mut events = Vec::new();

        // Process complete lines; keep the trailing partial line buffered.
        while let Some(newline) = self.buffer.find('\n') {
            let line: String = self.buffer.drain(..=newline).collect();
            let trimmed = line.trim();
            if trimmed.starts_with("```") {
                if self.in_fence {
                    self.in_fence = false;
                } else if trimmed == "```draft" || trimmed == "```" {
                    self.in_fence = true;
                } else {
                    // some other fenced block (e.g. ```text) — treat as chat
                    events.push(ExtractEvent::Chat(line));
                }
            } else if self.in_fence {
                if !trimmed.is_empty() {
                    events.push(ExtractEvent::Command(trimmed.to_string()));
                }
            } else {
                events.push(ExtractEvent::Chat(line));
            }
        }
        events
    }

    /// Flush any trailing text at end of stream.
    pub fn finish(&mut self) -> Vec<ExtractEvent> {
        if self.buffer.is_empty() {
            return Vec::new();
        }
        let rest = std::mem::take(&mut self.buffer);
        let trimmed = rest.trim();
        if trimmed.is_empty() || trimmed.starts_with("```") {
            return Vec::new();
        }
        if self.in_fence {
            vec![ExtractEvent::Command(trimmed.to_string())]
        } else {
            vec![ExtractEvent::Chat(rest)]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_commands_across_chunk_boundaries() {
        let mut ex = Extractor::default();
        let mut events = Vec::new();
        for chunk in ["Here you go:\n```dr", "aft\nbox 0,0,0 5,", "5,3\ncopy last 6,0,0\n``", "`\ndone!\n"] {
            events.extend(ex.push(chunk));
        }
        events.extend(ex.finish());
        let commands: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                ExtractEvent::Command(c) => Some(c.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(commands, vec!["box 0,0,0 5,5,3", "copy last 6,0,0"]);
        assert!(events.contains(&ExtractEvent::Chat("Here you go:\n".into())));
    }

    #[test]
    fn unterminated_fence_flushes_command() {
        let mut ex = Extractor::default();
        let mut events = ex.push("```draft\nbox 0,0,0 1,1,1");
        events.extend(ex.finish());
        assert_eq!(events, vec![ExtractEvent::Command("box 0,0,0 1,1,1".into())]);
    }
}
