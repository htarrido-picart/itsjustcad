// SPDX-License-Identifier: AGPL-3.0-only
// Copyright © 2026 Hector Tarrido-Picart

//! Hierarchical selection for parametric objects (#199). A click on anything a
//! parametric generator produced resolves to the parent → child [`Chain`]
//! under the cursor (site plan → street, or site plan → block → lot →
//! building), derived from the doc's [`SubdivLink`]s; generators nest, so a
//! block that is itself the source of a lot subdivision slots in between. The
//! hosts choose the level: [`Drill`] is the click / double-click / Esc state
//! machine the desktop uses (the iOS app mirrors it in Swift).

use std::collections::{BTreeSet, HashSet};

use glam::{DVec2, DVec3, Mat4, Vec2};
use itsjustcad_doc::{Document, Geometry, ObjectId, SubdivKind, SubdivLink};

use super::{
    BLOCKS_LAYER, BUILDINGS_LAYER, LOTS_LAYER, SETBACKS_LAYER, curve_to_polygon, lot_under,
    plan_probe, street_view,
};

/// Generators nest at most this deep (a guard against a cyclic link).
const MAX_DEPTH: usize = 8;

/// What one level of a [`Chain`] is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LevelKind {
    /// The top parent: a generator's source (a site boundary, a block that was
    /// subdivided into lots).
    Site,
    /// All the road objects of one generated street.
    Street,
    Block,
    Lot,
    /// A building's footprint and mass.
    Building,
    /// Any other generated output (a setback envelope, open space).
    Part,
    /// An object no generator produced: its chain is just itself.
    Object,
}

impl LevelKind {
    /// Stable token for the hosts (`site`, `street`, `block`, …).
    pub fn token(self) -> &'static str {
        match self {
            LevelKind::Site => "site",
            LevelKind::Street => "street",
            LevelKind::Block => "block",
            LevelKind::Lot => "lot",
            LevelKind::Building => "building",
            LevelKind::Part => "part",
            LevelKind::Object => "object",
        }
    }
}

/// One level of a [`Chain`]: the objects selecting that level selects.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Level {
    pub kind: LevelKind,
    /// Never empty; `ids[0]` is the object that stands for the level.
    pub ids: Vec<ObjectId>,
    /// Display name ("Site plan", "Main St", "Lot #3", …).
    pub label: String,
}

/// The parent → child levels under a click, top parent first.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Chain {
    pub levels: Vec<Level>,
    /// The level a plain click selects: 0 (the top parent), or 1 for a street
    /// that is not on the site's edge, which a click picks directly.
    pub start: usize,
}

impl Chain {
    /// The top parent's representative object.
    pub fn root(&self) -> ObjectId {
        self.levels[0].ids[0]
    }

    /// The deepest level index.
    pub fn last(&self) -> usize {
        self.levels.len() - 1
    }

    /// The level whose objects are exactly `selection`.
    pub fn level_of(&self, selection: &BTreeSet<ObjectId>) -> Option<usize> {
        self.levels.iter().position(|l| {
            l.ids.len() == selection.len() && l.ids.iter().all(|i| selection.contains(i))
        })
    }
}

fn capitalised(s: &str) -> String {
    let mut c = s.chars();
    c.next()
        .map(|f| f.to_uppercase().chain(c).collect())
        .unwrap_or_default()
}

fn level(kind: LevelKind, ids: Vec<ObjectId>, label: String) -> Level {
    Level { kind, ids, label }
}

/// The level of an object by its own name (`block #1` → "Block #1").
fn named(doc: &Document, kind: LevelKind, id: ObjectId, fallback: &str) -> Level {
    let label = doc
        .get(id)
        .and_then(|o| o.name.as_deref())
        .map(capitalised)
        .unwrap_or_else(|| fallback.to_string());
    level(kind, vec![id], label)
}

/// The chain of `hit`, the object a click picked (the deepest one).
pub fn chain_of(doc: &Document, hit: ObjectId) -> Chain {
    let levels = levels_of(doc, hit, 0);
    let start = usize::from(
        levels.len() >= 2
            && levels[1].kind == LevelKind::Street
            && !street_on_edge(doc, levels[0].ids[0], &levels[1].ids),
    );
    Chain { levels, start }
}

