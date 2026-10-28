// SPDX-License-Identifier: AGPL-3.0-only
// Copyright © 2026 Hector Tarrido-Picart

//! Editable street centrelines (#157). A site plan's streets form a graph:
//! centreline edges keyed by street id (#148), meeting at nodes (shared
//! vertices, or an end on another street's side). A hand edit — move a whole
//! street, move / insert / delete a centreline node — is stored as a
//! per-street geometry override ([`StreetEdit::centerline`]) and the plan is
//! re-derived around the edited network. Edits are validated first: a street
//! must stay in the site, two streets may only cross at a shared node, and no
//! block may become thinner than the minimum block depth.

use std::collections::{BTreeMap, BTreeSet};

use glam::DVec2;
use itsjustcad_doc::{Document, Geometry, ObjectId, StreetEdit, SubdivKind};
use kernel_curve::Curve;
use subdivision::{Polygon2d, Street, StreetGraph, StreetTier, SubdivisionSettings};

/// Two centreline points closer than this (m) are the same graph node.
pub const NODE_TOL: f64 = 0.01;

/// One street geometry edit (`streetmove` / `streetnode` / `streetremove` /
/// `streetreset`).
/// Serialized with an `op` tag (the FFI `ijc_street_edit` JSON uses the same
/// shape, plus the street id); points are plan `[x, y]`.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum StreetOp {
    /// Translate the street(s); ends of other streets resting on their sides
    /// follow.
    Move { delta: DVec2 },
    /// Move centreline node `index` to `to`. Every street sharing the node
    /// moves with it (a street passing through it gains a node there).
    Node { index: usize, to: DVec2 },
    /// Insert a node on the street at the point nearest `at`.
    Insert { at: DVec2 },
    /// Delete node `index` (#205). An interior node only one street passes through
    /// drops out of it (and two street ends meeting alone join through the
    /// gap). Any other node — a junction, a dead end, a street end on the
    /// site boundary — is deleted with every street meeting it; a node left
    /// joining just two street ends then merges them into one street.
    Delete { index: usize },
    /// Remove the street(s) from the network: blocks either side merge and
    /// the plan re-cuts. Kept through later param changes; `streetreset all`
    /// brings them back.
    Remove,
    /// Clear the street's overrides (geometry, name, class, width) back to the
    /// rule; `all` clears every street of the plan.
    Reset {
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        all: bool,
    },
}

impl StreetOp {
    /// The command verb, for messages and the op name.
    pub fn verb(&self) -> &'static str {
        match self {
            StreetOp::Move { .. } => "streetmove",
            StreetOp::Node { .. } | StreetOp::Insert { .. } | StreetOp::Delete { .. } => {
                "streetnode"
            }
            StreetOp::Remove => "streetremove",
            StreetOp::Reset { .. } => "streetreset",
        }
    }
}

/// The site boundary polygon of subdivision `source` (its current geometry).
pub fn site_polygon(doc: &Document, source: ObjectId) -> Option<Polygon2d> {
    match &doc.get(source)?.geometry {
        Geometry::Curve(c) if c.is_closed() => super::curve_to_polygon(c),
        _ => None,
    }
}

/// The street graph a plan bakes: the rule streets for `site` / `settings`
/// with the `edits` applied (width, hand centreline). An overridden street the
/// rule no longer generates (other params changed) is kept, so a hand-placed
/// street stays where it was put.
pub fn edited_graph(
    site: &Polygon2d,
    settings: &SubdivisionSettings,
    edits: &BTreeMap<u32, StreetEdit>,
) -> StreetGraph {
    let mut graph = subdivision::generate_streets(site, settings);
    graph
        .streets
        .retain(|s| !edits.get(&s.id).is_some_and(|e| e.removed));
    for s in &mut graph.streets {
        let Some(edit) = edits.get(&s.id) else { continue };
        if let Some(w) = edit.width {
            s.width = w;
        }
        if let Some(c) = &edit.centerline {
            s.centerline = c.clone();
        }
    }
    let default_width = subdivision::streets::generators::effective_road_width(settings);
    for (id, edit) in edits.iter().filter(|(_, e)| !e.removed) {
        let Some(c) = &edit.centerline else { continue };
        if graph.streets.iter().any(|s| s.id == *id) {
            continue;
        }
        graph.streets.push(Street {
            id: *id,
            centerline: c.clone(),
            width: edit.width.unwrap_or(default_width),
            tier: StreetTier::Connector,
        });
    }
    graph
}

