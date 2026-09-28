// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! Parametric expressive-structure generators: geodesic domes, space frames,
//! hyperbolic-paraboloid (hypar) shells, gaussian brick vaults, and gridshells.
//!
//! These are pure *generative geometry* (form-finding / shape generation), never
//! structural analysis. Every generator returns a single watertight-ish triangle
//! [`Mesh`] in f64 document space so it drops straight into the substrate as one
//! logged, replay-safe object.
//!
//! Struts (dome bars, space-frame diagonals, gridshell laths) are rendered as
//! square-section prisms merged into one mesh via [`strut_lattice`]. Surfaces
//! (hypar, gauss vault) are quad grids triangulated in place.

use glam::DVec3;

use crate::mesh::Mesh;

/// Build one merged mesh of square-section prisms, one per (a, b) strut segment.
/// `thickness` is the side of the square cross-section (meters). Struts with
/// near-zero length are skipped. Nodes are not welded — each strut is its own
/// little prism; this is intentional so the lattice reads as discrete bars.
pub fn strut_lattice(segments: &[(DVec3, DVec3)], thickness: f64) -> Mesh {
    let mut positions: Vec<DVec3> = Vec::new();
    let mut faces: Vec<[u32; 3]> = Vec::new();
    let h = thickness * 0.5;
    for &(a, b) in segments {
        let axis = b - a;
        let len = axis.length();
        if len < 1e-9 {
            continue;
        }
        let axis = axis / len;
        let (u, v) = plane_basis(axis);
        let u = u * h;
        let v = v * h;
        // Eight corners: four at a, four at b.
        let base = positions.len() as u32;
        for &c in &[a, b] {
            positions.push(c - u - v);
            positions.push(c + u - v);
            positions.push(c + u + v);
            positions.push(c - u + v);
        }
        // Faces: 0..3 = start ring, 4..7 = end ring. Quads → two tris each,
        // wound outward.
        let q = |a: u32, b: u32, c: u32, d: u32, out: &mut Vec<[u32; 3]>| {
            out.push([base + a, base + b, base + c]);
            out.push([base + a, base + c, base + d]);
        };
        // start cap (facing -axis): 0,3,2,1
        q(0, 3, 2, 1, &mut faces);
        // end cap (facing +axis): 4,5,6,7
        q(4, 5, 6, 7, &mut faces);
        // sides
        q(0, 1, 5, 4, &mut faces);
        q(1, 2, 6, 5, &mut faces);
        q(2, 3, 7, 6, &mut faces);
        q(3, 0, 4, 7, &mut faces);
    }
    Mesh::new(positions, faces)
}

/// An orthonormal (u, v) pair perpendicular to `axis` (assumed unit length).
fn plane_basis(axis: DVec3) -> (DVec3, DVec3) {
    let seed = if axis.x.abs() < 0.9 { DVec3::X } else { DVec3::Y };
    let u = seed.cross(axis).normalize();
    let v = axis.cross(u);
    (u, v)
}

// ── geodesic dome / sphere ──────────────────────────────────────────────────

/// Geodesic node+strut network from an icosahedron subdivided `frequency` times
/// then projected to a sphere of `radius`. `dome` keeps only the upper
/// hemisphere (z ≥ 0). Returns the unique struts (edges) as segment pairs and
/// the unique projected node positions.
///
/// The math: start from the 12 icosahedron vertices; each of the 20 triangular
/// faces is subdivided into `frequency²` small triangles by barycentric
/// interpolation; every generated point is normalized to the sphere. Edges are
/// deduplicated on a quantized-key basis. This is the Buckminster Fuller
/// geodesic construction (Class I / alternate breakdown).
pub fn geodesic_network(
    frequency: u32,
    radius: f64,
    dome: bool,
) -> (Vec<DVec3>, Vec<(DVec3, DVec3)>) {
    let f = frequency.max(1);
    let ico = icosahedron();
    // Quantized-key dedup for vertices and edges.
    let mut nodes: Vec<DVec3> = Vec::new();
    let mut node_key: std::collections::HashMap<[i64; 3], usize> = std::collections::HashMap::new();
    let mut edge_set: std::collections::HashSet<(usize, usize)> = std::collections::HashSet::new();
    let quant = |p: DVec3| -> [i64; 3] {
        [
            (p.x * 1e6).round() as i64,
            (p.y * 1e6).round() as i64,
            (p.z * 1e6).round() as i64,
        ]
    };
    let intern = |p: DVec3,
                  nodes: &mut Vec<DVec3>,
                  node_key: &mut std::collections::HashMap<[i64; 3], usize>|
     -> usize {
        let unit = p.normalize();
        let k = quant(unit);
        if let Some(&i) = node_key.get(&k) {
            i
        } else {
            let i = nodes.len();
            nodes.push(unit * radius);
            node_key.insert(k, i);
            i
        }
    };
    for tri in ico.1 {
        let (a, b, c) = (ico.0[tri[0]], ico.0[tri[1]], ico.0[tri[2]]);
        // Barycentric grid of the subdivided face.
        let mut grid: Vec<Vec<usize>> = Vec::with_capacity((f + 1) as usize);
        for i in 0..=f {
            let mut row = Vec::with_capacity((f - i + 1) as usize);
            for j in 0..=(f - i) {
                let k = f - i - j;
                let (wi, wj, wk) = (i as f64, j as f64, k as f64);
                let p = (a * wk + b * wj + c * wi) / f as f64;
                row.push(intern(p, &mut nodes, &mut node_key));
            }
            grid.push(row);
        }
        // Small-triangle edges: connect grid neighbors.
        for i in 0..f as usize {
            for j in 0..grid[i].len() - 1 {
                let add = |x: usize, y: usize, set: &mut std::collections::HashSet<(usize, usize)>| {
                    set.insert((x.min(y), x.max(y)));
                };
                // horizontal
                add(grid[i][j], grid[i][j + 1], &mut edge_set);
                // to next row (two diagonals of the upward triangle)
                add(grid[i][j], grid[i + 1][j], &mut edge_set);
                add(grid[i][j + 1], grid[i + 1][j], &mut edge_set);
            }
        }
    }
    // Filter to a dome (upper hemisphere) if requested. An edge survives only if
    // both endpoints are on/above the equator.
    let eps = radius * 1e-6;
    let mut segments: Vec<(DVec3, DVec3)> = Vec::new();
    for &(i, j) in &edge_set {
        let (pi, pj) = (nodes[i], nodes[j]);
        if dome && (pi.z < -eps || pj.z < -eps) {
            continue;
        }
        segments.push((pi, pj));
    }
    let out_nodes: Vec<DVec3> = if dome {
        nodes.into_iter().filter(|p| p.z >= -eps).collect()
    } else {
        nodes
    };
    segments.sort_by(seg_cmp);
    (out_nodes, segments)
}

