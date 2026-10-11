// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! Pure, headless-testable parsing for the inline command UI (issue #28):
//!
//! - [`parse_usage`] turns a registry `usage` string (e.g.
//!   `box <corner x,y,z> <size x,y,z>`) into an ordered list of [`Slot`]s so the
//!   composer can label the token being typed and dim the optional tail.
//! - [`active_slot`] maps the live input text to the slot index the cursor is in.
//! - [`classify_tokens`] colours a live command line by token role (verb /
//!   number / selector / string) — the iOS highlighter, ported as a pure span
//!   list so the colour mapping is testable without egui.
//! - [`slash`] classifies a chat input: is it a `/`-armed command, and if so
//!   what is the partial verb (for the inline palette)?
//!
//! All functions here are pure and unit-tested. The egui glue in
//! `command_line.rs` / `deck_pane.rs` is a thin renderer over them.

/// Whether a usage slot is required (`<...>`) or optional (`[...]`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotReq {
    Required,
    Optional,
}

/// The value a slot expects, inferred from its placeholder name. Drives the
/// live highlighter colour and the ghost-label hint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotKind {
    /// A coordinate triple/pair like `x,y,z` (`corner`, `point`, `dir`, …).
    Point,
    /// A scalar number (`height`, `radius`, `count`, …).
    Number,
    /// An object selector (`<selector>` / `<sel>`).
    Selector,
    /// An angle in degrees.
    Angle,
    /// Any other free token (names, paths, mode keywords).
    String,
}

/// One positional slot of a command's usage string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Slot {
    /// Human-readable name shown as the ghost hint (e.g. `size (x,y,z)`).
    pub name: String,
    pub req: SlotReq,
    pub kind: SlotKind,
}

/// Infer a [`SlotKind`] from a placeholder's inner text (lower-cased contents
/// between the brackets, e.g. `corner x,y,z` or `selector`).
fn kind_of(inner: &str) -> SlotKind {
    let l = inner.to_lowercase();
    if l.contains("selector") || l == "sel" || l.contains("<sel") {
        return SlotKind::Selector;
    }
    if l.contains("angle") || l.contains("deg") {
        return SlotKind::Angle;
    }
    // A coordinate placeholder names its axes: `x,y,z`, `x,y`, `point`, `dir`.
    if l.contains("x,y") || l.contains("point") || l.contains("corner") || l.contains("dir") {
        return SlotKind::Point;
    }
    if l.contains("height")
        || l.contains("radius")
        || l.contains("count")
        || l.contains("size")
        || l.contains("distance")
        || l.contains("dist")
        || l.contains("length")
        || l.contains("width")
        || l.contains("depth")
        || l.contains("<n>")
        || l.contains(" n")
        || l == "n"
        || l.contains("factor")
        || l.contains("spacing")
    {
        // `size x,y,z` already matched Point above; this catches scalar sizes.
        return SlotKind::Number;
    }
    SlotKind::String
}

/// A short, friendly label for a slot — the placeholder's inner text with the
/// bracket kind stripped. `corner x,y,z` → `corner (x,y,z)`; `selector` stays.
fn label_of(inner: &str) -> String {
    // Split a leading name from a trailing axis spec: `corner x,y,z`.
    let trimmed = inner.trim();
    if let Some((name, axes)) = trimmed.split_once(' ')
        && axes.contains(',')
    {
        return format!("{name} ({axes})");
    }
    trimmed.to_string()
}