fn street(graph: &StreetGraph, id: u32) -> Result<&Street, String> {
    graph
        .streets
        .iter()
        .find(|s| s.id == id)
        .ok_or_else(|| format!("no street {id} in this plan"))
}

fn one(targets: &[u32]) -> Result<u32, String> {
    match targets {
        [t] => Ok(*t),
        _ => Err("select exactly one street".into()),
    }
}

/// Distance from `p` to segment `ab`, and the projection parameter (0..1).
fn seg_project(p: DVec2, a: DVec2, b: DVec2) -> (f64, f64) {
    let ab = b - a;
    let len2 = ab.length_squared();
    let t = if len2 < 1e-18 {
        0.0
    } else {
        ((p - a).dot(ab) / len2).clamp(0.0, 1.0)
    };
    (p.distance(a + ab * t), t)
}

fn dist_to_polyline(p: DVec2, pts: &[DVec2]) -> f64 {
    pts.windows(2)
        .map(|w| seg_project(p, w[0], w[1]).0)
        .fold(f64::INFINITY, f64::min)
}

/// The segment of `pts` whose interior passes within [`NODE_TOL`] of `p`
/// (away from its vertices).
fn segment_through(pts: &[DVec2], p: DVec2) -> Option<usize> {
    pts.windows(2).position(|w| {
        seg_project(p, w[0], w[1]).0 < NODE_TOL
            && w[0].distance(p) >= NODE_TOL
            && w[1].distance(p) >= NODE_TOL
    })
}

/// What a geometry edit does to the network: new centrelines by street id,
/// and the streets it drops.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct StreetChange {
    pub lines: BTreeMap<u32, Vec<DVec2>>,
    pub removed: BTreeSet<u32>,
}

/// The change `op` makes, applied to `targets` (street ids) of `graph`.
/// `Remove` / `Reset` change no geometry here (they mark or clear the stored
/// overrides instead) and return an empty change.
pub fn apply_op(graph: &StreetGraph, targets: &[u32], op: &StreetOp) -> Result<StreetChange, String> {
    if let StreetOp::Delete { index } = op {
        return delete_node(&graph.streets, street(graph, one(targets)?)?, *index);
    }
    let mut out = BTreeMap::new();
    match op {
        StreetOp::Move { delta } => {
            if !delta.is_finite() || delta.length() < 1e-9 {
                return Err("delta must be a non-zero x,y offset".into());
            }
            let moved: Vec<&Street> =
                targets.iter().map(|t| street(graph, *t)).collect::<Result<_, _>>()?;
            for s in &moved {
                out.insert(s.id, s.centerline.iter().map(|p| *p + *delta).collect());
            }
            // Ends of other streets resting on a moved street's side (a T on
            // it, not at its own ends) follow it.
            let on_side = |p: DVec2, m: &Street| {
                let c = &m.centerline;
                dist_to_polyline(p, c) < NODE_TOL
                    && c[0].distance(p) >= NODE_TOL
                    && c[c.len() - 1].distance(p) >= NODE_TOL
            };
            for s in graph.streets.iter().filter(|s| !targets.contains(&s.id)) {
                let mut pts = s.centerline.clone();
                let last = pts.len() - 1;
                let mut hit = false;
                for end in [0, last] {
                    if moved.iter().any(|m| on_side(pts[end], m)) {
                        pts[end] += *delta;
                        hit = true;
                    }
                }
                if hit {
                    out.insert(s.id, pts);
                }
            }
        }
        StreetOp::Node { index, to } => {
            let t = street(graph, one(targets)?)?;
            let Some(&old) = t.centerline.get(*index) else {
                return Err(format!(
                    "street {} has {} nodes (index 0..{})",
                    t.id,
                    t.centerline.len(),
                    t.centerline.len() - 1
                ));
            };
            if !to.is_finite() {
                return Err("to= must be a finite x,y point".into());
            }
            for s in &graph.streets {
                let mut pts = s.centerline.clone();
                let mut hit = false;
                for p in pts.iter_mut().filter(|p| p.distance(old) < NODE_TOL) {
                    *p = *to;
                    hit = true;
                }
                if !hit && let Some(k) = segment_through(&pts, old) {
                    pts.insert(k + 1, *to);
                    hit = true;
                }
                if hit {
                    out.insert(s.id, pts);
                }
            }
        }
        StreetOp::Insert { at } => {
            let t = street(graph, one(targets)?)?;
            if !at.is_finite() {
                return Err("at= must be a finite x,y point".into());
            }
            let (k, q) = t
                .centerline
                .windows(2)
                .enumerate()
                .map(|(k, w)| {
                    let (d, u) = seg_project(*at, w[0], w[1]);
                    (d, k, w[0] + (w[1] - w[0]) * u)
                })
                .min_by(|a, b| a.0.total_cmp(&b.0))
                .map(|(_, k, q)| (k, q))
                .ok_or("street has no segments")?;
            if t.centerline.iter().any(|p| p.distance(q) < NODE_TOL) {
                return Err(format!("street {} already has a node there", t.id));
            }
            let mut pts = t.centerline.clone();
            pts.insert(k + 1, q);
            out.insert(t.id, pts);
        }
        StreetOp::Delete { .. } | StreetOp::Remove | StreetOp::Reset { .. } => {}
    }
    Ok(StreetChange { lines: out, removed: BTreeSet::new() })
}