fn seg_cmp(a: &(DVec3, DVec3), b: &(DVec3, DVec3)) -> std::cmp::Ordering {
    let ka = (a.0 + a.1) * 0.5;
    let kb = (b.0 + b.1) * 0.5;
    ka.x
        .partial_cmp(&kb.x)
        .unwrap_or(std::cmp::Ordering::Equal)
        .then(ka.y.partial_cmp(&kb.y).unwrap_or(std::cmp::Ordering::Equal))
        .then(ka.z.partial_cmp(&kb.z).unwrap_or(std::cmp::Ordering::Equal))
}

/// Unit icosahedron: 12 vertices, 20 triangular faces.
fn icosahedron() -> (Vec<DVec3>, Vec<[usize; 3]>) {
    let t = (1.0 + 5.0_f64.sqrt()) * 0.5; // golden ratio
    let mut v = vec![
        DVec3::new(-1.0, t, 0.0),
        DVec3::new(1.0, t, 0.0),
        DVec3::new(-1.0, -t, 0.0),
        DVec3::new(1.0, -t, 0.0),
        DVec3::new(0.0, -1.0, t),
        DVec3::new(0.0, 1.0, t),
        DVec3::new(0.0, -1.0, -t),
        DVec3::new(0.0, 1.0, -t),
        DVec3::new(t, 0.0, -1.0),
        DVec3::new(t, 0.0, 1.0),
        DVec3::new(-t, 0.0, -1.0),
        DVec3::new(-t, 0.0, 1.0),
    ];
    for p in &mut v {
        *p = p.normalize();
    }
    let f = vec![
        [0, 11, 5],
        [0, 5, 1],
        [0, 1, 7],
        [0, 7, 10],
        [0, 10, 11],
        [1, 5, 9],
        [5, 11, 4],
        [11, 10, 2],
        [10, 7, 6],
        [7, 1, 8],
        [3, 9, 4],
        [3, 4, 2],
        [3, 2, 6],
        [3, 6, 8],
        [3, 8, 9],
        [4, 9, 5],
        [2, 4, 11],
        [6, 2, 10],
        [8, 6, 7],
        [9, 8, 1],
    ];
    (v, f)
}

// ── space frame (double-layer grid) ─────────────────────────────────────────

/// Double-layer space-frame lattice. A `nx × ny` grid of top chords at z =
/// `depth`, a matching bottom grid offset by half a bay at z = 0, and pyramid
/// diagonals connecting each bottom node up to the four surrounding top nodes.
/// `bay` is the module spacing (meters). Returns the strut segments.
///
/// This is the classic octet / offset double-layer grid: the top layer sits on a
/// full `(nx+1)×(ny+1)` node grid; the bottom layer sits on the `nx×ny` cell
/// centers, one module below, and each bottom node ties to its four top
/// neighbors, giving the characteristic tetrahedral triangulation.
pub fn spaceframe_struts(nx: u32, ny: u32, bay: f64, depth: f64) -> Vec<(DVec3, DVec3)> {
    let nx = nx.max(1);
    let ny = ny.max(1);
    let top = |i: u32, j: u32| DVec3::new(i as f64 * bay, j as f64 * bay, depth);
    let bot = |i: u32, j: u32| {
        DVec3::new((i as f64 + 0.5) * bay, (j as f64 + 0.5) * bay, 0.0)
    };
    let mut segs: Vec<(DVec3, DVec3)> = Vec::new();
    // Top chords (grid of (nx+1)×(ny+1) nodes).
    for i in 0..=nx {
        for j in 0..=ny {
            if i < nx {
                segs.push((top(i, j), top(i + 1, j)));
            }
            if j < ny {
                segs.push((top(i, j), top(i, j + 1)));
            }
        }
    }
    // Bottom chords (nx×ny cell-center grid).
    for i in 0..nx {
        for j in 0..ny {
            if i + 1 < nx {
                segs.push((bot(i, j), bot(i + 1, j)));
            }
            if j + 1 < ny {
                segs.push((bot(i, j), bot(i, j + 1)));
            }
        }
    }
    // Diagonals: each bottom node ties to its four surrounding top nodes.
    for i in 0..nx {
        for j in 0..ny {
            let b = bot(i, j);
            segs.push((b, top(i, j)));
            segs.push((b, top(i + 1, j)));
            segs.push((b, top(i + 1, j + 1)));
            segs.push((b, top(i, j + 1)));
        }
    }
    segs
}

// ── diagrid (planar diagonal facade grid) ────────────────────────────────────

/// Planar diagonal grid ("diagrid") over a `width × height` rectangle in the XY
/// plane (z = 0), divided into `nx × ny` cells. Emits BOTH diagonal families
/// (the "/" and "\" diagonals of every cell) forming the classic diamond diagrid
/// mesh, plus the four perimeter edges of the rectangle. Returns the segments.
///
/// Plane choice: XY (z = 0). The rectangle spans x∈[0,width], y∈[0,height]; the
/// origin corner is at (0,0,0). Segment count = `2·nx·ny` diagonals + `4`
/// perimeter edges. (Deterministic: cells are visited in row-major order.)
pub fn diagrid_segments(nx: u32, ny: u32, width: f64, height: f64) -> Vec<(DVec3, DVec3)> {
    let nx = nx.max(1);
    let ny = ny.max(1);
    let cw = width / nx as f64;
    let ch = height / ny as f64;
    let p = |i: u32, j: u32| DVec3::new(i as f64 * cw, j as f64 * ch, 0.0);
    let mut segs: Vec<(DVec3, DVec3)> = Vec::new();
    // Both diagonal families per cell.
    for i in 0..nx {
        for j in 0..ny {
            // "/" diagonal: bottom-left → top-right.
            segs.push((p(i, j), p(i + 1, j + 1)));
            // "\" diagonal: top-left → bottom-right.
            segs.push((p(i, j + 1), p(i + 1, j)));
        }
    }
    // Perimeter (bottom, right, top, left).
    segs.push((p(0, 0), p(nx, 0)));
    segs.push((p(nx, 0), p(nx, ny)));
    segs.push((p(nx, ny), p(0, ny)));
    segs.push((p(0, ny), p(0, 0)));
    segs
}