/// Parse a registry `usage` string into its ordered positional slots. The first
/// whitespace token is the verb and is dropped (it is coloured as the verb, not
/// a slot). `<...>` → required, `[...]` → optional. Tokens that are neither
/// (literal keywords in a usage) are skipped — the positional model only tracks
/// bracketed placeholders.
pub fn parse_usage(usage: &str) -> Vec<Slot> {
    let mut slots = Vec::new();
    // Drop the verb (first token).
    let body = usage
        .trim()
        .split_once(char::is_whitespace)
        .map(|x| x.1)
        .unwrap_or("");
    let mut chars = body.char_indices().peekable();
    while let Some(&(start, c)) = chars.peek() {
        let (open, close, req) = match c {
            '<' => ('<', '>', SlotReq::Required),
            '[' => ('[', ']', SlotReq::Optional),
            _ => {
                chars.next();
                continue;
            }
        };
        let _ = open;
        // Consume until the matching close bracket.
        chars.next(); // consume opener
        let mut inner = String::new();
        let mut closed = false;
        for (_, ch) in chars.by_ref() {
            if ch == close {
                closed = true;
                break;
            }
            inner.push(ch);
        }
        let _ = start;
        if closed {
            slots.push(Slot {
                name: label_of(&inner),
                req,
                kind: kind_of(&inner),
            });
        }
    }
    slots
}

/// The index of the slot the user is currently typing, given the live `input`
/// (which INCLUDES the verb token). Returns `None` when still typing the verb
/// (no trailing space yet) or when past the last slot.
///
/// A trailing space means "on the next slot": `box 0,0,0 ` → slot 1 (size).
pub fn active_slot(input: &str, slot_count: usize) -> Option<usize> {
    if slot_count == 0 {
        return None;
    }
    let trimmed = input.trim_start();
    // Verb not finished yet (no space) → no active slot.
    if !trimmed.contains(char::is_whitespace) {
        return None;
    }
    // Count arg tokens AFTER the verb. A trailing space starts a fresh token.
    let after_verb = trimmed
        .split_once(char::is_whitespace)
        .map(|x| x.1)
        .unwrap_or("");
    let ends_space = input.ends_with(' ') || input.ends_with('\t');
    let typed = after_verb.split_whitespace().count();
    // Index of the slot being typed: if mid-token it's `typed - 1`; if a
    // trailing space just began a new token it's `typed`.
    let idx = if ends_space { typed } else { typed.saturating_sub(1) };
    if idx < slot_count {
        Some(idx)
    } else {
        None
    }
}

/// A classified span of a command line, for syntax highlighting. The byte range
/// is `[start, end)` into the original line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenSpan {
    pub start: usize,
    pub end: usize,
    pub role: TokenRole,
}

/// The colour role of a token, mirroring the iOS highlighter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenRole {
    /// First token — the verb (accent + bold).
    Verb,
    /// A number or comma-joined coordinate (teal).
    Number,
    /// A selector keyword `last`/`all`/`sel`/`selected` or a `key:value` (orange).
    Selector,
    /// Anything else (primary text).
    Plain,
}

/// Selector keywords coloured like selectors (matches the iOS set + `sel`).
const SELECTOR_WORDS: &[&str] = &["last", "all", "sel", "selected"];

/// True for `5`, `-3.2`, `0,0,0`, `1,2` — a number or comma-joined coordinate.
fn is_numeric(tok: &str) -> bool {
    let mut any = false;
    for part in tok.split(',') {
        if part.is_empty() {
            return false;
        }
        if part.parse::<f64>().is_err() {
            return false;
        }
        any = true;
    }
    any
}

/// Classify a command line into coloured spans (pure; no egui). The first token
/// is the verb; subsequent tokens are numbers/coords (teal), selector keywords
/// or `key:value` (orange), else plain. Whitespace between tokens is not
/// emitted — only the token spans are, so the renderer can colour each run.
pub fn classify_tokens(line: &str) -> Vec<TokenSpan> {
    let mut spans = Vec::new();
    let bytes = line.as_bytes();
    let mut i = 0usize;
    let mut first = true;
    while i < bytes.len() {
        // Skip whitespace.
        while i < bytes.len() && (bytes[i] == b' ' || bytes[i] == b'\t') {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }
        let start = i;
        while i < bytes.len() && bytes[i] != b' ' && bytes[i] != b'\t' {
            i += 1;
        }
        let tok = &line[start..i];
        let role = if first {
            TokenRole::Verb
        } else if is_numeric(tok) {
            TokenRole::Number
        } else if SELECTOR_WORDS.contains(&tok.to_lowercase().as_str()) || tok.contains(':') {
            TokenRole::Selector
        } else {
            TokenRole::Plain
        };
        spans.push(TokenSpan { start, end: i, role });
        first = false;
    }
    spans
}