fn levels_of(doc: &Document, id: ObjectId, depth: usize) -> Vec<Level> {
    let parent = (depth < MAX_DEPTH)
        .then(|| doc.subdiv_source_of(id))
        .flatten();
    let Some(src) = parent else {
        return vec![match doc.subdiv_link(id) {
            Some(link) => level(LevelKind::Site, vec![id], site_label(link).into()),
            None => named(doc, LevelKind::Object, id, "Object"),
        }];
    };
    let mut up = levels_of(doc, src, depth + 1);
    if up.len() > 1 {
        // A generated output that is itself subdivided reads as a block.
        let last = up.last_mut().expect("non-empty");
        if last.kind == LevelKind::Part {
            last.kind = LevelKind::Block;
        }
    }
    let link = doc.subdiv_link(src).expect("source has a link");
    up.extend(levels_below(doc, src, link, id));
    up
}

fn site_label(link: &SubdivLink) -> &'static str {
    match link.kind {
        SubdivKind::Lots => "Subdivision",
        SubdivKind::Site | SubdivKind::SitePlan => "Site plan",
    }
}

/// The levels between generator source `src` and its output `id`, `id`'s own
/// level last.
fn levels_below(doc: &Document, src: ObjectId, link: &SubdivLink, id: ObjectId) -> Vec<Level> {
    let Some(obj) = doc.get(id) else {
        return Vec::new();
    };
    if let Some(st) = &obj.street {
        let ids: Vec<ObjectId> = std::iter::once(id)
            .chain(link.produced.iter().copied().filter(|r| {
                *r != id
                    && doc
                        .get(*r)
                        .and_then(|o| o.street.as_ref())
                        .is_some_and(|s| s.id == st.id)
            }))
            .collect();
        let label = street_view(doc, src, st.id)
            .and_then(|v| v.name)
            .unwrap_or_else(|| format!("Street {}", st.id));
        return vec![level(LevelKind::Street, ids, label)];
    }
    match obj.layer.as_str() {
        BLOCKS_LAYER => vec![named(doc, LevelKind::Block, id, "Block")],
        LOTS_LAYER => {
            let mut v = block_of(doc, link, id);
            v.push(named(doc, LevelKind::Lot, id, "Lot"));
            v
        }
        BUILDINGS_LAYER | SETBACKS_LAYER => {
            let mut v = Vec::new();
            if let Some(lot) = lot_under(doc, link, id) {
                v = block_of(doc, link, lot);
                v.push(named(doc, LevelKind::Lot, lot, "Lot"));
            }
            v.push(if obj.layer == BUILDINGS_LAYER {
                level(
                    LevelKind::Building,
                    building_ids(doc, link, id),
                    "Building".into(),
                )
            } else {
                named(doc, LevelKind::Part, id, "Setback")
            });
            v
        }
        _ => vec![named(doc, LevelKind::Part, id, "Part")],
    }
}

/// The Block level holding `lot` (a produced block containing its centroid).
fn block_of(doc: &Document, link: &SubdivLink, lot: ObjectId) -> Vec<Level> {
    let Some(probe) = doc.get(lot).and_then(plan_probe) else {
        return Vec::new();
    };
    link.produced
        .iter()
        .filter_map(|b| doc.get(*b))
        .filter(|b| b.layer == BLOCKS_LAYER)
        .find(|b| match &b.geometry {
            Geometry::Curve(c) => curve_to_polygon(c).is_some_and(|p| p.contains(probe)),
            _ => false,
        })
        .map(|b| named(doc, LevelKind::Block, b.id, "Block"))
        .into_iter()
        .collect()
}

/// A building's footprint and mass: baked as a consecutive pair, footprint
/// first ([`super::insert_buildings`]).
fn building_ids(doc: &Document, link: &SubdivLink, id: ObjectId) -> Vec<ObjectId> {
    let p = &link.produced;
    let is = |i: Option<usize>, prefix: &str| {
        i.and_then(|i| p.get(i))
            .and_then(|x| doc.get(*x))
            .is_some_and(|o| {
                o.layer == BUILDINGS_LAYER
                    && o.name.as_deref().is_some_and(|n| n.starts_with(prefix))
            })
    };
    let Some(i) = p.iter().position(|x| *x == id) else {
        return vec![id];
    };
    let partner = if is(Some(i), "building:footprint") && is(Some(i + 1), "building:mass") {
        Some(p[i + 1])
    } else if is(Some(i), "building:mass") && is(i.checked_sub(1), "building:footprint") {
        Some(p[i - 1])
    } else {
        None
    };
    std::iter::once(id).chain(partner).collect()
}

/// Distance from `p` to the closed polygon `ring`.
fn dist_to_ring(p: DVec2, ring: &[DVec2]) -> f64 {
    let n = ring.len();
    (0..n)
        .map(|i| {
            let (a, b) = (ring[i], ring[(i + 1) % n]);
            let ab = b - a;
            let t = if ab.length_squared() <= f64::EPSILON {
                0.0
            } else {
                ((p - a).dot(ab) / ab.length_squared()).clamp(0.0, 1.0)
            };
            (p - (a + ab * t)).length()
        })
        .fold(f64::INFINITY, f64::min)
}

