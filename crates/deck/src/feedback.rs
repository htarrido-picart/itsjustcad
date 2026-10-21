// SPDX-License-Identifier: AGPL-3.0-only
// Copyright © 2026 Hector Tarrido-Picart

//! Alpha feedback tickets (GitHub issue #123). The deck (or the user) drafts a
//! `feedback bug|idea | <key>: <value> | …` line; the host shows a review sheet
//! and only on an explicit click writes the bundle and hands off to mail /
//! GitHub. Everything here is pure: no I/O, no network.
//!
//! The per-category [`TemplateField`] tables are the single source of truth:
//! the deck prompt ([`prompt_help`]), the review sheet, the bundle, the mailto
//! summary and the GitHub body all derive from them.
//!
//! PRIVACY: the repo is public. The document and chat travel only in the bundle
//! file (email); [`github_issue_url`] carries the typed text plus version info.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::deck::ChatMessage;

/// Where feedback email goes. Placeholder alias — the one place to change it.
pub const FEEDBACK_EMAIL: &str = "feedback@itsjustcad.app";
/// Pre-filled "new issue" page; the only GitHub URL the hosts may open.
pub const GITHUB_NEW_ISSUE_URL: &str = "https://github.com/htarrido-picart/itsjustcad/issues/new";
/// Documents larger than this (compact JSON bytes) are left out of the bundle.
pub const MAX_DOC_BYTES: usize = 10 * 1024 * 1024;
/// Chat turns included in the bundle (the most recent ones).
pub const MAX_CHAT_TURNS: usize = 10;
/// Cap on the percent-encoded mailto body; mail clients choke on long URLs.
pub const MAILTO_BODY_MAX: usize = 1500;
/// Cap on the percent-encoded GitHub issue body (URL length limits).
const GITHUB_BODY_MAX: usize = 6000;

/// How a template field is entered.
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
    /// Label on the verb line and key in the bundle / draft JSON.
    pub key: &'static str,
    /// English heading (mailto, GitHub body); the sheet localizes by `key`.
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
  feedback bug|idea | <field>: <value> | …   draft a ticket; opens a review sheet the user edits and sends — it NEVER sends anything itself
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

/// Bundle file name for a moment in Unix seconds.
pub fn bundle_filename(unix_secs: u64) -> String {
    format!("itsjustcad-feedback-{unix_secs}.json")
}

/// Whether a document of this compact-JSON size fits in the bundle.
pub fn document_fits(doc_json: &str) -> bool {
    doc_json.len() <= MAX_DOC_BYTES
}

/// The most recent [`MAX_CHAT_TURNS`] turns.
pub fn last_turns(chat: &[ChatMessage]) -> &[ChatMessage] {
    &chat[chat.len().saturating_sub(MAX_CHAT_TURNS)..]
}

/// The bundle file (compact JSON). `chat`/`doc_json` are `None` when the user
/// unticked them. A document over [`MAX_DOC_BYTES`] (or not JSON) is left out
/// and `document_omitted` says why.
pub fn build_bundle(
    draft: &FeedbackDraft,
    app_version: &str,
    platform: &str,
    chat: Option<&[ChatMessage]>,
    doc_json: Option<&str>,
) -> String {
    let mut b = json!({ "category": draft.category });
    for f in draft.category.template() {
        b[f.key] = match f.kind {
            FieldKind::Steps => json!(draft.steps()),
            _ => json!(draft.get(f.key)),
        };
    }
    b["app_version"] = json!(app_version);
    b["platform"] = json!(platform);
    if let Some(chat) = chat {
        b["chat"] = json!(last_turns(chat));
    }
    if let Some(doc) = doc_json {
        if !document_fits(doc) {
            b["document_omitted"] = json!(format!(
                "document is {:.1} MB, over the {} MB cap",
                doc.len() as f64 / (1024.0 * 1024.0),
                MAX_DOC_BYTES / (1024 * 1024)
            ));
        } else {
            match serde_json::from_str::<Value>(doc) {
                Ok(v) => b["document"] = v,
                Err(e) => b["document_omitted"] = json!(format!("document is not JSON: {e}")),
            }
        }
    }
    b.to_string()
}