/// One segment of the live labeled parse-preview (issue #28 FIX 2): a slot
/// paired with the value the user has typed into it so far.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreviewSlot {
    /// The slot's friendly name, e.g. `corner (x,y,z)` or `height`.
    pub name: String,
    /// The typed token for this slot, if any (`None` for an unfilled slot).
    pub value: Option<String>,
    /// True while the cursor is in this slot (the one being typed right now).
    pub active: bool,
    pub req: SlotReq,
    /// A validation complaint for `value`, e.g. a malformed point. `None` when
    /// the value is absent or well-formed for the slot's [`SlotKind`].
    pub error: Option<String>,
}

/// Validate a typed `value` against a slot `kind`, returning a human message
/// when it is malformed. Only the point/number cases are checked (the common
/// mistakes); everything else is accepted. Pure.
fn validate_value(kind: SlotKind, value: &str) -> Option<String> {
    match kind {
        SlotKind::Point => {
            // A point is 2–3 comma-joined numbers (`x,y` or `x,y,z`).
            let parts: Vec<&str> = value.split(',').collect();
            let all_num =
                parts.iter().all(|p| !p.is_empty() && p.parse::<f64>().is_ok());
            if !all_num || !(2..=3).contains(&parts.len()) {
                return Some(format!(
                    "'{value}' is not a point; write x,y or x,y,z (example: 0,0,0)"
                ));
            }
            None
        }
        SlotKind::Number | SlotKind::Angle => {
            if value.parse::<f64>().is_err() {
                return Some(format!("'{value}' is not a number (example: 3.5)"));
            }
            None
        }
        SlotKind::Selector | SlotKind::String => None,
    }
}

/// Build the live labeled parse-preview for `input` (which INCLUDES the verb)
/// against a command's `usage` string (issue #28 FIX 2). Each positional
/// argument the user has typed is paired with its slot so the composer can show
/// `corner(0,0,0) size(1,2,3)` as read-only feedback. Returns `None` when the
/// usage has no slots (nothing to preview) or the user is still typing the verb
/// (no slot reached yet). Pure — never mutates the executed command string.
///
/// - Filled slots carry their typed `value` (and a validation `error` if any).
/// - The slot currently being typed is marked `active`.
/// - Unfilled slots carry `value: None` (the renderer dims them; required ones
///   stand out from optional ones via `req`).
pub fn parse_preview(input: &str, usage: &str) -> Option<Vec<PreviewSlot>> {
    let slots = parse_usage(usage);
    if slots.is_empty() {
        return None;
    }
    // Argument tokens after the verb (the verb is the first whitespace token).
    let after_verb = input
        .trim_start()
        .split_once(char::is_whitespace)
        .map(|x| x.1)
        .unwrap_or("");
    // No args typed and no trailing space → still on the verb, nothing to show.
    if after_verb.trim().is_empty() && !input.ends_with(char::is_whitespace) {
        return None;
    }
    let args: Vec<&str> = after_verb.split_whitespace().collect();
    let active = active_slot(input, slots.len());
    let out = slots
        .iter()
        .enumerate()
        .map(|(i, slot)| {
            let value = args.get(i).map(|s| s.to_string());
            let error = value
                .as_deref()
                .and_then(|v| validate_value(slot.kind, v));
            PreviewSlot {
                name: slot.name.clone(),
                value,
                active: active == Some(i),
                req: slot.req,
                error,
            }
        })
        .collect();
    Some(out)
}