/// A street lies on the site's edge when its whole centreline is on, or
/// within its width of, the boundary of the generator source `src` (#199).
/// Such a street can be mistaken for the boundary, so a click on it selects
/// the site first.
pub fn street_on_edge(doc: &Document, src: ObjectId, roads: &[ObjectId]) -> bool {
    let Some(ring) = doc.get(src).and_then(|o| match &o.geometry {
        Geometry::Curve(c) => curve_to_polygon(c),
        _ => None,
    }) else {
        return false;
    };
    let ring: Vec<DVec2> = ring.verts().to_vec();
    if ring.len() < 2 {
        return false;
    }
    roads.iter().filter_map(|r| doc.get(*r)).all(|o| {
        let (Geometry::Curve(c), Some(st)) = (&o.geometry, &o.street) else {
            return false;
        };
        let pts: Vec<DVec3> = c.tessellate(super::BLOCK_TOL);
        let step = (st.width * 0.5).max(0.5);
        let near = |p: DVec2| dist_to_ring(p, &ring) <= st.width + 1e-6;
        pts.first().is_some_and(|p| near(p.truncate()))
            && pts.windows(2).all(|w| {
                let (a, b) = (w[0].truncate(), w[1].truncate());
                let n = ((b - a).length() / step).ceil().max(1.0) as usize;
                (1..=n).all(|k| near(a.lerp(b, k as f64 / n as f64)))
            })
    })
}

/// How deep a generated layer sits: a click that hits a block edge looks
/// for a lot, then a building, on that same line (their edges coincide).
fn layer_rank(layer: &str) -> u8 {
    match layer {
        BLOCKS_LAYER => 1,
        LOTS_LAYER => 2,
        SETBACKS_LAYER => 3,
        BUILDINGS_LAYER => 4,
        _ => 0,
    }
}

/// The chain under a click: [`itsjustcad_doc::pick::pick_object`]'s hit,
/// resolved to the deepest generated child on the same spot. A hit on a
/// generator's source (the site boundary) looks again among that generator's
/// outputs, and a block or lot edge looks for the lot or building beneath, so
/// a click can drill all the way down from where it landed.
pub fn pick_chain(
    doc: &Document,
    vp: Mat4,
    size: Vec2,
    pos: Vec2,
    origin: DVec3,
    dir: DVec3,
) -> Option<Chain> {
    use itsjustcad_doc::pick::{pick_object, pick_object_where};
    let mut hit = pick_object(doc, vp, size, pos, origin, dir)?;
    let src = if doc.subdiv_link(hit).is_some() {
        Some(hit)
    } else {
        doc.subdiv_source_of(hit)
    };
    if let Some(link) = src.and_then(|s| doc.subdiv_link(s)) {
        let kids: HashSet<ObjectId> = link.produced.iter().copied().collect();
        let mut rank = if hit == src.expect("some") {
            None
        } else {
            doc.get(hit).map(|o| layer_rank(&o.layer))
        };
        while rank != Some(0) {
            let deeper = pick_object_where(doc, vp, size, pos, origin, dir, |o| {
                kids.contains(&o.id)
                    && rank.is_none_or(|r| o.street.is_none() && layer_rank(&o.layer) > r)
            });
            let Some(d) = deeper else { break };
            hit = d;
            rank = doc.get(d).map(|o| {
                if o.street.is_some() {
                    0
                } else {
                    layer_rank(&o.layer)
                }
            });
            if rank == Some(layer_rank(BUILDINGS_LAYER)) {
                break;
            }
        }
    }
    Some(chain_of(doc, hit))
}

/// The top-parent objects of `ids` (box and lasso select pick parents,
/// #199): each generated object becomes its generator's top source.
pub fn top_parents(doc: &Document, ids: impl IntoIterator<Item = ObjectId>) -> BTreeSet<ObjectId> {
    ids.into_iter()
        .flat_map(|id| chain_of(doc, id).levels.swap_remove(0).ids)
        .collect()
}

/// The generator source at the top of `id`'s chain (itself when no generator
/// produced it).
fn top_source(doc: &Document, mut id: ObjectId) -> ObjectId {
    for _ in 0..MAX_DEPTH {
        match doc.subdiv_source_of(id) {
            Some(src) => id = src,
            None => break,
        }
    }
    id
}

