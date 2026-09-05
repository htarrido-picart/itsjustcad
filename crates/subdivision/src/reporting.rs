// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! Yield reporting (M-intemfit Phase 11).
//!
//! Pure, geometry-free summary math: given the parts of an intemfit layout
//! (lot areas, frontage lengths, gross site area, placed open-space areas and
//! reserved-block areas, built GFA, building count, footprint areas), compute a
//! [`YieldReport`] — a decision-grade yield summary that reports on the **net
//! developable area** (site minus open space) rather than the gross site, so the
//! number does not lie once open space exists.
//!
//! The commands crate owns the bridge that pulls these inputs out of the
//! document (lots on the `lots` layer, `openspace:*` objects on the `openspace`
//! layer, `building:mass` GFA); this module never touches the document, so its
//! math is unit-tested headless (analytic assertions).
//!
//! Key derived numbers (§11):
//! - **net developable area** = gross site − (open-space features + reserved
//!   blocks). Reported alongside gross so the open-space ratio is visible.
//! - **FAR** = total GFA / net developable area (the honest FAR: floor area over
//!   land you can actually build on, not the whole site).
//! - **lot coverage %** = Σ footprint area / Σ lot area, when footprints exist.

/// Inputs to a yield computation. Plain scalars/vectors — no geometry types, so
/// this stays testable headless and the bridge does all document extraction.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct YieldInputs {
    /// Area of every subdivided lot (m²), one entry per lot.
    pub lot_areas: Vec<f64>,
    /// Frontage length of every lot (m), measured at the setback line (§5).
    /// May be empty when frontage was not measured; then frontage stats are
    /// omitted.
    pub lot_frontages: Vec<f64>,
    /// GROSS site area (m²) — the whole boundary, before netting open space.
    pub gross_site_area: f64,
    /// Area of each placed open-space FEATURE (park / greenway / pond / tree-
    /// save) in m².
    pub open_space_feature_areas: Vec<f64>,
    /// Area of each RESERVED block (blind %-reserve) pulled out of subdivision
    /// (m²). Netted out the same as features.
    pub reserved_block_areas: Vec<f64>,
    /// Built GFA of every building (m²) — Σ per-floor areas, net of step-backs.
    pub building_gfas: Vec<f64>,
    /// Footprint (ground-floor) area of every building (m²). Empty when no
    /// footprints are present; then lot-coverage is omitted.
    pub building_footprints: Vec<f64>,
}

/// A computed yield summary (§11). All areas m², FAR/ratios dimensionless.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct YieldReport {
    // ── Lots ────────────────────────────────────────────────────────────
    pub lot_count: usize,
    pub total_lot_area: f64,
    pub avg_lot_area: f64,
    pub min_lot_area: f64,
    pub max_lot_area: f64,

    // ── Frontage (measured at the setback line, §5) ─────────────────────
    /// `None` when no frontages were supplied.
    pub frontage: Option<FrontageStats>,

    // ── Site: gross vs net-of-open-space (the §11 point) ────────────────
    pub gross_site_area: f64,
    pub open_space_feature_area: f64,
    pub reserved_block_area: f64,
    /// gross − (features + reserved), floored at 0.
    pub net_developable_area: f64,
    /// (features + reserved) / gross, in [0, 1]. 0 when no open space / no site.
    pub open_space_ratio: f64,

    // ── Built yield (Phase 10) ──────────────────────────────────────────
    pub building_count: usize,
    /// Σ per-floor building areas.
    pub total_gfa: f64,
    /// GFA / net developable area. `None` when net area is 0 (nothing to divide
    /// by → an FAR would be a lie / a division by zero).
    pub far_net: Option<f64>,
    /// GFA / gross site area — reported alongside so the two are comparable.
    /// `None` when gross is 0.
    pub far_gross: Option<f64>,
    /// Σ footprint / Σ lot area, in [0, 1]. `None` when no footprints supplied.
    pub lot_coverage: Option<f64>,
}

