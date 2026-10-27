// SPDX-License-Identifier: AGPL-3.0-only
// Copyright © 2026 Hector Tarrido-Picart

//! The one street graph of a site plan (#170): road centrelines split where
//! they cross or meet, as an undirected graph of segments that keep their
//! street id. Hosts see it through `ijc_street_graph_json` (its nodes are the
//! street ends and junctions, see [`super::streets::street_graph_json`]) and
//! the optimizer walks it ([`crate::site_metrics::walk_distances`]), so an
//! edited street changes both the drawn graph and the walking distances.
//!
//! Built from the baked roads of a document ([`StreetNetwork::from_doc`]) or
//! from a plan computed off-document ([`StreetNetwork::from_plan`]); both read
//! the same edited street graph ([`super::streets::edited_graph`]).

use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap};

use glam::DVec2;
use itsjustcad_doc::{Document, Geometry, ObjectId};
use kernel_curve::Curve;

/// Snap / meet tolerance between centrelines, m: ends this close to another
/// street join it.
pub const SNAP: f64 = 1.0;

/// One street centreline.
#[derive(Debug, Clone, PartialEq)]
pub struct StreetLine {
    /// Street id (#148).
    pub id: u32,
    pub points: Vec<DVec2>,
    /// A loop (cul-de-sac bulb).
    pub closed: bool,
}

/// One split segment of a street.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct NetEdge {
    pub from: usize,
    pub to: usize,
    pub street: u32,
    pub length: f64,
}

/// The street network as an undirected graph of split centreline segments.
#[derive(Debug, Clone, Default)]
pub struct StreetNetwork {
    pub nodes: Vec<DVec2>,
    pub edges: Vec<NetEdge>,
    /// (neighbour, length) per node.
    adj: Vec<Vec<(usize, f64)>>,
    /// Edge ids per [`CELL`] grid cell, for nearest-edge queries.
    grid: HashMap<(i64, i64), Vec<usize>>,
}

const CELL: f64 = 50.0;

fn cell(p: DVec2) -> (i64, i64) {
    ((p.x / CELL).floor() as i64, (p.y / CELL).floor() as i64)
}

/// Projection of `p` on segment `ab`: (parameter in [0, 1], point).
pub fn project(p: DVec2, a: DVec2, b: DVec2) -> (f64, DVec2) {
    let ab = b - a;
    let t = if ab.length_squared() > 0.0 {
        ((p - a).dot(ab) / ab.length_squared()).clamp(0.0, 1.0)
    } else {
        0.0
    };
    (t, a + ab * t)
}