/// Ids of the streets meeting `p`: a centreline node at it, or a segment
/// through it.
fn streets_at(streets: &[Street], p: DVec2) -> Vec<u32> {
    streets
        .iter()
        .filter(|s| dist_to_polyline(p, &s.centerline) < NODE_TOL)
        .map(|s| s.id)
        .collect()
}

/// How many ends of `pts` sit at `p` (2 for a closed loop starting there).
fn ends_at(pts: &[DVec2], p: DVec2) -> usize {
    [pts[0], pts[pts.len() - 1]].iter().filter(|q| q.distance(p) < NODE_TOL).count()
}

/// `a` and `b` (each with one end at `q`) joined into one centreline through
/// `q`. The node at `q` is dropped when `drop` is set, or when it is
/// straight through (on the line between its neighbours).
fn join(a: &[DVec2], b: &[DVec2], q: DVec2, drop: bool) -> Vec<DVec2> {
    let mut a = a.to_vec();
    if a[0].distance(q) < NODE_TOL {
        a.reverse();
    }
    let mut b = b.to_vec();
    if b[0].distance(q) >= NODE_TOL {
        b.reverse();
    }
    a.pop();
    let straight = seg_project(q, a[a.len() - 1], b[1]).0 < NODE_TOL;
    if !(drop || straight) {
        a.push(q);
    }
    a.extend_from_slice(&b[1..]);
    a
}