/// Frontage distribution stats (m).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct FrontageStats {
    pub count: usize,
    pub total: f64,
    pub avg: f64,
    pub min: f64,
    pub max: f64,
}

impl YieldReport {
    /// Compute the yield summary from its inputs. Pure + deterministic — the
    /// same inputs always give the same report (the ItsJustCAD replay invariant).
    pub fn compute(inp: &YieldInputs) -> YieldReport {
        let lot_count = inp.lot_areas.len();
        let total_lot_area: f64 = inp.lot_areas.iter().copied().sum();
        let avg_lot_area = if lot_count > 0 { total_lot_area / lot_count as f64 } else { 0.0 };
        let min_lot_area = inp.lot_areas.iter().copied().fold(f64::INFINITY, f64::min);
        let max_lot_area = inp.lot_areas.iter().copied().fold(0.0f64, f64::max);
        let (min_lot_area, max_lot_area) =
            if lot_count > 0 { (min_lot_area, max_lot_area) } else { (0.0, 0.0) };

        let frontage = if inp.lot_frontages.is_empty() {
            None
        } else {
            let count = inp.lot_frontages.len();
            let total: f64 = inp.lot_frontages.iter().copied().sum();
            Some(FrontageStats {
                count,
                total,
                avg: total / count as f64,
                min: inp.lot_frontages.iter().copied().fold(f64::INFINITY, f64::min),
                max: inp.lot_frontages.iter().copied().fold(0.0f64, f64::max),
            })
        };

        let open_space_feature_area: f64 = inp.open_space_feature_areas.iter().copied().sum();
        let reserved_block_area: f64 = inp.reserved_block_areas.iter().copied().sum();
        let open_space_total = open_space_feature_area + reserved_block_area;
        let gross = inp.gross_site_area.max(0.0);
        let net_developable_area = (gross - open_space_total).max(0.0);
        let open_space_ratio =
            if gross > 0.0 { (open_space_total / gross).clamp(0.0, 1.0) } else { 0.0 };

        let building_count = inp.building_gfas.len();
        let total_gfa: f64 = inp.building_gfas.iter().copied().sum();
        let far_net = if net_developable_area > 0.0 {
            Some(total_gfa / net_developable_area)
        } else {
            None
        };
        let far_gross = if gross > 0.0 { Some(total_gfa / gross) } else { None };

        let lot_coverage = if inp.building_footprints.is_empty() || total_lot_area <= 0.0 {
            None
        } else {
            let fp: f64 = inp.building_footprints.iter().copied().sum();
            Some((fp / total_lot_area).clamp(0.0, 1.0))
        };

        YieldReport {
            lot_count,
            total_lot_area,
            avg_lot_area,
            min_lot_area,
            max_lot_area,
            frontage,
            gross_site_area: gross,
            open_space_feature_area,
            reserved_block_area,
            net_developable_area,
            open_space_ratio,
            building_count,
            total_gfa,
            far_net,
            far_gross,
            lot_coverage,
        }
    }

    /// `true` when there is nothing to report (no lots, no site, no buildings) —
    /// the bridge turns this into a clean "nothing to report" message instead of
    /// emitting an empty report.
    pub fn is_empty(&self) -> bool {
        self.lot_count == 0 && self.gross_site_area <= 0.0 && self.building_count == 0
    }

