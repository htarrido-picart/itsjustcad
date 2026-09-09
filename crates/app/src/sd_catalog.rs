// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! Curated catalog of downloadable local Stable Diffusion weights for the
//! Render Setup panel — the SD twin of [`crate::model_catalog`].
//!
//! The render ENGINE is stable-diffusion.cpp's `sd` CLI (MIT), which the user
//! installs themselves — we NEVER bundle or link it (the same detect-and-shell-
//! out stance as LibreDWG's `dwg2dxf`). This catalog only lists the WEIGHTS the
//! app downloads on demand into `~/.config/itsjustcad/sdmodels/` and hands to
//! `sd --model` / `sd --control-net`.
//!
//! The data lives in `assets/sd_models.json` (embedded via [`include_str!`]) so
//! it is trivially updatable without touching code. Entries with an EMPTY
//! `sha256` are TODO placeholders (see the JSON `_comment`): the download still
//! works but verification is skipped, and the UI flags it as unverified.
//!
//! The gating logic (disk shortfall) reuses [`crate::model_catalog::ModelEntry`]
//! rules where it can; the parse + disk-gate are pure and unit-tested against
//! the bundled bytes.

use serde::Deserialize;

/// The JSON asset, embedded at build time.
const SD_CATALOG_JSON: &str = include_str!("../assets/sd_models.json");

/// The role a downloaded SD file plays.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SdKind {
    /// The base SD checkpoint gguf (`sd --model`).
    Model,
    /// An optional depth ControlNet gguf (`sd --control-net`).
    Controlnet,
}

/// One downloadable SD file.
#[derive(Debug, Clone, Deserialize)]
pub struct SdModelEntry {
    /// Stable slug (also the render-cassette suffix for the base model).
    pub id: String,
    pub display_name: String,
    pub kind: SdKind,
    pub url: String,
    pub size_bytes: u64,
    /// Expected SHA-256 (lower-case hex). Empty string ⇒ unverified placeholder.
    pub sha256: String,
    /// Minimum system RAM (GiB) to run comfortably; surfaced as a soft advisory
    /// in the panel (SD's hard block is disk, not RAM).
    pub ram_gb_min: u64,
}

impl SdModelEntry {
    /// The on-disk file name (last URL path segment, query stripped).
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

    /// True when this machine (with `ram_gb` GiB) meets the RAM gate. Unknown
    /// RAM is permissive ("allow, but warn") — the caller surfaces the warning.
    pub fn runnable_at(&self, ram_gb: Option<u64>) -> bool {
        match ram_gb {
            Some(ram) => ram >= self.ram_gb_min,
            None => true,
        }
    }

