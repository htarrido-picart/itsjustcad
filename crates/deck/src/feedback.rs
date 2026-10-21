// SPDX-License-Identifier: AGPL-3.0-only
// Copyright © 2026 Hector Tarrido-Picart

//! Alpha feedback tickets (GitHub issue #123). The deck (or the user) drafts a
//! `feedback bug|idea | <key>: <value> | …` line; the host shows a review sheet
//! and only on an explicit click opens a pre-filled web form in the browser,
//! where the user reviews and submits. Tickets are text only. Everything here
//! is pure: no I/O, no network.
//!
//! The per-category [`TemplateField`] tables are the single source of truth:
//! the deck prompt ([`prompt_help`]), the review sheet and the ticket text
//! ([`google_form_url`]) all derive from them.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Prefix of every Google Form URL the hosts may open.
pub const GOOGLE_FORM_URL_PREFIX: &str = "https://docs.google.com/forms/d/e/";
/// Cap on each percent-encoded ticket body (URL length limits).
const BODY_MAX: usize = 6000;
/// Cap on the percent-encoded title.
const TITLE_MAX: usize = 300;

/// The feedback Google Form, pre-filled through its
/// `entry.N` field ids (⋮ ▸ "Get pre-filled link" in the form editor).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GoogleForm {
    /// `https://docs.google.com/forms/d/e/<FORM_ID>/viewform`.
    pub prefill_base: &'static str,
    /// Short-answer "Title" field.
    pub title_entry: &'static str,
    /// Multiple-choice "Category" field with the options `Bug` / `Idea`.
    pub category_entry: &'static str,
    /// Paragraph "Details" field.
    pub details_entry: &'static str,
}

/// The feedback form — the one place to configure it. While `None` the sheet's
/// send button is disabled ("not configured in this build").
pub const GOOGLE_FORM: Option<GoogleForm> = Some(GoogleForm {
    prefill_base: "https://docs.google.com/forms/d/e/1FAIpQLSeoJsy98ji1SIxK7uQR8p_COEqaI2-Aw5gVfDACSZMONX_T_w/viewform",
    title_entry: "entry.580096760",
    category_entry: "entry.1431494796",
    details_entry: "entry.1516408728",
});

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FieldKind {
    /// One line of text.
    Line,
    /// Free text.
    Text,
    /// Numbered steps, stored one per line (`;`-separated on the verb line).
    Steps,
    /// One of [`FREQ_OPTIONS`].
    Freq,
}

/// One field of a ticket template.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TemplateField {
    /// Label on the verb line and key in the draft JSON.
    pub key: &'static str,
    /// English heading in the ticket text; the sheet localizes by `key`.
    pub label: &'static str,
    /// What the field holds (taught to the deck).
    pub hint: &'static str,
    pub kind: FieldKind,
}

const fn field(
    key: &'static str,
    label: &'static str,
    hint: &'static str,
    kind: FieldKind,
) -> TemplateField {
    TemplateField {
        key,
        label,
        hint,
        kind,
    }
}

/// Bug report template.
pub const BUG_TEMPLATE: &[TemplateField] = &[
    field("title", "Title", "one-line summary", FieldKind::Line),
    field(
        "steps",
        "Steps",
        "what you did, separate steps with ;",
        FieldKind::Steps,
    ),
    field(
        "expected",
        "Expected",
        "what should have happened",
        FieldKind::Text,
    ),
    field(
        "actual",
        "Actual",
        "what happened instead, with any error text",
        FieldKind::Text,
    ),
    field(
        "freq",
        "Frequency",
        "always|sometimes|once",
        FieldKind::Freq,
    ),
];

/// Feature idea template.
pub const IDEA_TEMPLATE: &[TemplateField] = &[
    field("title", "Title", "one-line summary", FieldKind::Line),
    field("goal", "Goal", "what you are trying to do", FieldKind::Text),
    field(
        "proposal",
        "Proposal",
        "what you would like it to do",
        FieldKind::Text,
    ),
    field("why", "Why", "why it matters", FieldKind::Text),
];

/// Values of a [`FieldKind::Freq`] field.
pub const FREQ_OPTIONS: &[&str] = &["always", "sometimes", "once"];

/// The example line the deck is taught (kept parseable by a test).
pub const EXAMPLE_LINE: &str = "feedback bug | title: Fillet fails | steps: box 0,0,0 1,1,1; fillet last 0.2 | expected: rounded edges | actual: error \"no edges\" | freq: always";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FeedbackCategory {
    #[default]
    Bug,
    Idea,
}