    /// Render the report as a compact GitHub-flavoured Markdown table (the chat
    /// renders markdown tables — M-chatmd) so it reads cleanly in the deck.
    /// `title` is a short label for the run (e.g. `"yield"` or `"A"`).
    pub fn to_markdown(&self, title: &str) -> String {
        let mut s = String::new();
        s.push_str(&format!("**Yield — {title}**\n\n"));
        s.push_str("| Metric | Value |\n|---|---|\n");
        s.push_str(&format!("| Lots | {} |\n", self.lot_count));
        s.push_str(&format!("| Total lot area | {:.0} m² |\n", self.total_lot_area));
        s.push_str(&format!(
            "| Lot area (min / avg / max) | {:.0} / {:.0} / {:.0} m² |\n",
            self.min_lot_area, self.avg_lot_area, self.max_lot_area
        ));
        if let Some(f) = &self.frontage {
            s.push_str(&format!(
                "| Frontage (min / avg / max) | {:.1} / {:.1} / {:.1} m |\n",
                f.min, f.avg, f.max
            ));
        }
        s.push_str(&format!("| Gross site area | {:.0} m² |\n", self.gross_site_area));
        s.push_str(&format!(
            "| Open space (features / reserved) | {:.0} / {:.0} m² |\n",
            self.open_space_feature_area, self.reserved_block_area
        ));
        s.push_str(&format!(
            "| **Net developable area** | {:.0} m² |\n",
            self.net_developable_area
        ));
        s.push_str(&format!("| Open-space ratio | {:.1}% |\n", self.open_space_ratio * 100.0));
        s.push_str(&format!("| Buildings | {} |\n", self.building_count));
        s.push_str(&format!("| Total GFA | {:.0} m² |\n", self.total_gfa));
        match self.far_net {
            Some(v) => s.push_str(&format!("| **FAR (net)** | {v:.2} |\n")),
            None => s.push_str("| **FAR (net)** | n/a |\n"),
        }
        match self.far_gross {
            Some(v) => s.push_str(&format!("| FAR (gross) | {v:.2} |\n")),
            None => s.push_str("| FAR (gross) | n/a |\n"),
        }
        if let Some(c) = self.lot_coverage {
            s.push_str(&format!("| Lot coverage | {:.1}% |\n", c * 100.0));
        }
        s
    }
}

/// A two-run A/B yield comparison (§11 option comparison). `a` is the earlier
/// run, `b` the later; deltas are `b − a`.
#[derive(Debug, Clone, PartialEq)]
pub struct YieldComparison {
    pub delta_lot_count: i64,
    pub delta_net_area: f64,
    pub delta_gfa: f64,
    /// `far_net` delta, `None` when either run had no net FAR.
    pub delta_far_net: Option<f64>,
    pub delta_open_space_ratio: f64,
}

impl YieldComparison {
    /// Diff two yield reports (`b − a`).
    pub fn diff(a: &YieldReport, b: &YieldReport) -> YieldComparison {
        YieldComparison {
            delta_lot_count: b.lot_count as i64 - a.lot_count as i64,
            delta_net_area: b.net_developable_area - a.net_developable_area,
            delta_gfa: b.total_gfa - a.total_gfa,
            delta_far_net: match (a.far_net, b.far_net) {
                (Some(pa), Some(pb)) => Some(pb - pa),
                _ => None,
            },
            delta_open_space_ratio: b.open_space_ratio - a.open_space_ratio,
        }
    }