// ── reciprocal frame (rotational fan) ────────────────────────────────────────

/// Reciprocal frame: `count` straight members arranged in a rotational fan
/// around the origin. Each member's endpoints sit on a pitch circle of `radius`
/// but are rotated tangentially by an `engagement` angle so consecutive members
/// mutually overlap, leaving the characteristic central polygon opening and an
/// outer ring. Returns the `count` member segments (in the XY plane, z = 0).
///
/// Engagement rule: member `k` (k = 0..count) starts at angle `θ = k·2π/count`
/// on the pitch circle and ends at `θ + Δ` where the engagement offset
/// `Δ = 2π/count` (one full pitch — each member spans to its neighbor's start
/// point, so members lap over one another and the inner ends define the central
/// opening). Member length is then scaled to `length`: the raw chord from the
/// start point along the (end−start) direction is normalized and extended to the
/// requested `length`. Deterministic (equal angular spacing, fixed member order).
pub fn reciprocal_segments(count: u32, radius: f64, length: f64) -> Vec<(DVec3, DVec3)> {
    let count = count.max(2);
    let n = count as f64;
    let step = std::f64::consts::TAU / n;
    // Engagement: each member laps to the next station's pitch point.
    let engage = step;
    let mut segs: Vec<(DVec3, DVec3)> = Vec::new();
    for k in 0..count {
        let a0 = k as f64 * step;
        let a1 = a0 + engage;
        let start = DVec3::new(radius * a0.cos(), radius * a0.sin(), 0.0);
        let pitch_end = DVec3::new(radius * a1.cos(), radius * a1.sin(), 0.0);
        let dir = pitch_end - start;
        let len = dir.length();
        let end = if len < 1e-9 {
            pitch_end
        } else {
            start + dir / len * length
        };
        segs.push((start, end));
    }
    segs
}

// ── waffle (egg-crate rib grid) ──────────────────────────────────────────────

/// Egg-crate / "waffle" grid: `nx` ribs running along Y and `ny` ribs running
/// along X, over a `width × length` footprint, each rib a vertical plane of
/// `depth` (top at z = `depth`, bottom at z = 0). As lines it returns the rib
/// TOP edges and BOTTOM edges (so the grid reads at two levels) plus the
/// vertical edges at every rib intersection — a full 3D grid of line segments.
///
/// Layout: X ribs sit at x = i·width/(nx−1) for i∈0..nx (spanning y∈[0,length]);
/// Y ribs at y = j·length/(ny−1) for j∈0..ny (spanning x∈[0,width]). Segment
/// count = 2·nx (X-rib top+bottom edges) + 2·ny (Y-rib top+bottom edges) +
/// nx·ny verticals at the intersection lattice. Deterministic.
pub fn waffle_segments(
    nx: u32,
    ny: u32,
    width: f64,
    length: f64,
    depth: f64,
) -> Vec<(DVec3, DVec3)> {
    let nx = nx.max(2);
    let ny = ny.max(2);
    let xs = |i: u32| width * i as f64 / (nx - 1) as f64;
    let ys = |j: u32| length * j as f64 / (ny - 1) as f64;
    let mut segs: Vec<(DVec3, DVec3)> = Vec::new();
    // X ribs (constant x, spanning y): top edge at z=depth, bottom at z=0.
    for i in 0..nx {
        let x = xs(i);
        segs.push((DVec3::new(x, 0.0, depth), DVec3::new(x, length, depth)));
        segs.push((DVec3::new(x, 0.0, 0.0), DVec3::new(x, length, 0.0)));
    }
    // Y ribs (constant y, spanning x): top and bottom edges.
    for j in 0..ny {
        let y = ys(j);
        segs.push((DVec3::new(0.0, y, depth), DVec3::new(width, y, depth)));
        segs.push((DVec3::new(0.0, y, 0.0), DVec3::new(width, y, 0.0)));
    }
    // Vertical edges at every rib intersection.
    for i in 0..nx {
        for j in 0..ny {
            let (x, y) = (xs(i), ys(j));
            segs.push((DVec3::new(x, y, 0.0), DVec3::new(x, y, depth)));
        }
    }
    segs
}

// ── voronoi shell (planar cell pattern) ──────────────────────────────────────

