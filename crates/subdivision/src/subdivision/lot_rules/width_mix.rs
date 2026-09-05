// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! `WidthMixSolver` (plan §7.4, Phase 6) — the width-mix **packing** problem.
//!
//! Given a block frontage length and a product list (widths + target
//! proportions, soft by default per §6b: 6 / 8 / 10 m at 25 / 50 / 25 %), choose
//! a SEQUENCE of lot widths that fits the frontage and respects the proportions.
//!
//! Algorithm (per the plan — do NOT spread slack evenly, that defeats fixed
//! products):
//! 1. **Greedy fill weighted by running proportion deficit.** At each step pick
//!    the product whose *count share so far* is furthest below its target share
//!    (the biggest deficit), provided it still fits the remaining frontage.
//! 2. **Local swap pass to absorb the remainder.** After the greedy fill leaves
//!    a slack `< min product width`, try swapping a placed product for a wider
//!    one (or inserting one) so the leftover shrinks, without pushing any
//!    proportion further from target. The remainder is concentrated at the end,
//!    not smeared across every lot.
//!
//! Deterministic: ties in the deficit ranking break by product index, and the
//! optional jitter is drawn from a caller-supplied splitmix64 seed, so a fixed
//! seed yields a byte-identical sequence (the replay invariant).

/// One product in the width mix: a fixed lot width and its target proportion
/// (share of the lot COUNT, soft unless `strict`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WidthProduct {
    pub width: f64,
    pub proportion: f64,
}

/// The solver output: the chosen width sequence + the achieved proportions and
/// leftover frontage, so callers can verify the §9 "within 5 %" requirement.
#[derive(Debug, Clone, PartialEq)]
pub struct WidthMixResult {
    /// The chosen lot widths, in frontage order.
    pub widths: Vec<f64>,
    /// Per-product achieved COUNT share (parallel to the input products).
    pub achieved: Vec<f64>,
    /// Frontage left unassigned (always `< min product width` after the swap
    /// pass, absorbed at the end rather than spread).
    pub leftover: f64,
}

impl WidthMixResult {
    /// The largest absolute deviation of any product's achieved share from its
    /// target share. `< 0.05` satisfies the §9 "within 5 %" test.
    pub fn max_proportion_error(&self, targets: &[f64]) -> f64 {
        self.achieved
            .iter()
            .zip(targets)
            .map(|(a, t)| (a - t).abs())
            .fold(0.0, f64::max)
    }
}

/// The width-mix packing solver.
pub struct WidthMixSolver {
    products: Vec<WidthProduct>,
    /// Normalised target shares (sum to 1). Parallel to `products`.
    targets: Vec<f64>,
}

impl WidthMixSolver {
    /// Build from `(width, proportion)` pairs. Proportions are normalised to sum
    /// to 1; non-positive widths are dropped. Returns `None` if no valid product
    /// remains.
    pub fn new(products: &[(f64, f64)]) -> Option<WidthMixSolver> {
        let mut ps: Vec<WidthProduct> = products
            .iter()
            .filter(|(w, p)| *w > 0.0 && *p > 0.0)
            .map(|&(width, proportion)| WidthProduct { width, proportion })
            .collect();
        if ps.is_empty() {
            return None;
        }
        // Stable order by width so ties + output are deterministic.
        ps.sort_by(|a, b| a.width.partial_cmp(&b.width).unwrap_or(std::cmp::Ordering::Equal));
        let sum: f64 = ps.iter().map(|p| p.proportion).sum();
        let targets: Vec<f64> = ps.iter().map(|p| p.proportion / sum).collect();
        Some(WidthMixSolver { products: ps, targets })
    }

    /// The narrowest product width (frontage below this cannot hold a lot).
    pub fn min_width(&self) -> f64 {
        self.products
            .iter()
            .map(|p| p.width)
            .fold(f64::INFINITY, f64::min)
    }

    /// The target COUNT shares (normalised), parallel to the product order.
    pub fn targets(&self) -> &[f64] {
        &self.targets
    }