/// `streetnode delete` of node `index` of street `t`: see [`StreetOp::Delete`].
fn delete_node(streets: &[Street], t: &Street, index: usize) -> Result<StreetChange, String> {
    let n = t.centerline.len();
    let Some(&p) = t.centerline.get(index) else {
        return Err(format!("street {} has {n} nodes (index 0..{})", t.id, n.saturating_sub(1)));
    };
    let find = |id: u32| streets.iter().find(|s| s.id == id).expect("listed street");
    let at = streets_at(streets, p);
    // Network degree: an end counts once, a street passing through twice.
    let degree: usize = at
        .iter()
        .map(|id| match ends_at(&find(*id).centerline, p) {
            1 => 1,
            _ => 2,
        })
        .sum();
    let mut out = StreetChange::default();
    match (degree, at.as_slice()) {
        (2, [_]) => {
            let mut pts = t.centerline.clone();
            if ends_at(&pts, p) == 2 {
                // A closed loop's start: re-close it on the next node.
                pts.pop();
                pts.remove(0);
                pts.push(pts[0]);
            } else {
                pts.remove(index);
            }
            if pts.len() < if ends_at(&t.centerline, p) == 2 { 4 } else { 2 } {
                return Err(format!("street {} would have too few nodes", t.id));
            }
            out.lines.insert(t.id, pts);
        }
        (2, [a, b]) => {
            let (a, b) = (find(*a), find(*b));
            out.lines.insert(a.id, join(&a.centerline, &b.centerline, p, true));
            out.removed.insert(b.id);
        }
        _ => {
            out.removed.extend(at.iter().copied());
            let mut work: Vec<Street> =
                streets.iter().filter(|s| !out.removed.contains(&s.id)).cloned().collect();
            // The nodes the deleted streets passed: remaining street ends on them.
            let gone: Vec<&Street> = at.iter().map(|id| find(*id)).collect();
            let mut far: Vec<DVec2> = Vec::new();
            for c in work.iter().map(|s| &s.centerline) {
                for q in [c[0], c[c.len() - 1]] {
                    if gone.iter().any(|g| dist_to_polyline(q, &g.centerline) < NODE_TOL)
                        && !far.iter().any(|f| f.distance(q) < NODE_TOL)
                    {
                        far.push(q);
                    }
                }
            }
            for q in far {
                let mut ids = streets_at(&work, q);
                ids.sort_unstable();
                let [a, b] = ids[..] else { continue };
                let (ia, ib) = (
                    work.iter().position(|s| s.id == a).expect("listed"),
                    work.iter().position(|s| s.id == b).expect("listed"),
                );
                if ends_at(&work[ia].centerline, q) != 1 || ends_at(&work[ib].centerline, q) != 1 {
                    continue;
                }
                let merged = join(&work[ia].centerline, &work[ib].centerline, q, false);
                work[ia].centerline = merged.clone();
                work.remove(ib);
                out.lines.remove(&b);
                out.lines.insert(a, merged);
                out.removed.insert(b);
            }
        }
    }
    Ok(out)
}

/// `true` when segments `ab` and `cd` cross at a point interior to both
/// (each end more than [`NODE_TOL`] off the other line): a crossing without a
/// shared node. Touching (a T-junction, a shared vertex) is not a crossing.
fn crosses(a: DVec2, b: DVec2, c: DVec2, d: DVec2) -> bool {
    let side = |p: DVec2, q: DVec2, r: DVec2| {
        let len = (q - p).length();
        if len < 1e-12 { 0.0 } else { (q - p).perp_dot(r - p) / len }
    };
    let opposite = |x: f64, y: f64| (x > NODE_TOL && y < -NODE_TOL) || (x < -NODE_TOL && y > NODE_TOL);
    opposite(side(c, d, a), side(c, d, b)) && opposite(side(a, b, c), side(a, b, d))
}

fn polylines_cross(p: &[DVec2], q: &[DVec2]) -> bool {
    p.windows(2)
        .any(|s| q.windows(2).any(|t| crosses(s[0], s[1], t[0], t[1])))
}

/// Street id pairs whose centrelines cross without a shared node.
fn crossing_pairs(graph: &StreetGraph) -> BTreeSet<(u32, u32)> {
    let mut out = BTreeSet::new();
    for (i, a) in graph.streets.iter().enumerate() {
        for b in &graph.streets[i + 1..] {
            if polylines_cross(&a.centerline, &b.centerline) {
                out.insert((a.id.min(b.id), a.id.max(b.id)));
            }
        }
    }
    out
}

fn on_or_in(site: &Polygon2d, p: DVec2) -> bool {
    site.contains(p) || site.edges().any(|(a, b)| seg_project(p, a, b).0 < NODE_TOL)
}

/// The minimum block depth an edit must keep: half the plan's block depth
/// (one lot deep).
pub fn min_block_depth(settings: &SubdivisionSettings) -> f64 {
    subdivision::streets::generators::effective_block_depth(settings) * 0.5
}

fn thin_blocks(site: &Polygon2d, graph: &StreetGraph, settings: &SubdivisionSettings) -> usize {
    let inset = min_block_depth(settings) * 0.5 - 1e-3;
    subdivision::extract_blocks(site, graph, settings)
        .iter()
        .filter(|b| subdivision::clip_bridge::offset(&b.polygon, -inset).is_empty())
        .count()
}