impl StreetNetwork {
    /// Build the graph: every centreline segment is split at crossings and
    /// where another street's end rests on it (within [`SNAP`]); points
    /// within [`SNAP`] are one node.
    pub fn build(lines: &[StreetLine]) -> StreetNetwork {
        let mut segs: Vec<(DVec2, DVec2, u32)> = Vec::new();
        for l in lines {
            segs.extend(l.points.windows(2).map(|w| (w[0], w[1], l.id)));
            if l.closed && l.points.len() > 2 {
                segs.push((l.points[l.points.len() - 1], l.points[0], l.id));
            }
        }
        segs.retain(|(a, b, _)| a.distance(*b) > 1e-6);
        // Split parameters per segment: crossings and endpoints that touch
        // another segment's interior (T-junctions).
        let mut cuts: Vec<Vec<f64>> = vec![vec![0.0, 1.0]; segs.len()];
        let mut sgrid: HashMap<(i64, i64), Vec<usize>> = HashMap::new();
        for (i, (a, b, _)) in segs.iter().enumerate() {
            let (lo, hi) = (cell(a.min(*b) - SNAP), cell(a.max(*b) + SNAP));
            for x in lo.0..=hi.0 {
                for y in lo.1..=hi.1 {
                    sgrid.entry((x, y)).or_default().push(i);
                }
            }
        }
        let cross = |u: DVec2, v: DVec2| u.x * v.y - u.y * v.x;
        let mut pairs: Vec<(usize, usize)> = sgrid
            .values()
            .flat_map(|ids| {
                ids.iter()
                    .flat_map(move |&i| ids.iter().filter(move |&&j| j > i).map(move |&j| (i, j)))
            })
            .collect();
        pairs.sort_unstable();
        pairs.dedup();
        for (i, j) in pairs {
            let ((a, b, _), (c, d, _)) = (segs[i], segs[j]);
            let (r, s) = (b - a, d - c);
            let den = cross(r, s);
            if den.abs() > 1e-12 {
                let t = cross(c - a, s) / den;
                let u = cross(c - a, r) / den;
                if (0.0..=1.0).contains(&t) && (0.0..=1.0).contains(&u) {
                    cuts[i].push(t);
                    cuts[j].push(u);
                }
            }
            // Endpoints lying on the other segment.
            for (p, k, (e, f)) in [
                (a, j, (c, d)),
                (b, j, (c, d)),
                (c, i, (a, b)),
                (d, i, (a, b)),
            ] {
                let (t, q) = project(p, e, f);
                if q.distance(p) <= SNAP {
                    cuts[k].push(t);
                }
            }
        }
        let mut net = StreetNetwork::default();
        // Nodes per SNAP cell; a point joins the nearest node within SNAP
        // (an end resting near a street meets the split point on it).
        let mut ids: HashMap<(i64, i64), Vec<usize>> = HashMap::new();
        let mut node = |net: &mut StreetNetwork, p: DVec2| -> usize {
            let k = ((p.x / SNAP).floor() as i64, (p.y / SNAP).floor() as i64);
            let near = (-1..=1)
                .flat_map(|dx| (-1..=1).map(move |dy| (k.0 + dx, k.1 + dy)))
                .filter_map(|c| ids.get(&c))
                .flatten()
                .map(|&n| (n, net.nodes[n].distance(p)))
                .filter(|(_, d)| *d <= SNAP)
                .min_by(|a, b| a.1.total_cmp(&b.1).then(a.0.cmp(&b.0)));
            if let Some((n, _)) = near {
                return n;
            }
            net.nodes.push(p);
            net.adj.push(Vec::new());
            ids.entry(k).or_default().push(net.nodes.len() - 1);
            net.nodes.len() - 1
        };
        for ((a, b, street), mut ts) in segs.into_iter().zip(cuts) {
            ts.sort_by(f64::total_cmp);
            ts.dedup_by(|x, y| (*x - *y).abs() < 1e-9);
            for w in ts.windows(2) {
                let (p, q) = (a + (b - a) * w[0], a + (b - a) * w[1]);
                let (u, v) = (node(&mut net, p), node(&mut net, q));
                if u == v {
                    continue;
                }
                let length = net.nodes[u].distance(net.nodes[v]);
                let e = net.edges.len();
                net.edges.push(NetEdge {
                    from: u,
                    to: v,
                    street,
                    length,
                });
                net.adj[u].push((v, length));
                net.adj[v].push((u, length));
                let (pu, pv) = (net.nodes[u], net.nodes[v]);
                let (lo, hi) = (cell(pu.min(pv)), cell(pu.max(pv)));
                for x in lo.0..=hi.0 {
                    for y in lo.1..=hi.1 {
                        net.grid.entry((x, y)).or_default().push(e);
                    }
                }
            }
        }
        net
    }

    /// The network of a plan computed off-document (its edited street graph).
    pub fn from_plan(site: &super::SiteBake) -> StreetNetwork {
        let lines: Vec<StreetLine> = site
            .roads
            .iter()
            .enumerate()
            .map(|(i, r)| StreetLine {
                id: site.streets.get(i).map_or(i as u32, |s| s.id),
                points: r.clone(),
                closed: site.road_closed.get(i).copied().unwrap_or(false),
            })
            .collect();
        StreetNetwork::build(&lines)
    }

    /// The network of site plan `source`'s baked roads (with their objects).
    pub fn from_doc(
        doc: &Document,
        source: ObjectId,
    ) -> (StreetNetwork, Vec<(ObjectId, StreetLine)>) {
        let roads = doc_lines(doc, source);
        let lines: Vec<StreetLine> = roads.iter().map(|(_, l)| l.clone()).collect();
        (StreetNetwork::build(&lines), roads)
    }

    /// The node at `p` (within [`SNAP`]), if any.
    pub fn node_at(&self, p: DVec2) -> Option<usize> {
        let mut best: Option<(usize, f64)> = None;
        for e in self.edges_near(p, SNAP) {
            let NetEdge { from, to, .. } = self.edges[e];
            for n in [from, to] {
                let d = self.nodes[n].distance(p);
                if d <= SNAP && best.is_none_or(|b| d < b.1 || (d == b.1 && n < b.0)) {
                    best = Some((n, d));
                }
            }
        }
        best.map(|b| b.0)
    }

    /// The street ids meeting at node `n`, sorted.
    pub fn streets_at(&self, n: usize) -> Vec<u32> {
        let mut s: Vec<u32> = self
            .edges
            .iter()
            .filter(|e| e.from == n || e.to == n)
            .map(|e| e.street)
            .collect();
        s.sort_unstable();
        s.dedup();
        s
    }

    /// How many segments meet at node `n`.
    pub fn degree(&self, n: usize) -> usize {
        self.adj.get(n).map_or(0, Vec::len)
    }

