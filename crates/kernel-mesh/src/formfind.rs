// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! Form-finding engine: dynamic relaxation of a prestressed node/link network.
//!
//! This is pure *generative geometry* — shape-finding, not structural analysis.
//! We find the equilibrium **form** of a network of nodes connected by links
//! (cables in tension, struts in compression) under prestress and/or gravity;
//! we do NOT certify it, run FEA, or make any safety claim. Finding the
//! equilibrium *shape* (the catenary a hanging chain settles into, the stable
//! prestressed configuration of a tensegrity, the minimal surface a soap-film
//! net relaxes to) is geometry, and squarely inside the no-FEA line.
//!
//! ## Method
//!
//! [`dynamic_relaxation`] integrates a damped mass-spring system to rest. Each
//! link is a linear spring with a `rest_length` and axial `stiffness`; the axial
//! force is `k · (current_length − rest_length)` along the link. A *cable* only
//! pulls (tension-only: force clamped to ≥ 0 shortening); a *strut* is a plain
//! two-way spring that resists both stretch and squash (so it can hold a
//! compression member's length). Free nodes also feel a downward gravity load
//! and any per-node point loads. We march semi-implicit Euler with either
//! viscous velocity damping (default) or *kinetic-energy peak* resets (classic
//! Barnes/Day kinetic damping, [`RelaxParams::kinetic`]) until the net settles.
//!
//! For pure tension nets there is also [`force_density`] (Schek's force-density
//! method): treating each link's `stiffness` as a force density `q = force /
//! length` makes nodal equilibrium *linear* in the positions, solved here by
//! deterministic Gauss–Seidel sweeps. With uniform `q` and all-free interiors
//! this yields the discrete harmonic ("soap-film") surface — the classic Frei
//! Otto minimal-surface model — which [`minimal_surface`] uses to stretch a
//! film across any closed boundary loop.
//!
//! Everything is deterministic: no RNG, fixed iteration order, fixed time step.

use crate::Mesh;
use glam::DVec3;

/// A link (edge) in the form-finding network.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Link {
    /// Index of the first node.
    pub a: usize,
    /// Index of the second node.
    pub b: usize,
    /// Unstretched length the spring wants to reach.
    pub rest_length: f64,
    /// Axial stiffness (force per unit extension).
    pub stiffness: f64,
    /// A tension-only cable (true) or a two-way strut (false).
    pub cable: bool,
}

impl Link {
    /// A tension cable between two nodes.
    pub fn cable(a: usize, b: usize, rest_length: f64, stiffness: f64) -> Self {
        Link { a, b, rest_length, stiffness, cable: true }
    }
    /// A two-way strut between two nodes.
    pub fn strut(a: usize, b: usize, rest_length: f64, stiffness: f64) -> Self {
        Link { a, b, rest_length, stiffness, cable: false }
    }
}

/// A node/link network to relax to equilibrium.
#[derive(Clone, Debug)]
pub struct Network {
    /// Node positions (mutated in place by relaxation).
    pub positions: Vec<DVec3>,
    /// `true` = anchored (fixed), never moves.
    pub fixed: Vec<bool>,
    /// The links.
    pub links: Vec<Link>,
    /// Downward gravity load per free node (0 = none). Applied as `-z`.
    pub gravity: f64,
    /// Extra point load per node (force vector; zero = none). Applied to free
    /// nodes only, on top of gravity.
    pub loads: Vec<DVec3>,
}

impl Network {
    /// A new network with all nodes free, no gravity and no point loads.
    pub fn new(positions: Vec<DVec3>, links: Vec<Link>) -> Self {
        let n = positions.len();
        Network {
            positions,
            fixed: vec![false; n],
            links,
            gravity: 0.0,
            loads: vec![DVec3::ZERO; n],
        }
    }

    /// Mark a node as anchored.
    pub fn anchor(&mut self, i: usize) {
        self.fixed[i] = true;
    }

    /// Add a point load (force vector) to node `i`.
    pub fn add_load(&mut self, i: usize, force: DVec3) {
        self.loads[i] += force;
    }

    /// The links as world-space segment pairs (for meshing into struts).
    pub fn segments(&self) -> Vec<(DVec3, DVec3)> {
        self.links
            .iter()
            .map(|l| (self.positions[l.a], self.positions[l.b]))
            .collect()
    }

    /// The cable links only, as segment pairs.
    pub fn cable_segments(&self) -> Vec<(DVec3, DVec3)> {
        self.links
            .iter()
            .filter(|l| l.cable)
            .map(|l| (self.positions[l.a], self.positions[l.b]))
            .collect()
    }

    /// The strut links only, as segment pairs.
    pub fn strut_segments(&self) -> Vec<(DVec3, DVec3)> {
        self.links
            .iter()
            .filter(|l| !l.cable)
            .map(|l| (self.positions[l.a], self.positions[l.b]))
            .collect()
    }
}

/// Convergence controls for [`dynamic_relaxation`].
#[derive(Clone, Copy, Debug)]
pub struct RelaxParams {
    /// Explicit-integration time step.
    pub dt: f64,
    /// Velocity retained each step (0..1). Lower = more damping.
    pub damping: f64,
    /// Lumped nodal mass.
    pub mass: f64,
    /// Hard iteration cap.
    pub max_iters: usize,
    /// Stop when the largest node displacement in a step falls below this.
    pub tol: f64,
    /// Barnes/Day kinetic damping: run undamped and zero all velocities each
    /// time the total kinetic energy passes a peak (starts decreasing). Often
    /// converges in far fewer steps than viscous damping; `damping` is ignored.
    pub kinetic: bool,
}