/// A deterministic splitmix64 PRNG — same construction the subdivision Voronoi
/// generator uses, so seed placement is reproducible with NO `Math.random`.
fn splitmix64(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Next f64 in [0,1) from the splitmix64 stream (53-bit mantissa).
fn next_unit(state: &mut u64) -> f64 {
    (splitmix64(state) >> 11) as f64 / (1u64 << 53) as f64
}

/// Circumcenter of a 2D triangle, or `None` if (near-)collinear.
fn circumcenter2(a: glam::DVec2, b: glam::DVec2, c: glam::DVec2) -> Option<glam::DVec2> {
    let d = 2.0 * (a.x * (b.y - c.y) + b.x * (c.y - a.y) + c.x * (a.y - b.y));
    if d.abs() < 1e-12 {
        return None;
    }
    let a2 = a.length_squared();
    let b2 = b.length_squared();
    let c2 = c.length_squared();
    let ux = (a2 * (b.y - c.y) + b2 * (c.y - a.y) + c2 * (a.y - b.y)) / d;
    let uy = (a2 * (c.x - b.x) + b2 * (a.x - c.x) + c2 * (b.x - a.x)) / d;
    Some(glam::DVec2::new(ux, uy))
}

/// Liang–Barsky clip of segment `a`→`b` to the axis-aligned box [lo, hi].
/// Returns the clipped endpoints (both on/inside the box) or `None` if the whole
/// segment lies outside. Keeps distant circumcenters of sliver triangles from
/// blowing the pattern up to infinity.
fn clip_seg_box(
    a: glam::DVec2,
    b: glam::DVec2,
    lo: glam::DVec2,
    hi: glam::DVec2,
) -> Option<(glam::DVec2, glam::DVec2)> {
    let d = b - a;
    let mut t0 = 0.0_f64;
    let mut t1 = 1.0_f64;
    let checks = [(-d.x, a.x - lo.x), (d.x, hi.x - a.x), (-d.y, a.y - lo.y), (d.y, hi.y - a.y)];
    for (num_dir, num_dist) in checks {
        if num_dir.abs() < 1e-18 {
            if num_dist < 0.0 {
                return None;
            }
        } else {
            let t = num_dist / num_dir;
            if num_dir < 0.0 {
                if t > t1 {
                    return None;
                }
                if t > t0 {
                    t0 = t;
                }
            } else {
                if t < t0 {
                    return None;
                }
                if t < t1 {
                    t1 = t;
                }
            }
        }
    }
    Some((a + d * t0, a + d * t1))
}

/// A **Voronoi shell** cell pattern over a `width × length` rectangle in the XY
/// plane (z = 0). `cells` DETERMINISTIC seed points are scattered by a
/// splitmix64 PRNG keyed off `seed` (reproducible, no `Math.random`), their
/// Delaunay triangulation is built (reusing `kernel_mesh::triangulate`, the same
/// core the subdivision Voronoi generator uses), and the Voronoi cell EDGES —
/// the dual: segments joining circumcenters of triangles sharing a Delaunay edge
/// — are emitted, each clipped to the rectangle. Returns the cell-edge segments.
///
/// Robust to degenerate/collinear seeds: collinear circumcenters are skipped and
/// duplicate seeds are jittered apart, so a bad seed set yields fewer edges
/// rather than a panic. Deterministic for a fixed `(cells, width, length, seed)`.
pub fn voronoishell_segments(
    cells: u32,
    width: f64,
    length: f64,
    seed: i64,
) -> Vec<(DVec3, DVec3)> {
    use glam::DVec2;
    let cells = cells.max(1);
    let width = width.abs().max(1e-6);
    let length = length.abs().max(1e-6);
    let lo = DVec2::ZERO;
    let hi = DVec2::new(width, length);

    // Deterministic seed scatter. Mix the user seed into the splitmix state so
    // different seeds move the sites reproducibly.
    let mut state = (seed as u64).wrapping_mul(0x2545_F491_4F6C_DD1D) ^ 0x566F_726F_5368_6C31;
    let mut sites: Vec<DVec2> = Vec::with_capacity(cells as usize);
    for _ in 0..cells {
        let x = next_unit(&mut state) * width;
        let y = next_unit(&mut state) * length;
        sites.push(DVec2::new(x, y));
    }
    // De-duplicate coincident sites (would make triangulate degenerate): nudge
    // any site landing on an existing one by a deterministic epsilon.
    let quant = |p: DVec2| -> (i64, i64) {
        ((p.x * 1e6).round() as i64, (p.y * 1e6).round() as i64)
    };
    let mut seen: std::collections::HashSet<(i64, i64)> = std::collections::HashSet::new();
    for s in &mut sites {
        let mut guard = 0;
        while !seen.insert(quant(*s)) && guard < 8 {
            s.x = (s.x + 1e-3).min(width);
            s.y = (s.y + 1e-3).min(length);
            guard += 1;
        }
    }
    if sites.len() < 3 {
        return Vec::new();
    }

    let tris = crate::triangulate(&sites);
    if tris.is_empty() {
        return Vec::new();
    }
    let ccs: Vec<Option<DVec2>> = tris
        .iter()
        .map(|t| {
            circumcenter2(
                sites[t[0] as usize],
                sites[t[1] as usize],
                sites[t[2] as usize],
            )
        })
        .collect();

    // Map undirected Delaunay edge → owning triangles; the dual of an interior
    // edge (2 owners) is the finite segment joining their circumcenters.
    let mut edge_tris: std::collections::HashMap<(u32, u32), Vec<usize>> =
        std::collections::HashMap::new();
    for (ti, t) in tris.iter().enumerate() {
        for e in [[t[0], t[1]], [t[1], t[2]], [t[2], t[0]]] {
            let ek = if e[0] <= e[1] { (e[0], e[1]) } else { (e[1], e[0]) };
            edge_tris.entry(ek).or_default().push(ti);
        }
    }
    // Deterministic emission: sort edges, dedup output segments.
    let mut shared: Vec<((u32, u32), Vec<usize>)> = edge_tris.into_iter().collect();
    shared.sort_by_key(|(k, _)| *k);

    let edge_key = |a: DVec2, b: DVec2| -> ((i64, i64), (i64, i64)) {
        let (ka, kb) = (quant(a), quant(b));
        if ka <= kb {
            (ka, kb)
        } else {
            (kb, ka)
        }
    };
    let mut seen_edges: std::collections::HashSet<((i64, i64), (i64, i64))> =
        std::collections::HashSet::new();
    let mut segs: Vec<(DVec3, DVec3)> = Vec::new();
    for (_ek, owners) in &shared {
        if owners.len() != 2 {
            continue; // hull edges: unbounded ray, dropped for the shell pattern
        }
        let (Some(p), Some(q)) = (ccs[owners[0]], ccs[owners[1]]) else {
            continue;
        };
        let Some((p, q)) = clip_seg_box(p, q, lo, hi) else {
            continue;
        };
        if p.distance_squared(q) < 1e-12 {
            continue;
        }
        if seen_edges.insert(edge_key(p, q)) {
            segs.push((DVec3::new(p.x, p.y, 0.0), DVec3::new(q.x, q.y, 0.0)));
        }
    }
    segs.sort_by(seg_cmp);
    segs
}

// ── Schwedler ribbed dome ─────────────────────────────────────────────────────

/// A **Schwedler dome**: `meridians` meridional ribs from apex to base, `rings`
/// latitude rings, plus one diagonal brace per quadrilateral panel (the classic
/// Schwedler bracing that triangulates each cell). `dome` keeps the upper
/// hemisphere; otherwise the whole sphere is built. Returns the member segments
/// on a sphere of `radius`.
///
/// Parameterization: latitude `φ` runs from the apex (`φ = π/2`, north pole) down
/// to the base. For a dome the base is the equator (`φ = 0`); for a full sphere
/// it continues to the south pole (`φ = -π/2`). Longitude `θ = m·2π/meridians`.
/// Node `(r, m)` sits at ring `r ∈ 0..=rings` and meridian `m ∈ 0..meridians`.
/// Segment count is deterministic: `meridians·rings` meridional bars +
/// `meridians·(rings-?)` ring bars + `meridians·(rings-?)` diagonals (see test).
pub fn schwedler_segments(
    meridians: u32,
    rings: u32,
    radius: f64,
    dome: bool,
) -> Vec<(DVec3, DVec3)> {
    let m = meridians.max(3);
    let rings = rings.max(1);
    let r = radius.abs().max(1e-6);
    // Latitude at ring index i (0 = apex). Dome spans [π/2 .. 0]; full [π/2 .. -π/2].
    let top = std::f64::consts::FRAC_PI_2;
    let bot = if dome { 0.0 } else { -std::f64::consts::FRAC_PI_2 };
    let lat = |i: u32| top + (bot - top) * (i as f64 / rings as f64);
    let node = |ring: u32, mer: u32| -> DVec3 {
        let phi = lat(ring);
        let theta = std::f64::consts::TAU * (mer % m) as f64 / m as f64;
        let cp = phi.cos();
        DVec3::new(r * cp * theta.cos(), r * cp * theta.sin(), r * phi.sin())
    };
    let mut segs: Vec<(DVec3, DVec3)> = Vec::new();
    // Ring 0 is the apex (single point): all meridians coincide there. To avoid
    // duplicate zero-length bars we treat ring 0 as the pole and connect it once
    // per meridian to ring 1.
    let apex = node(0, 0);
    // Meridional ribs: apex→ring1 per meridian, then ring i→ring i+1.
    for mer in 0..m {
        segs.push((apex, node(1, mer)));
        for ring in 1..rings {
            segs.push((node(ring, mer), node(ring + 1, mer)));
        }
    }
    // Latitude rings (skip the apex ring 0). Each ring r∈1..=rings is a closed
    // polygon of `m` bars.
    for ring in 1..=rings {
        for mer in 0..m {
            segs.push((node(ring, mer), node(ring, mer + 1)));
        }
    }
    // Schwedler diagonals: one per quadrilateral panel between ring r and r+1
    // (the panel bounded by meridians mer, mer+1). Brace corner (r,mer)→(r+1,mer+1).
    // The apex cap (ring 0→1) is already triangular (meets at the pole), so
    // diagonals start at ring 1.
    for ring in 1..rings {
        for mer in 0..m {
            segs.push((node(ring, mer), node(ring + 1, mer + 1)));
        }
    }
    segs.sort_by(seg_cmp);
    segs
}

// ── hyperbolic paraboloid (hypar) surface ───────────────────────────────────

/// Ruled hyperbolic-paraboloid (Candela) surface `z = x*y/c` sampled over the
/// rectangle `[-a, a] × [-b, b]` on a `(nu+1)×(nv+1)` grid, triangulated into a
/// single-sided mesh. This is the doubly-ruled anticlastic (saddle) shell.
pub fn hypar_surface(a: f64, b: f64, c: f64, nu: u32, nv: u32) -> Mesh {
    let nu = nu.max(1);
    let nv = nv.max(1);
    let cc = if c.abs() < 1e-12 { 1.0 } else { c };
    let mut positions = Vec::with_capacity(((nu + 1) * (nv + 1)) as usize);
    for i in 0..=nu {
        let x = -a + 2.0 * a * i as f64 / nu as f64;
        for j in 0..=nv {
            let y = -b + 2.0 * b * j as f64 / nv as f64;
            positions.push(DVec3::new(x, y, x * y / cc));
        }
    }
    let idx = |i: u32, j: u32| i * (nv + 1) + j;
    let mut faces = Vec::with_capacity((nu * nv * 2) as usize);
    for i in 0..nu {
        for j in 0..nv {
            let a0 = idx(i, j);
            let a1 = idx(i + 1, j);
            let a2 = idx(i + 1, j + 1);
            let a3 = idx(i, j + 1);
            faces.push([a0, a1, a2]);
            faces.push([a0, a2, a3]);
        }
    }
    Mesh::new(positions, faces)
}

// ── gaussian brick vault (Dieste) ───────────────────────────────────────────

/// Doubly-curved catenary brick vault (Eladio Dieste). A catenary arch of
/// horizontal `span` and `rise` is swept along the length `L`; when
/// `undulate` is true the springing line follows a sinusoidal directrix
/// (the signature undulating wall/vault), giving gaussian double curvature.
/// Returns a `(nu+1)×(nv+1)` triangulated surface mesh spanning x∈[0,span],
/// y∈[0,L].
///
/// Catenary section: `z(u) = rise * (cosh(k(2u-1)) - cosh(k)) / (1 - cosh(k))`
/// with `u∈[0,1]` across the span and shape factor `k` (default 1.6 gives a
/// natural funicular arch). The undulation adds `amp*sin(2π·y/L·waves)` to the
/// vault height so the crest snakes along its length — the double curvature that
/// lets thin brick shells stand without formwork.
pub fn gaussvault_surface(
    span: f64,
    length: f64,
    rise: f64,
    nu: u32,
    nv: u32,
    undulate: bool,
) -> Mesh {
    let nu = nu.max(1);
    let nv = nv.max(1);
    let k = 1.6_f64;
    let denom = 1.0 - k.cosh();
    let section = |u: f64| -> f64 {
        // u in [0,1]; 0 and 1 at the springings (z=0), max at u=0.5.
        let x = 2.0 * u - 1.0;
        rise * ((k * x).cosh() - k.cosh()) / denom
    };
    let waves = 2.0;
    let amp = if undulate { rise * 0.25 } else { 0.0 };
    let mut positions = Vec::with_capacity(((nu + 1) * (nv + 1)) as usize);
    for j in 0..=nv {
        let tv = j as f64 / nv as f64;
        let y = length * tv;
        let und = amp * (std::f64::consts::TAU * waves * tv).sin();
        for i in 0..=nu {
            let tu = i as f64 / nu as f64;
            let x = span * tu;
            // Undulation scales the section so springings stay on the ground.
            let z = section(tu) + und * (section(tu) / rise.max(1e-9));
            positions.push(DVec3::new(x, y, z));
        }
    }
    let idx = |i: u32, j: u32| j * (nu + 1) + i;
    let mut faces = Vec::with_capacity((nu * nv * 2) as usize);
    for j in 0..nv {
        for i in 0..nu {
            let a0 = idx(i, j);
            let a1 = idx(i + 1, j);
            let a2 = idx(i + 1, j + 1);
            let a3 = idx(i, j + 1);
            faces.push([a0, a1, a2]);
            faces.push([a0, a2, a3]);
        }
    }
    Mesh::new(positions, faces)
}

// ── catenary vault (compression arch, hanging-chain inverted) ────────────────

/// Solve the catenary shape parameter `a` for the profile `y = a·cosh(x/a)` such
/// that a chain hung between `x = ±span/2` has SAG equal to `rise`, i.e.
/// `rise = a·(cosh(span/(2a)) − 1)`. Inverted, this is the pure-compression
/// arch of the given `span` and crown `rise` — a real catenary, NOT a parabola.
/// Solved by monotone bisection (sag decreases as `a` grows). Deterministic.
fn catenary_param(span: f64, rise: f64) -> f64 {
    let s = span.abs().max(1e-9);
    let h = rise.abs().max(1e-9);
    // sag(a) = a·(cosh(s/(2a)) − 1); monotonically DECREASING in a.
    let sag = |a: f64| a * ((s / (2.0 * a)).cosh() - 1.0);
    let (mut lo, mut hi) = (1e-4, 1e6);
    for _ in 0..200 {
        let mid = 0.5 * (lo + hi);
        if sag(mid) > h {
            lo = mid; // too much sag → need larger a
        } else {
            hi = mid;
        }
    }
    0.5 * (lo + hi)
}

/// A **catenary vault**: a compression arch whose CROSS-SECTION is a true
/// catenary (a hanging chain, inverted) of horizontal `span` and crown `rise`,
/// lofted along `length`. Returns a `(nu+1)×(nv+1)` triangulated SURFACE mesh
/// spanning x∈[0,span], y∈[0,length]; its natural edges are the vault grid the
/// renderer draws as a wireframe.
///
/// Section: with catenary parameter `a` from [`catenary_param`], the inverted
/// arch height at fractional span `u∈[0,1]` is
/// `z(u) = rise − a·(cosh((2u−1)·span/(2a)) − 1)`, so `z(0)=z(1)=0` (springings
/// on the ground) and `z(0.5)=rise` (crown). This is a funicular catenary, whose
/// second derivative grows toward the springings — distinct from a parabola.
pub fn catenary_vault_surface(span: f64, length: f64, rise: f64, nu: u32, nv: u32) -> Mesh {
    let nu = nu.max(1);
    let nv = nv.max(1);
    let span = span.abs().max(1e-6);
    let length = length.abs().max(1e-6);
    let rise = rise.abs().max(1e-9);
    let a = catenary_param(span, rise);
    let section = |u: f64| -> f64 {
        // Inverted hanging chain: 0 at the springings, `rise` at the crown.
        let x = (2.0 * u - 1.0) * span * 0.5;
        rise - a * ((x / a).cosh() - 1.0)
    };
    let mut positions = Vec::with_capacity(((nu + 1) * (nv + 1)) as usize);
    for j in 0..=nv {
        let y = length * j as f64 / nv as f64;
        for i in 0..=nu {
            let u = i as f64 / nu as f64;
            positions.push(DVec3::new(span * u, y, section(u)));
        }
    }
    let idx = |i: u32, j: u32| j * (nu + 1) + i;
    let mut faces = Vec::with_capacity((nu * nv * 2) as usize);
    for j in 0..nv {
        for i in 0..nu {
            let a0 = idx(i, j);
            let a1 = idx(i + 1, j);
            let a2 = idx(i + 1, j + 1);
            let a3 = idx(i, j + 1);
            faces.push([a0, a1, a2]);
            faces.push([a0, a2, a3]);
        }
    }
    Mesh::new(positions, faces)
}

// ── gridshell ───────────────────────────────────────────────────────────────

/// Which doubly-curved surface a gridshell lattice rides on.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum GridshellSurface {
    /// Hypar `z = x*y/c` over `[-a,a]×[-b,b]`.
    Hypar { a: f64, b: f64, c: f64 },
    /// Gauss catenary vault over `[0,span]×[0,length]`.
    Vault {
        span: f64,
        length: f64,
        rise: f64,
        undulate: bool,
    },
}