impl FeedbackCategory {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Bug => "bug",
            Self::Idea => "idea",
        }
    }

    /// The form's multiple-choice option (`Bug` / `Idea`), matched exactly.
    pub fn label(self) -> &'static str {
        match self {
            Self::Bug => "Bug",
            Self::Idea => "Idea",
        }
    }

    pub fn template(self) -> &'static [TemplateField] {
        match self {
            Self::Bug => BUG_TEMPLATE,
            Self::Idea => IDEA_TEMPLATE,
        }
    }
}

/// A ticket the user reviews (and may edit) before anything is sent. Field
/// values are keyed by [`TemplateField::key`] and serialize flat next to the
/// category: `{"category":"bug","title":"…","steps":"a\nb"}`. Values typed for
/// the other category are kept, so switching category in the sheet loses nothing.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeedbackDraft {
    #[serde(default)]
    pub category: FeedbackCategory,
    #[serde(flatten)]
    pub fields: BTreeMap<String, String>,
}

impl FeedbackDraft {
    /// A field's value (empty if unset).
    pub fn get(&self, key: &str) -> &str {
        self.fields.get(key).map_or("", String::as_str)
    }

    /// Mutable access for editing (inserts an empty value).
    pub fn field_mut(&mut self, key: &str) -> &mut String {
        self.fields.entry(key.to_string()).or_default()
    }

    /// The current category's steps, one per non-empty line.
    pub fn steps(&self) -> Vec<&str> {
        self.get("steps")
            .lines()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .collect()
    }

    /// `(label, value)` of the current template's non-empty fields except the
    /// title, steps rendered as a numbered list.
    fn summary(&self) -> Vec<(&'static str, String)> {
        self.category
            .template()
            .iter()
            .filter(|f| f.key != "title")
            .map(|f| {
                let v = match f.kind {
                    FieldKind::Steps => self
                        .steps()
                        .iter()
                        .enumerate()
                        .map(|(i, s)| format!("{}. {s}", i + 1))
                        .collect::<Vec<_>>()
                        .join("\n"),
                    _ => self.get(f.key).trim().to_string(),
                };
                (f.label, v)
            })
            .filter(|(_, v)| !v.is_empty())
            .collect()
    }
}

fn template_field(key: &str) -> Option<&'static TemplateField> {
    BUG_TEMPLATE
        .iter()
        .chain(IDEA_TEMPLATE)
        .find(|f| f.key == key)
}

/// Parse `feedback [bug|idea] [| <key>: <value>]…`. Segments are `|`-separated,
/// labeled and order-independent; unknown labels (and an invalid `freq`) are
/// ignored, `;` separates steps. Words after the category with no label are the
/// title. The category defaults to bug. `None` if not a feedback line.
pub fn parse_line(line: &str) -> Option<FeedbackDraft> {
    let rest = line.trim().strip_prefix("feedback")?;
    if !rest.is_empty() && !rest.starts_with(char::is_whitespace) {
        return None;
    }
    let mut segments = rest.split('|');
    let mut draft = FeedbackDraft::default();
    let head = segments.next().unwrap_or("").trim();
    let (word, tail) = head.split_once(char::is_whitespace).unwrap_or((head, ""));
    let head = match word.to_ascii_lowercase().as_str() {
        "bug" => tail,
        "idea" => {
            draft.category = FeedbackCategory::Idea;
            tail
        }
        _ => head,
    };
    for seg in std::iter::once(head).chain(segments) {
        let seg = seg.trim();
        let Some((label, value)) = seg.split_once(':') else {
            if !seg.is_empty() && draft.get("title").is_empty() {
                draft.fields.insert("title".into(), seg.to_string());
            }
            continue;
        };
        let Some(f) = template_field(&label.trim().to_ascii_lowercase()) else {
            continue;
        };
        let value = value.trim();
        let value = match f.kind {
            FieldKind::Steps => value
                .split(';')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
                .join("\n"),
            FieldKind::Freq => match FREQ_OPTIONS.iter().find(|o| o.eq_ignore_ascii_case(value)) {
                Some(o) => o.to_string(),
                None => continue,
            },
            FieldKind::Line | FieldKind::Text => value.to_string(),
        };
        draft.fields.insert(f.key.to_string(), value);
    }
    Some(draft)
}