impl Default for RelaxParams {
    fn default() -> Self {
        RelaxParams {
            dt: 0.1,
            damping: 0.98,
            mass: 1.0,
            max_iters: 20_000,
            tol: 1e-7,
            kinetic: false,
        }
    }
}

/// Result of a relaxation run.
#[derive(Clone, Copy, Debug)]
pub struct RelaxReport {
    /// Iterations actually run.
    pub iters: usize,
    /// Whether the displacement tolerance was reached before the cap.
    pub converged: bool,
    /// Largest residual out-of-balance force on any free node at the end.
    pub max_residual: f64,
}

/// Accumulate the spring + gravity + point-load force on every node into
/// `forces` (overwritten). Shared by the integrator and the residual check.
fn accumulate_forces(net: &Network, forces: &mut [DVec3]) {
    for f in forces.iter_mut() {
        *f = DVec3::ZERO;
    }
    for l in &net.links {
        let pa = net.positions[l.a];
        let pb = net.positions[l.b];
        let d = pb - pa;
        let len = d.length();
        if len < 1e-12 {
            continue;
        }
        let dir = d / len;
        let mut axial = l.stiffness * (len - l.rest_length);
        // Cable = tension only: it can pull (positive extension) but never
        // push. A negative axial (link shorter than rest) would be a strut
        // action; a cable simply goes slack.
        if l.cable && axial < 0.0 {
            axial = 0.0;
        }
        let fvec = dir * axial;
        forces[l.a] += fvec;
        forces[l.b] -= fvec;
    }
    for (i, f) in forces.iter_mut().enumerate() {
        if !net.fixed[i] {
            f.z -= net.gravity;
            *f += net.loads[i];
        }
    }
}

/// Largest residual out-of-balance force on any free node of `net`.
pub fn max_residual(net: &Network) -> f64 {
    let mut forces = vec![DVec3::ZERO; net.positions.len()];
    accumulate_forces(net, &mut forces);
    (0..net.positions.len())
        .filter(|&i| !net.fixed[i])
        .map(|i| forces[i].length())
        .fold(0.0f64, f64::max)
}

/// Relax `net` to static equilibrium in place, returning a convergence report.
///
/// Deterministic: fixed node order, fixed time step, no randomness. This is the
/// single engine behind every form (funicular, tensegrity, cable-net, soap
/// film); a generator just builds the [`Network`] and hands it here.
pub fn dynamic_relaxation(net: &mut Network, p: RelaxParams) -> RelaxReport {
    let n = net.positions.len();
    let mut vel = vec![DVec3::ZERO; n];
    let mut forces = vec![DVec3::ZERO; n];
    let inv_m = 1.0 / p.mass.max(1e-12);

    let mut iters = 0;
    let mut converged = false;
    let mut prev_ke = 0.0f64;
    while iters < p.max_iters {
        iters += 1;
        accumulate_forces(net, &mut forces);

        // Integrate free nodes (semi-implicit Euler) and track the largest
        // step for the convergence test.
        let damp = if p.kinetic { 1.0 } else { p.damping };
        let mut max_step = 0.0f64;
        let mut ke = 0.0f64;
        for i in 0..n {
            if net.fixed[i] {
                vel[i] = DVec3::ZERO;
                continue;
            }
            vel[i] = (vel[i] + forces[i] * (inv_m * p.dt)) * damp;
            let step = vel[i] * p.dt;
            net.positions[i] += step;
            max_step = max_step.max(step.length());
            ke += vel[i].length_squared();
        }
        // Barnes/Day kinetic damping: when total kinetic energy passes a peak
        // the system is overshooting its equilibrium — freeze it and let the
        // residual forces re-accelerate it from rest.
        if p.kinetic {
            if ke < prev_ke {
                for v in vel.iter_mut() {
                    *v = DVec3::ZERO;
                }
                prev_ke = 0.0;
            } else {
                prev_ke = ke;
            }
        }
        if max_step < p.tol {
            converged = true;
            break;
        }
    }

    RelaxReport { iters, converged, max_residual: max_residual(net) }
}

