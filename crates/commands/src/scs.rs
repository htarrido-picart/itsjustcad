// SPDX-License-Identifier: AGPL-3.0-only
// Copyright © 2026 Hector Tarrido-Picart

//! SendCutSend material data (`sendcutsend-specs.json` +
//! `sendcutsend-catalog.json`) read from a local cache directory.
//!
//! SendCutSend publishes both files daily as evidence feeds for tools, not as
//! a stable API, and grants no redistribution license — so nothing is bundled
//! and the core never fetches. The host (or the user) downloads the files into
//! [`default_dir`]; this loader reads them tolerantly: unknown fields are
//! ignored, values may be numbers or strings (`"0.125"`, `"+0.005"`,
//! `"44 x 30"`), and a missing or unparsable value is `None` so `cutcheck`
//! falls back to the rule of thumb and says so. All values are inches, as the
//! feeds declare.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::Value;

pub const SPECS_FILE: &str = "sendcutsend-specs.json";
pub const CATALOG_FILE: &str = "sendcutsend-catalog.json";
pub const FEED_URL: &str = "https://cdn.sendcutsend.com/specs/";

/// The cache directory: `~/.config/itsjustcad/sendcutsend/`.
pub fn default_dir() -> Option<PathBuf> {
    Some(crate::config_dir()?.join("sendcutsend"))
}

/// How to get the data, for messages when the cache is missing.
pub fn how_to_get(dir: &Path) -> String {
    format!(
        "SendCutSend data not found in {}: download {FEED_URL}{SPECS_FILE} and {FEED_URL}{CATALOG_FILE} \
         into that folder (or use the app's \"Update SendCutSend data\" action)",
        dir.display()
    )
}

/// A published `(a, b)` part size, inches.
pub type Size = (f64, f64);

/// One material (sheet SKU), merged from both feeds. Lengths in inches.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Material {
    pub sku: String,
    pub name: String,
    pub category: String,
    pub process: Option<String>,
    pub thickness: Option<f64>,
    /// `(a, b)` as published (not sorted).
    pub min_part: Option<(f64, f64)>,
    pub max_part: Option<(f64, f64)>,
    pub min_hole: Option<f64>,
    pub min_bridge: Option<f64>,
    pub min_hole_to_edge: Option<f64>,
    pub services: Vec<String>,
    pub out_of_stock: bool,
}

/// Loaded feeds.
#[derive(Clone, Debug, Default)]
pub struct ScsData {
    pub dir: PathBuf,
    /// The feeds' `source_data_generated_at` (else `generated_at`).
    pub date: Option<String>,
    /// Sorted by SKU.
    pub materials: Vec<Material>,
    /// Which feeds were missing or unreadable (non-fatal when the other loaded).
    pub warnings: Vec<String>,
}

/// A number from a JSON number or a string like `"0.125"`, `"+0.005"`,
/// `"0.300\""`; `None` for `"N/A"` and anything else.
pub fn num(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => {
            let t = s.trim().trim_start_matches('+');
            let end = t
                .char_indices()
                .find(|&(i, c)| !(c.is_ascii_digit() || c == '.' || (i == 0 && c == '-')))
                .map_or(t.len(), |(i, _)| i);
            t[..end].parse().ok()
        }
        _ => None,
    }
    .filter(|x: &f64| x.is_finite())
}

/// A size like `"3.0x3.0"`, `"44 x 30"`, `"1.00 X 2.00"`.
pub fn size(v: &Value) -> Option<(f64, f64)> {
    let s = v.as_str()?.to_ascii_lowercase();
    let (a, b) = s.split_once('x')?;
    let n = |t: &str| num(&Value::String(t.trim().to_string()));
    Some((n(a)?, n(b)?)).filter(|(a, b)| *a > 0.0 && *b > 0.0)
}

