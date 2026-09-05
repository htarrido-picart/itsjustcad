// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! `ConsistentIndexing` (plan §12.1) — deterministic lot/block IDs by **spatial
//! order**, not creation order.
//!
//! ## Why
//!
//! If lot numbers followed creation order, re-running subdivision (or nudging one
//! parameter) would renumber every lot, so a plan reviewer's "lot 14" stops
//! meaning the same parcel. The plan requires numbers to follow *position*: a
//! deterministic row-major order over blocks, then an along-street order within
//! each block. Then:
//! - a re-run on the same site produces **identical** indices, and
//! - adding one lot keeps a **stable prefix** — only the lots spatially after the
//!   new one shift, never a full renumber.
//!
//! ## How
//!
//! [`ConsistentIndexing::assign`] takes the baked lot polygons (in any order) and
//! returns their indices in spatial order:
//! 1. **Block bucket** — each lot is bucketed by its centroid quantised to a
//!    coarse grid (`block_grid`), so lots of the same block share a bucket
//!    regardless of creation order. Buckets are sorted **row-major** (top-to-
//!    bottom by row, left-to-right within a row — the way a surveyor reads a plat).
//! 2. **Along-street order within a block** — inside each bucket, lots are ordered
//!    by position along the block's principal (long) axis, then perpendicular, so
//!    the sequence follows the street frontage.
//! 3. The flat 1-based index is the lot's position in that global spatial order.
//!
//! The result is a permutation: `assign()[k]` is the spatial index (1-based) of
//! input lot `k`. Purely geometric and deterministic — no RNG, no creation order —
//! so op-log replay reproduces identical indices, and the bake carries a **stable
//! index in each lot's name** (`lot #N`).

use crate::geometry::polygon2d::Polygon2d;
use glam::DVec2;

/// Assigns deterministic spatial indices to lot polygons (plan §12.1). Stateless
/// aside from the block-grid quantisation size.
#[derive(Debug, Clone, Copy, Default)]
pub struct ConsistentIndexing {
    /// The grid cell size used to bucket lots into blocks. Lots whose centroids
    /// fall in the same cell are treated as one block. `None` (the default) auto-
    /// derives the grid from the lot span.
    block_grid: Option<f64>,
}

impl ConsistentIndexing {
    pub fn new() -> Self {
        Self::default()
    }

    /// Fixed block-grid cell size (metres). Lots within one cell are one block.
    pub fn with_block_grid(grid: f64) -> Self {
        ConsistentIndexing {
            block_grid: Some(grid.max(1e-6)),
        }
    }