/// The deck-prompt section for `feedback`: syntax, each template's fields and
/// the interview rule. Generated from the templates so it cannot drift.
pub fn prompt_help() -> String {
    let fields = |t: &[TemplateField]| {
        t.iter()
            .map(|f| format!("{} ({})", f.key, f.hint))
            .collect::<Vec<_>>()
            .join(" · ")
    };
    format!(
        "
Feedback to the developers (ONLY when the user asks to report a problem or wishes for a feature):
  feedback bug|idea | <field>: <value> | …   draft a ticket; opens a review sheet, then a web form in the browser for the user to review and submit — it NEVER submits anything itself
  bug fields:  {}
  idea fields: {}
  e.g. {EXAMPLE_LINE}
  Interview first: fill every field you already know from this conversation (the failing command, the error text you saw). Ask ONLY for the missing fields — at most 3 questions, in ONE message — and emit the feedback line after the user answers. If the user says \"just send it\" or similar, emit it at once with unknown fields left empty. Segments are labeled and order-free; unknown labels are ignored.
",
        fields(BUG_TEMPLATE),
        fields(IDEA_TEMPLATE)
    )
}

/// `<os>-<arch>` of this build, e.g. `macos-aarch64`.
pub fn platform() -> String {
    format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH)
}

/// The ticket text: category, version, platform and the template fields — the
/// form's Details field.
fn issue_body(draft: &FeedbackDraft, version: &str, platform: &str) -> String {
    let mut body = format!(
        "**Category:** {}\n**Version:** {version}\n**Platform:** {platform}\n",
        draft.category.as_str()
    );
    for (label, v) in draft.summary() {
        body.push_str(&format!("\n### {label}\n{v}\n"));
    }
    body
}

/// Pre-filled Google Form URL — the only submit route.
pub fn google_form_url(
    form: &GoogleForm,
    draft: &FeedbackDraft,
    version: &str,
    platform: &str,
) -> String {
    format!(
        "{}?usp=pp_url&{}={}&{}={}&{}={}",
        form.prefill_base,
        form.title_entry,
        encode_capped(draft.get("title"), TITLE_MAX),
        form.category_entry,
        draft.category.label(),
        form.details_entry,
        encode_capped(&issue_body(draft, version, platform), BODY_MAX)
    )
}