fn text(v: Option<&Value>) -> Option<String> {
    v.and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn meta_date(root: &Value) -> Option<String> {
    let m = root.get("_meta")?;
    text(m.get("source_data_generated_at")).or_else(|| text(m.get("generated_at")))
}

fn read_json(path: &Path) -> Result<Value, String> {
    let s = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    serde_json::from_str(&s).map_err(|e| format!("{}: not valid JSON ({e})", path.display()))
}

/// Parse already-read feeds (either may be absent).
pub fn from_values(specs: Option<&Value>, catalog: Option<&Value>) -> ScsData {
    let mut by_sku: BTreeMap<String, Material> = BTreeMap::new();
    let entries = |root: Option<&Value>| -> Vec<Value> {
        root.and_then(|r| r.get("materials"))
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
    };
    for m in entries(catalog) {
        let Some(sku) = text(m.get("sku")) else {
            continue;
        };
        let e = by_sku.entry(sku.clone()).or_insert_with(|| Material {
            sku,
            ..Default::default()
        });
        e.name = text(m.get("name")).unwrap_or_default();
        e.category = text(m.get("category")).unwrap_or_default();
        e.process = text(m.get("cutting_process"));
        e.thickness = m.get("thickness").and_then(num);
        e.min_part = m.get("min_part_size").and_then(size);
        e.max_part = m.get("max_part_size").and_then(size);
        e.out_of_stock = m
            .get("out_of_stock")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        e.services = m
            .get("available_services")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(|s| text(Some(s))).collect())
            .unwrap_or_default();
    }
    for m in entries(specs) {
        let Some(sku) = text(m.get("sku")) else {
            continue;
        };
        let e = by_sku.entry(sku.clone()).or_insert_with(|| Material {
            sku,
            ..Default::default()
        });
        let cut = m.get("cutting_specs");
        let field = |k: &str| cut.and_then(|c| c.get(k));
        if e.thickness.is_none() {
            e.thickness = m
                .get("general_specs")
                .and_then(|g| g.get("thickness"))
                .and_then(num);
        }
        if e.process.is_none() {
            e.process = text(field("cutting_process"));
        }
        // Catalog part sizes win; the specs copy fills gaps.
        e.min_part = e.min_part.or_else(|| field("min_part_size").and_then(size));
        e.max_part = e.max_part.or_else(|| field("max_part_size").and_then(size));
        e.min_hole = field("min_hole_size").and_then(num).filter(|v| *v > 0.0);
        e.min_bridge = field("min_bridge_size").and_then(num).filter(|v| *v > 0.0);
        e.min_hole_to_edge = field("min_hole_to_edge").and_then(num).filter(|v| *v > 0.0);
    }
    ScsData {
        dir: PathBuf::new(),
        date: specs
            .and_then(meta_date)
            .or_else(|| catalog.and_then(meta_date)),
        materials: by_sku.into_values().collect(),
        warnings: Vec::new(),
    }
}

/// Load both feeds from `dir`. Errors only when neither feed is usable.
pub fn load(dir: &Path) -> Result<ScsData, String> {
    let specs = read_json(&dir.join(SPECS_FILE));
    let catalog = read_json(&dir.join(CATALOG_FILE));
    if specs.is_err() && catalog.is_err() {
        let missing = !dir.join(SPECS_FILE).exists() && !dir.join(CATALOG_FILE).exists();
        return Err(if missing {
            how_to_get(dir)
        } else {
            format!(
                "SendCutSend data unreadable: {}",
                specs.err().unwrap_or_default()
            )
        });
    }
    let mut d = from_values(specs.as_ref().ok(), catalog.as_ref().ok());
    d.dir = dir.to_path_buf();
    d.warnings = [specs.err(), catalog.err()].into_iter().flatten().collect();
    Ok(d)
}

impl Material {
    fn haystack(&self) -> String {
        format!(
            "{} {} {} {}",
            self.sku,
            self.name,
            self.category,
            self.process.as_deref().unwrap_or("")
        )
        .to_ascii_lowercase()
    }

    /// `min_part`/`max_part` as `(long, short)`.
    pub fn part_limits(&self) -> (Option<Size>, Option<Size>) {
        let sort = |(a, b): (f64, f64)| (a.max(b), a.min(b));
        (self.min_part.map(sort), self.max_part.map(sort))
    }
}

impl ScsData {
    /// Materials whose SKU/name/category/process contain every word of
    /// `filter` (case-insensitive). Empty filter = all.
    pub fn filter(&self, filter: &str) -> Vec<&Material> {
        let words: Vec<String> = filter
            .split_whitespace()
            .map(str::to_ascii_lowercase)
            .collect();
        self.materials
            .iter()
            .filter(|m| words.iter().all(|w| m.haystack().contains(w.as_str())))
            .collect()
    }