    /// Why installing on a machine with `free_disk_gb` GiB free would fail, or
    /// `None` when there's room / free space is unknown. Same rule as the LLM
    /// catalog: size + 1 GiB margin. Pure — unit-tested.
    pub fn disk_shortfall(&self, free_disk_gb: Option<u64>) -> Option<String> {
        const GIB: u64 = 1024 * 1024 * 1024;
        let free = free_disk_gb?;
        let needed = self.size_bytes.saturating_add(GIB);
        if free.saturating_mul(GIB) >= needed {
            return None;
        }
        Some(format!(
            "Not enough free disk space: needs {} (+1 GB margin), only {} GB free.",
            crate::download::fmt_bytes(self.size_bytes),
            free
        ))
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

/// The parsed SD catalog.
#[derive(Debug, Clone, Deserialize)]
pub struct SdCatalog {
    #[serde(rename = "models")]
    pub models: Vec<SdModelEntry>,
}

impl SdCatalog {
    /// Parse the bundled `assets/sd_models.json`. Panics only if that compile-
    /// time asset is malformed (caught by the unit test, never in a build).
    pub fn load() -> Self {
        serde_json::from_str(SD_CATALOG_JSON).expect("bundled sd_models.json is valid")
    }

    /// Look up an entry by id.
    pub fn get(&self, id: &str) -> Option<&SdModelEntry> {
        self.models.iter().find(|m| m.id == id)
    }

    /// The recommended base model to offer by default: the first `Model` entry.
    pub fn default_model(&self) -> Option<&SdModelEntry> {
        self.models.iter().find(|m| m.kind == SdKind::Model)
    }

    /// The first depth ControlNet entry, if any.
    pub fn default_controlnet(&self) -> Option<&SdModelEntry> {
        self.models.iter().find(|m| m.kind == SdKind::Controlnet)
    }
}

/// The directory downloaded SD weights live in:
/// `~/.config/itsjustcad/sdmodels` (parallel to the LLM `models` dir).
pub fn sd_models_dir() -> Option<std::path::PathBuf> {
    Some(
        dirs::home_dir()?
            .join(".config")
            .join("itsjustcad")
            .join("sdmodels"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_sd_catalog_parses() {
        let cat = SdCatalog::load();
        assert!(!cat.models.is_empty(), "SD catalog must not be empty");
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
    fn catalog_has_a_base_model_and_a_controlnet() {
        let cat = SdCatalog::load();
        assert!(cat.default_model().is_some(), "a base SD model");
        assert!(cat.default_controlnet().is_some(), "a depth ControlNet");
        assert_eq!(cat.default_model().unwrap().kind, SdKind::Model);
        assert_eq!(cat.default_controlnet().unwrap().kind, SdKind::Controlnet);
    }

    #[test]
    fn get_by_id() {
        let cat = SdCatalog::load();
        let first = cat.models[0].id.clone();
        assert!(cat.get(&first).is_some());
        assert!(cat.get("nope").is_none());
    }

    #[test]
    fn file_name_strips_path_and_query() {
        let e = SdModelEntry {
            id: "m".into(),
            display_name: "M".into(),
            kind: SdKind::Model,
            url: "https://h/a/b/sd15.gguf?download=true".into(),
            size_bytes: 1,
            sha256: String::new(),
            ram_gb_min: 8,
        };
        assert_eq!(e.file_name(), "sd15.gguf");
    }

    #[test]
    fn disk_gate_blocks_and_allows_with_margin() {
        const GIB: u64 = 1024 * 1024 * 1024;
        let e = SdModelEntry {
            id: "m".into(),
            display_name: "M".into(),
            kind: SdKind::Model,
            url: "https://h/m.gguf".into(),
            size_bytes: 4 * GIB,
            sha256: String::new(),
            ram_gb_min: 8,
        };
        // 4 GB free < 4 GiB + 1 GiB margin → blocked.
        assert!(e.disk_shortfall(Some(4)).is_some());
        // 5 GB free == size + margin → allowed.
        assert_eq!(e.disk_shortfall(Some(5)), None);
        // Unknown free space is permissive.
        assert_eq!(e.disk_shortfall(None), None);
    }

    #[test]
    fn placeholder_detection_and_expected_sha() {
        let mut e = SdModelEntry {
            id: "m".into(),
            display_name: "M".into(),
            kind: SdKind::Model,
            url: "https://h/m.gguf".into(),
            size_bytes: 1,
            sha256: String::new(),
            ram_gb_min: 8,
        };
        assert!(e.is_placeholder());
        assert_eq!(e.expected_sha(), None);
        e.sha256 = "abc123".into();
        assert!(!e.is_placeholder());
        assert_eq!(e.expected_sha(), Some("abc123"));
    }

    #[test]
    fn runnable_gate_by_ram() {
        let e = SdModelEntry {
            id: "m".into(),
            display_name: "M".into(),
            kind: SdKind::Model,
            url: "https://h/m.gguf".into(),
            size_bytes: 1,
            sha256: String::new(),
            ram_gb_min: 8,
        };
        assert!(!e.runnable_at(Some(4)));
        assert!(e.runnable_at(Some(8)));
        assert!(e.runnable_at(None)); // unknown → permissive
    }

    #[test]
    fn sd_models_dir_is_under_config() {
        if let Some(d) = sd_models_dir() {
            assert!(d.ends_with("sdmodels"), "{d:?}");
            assert!(d.to_string_lossy().contains("itsjustcad"), "{d:?}");
        }
    }
}