    fn edges_near(&self, p: DVec2, reach: f64) -> impl Iterator<Item = usize> + '_ {
        let (lo, hi) = (cell(p - reach), cell(p + reach));
        let mut out: Vec<usize> = (lo.0..=hi.0)
            .flat_map(|x| (lo.1..=hi.1).map(move |y| (x, y)))
            .filter_map(|k| self.grid.get(&k))
            .flatten()
            .copied()
            .collect();
        out.sort_unstable();
        out.dedup();
        out.into_iter()
    }

    /// The nearest edge to `p` within `reach`: (edge, projection, offset).
    pub fn nearest(&self, p: DVec2, reach: f64) -> Option<(usize, DVec2, f64)> {
        let mut best: Option<(usize, DVec2, f64)> = None;
        for e in self.edges_near(p, reach) {
            let NetEdge { from, to, .. } = self.edges[e];
            let (_, q) = project(p, self.nodes[from], self.nodes[to]);
            let d = q.distance(p);
            if d <= reach && best.is_none_or(|b| d < b.2 || (d == b.2 && e < b.0)) {
                best = Some((e, q, d));
            }
        }
        best
    }

    /// Multi-source Dijkstra: shortest network distance from any source
    /// (a point on an edge with an extra offset) to every node.
    pub fn distances(&self, sources: &[(usize, DVec2, f64)]) -> Vec<f64> {
        #[derive(PartialEq)]
        struct Item(f64, usize);
        impl Eq for Item {}
        impl Ord for Item {
            fn cmp(&self, o: &Self) -> Ordering {
                o.0.total_cmp(&self.0).then(o.1.cmp(&self.1))
            }
        }
        impl PartialOrd for Item {
            fn partial_cmp(&self, o: &Self) -> Option<Ordering> {
                Some(self.cmp(o))
            }
        }
        let mut dist = vec![f64::INFINITY; self.nodes.len()];
        let mut heap = BinaryHeap::new();
        for &(e, q, off) in sources {
            let NetEdge { from, to, .. } = self.edges[e];
            for n in [from, to] {
                let d = off + q.distance(self.nodes[n]);
                if d < dist[n] {
                    dist[n] = d;
                    heap.push(Item(d, n));
                }
            }
        }
        while let Some(Item(d, n)) = heap.pop() {
            if d > dist[n] {
                continue;
            }
            for &(m, w) in &self.adj[n] {
                if d + w < dist[m] {
                    dist[m] = d + w;
                    heap.push(Item(d + w, m));
                }
            }
        }
        dist
    }

    /// Total centreline length, m.
    pub fn length(&self) -> f64 {
        self.edges.iter().map(|e| e.length).sum()
    }
}

/// The baked roads of site plan `source`, as (object, centreline).
fn doc_lines(doc: &Document, source: ObjectId) -> Vec<(ObjectId, StreetLine)> {
    let Some(link) = doc.subdiv_link(source) else {
        return Vec::new();
    };
    link.produced
        .iter()
        .filter_map(|id| doc.get(*id))
        .filter_map(|o| {
            let info = o.street.as_ref()?;
            let Geometry::Curve(Curve::Polyline { points, closed }) = &o.geometry else {
                return None;
            };
            let points: Vec<DVec2> = points.iter().map(|p| p.truncate()).collect();
            let closed =
                *closed || (points.len() >= 3 && points[0].distance(*points.last()?) < 1e-6);
            Some((
                o.id,
                StreetLine {
                    id: info.id,
                    points,
                    closed,
                },
            ))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(id: u32, pts: &[(f64, f64)]) -> StreetLine {
        StreetLine {
            id,
            points: pts.iter().map(|(x, y)| DVec2::new(*x, *y)).collect(),
            closed: false,
        }
    }

    /// A T-junction and a crossing split the streets; segments keep their
    /// street; ends near a street join it.
    #[test]
    fn splits_at_crossings_and_t_junctions() {
        let net = StreetNetwork::build(&[
            line(0, &[(0.0, 0.0), (100.0, 0.0)]),
            line(1, &[(50.0, 0.5), (50.0, 80.0)]), // rests on street 0
            line(2, &[(0.0, 40.0), (100.0, 40.0)]), // crosses street 1
        ]);
        let t = net.node_at(DVec2::new(50.0, 0.0)).expect("T node");
        assert_eq!(net.streets_at(t), [0, 1]);
        assert_eq!(net.degree(t), 3);
        let x = net.node_at(DVec2::new(50.0, 40.0)).expect("crossing node");
        assert_eq!((net.streets_at(x), net.degree(x)), (vec![1, 2], 4));
        assert_eq!(net.edges.len(), 2 + 2 + 2);
        assert!(
            (net.length() - (100.0 + 79.5 + 100.0)).abs() < 1.0,
            "{}",
            net.length()
        );
    }
}