/// Reject an edit that turns `before` into `after` (streets `changed`) when a
/// changed street leaves the site, two streets newly cross without a node, or
/// a block becomes thinner than [`min_block_depth`]. The error names the
/// problem.
pub fn validate(
    site: &Polygon2d,
    settings: &SubdivisionSettings,
    before: &StreetGraph,
    after: &StreetGraph,
    changed: &BTreeSet<u32>,
) -> Result<(), String> {
    for s in after.streets.iter().filter(|s| changed.contains(&s.id)) {
        let pts = &s.centerline;
        if pts.len() < 2 || pts.windows(2).any(|w| w[0].distance(w[1]) < NODE_TOL) {
            return Err(format!("street {} would have two nodes on top of each other", s.id));
        }
        if let Some((j, p)) = pts.iter().enumerate().find(|(_, p)| !on_or_in(site, **p)) {
            return Err(format!(
                "street {} would leave the site: node {j} at ({:.2}, {:.2}) is outside the boundary",
                s.id, p.x, p.y
            ));
        }
        let leaves = pts.windows(2).any(|w| {
            !on_or_in(site, (w[0] + w[1]) * 0.5)
                || site.edges().any(|(a, b)| crosses(w[0], w[1], a, b))
        });
        if leaves {
            return Err(format!(
                "street {} would leave the site: it crosses the boundary",
                s.id
            ));
        }
    }
    let old = crossing_pairs(before);
    if let Some((a, b)) = crossing_pairs(after)
        .into_iter()
        .find(|p| !old.contains(p) && (changed.contains(&p.0) || changed.contains(&p.1)))
    {
        return Err(format!("streets {a} and {b} would cross without a shared node"));
    }
    if thin_blocks(site, after, settings) > thin_blocks(site, before, settings) {
        return Err(format!(
            "a block would be thinner than the minimum block depth ({:.1} m)",
            min_block_depth(settings)
        ));
    }
    Ok(())
}

/// The generated roads of a site plan link, as (object id, street id,
/// plan-XY centreline).
fn link_roads(doc: &Document, source: ObjectId) -> Vec<(ObjectId, u32, Vec<DVec2>, &itsjustcad_doc::StreetInfo)> {
    let Some(link) = doc.subdiv_link(source) else {
        return Vec::new();
    };
    link.produced
        .iter()
        .filter_map(|id| doc.get(*id))
        .filter_map(|o| {
            let info = o.street.as_ref()?;
            let Geometry::Curve(Curve::Polyline { points, .. }) = &o.geometry else {
                return None;
            };
            Some((o.id, info.id, points.iter().map(|p| p.truncate()).collect(), info))
        })
        .collect()
}

