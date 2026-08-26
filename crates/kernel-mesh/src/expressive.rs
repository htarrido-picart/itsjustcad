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
    strut_lattice(&segs, thickness)
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
    fn gridshell_member_count() {
        let s = GridshellSurface::Hypar { a: 2.0, b: 2.0, c: 2.0 };
        let m = gridshell(s, 3, 3, 0.05);
        // u-members: (nv+1)*nu = 4*3 = 12; v-members: (nu+1)*nv = 4*3 = 12 → 24
        // struts × 8 verts.
        assert_eq!(m.positions().len(), 24 * 8);
    }
}
