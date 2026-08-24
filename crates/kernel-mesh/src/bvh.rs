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
#[derive(Clone, Debug)]
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
        let mut nodes = Vec::new();
        let mut leaf_bounds = Vec::new();
        let root = if boxes.is_empty() {
            usize::MAX
        } else {
            build_node(boxes, &mut prims, 0, boxes.len(), &mut nodes, &mut leaf_bounds)
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

/// Recursively partition `prims[start..end]` by median centroid on the widest
/// axis. Returns the created node's index.
fn build_node(
    boxes: &[Aabb],
    prims: &mut [u32],
    start: usize,
    end: usize,
    nodes: &mut Vec<Node>,
    leaf_bounds: &mut Vec<Aabb>,
) -> usize {
    let bounds = prims[start..end]
        .iter()
        .map(|&i| boxes[i as usize])
        .reduce(Aabb::union)
        .expect("non-empty range");
    let count = end - start;
    if count <= LEAF_SIZE {
        let idx = nodes.len();
        nodes.push(Node::Leaf { start, count });
        leaf_bounds.push(bounds);
        return idx;
    }

    // Split on the axis with the largest centroid spread; fall back to a leaf
    // if all centroids coincide (degenerate, e.g. many coincident primitives).
    let centroid = |i: u32| boxes[i as usize].center();
    let mut cmin = DVec3::splat(f64::INFINITY);
    let mut cmax = DVec3::splat(f64::NEG_INFINITY);
    for &i in &prims[start..end] {
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
        let idx = nodes.len();
        nodes.push(Node::Leaf { start, count });
        leaf_bounds.push(bounds);
        return idx;
    }

    let mid = start + count / 2;
    // Partition around the median centroid on `axis` (nth_element style).
    prims[start..end].select_nth_unstable_by(count / 2, |&a, &b| {
        centroid(a)[axis]
            .partial_cmp(&centroid(b)[axis])
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    // Reserve this node's slot before recursing so children get later indices.
    let idx = nodes.len();
    nodes.push(Node::Leaf { start: 0, count: 0 }); // placeholder
    leaf_bounds.push(bounds);
    let left = build_node(boxes, prims, start, mid, nodes, leaf_bounds);
    let right = build_node(boxes, prims, mid, end, nodes, leaf_bounds);
    nodes[idx] = Node::Internal { bounds, left: left as u32, right: right as u32 };
    idx
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
        let boxes: Vec<Aabb> = tris
            .iter()
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
        for i in self.bvh.ray_candidates(origin, dir) {
            let [a, b, c] = self.tris[i as usize];
            if ray_triangle(origin, dir, a, b, c).is_some() {
                return true;
            }
        }
        false
    }

    /// Nearest ray/triangle hit distance `t > eps` along `dir`, or `None`.
    pub fn ray_hit(&self, origin: DVec3, dir: DVec3) -> Option<f64> {
        let mut best: Option<f64> = None;
        for i in self.bvh.ray_candidates(origin, dir) {
            let [a, b, c] = self.tris[i as usize];
            if let Some(t) = ray_triangle(origin, dir, a, b, c)
                && best.is_none_or(|bt| t < bt)
            {
                best = Some(t);
            }
        }
        best
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