    /// Render the comparison as a Markdown table with A, B, and Δ columns.
    pub fn to_markdown(&self, a: &YieldReport, b: &YieldReport) -> String {
        let signed = |v: f64, dp: usize| {
            if v >= 0.0 {
                format!("+{v:.dp$}")
            } else {
                format!("{v:.dp$}")
            }
        };
        let mut s = String::from("**Yield comparison (Δ = B − A)**\n\n");
        s.push_str("| Metric | A | B | Δ |\n|---|---|---|---|\n");
        s.push_str(&format!(
            "| Lots | {} | {} | {}{} |\n",
            a.lot_count,
            b.lot_count,
            if self.delta_lot_count >= 0 { "+" } else { "" },
            self.delta_lot_count
        ));
        s.push_str(&format!(
            "| Net area (m²) | {:.0} | {:.0} | {} |\n",
            a.net_developable_area,
            b.net_developable_area,
            signed(self.delta_net_area, 0)
        ));
        s.push_str(&format!(
            "| GFA (m²) | {:.0} | {:.0} | {} |\n",
            a.total_gfa,
            b.total_gfa,
            signed(self.delta_gfa, 0)
        ));
        let far = |r: &YieldReport| r.far_net.map(|v| format!("{v:.2}")).unwrap_or_else(|| "n/a".into());
        s.push_str(&format!(
            "| FAR (net) | {} | {} | {} |\n",
            far(a),
            far(b),
            self.delta_far_net.map(|v| signed(v, 2)).unwrap_or_else(|| "n/a".into())
        ));
        s.push_str(&format!(
            "| Open-space ratio | {:.1}% | {:.1}% | {} |\n",
            a.open_space_ratio * 100.0,
            b.open_space_ratio * 100.0,
            signed(self.delta_open_space_ratio * 100.0, 1)
        ));
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx(a: f64, b: f64, tol: f64) -> bool {
        (a - b).abs() <= tol
    }

    #[test]
    fn known_site_lot_and_area_stats() {
        let inp = YieldInputs {
            lot_areas: vec![100.0, 200.0, 300.0],
            gross_site_area: 1000.0,
            ..Default::default()
        };
        let r = YieldReport::compute(&inp);
        assert_eq!(r.lot_count, 3);
        assert!(approx(r.total_lot_area, 600.0, 1e-9));
        assert!(approx(r.avg_lot_area, 200.0, 1e-9));
        assert!(approx(r.min_lot_area, 100.0, 1e-9));
        assert!(approx(r.max_lot_area, 300.0, 1e-9));
    }

    #[test]
    fn net_equals_gross_when_no_open_space() {
        let inp = YieldInputs {
            lot_areas: vec![500.0, 500.0],
            gross_site_area: 1000.0,
            ..Default::default()
        };
        let r = YieldReport::compute(&inp);
        assert!(approx(r.net_developable_area, r.gross_site_area, 1e-9));
        assert!(approx(r.open_space_ratio, 0.0, 1e-12));
    }

    #[test]
    fn net_subtracts_open_space_and_reserved() {
        let inp = YieldInputs {
            lot_areas: vec![700.0],
            gross_site_area: 1000.0,
            open_space_feature_areas: vec![100.0],
            reserved_block_areas: vec![200.0],
            ..Default::default()
        };
        let r = YieldReport::compute(&inp);
        // net = 1000 − (100 + 200) = 700
        assert!(approx(r.net_developable_area, 700.0, 1e-9));
        assert!(approx(r.open_space_ratio, 0.3, 1e-9));
    }

    #[test]
    fn far_is_gfa_over_net() {
        let inp = YieldInputs {
            lot_areas: vec![800.0],
            gross_site_area: 1000.0,
            open_space_feature_areas: vec![200.0], // net = 800
            building_gfas: vec![400.0, 400.0],     // GFA = 800
            ..Default::default()
        };
        let r = YieldReport::compute(&inp);
        assert!(approx(r.total_gfa, 800.0, 1e-9));
        assert_eq!(r.building_count, 2);
        // FAR net = 800 / 800 = 1.0; FAR gross = 800 / 1000 = 0.8
        assert!(approx(r.far_net.unwrap(), 1.0, 1e-9));
        assert!(approx(r.far_gross.unwrap(), 0.8, 1e-9));
    }

    #[test]
    fn lot_coverage_when_footprints_present() {
        let inp = YieldInputs {
            lot_areas: vec![1000.0],
            gross_site_area: 1000.0,
            building_footprints: vec![250.0, 250.0],
            ..Default::default()
        };
        let r = YieldReport::compute(&inp);
        assert!(approx(r.lot_coverage.unwrap(), 0.5, 1e-9));
    }

    #[test]
    fn frontage_stats_present_and_absent() {
        let with = YieldReport::compute(&YieldInputs {
            lot_areas: vec![100.0],
            lot_frontages: vec![10.0, 20.0, 30.0],
            gross_site_area: 100.0,
            ..Default::default()
        });
        let f = with.frontage.unwrap();
        assert_eq!(f.count, 3);
        assert!(approx(f.avg, 20.0, 1e-9));
        assert!(approx(f.min, 10.0, 1e-9));
        assert!(approx(f.max, 30.0, 1e-9));

        let without = YieldReport::compute(&YieldInputs {
            lot_areas: vec![100.0],
            gross_site_area: 100.0,
            ..Default::default()
        });
        assert!(without.frontage.is_none());
    }

    #[test]
    fn empty_report_flagged() {
        let r = YieldReport::compute(&YieldInputs::default());
        assert!(r.is_empty());
        assert!(r.far_net.is_none());
        assert!(r.far_gross.is_none());
        assert!(r.lot_coverage.is_none());
    }

    #[test]
    fn far_none_when_net_is_zero() {
        // All site consumed by open space → net 0 → FAR net undefined (not a
        // division by zero / a lie).
        let inp = YieldInputs {
            lot_areas: vec![],
            gross_site_area: 500.0,
            reserved_block_areas: vec![500.0],
            building_gfas: vec![100.0],
            ..Default::default()
        };
        let r = YieldReport::compute(&inp);
        assert!(approx(r.net_developable_area, 0.0, 1e-9));
        assert!(r.far_net.is_none());
        assert!(r.far_gross.is_some());
    }

    #[test]
    fn deterministic() {
        let inp = YieldInputs {
            lot_areas: vec![120.0, 340.0, 90.0],
            lot_frontages: vec![6.0, 8.0, 10.0],
            gross_site_area: 2000.0,
            open_space_feature_areas: vec![150.0],
            reserved_block_areas: vec![50.0],
            building_gfas: vec![300.0, 500.0],
            building_footprints: vec![100.0, 150.0],
        };
        let a = YieldReport::compute(&inp);
        let b = YieldReport::compute(&inp);
        assert_eq!(a, b);
    }

    #[test]
    fn compare_two_runs_deltas() {
        let a = YieldReport::compute(&YieldInputs {
            lot_areas: vec![500.0, 500.0],
            gross_site_area: 1000.0,
            building_gfas: vec![500.0],
            ..Default::default()
        });
        let b = YieldReport::compute(&YieldInputs {
            lot_areas: vec![300.0, 300.0, 300.0],
            gross_site_area: 1000.0,
            open_space_feature_areas: vec![100.0], // net 900
            building_gfas: vec![600.0, 300.0],     // GFA 900
            ..Default::default()
        });
        let c = YieldComparison::diff(&a, &b);
        assert_eq!(c.delta_lot_count, 1); // 3 − 2
        assert!(approx(c.delta_net_area, -100.0, 1e-9)); // 900 − 1000
        assert!(approx(c.delta_gfa, 400.0, 1e-9)); // 900 − 500
        // FAR net: a = 500/1000 = 0.5; b = 900/900 = 1.0; Δ = +0.5
        assert!(approx(c.delta_far_net.unwrap(), 0.5, 1e-9));
        // markdown renders without panic
        let md = c.to_markdown(&a, &b);
        assert!(md.contains("| Metric | A | B | Δ |"));
    }

    #[test]
    fn markdown_contains_key_rows() {
        let r = YieldReport::compute(&YieldInputs {
            lot_areas: vec![100.0, 200.0],
            gross_site_area: 1000.0,
            open_space_feature_areas: vec![100.0],
            building_gfas: vec![300.0],
            building_footprints: vec![80.0],
            ..Default::default()
        });
        let md = r.to_markdown("yield");
        assert!(md.contains("| Metric | Value |"));
        assert!(md.contains("Net developable area"));
        assert!(md.contains("FAR (net)"));
        assert!(md.contains("Lot coverage"));
    }
}