/// Every object a generator under `root` produced, nested generators
/// included.
fn descendants(doc: &Document, root: ObjectId) -> Vec<ObjectId> {
    let mut out = Vec::new();
    let mut stack = vec![(root, 0)];
    while let Some((src, depth)) = stack.pop() {
        let Some(link) = doc.subdiv_link(src).filter(|_| depth < MAX_DEPTH) else {
            continue;
        };
        for id in &link.produced {
            out.push(*id);
            stack.push((*id, depth + 1));
        }
    }
    out
}

/// The objects that stay in full colour while `parent` is the edit context
/// (#199): `parent` and everything below it in the hierarchy.
pub fn context_members(doc: &Document, parent: ObjectId) -> HashSet<ObjectId> {
    let mut out = HashSet::from([parent]);
    let root = top_source(doc, parent);
    for id in descendants(doc, root) {
        if out.contains(&id) {
            continue;
        }
        let c = chain_of(doc, id);
        if let Some(l) = c.levels.iter().position(|l| l.ids.contains(&parent)) {
            for lv in &c.levels[l..] {
                out.extend(lv.ids.iter().copied());
            }
        }
    }
    out
}

/// "Select Same Type" (#199): every object of the same level kind as `id`'s
/// own level, within its top parent (all lots, all buildings, all streets
/// of the site plan). Just `id`'s level for a plain object.
pub fn same_type(doc: &Document, id: ObjectId) -> BTreeSet<ObjectId> {
    let c = chain_of(doc, id);
    let own = &c.levels[c.last()];
    if c.levels.len() < 2 {
        return own.ids.iter().copied().collect();
    }
    let kind = own.kind;
    let layer = doc.get(id).map(|o| o.layer.clone());
    descendants(doc, c.root())
        .into_iter()
        .filter_map(|d| doc.get(d))
        .filter(|o| match kind {
            LevelKind::Street => o.street.is_some(),
            LevelKind::Building => o.layer == BUILDINGS_LAYER,
            _ => o.street.is_none() && Some(&o.layer) == layer.as_ref(),
        })
        .filter(|o| {
            // Same kind at its own level (a subdivided block stays a block).
            let oc = chain_of(doc, o.id);
            oc.levels[oc.last()].kind == kind
        })
        .map(|o| o.id)
        .collect()
}

/// A road's centreline endpoints and the unit directions leaving them.
fn road_ends(o: &itsjustcad_doc::SceneObject) -> Option<[(DVec2, DVec2); 2]> {
    let Geometry::Curve(c) = &o.geometry else {
        return None;
    };
    let p: Vec<DVec2> = c
        .tessellate(super::BLOCK_TOL)
        .iter()
        .map(|v| v.truncate())
        .collect();
    let n = p.len();
    if n < 2 {
        return None;
    }
    Some([
        (p[0], (p[0] - p[1]).normalize_or_zero()),
        (p[n - 1], (p[n - 1] - p[n - 2]).normalize_or_zero()),
    ])
}

/// Streets continue through a junction when they turn by less than this.
const CONTINUE_DEG: f64 = 30.0;

/// "Select Whole Street" (#199, CityEngine's Select Continuous Graph
/// Objects): the roads of `id`'s street plus, through each junction, the
/// best-aligned road of the same class that carries on within
/// [`CONTINUE_DEG`]. Empty when `id` is not a generated road.
pub fn whole_street(doc: &Document, id: ObjectId) -> BTreeSet<ObjectId> {
    let Some(src) = doc.subdiv_source_of(id) else {
        return BTreeSet::new();
    };
    let Some(class) = doc.get(id).and_then(|o| o.street.as_ref()).map(|s| s.class) else {
        return BTreeSet::new();
    };
    let link = doc.subdiv_link(src).expect("source has a link");
    let roads: Vec<(ObjectId, [(DVec2, DVec2); 2])> = link
        .produced
        .iter()
        .filter_map(|r| doc.get(*r))
        .filter(|o| o.street.as_ref().is_some_and(|s| s.class == class))
        .filter_map(|o| Some((o.id, road_ends(o)?)))
        .collect();
    let cos = CONTINUE_DEG.to_radians().cos();
    let mut out = BTreeSet::from([id]);
    let mut stack = vec![id];
    while let Some(r) = stack.pop() {
        let Some((_, ends)) = roads.iter().find(|(x, _)| *x == r) else {
            continue;
        };
        for (at, out_dir) in ends {
            // Leaving `r` at `at` along `-out_dir` reversed: the next road
            // starts at `at` and heads on along `out_dir`.
            let next = roads
                .iter()
                .filter(|(x, _)| !out.contains(x))
                .flat_map(|(x, e)| e.iter().map(move |(p, d)| (*x, *p, -*d)))
                .filter(|(_, p, _)| p.distance(*at) < 1e-3)
                .map(|(x, _, d)| (x, d.dot(*out_dir)))
                .filter(|(_, c)| *c >= cos)
                .max_by(|a, b| a.1.total_cmp(&b.1));
            if let Some((x, _)) = next {
                out.insert(x);
                stack.push(x);
            }
        }
    }
    // The other road objects of the streets reached.
    let ids: BTreeSet<u32> = out
        .iter()
        .filter_map(|r| doc.get(*r)?.street.as_ref().map(|s| s.id))
        .collect();
    out.extend(roads.iter().map(|(x, _)| *x).filter(|x| {
        doc.get(*x)
            .and_then(|o| o.street.as_ref())
            .is_some_and(|s| ids.contains(&s.id))
    }));
    out
}