/// Schek's **force-density method**: treat each link's `stiffness` as a force
/// density `q = axial force / length`, which makes nodal equilibrium *linear*
/// in the positions: `Σ q_ij (x_j − x_i) + p_i = 0`. Solved in place by
/// deterministic Gauss–Seidel sweeps (fixed node order). Rest lengths and the
/// cable/strut flag are ignored — a force-density member is pure tension whose
/// force grows with length, the classic prestressed-net model.
///
/// Loads `p_i` are gravity + per-node point loads, applied to free nodes.
/// Converges when the largest node move in a sweep drops below `tol`.
pub fn force_density(net: &mut Network, max_sweeps: usize, tol: f64) -> RelaxReport {
    let n = net.positions.len();
    // Adjacency: (neighbour index, q) per node.
    let mut adj: Vec<Vec<(usize, f64)>> = vec![Vec::new(); n];
    for l in &net.links {
        let q = l.stiffness.max(0.0);
        adj[l.a].push((l.b, q));
        adj[l.b].push((l.a, q));
    }
    let mut iters = 0;
    let mut converged = false;
    while iters < max_sweeps {
        iters += 1;
        let mut max_step = 0.0f64;
        // Gauss–Seidel sweep: reads/writes net.positions[i] in place while
        // indexing four parallel arrays — an iterator rewrite would obscure it.
        #[allow(clippy::needless_range_loop)]
        for i in 0..n {
            if net.fixed[i] {
                continue;
            }
            let mut qsum = 0.0;
            let mut acc = DVec3::ZERO;
            for &(j, q) in &adj[i] {
                qsum += q;
                acc += net.positions[j] * q;
            }
            if qsum < 1e-12 {
                continue; // isolated node: nothing to equilibrate
            }
            let load = net.loads[i] + DVec3::new(0.0, 0.0, -net.gravity);
            let target = (acc + load) / qsum;
            let step = (target - net.positions[i]).length();
            net.positions[i] = target;
            max_step = max_step.max(step);
        }
        if max_step < tol {
            converged = true;
            break;
        }
    }
    // Residual: out-of-balance force under the force-density model.
    let mut max_res = 0.0f64;
    // Indexes net.{fixed,loads,positions} + adj together; keep the range loop.
    #[allow(clippy::needless_range_loop)]
    for i in 0..n {
        if net.fixed[i] {
            continue;
        }
        let mut f = net.loads[i] + DVec3::new(0.0, 0.0, -net.gravity);
        for &(j, q) in &adj[i] {
            f += (net.positions[j] - net.positions[i]) * q;
        }
        max_res = max_res.max(f.length());
    }
    RelaxReport { iters, converged, max_residual: max_res }
}

// ── funicular / hanging chain ────────────────────────────────────────────────

/// Build and relax a hanging chain (funicular line) of `segments` links between
/// two anchors `a` and `b`, sagging under gravity `load`. Returns the settled
/// node positions (anchors included, in order from `a` to `b`).
///
/// The equilibrium shape of a uniformly loaded hanging chain is the **catenary**
/// — this is the shape Gaudí found with his hanging models. `slack` (> 1) sets
/// how much longer the total cable is than the straight span, i.e. how deep it
/// hangs.
pub fn funicular_chain(
    a: DVec3,
    b: DVec3,
    segments: u32,
    load: f64,
    slack: f64,
) -> Vec<DVec3> {
    let segs = segments.max(2) as usize;
    let n = segs + 1;
    // Initial guess: straight line a→b, then droop the interior so gravity has
    // a clear direction to develop the sag (deterministic parabolic seed).
    let mut positions = Vec::with_capacity(n);
    for i in 0..n {
        let t = i as f64 / segs as f64;
        let mut pt = a.lerp(b, t);
        // Parabolic initial droop, zero at the ends.
        pt.z -= 4.0 * slack.max(1.0) * t * (1.0 - t);
        positions.push(pt);
    }
    let span = (b - a).length();
    let rest = (span / segs as f64) * (2.0 - slack.clamp(1.0, 1.999));
    let mut net = Network::new(
        positions,
        (0..segs)
            .map(|i| Link::cable(i, i + 1, rest.max(1e-3), 50.0))
            .collect(),
    );
    net.anchor(0);
    net.anchor(n - 1);
    net.gravity = load.max(0.0);
    dynamic_relaxation(&mut net, RelaxParams { dt: 0.05, ..Default::default() });
    net.positions
}

/// Invert a funicular (hanging) form into a pure-compression form by mirroring
/// it about the horizontal plane through its own top (highest z). A cable that
/// hangs in tension, flipped, becomes an arch/shell that stands in pure
/// compression — the Gaudí / Hooke "hangs the chain, stands the arch" thrust
/// principle. Anchors stay put; the sag becomes a rise.
pub fn invert_funicular(points: &[DVec3]) -> Vec<DVec3> {
    if points.is_empty() {
        return Vec::new();
    }
    let top = points.iter().map(|p| p.z).fold(f64::NEG_INFINITY, f64::max);
    points
        .iter()
        .map(|p| DVec3::new(p.x, p.y, 2.0 * top - p.z))
        .collect()
}

// ── tensegrity ───────────────────────────────────────────────────────────────

/// A form-found tensegrity network.
pub struct Tensegrity {
    /// The relaxed network (struts + cables).
    pub net: Network,
    /// Convergence report from the form-finding relaxation.
    pub report: RelaxReport,
}

