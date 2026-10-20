// SPDX-License-Identifier: AGPL-3.0-only
// Copyright © 2026 Hector Tarrido-Picart

/// Streaming extractor: splits model output into chat text and command lines
/// from ```draft fenced blocks, emitting each command the moment its newline
/// arrives — that is what makes geometry appear live while tokens stream.
#[derive(Default)]
pub struct Extractor {
    buffer: String,
    in_fence: bool,
    /// Inside a non-draft fence (```json, ```text, …): its lines are chat, and
    /// its closing ``` must close it rather than open a draft fence.
    in_other_fence: bool,
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
                } else if self.in_other_fence {
                    self.in_other_fence = false;
                    events.push(ExtractEvent::Chat(line));
                } else if trimmed == "```draft" || trimmed == "```" {
                    self.in_fence = true;
                } else {
                    // some other fenced block (e.g. ```text) — treat as chat
                    self.in_other_fence = true;
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
            self.in_fence = false;
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

    fn commands(events: &[ExtractEvent]) -> Vec<String> {
        events
            .iter()
            .filter_map(|e| match e {
                ExtractEvent::Command(c) => Some(c.clone()),
                _ => None,
            })
            .collect()
    }

    fn chat(events: &[ExtractEvent]) -> String {
        events
            .iter()
            .filter_map(|e| match e {
                ExtractEvent::Chat(c) => Some(c.as_str()),
                _ => None,
            })
            .collect()
    }

    /// Stream `text` in the given chunks and collect every event.
    fn stream(chunks: &[&str]) -> Vec<ExtractEvent> {
        let mut ex = Extractor::default();
        let mut events = Vec::new();
        for c in chunks {
            events.extend(ex.push(c));
        }
        events.extend(ex.finish());
        events
    }

    /// Split `text` at every char boundary in `cuts` (byte offsets).
    fn split_at<'a>(text: &'a str, cuts: &[usize]) -> Vec<&'a str> {
        let mut out = Vec::new();
        let mut start = 0;
        for &c in cuts {
            if c > start && c < text.len() && text.is_char_boundary(c) {
                out.push(&text[start..c]);
                start = c;
            }
        }
        out.push(&text[start..]);
        out
    }

    const REPLY: &str = "Here’s a box and a grid of it — sized 4×4×3.\n```draft\nbox 0,0,0 4,4,3\narray last 3,3,1 15,15,0\ngeodesic 3 8 full\nmove last 0,0,10\n```\nThat should size it nicely.\n";
    const WANT: [&str; 4] = [
        "box 0,0,0 4,4,3",
        "array last 3,3,1 15,15,0",
        "geodesic 3 8 full",
        "move last 0,0,10",
    ];

    #[test]
    fn every_two_way_split_emits_whole_lines_only() {
        // Every single cut point, including inside `size`, inside a fence and
        // between the backticks: the commands are always exactly the four.
        for cut in 0..=REPLY.len() {
            let events = stream(&split_at(REPLY, &[cut]));
            assert_eq!(commands(&events), WANT, "cut at {cut}");
            assert_eq!(
                chat(&events),
                "Here’s a box and a grid of it — sized 4×4×3.\nThat should size it nicely.\n",
                "cut at {cut}"
            );
        }
    }

    #[test]
    fn every_three_way_split_emits_whole_lines_only() {
        for a in 0..=REPLY.len() {
            for b in (a..=REPLY.len()).step_by(3) {
                let events = stream(&split_at(REPLY, &[a, b]));
                assert_eq!(commands(&events), WANT, "cuts at {a},{b}");
            }
        }
    }

    #[test]
    fn char_by_char_stream_emits_whole_lines_only() {
        let chunks: Vec<String> = REPLY.chars().map(String::from).collect();
        let refs: Vec<&str> = chunks.iter().map(String::as_str).collect();
        assert_eq!(commands(&stream(&refs)), WANT);
    }

    #[test]
    fn word_split_inside_size_and_fences_never_leaks_a_fragment() {
        // The shapes a provider actually produces: tokens that cut words like
        // `size` / `ze`, fences split across deltas, CRLF line ends.
        let events = stream(&[
            "Si", "ze it:\r\n``", "`dra", "ft\r\nbox 0,0,0 4,4,3\r", "\narray last 3,3,1 15,15,", "0\r\n`", "``\r\nsi", "ze done",
        ]);
        assert_eq!(commands(&events), vec!["box 0,0,0 4,4,3", "array last 3,3,1 15,15,0"]);
        assert!(!commands(&events).iter().any(|c| c == "ze" || c == "si"));
    }

    #[test]
    fn trailing_text_after_closing_fence_is_chat() {
        let events = stream(&["```draft\nbox 0,0,0 1,1,1\n```\nze"]);
        assert_eq!(commands(&events), vec!["box 0,0,0 1,1,1"]);
        assert_eq!(chat(&events), "ze");
    }

    #[test]
    fn closing_an_other_fence_does_not_open_a_draft_fence() {
        // A ```json block's closing fence used to flip the extractor INTO a
        // draft fence, so the prose after it streamed out as commands.
        let events = stream(&[
            "Params:\n```json\n{\"a\": 1}\n```\nThen I resize it.\n```draft\nscale last 2\n```\nDone.\n",
        ]);
        assert_eq!(commands(&events), vec!["scale last 2"]);
        assert!(chat(&events).contains("Then I resize it."));
        assert!(chat(&events).contains("{\"a\": 1}"));
    }

    #[test]
    fn two_draft_blocks_and_blank_lines() {
        let events = stream(&["```draft\n\nbox 0,0,0 1,1,1\n\n```\ntext\n```draft\ncopy last 2,0,0\n```"]);
        assert_eq!(commands(&events), vec!["box 0,0,0 1,1,1", "copy last 2,0,0"]);
    }
}