/// The entered level of a hierarchical selection (Figma-style, #199): after
/// a double-click drills in, plain clicks select at that depth inside the
/// entered parent. A click on empty space, on an object outside the entered
/// parent (drawn faded), or Esc at the top resets it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Drill {
    root: Option<ObjectId>,
    depth: usize,
    /// The entered parent: the level above `depth` (its first object).
    parent: Option<ObjectId>,
}

impl Drill {
    /// The entered depth inside the current top parent (0: not entered).
    pub fn depth(&self) -> usize {
        self.depth
    }

    /// Whether a double-click has drilled into a parent.
    pub fn entered(&self) -> bool {
        self.depth > 0
    }

    /// The entered parent, whose subtree stays in full colour (the SketchUp
    /// edit context) and bounds box and lasso selection. `None` at the top.
    pub fn parent(&self) -> Option<ObjectId> {
        self.parent.filter(|_| self.entered())
    }

    /// The top parent of the entered chain, when entered.
    pub fn root(&self) -> Option<ObjectId> {
        self.root.filter(|_| self.entered())
    }

    /// Whether `chain` lies inside the entered parent.
    pub fn inside(&self, chain: &Chain) -> bool {
        self.entered()
            && self.root == Some(chain.root())
            && self.parent.is_some_and(|p| {
                chain
                    .levels
                    .get(self.depth - 1)
                    .is_some_and(|l| l.ids.contains(&p))
            })
    }

    /// The level a plain click on `chain` selects: the entered depth inside
    /// the entered parent, else what a fresh first click selects.
    pub fn click_level(&self, chain: &Chain) -> usize {
        if self.inside(chain) {
            self.depth.min(chain.last()).max(chain.start)
        } else {
            chain.start
        }
    }

    /// A plain click on `chain`: the level to select. A click outside the
    /// entered parent (a faded object) leaves the entered level.
    pub fn click(&mut self, chain: &Chain) -> usize {
        let l = self.click_level(chain);
        if !self.inside(chain) {
            self.reset();
        }
        l
    }

    /// A Shift-click on `chain` (add or remove): the level it toggles, the
    /// current depth in the same top parent; the drill is unchanged.
    pub fn add_level(&self, chain: &Chain) -> usize {
        if self.entered() && self.root == Some(chain.root()) {
            self.depth.min(chain.last()).max(chain.start)
        } else {
            chain.start
        }
    }

    /// A double-click on `chain`, with `selection` the current selection: one
    /// level below the selected level of this chain (or below what a click
    /// would select), clamped to the deepest. Enters that level.
    pub fn double_click(&mut self, chain: &Chain, selection: &BTreeSet<ObjectId>) -> usize {
        let base = chain
            .level_of(selection)
            .unwrap_or_else(|| self.click_level(chain));
        let next = (base + 1).min(chain.last());
        self.enter(chain, next);
        next
    }

    /// Esc: up ONE level from the selected `level` of `chain` (#199). The
    /// level to select, entered unless it is the top; `None` at the top (the
    /// host's usual Esc applies, and the drill resets).
    pub fn up(&mut self, chain: &Chain, level: usize) -> Option<usize> {
        if level == 0 || level > chain.last() {
            self.reset();
            return None;
        }
        let l = level - 1;
        self.enter(chain, l);
        Some(l)
    }

    /// Enter `level` of `chain` directly (a breadcrumb click); level 0 resets.
    pub fn enter(&mut self, chain: &Chain, level: usize) {
        let depth = level.min(chain.last());
        if depth == 0 {
            self.reset();
            return;
        }
        self.root = Some(chain.root());
        self.depth = depth;
        self.parent = Some(chain.levels[depth - 1].ids[0]);
    }

    /// Back to the top level (a click on empty space).
    pub fn reset(&mut self) {
        *self = Self::default();
    }
}