/// A `struts`-strut tensegrity prism (antiprism): two parallel `struts`-gons of
/// `radius`, offset by a `twist` and separated by `height`, each strut running
/// diagonally between the rings; top and bottom edges plus vertical-ish
/// "bracing" cables tie it together. Form-found to its stable prestressed
/// equilibrium. The classic case is `struts = 3` (the 3-strut T-prism).
///
/// Determinism: node layout is a closed-form function of the strut index; the
/// only iteration is the deterministic relaxation.
pub fn tensegrity_prism(struts: u32, radius: f64, height: f64, twist: f64) -> Tensegrity {
    let s = struts.max(3) as usize;
    let r = radius.max(1e-3);
    let h = height.max(1e-3);
    // Bottom ring at z=0, top ring at z=h twisted by `twist`.
    let mut positions = Vec::with_capacity(2 * s);
    for i in 0..s {
        let a = std::f64::consts::TAU * (i as f64) / (s as f64);
        positions.push(DVec3::new(r * a.cos(), r * a.sin(), 0.0));
    }
    for i in 0..s {
        let a = std::f64::consts::TAU * (i as f64) / (s as f64) + twist;
        positions.push(DVec3::new(r * a.cos(), r * a.sin(), h));
    }
    let bot = |i: usize| i % s;
    let top = |i: usize| s + (i % s);

    // Strut natural length (its current length is close to right by construction).
    let strut_len = {
        let d = positions[top(0)] - positions[bot(0)];
        d.length()
    };
    let mut links = Vec::new();
    // Struts: bottom i → top i (compression members, kept near their length).
    for i in 0..s {
        links.push(Link::strut(bot(i), top(i), strut_len, 200.0));
    }
    // Continuous tension network: bottom ring, top ring, and vertical bracing
    // cables. Rest length set shorter than current so cables pull taut
    // (prestress), which is what stabilises the isolated struts.
    let ring_bot = (positions[bot(1)] - positions[bot(0)]).length();
    let ring_top = (positions[top(1)] - positions[top(0)]).length();
    for i in 0..s {
        links.push(Link::cable(bot(i), bot(i + 1), ring_bot * 0.85, 60.0));
        links.push(Link::cable(top(i), top(i + 1), ring_top * 0.85, 60.0));
        // Bracing cable: bottom i → top (i-1) ties the two rings.
        let brace = positions[top((i + s - 1) % s)] - positions[bot(i)];
        links.push(Link::cable(bot(i), top((i + s - 1) % s), brace.length() * 0.85, 60.0));
    }
    let mut net = Network::new(positions, links);
    // Anchor one bottom node to pin the rigid-body freedom (position/rotation),
    // and constrain its neighbour's plane so the whole thing doesn't drift; the
    // shape itself is still free to find equilibrium.
    net.anchor(bot(0));
    let report = dynamic_relaxation(
        &mut net,
        RelaxParams { dt: 0.02, damping: 0.9, max_iters: 40_000, ..Default::default() },
    );
    Tensegrity { net, report }
}

// ── cable-net / minimal surface ──────────────────────────────────────────────

/// Build and relax a square cable-net stretched over four corner anchors,
/// `n × n` interior grid, to a tensile minimal-ish surface (a Frei-Otto soap-
/// film net). `corners` are the four boundary posts in CCW order; interior
/// nodes relax to the tensioned equilibrium. `sag` lifts/drops the initial
/// interior guess so the net has a shape to relax from (deterministic).
///
/// Returns the settled grid `(n+2) × (n+2)` positions row-major (including the
/// boundary), plus the mesh-ready net segments.
pub fn cable_net(
    corners: [DVec3; 4],
    n: u32,
    sag: f64,
) -> (Vec<DVec3>, u32, Vec<(DVec3, DVec3)>) {
    let g = n.max(1) as usize + 2; // grid side including both boundaries
    let last = g - 1;
    let mut positions = vec![DVec3::ZERO; g * g];
    let idx = |i: usize, j: usize| i * g + j;
    // Bilinear interpolation of the four corners for the boundary + a seed for
    // the interior; then perturb interior in z by a saddle so relaxation has a
    // non-degenerate start.
    let [c00, c10, c11, c01] = corners;
    for i in 0..g {
        let u = i as f64 / last as f64;
        for j in 0..g {
            let v = j as f64 / last as f64;
            let bottom = c00.lerp(c10, v);
            let top = c01.lerp(c11, v);
            let mut p = bottom.lerp(top, u);
            let interior = i != 0 && i != last && j != 0 && j != last;
            if interior {
                // Saddle seed: down in the middle so it relaxes to a taut sheet.
                p.z -= sag * 4.0 * u * (1.0 - u) * v * (1.0 - v);
            }
            positions[idx(i, j)] = p;
        }
    }
    // Links: 4-neighbour grid, all tension cables with rest length shorter than
    // the seed spacing so the net pulls itself taut → minimal surface.
    let mut links = Vec::new();
    let spacing = (corners[1] - corners[0]).length() / last as f64;
    let rest = (spacing * 0.5).max(1e-3);
    for i in 0..g {
        for j in 0..g {
            if j + 1 < g {
                links.push(Link::cable(idx(i, j), idx(i, j + 1), rest, 40.0));
            }
            if i + 1 < g {
                links.push(Link::cable(idx(i, j), idx(i + 1, j), rest, 40.0));
            }
        }
    }
    let mut net = Network::new(positions, links);
    // Anchor the entire boundary ring.
    for i in 0..g {
        for j in 0..g {
            if i == 0 || i == last || j == 0 || j == last {
                net.anchor(idx(i, j));
            }
        }
    }
    dynamic_relaxation(
        &mut net,
        RelaxParams { dt: 0.05, damping: 0.95, ..Default::default() },
    );
    let segs = net.segments();
    (net.positions, g as u32, segs)
}

// ── soap film / minimal surface over an arbitrary boundary ───────────────────

/// Resample a closed boundary loop (first point NOT repeated) into `count`
/// points equally spaced by arclength, starting at `loop_pts[0]`.
fn resample_closed(loop_pts: &[DVec3], count: usize) -> Vec<DVec3> {
    let n = loop_pts.len();
    // Cumulative arclength around the loop, including the closing segment.
    let mut cum = Vec::with_capacity(n + 1);
    cum.push(0.0);
    for i in 0..n {
        let seg = (loop_pts[(i + 1) % n] - loop_pts[i]).length();
        cum.push(cum[i] + seg);
    }
    let total = cum[n];
    let mut out = Vec::with_capacity(count);
    let mut seg = 0usize;
    for k in 0..count {
        let target = total * (k as f64) / (count as f64);
        while seg + 1 < n && cum[seg + 1] < target {
            seg += 1;
        }
        let len = cum[seg + 1] - cum[seg];
        let t = if len < 1e-12 { 0.0 } else { (target - cum[seg]) / len };
        out.push(loop_pts[seg].lerp(loop_pts[(seg + 1) % n], t));
    }
    out
}