impl GridshellSurface {
    /// Evaluate the surface at grid indices (i, j) over an `nu × nv` division.
    fn point(&self, i: u32, j: u32, nu: u32, nv: u32) -> DVec3 {
        let tu = i as f64 / nu as f64;
        let tv = j as f64 / nv as f64;
        match *self {
            GridshellSurface::Hypar { a, b, c } => {
                let cc = if c.abs() < 1e-12 { 1.0 } else { c };
                let x = -a + 2.0 * a * tu;
                let y = -b + 2.0 * b * tv;
                DVec3::new(x, y, x * y / cc)
            }
            GridshellSurface::Vault { span, length, rise, undulate } => {
                let k = 1.6_f64;
                let denom = 1.0 - k.cosh();
                let sec = |u: f64| rise * ((k * (2.0 * u - 1.0)).cosh() - k.cosh()) / denom;
                let amp = if undulate { rise * 0.25 } else { 0.0 };
                let und = amp * (std::f64::consts::TAU * 2.0 * tv).sin();
                let z = sec(tu) + und * (sec(tu) / rise.max(1e-9));
                DVec3::new(span * tu, length * tv, z)
            }
        }
    }
}

/// A lattice of laths on a doubly-curved surface: the two families of UV grid
/// lines (u-direction and v-direction members) rendered as square-section
/// struts of side `thickness`. This is the gridshell — a reciprocal net of
/// slender members that gets its stiffness from the double curvature.
pub fn gridshell(surface: GridshellSurface, nu: u32, nv: u32, thickness: f64) -> Mesh {
    strut_lattice(&gridshell_segments(surface, nu, nv), thickness)
}