/// After an edit re-derived the plan (fresh ids): the level to go to when
/// the edited object has no match. The nearest level above `level` of the
/// pre-edit `chain` whose objects still `exist`, else the top (#199).
pub fn fallback_level(chain: &Chain, level: usize, exist: impl Fn(&[ObjectId]) -> bool) -> usize {
    (1..level.min(chain.levels.len()))
        .rev()
        .find(|l| exist(&chain.levels[*l].ids))
        .unwrap_or(0)
}

/// The objects a box or lasso over `hits` selects (#199): their top parents
/// at the top level; while drilled into `parent`, only objects inside it,
/// at the level just below it.
pub fn region_targets(
    doc: &Document,
    hits: impl IntoIterator<Item = ObjectId>,
    parent: Option<ObjectId>,
) -> BTreeSet<ObjectId> {
    let Some(parent) = parent else {
        return top_parents(doc, hits);
    };
    hits.into_iter()
        .flat_map(|id| {
            let c = chain_of(doc, id);
            c.levels
                .iter()
                .position(|l| l.ids.contains(&parent))
                .and_then(|i| c.levels.get(i + 1))
                .map(|l| l.ids.clone())
                .unwrap_or_default()
        })
        .collect()
}

/// Whether deleting `id` is an edit the core supports (#199): a plain
/// object, a generator's source (the whole site plan) or a generated road
/// (`delete` removes the street and re-derives). Generated blocks, lots,
/// buildings and other outputs are not deletable.
pub fn deletable(doc: &Document, id: ObjectId) -> bool {
    doc.subdiv_source_of(id).is_none() || doc.get(id).is_some_and(|o| o.street.is_some())
}

