// SPDX-License-Identifier: AGPL-3.0-only
// Copyright © 2026 Hector Tarrido-Picart

//! Bounding-volume hierarchy: a binned-SAH AABB tree for accelerating ray
//! and proximity queries over collections of primitives. Zero dependencies
//! beyond `glam`; f64 document space, with `f32` copies of the inner boxes
//! used only to prune.
//!
//! Two entry points share the same tree:
//! - [`Bvh`] indexes arbitrary items by their AABBs (objects, for pick/osnap
//!   culling). Queries return item indices whose boxes the ray/box touches.
//! - [`TriBvh`] wraps a triangle soup and answers ray-occlusion directly
//!   (used by solar sun-hours), so callers never re-derive triangle math.
//!
//! The tree is built binary (binned SAH), then collapsed to 4-wide nodes.
//! Each wide node keeps its children's boxes as `f32` struct-of-arrays,
//! rounded outward, so one 4-lane SIMD pass tests all four. Those tests are
//! conservative: they may keep a box the exact test would reject, never the
//! reverse. Leaves keep their exact `f64` box, and every leaf is admitted by
//! the same `f64` slab test as before, so the leaves (and primitives) a
//! query reaches do not depend on the `f32` pruning.

use glam::DVec3;
use wide::f32x4;

use crate::Aabb;

/// A node of the binary build tree. Leaves hold a contiguous slice of the
/// reordered primitive index list; internal nodes hold two child node indices.
#[derive(Clone, Debug, PartialEq)]
enum BinNode {
    Leaf { start: usize, count: usize },
    Internal { bounds: Aabb, left: u32, right: u32 },
}

/// The binary SAH tree, before it is collapsed to 4-wide nodes.
struct BinTree {
    nodes: Vec<BinNode>,
    /// Leaf bounds, one per node (only meaningful for leaves; internal nodes
    /// carry their own bounds inline). Indexed by node index.
    leaf_bounds: Vec<Aabb>,
    prims: Vec<u32>,
}

impl BinTree {
    fn node_bounds(&self, node: usize) -> Aabb {
        match &self.nodes[node] {
            BinNode::Leaf { .. } => self.leaf_bounds[node],
            BinNode::Internal { bounds, .. } => *bounds,
        }
    }
}

/// Child slot tag: the low 31 bits index [`Bvh::leaves`] instead of
/// [`Bvh::nodes`].
const LEAF_BIT: u32 = 1 << 31;
/// An unused child slot.
const EMPTY: u32 = u32::MAX;

/// A 4-wide inner node: up to four children and their boxes, struct-of-arrays
/// (`min[axis][lane]`), stored as [`half_box`]: half the coordinates, in
/// `f32` rounded outward, so each box contains half the child's exact `f64`
/// box. Unused lanes hold [`EMPTY`].
#[derive(Clone, Debug)]
struct Node4 {
    min: [[f32; 4]; 3],
    max: [[f32; 4]; 3],
    child: [u32; 4],
}

/// A leaf: its contiguous range of [`Bvh::prims`] and its exact box.
#[derive(Clone, Debug, PartialEq)]
struct Leaf {
    start: u32,
    count: u32,
    bounds: Aabb,
}

/// AABB tree over a set of primitives, referenced by index into the original
/// slice the caller passed to [`Bvh::build`].
#[derive(Clone, Debug)]
pub struct Bvh {
    nodes: Vec<Node4>,
    leaves: Vec<Leaf>,
    /// Primitive indices, reordered so each leaf owns a contiguous range.
    prims: Vec<u32>,
    /// Root child slot ([`LEAF_BIT`]-tagged for a single-leaf tree), or
    /// [`EMPTY`] when the tree is empty.
    root: u32,
    /// Largest coordinate magnitude in the tree (∞ if unbounded).
    reach: f64,
    /// `prim_leaf[i]`: index into [`Bvh::leaves`] of the leaf holding
    /// primitive `i` (for [`Bvh::ray_reaches_prim`]).
    prim_leaf: Vec<u32>,
}

/// Max primitives per leaf. Small leaves keep traversal shallow; too small and
/// the node overhead dominates. 4 is a good default for scattered soups.
const LEAF_SIZE: usize = 4;

impl Bvh {
    /// Build a tree over `boxes[i]` = AABB of primitive `i`. Empty input yields
    /// an empty tree whose queries return nothing. The result is the same for
    /// any thread count.
    pub fn build(boxes: &[Aabb]) -> Self {
        Self::collapse(Self::build_binary(boxes))
    }

    /// Parallel binary SAH build, identical to [`Bvh::build_binary_serial`].
    fn build_binary(boxes: &[Aabb]) -> BinTree {
        let mut prims: Vec<u32> = (0..boxes.len() as u32).collect();
        if boxes.is_empty() {
            return BinTree { nodes: Vec::new(), leaf_bounds: Vec::new(), prims };
        }
        let (nodes, leaf_bounds) = build_subtree_par(boxes, &mut prims, 0);
        BinTree { nodes, leaf_bounds, prims }
    }

    /// Single-threaded binary build: the reference the parallel binary build
    /// must reproduce node for node.
    #[cfg(test)]
    fn build_binary_serial(boxes: &[Aabb]) -> BinTree {
        let mut prims: Vec<u32> = (0..boxes.len() as u32).collect();
        let mut nodes = Vec::new();
        let mut leaf_bounds = Vec::new();
        if !boxes.is_empty() {
            build_node(boxes, &mut prims, 0, &mut nodes, &mut leaf_bounds);
        }
        BinTree { nodes, leaf_bounds, prims }
    }

    /// Collapse a binary tree (root at node 0) to 4-wide nodes, pre-order.
    fn collapse(bin: BinTree) -> Self {
        let mut out =
            Self {
                nodes: Vec::new(),
                leaves: Vec::new(),
                prims: Vec::new(),
                root: EMPTY,
                reach: 0.0,
                prim_leaf: Vec::new(),
            };
        if !bin.nodes.is_empty() {
            let b = bin.node_bounds(0);
            out.reach = b.min.abs().max(b.max.abs()).max_element();
            out.root = out.collapse_node(&bin, 0);
        }
        out.prims = bin.prims;
        out.prim_leaf = vec![0; out.prims.len()];
        for (li, l) in out.leaves.iter().enumerate() {
            for &p in &out.prims[l.start as usize..(l.start + l.count) as usize] {
                out.prim_leaf[p as usize] = li as u32;
            }
        }
        out
    }

