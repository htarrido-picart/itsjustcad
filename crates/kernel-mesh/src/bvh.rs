// SPDX-License-Identifier: AGPL-3.0-only
// Copyright © 2026 Hector Tarrido-Picart

//! Bounding-volume hierarchy: a median-split AABB tree for accelerating ray
//! and proximity queries over collections of primitives. Zero dependencies
//! beyond `glam`; pure f64 document space.
//!
//! Two entry points share the same tree:
//! - [`Bvh`] indexes arbitrary items by their AABBs (objects, for pick/osnap
//!   culling). Queries return item indices whose boxes the ray/box touches.
//! - [`TriBvh`] wraps a triangle soup and answers ray-occlusion directly
//!   (used by solar sun-hours), so callers never re-derive triangle math.

use glam::DVec3;

use crate::Aabb;

/// A node in the median-split tree. Leaves hold a contiguous slice of the
/// reordered primitive index list; internal nodes hold two child node indices.
#[derive(Clone, Debug, PartialEq)]
enum Node {
    Leaf { start: usize, count: usize },
    Internal { bounds: Aabb, left: u32, right: u32 },
}

/// AABB tree over a set of primitives, referenced by index into the original
/// slice the caller passed to [`Bvh::build`].
#[derive(Clone, Debug)]
pub struct Bvh {
    nodes: Vec<Node>,
    /// Leaf bounds, one per node (only meaningful for leaves; internal nodes
    /// carry their own bounds inline). Indexed by node index.
    leaf_bounds: Vec<Aabb>,
    /// Primitive indices, reordered so each leaf owns a contiguous range.
    prims: Vec<u32>,
    /// Root node index, or `usize::MAX` when the tree is empty.
    root: usize,
}

/// Max primitives per leaf. Small leaves keep traversal shallow; too small and
/// the node overhead dominates. 4 is a good default for scattered soups.
const LEAF_SIZE: usize = 4;

impl Bvh {
    /// Build a tree over `boxes[i]` = AABB of primitive `i`. Empty input yields
    /// an empty tree whose queries return nothing.
    pub fn build(boxes: &[Aabb]) -> Self {
        let mut prims: Vec<u32> = (0..boxes.len() as u32).collect();
        if boxes.is_empty() {
            return Self { nodes: Vec::new(), leaf_bounds: Vec::new(), prims, root: usize::MAX };
        }
        let (nodes, leaf_bounds) = build_subtree_par(boxes, &mut prims, 0);
        Self { nodes, leaf_bounds, prims, root: 0 }
    }

    /// Single-threaded build: the reference the parallel [`Bvh::build`] must
    /// reproduce node for node.
    #[cfg(test)]
    fn build_serial(boxes: &[Aabb]) -> Self {
        let mut prims: Vec<u32> = (0..boxes.len() as u32).collect();
        let mut nodes = Vec::new();
        let mut leaf_bounds = Vec::new();
        let root = if boxes.is_empty() {
            usize::MAX
        } else {
            build_node(boxes, &mut prims, 0, &mut nodes, &mut leaf_bounds)
        };
        Self { nodes, leaf_bounds, prims, root }
    }

    pub fn is_empty(&self) -> bool {
        self.root == usize::MAX
    }

    fn node_bounds(&self, node: usize) -> Aabb {
        match &self.nodes[node] {
            Node::Leaf { .. } => self.leaf_bounds[node],
            Node::Internal { bounds, .. } => *bounds,
        }
    }

    /// Indices of every primitive whose AABB the ray `origin + t*dir` (t ≥ 0)
    /// might intersect. Conservative: culling is by AABB only, so callers still
    /// do the exact per-primitive test. Order is unspecified.
    pub fn ray_candidates(&self, origin: DVec3, dir: DVec3) -> Vec<u32> {
        let mut out = Vec::new();
        if self.is_empty() {
            return out;
        }
        let inv = dir.recip();
        let mut stack = vec![self.root];
        while let Some(n) = stack.pop() {
            if ray_aabb(origin, inv, self.node_bounds(n)).is_none() {
                continue;
            }
            match &self.nodes[n] {
                Node::Leaf { start, count } => {
                    out.extend_from_slice(&self.prims[*start..*start + *count]);
                }
                Node::Internal { left, right, .. } => {
                    stack.push(*left as usize);
                    stack.push(*right as usize);
                }
            }
        }
        out
    }

    /// Does `test` accept any primitive whose AABB the ray `origin + t*dir`
    /// meets at some `t` in `[0, max_t]`? `test(i)` is the caller's exact test
    /// for primitive `i` (including its own `t` limits); traversal stops at the
    /// first `true`. Allocation-free for trees up to 64 deep.
    pub fn ray_any_hit(
        &self,
        origin: DVec3,
        dir: DVec3,
        max_t: f64,
        mut test: impl FnMut(u32) -> bool,
    ) -> bool {
        let mut hit = false;
        self.traverse(origin, dir, max_t, |prims| {
            if prims.iter().any(|&i| test(i)) {
                hit = true;
                return None;
            }
            Some(max_t)
        });
        hit
    }