/// `mailto:` to [`FEEDBACK_EMAIL`] with a short summary; the full details live
/// in the attached bundle (mailto cannot attach, so the body names the file).
pub fn mailto_url(draft: &FeedbackDraft, attachment_filename: &str) -> String {
    let subject = format!(
        "[itsjustcad {}] {}",
        draft.category.as_str(),
        draft.get("title")
    );
    let summary: String = draft
        .summary()
        .into_iter()
        .map(|(label, v)| format!("{label}:\n{v}\n\n"))
        .collect();
    let footer = encode(&format!(
        "Attached: {attachment_filename}\n(Please attach this file; it was just revealed in your file manager.)"
    ));
    let body = encode_capped(&summary, MAILTO_BODY_MAX.saturating_sub(footer.len())) + &footer;
    format!(
        "mailto:{FEEDBACK_EMAIL}?subject={}&body={body}",
        encode_capped(&subject, 300)
    )
}

/// Pre-filled GitHub "new issue" URL. Text only — never chat or the document.
pub fn github_issue_url(draft: &FeedbackDraft, version: &str, platform: &str) -> String {
    let mut body = format!(
        "**Category:** {}\n**Version:** {version}\n**Platform:** {platform}\n",
        draft.category.as_str()
    );
    for (label, v) in draft.summary() {
        body.push_str(&format!("\n### {label}\n{v}\n"));
    }
    format!(
        "{GITHUB_NEW_ISSUE_URL}?title={}&body={}&labels=alpha-feedback",
        encode_capped(draft.get("title"), 300),
        encode_capped(&body, GITHUB_BODY_MAX)
    )
}

/// RFC 3986 percent-encoding: everything but unreserved characters.
fn encode(s: &str) -> String {
    encode_capped(s, usize::MAX)
}