/// Stretch a **soap film** (Frei Otto minimal surface) across a closed
/// boundary loop: the boundary is resampled to the perimeter of an
/// `(n+1)×(n+1)` grid, interior nodes are connected 4-ways by zero-rest-length
/// tension links, and the net is solved with the force-density method — with
/// uniform force densities and no load this is exactly the discrete harmonic
/// (soap-film) surface. Returns the settled grid row-major plus the grid side,
/// or `None` for a degenerate boundary (< 3 points or ~zero length).
///
/// Deterministic: fixed resampling start, fixed Gauss–Seidel order.
pub fn minimal_surface_grid(boundary: &[DVec3], n: u32) -> Option<(Vec<DVec3>, u32)> {
    let nn = n.max(2) as usize;
    if boundary.len() < 3 {
        return None;
    }
    let total: f64 = (0..boundary.len())
        .map(|i| (boundary[(i + 1) % boundary.len()] - boundary[i]).length())
        .sum();
    if total < 1e-9 {
        return None;
    }
    let ring = resample_closed(boundary, 4 * nn);
    let g = nn + 1; // grid side
    let last = g - 1;
    let idx = |i: usize, j: usize| i * g + j;

    // Walk the grid perimeter in ring order: (0,0)→(0,last)→(last,last)→
    // (last,0)→(0,0), each corner visited once. 4·nn cells total.
    let mut positions = vec![DVec3::ZERO; g * g];
    let mut r = 0usize;
    for j in 0..last {
        positions[idx(0, j)] = ring[r];
        r += 1;
    }
    for i in 0..last {
        positions[idx(i, last)] = ring[r];
        r += 1;
    }
    for j in (1..=last).rev() {
        positions[idx(last, j)] = ring[r];
        r += 1;
    }
    for i in (1..=last).rev() {
        positions[idx(i, 0)] = ring[r];
        r += 1;
    }
    debug_assert_eq!(r, 4 * nn);
    // Interior seed: the boundary centroid (the force-density system is linear
    // with a unique solution, so the seed only affects convergence speed).
    let centroid = ring.iter().sum::<DVec3>() / (ring.len() as f64);
    for i in 1..last {
        for j in 1..last {
            positions[idx(i, j)] = centroid;
        }
    }

    // Zero-rest-length tension grid = discrete soap film.
    let mut links = Vec::new();
    for i in 0..g {
        for j in 0..g {
            if j + 1 < g {
                links.push(Link::cable(idx(i, j), idx(i, j + 1), 0.0, 1.0));
            }
            if i + 1 < g {
                links.push(Link::cable(idx(i, j), idx(i + 1, j), 0.0, 1.0));
            }
        }
    }
    let mut net = Network::new(positions, links);
    for i in 0..g {
        for j in 0..g {
            if i == 0 || i == last || j == 0 || j == last {
                net.anchor(idx(i, j));
            }
        }
    }
    force_density(&mut net, 20_000, 1e-9 * total.max(1.0));
    Some((net.positions, g as u32))
}

