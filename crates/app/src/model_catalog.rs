// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! Curated catalog of downloadable local models.
//!
//! The data lives in `assets/models.json` (bundled at compile time via
//! [`include_str!`]) so it is trivially updatable without touching code: refresh
//! a URL/size/sha and rebuild. Each entry is verified against the live source
//! where possible (see the JSON's `_comment`); unverifiable entries would be
//! marked with a `PLACEHOLDER` id so the UI can flag them.
//!
//! The gating logic — which tier a machine can run, which models fit in `N` GB —
//! is pure and unit-tested. The [`Catalog::load`] parser is tested against the
//! bundled bytes so a malformed edit fails the test suite, not the app.

use serde::Deserialize;

use crate::hardware::ModelTier;

/// The JSON asset, embedded at build time.
const CATALOG_JSON: &str = include_str!("../assets/models.json");

/// How the local runtime will serve a downloaded file. The actual spawn is a
/// later agent's job; the catalog only records the intent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Runtime {
    /// A `.gguf` weights file served by a llama.cpp-compatible server.
    Gguf,
    /// A self-contained Justine/Mozilla `llamafile` executable.
    Llamafile,
}

/// One downloadable model.
#[derive(Debug, Clone, Deserialize)]
pub struct ModelEntry {
    /// Stable slug (also the local cassette suffix: `local-<id>`).
    pub id: String,
    pub display_name: String,
    /// Which hardware tier this model targets.
    pub tier: TierTag,
    pub url: String,
    pub size_bytes: u64,
    /// Expected SHA-256 (lower-case hex). Empty string ⇒ unverified placeholder.
    pub sha256: String,
    /// How the local server will serve this file. Consumed by the runtime-spawn
    /// agent (next phase); parsed + validated here so the catalog stays honest.
    #[allow(dead_code)]
    pub runtime: Runtime,
    /// Minimum system RAM (GiB) to run comfortably; the Install gate.
    pub ram_gb_min: u64,
}

/// Serde-friendly mirror of [`ModelTier`] (that enum has no `None` in the JSON
/// vocabulary and isn't `Deserialize`, so we keep a small tag type here).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub enum TierTag {
    Small3B,
    Mid7B,
}

impl TierTag {
    /// The corresponding [`ModelTier`]. Bridges catalog tags to the hardware
    /// policy enum; exercised by the gating tests.
    #[allow(dead_code)]
    pub fn as_tier(self) -> ModelTier {
        match self {
            TierTag::Small3B => ModelTier::Small3B,
            TierTag::Mid7B => ModelTier::Mid7B,
        }
    }
}

impl ModelEntry {
    /// The on-disk file name to save this model as (last path segment of the
    /// URL, query stripped).
    pub fn file_name(&self) -> String {
        self.url
            .rsplit('/')
            .next()
            .unwrap_or(&self.id)
            .split(['?', '#'])
            .next()
            .unwrap_or(&self.id)
            .to_string()
    }

    /// True when this machine (with `ram_gb` GiB) can run the model per the
    /// catalog's `ram_gb_min` gate. Unknown RAM (`None`) is treated permissively
    /// as "allow, but warn" — the caller decides how to surface that.
    pub fn runnable_at(&self, ram_gb: Option<u64>) -> bool {
        match ram_gb {
            Some(ram) => ram >= self.ram_gb_min,
            None => true,
        }
    }

    /// True when the entry is an unverified placeholder (empty sha256).
    pub fn is_placeholder(&self) -> bool {
        self.sha256.trim().is_empty()
    }

    /// The expected SHA-256 for the downloader, or `None` for placeholders.
    pub fn expected_sha(&self) -> Option<&str> {
        if self.is_placeholder() {
            None
        } else {
            Some(self.sha256.as_str())
        }
    }
}

/// The parsed catalog.
#[derive(Debug, Clone, Deserialize)]
pub struct Catalog {
    #[serde(rename = "models")]
    pub models: Vec<ModelEntry>,
}

impl Catalog {
    /// Parse the bundled `assets/models.json`. Panics only if that compile-time
    /// asset is malformed (caught by the unit test, never in a shipped build).
    pub fn load() -> Self {
        serde_json::from_str(CATALOG_JSON).expect("bundled models.json is valid")
    }

    /// Models this machine can run at `ram_gb` GiB, in catalog order. Pure — the
    /// RAM-gate query the setup panel filters by; kept as tested public API even
    /// though the panel currently annotates per-entry rather than pre-filtering.
    #[allow(dead_code)]
    pub fn runnable_at(&self, ram_gb: Option<u64>) -> Vec<&ModelEntry> {
        self.models
            .iter()
            .filter(|m| m.runnable_at(ram_gb))
            .collect()
    }

    /// Look up an entry by id.
    pub fn get(&self, id: &str) -> Option<&ModelEntry> {
        self.models.iter().find(|m| m.id == id)
    }