/// Percent-encode `s`, stopping before the char whose escape would exceed
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
    use crate::deck::Role;

    fn draft() -> FeedbackDraft {
        parse_line(EXAMPLE_LINE).unwrap()
    }

    fn chat(n: usize) -> Vec<ChatMessage> {
        (0..n)
            .map(|i| ChatMessage {
                role: Role::User,
                content: format!("turn {i}"),
            })
            .collect()
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
        assert!(p.contains("NEVER sends anything itself"));
    }

    #[test]
    fn bundle_has_template_fields_version_platform() {
        let v: Value = serde_json::from_str(&build_bundle(
            &draft(),
            "0.6.7",
            "macos-aarch64",
            None,
            None,
        ))
        .unwrap();
        assert_eq!(v["category"], "bug");
        assert_eq!(v["title"], "Fillet fails");
        assert_eq!(v["steps"], json!(["box 0,0,0 1,1,1", "fillet last 0.2"]));
        assert_eq!(v["expected"], "rounded edges");
        assert_eq!(v["actual"], "error \"no edges\"");
        assert_eq!(v["freq"], "always");
        assert_eq!(v["app_version"], "0.6.7");
        assert_eq!(v["platform"], "macos-aarch64");
        assert!(
            v.get("chat").is_none()
                && v.get("document").is_none()
                && v.get("document_omitted").is_none()
        );
        assert!(platform().contains(std::env::consts::OS));

        // Only the current category's template goes in the bundle.
        let mut idea = draft();
        idea.category = FeedbackCategory::Idea;
        idea.field_mut("goal").push_str("print");
        let v: Value = serde_json::from_str(&build_bundle(&idea, "v", "p", None, None)).unwrap();
        assert_eq!(
            (v["goal"].as_str(), v["why"].as_str()),
            (Some("print"), Some(""))
        );
        assert!(v.get("steps").is_none() && v.get("freq").is_none());
    }

    #[test]
    fn bundle_attach_and_chat_toggles() {
        let doc = r#"{"itsjustcad":1,"ops":[]}"#;
        let c = chat(14);
        let v: Value =
            serde_json::from_str(&build_bundle(&draft(), "v", "p", Some(&c), Some(doc))).unwrap();
        assert_eq!(v["document"]["itsjustcad"], 1);
        let turns = v["chat"].as_array().unwrap();
        assert_eq!(turns.len(), MAX_CHAT_TURNS);
        assert_eq!(turns[0]["content"], "turn 4");
        assert_eq!(turns[9]["role"], "user");

        let v: Value =
            serde_json::from_str(&build_bundle(&draft(), "v", "p", Some(&chat(2)), None)).unwrap();
        assert!(v.get("document").is_none());
        assert_eq!(v["chat"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn bundle_omits_document_over_cap() {
        let big = format!("{{\"pad\":\"{}\"}}", "x".repeat(MAX_DOC_BYTES));
        assert!(!document_fits(&big));
        let v: Value =
            serde_json::from_str(&build_bundle(&draft(), "v", "p", None, Some(&big))).unwrap();
        assert!(v.get("document").is_none());
        assert!(
            v["document_omitted"]
                .as_str()
                .unwrap()
                .contains("10 MB cap")
        );
    }

    #[test]
    fn mailto_is_encoded_and_capped() {
        let url = mailto_url(&draft(), "itsjustcad-feedback-1.json");
        assert!(url.starts_with(
            "mailto:feedback@itsjustcad.app?subject=%5Bitsjustcad%20bug%5D%20Fillet%20fails&body="
        ));
        let body = url.split("&body=").nth(1).unwrap();
        for part in [
            "Steps%3A%0A1.%20box",
            "2.%20fillet%20last%200.2",
            "Expected%3A%0Arounded%20edges",
            "Actual%3A",
            "Frequency%3A%0Aalways",
        ] {
            assert!(body.contains(part), "{part} missing from {body}");
        }
        assert!(url.contains("itsjustcad-feedback-1.json"));
        assert!(!url.chars().any(|c| c.is_whitespace() || c == '"'));

        let mut long = FeedbackDraft::default();
        *long.field_mut("actual") = "é".repeat(5000);
        let url = mailto_url(&long, "f.json");
        let body = url.split("&body=").nth(1).unwrap();
        assert!(body.len() <= MAILTO_BODY_MAX, "{}", body.len());
        assert!(body.ends_with(&encode(
            "Attached: f.json\n(Please attach this file; it was just revealed in your file manager.)"
        )));
        // Truncated on a char boundary: only whole `%C3%A9` escapes before the footer.
        let head = body.strip_prefix("Actual%3A%0A").unwrap();
        assert!(
            head.starts_with("%C3%A9")
                && head
                    .split("Attached")
                    .next()
                    .unwrap()
                    .len()
                    .is_multiple_of(6)
        );
    }

    #[test]
    fn github_url_carries_template_text_only() {
        let d = draft();
        let url = github_issue_url(&d, "0.6.7", "linux-x86_64");
        assert!(url.starts_with(GITHUB_NEW_ISSUE_URL));
        assert!(url.ends_with("&labels=alpha-feedback"));
        assert!(url.contains("title=Fillet%20fails"));
        assert!(url.contains("0.6.7") && url.contains("linux-x86_64"));
        for part in [
            "%23%23%23%20Steps%0A1.%20box",
            "%23%23%23%20Expected",
            "%23%23%23%20Actual",
            "%23%23%23%20Frequency%0Aalways",
        ] {
            assert!(url.contains(part), "{part} missing from {url}");
        }
        // The signature has no chat or document input, and the bundle's chat
        // and document never leak into it.
        let bundle = build_bundle(
            &d,
            "0.6.7",
            "linux-x86_64",
            Some(&chat(3)),
            Some(r#"{"secret_doc":1}"#),
        );
        assert!(bundle.contains("turn 2") && bundle.contains("secret_doc"));
        assert!(!url.contains("turn") && !url.contains("secret_doc"));
    }
}