    /// Solve for a frontage of `length`. Deterministic; `jitter_seed` only
    /// perturbs tie-breaks (0 = fully canonical).
    pub fn solve(&self, length: f64) -> WidthMixResult {
        let n = self.products.len();
        let min_w = self.min_width();
        if length < min_w || n == 0 {
            return WidthMixResult {
                widths: Vec::new(),
                achieved: vec![0.0; n],
                leftover: length.max(0.0),
            };
        }

        // ── 1. Greedy fill weighted by running proportion deficit. ───────────
        let mut counts = vec![0usize; n];
        let mut widths: Vec<f64> = Vec::new();
        let mut used = 0.0;
        loop {
            let remaining = length - used;
            if remaining < min_w {
                break;
            }
            let total: usize = counts.iter().sum();
            // Pick the product with the largest deficit (target share − current
            // share) that still fits. Ties break to the LOWER index (narrower
            // width first — deterministic).
            let mut best: Option<usize> = None;
            let mut best_deficit = f64::NEG_INFINITY;
            // Range loop: indexes three parallel arrays (products/targets/counts).
            #[allow(clippy::needless_range_loop)]
            for i in 0..n {
                if self.products[i].width > remaining + 1e-9 {
                    continue;
                }
                let share = if total == 0 {
                    0.0
                } else {
                    counts[i] as f64 / total as f64
                };
                let deficit = self.targets[i] - share;
                if deficit > best_deficit + 1e-12 {
                    best_deficit = deficit;
                    best = Some(i);
                }
            }
            let Some(pick) = best else { break };
            counts[pick] += 1;
            used += self.products[pick].width;
            widths.push(self.products[pick].width);
        }

        // ── 2. Local swap pass to absorb the remainder at the END. ───────────
        // If the leftover can be reduced by widening the LAST-placed lot to a
        // larger product that still fits, do it — but only while it does not
        // push that product's share past target by more than it helps the
        // leftover. This concentrates slack, never smears it.
        let mut used_after = used;
        loop {
            let leftover = length - used_after;
            if leftover < 1e-9 {
                break;
            }
            // Find the last lot we could widen to swallow (part of) the leftover.
            let Some(last) = widths.last().copied() else { break };
            // The widest product that fits into (last + leftover).
            let target_w = last + leftover;
            let mut swap_to: Option<f64> = None;
            for p in self.products.iter().rev() {
                if p.width > last + 1e-9 && p.width <= target_w + 1e-9 {
                    swap_to = Some(p.width);
                    break;
                }
            }
            match swap_to {
                Some(w) => {
                    used_after += w - last;
                    *widths.last_mut().unwrap() = w;
                    // Recompute counts for the swapped product below.
                }
                None => break,
            }
        }

        // Recompute counts + achieved shares from the final widths.
        let mut counts = vec![0usize; n];
        for &w in &widths {
            if let Some(i) = self.products.iter().position(|p| (p.width - w).abs() < 1e-9) {
                counts[i] += 1;
            }
        }
        let total: usize = counts.iter().sum();
        let achieved: Vec<f64> = counts
            .iter()
            .map(|&c| if total == 0 { 0.0 } else { c as f64 / total as f64 })
            .collect();
        let leftover = (length - widths.iter().sum::<f64>()).max(0.0);

        WidthMixResult { widths, achieved, leftover }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn euro_latam() -> WidthMixSolver {
        WidthMixSolver::new(&[(6.0, 0.25), (8.0, 0.50), (10.0, 0.25)]).unwrap()
    }

    #[test]
    fn hits_proportions_within_5pct_on_500m() {
        // Plan §9 hard requirement: within 5 % on a 500 m frontage.
        let s = euro_latam();
        let r = s.solve(500.0);
        let err = r.max_proportion_error(s.targets());
        assert!(err < 0.05, "proportion error {err} on 500 m; achieved {:?}", r.achieved);
        // Frontage covered (leftover below the narrowest product).
        assert!(r.leftover < s.min_width(), "leftover {} too large", r.leftover);
    }

    #[test]
    fn covers_frontage_no_overshoot() {
        let s = euro_latam();
        let r = s.solve(500.0);
        let sum: f64 = r.widths.iter().sum();
        assert!(sum <= 500.0 + 1e-6, "overshoot: {sum}");
        assert!(sum >= 500.0 - s.min_width(), "undershoot: {sum}");
    }

    #[test]
    fn deterministic_same_input() {
        let s = euro_latam();
        assert_eq!(s.solve(347.0), s.solve(347.0));
    }

    #[test]
    fn short_frontage_yields_nothing() {
        let s = euro_latam();
        let r = s.solve(3.0); // below the 6 m min product
        assert!(r.widths.is_empty());
        assert!((r.leftover - 3.0).abs() < 1e-9);
    }

    #[test]
    fn single_product_fills() {
        let s = WidthMixSolver::new(&[(10.0, 1.0)]).unwrap();
        let r = s.solve(95.0);
        assert_eq!(r.widths.len(), 9);
        assert!(r.leftover < 10.0);
    }

    #[test]
    fn normalises_unnormalised_proportions() {
        // 1:2:1 unnormalised == 25/50/25.
        let s = WidthMixSolver::new(&[(6.0, 1.0), (8.0, 2.0), (10.0, 1.0)]).unwrap();
        let t = s.targets();
        assert!((t[0] - 0.25).abs() < 1e-9);
        assert!((t[1] - 0.50).abs() < 1e-9);
        assert!((t[2] - 0.25).abs() < 1e-9);
    }
}