    /// The model that best matches a recommended [`ModelTier`]: the largest
    /// entry whose tier the machine is cleared for. For `Mid7B` prefer a 7B and
    /// fall back to a 3B; for `Small3B`/`None` pick a 3B.
    pub fn recommended_for(&self, tier: ModelTier) -> Option<&ModelEntry> {
        let want_7b = matches!(tier, ModelTier::Mid7B);
        if want_7b && let Some(m) = self.models.iter().find(|m| m.tier == TierTag::Mid7B) {
            return Some(m);
        }
        self.models
            .iter()
            .find(|m| m.tier == TierTag::Small3B)
            .or_else(|| self.models.first())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── parse the bundled asset ────────────────────────────────────────────

    #[test]
    fn bundled_catalog_parses() {
        let cat = Catalog::load();
        assert!(!cat.models.is_empty(), "catalog must not be empty");
        // Every non-placeholder entry must have a plausible https URL and a
        // 64-hex sha.
        for m in &cat.models {
            assert!(m.url.starts_with("https://"), "{} url not https", m.id);
            assert!(m.size_bytes > 0, "{} zero size", m.id);
            if !m.is_placeholder() {
                assert_eq!(m.sha256.len(), 64, "{} sha not 64 hex", m.id);
                assert!(
                    m.sha256.chars().all(|c| c.is_ascii_hexdigit()),
                    "{} sha not hex",
                    m.id
                );
            }
        }
    }

    #[test]
    fn catalog_has_a_3b_and_a_7b() {
        let cat = Catalog::load();
        assert!(cat.models.iter().any(|m| m.tier == TierTag::Small3B));
        assert!(cat.models.iter().any(|m| m.tier == TierTag::Mid7B));
    }

    // ── RAM gating (pure) ──────────────────────────────────────────────────

    #[test]
    fn runnable_gate_by_ram() {
        let cat = Catalog::load();
        // 8 GB → only the small-tier entry is runnable, not the 7B.
        let at8 = cat.runnable_at(Some(8));
        assert!(at8.iter().any(|m| m.tier == TierTag::Small3B), "{at8:?}");
        assert!(!at8.iter().any(|m| m.tier == TierTag::Mid7B), "{at8:?}");
        // 16 GB → both are runnable.
        let at16 = cat.runnable_at(Some(16));
        assert_eq!(at16.len(), cat.models.len());
    }

    #[test]
    fn runnable_below_all_minimums_is_empty_or_small() {
        let cat = Catalog::load();
        // 4 GB is under every entry's minimum (min is 8) → nothing runs.
        assert!(cat.runnable_at(Some(4)).is_empty());
    }

    #[test]
    fn unknown_ram_allows_all() {
        let cat = Catalog::load();
        assert_eq!(cat.runnable_at(None).len(), cat.models.len());
    }

    #[test]
    fn entry_runnable_at_boundary_is_inclusive() {
        let e = ModelEntry {
            id: "x".into(),
            display_name: "X".into(),
            tier: TierTag::Mid7B,
            url: "https://h/x.gguf".into(),
            size_bytes: 1,
            sha256: "".into(),
            runtime: Runtime::Gguf,
            ram_gb_min: 16,
        };
        assert!(!e.runnable_at(Some(15)));
        assert!(e.runnable_at(Some(16)));
        assert!(e.runnable_at(Some(32)));
    }

    // ── recommendation ─────────────────────────────────────────────────────

    #[test]
    fn recommended_picks_7b_for_mid_tier() {
        let cat = Catalog::load();
        let rec = cat.recommended_for(ModelTier::Mid7B).unwrap();
        assert_eq!(rec.tier, TierTag::Mid7B);
    }

    #[test]
    fn recommended_picks_3b_for_small_and_none() {
        let cat = Catalog::load();
        assert_eq!(
            cat.recommended_for(ModelTier::Small3B).unwrap().tier,
            TierTag::Small3B
        );
        assert_eq!(
            cat.recommended_for(ModelTier::None).unwrap().tier,
            TierTag::Small3B
        );
    }

    // ── file name derivation ───────────────────────────────────────────────

    #[test]
    fn file_name_strips_path_and_query() {
        let e = ModelEntry {
            id: "m".into(),
            display_name: "M".into(),
            tier: TierTag::Small3B,
            url: "https://host/a/b/model.gguf?download=true".into(),
            size_bytes: 1,
            sha256: "".into(),
            runtime: Runtime::Gguf,
            ram_gb_min: 8,
        };
        assert_eq!(e.file_name(), "model.gguf");
    }

    #[test]
    fn placeholder_detection_and_expected_sha() {
        let mut e = ModelEntry {
            id: "m".into(),
            display_name: "M".into(),
            tier: TierTag::Small3B,
            url: "https://host/model.gguf".into(),
            size_bytes: 1,
            sha256: "".into(),
            runtime: Runtime::Gguf,
            ram_gb_min: 8,
        };
        assert!(e.is_placeholder());
        assert_eq!(e.expected_sha(), None);
        e.sha256 = "abc123".into();
        assert!(!e.is_placeholder());
        assert_eq!(e.expected_sha(), Some("abc123"));
    }

    #[test]
    fn get_by_id() {
        let cat = Catalog::load();
        let first = cat.models[0].id.clone();
        assert!(cat.get(&first).is_some());
        assert!(cat.get("nope").is_none());
    }
}