/// RFC 3986 percent-encoding (everything but unreserved characters) of `s`,
/// stopping before the char whose escape would exceed
/// `max` bytes — truncation lands on a char boundary, never mid-escape.
fn encode_capped(s: &str, max: usize) -> String {
    let mut out = String::new();
    let mut buf = [0u8; 4];
    for c in s.chars() {
        let piece: String = if c.is_ascii_alphanumeric() || "-._~".contains(c) {
            c.to_string()
        } else {
            c.encode_utf8(&mut buf)
                .bytes()
                .map(|b| format!("%{b:02X}"))
                .collect()
        };
        if out.len() + piece.len() > max {
            break;
        }
        out.push_str(&piece);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn draft() -> FeedbackDraft {
        parse_line(EXAMPLE_LINE).unwrap()
    }

    #[test]
    fn parses_labeled_segments() {
        let d = draft();
        assert_eq!(d.category, FeedbackCategory::Bug);
        assert_eq!(d.get("title"), "Fillet fails");
        assert_eq!(d.steps(), ["box 0,0,0 1,1,1", "fillet last 0.2"]);
        assert_eq!(d.get("expected"), "rounded edges");
        assert_eq!(d.get("actual"), "error \"no edges\"");
        assert_eq!(d.get("freq"), "always");
        // Every bug field is exercised by the taught example.
        assert!(BUG_TEMPLATE.iter().all(|f| !d.get(f.key).is_empty()));
    }

    #[test]
    fn parses_reordered_unknown_missing_and_bare() {
        let d = parse_line(
            "feedback idea | why: speed | Title: Dark sheets | colour: red | goal: print",
        )
        .unwrap();
        assert_eq!(d.category, FeedbackCategory::Idea);
        assert_eq!(
            (d.get("title"), d.get("goal"), d.get("why")),
            ("Dark sheets", "print", "speed")
        );
        assert_eq!(d.get("proposal"), "");
        assert!(
            !d.fields.contains_key("colour"),
            "unknown labels are ignored"
        );

        let d = parse_line("feedback bug | freq: daily | steps: a;; b ;").unwrap();
        assert_eq!(d.get("freq"), "", "invalid freq is ignored");
        assert_eq!(d.get("steps"), "a\nb");

        assert_eq!(parse_line("feedback"), Some(FeedbackDraft::default()));
        assert_eq!(
            parse_line("  feedback bug "),
            Some(FeedbackDraft::default())
        );
        assert_eq!(
            parse_line("feedback IDEA").unwrap().category,
            FeedbackCategory::Idea
        );
        // Unlabeled words are the title; the category defaults to bug.
        let d = parse_line("feedback snapping is off | freq: once").unwrap();
        assert_eq!(
            (d.category, d.get("title"), d.get("freq")),
            (FeedbackCategory::Bug, "snapping is off", "once")
        );

        assert_eq!(parse_line("feedbacks"), None);
        assert_eq!(parse_line("box 0,0,0 1,1,1"), None);
    }

    #[test]
    fn draft_json_is_flat_and_round_trips() {
        let d = draft();
        let j = serde_json::to_string(&d).unwrap();
        assert!(
            j.starts_with(r#"{"category":"bug","#) && j.contains(r#""freq":"always""#),
            "{j}"
        );
        assert_eq!(serde_json::from_str::<FeedbackDraft>(&j).unwrap(), d);
        let d: FeedbackDraft = serde_json::from_str(r#"{"title":"x"}"#).unwrap();
        assert_eq!((d.category, d.get("title")), (FeedbackCategory::Bug, "x"));
    }

    #[test]
    fn prompt_teaches_every_template_field_and_the_interview() {
        let p = crate::system_prompt("", &itsjustcad_commands::PluginRegistry::new());
        for f in BUG_TEMPLATE.iter().chain(IDEA_TEMPLATE) {
            assert!(
                p.contains(&format!("{} ({})", f.key, f.hint)),
                "prompt is missing field '{}'",
                f.key
            );
        }
        assert!(p.contains(EXAMPLE_LINE));
        assert!(p.contains("at most 3 questions") && p.contains("just send it"));
        assert!(p.contains("NEVER submits anything itself"));
    }

    const FORM: GoogleForm = GoogleForm {
        prefill_base: "https://docs.google.com/forms/d/e/FORMID/viewform",
        title_entry: "entry.1",
        category_entry: "entry.2",
        details_entry: "entry.3",
    };

    #[test]
    fn form_url_is_prefilled_encoded_and_capped() {
        let url = google_form_url(&FORM, &draft(), "0.6.7", "macos-aarch64");
        let details = encode_capped(&issue_body(&draft(), "0.6.7", "macos-aarch64"), BODY_MAX);
        assert_eq!(
            url,
            format!(
                "https://docs.google.com/forms/d/e/FORMID/viewform?usp=pp_url&entry.1=Fillet%20fails&entry.2=Bug&entry.3={details}"
            )
        );
        assert!(
            url.contains("%23%23%23%20Expected%0Arounded%20edges") && url.contains("macos-aarch64")
        );
        let mut idea = FeedbackDraft {
            category: FeedbackCategory::Idea,
            ..Default::default()
        };
        assert!(google_form_url(&FORM, &idea, "v", "p").contains("&entry.2=Idea&"));

        // Long text is capped on a char boundary: only whole `%C3%A9` escapes.
        idea.field_mut("goal").push_str(&"é".repeat(5000));
        *idea.field_mut("title") = "é".repeat(500);
        let url = google_form_url(&FORM, &idea, "v", "p");
        let title = url
            .split("&entry.1=")
            .nth(1)
            .unwrap()
            .split('&')
            .next()
            .unwrap();
        assert_eq!(title.len(), TITLE_MAX / 6 * 6);
        let details = url.split("&entry.3=").nth(1).unwrap();
        assert!(details.len() <= BODY_MAX && details.ends_with("%C3%A9"));
    }

    #[test]
    fn form_url_carries_template_text_only() {
        let url = google_form_url(&FORM, &draft(), "0.6.7", "linux-x86_64");
        for part in [
            "Category%3A%2A%2A%20bug",
            "0.6.7",
            "linux-x86_64",
            "%23%23%23%20Steps%0A1.%20box",
            "2.%20fillet%20last%200.2",
            "%23%23%23%20Actual",
            "%23%23%23%20Frequency%0Aalways",
        ] {
            assert!(url.contains(part), "{part} missing from {url}");
        }
        assert!(!url.chars().any(|c| c.is_whitespace() || c == '"'));
        // The builder takes no chat or document: only the draft's own text and
        // the version info reach the URL.
        for leak in ["chat", "document", "turn"] {
            assert!(!url.contains(leak), "{leak} in {url}");
        }
    }
}