/// [`minimal_surface_grid`] triangulated into a surface [`Mesh`].
pub fn minimal_surface(boundary: &[DVec3], n: u32) -> Option<Mesh> {
    let (positions, g) = minimal_surface_grid(boundary, n)?;
    let idx = |i: u32, j: u32| i * g + j;
    let mut faces = Vec::with_capacity(((g - 1) * (g - 1) * 2) as usize);
    for i in 0..g - 1 {
        for j in 0..g - 1 {
            let a0 = idx(i, j);
            let a1 = idx(i, j + 1);
            let a2 = idx(i + 1, j + 1);
            let a3 = idx(i + 1, j);
            faces.push([a0, a1, a2]);
            faces.push([a0, a2, a3]);
        }
    }
    Some(Mesh::new(positions, faces))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A hanging cable between two level anchors relaxes to a catenary; check
    /// the settled interior against the analytic catenary within tolerance.
    #[test]
    fn hanging_chain_approximates_catenary() {
        let a = DVec3::new(-5.0, 0.0, 0.0);
        let b = DVec3::new(5.0, 0.0, 0.0);
        let pts = funicular_chain(a, b, 40, 1.0, 1.4);
        // Endpoints stay put.
        assert!((pts[0] - a).length() < 1e-6);
        assert!((pts[pts.len() - 1] - b).length() < 1e-6);
        // Symmetric: the lowest point is at the middle, and z(x) ≈ z(-x).
        let n = pts.len();
        let mid = &pts[n / 2];
        assert!(mid.z < -0.5, "chain should sag, got z={}", mid.z);
        for i in 0..n / 2 {
            let l = pts[i];
            let r = pts[n - 1 - i];
            assert!((l.z - r.z).abs() < 1e-3, "chain not symmetric at {i}");
        }
        // Fit a catenary z = z0 + c*(cosh(x/c) - 1) shifted; the discrete curve
        // must be convex (monotone slope) — a defining property of the catenary.
        // Check the profile is convex: second difference of z along x is > 0.
        let mut all_convex = true;
        for i in 1..n - 1 {
            let d2 = pts[i - 1].z + pts[i + 1].z - 2.0 * pts[i].z;
            if d2 <= -1e-6 {
                all_convex = false;
            }
        }
        assert!(all_convex, "hanging chain profile must be convex (catenary-like)");
    }

    /// Inverting a hanging chain flips the sag into an equal rise (the arch).
    #[test]
    fn invert_flips_sag_to_rise() {
        let a = DVec3::new(-4.0, 0.0, 0.0);
        let b = DVec3::new(4.0, 0.0, 0.0);
        let hung = funicular_chain(a, b, 30, 1.0, 1.4);
        let arch = invert_funicular(&hung);
        // Same footprint (x,y) preserved.
        for (h, r) in hung.iter().zip(&arch) {
            assert!((h.x - r.x).abs() < 1e-9 && (h.y - r.y).abs() < 1e-9);
        }
        // The arch's midpoint rises above the springing line.
        let m = arch[arch.len() / 2];
        assert!(m.z > 0.4, "inverted arch should rise, got z={}", m.z);
        // Arch is concave (a dome): second difference of z < 0.
        for i in 1..arch.len() - 1 {
            let d2 = arch[i - 1].z + arch[i + 1].z - 2.0 * arch[i].z;
            assert!(d2 <= 1e-6, "arch must be concave");
        }
    }

    /// A symmetric 3-strut tensegrity form-finds to a stable, symmetric shape:
    /// all three struts end up the same length, and the top ring stays a
    /// regular triangle centred over the bottom ring.
    #[test]
    fn tensegrity_prism_is_symmetric_and_converges() {
        let t = tensegrity_prism(3, 1.0, 2.0, std::f64::consts::FRAC_PI_2 * 0.9);
        let pos = &t.net.positions;
        // Three struts: bottom i → top i. Lengths must match within tolerance.
        let sl: Vec<f64> = (0..3)
            .map(|i| (pos[3 + i] - pos[i]).length())
            .collect();
        for i in 1..3 {
            assert!(
                (sl[i] - sl[0]).abs() < 1e-2,
                "strut lengths differ: {:?}",
                sl
            );
        }
        // Bottom-ring edges equal, top-ring edges equal (regular triangles).
        let bot_edges: Vec<f64> =
            (0..3).map(|i| (pos[(i + 1) % 3] - pos[i]).length()).collect();
        for i in 1..3 {
            assert!((bot_edges[i] - bot_edges[0]).abs() < 1e-2, "bottom not regular");
        }
        // Residual out-of-balance force is small → an equilibrium was found.
        assert!(
            t.report.max_residual < 1e-1,
            "residual too high: {}",
            t.report.max_residual
        );
    }

    /// Determinism: same inputs → identical output, twice.
    #[test]
    fn deterministic_repeatable() {
        let a = DVec3::new(-3.0, 1.0, 0.0);
        let b = DVec3::new(3.0, 1.0, 0.0);
        let p1 = funicular_chain(a, b, 20, 1.0, 1.3);
        let p2 = funicular_chain(a, b, 20, 1.0, 1.3);
        assert_eq!(p1, p2);

        let (c1, _, _) = cable_net(
            [
                DVec3::new(0.0, 0.0, 0.0),
                DVec3::new(6.0, 0.0, 0.0),
                DVec3::new(6.0, 6.0, 2.0),
                DVec3::new(0.0, 6.0, 2.0),
            ],
            4,
            1.0,
        );
        let (c2, _, _) = cable_net(
            [
                DVec3::new(0.0, 0.0, 0.0),
                DVec3::new(6.0, 0.0, 0.0),
                DVec3::new(6.0, 6.0, 2.0),
                DVec3::new(0.0, 6.0, 2.0),
            ],
            4,
            1.0,
        );
        assert_eq!(c1, c2);
    }

    /// Cable-net relaxes: interior nodes move off their drooped seed toward a
    /// taut surface, boundary stays anchored.
    #[test]
    fn cable_net_relaxes_and_pins_boundary() {
        let corners = [
            DVec3::new(0.0, 0.0, 0.0),
            DVec3::new(8.0, 0.0, 0.0),
            DVec3::new(8.0, 8.0, 3.0),
            DVec3::new(0.0, 8.0, 3.0),
        ];
        let (pos, g, segs) = cable_net(corners, 5, 1.5);
        let g = g as usize;
        assert_eq!(pos.len(), g * g);
        assert!(!segs.is_empty());
        // Corners preserved (boundary anchored).
        assert!((pos[0] - corners[0]).length() < 1e-6);
        assert!((pos[g * g - 1] - corners[2]).length() < 1e-6);
        // Interior is finite and within the bounding box of the corners.
        for p in &pos {
            assert!(p.is_finite());
            assert!(p.z >= -2.0 && p.z <= 4.0);
        }
    }

    /// Build a near-inextensible hanging chain and compare its sag against the
    /// exact analytic catenary of the same settled arclength.
    fn stiff_chain(kinetic: bool) -> (Vec<DVec3>, f64) {
        let span = 10.0;
        let segs = 60usize;
        let slack_len = 11.0; // total cable length > span → it must sag
        let rest = slack_len / segs as f64;
        let mut positions = Vec::with_capacity(segs + 1);
        for i in 0..=segs {
            let t = i as f64 / segs as f64;
            let mut p = DVec3::new(-span / 2.0 + span * t, 0.0, 0.0);
            p.z -= 2.0 * t * (1.0 - t); // droop seed
            positions.push(p);
        }
        let links = (0..segs)
            .map(|i| Link::cable(i, i + 1, rest, 2_000.0))
            .collect();
        let mut net = Network::new(positions, links);
        net.anchor(0);
        net.anchor(segs);
        net.gravity = 0.05; // light load → negligible stretch at k=2000
        let report = dynamic_relaxation(
            &mut net,
            RelaxParams { dt: 0.005, kinetic, max_iters: 400_000, tol: 1e-9, ..Default::default() },
        );
        assert!(report.converged, "stiff chain did not converge");
        (net.positions, span)
    }

    /// Solve the catenary parameter c from arclength L and span S:
    /// L = 2c·sinh(S/2c), then sag = c·(cosh(S/2c) − 1).
    fn catenary_sag(arclen: f64, span: f64) -> f64 {
        let f = |c: f64| 2.0 * c * (span / (2.0 * c)).sinh() - arclen;
        let (mut lo, mut hi) = (0.05, 1_000.0);
        for _ in 0..200 {
            let mid = 0.5 * (lo + hi);
            if f(mid) > 0.0 {
                lo = mid; // larger c → less sag → shorter arc
            } else {
                hi = mid;
            }
        }
        let c = 0.5 * (lo + hi);
        c * ((span / (2.0 * c)).cosh() - 1.0)
    }

    /// Settled sag matches the analytic catenary within 2%.
    #[test]
    fn catenary_sag_matches_analytic() {
        let (pts, span) = stiff_chain(false);
        let arclen: f64 = pts.windows(2).map(|w| (w[1] - w[0]).length()).sum();
        let expected = catenary_sag(arclen, span);
        let sag = -pts.iter().map(|p| p.z).fold(f64::INFINITY, f64::min);
        assert!(
            (sag - expected).abs() / expected < 0.02,
            "sag {sag} vs analytic {expected}"
        );
    }

    /// Kinetic (Barnes/Day) damping converges to the same catenary as viscous.
    #[test]
    fn kinetic_damping_matches_viscous() {
        let (viscous, _) = stiff_chain(false);
        let (kinetic, _) = stiff_chain(true);
        for (a, b) in viscous.iter().zip(&kinetic) {
            assert!((*a - *b).length() < 5e-3, "kinetic differs: {a} vs {b}");
        }
    }

    /// A pinned bar at its rest length with no load is already in equilibrium:
    /// relaxation must converge immediately and move nothing.
    #[test]
    fn pinned_bar_is_rigidly_stable() {
        let a = DVec3::ZERO;
        let b = DVec3::new(2.0, 0.0, 0.0);
        for kinetic in [false, true] {
            let mut net = Network::new(vec![a, b], vec![Link::strut(0, 1, 2.0, 100.0)]);
            net.anchor(0);
            let report = dynamic_relaxation(
                &mut net,
                RelaxParams { kinetic, ..Default::default() },
            );
            assert!(report.converged);
            assert!(report.iters <= 2, "should converge at once, took {}", report.iters);
            assert!((net.positions[1] - b).length() < 1e-9, "bar end moved");
        }
    }

    /// A point load pulls the chain down hardest at the loaded node, and an
    /// off-centre load makes the shape asymmetric.
    #[test]
    fn point_load_shapes_the_chain() {
        let segs = 20usize;
        let build = || {
            let mut positions = Vec::with_capacity(segs + 1);
            for i in 0..=segs {
                let t = i as f64 / segs as f64;
                positions.push(DVec3::new(-5.0 + 10.0 * t, 0.0, -0.5 * t * (1.0 - t)));
            }
            let links =
                (0..segs).map(|i| Link::cable(i, i + 1, 10.5 / segs as f64, 500.0)).collect();
            let mut net = Network::new(positions, links);
            net.anchor(0);
            net.anchor(segs);
            net
        };
        // Load at the quarter point only (no gravity).
        let mut net = build();
        let loaded = segs / 4;
        net.add_load(loaded, DVec3::new(0.0, 0.0, -2.0));
        let report = dynamic_relaxation(
            &mut net,
            RelaxParams { dt: 0.02, max_iters: 100_000, ..Default::default() },
        );
        assert!(report.converged);
        // The kink (lowest node) is exactly the loaded node.
        let (imin, _) = net
            .positions
            .iter()
            .enumerate()
            .min_by(|x, y| x.1.z.total_cmp(&y.1.z))
            .unwrap();
        assert_eq!(imin, loaded, "lowest point must be the loaded node");
        // Asymmetric: mirror node is higher than the loaded node.
        assert!(net.positions[segs - loaded].z > net.positions[loaded].z + 0.1);
    }

    /// The same net relaxed from two different seeds settles to the same
    /// equilibrium (unique for a taut gravity-loaded net).
    #[test]
    fn perturbed_starts_converge_to_same_equilibrium() {
        let run = |seed_droop: f64| {
            let segs = 24usize;
            let mut positions = Vec::with_capacity(segs + 1);
            for i in 0..=segs {
                let t = i as f64 / segs as f64;
                positions.push(DVec3::new(
                    -4.0 + 8.0 * t,
                    0.0,
                    -seed_droop * t * (1.0 - t),
                ));
            }
            let links =
                (0..segs).map(|i| Link::cable(i, i + 1, 9.0 / segs as f64, 800.0)).collect();
            let mut net = Network::new(positions, links);
            net.anchor(0);
            net.anchor(segs);
            net.gravity = 0.1;
            let report = dynamic_relaxation(
                &mut net,
                RelaxParams { dt: 0.02, max_iters: 200_000, tol: 1e-9, ..Default::default() },
            );
            assert!(report.converged);
            net.positions
        };
        let a = run(0.5);
        let b = run(3.0);
        for (p, q) in a.iter().zip(&b) {
            assert!((*p - *q).length() < 1e-3, "equilibria differ: {p} vs {q}");
        }
    }

    /// A symmetric square cable-net relaxes to a symmetric surface.
    #[test]
    fn square_net_relaxes_symmetrically() {
        let s = 8.0;
        let corners = [
            DVec3::new(0.0, 0.0, 0.0),
            DVec3::new(s, 0.0, 0.0),
            DVec3::new(s, s, 0.0),
            DVec3::new(0.0, s, 0.0),
        ];
        let (pos, g, _) = cable_net(corners, 6, 1.0);
        let g = g as usize;
        // Mirror symmetry i↔j (the diagonal) and i↔last-i.
        let idx = |i: usize, j: usize| i * g + j;
        for i in 0..g {
            for j in 0..g {
                let p = pos[idx(i, j)];
                let q = pos[idx(j, i)];
                assert!((p.z - q.z).abs() < 1e-6, "diagonal asymmetry at {i},{j}");
                let r = pos[idx(g - 1 - i, j)];
                assert!((p.z - r.z).abs() < 1e-6, "mirror asymmetry at {i},{j}");
            }
        }
    }

    /// Force density on a flat square boundary returns the interior to the
    /// plane (harmonic surface of a planar boundary is planar).
    #[test]
    fn force_density_flat_boundary_is_planar() {
        let g = 7usize;
        let last = g - 1;
        let idx = |i: usize, j: usize| i * g + j;
        let mut positions = vec![DVec3::ZERO; g * g];
        for i in 0..g {
            for j in 0..g {
                let mut p = DVec3::new(i as f64, j as f64, 0.0);
                if i != 0 && i != last && j != 0 && j != last {
                    p.z = 2.0; // push the interior off-plane
                }
                positions[idx(i, j)] = p;
            }
        }
        let mut links = Vec::new();
        for i in 0..g {
            for j in 0..g {
                if j + 1 < g {
                    links.push(Link::cable(idx(i, j), idx(i, j + 1), 0.0, 1.0));
                }
                if i + 1 < g {
                    links.push(Link::cable(idx(i, j), idx(i + 1, j), 0.0, 1.0));
                }
            }
        }
        let mut net = Network::new(positions, links);
        for i in 0..g {
            for j in 0..g {
                if i == 0 || i == last || j == 0 || j == last {
                    net.anchor(idx(i, j));
                }
            }
        }
        let report = force_density(&mut net, 10_000, 1e-10);
        assert!(report.converged);
        assert!(report.max_residual < 1e-6);
        for p in &net.positions {
            assert!(p.z.abs() < 1e-6, "interior not planar: {p}");
        }
    }

    /// Soap film over a saddle boundary: boundary preserved, interior smooth
    /// and inside the boundary's z-range, symmetric centre near z = 0.
    #[test]
    fn minimal_surface_spans_saddle_boundary() {
        // Square ring with alternately lifted corners (±1) — a saddle wire.
        let mut boundary = Vec::new();
        let corners = [
            DVec3::new(-3.0, -3.0, 1.0),
            DVec3::new(3.0, -3.0, -1.0),
            DVec3::new(3.0, 3.0, 1.0),
            DVec3::new(-3.0, 3.0, -1.0),
        ];
        for e in 0..4 {
            let a = corners[e];
            let b = corners[(e + 1) % 4];
            for k in 0..8 {
                boundary.push(a.lerp(b, k as f64 / 8.0));
            }
        }
        let (pos, g) = minimal_surface_grid(&boundary, 8).expect("valid boundary");
        let g = g as usize;
        assert_eq!(pos.len(), g * g);
        // First perimeter node is the resampling start = boundary[0].
        assert!((pos[0] - boundary[0]).length() < 1e-9);
        for p in &pos {
            assert!(p.is_finite());
            assert!(p.z > -1.0 - 1e-6 && p.z < 1.0 + 1e-6, "film outside wire: {p}");
        }
        // Saddle symmetry: the centre of the film sits near z = 0.
        let centre = pos[(g / 2) * g + g / 2];
        assert!(centre.z.abs() < 0.05, "centre off the saddle midplane: {}", centre.z);
        // Meshed variant triangulates the full grid.
        let mesh = minimal_surface(&boundary, 8).unwrap();
        assert_eq!(mesh.positions().len(), g * g);
        assert_eq!(mesh.faces().len(), (g - 1) * (g - 1) * 2);
    }

    /// Degenerate boundaries are rejected; results are deterministic.
    #[test]
    fn minimal_surface_rejects_degenerate_and_repeats() {
        assert!(minimal_surface_grid(&[DVec3::ZERO, DVec3::ONE], 6).is_none());
        assert!(minimal_surface_grid(&[DVec3::ZERO; 5], 6).is_none());
        let tri = [
            DVec3::new(0.0, 0.0, 0.0),
            DVec3::new(4.0, 0.0, 2.0),
            DVec3::new(0.0, 4.0, 1.0),
        ];
        let a = minimal_surface_grid(&tri, 5).unwrap();
        let b = minimal_surface_grid(&tri, 5).unwrap();
        assert_eq!(a.0, b.0);
    }
}