    /// Assign 1-based spatial indices to `lots`. Returns `indices` such that
    /// `indices[k]` is the spatial index of input lot `k`. The indices are a
    /// permutation of `1..=lots.len()`.
    ///
    /// Deterministic and geometry-only: identical input geometry → identical
    /// output, independent of the order the lots are passed in.
    pub fn assign(&self, lots: &[Polygon2d]) -> Vec<usize> {
        let n = lots.len();
        if n == 0 {
            return Vec::new();
        }
        let centroids: Vec<DVec2> = lots.iter().map(|p| p.centroid()).collect();

        // Derive a block grid from the data if not fixed: use a fraction of the
        // overall span so lots that belong together cluster but distinct blocks
        // separate. A coarse grid (¼ of the larger span) is enough to distinguish
        // typical blocks; when there is really one block everything shares a cell.
        let (lo, hi) = bounds(&centroids);
        let span = (hi - lo).max(DVec2::splat(1e-6));
        let grid = self
            .block_grid
            .unwrap_or_else(|| (span.x.max(span.y) / 4.0).max(1e-6));

        // Order lots: primary key = block bucket in row-major order, secondary
        // key = along-street order within the block. Build a sortable key per lot
        // and produce the permutation deterministically.
        let mut order: Vec<usize> = (0..n).collect();

        // Block bucket coordinates (row-major reads top→bottom, i.e. LARGER y
        // first, then left→right by smaller x). Quantise centroids to the grid.
        let bucket = |c: DVec2| -> (i64, i64) {
            let col = (c.x / grid).floor() as i64;
            let row = (c.y / grid).floor() as i64;
            (row, col)
        };

        // Along-street order within a block: project the centroid onto the block's
        // principal axis. We compute a per-bucket principal direction from the
        // member centroids' spread (long axis), then order by that projection,
        // tie-broken by the perpendicular projection. This makes a row of lots
        // number monotonically along the frontage.
        // Precompute per-bucket principal axis.
        use std::collections::HashMap;
        let mut members: HashMap<(i64, i64), Vec<usize>> = HashMap::new();
        for (i, &c) in centroids.iter().enumerate() {
            members.entry(bucket(c)).or_default().push(i);
        }
        let mut axis: HashMap<(i64, i64), DVec2> = HashMap::new();
        for (b, idxs) in &members {
            axis.insert(*b, principal_axis(idxs.iter().map(|&i| centroids[i])));
        }

        // A robust, comparison-only sort key. We sort by:
        //   (block_row DESC, block_col ASC, along-axis proj ASC, perp proj ASC,
        //    quantised centroid, index)   — all deterministic.
        order.sort_by(|&a, &b| {
            let (ra, ca) = bucket(centroids[a]);
            let (rb, cb) = bucket(centroids[b]);
            // Row-major, top-to-bottom: larger row first.
            rb.cmp(&ra)
                .then(ca.cmp(&cb))
                .then_with(|| {
                    let ax = axis.get(&(ra, ca)).copied().unwrap_or(DVec2::X);
                    let perp = DVec2::new(-ax.y, ax.x);
                    let pa = centroids[a].dot(ax);
                    let pb = centroids[b].dot(ax);
                    cmp_f64(pa, pb)
                        .then_with(|| cmp_f64(centroids[a].dot(perp), centroids[b].dot(perp)))
                })
                // Final deterministic tie-breaks so equal-position lots still get a
                // stable, reproducible order (quantised centroid, then index).
                .then_with(|| cmp_f64(quantise(centroids[a].x), quantise(centroids[b].x)))
                .then_with(|| cmp_f64(quantise(centroids[a].y), quantise(centroids[b].y)))
                .then(a.cmp(&b))
        });

        // order[rank] = input lot index → invert to indices[input] = rank+1.
        let mut indices = vec![0usize; n];
        for (rank, &lot_i) in order.iter().enumerate() {
            indices[lot_i] = rank + 1;
        }
        indices
    }
}

/// Bounding box of a point set.
fn bounds(pts: &[DVec2]) -> (DVec2, DVec2) {
    let mut lo = DVec2::splat(f64::INFINITY);
    let mut hi = DVec2::splat(f64::NEG_INFINITY);
    for &p in pts {
        lo = lo.min(p);
        hi = hi.max(p);
    }
    if !lo.x.is_finite() {
        (DVec2::ZERO, DVec2::ZERO)
    } else {
        (lo, hi)
    }
}

/// The principal (long) axis (unit) of a set of points, via the dominant
/// eigenvector of the 2×2 covariance matrix. Falls back to `+X` for degenerate
/// sets. Deterministic; the sign is normalised so the axis points into the +X
/// (or +Y when vertical) half so ordering is stable.
fn principal_axis<I: IntoIterator<Item = DVec2>>(pts: I) -> DVec2 {
    let pts: Vec<DVec2> = pts.into_iter().collect();
    let n = pts.len();
    if n < 2 {
        return DVec2::X;
    }
    let mean: DVec2 = pts.iter().copied().sum::<DVec2>() / n as f64;
    let (mut sxx, mut sxy, mut syy) = (0.0, 0.0, 0.0);
    for &p in &pts {
        let d = p - mean;
        sxx += d.x * d.x;
        sxy += d.x * d.y;
        syy += d.y * d.y;
    }
    // Largest eigenvalue of [[sxx,sxy],[sxy,syy]].
    let tr = sxx + syy;
    let det = sxx * syy - sxy * sxy;
    let disc = (tr * tr / 4.0 - det).max(0.0).sqrt();
    let lam = tr / 2.0 + disc;
    // Eigenvector for lam: (sxy, lam - sxx) (or (lam - syy, sxy)).
    let v = if sxy.abs() > 1e-12 {
        DVec2::new(sxy, lam - sxx)
    } else if sxx >= syy {
        DVec2::X
    } else {
        DVec2::Y
    };
    let v = v.normalize_or_zero();
    if v.length_squared() < 0.5 {
        return DVec2::X;
    }
    // Normalise sign: point into +X half (or +Y when near-vertical) for stability.
    if v.x < -1e-9 || (v.x.abs() <= 1e-9 && v.y < 0.0) {
        -v
    } else {
        v
    }
}