    /// Wide child slot for binary node `n`. An inner node takes the two
    /// binary children, then repeatedly opens the inner child with the
    /// largest surface area (first such on ties) until it has four children
    /// or only leaves remain. Children keep their left-to-right order.
    fn collapse_node(&mut self, bin: &BinTree, n: usize) -> u32 {
        let BinNode::Internal { left, right, .. } = bin.nodes[n] else {
            let BinNode::Leaf { start, count } = bin.nodes[n] else { unreachable!() };
            self.leaves.push(Leaf { start: start as u32, count: count as u32, bounds: bin.leaf_bounds[n] });
            return (self.leaves.len() - 1) as u32 | LEAF_BIT;
        };
        let mut kids = vec![left as usize, right as usize];
        while kids.len() < 4 {
            let mut open: Option<(usize, f64)> = None;
            for (j, &k) in kids.iter().enumerate() {
                if let BinNode::Internal { bounds, .. } = bin.nodes[k] {
                    let area = half_area(bounds);
                    if open.is_none_or(|(_, a)| area > a) {
                        open = Some((j, area));
                    }
                }
            }
            let Some((j, _)) = open else { break };
            let BinNode::Internal { left, right, .. } = bin.nodes[kids[j]] else { unreachable!() };
            kids.splice(j..=j, [left as usize, right as usize]);
        }
        let idx = self.nodes.len();
        self.nodes.push(Node4 {
            min: [[f32::INFINITY; 4]; 3],
            max: [[f32::NEG_INFINITY; 4]; 3],
            child: [EMPTY; 4],
        });
        for (lane, &k) in kids.iter().enumerate() {
            let b = bin.node_bounds(k);
            let slot = self.collapse_node(bin, k);
            let (min, max) = half_box(b);
            let node = &mut self.nodes[idx];
            node.child[lane] = slot;
            for a in 0..3 {
                node.min[a][lane] = min[a];
                node.max[a][lane] = max[a];
            }
        }
        idx as u32
    }

    pub fn is_empty(&self) -> bool {
        self.root == EMPTY
    }

    fn leaf(&self, slot: u32) -> &Leaf {
        &self.leaves[(slot & !LEAF_BIT) as usize]
    }

    fn leaf_prims(&self, l: &Leaf) -> &[u32] {
        &self.prims[l.start as usize..(l.start + l.count) as usize]
    }

    /// Indices of every primitive whose AABB the ray `origin + t*dir` (t ≥ 0)
    /// might intersect. Conservative: culling is by AABB only, so callers still
    /// do the exact per-primitive test. Order is unspecified.
    pub fn ray_candidates(&self, origin: DVec3, dir: DVec3) -> Vec<u32> {
        let mut out = Vec::new();
        self.traverse::<false>(origin, dir, f64::INFINITY, &mut (), |prims| {
            out.extend_from_slice(prims);
            Some(f64::INFINITY)
        });
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
        test: impl FnMut(u32) -> bool,
    ) -> bool {
        self.any_hit_tally(origin, dir, max_t, test, &mut ())
    }

    /// Does `ray_any_hit(origin, dir, f64::INFINITY, test)` reach primitive
    /// `prim`, i.e. would it call `test(prim)` if no earlier primitive were
    /// accepted? True exactly when `prim`'s leaf passes the same `f64` slab
    /// test the traversal admits leaves with: the `f32` inner-node pruning
    /// never drops a leaf that test admits (see the module docs). So
    /// `ray_reaches_prim(o, d, i) && test(i)` implies `ray_any_hit` is true.
    pub fn ray_reaches_prim(&self, origin: DVec3, dir: DVec3, prim: u32) -> bool {
        ray_aabb(origin, dir.recip(), self.prim_leaf_bounds(prim)).is_some_and(|t| t <= f64::INFINITY)
    }

    /// The exact box of the leaf holding primitive `prim`: the box
    /// [`Bvh::ray_reaches_prim`] tests.
    pub fn prim_leaf_bounds(&self, prim: u32) -> Aabb {
        self.leaves[self.prim_leaf[prim as usize] as usize].bounds
    }

    /// [`Bvh::ray_any_hit`] that also adds its traversal work to `stats`.
    /// For tests and benchmarks; the plain query compiles the counting out.
    #[doc(hidden)]
    pub fn ray_any_hit_counted(
        &self,
        origin: DVec3,
        dir: DVec3,
        max_t: f64,
        test: impl FnMut(u32) -> bool,
        stats: &mut TraversalStats,
    ) -> bool {
        self.any_hit_tally(origin, dir, max_t, test, stats)
    }