    /// Closest primitive along the ray `origin + t*dir`: `test(i)` returns the
    /// exact hit `t` of primitive `i`, or `None` on a miss. Returns `(i, t)` of
    /// the smallest `t ≤ max_t`; on exact ties the lowest primitive index wins,
    /// so the result depends only on the primitives, never on tree layout or
    /// traversal order. Boxes entered strictly beyond the best hit so far are
    /// skipped; a box entered exactly at it is still visited, since it may
    /// hold a lower-index tie. Allocation-free for trees up to 64 deep.
    pub fn ray_closest_hit(
        &self,
        origin: DVec3,
        dir: DVec3,
        max_t: f64,
        mut test: impl FnMut(u32) -> Option<f64>,
    ) -> Option<(u32, f64)> {
        let mut best: Option<(u32, f64)> = None;
        self.traverse(origin, dir, max_t, |prims| {
            for &i in prims {
                if let Some(t) = test(i)
                    && best.map_or(t <= max_t, |(bi, bt)| t < bt || (t == bt && i < bi))
                {
                    best = Some((i, t));
                }
            }
            Some(best.map_or(max_t, |(_, bt)| bt))
        });
        best
    }

    /// Depth-first ray traversal in [`Bvh::ray_candidates`] order. `leaf` gets
    /// each reached leaf's primitives and returns the new pruning bound, or
    /// `None` to stop. Boxes whose entry `t` is strictly greater than the bound
    /// (initially `max_t`) are skipped.
    fn traverse(
        &self,
        origin: DVec3,
        dir: DVec3,
        max_t: f64,
        mut leaf: impl FnMut(&[u32]) -> Option<f64>,
    ) {
        if self.is_empty() {
            return;
        }
        let inv = dir.recip();
        let mut bound = max_t;
        let mut stack = NodeStack::new(self.root as u32);
        while let Some(n) = stack.pop() {
            let n = n as usize;
            match ray_aabb(origin, inv, self.node_bounds(n)) {
                Some(entry) if entry > bound => continue,
                None => continue,
                Some(_) => {}
            }
            match &self.nodes[n] {
                Node::Leaf { start, count } => match leaf(&self.prims[*start..*start + *count]) {
                    Some(b) => bound = b,
                    None => return,
                },
                Node::Internal { left, right, .. } => {
                    stack.push(*left);
                    stack.push(*right);
                }
            }
        }
    }

    /// Indices of every primitive whose AABB overlaps the query box. Order is
    /// unspecified. Used to cull osnap/box-select candidates to a screen region
    /// once the caller has mapped that region back to a world AABB.
    pub fn box_candidates(&self, query: Aabb) -> Vec<u32> {
        let mut out = Vec::new();
        if self.is_empty() {
            return out;
        }
        let mut stack = vec![self.root];
        while let Some(n) = stack.pop() {
            if !aabb_overlap(self.node_bounds(n), query) {
                continue;
            }
            match &self.nodes[n] {
                Node::Leaf { start, count } => {
                    out.extend_from_slice(&self.prims[*start..*start + *count]);
                }
                Node::Internal { left, right, .. } => {
                    stack.push(*left as usize);
                    stack.push(*right as usize);
                }
            }
        }
        out
    }
}

/// Ranges at least this large build their two subtrees in parallel; smaller
/// ones recurse serially. Big enough that task overhead stays negligible.
const PAR_THRESHOLD: usize = 4096;

