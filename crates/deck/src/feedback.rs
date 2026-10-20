// SPDX-License-Identifier: AGPL-3.0-only
// Copyright © 2026 Hector Tarrido-Picart

//! Alpha feedback tickets (GitHub issue #123). The deck (or the user) drafts a
//! `feedback bug|idea <title> | <description> | <steps>` line; the host shows a
//! review sheet and only on an explicit click writes the bundle and hands off
//! to mail / GitHub. Everything here is pure: no I/O, no network.
//!
//! PRIVACY: the repo is public. The document and chat travel only in the bundle
//! file (email); [`github_issue_url`] carries the typed text plus version info.

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
}

/// A ticket the user reviews (and may edit) before anything is sent.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct FeedbackDraft {
    pub category: FeedbackCategory,
    pub title: String,
    pub description: String,
    pub steps: String,
}

/// Parse `feedback [bug|idea] [<title> | <description> | <steps>]`. Missing
/// fields stay empty; the category defaults to bug. `None` if the line is not a
/// feedback line.
pub fn parse_line(line: &str) -> Option<FeedbackDraft> {
    let rest = line.trim().strip_prefix("feedback")?;
    if !rest.is_empty() && !rest.starts_with(char::is_whitespace) {
        return None;
    }
    let rest = rest.trim_start();
    let (word, tail) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
    let (category, rest) = match word.to_ascii_lowercase().as_str() {
        "bug" => (FeedbackCategory::Bug, tail),
        "idea" => (FeedbackCategory::Idea, tail),
        _ => (FeedbackCategory::Bug, rest),
    };
    let mut parts = rest.splitn(3, '|').map(|s| s.trim().to_string());
    Some(FeedbackDraft {
        category,
        title: parts.next().unwrap_or_default(),
        description: parts.next().unwrap_or_default(),
        steps: parts.next().unwrap_or_default(),
    })
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
    let mut b = json!({
        "title": draft.title,
        "category": draft.category,
        "description": draft.description,
        "steps": draft.steps,
        "app_version": app_version,
        "platform": platform,
    });
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
    let subject = format!("[itsjustcad {}] {}", draft.category.as_str(), draft.title);
    let mut summary = draft.description.clone();
    if !draft.steps.is_empty() {
        summary.push_str("\n\nSteps:\n");
        summary.push_str(&draft.steps);
    }
    let footer = encode(&format!(
        "\n\nAttached: {attachment_filename}\n(Please attach this file; it was just revealed in your file manager.)"
    ));
    let body = encode_capped(&summary, MAILTO_BODY_MAX.saturating_sub(footer.len())) + &footer;
    format!(
        "mailto:{FEEDBACK_EMAIL}?subject={}&body={body}",
        encode_capped(&subject, 300)
    )
}

/// Pre-filled GitHub "new issue" URL. Text only — never chat or the document.
pub fn github_issue_url(draft: &FeedbackDraft, version: &str, platform: &str) -> String {
    let body = format!(
        "**Category:** {}\n**Version:** {version}\n**Platform:** {platform}\n\n\
         ### Description\n{}\n\n### Steps\n{}\n",
        draft.category.as_str(),
        draft.description,
        draft.steps
    );
    format!(
        "{GITHUB_NEW_ISSUE_URL}?title={}&body={}&labels=alpha-feedback",
        encode_capped(&draft.title, 300),
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
        parse_line(
            "feedback bug Fillet fails | fillet on a box errors | box 0,0,0 1,1,1 then fillet",
        )
        .unwrap()
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
    fn parses_full_partial_bare_and_category() {
        let d = draft();
        assert_eq!(d.category, FeedbackCategory::Bug);
        assert_eq!(d.title, "Fillet fails");
        assert_eq!(d.description, "fillet on a box errors");
        assert_eq!(d.steps, "box 0,0,0 1,1,1 then fillet");

        let d = parse_line("feedback idea Dark sheets | print sheets on black").unwrap();
        assert_eq!(d.category, FeedbackCategory::Idea);
        assert_eq!(
            (d.title.as_str(), d.description.as_str(), d.steps.as_str()),
            ("Dark sheets", "print sheets on black", "")
        );

        assert_eq!(parse_line("feedback"), Some(FeedbackDraft::default()));
        assert_eq!(
            parse_line("  feedback bug "),
            Some(FeedbackDraft::default())
        );
        let d = parse_line("feedback IDEA").unwrap();
        assert_eq!(d.category, FeedbackCategory::Idea);
        assert!(d.title.is_empty());
        // No category word: bug, and the words are the title.
        let d = parse_line("feedback snapping is off").unwrap();
        assert_eq!(
            (d.category, d.title.as_str()),
            (FeedbackCategory::Bug, "snapping is off")
        );

        assert_eq!(parse_line("feedbacks"), None);
        assert_eq!(parse_line("box 0,0,0 1,1,1"), None);
    }

    #[test]
    fn bundle_has_draft_version_platform() {
        let v: Value = serde_json::from_str(&build_bundle(
            &draft(),
            "0.6.7",
            "macos-aarch64",
            None,
            None,
        ))
        .unwrap();
        assert_eq!(v["title"], "Fillet fails");
        assert_eq!(v["category"], "bug");
        assert_eq!(v["description"], "fillet on a box errors");
        assert_eq!(v["steps"], "box 0,0,0 1,1,1 then fillet");
        assert_eq!(v["app_version"], "0.6.7");
        assert_eq!(v["platform"], "macos-aarch64");
        assert!(
            v.get("chat").is_none()
                && v.get("document").is_none()
                && v.get("document_omitted").is_none()
        );
        assert!(platform().contains(std::env::consts::OS));
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
        assert!(url.contains("itsjustcad-feedback-1.json"));
        assert!(!url.chars().any(|c| c.is_whitespace() || c == '"'));

        let mut long = draft();
        long.description = "é".repeat(5000);
        let url = mailto_url(&long, "f.json");
        let body = url.split("&body=").nth(1).unwrap();
        assert!(body.len() <= MAILTO_BODY_MAX, "{}", body.len());
        assert!(body.ends_with(&encode("\n\nAttached: f.json\n(Please attach this file; it was just revealed in your file manager.)")));
        // Truncated on a char boundary: only whole `%C3%A9` escapes before the footer.
        assert!(
            body.starts_with("%C3%A9%C3%A9") && body.split("%0A").next().unwrap().len().is_multiple_of(6)
        );
    }

    #[test]
    fn github_url_carries_text_only() {
        let d = draft();
        let url = github_issue_url(&d, "0.6.7", "linux-x86_64");
        assert!(url.starts_with(GITHUB_NEW_ISSUE_URL));
        assert!(url.ends_with("&labels=alpha-feedback"));
        assert!(url.contains("title=Fillet%20fails"));
        assert!(url.contains("0.6.7") && url.contains("linux-x86_64"));
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