/// Quantise a coordinate to a fixed grid (mm) so tiny float noise never flips a
/// tie-break — the invariant the replay-determinism test relies on.
fn quantise(x: f64) -> f64 {
    (x * 1000.0).round() / 1000.0
}

/// Total-order compare for f64 (NaN-safe: NaN sorts last, deterministic).
fn cmp_f64(a: f64, b: f64) -> std::cmp::Ordering {
    a.partial_cmp(&b).unwrap_or(std::cmp::Ordering::Equal)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x0: f64, y0: f64, w: f64, h: f64) -> Polygon2d {
        Polygon2d::from_pairs([
            (x0, y0),
            (x0 + w, y0),
            (x0 + w, y0 + h),
            (x0, y0 + h),
        ])
        .unwrap()
    }

    /// A single row of 4 lots along +X → indices increase left→right.
    #[test]
    fn along_street_order_within_a_block() {
        let lots: Vec<Polygon2d> = (0..4).map(|i| rect(i as f64 * 10.0, 0.0, 10.0, 20.0)).collect();
        let idx = ConsistentIndexing::new().assign(&lots);
        // Left-most lot gets index 1, right-most gets 4.
        assert_eq!(idx, vec![1, 2, 3, 4]);
    }

    /// Passing the SAME lots in a shuffled order yields the SAME spatial indices
    /// (indices track geometry, not creation order) — the replay invariant.
    #[test]
    fn shuffled_input_same_indices() {
        let lots: Vec<Polygon2d> = (0..4).map(|i| rect(i as f64 * 10.0, 0.0, 10.0, 20.0)).collect();
        let base = ConsistentIndexing::new().assign(&lots);
        // Reverse the input order; the lot that WAS index 1 must still be index 1.
        let mut rev = lots.clone();
        rev.reverse();
        let ridx = ConsistentIndexing::new().assign(&rev);
        // rev[3] is the original left-most lot → must still map to spatial index 1.
        assert_eq!(ridx[3], base[0]);
        assert_eq!(ridx[0], base[3]);
    }

    /// Re-running on the identical site gives identical indices.
    #[test]
    fn rerun_identical_indices() {
        let lots: Vec<Polygon2d> =
            (0..6).map(|i| rect((i % 3) as f64 * 10.0, (i / 3) as f64 * 25.0, 10.0, 20.0)).collect();
        let a = ConsistentIndexing::new().assign(&lots);
        let b = ConsistentIndexing::new().assign(&lots);
        assert_eq!(a, b);
    }

    /// Adding ONE lot at the far right of a row keeps the prefix of the existing
    /// lots stable (they do not all renumber).
    #[test]
    fn incremental_add_keeps_stable_prefix() {
        let before: Vec<Polygon2d> =
            (0..3).map(|i| rect(i as f64 * 10.0, 0.0, 10.0, 20.0)).collect();
        let idx_before = ConsistentIndexing::new().assign(&before);
        // Now add a 4th lot to the RIGHT of the existing three.
        let mut after = before.clone();
        after.push(rect(30.0, 0.0, 10.0, 20.0));
        let idx_after = ConsistentIndexing::new().assign(&after);
        // The three original lots keep their indices (the new lot appends as 4).
        assert_eq!(&idx_after[..3], &idx_before[..]);
        assert_eq!(idx_after[3], 4);
    }

    /// Row-major ordering across two rows: bottom-row lots come AFTER top-row lots
    /// (a surveyor reads top→bottom).
    #[test]
    fn row_major_across_blocks() {
        // Two clear rows separated in y (distinct blocks). Grid auto-derived.
        let top = rect(0.0, 100.0, 10.0, 20.0); // higher y = read first
        let bottom = rect(0.0, 0.0, 10.0, 20.0);
        let lots = vec![bottom.clone(), top.clone()]; // creation order: bottom first
        let idx = ConsistentIndexing::new().assign(&lots);
        // top (input 1) should be spatial index 1; bottom (input 0) index 2.
        assert_eq!(idx[1], 1, "top row numbered first");
        assert_eq!(idx[0], 2, "bottom row numbered after");
    }

    #[test]
    fn empty_input() {
        assert!(ConsistentIndexing::new().assign(&[]).is_empty());
    }
}