/// The gridshell's member segments (the two UV families of grid lines) as raw
/// `(start, end)` pairs — the same members `gridshell` sweeps into strut tubes,
/// but returned as lightweight lines for a wireframe render. `u`-members run in
/// the u direction (constant j), then `v`-members (constant i), so the ordering
/// matches `gridshell`'s tube meshing.
pub fn gridshell_segments(surface: GridshellSurface, nu: u32, nv: u32) -> Vec<(DVec3, DVec3)> {
    let nu = nu.max(1);
    let nv = nv.max(1);
    let p = |i: u32, j: u32| surface.point(i, j, nu, nv);
    let mut segs: Vec<(DVec3, DVec3)> = Vec::new();
    // u-direction members (constant j).
    for j in 0..=nv {
        for i in 0..nu {
            segs.push((p(i, j), p(i + 1, j)));
        }
    }
    // v-direction members (constant i).
    for i in 0..=nu {
        for j in 0..nv {
            segs.push((p(i, j), p(i, j + 1)));
        }
    }
    segs
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn icosahedron_has_12_nodes_20_faces() {
        let (v, f) = icosahedron();
        assert_eq!(v.len(), 12);
        assert_eq!(f.len(), 20);
        for p in &v {
            assert!((p.length() - 1.0).abs() < 1e-9);
        }
    }

    #[test]
    fn geodesic_freq1_full_is_icosahedron() {
        // Frequency 1 full sphere = the raw icosahedron: 12 nodes, 30 edges.
        let (nodes, segs) = geodesic_network(1, 1.0, false);
        assert_eq!(nodes.len(), 12);
        assert_eq!(segs.len(), 30);
        for &(a, b) in &segs {
            assert!((a.length() - 1.0).abs() < 1e-6);
            assert!((b.length() - 1.0).abs() < 1e-6);
        }
    }

    #[test]
    fn geodesic_freq_scales_nodes_and_edges() {
        // Class-I geodesic: V = 10f²+2, E = 30f² for the full sphere.
        for f in 1..=4 {
            let (nodes, segs) = geodesic_network(f, 2.0, false);
            assert_eq!(nodes.len(), (10 * f * f + 2) as usize, "V for f={f}");
            assert_eq!(segs.len(), (30 * f * f) as usize, "E for f={f}");
        }
    }

    #[test]
    fn geodesic_dome_drops_lower_hemisphere() {
        let (full_nodes, _) = geodesic_network(3, 5.0, false);
        let (dome_nodes, dome_segs) = geodesic_network(3, 5.0, true);
        assert!(dome_nodes.len() < full_nodes.len());
        for p in &dome_nodes {
            assert!(p.z >= -5.0 * 1e-6);
        }
        for &(a, b) in &dome_segs {
            assert!(a.z >= -5.0 * 1e-6 && b.z >= -5.0 * 1e-6);
        }
    }

    #[test]
    fn spaceframe_counts() {
        // nx=ny=1: top grid 2×2 → 4 nodes, 4 top edges; bottom 1×1 → 1 node,
        // 0 bottom edges; diagonals 1 node ×4 = 4. Total 8 struts.
        let segs = spaceframe_struts(1, 1, 3.0, 1.5);
        assert_eq!(segs.len(), 8);
        // nx=2,ny=2: top edges = 2*(3*2)=12; bottom edges 2*(1*2)=... compute:
        // top: i..=2,j..=2 → horiz 2*3=6, vert 3*2=6 → 12; bottom 2×2 grid:
        // horiz (i+1<2 → i=0) 1 per j-col ×2 =2, vert similarly 2 → 4; diag 4×4=16.
        let s2 = spaceframe_struts(2, 2, 3.0, 1.5);
        assert_eq!(s2.len(), 12 + 4 + 16);
    }

    #[test]
    fn hypar_saddle_shape() {
        let m = hypar_surface(2.0, 2.0, 2.0, 4, 4);
        assert_eq!(m.positions().len(), 25);
        assert_eq!(m.faces().len(), 32);
        // Corner (a,a): z = a*a/c = 4/2 = 2; corner (a,-a): z = -2. Saddle.
        let zs: Vec<f64> = m.positions().iter().map(|p| p.z).collect();
        let zmax = zs.iter().cloned().fold(f64::MIN, f64::max);
        let zmin = zs.iter().cloned().fold(f64::MAX, f64::min);
        assert!((zmax - 2.0).abs() < 1e-9);
        assert!((zmin + 2.0).abs() < 1e-9);
    }

    #[test]
    fn gaussvault_springings_on_ground_crest_at_rise() {
        let m = gaussvault_surface(6.0, 10.0, 3.0, 8, 8, false);
        assert_eq!(m.positions().len(), 81);
        let zs: Vec<f64> = m.positions().iter().map(|p| p.z).collect();
        let zmin = zs.iter().cloned().fold(f64::MAX, f64::min);
        let zmax = zs.iter().cloned().fold(f64::MIN, f64::max);
        assert!(zmin.abs() < 1e-9, "springings on ground, got {zmin}");
        assert!((zmax - 3.0).abs() < 1e-6, "crest at rise, got {zmax}");
    }

    #[test]
    fn gaussvault_undulation_moves_crest() {
        let flat = gaussvault_surface(6.0, 10.0, 3.0, 8, 8, false);
        let wavy = gaussvault_surface(6.0, 10.0, 3.0, 8, 8, true);
        // Undulating variant has a higher peak than the plain sweep.
        let peak = |m: &Mesh| m.positions().iter().map(|p| p.z).fold(f64::MIN, f64::max);
        assert!(peak(&wavy) > peak(&flat) + 1e-6);
    }

    #[test]
    fn strut_lattice_box_per_segment() {
        let segs = vec![(DVec3::ZERO, DVec3::new(1.0, 0.0, 0.0))];
        let m = strut_lattice(&segs, 0.1);
        assert_eq!(m.positions().len(), 8); // one box = 8 verts
        assert_eq!(m.faces().len(), 12); // 6 quads = 12 tris
    }

    #[test]
    fn diagrid_counts_and_deterministic() {
        // nx=2, ny=3 → 2·2·3 = 12 diagonals + 4 perimeter = 16 segments.
        let a = diagrid_segments(2, 3, 20.0, 40.0);
        assert_eq!(a.len(), 2 * 2 * 3 + 4);
        let b = diagrid_segments(2, 3, 20.0, 40.0);
        assert_eq!(a, b, "diagrid is deterministic");
        // All planar (z = 0).
        for &(p, q) in &a {
            assert!(p.z.abs() < 1e-12 && q.z.abs() < 1e-12);
        }
    }

    #[test]
    fn reciprocal_counts_and_deterministic() {
        let a = reciprocal_segments(8, 3.0, 4.0);
        assert_eq!(a.len(), 8, "one member per count");
        let b = reciprocal_segments(8, 3.0, 4.0);
        assert_eq!(a, b, "reciprocal is deterministic");
        // Every member has the requested length.
        for &(p, q) in &a {
            assert!(((q - p).length() - 4.0).abs() < 1e-9, "member length == length");
        }
        // Start points lie on the pitch circle.
        for &(p, _) in &a {
            assert!((p.length() - 3.0).abs() < 1e-9, "start on pitch circle r=3");
        }
    }

    #[test]
    fn waffle_counts_and_deterministic() {
        // nx=3, ny=4 → 2·3 + 2·4 + 3·4 = 6 + 8 + 12 = 26 segments.
        let a = waffle_segments(3, 4, 10.0, 16.0, 1.0);
        assert_eq!(a.len(), 2 * 3 + 2 * 4 + 3 * 4);
        let b = waffle_segments(3, 4, 10.0, 16.0, 1.0);
        assert_eq!(a, b, "waffle is deterministic");
        // Z extent spans [0, depth].
        let zmax = a.iter().flat_map(|&(p, q)| [p.z, q.z]).fold(f64::MIN, f64::max);
        let zmin = a.iter().flat_map(|&(p, q)| [p.z, q.z]).fold(f64::MAX, f64::min);
        assert!((zmax - 1.0).abs() < 1e-12 && zmin.abs() < 1e-12);
    }

    #[test]
    fn voronoishell_edges_deterministic_and_clipped() {
        // A tiny deterministic case: 6 seeds in a 10×10 square, seed 1.
        let a = voronoishell_segments(6, 10.0, 10.0, 1);
        let b = voronoishell_segments(6, 10.0, 10.0, 1);
        assert_eq!(a, b, "voronoishell is deterministic for a fixed seed");
        assert!(!a.is_empty(), "expected some interior Voronoi edges");
        // Every emitted edge is planar (z=0) and clipped inside the rectangle.
        for &(p, q) in &a {
            assert!(p.z.abs() < 1e-12 && q.z.abs() < 1e-12);
            for r in [p, q] {
                assert!(r.x >= -1e-9 && r.x <= 10.0 + 1e-9, "x in box: {r}");
                assert!(r.y >= -1e-9 && r.y <= 10.0 + 1e-9, "y in box: {r}");
            }
        }
        // A different seed moves the sites → a different edge set (very likely a
        // different count; at minimum not byte-identical).
        let c = voronoishell_segments(6, 10.0, 10.0, 2);
        assert!(a != c, "distinct seeds should change the pattern");
    }

    #[test]
    fn voronoishell_degenerate_seeds_do_not_panic() {
        // 1 cell → <3 sites → no triangulation, empty (no panic).
        assert!(voronoishell_segments(1, 5.0, 5.0, 3).is_empty());
        // Zero extent is clamped, not a divide-by-zero panic.
        let _ = voronoishell_segments(8, 0.0, 0.0, 3);
    }

    #[test]
    fn schwedler_segment_count_small_case() {
        // m=4 meridians, rings=2, dome. Deterministic count:
        //  meridional: apex→ring1 (m) + rings 1..2 (m·(rings-1)) = 4 + 4 = 8
        //  latitude rings 1..=2: m·rings = 4·2 = 8
        //  diagonals rings 1..2: m·(rings-1) = 4·1 = 4
        //  total = 8 + 8 + 4 = 20
        let segs = schwedler_segments(4, 2, 8.0, true);
        let expected = {
            let (m, rings) = (4u32, 2u32);
            let merid = m + m * (rings - 1);
            let latr = m * rings;
            let diag = m * (rings - 1);
            (merid + latr + diag) as usize
        };
        assert_eq!(segs.len(), expected, "schwedler dome segment count");
        // All nodes on the sphere of the given radius (within tolerance).
        for &(p, q) in &segs {
            assert!((p.length() - 8.0).abs() < 1e-6, "node on sphere: {p}");
            assert!((q.length() - 8.0).abs() < 1e-6, "node on sphere: {q}");
        }
        // Dome: nothing below the equator.
        for &(p, q) in &segs {
            assert!(p.z >= -1e-6 && q.z >= -1e-6, "dome stays above equator");
        }
        // Full sphere with the same ring count covers twice the latitude span, so
        // it dips below the equator (whereas the dome stops at z=0).
        let full = schwedler_segments(4, 2, 8.0, false);
        assert!(
            full.iter().any(|&(p, q)| p.z < -1e-6 || q.z < -1e-6),
            "full sphere reaches below the equator"
        );
    }

    #[test]
    fn catenary_vault_counts_crown_and_catenary_shape() {
        let m = catenary_vault_surface(8.0, 12.0, 4.0, 16, 16);
        assert_eq!(m.positions().len(), 17 * 17);
        assert_eq!(m.faces().len(), 16 * 16 * 2);
        let zs: Vec<f64> = m.positions().iter().map(|p| p.z).collect();
        let zmin = zs.iter().cloned().fold(f64::MAX, f64::min);
        let zmax = zs.iter().cloned().fold(f64::MIN, f64::max);
        // Springings on the ground; crown at ~rise.
        assert!(zmin.abs() < 1e-6, "springings on ground, got {zmin}");
        assert!((zmax - 4.0).abs() < 1e-6, "crown at rise, got {zmax}");
        // Catenary, NOT a parabola: sample the profile at the crown row (j=0).
        // A catenary is flatter at the crown and steeper near the springings than
        // a parabola of the same span+rise. Compare the height at u=0.25 to the
        // parabola z_par = rise·(1-(2u-1)²): at u=0.25, (2u-1)²=0.25 → z_par=0.75·rise.
        // The catenary sits ABOVE the parabola there (flatter crown).
        let nu = 16usize;
        let row: Vec<f64> = (0..=nu).map(|i| m.positions()[i].z).collect();
        let quarter = row[nu / 4];
        let parabola_q = 4.0 * 0.75; // rise·0.75
        assert!(
            quarter > parabola_q + 1e-3,
            "catenary should be flatter (higher) than parabola at u=0.25: cat={quarter} par={parabola_q}"
        );
        // Convexity of the arch (concave-down): second difference of z ≤ 0.
        for i in 1..row.len() - 1 {
            let d2 = row[i - 1] + row[i + 1] - 2.0 * row[i];
            assert!(d2 <= 1e-9, "arch must be concave at {i}");
        }
    }

    #[test]
    fn catenary_param_matches_sag_definition() {
        // The solved parameter must reproduce the requested sag.
        let (span, rise) = (10.0, 3.0);
        let a = catenary_param(span, rise);
        let sag = a * ((span / (2.0 * a)).cosh() - 1.0);
        assert!((sag - rise).abs() < 1e-6, "sag {sag} != rise {rise}");
    }

    #[test]
    fn gridshell_member_count() {
        let s = GridshellSurface::Hypar { a: 2.0, b: 2.0, c: 2.0 };
        let m = gridshell(s, 3, 3, 0.05);
        // u-members: (nv+1)*nu = 4*3 = 12; v-members: (nu+1)*nv = 4*3 = 12 → 24
        // struts × 8 verts.
        assert_eq!(m.positions().len(), 24 * 8);
    }
}
