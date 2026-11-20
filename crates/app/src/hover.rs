//! Hover pre-highlight (#374): the objects a click would select light up
//! under the cursor before the click (SketchUp).
//!
//! The pick is the click's own ([`click_targets`] serves both), recomputed
//! only when its [`HoverKey`] changes: a still pointer over a still camera
//! and an unchanged document costs one comparison per frame. The target's
//! world wireframe is cached with it, so a frame only projects segments for
//! the egui overlay; the GPU scene is never re-uploaded for hover.

use std::collections::BTreeSet;

use glam::DVec3;
use itsjustcad_commands::lot::hierarchy::{Chain, Drill, LevelKind};
use itsjustcad_doc::{Document, ObjectId};

/// Above this many segments the overlay draws the targets' bounding box
/// instead, so hovering a huge mesh stays one cheap frame.
const MAX_SEGS: usize = 20_000;

/// The ids a click on `chain` selects, before the selection toggles: a
/// generated object at its drilled level (#199, Shift at the same depth),
/// otherwise the hit, expanded to its whole group unless `expand` is off
/// (Cmd held). Shared by [`crate::app::App`]'s click pick and the hover.
pub fn click_targets(
    doc: &Document,
    drill: &Drill,
    chain: &Chain,
    additive: bool,
    expand: bool,
) -> BTreeSet<ObjectId> {
    if chain.levels[0].kind != LevelKind::Object {
        let l = if additive { drill.add_level(chain) } else { drill.click_level(chain) };
        return chain.levels[l].ids.iter().copied().collect();
    }
    let id = chain.levels[0].ids[0];
    if expand { doc.expand_pick(id) } else { BTreeSet::from([id]) }
}

/// What a hover shows for `targets`: nothing already selected (it keeps its
/// selection colour) and nothing on a locked layer.
pub fn hover_filter(doc: &Document, targets: BTreeSet<ObjectId>) -> BTreeSet<ObjectId> {
    targets
        .into_iter()
        .filter(|id| !doc.selection.contains(id) && !doc.object_locked(*id))
        .collect()
}

/// Everything the hover pick depends on. Any change recomputes it.
#[derive(Clone, Debug, PartialEq)]
pub struct HoverKey {
    pub pane: usize,
    pub pos: [f32; 2],
    pub view_proj: [f32; 16],
    pub generation: u64,
    /// The pick rule: Shift / Cmd for a select click, or the guided step.
    pub mode: usize,
}

#[derive(Default)]
pub struct Hover {
    key: Option<HoverKey>,
    ids: BTreeSet<ObjectId>,
    segs: Vec<[DVec3; 2]>,
    /// Picks computed so far (tests: a still pointer computes none).
    #[cfg(test)]
    pub computes: usize,
}

impl Hover {
    /// No hover (tools, orbit, pan, drags, pointer off the viewport).
    pub fn clear(&mut self) {
        self.key = None;
        self.ids.clear();
        self.segs.clear();
    }

    /// Hover for `key`, running `pick` only when the key changed.
    pub fn update(
        &mut self,
        doc: &Document,
        key: HoverKey,
        pick: impl FnOnce() -> BTreeSet<ObjectId>,
    ) {
        if self.key.as_ref() == Some(&key) {
            return;
        }
        #[cfg(test)]
        {
            self.computes += 1;
        }
        self.key = Some(key);
        let ids = pick();
        if ids != self.ids {
            self.segs = wireframe(doc, &ids);
            self.ids = ids;
        }
    }

    #[cfg(test)]
    pub fn ids(&self) -> &BTreeSet<ObjectId> {
        &self.ids
    }

    /// The pane the hover belongs to, if any.
    pub fn pane(&self) -> Option<usize> {
        self.key.as_ref().filter(|_| !self.ids.is_empty()).map(|k| k.pane)
    }

    /// The hovered objects' world-space outline.
    pub fn segments(&self) -> &[[DVec3; 2]] {
        &self.segs
    }
}

/// The objects' display wireframe; their bounding box when that is empty
/// (points, text, instances) or too dense to draw every frame.
fn wireframe(doc: &Document, ids: &BTreeSet<ObjectId>) -> Vec<[DVec3; 2]> {
    let objs: Vec<_> = ids.iter().filter_map(|id| doc.get(*id)).collect();
    let mut segs: Vec<[DVec3; 2]> = Vec::new();
    for o in &objs {
        segs.extend(itsjustcad_render::object_wireframe_world(o));
        if segs.len() > MAX_SEGS {
            break;
        }
    }
    if !segs.is_empty() && segs.len() <= MAX_SEGS {
        return segs;
    }
    let Some((min, max)) = objs.iter().map(|o| doc.geometry_aabb(&o.geometry)).fold(
        None,
        |acc: Option<(DVec3, DVec3)>, b| {
            Some(acc.map_or((b.min, b.max), |(lo, hi)| (lo.min(b.min), hi.max(b.max))))
        },
    ) else {
        return Vec::new();
    };
    box_edges(min, max)
}