    fn any_hit_tally(
        &self,
        origin: DVec3,
        dir: DVec3,
        max_t: f64,
        mut test: impl FnMut(u32) -> bool,
        tally: &mut impl Tally,
    ) -> bool {
        let mut hit = false;
        self.traverse::<false>(origin, dir, max_t, tally, |prims| {
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
        test: impl FnMut(u32) -> Option<f64>,
    ) -> Option<(u32, f64)> {
        self.closest_hit_tally(origin, dir, max_t, test, &mut ())
    }

    /// [`Bvh::ray_closest_hit`] that also adds its traversal work to `stats`.
    /// For tests and benchmarks; the plain query compiles the counting out.
    #[doc(hidden)]
    pub fn ray_closest_hit_counted(
        &self,
        origin: DVec3,
        dir: DVec3,
        max_t: f64,
        test: impl FnMut(u32) -> Option<f64>,
        stats: &mut TraversalStats,
    ) -> Option<(u32, f64)> {
        self.closest_hit_tally(origin, dir, max_t, test, stats)
    }

    fn closest_hit_tally(
        &self,
        origin: DVec3,
        dir: DVec3,
        max_t: f64,
        mut test: impl FnMut(u32) -> Option<f64>,
        tally: &mut impl Tally,
    ) -> Option<(u32, f64)> {
        let mut best: Option<(u32, f64)> = None;
        self.traverse::<true>(origin, dir, max_t, tally, |prims| {
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

    /// Depth-first ray traversal. `leaf` gets each reached leaf's primitives
    /// and returns the new pruning bound, or `None` to stop. A leaf is reached
    /// when the exact `f64` slab test meets its box at an entry `t` no greater
    /// than the bound (initially `max_t`); inner boxes are pruned by the
    /// conservative `f32` test, which never rejects a box the `f64` test would
    /// admit. `NEAR_FIRST` visits a node's children in order of their (`f32`,
    /// conservative) entry `t`. `tally` sees every box test.
    fn traverse<const NEAR_FIRST: bool>(
        &self,
        origin: DVec3,
        dir: DVec3,
        max_t: f64,
        tally: &mut impl Tally,
        mut leaf: impl FnMut(&[u32]) -> Option<f64>,
    ) {
        if self.is_empty() {
            return;
        }
        let inv = dir.recip();
        let ray = Ray32::new(origin, inv, self.reach);
        let mut bound = max_t;
        let mut bound32 = ray.bound(bound);
        // Entries pack (slot, lower bound on its scaled entry `t`) into a
        // `u64` so the inline stack zero-fills cheaply.
        let pack = |slot: u32, entry: f32| u64::from(entry.to_bits()) << 32 | u64::from(slot);
        let mut stack = NodeStack::new(pack(self.root, f32::NEG_INFINITY));
        while let Some(e) = stack.pop() {
            let (slot, entry) = (e as u32, f32::from_bits((e >> 32) as u32));
            // `entry` is a lower bound on the scaled exact entry `t`; the
            // bound may have shrunk since the slot was pushed.
            if entry > bound32 {
                continue;
            }
            if slot & LEAF_BIT != 0 {
                let l = self.leaf(slot);
                let entered = ray_aabb(origin, inv, l.bounds).is_some_and(|t| t <= bound);
                tally.box_tests(1);
                if !entered {
                    continue;
                }
                tally.visit();
                match leaf(self.leaf_prims(l)) {
                    Some(b) => {
                        bound = b;
                        bound32 = ray.bound(b);
                    }
                    None => return,
                }
                continue;
            }
            let node = &self.nodes[slot as usize];
            tally.visit();
            let (lo, hit) = node.ray_test(&ray, bound32);
            let used = node.used();
            tally.box_tests(used.count_ones().into());
            let mut kids = [(0u32, 0f32); 4];
            let mut n = 0;
            let mut bits = hit & used;
            while bits != 0 {
                let lane = bits.trailing_zeros() as usize;
                bits &= bits - 1;
                kids[n] = (node.child[lane], lo[lane]);
                n += 1;
            }
            if NEAR_FIRST {
                // Farthest pushed first so the nearest is popped first; equal
                // entries keep lane order.
                kids[..n].sort_by(|a, b| b.1.total_cmp(&a.1));
            }
            for &(slot, entry) in &kids[..n] {
                stack.push(pack(slot, entry));
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
        let (qmin, qmax) = half_box(query);
        let mut stack = vec![self.root];
        while let Some(slot) = stack.pop() {
            if slot & LEAF_BIT != 0 {
                let l = self.leaf(slot);
                if aabb_overlap(l.bounds, query) {
                    out.extend_from_slice(self.leaf_prims(l));
                }
                continue;
            }
            let node = &self.nodes[slot as usize];
            let mut bits = node.overlaps(qmin, qmax) & node.used();
            while bits != 0 {
                stack.push(node.child[bits.trailing_zeros() as usize]);
                bits &= bits - 1;
            }
        }
        out
    }
}

/// Relative slack on `f32` slab distances: covers the three roundings of
/// `(box - origin) * inv` (≤ 3·2⁻²⁴ relative) and those of the slack itself.
const SLAB_REL: f32 = 1.0 / (1 << 21) as f32;

/// Half of `b`, rounded outward to `f32`: the form [`Node4`] stores. Halving
/// keeps `face - origin` (both halved, each within ±`f32::MAX / 2`) from
/// overflowing. Faces beyond ±`f32::MAX` (or NaN) on the outer side become
/// ±∞, which leaves that side unbounded.
fn half_box(b: Aabb) -> ([f32; 3], [f32; 3]) {
    let lim = f64::from(f32::MAX);
    // Clamping a minimum down to `lim` (a maximum up to `-lim`) stays outward.
    let min = b.min.to_array().map(|v| if v >= -lim { f32_down(v.min(lim) * 0.5) } else { f32::NEG_INFINITY });
    let max = b.max.to_array().map(|v| if v <= lim { f32_up(v.max(-lim) * 0.5) } else { f32::INFINITY });
    (min, max)
}

/// The ray in the `f32` form the 4-wide box test needs. Slab distances come
/// out multiplied by `scale`, a power of two (so scaling is exact) chosen to
/// put the largest finite `|scale / dir|` component in `[0.25, 0.5)`: with
/// half coordinates (|`face - origin`| ≤ `f32::MAX`) and `inv` = `2 · scale /
/// dir` below 1, no finite distance overflows.
struct Ray32 {
    /// Half the origin rounded down, paired with box maxima (per axis, in
    /// all four lanes).
    o_lo: [f32x4; 3],
    /// Half the origin rounded up, paired with box minima.
    o_hi: [f32x4; 3],
    /// `2 · scale / dir`, rounded to nearest.
    inv: [f32x4; 3],
    /// Absolute slack on scaled distances: covers subnormal `f32` results and
    /// subnormal `f64` reference distances, whose rounding error is absolute.
    abs: f32x4,
    scale: f64,
}

impl Ray32 {
    /// `inv` = `dir.recip()` in `f64`; `reach` bounds the magnitude of every
    /// box coordinate. Pairing box minima with the origin rounded up and
    /// maxima with it rounded down bounds each slab distance outward whatever
    /// the sign of `inv`, so only the `f32` arithmetic's relative error
    /// remains (absorbed by [`SLAB_REL`]).
    #[inline(always)]
    fn new(origin: DVec3, inv: DVec3, reach: f64) -> Self {
        let m = inv.abs().to_array().into_iter().filter(|v| v.is_finite()).fold(0.0, f64::max);
        let e = if m > 0.0 { ((m.to_bits() >> 52) as i64 & 0x7ff) - 1023 } else { 0 };
        let scale = f64::from_bits(((1021 - e.clamp(-1000, 1000)) as u64) << 52);
        let abs = f32_up(scale * f64::from_bits(1)).max(f32::MIN_POSITIVE);
        let (mut o_lo, mut o_hi, mut inv32) = ([0.0; 3], [0.0; 3], [0.0; 3]);
        // If an `f64` slab distance could overflow, the exact test may admit
        // boxes "at infinity" that the ray never meets; prune nothing then,
        // so the leaves' `f64` test alone decides, exactly as before.
        let unbounded = beyond((origin.abs().max_element() + reach) * m, f64::MAX / 4.0);
        for a in 0..3 {
            let (lo, hi) = f32_around(origin[a] * 0.5);
            let s = (inv[a] * scale * 2.0) as f32;
            if unbounded || beyond(origin[a].abs(), f64::from(f32::MAX)) {
                // Origin outside the `f32` range (or NaN): unbounded axis.
                (o_lo[a], o_hi[a], inv32[a]) = (f32::NEG_INFINITY, f32::INFINITY, f32::INFINITY);
            } else if inv[a].is_infinite() {
                // `dir[a]` is ±0: no slab distance, only "is the origin
                // between the faces". Moving the origin one step outward
                // keeps `face - origin` nonzero (no 0·∞ NaN), with the sign
                // that admits every box whose faces reach the exact origin.
                (o_lo[a], o_hi[a], inv32[a]) = (lo.next_down(), hi.next_up(), s);
            } else if s.is_normal() {
                (o_lo[a], o_hi[a], inv32[a]) = (lo, hi, s);
            } else {
                // Underflowed (or NaN) `f32` reciprocal: this axis cannot be
                // bounded, so it constrains nothing (slab is (-∞, ∞)).
                (o_lo[a], o_hi[a], inv32[a]) = (f32::NEG_INFINITY, f32::INFINITY, f32::INFINITY);
            }
        }
        Self {
            o_lo: o_lo.map(splat4),
            o_hi: o_hi.map(splat4),
            inv: inv32.map(splat4),
            abs: splat4(abs),
            scale,
        }
    }

    /// Pruning bound `b` (an unscaled `t`), scaled and rounded up.
    #[inline(always)]
    fn bound(&self, b: f64) -> f32 {
        f32_up(b * self.scale)
    }
}

impl Node4 {
    /// Bit `k` set when lane `k` holds a child.
    fn used(&self) -> u32 {
        (0..4).filter(|&k| self.child[k] != EMPTY).fold(0, |m, k| m | 1 << k)
    }

    /// Conservative slab test of the four child boxes against `r`, one 4-lane
    /// SIMD op per step. Returns per lane a lower bound on the scaled entry
    /// `t`, and as bit `k` whether the ray may meet box `k` at an entry `t`
    /// in `[0, bound]`.
    #[inline(always)]
    fn ray_test(&self, r: &Ray32, bound: f32) -> ([f32; 4], u32) {
        // Per axis, the slab's near and far distance.
        let slab = |a: usize| {
            let t1 = (f32x4::new(self.min[a]) - r.o_hi[a]) * r.inv[a];
            let t2 = (f32x4::new(self.max[a]) - r.o_lo[a]) * r.inv[a];
            (t1.min(t2), t1.max(t2))
        };
        let ((nx, fx), (ny, fy), (nz, fz)) = (slab(0), slab(1), slab(2));
        let near = nx.max(ny).max(nz);
        let far = fx.min(fy).min(fz);
        // Widen outward by the rounding slack: monotone in `t`, so it
        // commutes with the min/max above.
        let (down, up) = (splat4(1.0 - SLAB_REL), splat4(1.0 + SLAB_REL));
        let lo = (near * down).min(near * up) - r.abs;
        let hi = (far * up).max(far * down) + r.abs;
        let hit = lo.max(splat4(0.0)).simd_le(hi.min(splat4(bound)));
        (lo.to_array(), hit.to_bitmask())
    }

    /// Bit `k` set when child box `k` overlaps the query box `[qmin, qmax]`
    /// (a [`half_box`]).
    #[inline(always)]
    fn overlaps(&self, qmin: [f32; 3], qmax: [f32; 3]) -> u32 {
        let mut hit = 0b1111;
        for a in 0..3 {
            hit &= f32x4::new(self.min[a]).simd_le(splat4(qmax[a])).to_bitmask();
            hit &= f32x4::new(self.max[a]).simd_ge(splat4(qmin[a])).to_bitmask();
        }
        hit
    }
}

/// `v > lim`, or `v` is NaN.
#[inline(always)]
fn beyond(v: f64, lim: f64) -> bool {
    v.partial_cmp(&lim).is_none_or(std::cmp::Ordering::is_gt)
}

/// `x` in all four lanes. Spelled out because at opt-level "s"
/// `f32x4::splat` becomes a `memset_pattern16` call.
#[inline(always)]
fn splat4(x: f32) -> f32x4 {
    f32x4::new([x, x, x, x])
}

/// (`f32_down(x)`, `f32_up(x)`) from one conversion.
#[inline(always)]
fn f32_around(x: f64) -> (f32, f32) {
    let f = x as f32;
    let back = f64::from(f);
    (if back > x { f.next_down() } else { f }, if back < x { f.next_up() } else { f })
}

/// Largest `f32` ≤ `x` (−∞ below the `f32` range).
fn f32_down(x: f64) -> f32 {
    f32_around(x).0
}

/// Smallest `f32` ≥ `x` (+∞ above the `f32` range).
fn f32_up(x: f64) -> f32 {
    f32_around(x).1
}

/// Ray traversal work, summed over the queries it was passed to.
#[doc(hidden)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TraversalStats {
    /// Boxes tested: each child box of a visited 4-wide node (one 4-lane
    /// `f32` test covers up to four), plus the exact `f64` test of each leaf
    /// box reached.
    pub box_tests: u64,
    /// 4-wide nodes visited plus leaves entered (exact test passed within the
    /// pruning bound).
    pub nodes_visited: u64,
}

/// Sink for traversal counters; `()` discards them at zero cost.
trait Tally {
    fn box_tests(&mut self, n: u64);
    fn visit(&mut self);
}

impl Tally for () {
    #[inline(always)]
    fn box_tests(&mut self, _: u64) {}
    #[inline(always)]
    fn visit(&mut self) {}
}

impl Tally for TraversalStats {
    fn box_tests(&mut self, n: u64) {
        self.box_tests += n;
    }
    fn visit(&mut self) {
        self.nodes_visited += 1;
    }
}


/// Ranges at least this large build their two subtrees in parallel; smaller
/// ones recurse serially. Big enough that task overhead stays negligible.
const PAR_THRESHOLD: usize = 4096;

/// Bounds of `prims` and, unless it becomes a leaf, the split position within
/// it (after partitioning `prims` in place by centroid on the widest centroid
/// axis: binned SAH, median as fallback). Shared by the serial and parallel builders so both make the exact
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

    if let Some(mid) = sah_split(boxes, prims, axis, cmin[axis], extent[axis], bounds) {
        return (bounds, mid);
    }

    // No usable SAH split (zero-area bounds, or non-finite centroids put
    // everything in one bin): median split. `select_nth_unstable_by` is
    // deterministic for a given slice, so the parallel builder reproduces the
    // serial partition exactly.
    prims.select_nth_unstable_by(count / 2, |&a, &b| {
        centroid(a)[axis]
            .partial_cmp(&centroid(b)[axis])
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    (bounds, Some(count / 2))
}

/// Centroid bins for the SAH sweep along the split axis.
const SAH_BINS: usize = 16;
/// Cost of one box test, in units of one primitive test, for the SAH.
const SAH_BOX_COST: f64 = 1.0;
/// Ranges larger than this are always split, even when the SAH would rather
/// keep them as one leaf.
const MAX_LEAF_SIZE: usize = 16;

/// Half the surface area of `b`: the SAH's (scaled) chance that a random ray
/// through the parent also crosses `b`.
fn half_area(b: Aabb) -> f64 {
    let d = b.max - b.min;
    d.x * d.y + d.y * d.z + d.z * d.x
}

/// Binned surface-area-heuristic split of `prims` (bounds `bounds`, centroids
/// spanning `[lo, lo + extent]` on `axis`, `extent > 0`). Partitions `prims`
/// in place and returns `Some(Some(mid))` to split at `mid`, `Some(None)` to
/// keep a leaf (only when that is cheaper and the range is small enough), or
/// `None` when the SAH has no usable split. Deterministic: equal costs pick
/// the lowest bin boundary, and the partition is a fixed in-order pass.
fn sah_split(
    boxes: &[Aabb],
    prims: &mut [u32],
    axis: usize,
    lo: f64,
    extent: f64,
    bounds: Aabb,
) -> Option<Option<usize>> {
    let parent = half_area(bounds);
    if !(parent > 0.0 && parent.is_finite()) {
        return None;
    }
    let scale = SAH_BINS as f64 / extent;
    let bin = |i: u32| {
        let c = boxes[i as usize].center()[axis];
        // Saturating cast: NaN lands in bin 0, overflow in the last bin.
        (((c - lo) * scale) as usize).min(SAH_BINS - 1)
    };
    let mut counts = [0usize; SAH_BINS];
    let mut bin_bounds: [Option<Aabb>; SAH_BINS] = [None; SAH_BINS];
    for &i in prims.iter() {
        let k = bin(i);
        let b = boxes[i as usize];
        counts[k] += 1;
        bin_bounds[k] = Some(bin_bounds[k].map_or(b, |a| a.union(b)));
    }
    let grow = |acc: Option<Aabb>, b: Option<Aabb>| match (acc, b) {
        (Some(a), Some(b)) => Some(a.union(b)),
        (a, b) => a.or(b),
    };

    // right[k]: area × count of bins k.. (the right side of boundary k).
    let mut right = [0.0; SAH_BINS];
    let (mut acc, mut n) = (None, 0);
    for k in (1..SAH_BINS).rev() {
        acc = grow(acc, bin_bounds[k]);
        n += counts[k];
        right[k] = acc.map_or(0.0, half_area) * n as f64;
    }
    let count = prims.len();
    let mut best: Option<(f64, usize)> = None;
    let (mut acc, mut n) = (None, 0);
    for k in 1..SAH_BINS {
        acc = grow(acc, bin_bounds[k - 1]);
        n += counts[k - 1];
        if n == 0 || n == count {
            continue;
        }
        let cost = acc.map_or(0.0, half_area) * n as f64 + right[k];
        if best.is_none_or(|(c, _)| cost < c) {
            best = Some((cost, k));
        }
    }
    let (cost, k) = best?;
    // Split: one box test, then each side's primitives weighted by the chance
    // of entering it. Leaf: every primitive tested. Both scaled by `parent`.
    if count <= MAX_LEAF_SIZE && SAH_BOX_COST * parent + cost >= count as f64 * parent {
        return Some(None);
    }
    let mut mid = 0;
    for j in 0..count {
        if bin(prims[j]) < k {
            prims.swap(mid, j);
            mid += 1;
        }
    }
    Some(Some(mid))
}

/// Serially build the subtree over `prims` (which sits at `base` in the full
/// primitive list) into `nodes`, pre-order: node, left subtree, right subtree.
/// Returns the created node's index.
fn build_node(
    boxes: &[Aabb],
    prims: &mut [u32],
    base: usize,
    nodes: &mut Vec<BinNode>,
    leaf_bounds: &mut Vec<Aabb>,
) -> usize {
    let count = prims.len();
    let (bounds, split) = partition(boxes, prims);
    // Reserve this node's slot before recursing so children get later indices;
    // it stays a leaf unless split.
    let idx = nodes.len();
    nodes.push(BinNode::Leaf { start: base, count });
    leaf_bounds.push(bounds);
    if let Some(mid) = split {
        let (lp, rp) = prims.split_at_mut(mid);
        let left = build_node(boxes, lp, base, nodes, leaf_bounds);
        let right = build_node(boxes, rp, base + mid, nodes, leaf_bounds);
        nodes[idx] = BinNode::Internal { bounds, left: left as u32, right: right as u32 };
    }
    idx
}

/// A subtree built on its own, with node indices local to its arrays (root 0).
type Subtree = (Vec<BinNode>, Vec<Aabb>);

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
    nodes.push(BinNode::Leaf { start: base, count });
    leaf_bounds.push(bounds);
    if let Some(mid) = split {
        let (lp, rp) = prims.split_at_mut(mid);
        let (l, r) = rayon::join(
            || build_subtree_par(boxes, lp, base),
            || build_subtree_par(boxes, rp, base + mid),
        );
        let left = append_subtree(&mut nodes, &mut leaf_bounds, l);
        let right = append_subtree(&mut nodes, &mut leaf_bounds, r);
        nodes[0] = BinNode::Internal { bounds, left: left as u32, right: right as u32 };
    }
    (nodes, leaf_bounds)
}

/// Append `sub` to `nodes`, rebasing its internal child indices; returns the
/// index its root landed at.
fn append_subtree(nodes: &mut Vec<BinNode>, leaf_bounds: &mut Vec<Aabb>, sub: Subtree) -> usize {
    let offset = nodes.len();
    let shift = offset as u32;
    nodes.extend(sub.0.into_iter().map(|n| match n {
        BinNode::Internal { bounds, left, right } => {
            BinNode::Internal { bounds, left: left + shift, right: right + shift }
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

/// Traversal stack: 64 inline slots (deeper than any tree over realistic
/// inputs) with a heap spill that keeps LIFO order if exceeded.
struct NodeStack {
    inline: [u64; 64],
    len: usize,
    spill: Vec<u64>,
}

// Inlined by hand: the release profile's opt-level "s" otherwise keeps these
// out of line in the hottest loop.
impl NodeStack {
    #[inline(always)]
    fn new(root: u64) -> Self {
        let mut s = Self { inline: [0; 64], len: 0, spill: Vec::new() };
        s.push(root);
        s
    }

    #[inline(always)]
    fn push(&mut self, n: u64) {
        if self.len < self.inline.len() {
            self.inline[self.len] = n;
            self.len += 1;
        } else {
            self.spill.push(n);
        }
    }

    /// Spilled entries were pushed while the inline slots were full, so they
    /// are always on top.
    #[inline(always)]
    fn pop(&mut self) -> Option<u64> {
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

    /// The triangles, in the index order [`TriBvh::ray_hits_prim`] uses.
    pub fn triangles(&self) -> &[[DVec3; 3]] {
        &self.tris
    }

    /// Does [`TriBvh::ray_occluded`]'s traversal reach triangle `i`? See
    /// [`Bvh::ray_reaches_prim`].
    pub fn ray_reaches_prim(&self, origin: DVec3, dir: DVec3, i: u32) -> bool {
        self.bvh.ray_reaches_prim(origin, dir, i)
    }

    /// The box [`TriBvh::ray_reaches_prim`] tests for triangle `i`.
    pub fn prim_leaf_bounds(&self, i: u32) -> Aabb {
        self.bvh.prim_leaf_bounds(i)
    }

    /// Would [`TriBvh::ray_occluded`] accept triangle `i`? True only when its
    /// traversal reaches `i` and the exact test hits it, so a `true` here
    /// implies `ray_occluded(origin, dir)` is `true`, bit for bit. Lets a
    /// hardware ray tracer's candidate be confirmed with one exact test.
    pub fn ray_hits_prim(&self, origin: DVec3, dir: DVec3, i: u32) -> bool {
        let [a, b, c] = self.tris[i as usize];
        self.bvh.ray_reaches_prim(origin, dir, i) && ray_triangle(origin, dir, a, b, c).is_some()
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

/// The tolerance [`ray_triangle`] uses on `|det|`, the barycentrics and `t`.
pub const RAY_TRIANGLE_EPS: f64 = 1e-9;

/// Möller–Trumbore ray/triangle intersection in f64 doc space. Returns the ray
/// parameter `t > eps` at the hit (distance along `dir`), or `None` on a miss.
/// `dir` need not be normalized; `t` is in units of `dir`'s length.
pub fn ray_triangle(origin: DVec3, dir: DVec3, v0: DVec3, v1: DVec3, v2: DVec3) -> Option<f64> {
    const EPS: f64 = RAY_TRIANGLE_EPS;
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

    /// `ray_occluded` is exactly "some triangle passes `ray_hits_prim`": the
    /// single-candidate confirmation a hardware ray tracer relies on (#317).
    #[test]
    fn ray_hits_prim_characterises_ray_occluded() {
        let mut rng = Rng(0x317_317_317);
        let mut tris = random_tris(&mut rng, 400);
        // Shared edges and an axis-aligned (zero-thickness box) fan.
        for k in 0..30 {
            let x = k as f64;
            tris.push([DVec3::new(x, 0.0, 0.0), DVec3::new(x + 1.0, 0.0, 0.0), DVec3::new(x, 1.0, 0.0)]);
            tris.push([DVec3::new(x + 1.0, 0.0, 0.0), DVec3::new(x + 1.0, 1.0, 0.0), DVec3::new(x, 1.0, 0.0)]);
        }
        let tb = TriBvh::build(tris.clone());
        for k in 0..4000 {
            let o = rng.v(-50.0, 50.0);
            let d = match k % 3 {
                0 => rng.v(-1.0, 1.0),
                1 => [DVec3::X, DVec3::NEG_Y, DVec3::Z, DVec3::NEG_Z][k % 4],
                _ => DVec3::new(rng.f(-1.0, 1.0), rng.f(-1.0, 1.0), 1e-12),
            };
            let o = if k % 5 == 0 { DVec3::new(rng.f(0.0, 30.0), rng.f(0.0, 1.0), -1.0) } else { o };
            let any = (0..tris.len() as u32).any(|i| tb.ray_hits_prim(o, d, i));
            assert_eq!(any, tb.ray_occluded(o, d), "o={o} d={d}");
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

    fn assert_same_bounds(a: Aabb, b: Aabb) {
        // Bitwise, so -0.0 vs 0.0 or NaN payloads would also be caught.
        assert_eq!(a.min.to_array().map(f64::to_bits), b.min.to_array().map(f64::to_bits));
        assert_eq!(a.max.to_array().map(f64::to_bits), b.max.to_array().map(f64::to_bits));
    }

    fn assert_same_bin(a: &BinTree, b: &BinTree) {
        assert_eq!(a.prims, b.prims);
        assert_eq!(a.nodes, b.nodes);
        assert_eq!(a.leaf_bounds.len(), b.leaf_bounds.len());
        for (x, y) in a.leaf_bounds.iter().zip(&b.leaf_bounds) {
            assert_same_bounds(*x, *y);
        }
    }

    fn assert_same_tree(a: &Bvh, b: &Bvh) {
        assert_eq!(a.root, b.root);
        assert_eq!(a.prims, b.prims);
        assert_eq!(a.nodes.len(), b.nodes.len());
        for (x, y) in a.nodes.iter().zip(&b.nodes) {
            assert_eq!(x.child, y.child);
            assert_eq!(x.min.map(|r| r.map(f32::to_bits)), y.min.map(|r| r.map(f32::to_bits)));
            assert_eq!(x.max.map(|r| r.map(f32::to_bits)), y.max.map(|r| r.map(f32::to_bits)));
        }
        assert_eq!(a.leaves.len(), b.leaves.len());
        for (x, y) in a.leaves.iter().zip(&b.leaves) {
            assert_eq!((x.start, x.count), (y.start, y.count));
            assert_same_bounds(x.bounds, y.bounds);
        }
    }

    /// Both builds of `boxes` match the serial binary oracle, before and
    /// after collapsing.
    fn assert_matches_serial(boxes: &[Aabb]) {
        let serial = Bvh::build_binary_serial(boxes);
        assert_same_bin(&Bvh::build_binary(boxes), &serial);
        assert_same_tree(&Bvh::build(boxes), &Bvh::collapse(serial));
    }

    fn tri_boxes(tris: &[[DVec3; 3]]) -> Vec<Aabb> {
        tris.iter().map(|t| Aabb::from_points(t.iter().copied())).collect()
    }

    #[test]
    fn parallel_build_matches_serial_oracle_on_random_soups() {
        let mut rng = Rng(0xfeed_beef_0bad_cafe);
        // Spans below, at and far above the parallel threshold.
        for n in [0, 1, 5, PAR_THRESHOLD - 1, PAR_THRESHOLD, PAR_THRESHOLD + 1, 50_000, 200_003] {
            assert_matches_serial(&tri_boxes(&random_tris(&mut rng, n)));
        }
    }

    #[test]
    fn parallel_build_matches_serial_oracle_with_ties_and_duplicates() {
        // A flat grid (many equal centroids per axis, zero z extent) plus
        // coincident duplicates: stresses SAH bin ties and the
        // degenerate-extent leaf.
        let mut boxes = Vec::new();
        for i in 0..300 {
            for j in 0..300 {
                let p = DVec3::new((i % 37) as f64, j as f64, 0.0);
                boxes.push(Aabb::from_points([p, p + DVec3::new(1.0, 1.0, 0.0)]));
            }
        }
        boxes.extend(std::iter::repeat_n(boxes[17], 20_000));
        assert_matches_serial(&boxes);

        let tris = random_tris(&mut Rng(7), 30_000);
        let tb = TriBvh::build(tris.clone());
        assert_same_tree(&tb.bvh, &Bvh::collapse(Bvh::build_binary_serial(&tri_boxes(&tris))));
    }

    #[test]
    fn build_is_identical_for_any_thread_count() {
        let boxes = tri_boxes(&random_tris(&mut Rng(0x0311_5a40), 100_000));
        let reference = Bvh::collapse(Bvh::build_binary_serial(&boxes));
        for threads in [1, 2, 3, 8] {
            let pool = rayon::ThreadPoolBuilder::new().num_threads(threads).build().unwrap();
            assert_same_tree(&pool.install(|| Bvh::build(&boxes)), &reference);
        }
    }

    #[test]
    fn sah_keeps_clusters_apart_and_handles_degenerate_extents() {
        // Two tight clusters far apart with 1:9 counts: a median split would
        // cut the big cluster; the SAH cuts the empty gap between them.
        let mut rng = Rng(0x0311_c105);
        let mut tris = random_tris(&mut rng, 10_000);
        for t in &mut tris[..1000] {
            for v in t.iter_mut() {
                *v += DVec3::new(1e4, 0.0, 0.0);
            }
        }
        let bvh = Bvh::build_binary(&tri_boxes(&tris));
        let BinNode::Internal { left, right, .. } = bvh.nodes[0] else { panic!("leaf root") };
        let size = |n: u32| match bvh.nodes[n as usize] {
            BinNode::Leaf { count, .. } => count,
            _ => {
                let b = bvh.node_bounds(n as usize);
                tris.iter().filter(|t| t.iter().all(|&v| v.cmpge(b.min).all() && v.cmple(b.max).all())).count()
            }
        };
        assert_eq!((size(left), size(right)), (9000, 1000));

        // Zero-area bounds (collinear points) fall back to the median split
        // instead of one huge leaf; infinite boxes do not panic.
        let line: Vec<Aabb> = (0..1000)
            .map(|i| Aabb::from_points([DVec3::new(i as f64, 0.0, 0.0)]))
            .collect();
        let bvh = Bvh::build(&line);
        assert!(bvh.leaves.iter().all(|l| l.count as usize <= LEAF_SIZE));
        let q = Aabb::from_points([DVec3::new(10.0, -1.0, -1.0), DVec3::new(12.0, 1.0, 1.0)]);
        let exact = |bvh: &Bvh, boxes: &[Aabb]| {
            let mut hits = bvh.box_candidates(q);
            hits.retain(|&i| aabb_overlap(boxes[i as usize], q));
            hits.sort_unstable();
            hits
        };
        assert_eq!(exact(&bvh, &line), vec![10, 11, 12]);
        let mut wild = line.clone();
        wild.push(Aabb::from_points([DVec3::splat(f64::NEG_INFINITY), DVec3::splat(f64::INFINITY)]));
        assert_eq!(exact(&Bvh::build(&wild), &wild), vec![10, 11, 12, 1000]);
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

    /// The same tree with every node's children permuted at random and every
    /// leaf's primitives rotated: a different traversal order over identical
    /// boxes.
    fn shuffled(bvh: &Bvh, rng: &mut Rng) -> Bvh {
        let mut out = bvh.clone();
        for node in &mut out.nodes {
            let n = node.child.iter().take_while(|&&c| c != EMPTY).count();
            for k in (1..n).rev() {
                let j = (rng.next_u64() % (k as u64 + 1)) as usize;
                node.child.swap(k, j);
                for a in 0..3 {
                    node.min[a].swap(k, j);
                    node.max[a].swap(k, j);
                }
            }
        }
        for l in &out.leaves {
            let k = (rng.next_u64() % u64::from(l.count)) as usize;
            out.prims[l.start as usize..(l.start + l.count) as usize].rotate_left(k);
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
            assert!(layouts.iter().any(|b| b.prims != tb.bvh.prims || b.nodes.iter().zip(&tb.bvh.nodes).any(|(x, y)| x.child != y.child)));
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
        let popped: Vec<u64> = std::iter::from_fn(|| s.pop()).collect();
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

    /// The binary tree's own `f64` traversal: what `ray_candidates` returned
    /// before the 4-wide collapse. Sorted.
    fn bin_ray_candidates(bin: &BinTree, o: DVec3, d: DVec3) -> Vec<u32> {
        bin_candidates(bin, |b| ray_aabb(o, d.recip(), b).is_some())
    }

    fn bin_candidates(bin: &BinTree, keep: impl Fn(Aabb) -> bool) -> Vec<u32> {
        let mut out = Vec::new();
        let mut stack = if bin.nodes.is_empty() { vec![] } else { vec![0] };
        while let Some(n) = stack.pop() {
            if !keep(bin.node_bounds(n)) {
                continue;
            }
            match bin.nodes[n] {
                BinNode::Leaf { start, count } => out.extend_from_slice(&bin.prims[start..start + count]),
                BinNode::Internal { left, right, .. } => stack.extend([left as usize, right as usize]),
            }
        }
        out.sort_unstable();
        out
    }

    fn sorted(mut v: Vec<u32>) -> Vec<u32> {
        v.sort_unstable();
        v
    }

    /// `random_tris` scaled by `s` and shifted by `off`: site-like offsets,
    /// and magnitudes near and beyond the edges of the `f32` range.
    const SCALES: [(f64, f64); 8] = [
        (1.0, 0.0),
        (1.0, 6.2e6),
        (1e-3, 4.1e5),
        (1e-30, 0.0),
        (1e-41, 0.0),
        (1e30, 0.0),
        (1e36, 0.0),
        (1e40, 0.0),
    ];

    /// Directions with mixed magnitudes, so `1/dir` spans (and leaves) the
    /// `f32` range.
    fn odd_dir(rng: &mut Rng) -> DVec3 {
        let mags = [1.0, 1e-20, 1e20, 1e-45, 1e45, 0.0, 1e-300];
        let mut d = rng.v(-1.0, 1.0);
        for a in 0..3 {
            if rng.next_u64().is_multiple_of(3) {
                d[a] *= mags[(rng.next_u64() % mags.len() as u64) as usize];
            }
        }
        d
    }

    #[test]
    fn candidates_match_binary_tree_at_any_scale() {
        let mut rng = Rng(0x0312_ca4d_0000_0001);
        for (s, off) in SCALES {
            let tris: Vec<[DVec3; 3]> =
                random_tris(&mut rng, 3000).into_iter().map(|t| t.map(|v| v * s + off)).collect();
            let boxes = tri_boxes(&tris);
            let bin = Bvh::build_binary(&boxes);
            let tb = TriBvh::build(tris.clone());
            for k in 0..600 {
                let o = rng.v(-60.0, 60.0) * s + off;
                let d = match k % 4 {
                    0 => rng.v(-1.0, 1.0),
                    1 => PARALLEL_DIRS[(rng.next_u64() % 8) as usize],
                    2 => odd_dir(&mut rng),
                    _ => {
                        // Grazing: from a triangle vertex along an axis or
                        // in an axis plane, i.e. along box faces.
                        let v = tris[(rng.next_u64() % tris.len() as u64) as usize][0];
                        let d = PARALLEL_DIRS[(rng.next_u64() % 8) as usize];
                        check_ray(&tb, v, d);
                        assert_eq!(sorted(tb.bvh.ray_candidates(v, d)), bin_ray_candidates(&bin, v, d), "s={s} o={v} d={d}");
                        d
                    }
                };
                assert_eq!(sorted(tb.bvh.ray_candidates(o, d)), bin_ray_candidates(&bin, o, d), "s={s} o={o} d={d}");
                if d.is_finite() && d != DVec3::ZERO {
                    check_ray(&tb, o, d);
                }
                let a = rng.v(-60.0, 60.0) * s + off;
                let q = Aabb::from_points([a, a + rng.v(0.0, 8.0) * s]);
                assert_eq!(
                    sorted(tb.bvh.box_candidates(q)),
                    bin_candidates(&bin, |b| aabb_overlap(b, q)),
                    "s={s} q={q:?}"
                );
            }
        }
    }

    /// One box against [`Node4::ray_test`]: whenever the exact `f64` slab
    /// test enters it within `bound`, so must the `f32` test, with an entry
    /// no larger than the exact one.
    #[test]
    fn f32_box_test_is_conservative() {
        let mut rng = Rng(0x0312_c0de_0000_0002);
        let mut entered = 0;
        for _ in 0..200_000 {
            let (s, off) = SCALES[(rng.next_u64() % SCALES.len() as u64) as usize];
            let a = rng.v(-10.0, 10.0);
            let b = Aabb::from_points([a * s + off, (a + rng.v(0.0, 4.0)) * s + off]);
            // Origins inside, outside and exactly on the faces.
            let o = match rng.next_u64() % 3 {
                0 => rng.v(-20.0, 20.0) * s + off,
                1 => DVec3::new(b.min.x, rng.f(-20.0, 20.0) * s + off, b.max.z),
                _ => b.min,
            };
            let d = if rng.next_u64().is_multiple_of(2) {
                odd_dir(&mut rng)
            } else {
                PARALLEL_DIRS[(rng.next_u64() % 8) as usize]
            };
            let inv = d.recip();
            let Some(entry) = ray_aabb(o, inv, b) else { continue };
            let ray = Ray32::new(o, inv, b.min.abs().max(b.max.abs()).max_element());
            let mut node = Node4 {
                min: [[f32::INFINITY; 4]; 3],
                max: [[f32::NEG_INFINITY; 4]; 3],
                child: [0, EMPTY, EMPTY, EMPTY],
            };
            let (min, max) = half_box(b);
            for k in 0..3 {
                node.min[k][0] = min[k];
                node.max[k][0] = max[k];
            }
            for bound in [f64::INFINITY, entry, entry * 2.0, entry.next_up()] {
                let (lo, hit) = node.ray_test(&ray, ray.bound(bound));
                assert!(hit & 1 != 0, "missed b={b:?} o={:e} {:e} {:e} d={:e} {:e} {:e} bound={bound} entry={entry}", o.x, o.y, o.z, d.x, d.y, d.z);
                assert!(f64::from(lo[0]) <= entry * ray.scale, "entry {} > {entry} b={b:?} o={o} d={d}", lo[0]);
            }
            entered += 1;
        }
        assert!(entered > 25_000, "only {entered} rays entered");
    }

    #[test]
    fn collapse_fills_wide_nodes_and_keeps_every_leaf() {
        let boxes = tri_boxes(&random_tris(&mut Rng(0x0312_f111), 20_000));
        let bin = Bvh::build_binary(&boxes);
        let bvh = Bvh::collapse(Bvh::build_binary(&boxes));
        let bin_leaves = bin.nodes.iter().filter(|n| matches!(n, BinNode::Leaf { .. })).count();
        assert_eq!(bvh.leaves.len(), bin_leaves);
        let mut covered: Vec<u32> = bvh.leaves.iter().flat_map(|l| bvh.leaf_prims(l).to_vec()).collect();
        covered.sort_unstable();
        assert_eq!(covered, (0..boxes.len() as u32).collect::<Vec<_>>());
        let lanes: usize = bvh.nodes.iter().map(|n| n.child.iter().filter(|&&c| c != EMPTY).count()).sum();
        // Every slot but the root is some node's child.
        assert_eq!(lanes, bvh.nodes.len() + bvh.leaves.len() - 1);
        assert!(lanes as f64 / bvh.nodes.len() as f64 > 2.5, "{lanes} lanes / {} nodes", bvh.nodes.len());
    }
}