/// Bounds of `prims` and, unless it becomes a leaf, the median split position
/// within it (after partitioning `prims` in place by centroid on the widest
/// axis). Shared by the serial and parallel builders so both make the exact
/// same decisions.
fn partition(boxes: &[Aabb], prims: &mut [u32]) -> (Aabb, Option<usize>) {
    let bounds = prims
        .iter()
        .map(|&i| boxes[i as usize])
        .reduce(Aabb::union)
        .expect("non-empty range");
    let count = prims.len();
    if count <= LEAF_SIZE {
        return (bounds, None);
    }

    // Split on the axis with the largest centroid spread.
    let centroid = |i: u32| boxes[i as usize].center();
    let mut cmin = DVec3::splat(f64::INFINITY);
    let mut cmax = DVec3::splat(f64::NEG_INFINITY);
    for &i in prims.iter() {
        let c = centroid(i);
        cmin = cmin.min(c);
        cmax = cmax.max(c);
    }
    let extent = cmax - cmin;
    let axis = if extent.x >= extent.y && extent.x >= extent.z {
        0
    } else if extent.y >= extent.z {
        1
    } else {
        2
    };
    if extent[axis] <= 0.0 {
        // All centroids coincide on every axis: splitting cannot separate
        // them, so stop here to avoid infinite recursion on duplicates.
        return (bounds, None);
    }

    // Median split: partially sort so the median centroid sits at count/2.
    // `select_nth_unstable_by` is deterministic for a given slice, so the
    // parallel builder reproduces the serial partition exactly.
    prims.select_nth_unstable_by(count / 2, |&a, &b| {
        centroid(a)[axis]
            .partial_cmp(&centroid(b)[axis])
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    (bounds, Some(count / 2))
}

/// Serially build the subtree over `prims` (which sits at `base` in the full
/// primitive list) into `nodes`, pre-order: node, left subtree, right subtree.
/// Returns the created node's index.
fn build_node(
    boxes: &[Aabb],
    prims: &mut [u32],
    base: usize,
    nodes: &mut Vec<Node>,
    leaf_bounds: &mut Vec<Aabb>,
) -> usize {
    let count = prims.len();
    let (bounds, split) = partition(boxes, prims);
    // Reserve this node's slot before recursing so children get later indices;
    // it stays a leaf unless split.
    let idx = nodes.len();
    nodes.push(Node::Leaf { start: base, count });
    leaf_bounds.push(bounds);
    if let Some(mid) = split {
        let (lp, rp) = prims.split_at_mut(mid);
        let left = build_node(boxes, lp, base, nodes, leaf_bounds);
        let right = build_node(boxes, rp, base + mid, nodes, leaf_bounds);
        nodes[idx] = Node::Internal { bounds, left: left as u32, right: right as u32 };
    }
    idx
}

/// A subtree built on its own, with node indices local to its arrays (root 0).
type Subtree = (Vec<Node>, Vec<Aabb>);

/// Parallel counterpart of [`build_node`]: same pre-order layout, so the
/// result is identical to the serial build. Large ranges build both children
/// concurrently into their own arrays, then append them (left, then right)
/// behind the parent with child indices rebased.
fn build_subtree_par(boxes: &[Aabb], prims: &mut [u32], base: usize) -> Subtree {
    let mut nodes = Vec::new();
    let mut leaf_bounds = Vec::new();
    if prims.len() < PAR_THRESHOLD {
        build_node(boxes, prims, base, &mut nodes, &mut leaf_bounds);
        return (nodes, leaf_bounds);
    }
    let count = prims.len();
    let (bounds, split) = partition(boxes, prims);
    nodes.push(Node::Leaf { start: base, count });
    leaf_bounds.push(bounds);
    if let Some(mid) = split {
        let (lp, rp) = prims.split_at_mut(mid);
        let (l, r) = rayon::join(
            || build_subtree_par(boxes, lp, base),
            || build_subtree_par(boxes, rp, base + mid),
        );
        let left = append_subtree(&mut nodes, &mut leaf_bounds, l);
        let right = append_subtree(&mut nodes, &mut leaf_bounds, r);
        nodes[0] = Node::Internal { bounds, left: left as u32, right: right as u32 };
    }
    (nodes, leaf_bounds)
}

/// Append `sub` to `nodes`, rebasing its internal child indices; returns the
/// index its root landed at.
fn append_subtree(nodes: &mut Vec<Node>, leaf_bounds: &mut Vec<Aabb>, sub: Subtree) -> usize {
    let offset = nodes.len();
    let shift = offset as u32;
    nodes.extend(sub.0.into_iter().map(|n| match n {
        Node::Internal { bounds, left, right } => {
            Node::Internal { bounds, left: left + shift, right: right + shift }
        }
        leaf => leaf,
    }));
    leaf_bounds.extend(sub.1);
    offset
}

/// Slab test with a precomputed reciprocal direction. Returns the near hit
/// distance (clamped to 0) when the ray meets the box, else `None`.
fn ray_aabb(origin: DVec3, inv_dir: DVec3, b: Aabb) -> Option<f64> {
    let t1 = (b.min - origin) * inv_dir;
    let t2 = (b.max - origin) * inv_dir;
    let t_min = t1.min(t2).max_element();
    let t_max = t1.max(t2).min_element();
    (t_max >= t_min.max(0.0)).then_some(t_min.max(0.0))
}

/// Traversal stack: 64 inline slots (deeper than any median-split tree over
/// realistic inputs) with a heap spill that keeps LIFO order if exceeded.
struct NodeStack {
    inline: [u32; 64],
    len: usize,
    spill: Vec<u32>,
}

impl NodeStack {
    fn new(root: u32) -> Self {
        let mut s = Self { inline: [0; 64], len: 0, spill: Vec::new() };
        s.push(root);
        s
    }

    fn push(&mut self, n: u32) {
        if self.len < self.inline.len() {
            self.inline[self.len] = n;
            self.len += 1;
        } else {
            self.spill.push(n);
        }
    }

    /// Spilled entries were pushed while the inline slots were full, so they
    /// are always on top.
    fn pop(&mut self) -> Option<u32> {
        self.spill.pop().or_else(|| {
            self.len = self.len.checked_sub(1)?;
            Some(self.inline[self.len])
        })
    }
}

fn aabb_overlap(a: Aabb, b: Aabb) -> bool {
    a.min.x <= b.max.x
        && a.max.x >= b.min.x
        && a.min.y <= b.max.y
        && a.max.y >= b.min.y
        && a.min.z <= b.max.z
        && a.max.z >= b.min.z
}

/// A triangle soup with an AABB tree, answering ray-occlusion queries directly.
/// Replaces brute-force `for tri in tris` loops (e.g. solar sun-hours).
#[derive(Clone, Debug)]
pub struct TriBvh {
    tris: Vec<[DVec3; 3]>,
    bvh: Bvh,
}

impl TriBvh {
    pub fn build(tris: Vec<[DVec3; 3]>) -> Self {
        use rayon::prelude::*;
        let boxes: Vec<Aabb> = tris
            .par_iter()
            .map(|t| Aabb::from_points(t.iter().copied()))
            .collect();
        let bvh = Bvh::build(&boxes);
        Self { tris, bvh }
    }

    pub fn is_empty(&self) -> bool {
        self.tris.is_empty()
    }

    pub fn triangle_count(&self) -> usize {
        self.tris.len()
    }

    /// Does any triangle block the ray `origin + t*dir`, `t > eps`? `dir` need
    /// not be normalized. Used for sun-occlusion: true = a triangle shadows the
    /// point. Only culled candidate triangles get the exact Möller–Trumbore test.
    pub fn ray_occluded(&self, origin: DVec3, dir: DVec3) -> bool {
        self.bvh.ray_any_hit(origin, dir, f64::INFINITY, |i| {
            let [a, b, c] = self.tris[i as usize];
            ray_triangle(origin, dir, a, b, c).is_some()
        })
    }

    /// Nearest ray/triangle hit distance `t > eps` along `dir`, or `None`.
    pub fn ray_hit(&self, origin: DVec3, dir: DVec3) -> Option<f64> {
        self.bvh
            .ray_closest_hit(origin, dir, f64::INFINITY, |i| {
                let [a, b, c] = self.tris[i as usize];
                ray_triangle(origin, dir, a, b, c)
            })
            .map(|(_, t)| t)
    }
}

/// Möller–Trumbore ray/triangle intersection in f64 doc space. Returns the ray
/// parameter `t > eps` at the hit (distance along `dir`), or `None` on a miss.
/// `dir` need not be normalized; `t` is in units of `dir`'s length.
pub fn ray_triangle(origin: DVec3, dir: DVec3, v0: DVec3, v1: DVec3, v2: DVec3) -> Option<f64> {
    const EPS: f64 = 1e-9;
    let e1 = v1 - v0;
    let e2 = v2 - v0;
    let pvec = dir.cross(e2);
    let det = e1.dot(pvec);
    if det.abs() < EPS {
        return None; // ray parallel to triangle
    }
    let inv_det = 1.0 / det;
    let tvec = origin - v0;
    let u = tvec.dot(pvec) * inv_det;
    if !(-EPS..=1.0 + EPS).contains(&u) {
        return None;
    }
    let qvec = tvec.cross(e1);
    let v = dir.dot(qvec) * inv_det;
    if v < -EPS || u + v > 1.0 + EPS {
        return None;
    }
    let t = e2.dot(qvec) * inv_det;
    (t > EPS).then_some(t)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Cheap xorshift RNG so tests are deterministic and dependency-free.
    struct Rng(u64);
    impl Rng {
        fn next_u64(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.0 = x;
            x
        }
        fn f(&mut self, lo: f64, hi: f64) -> f64 {
            let u = (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64;
            lo + u * (hi - lo)
        }
        fn v(&mut self, lo: f64, hi: f64) -> DVec3 {
            DVec3::new(self.f(lo, hi), self.f(lo, hi), self.f(lo, hi))
        }
    }

    fn random_tris(rng: &mut Rng, n: usize) -> Vec<[DVec3; 3]> {
        (0..n)
            .map(|_| {
                let base = rng.v(-50.0, 50.0);
                [
                    base,
                    base + rng.v(-3.0, 3.0),
                    base + rng.v(-3.0, 3.0),
                ]
            })
            .collect()
    }

    fn brute_hit(tris: &[[DVec3; 3]], o: DVec3, d: DVec3) -> Option<f64> {
        let mut best: Option<f64> = None;
        for t in tris {
            if let Some(h) = ray_triangle(o, d, t[0], t[1], t[2]) {
                best = Some(best.map_or(h, |b: f64| b.min(h)));
            }
        }
        best
    }

    #[test]
    fn bvh_ray_hits_match_brute_force() {
        let mut rng = Rng(0x1234_5678_9abc_def0);
        let tris = random_tris(&mut rng, 400);
        let bvh = TriBvh::build(tris.clone());
        for _ in 0..500 {
            let o = rng.v(-60.0, 60.0);
            let d = rng.v(-1.0, 1.0);
            if d.length_squared() < 1e-6 {
                continue;
            }
            let brute = brute_hit(&tris, o, d);
            let fast = bvh.ray_hit(o, d);
            match (brute, fast) {
                (None, None) => {}
                (Some(b), Some(f)) => {
                    assert!((b - f).abs() < 1e-6, "t mismatch brute={b} fast={f}");
                }
                other => panic!("hit disagreement: {other:?} o={o} d={d}"),
            }
        }
    }

    #[test]
    fn bvh_occlusion_matches_brute_force() {
        let mut rng = Rng(0xdead_beef_cafe_0042);
        let tris = random_tris(&mut rng, 300);
        let bvh = TriBvh::build(tris.clone());
        for _ in 0..500 {
            let o = rng.v(-60.0, 60.0);
            let d = rng.v(-1.0, 1.0);
            if d.length_squared() < 1e-6 {
                continue;
            }
            let brute = brute_hit(&tris, o, d).is_some();
            assert_eq!(brute, bvh.ray_occluded(o, d), "occlusion mismatch o={o} d={d}");
        }
    }

    #[test]
    fn box_candidates_are_a_conservative_superset() {
        let mut rng = Rng(0x0bad_f00d_1337_9999);
        let boxes: Vec<Aabb> = (0..200)
            .map(|_| {
                let a = rng.v(-40.0, 40.0);
                let b = a + rng.v(0.1, 5.0);
                Aabb::from_points([a, b])
            })
            .collect();
        let bvh = Bvh::build(&boxes);
        for _ in 0..200 {
            let a = rng.v(-40.0, 40.0);
            let q = Aabb::from_points([a, a + rng.v(0.5, 10.0)]);
            let got: std::collections::BTreeSet<u32> =
                bvh.box_candidates(q).into_iter().collect();
            // Every truly-overlapping box must be reported.
            for (i, b) in boxes.iter().enumerate() {
                if aabb_overlap(*b, q) {
                    assert!(got.contains(&(i as u32)), "missed overlapping box {i}");
                }
            }
            // Culling is by leaf box, so a leaf's siblings may ride along;
            // `box_candidates` is a conservative superset, not exact. We only
            // require it never omits a genuine overlap (checked above).
        }
    }

    #[test]
    fn empty_tree_returns_nothing() {
        let bvh = Bvh::build(&[]);
        assert!(bvh.is_empty());
        assert!(bvh.ray_candidates(DVec3::ZERO, DVec3::X).is_empty());
        assert!(bvh.box_candidates(Aabb::from_points([DVec3::ZERO, DVec3::ONE])).is_empty());

        let tb = TriBvh::build(vec![]);
        assert!(tb.is_empty());
        assert!(!tb.ray_occluded(DVec3::ZERO, DVec3::Z));
        assert!(tb.ray_hit(DVec3::ZERO, DVec3::Z).is_none());
    }

    fn assert_same_tree(a: &Bvh, b: &Bvh) {
        assert_eq!(a.root, b.root);
        assert_eq!(a.prims, b.prims);
        assert_eq!(a.nodes, b.nodes);
        assert_eq!(a.leaf_bounds.len(), b.leaf_bounds.len());
        for (x, y) in a.leaf_bounds.iter().zip(&b.leaf_bounds) {
            // Bitwise, so -0.0 vs 0.0 or NaN payloads would also be caught.
            assert_eq!(x.min.to_array().map(f64::to_bits), y.min.to_array().map(f64::to_bits));
            assert_eq!(x.max.to_array().map(f64::to_bits), y.max.to_array().map(f64::to_bits));
        }
    }

    fn tri_boxes(tris: &[[DVec3; 3]]) -> Vec<Aabb> {
        tris.iter().map(|t| Aabb::from_points(t.iter().copied())).collect()
    }

    #[test]
    fn parallel_build_matches_serial_oracle_on_random_soups() {
        let mut rng = Rng(0xfeed_beef_0bad_cafe);
        // Spans below, at and far above the parallel threshold.
        for n in [0, 1, 5, PAR_THRESHOLD - 1, PAR_THRESHOLD, PAR_THRESHOLD + 1, 50_000, 200_003] {
            let boxes = tri_boxes(&random_tris(&mut rng, n));
            assert_same_tree(&Bvh::build(&boxes), &Bvh::build_serial(&boxes));
        }
    }

    #[test]
    fn parallel_build_matches_serial_oracle_with_ties_and_duplicates() {
        // A flat grid (many equal centroids per axis, zero z extent) plus
        // coincident duplicates: stresses the median-select tie handling and
        // the degenerate-extent leaf.
        let mut boxes = Vec::new();
        for i in 0..300 {
            for j in 0..300 {
                let p = DVec3::new((i % 37) as f64, j as f64, 0.0);
                boxes.push(Aabb::from_points([p, p + DVec3::new(1.0, 1.0, 0.0)]));
            }
        }
        boxes.extend(std::iter::repeat_n(boxes[17], 20_000));
        assert_same_tree(&Bvh::build(&boxes), &Bvh::build_serial(&boxes));

        let tris = random_tris(&mut Rng(7), 30_000);
        let tb = TriBvh::build(tris.clone());
        assert_same_tree(&tb.bvh, &Bvh::build_serial(&tri_boxes(&tris)));
    }

    /// Brute force over [`Bvh::ray_candidates`]: smallest `t ≤ max_t`, exact
    /// ties to the lowest primitive index. `ray_closest_hit` must reproduce it
    /// bit for bit.
    fn oracle_closest(
        bvh: &Bvh,
        o: DVec3,
        d: DVec3,
        max_t: f64,
        mut test: impl FnMut(u32) -> Option<f64>,
    ) -> Option<(u32, f64)> {
        bvh.ray_candidates(o, d)
            .into_iter()
            .filter_map(|i| test(i).filter(|&t| t <= max_t).map(|t| (i, t)))
            .min_by(|a, b| a.1.total_cmp(&b.1).then(a.0.cmp(&b.0)))
    }

    /// The pre-#310 rule: exact ties to the first candidate in traversal order.
    fn first_in_order_closest(
        bvh: &Bvh,
        o: DVec3,
        d: DVec3,
        max_t: f64,
        mut test: impl FnMut(u32) -> Option<f64>,
    ) -> Option<(u32, f64)> {
        let mut best: Option<(u32, f64)> = None;
        for i in bvh.ray_candidates(o, d) {
            if let Some(t) = test(i)
                && best.map_or(t <= max_t, |(_, bt)| t < bt)
            {
                best = Some((i, t));
            }
        }
        best
    }

    /// The same tree with every internal node's children swapped at random and
    /// every leaf's primitives rotated: a different traversal order over
    /// identical boxes.
    fn shuffled(bvh: &Bvh, rng: &mut Rng) -> Bvh {
        let mut out = bvh.clone();
        for node in &mut out.nodes {
            match node {
                Node::Internal { left, right, .. } => {
                    if rng.next_u64().is_multiple_of(2) {
                        std::mem::swap(left, right);
                    }
                }
                Node::Leaf { start, count } => {
                    let k = (rng.next_u64() % *count as u64) as usize;
                    out.prims[*start..*start + *count].rotate_left(k);
                }
            }
        }
        out
    }

    fn bits(h: Option<(u32, f64)>) -> Option<(u32, u64)> {
        h.map(|(i, t)| (i, t.to_bits()))
    }

    /// Closest and any hit against the oracle for one ray at several `max_t`,
    /// including exactly the oracle's hit `t` and its neighbouring floats.
    fn check_ray(tb: &TriBvh, o: DVec3, d: DVec3) {
        let test = |i: u32| {
            let [a, b, c] = tb.tris[i as usize];
            ray_triangle(o, d, a, b, c)
        };
        let full = oracle_closest(&tb.bvh, o, d, f64::INFINITY, test);
        let mut limits = vec![f64::INFINITY, 0.0, 1.0, 40.0];
        if let Some((_, t)) = full {
            limits.extend([t, t.next_down(), t.next_up()]);
        }
        for max_t in limits {
            let got = tb.bvh.ray_closest_hit(o, d, max_t, test);
            assert_eq!(bits(got), bits(oracle_closest(&tb.bvh, o, d, max_t, test)), "closest o={o} d={d} max_t={max_t}");
            // Against the pre-#310 rule: same `t` always, same winner unless
            // another primitive hits at exactly that `t`.
            let old = first_in_order_closest(&tb.bvh, o, d, max_t, test);
            assert_eq!(got.map(|h| h.1.to_bits()), old.map(|h| h.1.to_bits()));
            if let Some((_, t)) = got {
                let at_t = tb.bvh.ray_candidates(o, d).into_iter().filter(|&i| test(i) == Some(t)).count();
                if at_t == 1 {
                    assert_eq!(got, old, "non-tie winner changed o={o} d={d} max_t={max_t}");
                }
            }
            let shadow = |i| test(i).is_some_and(|t| t < max_t);
            let oracle_any = tb.bvh.ray_candidates(o, d).into_iter().any(shadow);
            assert_eq!(tb.bvh.ray_any_hit(o, d, max_t, shadow), oracle_any, "any o={o} d={d} max_t={max_t}");
        }
        assert_eq!(tb.ray_hit(o, d).map(f64::to_bits), full.map(|(_, t)| t.to_bits()));
        assert_eq!(tb.ray_occluded(o, d), full.is_some());
    }

    /// Axis-aligned rays and rays with zero components (infinite reciprocals).
    const PARALLEL_DIRS: [DVec3; 8] = [
        DVec3::X,
        DVec3::NEG_X,
        DVec3::Y,
        DVec3::NEG_Y,
        DVec3::Z,
        DVec3::NEG_Z,
        DVec3::new(1.0, 1.0, 0.0),
        DVec3::new(0.0, -1.0, 1.0),
    ];

    #[test]
    fn traversal_matches_oracle_on_random_soups() {
        let mut rng = Rng(0x5eed_0300_b0b0_0001);
        for n in [1, 3, 4, 5, 64, 2000] {
            let tb = TriBvh::build(random_tris(&mut rng, n));
            for _ in 0..400 {
                // Origins inside and outside the soup's boxes.
                let o = rng.v(-60.0, 60.0);
                let d = rng.v(-1.0, 1.0);
                if d.length_squared() > 1e-6 {
                    check_ray(&tb, o, d);
                }
                check_ray(&tb, o, PARALLEL_DIRS[(rng.next_u64() % 8) as usize]);
            }
        }
    }

    #[test]
    fn traversal_matches_oracle_with_exact_ties() {
        // Unit-square grid of coplanar triangles on several z levels, every
        // triangle duplicated: rays through shared edges/vertices and through
        // duplicates hit several triangles at the same t.
        let mut tris = Vec::new();
        for z in [0.0, 1.0, 1.0, 2.5] {
            for i in 0..12 {
                for j in 0..12 {
                    let p = DVec3::new(i as f64, j as f64, z);
                    let t0 = [p, p + DVec3::X, p + DVec3::Y];
                    let t1 = [p + DVec3::X, p + DVec3::new(1.0, 1.0, 0.0), p + DVec3::Y];
                    tris.extend([t0, t1, t0]);
                }
            }
        }
        let tb = TriBvh::build(tris);
        let mut rng = Rng(0x7135_0300_0000_0002);
        for _ in 0..3000 {
            // Integer/half-integer origins hit edges and vertices exactly.
            let o = DVec3::new(
                (rng.next_u64() % 26) as f64 * 0.5,
                (rng.next_u64() % 26) as f64 * 0.5,
                [-1.0, 0.0, 0.5, 1.0, 3.0][(rng.next_u64() % 5) as usize],
            );
            let d = match rng.next_u64() % 3 {
                0 => PARALLEL_DIRS[(rng.next_u64() % 8) as usize],
                1 => DVec3::new(0.0, 0.0, if o.z > 1.0 { -1.0 } else { 1.0 }),
                _ => rng.v(-1.0, 1.0),
            };
            if d.length_squared() > 1e-6 {
                check_ray(&tb, o, d);
            }
        }
    }

    /// Exact-tie scenes: coplanar grids duplicated in place (the same triangle
    /// at several indices), rays down shared edges and through shared
    /// vertices. Returns the soup and rays that hit it.
    fn tie_scene(rng: &mut Rng) -> (Vec<[DVec3; 3]>, Vec<(DVec3, DVec3)>) {
        let mut tris = Vec::new();
        for z in [0.0, 1.0, 1.0, 2.5] {
            for i in 0..12 {
                for j in 0..12 {
                    let p = DVec3::new(i as f64, j as f64, z);
                    let t0 = [p, p + DVec3::X, p + DVec3::Y];
                    let t1 = [p + DVec3::X, p + DVec3::new(1.0, 1.0, 0.0), p + DVec3::Y];
                    tris.extend([t0, t1, t0, t1]);
                }
            }
        }
        let mut rays = Vec::new();
        for _ in 0..3000 {
            // Grid vertices (x.0, y.0), edge midpoints and the shared diagonal
            // (x.5, y.5), cast straight down/up or obliquely.
            let o = DVec3::new(
                (rng.next_u64() % 26) as f64 * 0.5,
                (rng.next_u64() % 26) as f64 * 0.5,
                [-1.0, 0.5, 1.5, 3.0][(rng.next_u64() % 4) as usize],
            );
            let d = match rng.next_u64() % 3 {
                0 => DVec3::new(0.0, 0.0, if o.z > 1.0 { -1.0 } else { 1.0 }),
                1 => DVec3::new(0.25, 0.5, if o.z > 1.0 { -1.0 } else { 1.0 }),
                _ => rng.v(-1.0, 1.0),
            };
            if d.length_squared() > 1e-6 {
                rays.push((o, d));
            }
        }
        (tris, rays)
    }

    #[test]
    fn closest_hit_breaks_exact_ties_by_lowest_index() {
        let (tris, rays) = tie_scene(&mut Rng(0x7135_0310_0000_0001));
        let tb = TriBvh::build(tris.clone());
        let mut ties = 0;
        for (o, d) in rays {
            let test = |i: u32| {
                let [a, b, c] = tris[i as usize];
                ray_triangle(o, d, a, b, c)
            };
            let Some((i, t)) = tb.bvh.ray_closest_hit(o, d, f64::INFINITY, test) else { continue };
            let at_t: Vec<u32> = (0..tris.len() as u32).filter(|&k| test(k) == Some(t)).collect();
            assert_eq!(at_t.first(), Some(&i), "o={o} d={d}");
            assert!((0..tris.len() as u32).all(|k| test(k).is_none_or(|u| u >= t)));
            ties += usize::from(at_t.len() > 1);
        }
        assert!(ties > 1000, "only {ties} tie rays");
    }

    #[test]
    fn closest_hit_is_independent_of_tree_layout() {
        let mut rng = Rng(0x7135_0310_0000_0002);
        let (tie_tris, tie_rays) = tie_scene(&mut rng);
        let soup = random_tris(&mut rng, 2000);
        let soup_rays: Vec<_> = (0..2000).map(|_| (rng.v(-60.0, 60.0), rng.v(-1.0, 1.0))).collect();
        for (tris, rays) in [(tie_tris, tie_rays), (soup, soup_rays)] {
            let tb = TriBvh::build(tris.clone());
            let layouts: Vec<Bvh> = (0..4).map(|_| shuffled(&tb.bvh, &mut rng)).collect();
            assert!(layouts.iter().any(|b| b.prims != tb.bvh.prims || b.nodes != tb.bvh.nodes));
            // A permuted input builds a different tree; reversing its result
            // index must land on the same primitive.
            let mut perm: Vec<u32> = (0..tris.len() as u32).collect();
            for k in (1..perm.len()).rev() {
                perm.swap(k, (rng.next_u64() % (k as u64 + 1)) as usize);
            }
            let permuted = Bvh::build(&tri_boxes(&perm.iter().map(|&p| tris[p as usize]).collect::<Vec<_>>()));
            for (o, d) in rays {
                let test = |i: u32| {
                    let [a, b, c] = tris[i as usize];
                    ray_triangle(o, d, a, b, c)
                };
                let want = bits(tb.bvh.ray_closest_hit(o, d, f64::INFINITY, test));
                for b in &layouts {
                    assert_eq!(bits(b.ray_closest_hit(o, d, f64::INFINITY, test)), want, "o={o} d={d}");
                }
                // Permuted build, same primitive indices: rank by original index.
                let got = permuted
                    .ray_closest_hit(o, d, f64::INFINITY, |p| test(perm[p as usize]))
                    .map(|(p, t)| (perm[p as usize], t));
                let at_t = want.map(|(_, tb)| {
                    (0..tris.len() as u32).filter(|&k| test(k).map(f64::to_bits) == Some(tb)).count()
                });
                if at_t == Some(1) || want.is_none() {
                    assert_eq!(bits(got), want, "permuted o={o} d={d}");
                } else {
                    assert_eq!(got.map(|h| h.1.to_bits()), want.map(|h| h.1));
                }
            }
        }
    }

    #[test]
    fn node_stack_spills_to_heap_in_lifo_order() {
        let mut s = NodeStack::new(0);
        for n in 1..200 {
            s.push(n);
        }
        assert!(!s.spill.is_empty());
        let popped: Vec<u32> = std::iter::from_fn(|| s.pop()).collect();
        assert_eq!(popped, (0..200).rev().collect::<Vec<_>>());
        // Interleaved pushes/pops across the inline/heap boundary.
        let mut s = NodeStack::new(0);
        let mut model = vec![0];
        let mut rng = Rng(9);
        for k in 1..5000 {
            if rng.next_u64().is_multiple_of(3) {
                assert_eq!(s.pop(), model.pop());
            } else {
                s.push(k);
                model.push(k);
            }
        }
        while let Some(n) = model.pop() {
            assert_eq!(s.pop(), Some(n));
        }
        assert_eq!(s.pop(), None);
    }

    #[test]
    fn empty_tree_any_and_closest_hit_find_nothing() {
        let bvh = Bvh::build(&[]);
        assert!(!bvh.ray_any_hit(DVec3::ZERO, DVec3::X, f64::INFINITY, |_| true));
        assert!(bvh.ray_closest_hit(DVec3::ZERO, DVec3::X, f64::INFINITY, |_| Some(1.0)).is_none());
    }

    #[test]
    fn coincident_primitives_do_not_infinite_recurse() {
        // Many identical boxes → zero centroid extent → must fall back to a leaf.
        let b = Aabb::from_points([DVec3::ZERO, DVec3::ONE]);
        let boxes = vec![b; 32];
        let bvh = Bvh::build(&boxes);
        let hits = bvh.box_candidates(Aabb::from_points([DVec3::splat(0.5), DVec3::splat(0.6)]));
        assert_eq!(hits.len(), 32, "all coincident boxes overlap the query");
    }
}
