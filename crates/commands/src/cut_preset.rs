// SPDX-License-Identifier: AGPL-3.0-only
// Copyright © 2026 Hector Tarrido-Picart

//! Cutting presets (#209): who the cut file is for.
//!
//! **SendCutSend** (the default) checks against SendCutSend's material list
//! and limits ([`crate::scs`]) and the hosts link to its ordering site after
//! an export. **Generic** (laser/waterjet/router) uses only the rule of thumb
//! ([`crate::cutcheck::CutLimits::rule_of_thumb`]): no SendCutSend data, no
//! link, so any other shop still works.
//!
//! The last preset used is document metadata, like the saved optimizer runs
//! ([`crate::optimize_runs`]): saved in the file header (`cut_export`), never
//! in the op-log, so it is not an undo step. Older builds ignore the field; a
//! header this build cannot read (a newer `version`, an unknown preset, a bad
//! shape) loads as unset, which means SendCutSend.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The `cut_export` header format this build writes and reads.
pub const CUT_EXPORT_VERSION: u32 = 1;

/// Who the cut file is for.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CutPreset {
    /// SendCutSend's materials and limits.
    #[default]
    Scs,
    /// Any laser/waterjet/router shop: the rule of thumb.
    Generic,
}

impl CutPreset {
    pub fn parse(s: &str) -> Option<CutPreset> {
        match s.to_ascii_lowercase().as_str() {
            "scs" | "sendcutsend" => Some(CutPreset::Scs),
            "generic" => Some(CutPreset::Generic),
            _ => None,
        }
    }

    /// The token `preset=` takes and the header stores.
    pub fn label(self) -> &'static str {
        match self {
            CutPreset::Scs => "scs",
            CutPreset::Generic => "generic",
        }
    }

    pub fn is_scs(&self) -> bool {
        *self == CutPreset::Scs
    }
}

/// The file header's `cut_export` object (`None` when unset, so files that
/// never chose a preset stay byte-identical).
pub fn to_header(preset: Option<CutPreset>) -> Option<Value> {
    preset.map(|p| serde_json::json!({ "version": CUT_EXPORT_VERSION, "preset": p.label() }))
}

/// Read the header leniently: anything unreadable is unset.
pub fn from_header(v: Option<Value>) -> Option<CutPreset> {
    let v = v?;
    let version = v.get("version")?.as_u64()?;
    if version == 0 || version > u64::from(CUT_EXPORT_VERSION) {
        return None;
    }
    CutPreset::parse(v.get("preset")?.as_str()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Session, io};

    #[test]
    fn preset_round_trips_through_the_file_and_is_not_an_undo_step() {
        let mut s = Session::default();
        let plain = io::to_json(&s);
        assert!(!plain.contains("cut_export"), "unset, no header field");
        assert_eq!(s.cut_preset(), None);

        // Choosing the default on an unset document is still a choice: saved.
        let (rev, ops) = (s.meta_revision(), s.undo_len());
        s.set_cut_preset(CutPreset::Generic);
        assert_eq!(s.meta_revision(), rev + 1, "metadata changed");
        assert_eq!(s.undo_len(), ops, "not an undo step");
        s.set_cut_preset(CutPreset::Generic);
        assert_eq!(s.meta_revision(), rev + 1, "same preset: no change");

        let json = io::to_json(&s);
        assert!(json.contains(r#""preset": "generic""#), "{json}");
        let back = io::from_json(&json).unwrap();
        assert_eq!(back.cut_preset(), Some(CutPreset::Generic));
        assert_eq!(io::to_json(&back), json, "byte-identical re-save");
    }

    #[test]
    fn unreadable_headers_load_as_unset() {
        let base = io::to_json(&Session::default());
        for header in [
            r#"{"version":2,"preset":"generic"}"#,
            r#"{"version":1,"preset":"plasma"}"#,
            r#"{"preset":"generic"}"#,
            r#""generic""#,
        ] {
            let json = base.replacen('{', &format!(r#"{{"cut_export":{header},"#), 1);
            let s = io::from_json(&json).unwrap_or_else(|e| panic!("{header}: {e}"));
            assert_eq!(s.cut_preset(), None, "{header}");
        }
        assert_eq!(CutPreset::parse("SendCutSend"), Some(CutPreset::Scs));
        assert_eq!(CutPreset::default(), CutPreset::Scs);
    }
}