/// The 12 edges of the box `min`..`max`.
fn box_edges(min: DVec3, max: DVec3) -> Vec<[DVec3; 2]> {
    let c = |i: usize| {
        DVec3::new(
            if i & 1 == 0 { min.x } else { max.x },
            if i & 2 == 0 { min.y } else { max.y },
            if i & 4 == 0 { min.z } else { max.z },
        )
    };
    let mut e = Vec::with_capacity(12);
    for i in 0..8 {
        for bit in [1, 2, 4] {
            if i & bit == 0 {
                e.push([c(i), c(i | bit)]);
            }
        }
    }
    e
}

/// SketchUp-like blue, distinct from the amber selection on both themes.
pub fn color(dark: bool) -> egui::Color32 {
    if dark {
        egui::Color32::from_rgb(90, 170, 255)
    } else {
        egui::Color32::from_rgb(20, 105, 230)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use itsjustcad_commands::lot::hierarchy::chain_of;
    use itsjustcad_commands::{parse, Session};

    fn session(lines: &[&str]) -> Session {
        let mut s = Session::default();
        for l in lines {
            s.run(parse(l).unwrap()).unwrap();
        }
        s
    }

    fn ids(s: &Session) -> Vec<ObjectId> {
        let mut v: Vec<_> = s.doc.objects().map(|o| o.id).collect();
        v.sort();
        v
    }

    fn hover_of(s: &Session, hit: ObjectId, expand: bool) -> BTreeSet<ObjectId> {
        let c = chain_of(&s.doc, hit);
        hover_filter(&s.doc, click_targets(&s.doc, &Drill::default(), &c, false, expand))
    }

    #[test]
    fn hover_is_the_whole_group_a_click_selects() {
        let s = session(&["box 0,0,0 1,1,1", "box 3,0,0 4,1,1", "group last 2", "box 6,0,0 7,1,1"]);
        let all = ids(&s);
        let grouped: BTreeSet<_> = s.doc.groups.values().next().unwrap().clone();
        let member = *grouped.first().unwrap();
        assert_eq!(hover_of(&s, member, true), grouped, "group expands");
        assert_eq!(hover_of(&s, member, false), BTreeSet::from([member]), "Cmd bypasses");
        let loose = *all.iter().find(|i| !grouped.contains(i)).unwrap();
        assert_eq!(hover_of(&s, loose, true), BTreeSet::from([loose]));
    }

    #[test]
    fn no_hover_on_selected_or_locked() {
        let mut s = session(&["box 0,0,0 1,1,1", "layer walls", "box 3,0,0 4,1,1", "layerlock walls on"]);
        let locked = s.doc.objects().find(|o| o.layer == "walls").unwrap().id;
        let free = s.doc.objects().find(|o| o.layer != "walls").unwrap().id;
        assert!(hover_of(&s, locked, true).is_empty(), "locked layer: no hover");
        assert_eq!(hover_of(&s, free, true), BTreeSet::from([free]));
        s.doc.selection = BTreeSet::from([free]);
        assert!(hover_of(&s, free, true).is_empty(), "selected keeps its selection colour");
    }

    fn key(pos: [f32; 2], generation: u64) -> HoverKey {
        HoverKey { pane: 0, pos, view_proj: [0.0; 16], generation, mode: 0 }
    }

    #[test]
    fn still_pointer_does_not_recompute() {
        let s = session(&["box 0,0,0 1,1,1"]);
        let id = ids(&s)[0];
        let mut h = Hover::default();
        for _ in 0..5 {
            h.update(&s.doc, key([10.0, 10.0], 1), || BTreeSet::from([id]));
        }
        assert_eq!(h.computes, 1, "five frames, one pick");
        assert_eq!(h.segments().len(), 12, "a box shows its 12 edges");
        h.update(&s.doc, key([11.0, 10.0], 1), || BTreeSet::from([id]));
        h.update(&s.doc, key([11.0, 10.0], 2), BTreeSet::new);
        assert_eq!(h.computes, 3, "move and doc change recompute");
        assert!(h.ids().is_empty() && h.segments().is_empty() && h.pane().is_none());
    }

    #[test]
    fn clear_drops_the_hover_and_recomputes_after() {
        let s = session(&["box 0,0,0 1,1,1"]);
        let id = ids(&s)[0];
        let mut h = Hover::default();
        h.update(&s.doc, key([1.0, 1.0], 1), || BTreeSet::from([id]));
        assert_eq!(h.pane(), Some(0));
        h.clear();
        assert!(h.ids().is_empty() && h.pane().is_none());
        h.update(&s.doc, key([1.0, 1.0], 1), || BTreeSet::from([id]));
        assert_eq!(h.computes, 2, "a cleared hover picks again when the tool ends");
    }

    #[test]
    fn empty_wireframe_falls_back_to_the_box() {
        assert_eq!(box_edges(DVec3::ZERO, DVec3::ONE).len(), 12);
    }
}