/// The street graph of site plan `source` as hosts draw and edit it (#157):
/// `{"source","nodes":[{"id","at":[x,y],"streets":[…],"boundary"}],
/// "edges":[{"street","object","from","to","points":[[x,y]…],"width","class",
/// "name"?,"overridden":{"geometry","name","class","width"}}]}`. Nodes are the
/// street ends, then the other junctions (crossings), of the plan's street
/// network ([`super::street_net::StreetNetwork`], the graph the optimizer's
/// park walk follows): an end resting on (within 1 m of) another street joins
/// it; `from` / `to` are the node ids of an edge's ends; `points` index the
/// `streetnode` node indices. `None` when `source` is not a site plan.
pub fn street_graph_json(doc: &Document, source: ObjectId) -> Option<serde_json::Value> {
    let link = doc.subdiv_link(source)?;
    if !matches!(link.kind, SubdivKind::Site | SubdivKind::SitePlan) {
        return None;
    }
    let site = site_polygon(doc, source);
    let roads = link_roads(doc, source);
    // The one street graph (#170): the walk distances use it too.
    let lines: Vec<super::street_net::StreetLine> = roads
        .iter()
        .map(|(obj, id, pts, _)| super::street_net::StreetLine {
            id: *id,
            points: pts.clone(),
            closed: doc
                .get(*obj)
                .is_some_and(|o| matches!(&o.geometry, Geometry::Curve(c) if c.is_closed())),
        })
        .collect();
    let net = super::street_net::StreetNetwork::build(&lines);
    // Listed nodes: street ends (in street order), then the other junctions
    // (crossings, a street passing through another's bend).
    let mut nodes: Vec<usize> = Vec::new();
    let mut node_of = |n: usize| -> usize {
        nodes.iter().position(|m| *m == n).unwrap_or_else(|| {
            nodes.push(n);
            nodes.len() - 1
        })
    };
    let at = |p: DVec2| net.node_at(p);
    // `None` for a road too short to be an edge.
    let ends: Vec<Option<(usize, usize)>> = roads
        .iter()
        .map(|(_, _, pts, _)| Some((node_of(at(pts[0])?), node_of(at(*pts.last()?)?))))
        .collect();
    for n in 0..net.nodes.len() {
        if net.degree(n) >= 3 {
            node_of(n);
        }
    }
    let xy = |p: DVec2| serde_json::json!([p.x, p.y]);
    let nodes_json: Vec<_> = nodes
        .iter()
        .enumerate()
        .map(|(k, n)| {
            let p = net.nodes[*n];
            let boundary = site
                .as_ref()
                .is_some_and(|s| s.edges().any(|(a, b)| seg_project(p, a, b).0 < NODE_TOL));
            serde_json::json!({ "id": k, "at": xy(p), "streets": net.streets_at(*n), "boundary": boundary })
        })
        .collect();
    let edges: Vec<_> = roads
        .iter()
        .zip(ends)
        .filter_map(|(r, e)| Some((r, e?)))
        .map(|((obj, id, pts, info), (from, to))| {
            let edit = link.streets.get(id);
            let mut e = serde_json::json!({
                "street": id,
                "object": obj.to_string(),
                "from": from,
                "to": to,
                "points": pts.iter().map(|p| xy(*p)).collect::<Vec<_>>(),
                "width": info.width,
                "class": info.class.as_str(),
                "overridden": {
                    "geometry": edit.is_some_and(|e| e.centerline.is_some()),
                    "name": edit.is_some_and(|e| e.name.is_some()),
                    "class": edit.is_some_and(|e| e.class.is_some()),
                    "width": edit.is_some_and(|e| e.width.is_some()),
                },
            });
            if let Some(name) = &info.name {
                e["name"] = name.clone().into();
            }
            e
        })
        .collect();
    Some(serde_json::json!({
        "source": source.to_string(),
        "nodes": nodes_json,
        "edges": edges,
    }))
}