/// Whether the gumball may move `id` (#199): only hand-drawn objects and
/// top-level parents, never generated children.
pub fn gumball_allowed(doc: &Document, id: ObjectId) -> bool {
    doc.subdiv_source_of(id).is_none()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Session, parse};
    use kernel_curve::Curve;

    fn site_plan() -> Session {
        let mut s = Session::default();
        for line in ["rect 0,0,0 200 150", "siteplan last"] {
            s.run(parse(line).unwrap()).unwrap();
        }
        s
    }

    fn source(doc: &Document) -> ObjectId {
        *doc.subdivision_links.keys().next().unwrap()
    }

    fn road(doc: &Document) -> ObjectId {
        doc.objects().find(|o| o.street.is_some()).unwrap().id
    }

    fn kinds(c: &Chain) -> Vec<&'static str> {
        c.levels.iter().map(|l| l.kind.token()).collect()
    }

    #[test]
    fn interior_street_is_picked_directly() {
        let s = site_plan();
        let c = chain_of(&s.doc, road(&s.doc));
        assert_eq!(kinds(&c), ["site", "street"]);
        assert_eq!(c.root(), source(&s.doc));
        assert_eq!(c.levels[0].label, "Site plan");
        assert_eq!(c.start, 1, "x=100 street is interior");
    }

    #[test]
    fn boundary_street_selects_the_site_first() {
        let mut s = site_plan();
        let r = road(&s.doc);
        // Run the street along the bottom edge, 5 m in (width 12).
        s.doc.get_mut(r).unwrap().geometry = Geometry::Curve(Curve::Polyline {
            points: vec![DVec3::new(0.0, 5.0, 0.0), DVec3::new(200.0, 5.0, 0.0)],
            closed: false,
        });
        let c = chain_of(&s.doc, r);
        assert_eq!(kinds(&c), ["site", "street"]);
        assert_eq!(c.start, 0);
        let mut d = Drill::default();
        assert_eq!(d.click(&c), 0, "click → site plan");
        let sel = c.levels[0].ids.iter().copied().collect();
        assert_eq!(d.double_click(&c, &sel), 1, "double-click → the street");
    }

    #[test]
    fn lot_to_building_chain() {
        let s = site_plan();
        let doc = &s.doc;
        let mass = doc
            .objects()
            .find(|o| o.layer == BUILDINGS_LAYER && matches!(o.geometry, Geometry::Mesh(_)))
            .unwrap()
            .id;
        let c = chain_of(doc, mass);
        assert_eq!(kinds(&c), ["site", "block", "lot", "building"]);
        assert_eq!(c.start, 0);
        assert_eq!(c.levels[3].ids.len(), 2, "footprint + mass");
        assert!(c.levels[3].ids.contains(&mass));
        let lot = c.levels[2].ids[0];
        assert_eq!(doc.get(lot).unwrap().layer, LOTS_LAYER);
        assert_eq!(kinds(&chain_of(doc, lot)), ["site", "block", "lot"]);
        assert_eq!(kinds(&chain_of(doc, source(doc))), ["site"]);

        // Click → site; each double-click one level deeper, clamped.
        let mut d = Drill::default();
        let mut sel: BTreeSet<ObjectId> = c.levels[d.click(&c)].ids.iter().copied().collect();
        for want in [1, 2, 3, 3] {
            let l = d.double_click(&c, &sel);
            assert_eq!(l, want);
            sel = c.levels[l].ids.iter().copied().collect();
        }
        // Entered: a click on a sibling building stays at building level.
        assert_eq!(d.click(&c), 3);
        // The edit context is the lot: it and its building stay in colour.
        let lot = c.levels[2].ids[0];
        assert_eq!(d.parent(), Some(lot));
        let members = context_members(doc, lot);
        assert!(members.contains(&lot) && c.levels[3].ids.iter().all(|i| members.contains(i)));
        assert!(!members.contains(&source(doc)) && !members.contains(&road(doc)));
        assert!(
            context_members(doc, source(doc)).contains(&road(doc)),
            "the site holds all"
        );
        // Esc goes up ONE level at a time, then hands over at the top.
        assert_eq!(d.up(&c, 3), Some(2));
        assert_eq!(d.click(&c), 2, "lot level entered");
        assert_eq!(d.up(&c, 2), Some(1));
        assert_eq!(d.up(&c, 1), Some(0));
        assert!(!d.entered(), "back at the top");
        assert_eq!(d.parent(), None, "no fade at the top");
        assert_eq!(d.up(&c, 0), None);
        assert_eq!(d.click(&c), 0);
    }

    #[test]
    fn entered_level_is_kept_for_siblings_and_clamped() {
        let s = site_plan();
        let doc = &s.doc;
        let lot = doc.objects().find(|o| o.layer == LOTS_LAYER).unwrap().id;
        let lots = chain_of(doc, lot);
        let street = chain_of(doc, road(doc));
        let mut d = Drill::default();
        d.double_click(&lots, &BTreeSet::from([source(doc)]));
        assert_eq!(d.depth(), 1);
        d.double_click(&lots, &lots.levels[1].ids.iter().copied().collect());
        assert_eq!(d.depth(), 2, "entered lot level");
        assert_eq!(d.click(&lots), 2, "a lot of the entered block: lot level");
        // A lot of the other block is faded: a tap there starts fresh.
        let block = lots.levels[1].ids[0];
        let other = doc
            .objects()
            .filter(|o| o.layer == LOTS_LAYER)
            .map(|o| chain_of(doc, o.id))
            .find(|c| c.levels[1].ids[0] != block)
            .unwrap();
        assert!(!d.inside(&other));
        assert_eq!(
            d.add_level(&other),
            2,
            "Shift-click still adds at lot level"
        );
        assert_eq!(
            d.click(&other),
            0,
            "outside the entered block: its site plan"
        );
        assert!(!d.entered());
        d.double_click(&lots, &BTreeSet::from([source(doc)]));
        assert_eq!(d.click(&street), 1, "inside the site: street level");
        assert!(d.entered());
        d.double_click(&lots, &BTreeSet::from([block]));
        assert_eq!(d.click(&street), 1, "the street is outside the block");
        assert!(!d.entered());
        assert_eq!(d.click(&lots), 0);
        assert_eq!(d.up(&lots, 0), None);
    }

    #[test]
    fn box_inside_the_entered_parent_delete_and_gumball_rules() {
        let s = site_plan();
        let doc = &s.doc;
        let all: Vec<ObjectId> = doc.objects().map(|o| o.id).collect();
        assert_eq!(
            region_targets(doc, all.clone(), None),
            BTreeSet::from([source(doc)])
        );
        let lot = doc.objects().find(|o| o.layer == LOTS_LAYER).unwrap().id;
        let c = chain_of(doc, lot);
        let block = c.levels[1].ids[0];
        let lots = region_targets(doc, all.clone(), Some(block));
        assert_eq!(lots.len(), 2, "the block's two lots, not the other block's");
        assert!(
            lots.iter()
                .all(|l| doc.get(*l).unwrap().layer == LOTS_LAYER)
        );
        let at_site = region_targets(doc, all, Some(source(doc)));
        assert!(
            at_site.contains(&block) && at_site.contains(&road(doc)),
            "blocks and streets"
        );

        assert!(deletable(doc, source(doc)) && deletable(doc, road(doc)));
        assert!(!deletable(doc, lot) && !deletable(doc, block));
        assert!(gumball_allowed(doc, source(doc)));
        assert!(!gumball_allowed(doc, road(doc)) && !gumball_allowed(doc, lot));

        // After a re-derive: the nearest surviving level above, else the top.
        let b = chain_of(
            doc,
            doc.objects()
                .find(|o| o.layer == BUILDINGS_LAYER)
                .unwrap()
                .id,
        );
        assert_eq!(fallback_level(&b, 3, |_| true), 2, "the lot");
        assert_eq!(
            fallback_level(&b, 3, |ids| ids == b.levels[1].ids.as_slice()),
            1
        );
        assert_eq!(fallback_level(&b, 3, |_| false), 0);
        assert_eq!(fallback_level(&b, 1, |_| true), 0);
    }

    #[test]
    fn same_type_selects_every_sibling_kind_of_the_site_plan() {
        let s = site_plan();
        let doc = &s.doc;
        let lot = doc.objects().find(|o| o.layer == LOTS_LAYER).unwrap().id;
        assert_eq!(same_type(doc, lot).len(), 4, "all four lots");
        let mass = doc
            .objects()
            .find(|o| o.layer == BUILDINGS_LAYER && matches!(o.geometry, Geometry::Mesh(_)))
            .unwrap()
            .id;
        assert_eq!(
            same_type(doc, mass).len(),
            8,
            "four buildings, footprint + mass"
        );
        assert_eq!(same_type(doc, road(doc)), BTreeSet::from([road(doc)]));
    }

    #[test]
    fn whole_street_carries_on_through_straight_junctions() {
        let mut s = Session::default();
        for line in ["rect 0,0,0 600 450", "siteplan last"] {
            s.run(parse(line).unwrap()).unwrap();
        }
        let doc = &s.doc;
        let ends = |id: ObjectId| road_ends(doc.get(id).unwrap()).unwrap();
        let roads: Vec<ObjectId> = doc
            .objects()
            .filter(|o| o.street.is_some())
            .map(|o| o.id)
            .collect();
        assert!(roads.len() > 2, "a grid of streets");
        for r in &roads {
            let all = whole_street(doc, *r);
            assert!(all.contains(r));
            // Every road reached runs parallel to the first one.
            let d0 = ends(*r)[0].1;
            for x in &all {
                assert!(
                    ends(*x)[0].1.dot(d0).abs() > 0.86,
                    "{x:?} turns off the street"
                );
            }
        }
        // Some road continues through a junction into another road.
        assert!(roads.iter().any(|r| whole_street(doc, *r).len() > 1));
        assert!(whole_street(doc, source(doc)).is_empty());
    }

    #[test]
    fn plain_object_is_its_own_chain_and_box_picks_parents() {
        let mut s = site_plan();
        s.run(parse("line 0,0,0 10,10,0").unwrap()).unwrap();
        let doc = &s.doc;
        let line = doc.objects().last().unwrap().id;
        let c = chain_of(doc, line);
        assert_eq!(kinds(&c), ["object"]);
        let lot = doc.objects().find(|o| o.layer == LOTS_LAYER).unwrap().id;
        assert_eq!(
            top_parents(doc, [lot, line, road(doc)]),
            BTreeSet::from([source(doc), line])
        );
    }

    #[test]
    fn a_click_on_the_boundary_resolves_to_the_child_beneath() {
        let s = site_plan();
        let doc = &s.doc;
        // Top view of the 200 × 150 site in a 400 × 300 viewport.
        let view = Mat4::look_at_rh(
            glam::Vec3::new(100.0, 75.0, 100.0),
            glam::Vec3::new(100.0, 75.0, 0.0),
            glam::Vec3::Y,
        );
        let vp = Mat4::orthographic_rh(-100.0, 100.0, -75.0, 75.0, 0.1, 1000.0) * view;
        let size = Vec2::new(400.0, 300.0);
        let at = |x: f32, y: f32| {
            let pos = Vec2::new(x * 2.0, (150.0 - y) * 2.0);
            pick_chain(
                doc,
                vp,
                size,
                pos,
                DVec3::new(x as f64, y as f64, 100.0),
                DVec3::NEG_Z,
            )
        };
        // On the left boundary edge, mid-lot: down to the building whose
        // footprint runs 1 m inside it (2 px at this zoom).
        let c = at(0.0, 110.0).unwrap();
        assert_eq!(kinds(&c), ["site", "block", "lot", "building"]);
        // Inside a lot, away from every line: the building mass's box.
        let c = at(50.0, 40.0).unwrap();
        assert_eq!(kinds(&c), ["site", "block", "lot", "building"]);
        // On the interior street: the street, picked directly.
        let c = at(100.0, 40.0).unwrap();
        assert_eq!((kinds(&c), c.start), (vec!["site", "street"], 1));
    }
}