/// Render a [`parse_preview`] result to a compact one-line string for tests and
/// logging, e.g. `corner(0,0,0) size(1,2,3)`. Filled slots show
/// `name(value)`; the active-but-empty slot shows `name(…)`; other unfilled
/// slots show `name`. Pure. Used by tests and available for logging the
/// resolved command shape; the egui renderer colours each segment itself.
#[cfg_attr(not(test), allow(dead_code))]
pub fn preview_line(slots: &[PreviewSlot]) -> String {
    slots
        .iter()
        .map(|s| match &s.value {
            Some(v) => format!("{}={v}", first_word(&s.name)),
            None if s.active => format!("{}=…", first_word(&s.name)),
            None => first_word(&s.name).to_string(),
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// The leading word of a slot name — `corner (x,y,z)` → `corner`, `height` →
/// `height` — so the preview reads `corner(0,0,0)` not `corner (x,y,z)(0,0,0)`.
#[cfg_attr(not(test), allow(dead_code))]
fn first_word(name: &str) -> &str {
    name.split_whitespace().next().unwrap_or(name)
}

/// The result of classifying a chat input for the `/` inline palette.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlashState {
    /// Not a slash command — send to the LLM as normal chat.
    Chat,
    /// A `/`-armed command whose verb is still being typed (no trailing space):
    /// show the palette filtered by this partial verb (empty for just `/`).
    Arming(String),
    /// A `/`-command with a completed verb (a space follows it): the palette is
    /// closed; on submit, run the rest as a command.
    Command,
}

/// Classify a chat input for the inline `/` palette and leading-`/` routing.
///
/// Leading whitespace is allowed before the `/`. A space after the verb ends
/// the arming (palette closes). A non-slash input is plain chat.
pub fn slash(input: &str) -> SlashState {
    let trimmed = input.trim_start();
    let Some(rest) = trimmed.strip_prefix('/') else {
        return SlashState::Chat;
    };
    // The verb token ends at the first whitespace. If any whitespace is present,
    // the verb is complete → command (palette closed).
    match rest.split_once(char::is_whitespace) {
        Some(_) => SlashState::Command,
        None => SlashState::Arming(rest.to_string()),
    }
}

/// Strip a leading `/` (and any following whitespace) from a submitted chat
/// line, yielding the raw command to run. Returns `None` when the line is not a
/// slash command or is empty after the slash.
pub fn strip_slash(input: &str) -> Option<String> {
    let trimmed = input.trim();
    let rest = trimmed.strip_prefix('/')?.trim_start();
    if rest.is_empty() {
        None
    } else {
        Some(rest.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── parse_usage ─────────────────────────────────────────────────────────

    #[test]
    fn parse_box_two_point_slots() {
        let slots = parse_usage("box <corner x,y,z> <size x,y,z>");
        assert_eq!(slots.len(), 2);
        assert_eq!(slots[0].name, "corner (x,y,z)");
        assert_eq!(slots[0].req, SlotReq::Required);
        assert_eq!(slots[0].kind, SlotKind::Point);
        assert_eq!(slots[1].name, "size (x,y,z)");
        assert_eq!(slots[1].kind, SlotKind::Point);
    }

    #[test]
    fn parse_extrude_selector_then_number() {
        let slots = parse_usage("extrude <selector> <height>");
        assert_eq!(slots.len(), 2);
        assert_eq!(slots[0].kind, SlotKind::Selector);
        assert_eq!(slots[1].kind, SlotKind::Number);
        assert_eq!(slots[1].name, "height");
    }

    #[test]
    fn parse_revolve_optional_slots() {
        let slots =
            parse_usage("revolve <selector> [axis point x,y,z] [axis dir x,y,z] [angle deg]");
        assert_eq!(slots.len(), 4);
        assert_eq!(slots[0].req, SlotReq::Required);
        assert_eq!(slots[0].kind, SlotKind::Selector);
        assert_eq!(slots[1].req, SlotReq::Optional);
        assert_eq!(slots[1].kind, SlotKind::Point);
        assert_eq!(slots[3].req, SlotReq::Optional);
        assert_eq!(slots[3].kind, SlotKind::Angle);
    }

    #[test]
    fn parse_usage_no_slots() {
        assert!(parse_usage("undo").is_empty());
        assert!(parse_usage("").is_empty());
    }

    // ── active_slot ─────────────────────────────────────────────────────────

    #[test]
    fn active_slot_none_while_typing_verb() {
        assert_eq!(active_slot("bo", 2), None);
        assert_eq!(active_slot("box", 2), None);
    }

    #[test]
    fn active_slot_first_after_verb_space() {
        // `box ` → on the first slot.
        assert_eq!(active_slot("box ", 2), Some(0));
    }

    #[test]
    fn active_slot_mid_first_token() {
        // Typing the first arg, still slot 0.
        assert_eq!(active_slot("box 0,0", 2), Some(0));
        assert_eq!(active_slot("box 0,0,0", 2), Some(0));
    }

    #[test]
    fn active_slot_advances_on_trailing_space() {
        // `box 0,0,0 ` → on slot 1 (size).
        assert_eq!(active_slot("box 0,0,0 ", 2), Some(1));
        // mid-second token, still slot 1.
        assert_eq!(active_slot("box 0,0,0 5,5", 2), Some(1));
    }

    #[test]
    fn active_slot_past_last_is_none() {
        // Both slots typed + trailing space → past the end.
        assert_eq!(active_slot("box 0,0,0 5,5,3 ", 2), None);
    }

    #[test]
    fn active_slot_zero_slots_is_none() {
        assert_eq!(active_slot("undo ", 0), None);
    }

    // ── classify_tokens ──────────────────────────────────────────────────────

    #[test]
    fn classify_verb_numbers_points() {
        let spans = classify_tokens("box 0,0,0 5,5,3");
        assert_eq!(spans.len(), 3);
        assert_eq!(spans[0].role, TokenRole::Verb);
        assert_eq!(&"box 0,0,0 5,5,3"[spans[0].start..spans[0].end], "box");
        assert_eq!(spans[1].role, TokenRole::Number);
        assert_eq!(spans[2].role, TokenRole::Number);
    }

    #[test]
    fn classify_selector_keyword_orange() {
        let spans = classify_tokens("extrude last 3");
        assert_eq!(spans[0].role, TokenRole::Verb);
        assert_eq!(spans[1].role, TokenRole::Selector);
        assert_eq!(spans[2].role, TokenRole::Number);
    }

    #[test]
    fn classify_key_value_is_selector() {
        let spans = classify_tokens("edgefx jitter:0.5");
        assert_eq!(spans[1].role, TokenRole::Selector);
    }

    #[test]
    fn classify_plain_token() {
        let spans = classify_tokens("save floor-plan");
        assert_eq!(spans[1].role, TokenRole::Plain);
    }

    #[test]
    fn classify_negative_and_decimal_numbers() {
        let spans = classify_tokens("move last -3.5,0,2");
        assert_eq!(spans[2].role, TokenRole::Number);
    }

    #[test]
    fn classify_handles_extra_whitespace() {
        let spans = classify_tokens("  box   0,0,0  ");
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].role, TokenRole::Verb);
        assert_eq!(spans[1].role, TokenRole::Number);
    }

    #[test]
    fn classify_empty_is_empty() {
        assert!(classify_tokens("").is_empty());
        assert!(classify_tokens("   ").is_empty());
    }

    // ── parse_preview / preview_line ─────────────────────────────────────────

    #[test]
    fn preview_box_two_points_filled() {
        let p = parse_preview("box 0,0,0 1,2,3", "box <corner x,y,z> <size x,y,z>")
            .expect("box has slots");
        assert_eq!(preview_line(&p), "corner=0,0,0 size=1,2,3");
        assert!(p.iter().all(|s| s.error.is_none()));
    }

    #[test]
    fn preview_partial_marks_active_slot() {
        // Second slot being typed: first filled, second active with a value.
        let p = parse_preview("box 0,0,0 1,2", "box <corner x,y,z> <size x,y,z>").unwrap();
        assert_eq!(p[0].value.as_deref(), Some("0,0,0"));
        assert!(!p[0].active);
        assert_eq!(p[1].value.as_deref(), Some("1,2"));
        assert!(p[1].active);
        assert_eq!(preview_line(&p), "corner=0,0,0 size=1,2");
    }

    #[test]
    fn preview_trailing_space_shows_empty_active_slot() {
        // `box 0,0,0 ` → first filled, second active-but-empty (shown as `…`).
        let p = parse_preview("box 0,0,0 ", "box <corner x,y,z> <size x,y,z>").unwrap();
        assert_eq!(p[0].value.as_deref(), Some("0,0,0"));
        assert_eq!(p[1].value, None);
        assert!(p[1].active);
        assert_eq!(preview_line(&p), "corner=0,0,0 size=…");
    }

    #[test]
    fn preview_still_typing_verb_is_none() {
        assert_eq!(parse_preview("bo", "box <corner x,y,z> <size x,y,z>"), None);
        assert_eq!(parse_preview("box", "box <corner x,y,z> <size x,y,z>"), None);
    }

    #[test]
    fn preview_verb_with_space_shows_first_slot_dim() {
        let p = parse_preview("box ", "box <corner x,y,z> <size x,y,z>").unwrap();
        assert_eq!(p[0].value, None);
        assert!(p[0].active);
        assert_eq!(preview_line(&p), "corner=… size");
    }

    #[test]
    fn preview_optional_slots_unfilled() {
        let p = parse_preview(
            "revolve last ",
            "revolve <selector> [axis point x,y,z] [angle deg]",
        )
        .unwrap();
        assert_eq!(p[0].value.as_deref(), Some("last"));
        assert_eq!(p[1].req, SlotReq::Optional);
        assert_eq!(p[2].req, SlotReq::Optional);
        assert_eq!(preview_line(&p), "selector=last axis=… angle");
    }

    #[test]
    fn preview_unknown_verb_no_slots_is_none() {
        assert_eq!(parse_preview("undo ", "undo"), None);
        assert_eq!(parse_preview("undo ", ""), None);
    }

    #[test]
    fn preview_malformed_point_flags_specific_error() {
        let p = parse_preview("box abc 1,2,3", "box <corner x,y,z> <size x,y,z>").unwrap();
        let err = p[0].error.as_deref().expect("malformed point flagged");
        assert!(err.contains("not a point"), "{err}");
        assert!(err.contains("0,0,0"), "example present: {err}");
        assert!(p[1].error.is_none(), "well-formed point not flagged");
    }

    #[test]
    fn preview_malformed_number_flagged() {
        let p = parse_preview("extrude last foo", "extrude <selector> <height>").unwrap();
        assert!(p[0].error.is_none(), "selector accepts any token");
        assert!(p[1].error.as_deref().unwrap().contains("not a number"));
    }

    // ── slash ─────────────────────────────────────────────────────────────────

    #[test]
    fn slash_plain_text_is_chat() {
        assert_eq!(slash("draw a box"), SlashState::Chat);
        assert_eq!(slash(""), SlashState::Chat);
        assert_eq!(slash("   "), SlashState::Chat);
    }

    #[test]
    fn slash_bare_slash_arms_empty() {
        assert_eq!(slash("/"), SlashState::Arming(String::new()));
    }

    #[test]
    fn slash_partial_verb_arms_with_prefix() {
        assert_eq!(slash("/bo"), SlashState::Arming("bo".to_string()));
        // Leading whitespace is tolerated.
        assert_eq!(slash("  /box"), SlashState::Arming("box".to_string()));
    }

    #[test]
    fn slash_space_ends_the_verb() {
        assert_eq!(slash("/box "), SlashState::Command);
        assert_eq!(slash("/box 0,0,0"), SlashState::Command);
    }

    // ── strip_slash ─────────────────────────────────────────────────────────

    #[test]
    fn strip_slash_yields_command() {
        assert_eq!(strip_slash("/box 0,0,0 5,5,3"), Some("box 0,0,0 5,5,3".to_string()));
        assert_eq!(strip_slash("  /ze  "), Some("ze".to_string()));
    }

    #[test]
    fn strip_slash_non_slash_is_none() {
        assert_eq!(strip_slash("box 0,0,0"), None);
        assert_eq!(strip_slash("/"), None);
        assert_eq!(strip_slash("/   "), None);
    }
}