    /// Resolve `material=`: an exact SKU (case-insensitive), else the best
    /// word match — nearest to `thickness_in` when given, in stock first,
    /// then SKU order.
    pub fn resolve(&self, query: &str, thickness_in: Option<f64>) -> Result<&Material, String> {
        if let Some(m) = self
            .materials
            .iter()
            .find(|m| m.sku.eq_ignore_ascii_case(query.trim()))
        {
            return Ok(m);
        }
        let words = query.replace(['-', '_'], " ");
        let mut hits = self.filter(&words);
        if hits.is_empty() {
            return Err(format!(
                "no SendCutSend material matches '{query}' — `cutmaterials <words>` lists SKUs"
            ));
        }
        if thickness_in.is_none() && hits.len() > 1 {
            let skus: Vec<&str> = hits.iter().take(12).map(|m| m.sku.as_str()).collect();
            return Err(format!(
                "'{query}' matches {} materials ({}{}); give thickness= or an exact SKU",
                hits.len(),
                skus.join(", "),
                if hits.len() > 12 { ", …" } else { "" }
            ));
        }
        let dist = |m: &Material| match (thickness_in, m.thickness) {
            (Some(t), Some(mt)) => (mt - t).abs(),
            (Some(_), None) => f64::INFINITY,
            _ => 0.0,
        };
        hits.sort_by(|a, b| {
            dist(a)
                .total_cmp(&dist(b))
                .then(a.out_of_stock.cmp(&b.out_of_stock))
                .then(a.sku.cmp(&b.sku))
        });
        Ok(hits[0])
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) const SPECS: &str =
        include_str!("../tests/fixtures/sendcutsend/sendcutsend-specs.json");
    pub(crate) const CATALOG: &str =
        include_str!("../tests/fixtures/sendcutsend/sendcutsend-catalog.json");

    /// Write the fixtures into this test thread's config dir.
    pub(crate) fn install_fixtures() -> PathBuf {
        let dir = default_dir().unwrap();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(SPECS_FILE), SPECS).unwrap();
        std::fs::write(dir.join(CATALOG_FILE), CATALOG).unwrap();
        dir
    }

    fn data() -> ScsData {
        let v = |s: &str| serde_json::from_str::<Value>(s).unwrap();
        from_values(Some(&v(SPECS)), Some(&v(CATALOG)))
    }

    #[test]
    fn tolerant_value_parsing() {
        assert_eq!(num(&Value::String("+0.005".into())), Some(0.005));
        assert_eq!(num(&Value::String("0.300\"".into())), Some(0.3));
        assert_eq!(num(&Value::String("N/A".into())), None);
        assert_eq!(num(&serde_json::json!(0.25)), Some(0.25));
        assert_eq!(size(&Value::String("44 x 30".into())), Some((44.0, 30.0)));
        assert_eq!(size(&Value::String("1.00 X 2.00".into())), Some((1.0, 2.0)));
        assert_eq!(size(&Value::String("not a size".into())), None);
    }

    #[test]
    fn merges_feeds_and_ignores_unknown_fields() {
        let d = data();
        assert_eq!(d.date.as_deref(), Some("2026-01-02T03:04:05"));
        assert_eq!(d.materials.len(), 4, "entry without sku skipped");
        let al = d.materials.iter().find(|m| m.sku == "FIX-AL-125").unwrap();
        assert_eq!(al.thickness, Some(0.125));
        assert_eq!(
            (al.min_hole, al.min_bridge, al.min_hole_to_edge),
            (Some(0.05), Some(0.06), Some(0.04))
        );
        assert_eq!(al.part_limits(), (Some((0.5, 0.25)), Some((40.0, 20.0))));
        assert_eq!(al.services, ["bending", "anodizing"]);
        let thin = d.materials.iter().find(|m| m.sku == "FIX-AL-063").unwrap();
        assert_eq!(
            (thin.min_bridge, thin.min_hole_to_edge, thin.max_part),
            (None, None, None)
        );
        let st = d.materials.iter().find(|m| m.sku == "FIX-ST-250").unwrap();
        assert_eq!(st.thickness, Some(0.25), "catalog string thickness");
        assert_eq!(
            st.min_part,
            Some((1.0, 2.0)),
            "spec size fills the catalog gap"
        );
    }

    #[test]
    fn resolve_by_sku_words_and_thickness() {
        let d = data();
        assert_eq!(d.resolve("fix-st-250", None).unwrap().sku, "FIX-ST-250");
        // 3 mm ≈ 0.118 in → the 0.125 sheets; tie broken by SKU.
        assert_eq!(
            d.resolve("aluminum", Some(3.0 / 25.4)).unwrap().sku,
            "FIX-AL-125"
        );
        assert_eq!(
            d.resolve("7075 aluminum", Some(0.1)).unwrap().sku,
            "FIX-AL7-125"
        );
        assert!(
            d.resolve("aluminum", None)
                .unwrap_err()
                .contains("thickness=")
        );
        assert!(d.resolve("unobtainium", Some(0.1)).is_err());
        assert_eq!(d.filter("steel").len(), 1);
    }

    #[test]
    fn load_reports_missing_cache_with_instructions() {
        let dir = std::env::temp_dir().join("ijc_scs_definitely_missing_dir");
        let e = load(&dir).unwrap_err();
        assert!(e.contains(SPECS_FILE) && e.contains(FEED_URL), "{e}");
        let ok = load(&install_fixtures()).unwrap();
        assert!(ok.warnings.is_empty());
        assert_eq!(ok.materials.len(), 4);
    }
}