/// The live site plan whose generated roads are exactly `ids` (all from one
/// non-frozen Site/SitePlan link). A move or point edit on them becomes a
/// street geometry edit instead of a stale hand edit.
pub fn live_roads_source(doc: &Document, ids: &[ObjectId]) -> Option<ObjectId> {
    let mut source = None;
    for id in ids {
        doc.get(*id)?.street.as_ref()?;
        let src = doc.subdiv_source_of(*id)?;
        if source.is_some_and(|s| s != src) {
            return None;
        }
        source = Some(src);
    }
    let link = doc.subdiv_link(source?)?;
    (!link.frozen && matches!(link.kind, SubdivKind::Site | SubdivKind::SitePlan)).then_some(source?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn graph(lines: &[&[(f64, f64)]]) -> StreetGraph {
        let mut g = StreetGraph::new();
        for l in lines {
            g.add(l.iter().map(|(x, y)| DVec2::new(*x, *y)).collect(), 12.0, StreetTier::Connector);
        }
        g
    }

    #[test]
    fn dragging_a_t_node_bends_the_through_street() {
        // 0: vertical through street; 1, 2: stubs ending on its side.
        let g = graph(&[&[(100.0, 0.0), (100.0, 150.0)], &[(0.0, 75.0), (100.0, 75.0)], &[(100.0, 75.0), (200.0, 75.0)]]);
        let to = DVec2::new(110.0, 75.0);
        let out = apply_op(&g, &[1], &StreetOp::Node { index: 1, to }).unwrap();
        let out = out.lines;
        assert_eq!(out[&1][1], to);
        assert_eq!(out[&2][0], to);
        assert_eq!(out[&0], vec![DVec2::new(100.0, 0.0), to, DVec2::new(100.0, 150.0)]);
    }

    #[test]
    fn moving_a_street_carries_the_ends_resting_on_it() {
        let g = graph(&[&[(100.0, 0.0), (100.0, 150.0)], &[(0.0, 75.0), (100.0, 75.0)], &[(50.0, 75.0), (50.0, 150.0)]]);
        let out = apply_op(&g, &[1], &StreetOp::Move { delta: DVec2::new(0.0, 10.0) }).unwrap().lines;
        assert_eq!(out[&1], vec![DVec2::new(0.0, 85.0), DVec2::new(100.0, 85.0)]);
        assert_eq!(out[&2][0], DVec2::new(50.0, 85.0), "the stub on it follows");
        assert!(!out.contains_key(&0), "the street it rests on stays");
    }

    #[test]
    fn insert_and_delete_nodes() {
        let g = graph(&[&[(0.0, 0.0), (100.0, 0.0)]]);
        let out = apply_op(&g, &[0], &StreetOp::Insert { at: DVec2::new(40.0, 3.0) }).unwrap().lines;
        assert_eq!(out[&0][1], DVec2::new(40.0, 0.0), "projected onto the street");
        let g2 = graph(&[&[(0.0, 0.0), (40.0, 0.0), (100.0, 0.0)]]);
        assert_eq!(apply_op(&g2, &[0], &StreetOp::Delete { index: 1 }).unwrap().lines[&0].len(), 2);
        let end = apply_op(&g2, &[0], &StreetOp::Delete { index: 0 }).unwrap();
        assert_eq!(end.removed, BTreeSet::from([0]), "a dead end takes its street");
        assert!(apply_op(&g2, &[0], &StreetOp::Delete { index: 3 }).is_err(), "no such node");
    }

    /// A junction goes with every street meeting it; a node left joining
    /// just two street ends merges them into one street.
    #[test]
    fn deleting_a_junction_drops_its_streets_and_merges_what_is_left() {
        let g = graph(&[&[(0.0, 0.0), (40.0, 0.0), (100.0, 0.0)], &[(40.0, 0.0), (40.0, 50.0)]]);
        let t = apply_op(&g, &[0], &StreetOp::Delete { index: 1 }).unwrap();
        assert_eq!(t.removed, BTreeSet::from([0, 1]));
        assert!(t.lines.is_empty());

        let g = graph(&[
            &[(100.0, 0.0), (100.0, 150.0)],
            &[(0.0, 75.0), (100.0, 75.0)],
            &[(50.0, 75.0), (50.0, 150.0)],
            &[(50.0, 0.0), (50.0, 75.0)],
            &[(100.0, 75.0), (200.0, 75.0)],
        ]);
        let out = apply_op(&g, &[1], &StreetOp::Delete { index: 1 }).unwrap();
        assert_eq!(out.removed, BTreeSet::from([0, 1, 3, 4]));
        assert_eq!(out.lines[&2], vec![DVec2::new(50.0, 150.0), DVec2::new(50.0, 0.0)], "2 and 3 run through");
        assert_eq!(out.lines.len(), 1);

        // Two street ends meeting alone join through the gap.
        let g = graph(&[&[(0.0, 0.0), (50.0, 0.0)], &[(50.0, 0.0), (100.0, 10.0)]]);
        let out = apply_op(&g, &[0], &StreetOp::Delete { index: 1 }).unwrap();
        assert_eq!(out.lines[&0], vec![DVec2::new(0.0, 0.0), DVec2::new(100.0, 10.0)]);
        assert_eq!(out.removed, BTreeSet::from([1]));
    }

    #[test]
    fn crossing_without_a_node_is_detected_but_t_junctions_are_not() {
        assert!(crosses(DVec2::new(0.0, 0.0), DVec2::new(10.0, 0.0), DVec2::new(5.0, -5.0), DVec2::new(5.0, 5.0)));
        assert!(!crosses(DVec2::new(0.0, 0.0), DVec2::new(10.0, 0.0), DVec2::new(5.0, 0.0), DVec2::new(5.0, 5.0)));
        assert!(!crosses(DVec2::new(0.0, 0.0), DVec2::new(10.0, 0.0), DVec2::new(10.0, 0.0), DVec2::new(15.0, 5.0)));
    }
}
