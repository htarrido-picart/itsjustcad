// SPDX-License-Identifier: AGPL-3.0-only
// Copyright © 2026 Hector Tarrido-Picart

//! What a selection is about (#159): one or more generated lots, one generated
//! street, or a whole subdivision. Both apps resolve the focused inspector
//! through [`focus_target`], so focus works the same on desktop and iOS/Mac.
//! [`lot_view`] and [`street_view`] are the data those inspectors show.

use std::collections::BTreeSet;

use glam::DVec2;
use itsjustcad_doc::{
    Document, EdgeTag, Geometry, ObjectId, SceneObject, StreetClass, SubdivKind, SubdivLink,
};
use subdivision::{FrontageAt, Polygon2d};

use super::{
    BUILDINGS_LAYER, LOTS_LAYER, LotRules, curve_to_polygon, doc_block, lot_rules, parse_mass_name,
};

/// The inspector a selection opens.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Focus {
    /// Nothing generated is selected (or several subdivisions are).
    None,
    /// Generated lots of one subdivision (`lots` in selection order, deduped):
    /// lots picked directly, or the lots under picked buildings.
    Lots {
        source: ObjectId,
        lots: Vec<ObjectId>,
    },
    /// One generated street: road object(s) sharing `street_id`.
    Street { source: ObjectId, street_id: u32 },
    /// The whole subdivision: its boundary, other outputs, or a mixed selection.
    Site(ObjectId),
}

impl Focus {
    /// The subdivision source block the focus belongs to.
    pub fn source(&self) -> Option<ObjectId> {
        match self {
            Focus::None => None,
            Focus::Lots { source, .. } | Focus::Street { source, .. } | Focus::Site(source) => {
                Some(*source)
            }
        }
    }
}

/// A point inside the footprint of `obj`: a closed curve's centroid, or the
/// mean of a mesh's vertices (x, y).
pub fn plan_probe(obj: &SceneObject) -> Option<DVec2> {
    match &obj.geometry {
        Geometry::Curve(c) => Some(curve_to_polygon(c)?.centroid()),
        Geometry::Mesh(m) => {
            let p = m.positions();
            (!p.is_empty()).then(|| p.iter().map(|v| v.truncate()).sum::<DVec2>() / p.len() as f64)
        }
        _ => None,
    }
}

/// The lot curves `link` produced, with their polygons, in bake order.
pub fn link_lots(doc: &Document, link: &SubdivLink) -> Vec<(ObjectId, Polygon2d)> {
    link.produced
        .iter()
        .filter_map(|id| {
            let o = doc.get(*id).filter(|o| o.layer == LOTS_LAYER)?;
            let Geometry::Curve(c) = &o.geometry else {
                return None;
            };
            Some((*id, curve_to_polygon(c)?))
        })
        .collect()
}

/// The lot of `link` that `id` is, or that it stands on (a building whose
/// footprint lies inside the lot).
pub fn lot_under(doc: &Document, link: &SubdivLink, id: ObjectId) -> Option<ObjectId> {
    let lots = link_lots(doc, link);
    if lots.iter().any(|(l, _)| *l == id) {
        return Some(id);
    }
    let probe = plan_probe(doc.get(id)?)?;
    lots.into_iter()
        .find(|(_, p)| p.contains(probe))
        .map(|(l, _)| l)
}

/// Resolve what `selection` is about. All selected objects must belong to one
/// subdivision (its source block or its outputs); otherwise [`Focus::None`].
/// Only lots and buildings → [`Focus::Lots`]; only roads of one street →
/// [`Focus::Street`]; anything else of that subdivision (the boundary, blocks,
/// setbacks, a mix, an object of no subdivision) → [`Focus::Site`].
pub fn focus_target<'a>(
    doc: &Document,
    selection: impl IntoIterator<Item = &'a ObjectId>,
) -> Focus {
    let ids: Vec<ObjectId> = selection.into_iter().copied().collect();
    let source_of = |id: ObjectId| {
        if doc.subdivision_links.contains_key(&id) {
            Some(id)
        } else {
            doc.subdiv_source_of(id)
        }
    };
    let sources: BTreeSet<ObjectId> = ids.iter().filter_map(|id| source_of(*id)).collect();
    let mut it = sources.into_iter();
    let (Some(source), None) = (it.next(), it.next()) else {
        return Focus::None;
    };
    let site = Focus::Site(source);
    let Some(link) = doc.subdiv_link(source) else {
        return site;
    };
    let objs: Vec<&SceneObject> = ids.iter().filter_map(|id| doc.get(*id)).collect();
    if objs.len() != ids.len()
        || ids
            .iter()
            .any(|id| *id == source || source_of(*id).is_none())
    {
        return site;
    }
    if objs
        .iter()
        .all(|o| o.layer == LOTS_LAYER || o.layer == BUILDINGS_LAYER)
    {
        let mut lots = Vec::new();
        for o in &objs {
            let Some(lot) = lot_under(doc, link, o.id) else {
                return site;
            };
            if !lots.contains(&lot) {
                lots.push(lot);
            }
        }
        return Focus::Lots { source, lots };
    }
    let streets: BTreeSet<u32> = objs
        .iter()
        .filter_map(|o| o.street.as_ref().map(|s| s.id))
        .collect();
    if streets.len() == 1 && objs.iter().all(|o| o.street.is_some()) {
        let street_id = *streets.first().expect("one street");
        return Focus::Street { source, street_id };
    }
    site
}

/// A focus that survives a re-derive (which mints fresh lot and road ids):
/// lots by a point inside each, a street by its id.
#[derive(Clone, Debug, PartialEq)]
pub enum FocusAnchor {
    Lots {
        source: ObjectId,
        points: Vec<DVec2>,
    },
    Street {
        source: ObjectId,
        street_id: u32,
    },
}

/// The anchor of `focus`, taken before an edit that re-derives the plan.
pub fn focus_anchor(doc: &Document, focus: &Focus) -> Option<FocusAnchor> {
    match focus {
        Focus::Lots { source, lots } => Some(FocusAnchor::Lots {
            source: *source,
            points: lots
                .iter()
                .filter_map(|l| plan_probe(doc.get(*l)?))
                .collect(),
        }),
        Focus::Street { source, street_id } => Some(FocusAnchor::Street {
            source: *source,
            street_id: *street_id,
        }),
        Focus::Site(_) | Focus::None => None,
    }
}

/// The objects to select so the focus is `anchor` again after a re-derive:
/// the lots now containing its points, or the street's roads.
pub fn refocus(doc: &Document, anchor: &FocusAnchor) -> BTreeSet<ObjectId> {
    match anchor {
        FocusAnchor::Lots { source, points } => {
            let Some(link) = doc.subdiv_link(*source) else {
                return BTreeSet::new();
            };
            let lots = link_lots(doc, link);
            points
                .iter()
                .filter_map(|p| {
                    lots.iter()
                        .find(|(_, poly)| poly.contains(*p))
                        .map(|(id, _)| *id)
                })
                .collect()
        }
        FocusAnchor::Street { source, street_id } => street_view(doc, *source, *street_id)
            .map(|v| v.roads.into_iter().collect())
            .unwrap_or_default(),
    }
}

/// One generated lot, as the Lot inspector shows it.
#[derive(Clone, Debug, PartialEq)]
pub struct LotView {
    pub id: ObjectId,
    pub source: ObjectId,
    /// m².
    pub area: f64,
    /// Primary-front frontage measured at the setback line (m).
    pub frontage: f64,
    /// Effective building floor count (pinned, else the plan's height rule);
    /// `None` when the subdivision builds no buildings (not a site plan).
    pub floors: Option<usize>,
    /// `floors` is pinned by hand (`lotset floors=` / `lotheight`).
    pub floors_pinned: bool,
    /// The lot's per-lot rules — effective typology and setbacks, the plan's
    /// values and the `lotset` fields pinned on it; `None` when not a site
    /// plan.
    pub rules: Option<LotRules>,
    /// Gross floor area of the building(s) standing on the lot.
    pub gfa: Option<f64>,
    /// The building objects (footprint + mass) standing on the lot.
    pub buildings: Vec<ObjectId>,
    /// The lot's edge tags (edge `i` runs from point `i` to `i + 1`), while
    /// valid.
    pub edges: Option<Vec<EdgeTag>>,
}

impl LotView {
    /// Some edge role of the lot is set by hand (`edgerole`).
    pub fn edge_roles_overridden(&self) -> bool {
        self.edges
            .as_ref()
            .is_some_and(|e| e.iter().any(|t| t.overridden))
    }

    /// Anything on the lot differs from the plan's rule.
    pub fn overridden(&self) -> bool {
        self.edge_roles_overridden()
            || self
                .rules
                .as_ref()
                .is_some_and(|r| !r.overridden.is_empty())
    }
}

/// The Lot inspector data for generated lot `lot`, or `None` when `lot` is not
/// a lot produced by a subdivision.
pub fn lot_view(doc: &Document, lot: ObjectId) -> Option<LotView> {
    let source = doc.subdiv_source_of(lot)?;
    let link = doc.subdiv_link(source)?;
    let lots = link_lots(doc, link);
    let k = lots.iter().position(|(id, _)| *id == lot)?;
    let obj = doc.get(lot)?;
    let poly = &lots[k].1;
    let block = doc_block(obj, poly.clone());
    let frontage = subdivision::frontage(&block, FrontageAt::Setback, &link.settings);

    let rules = (link.kind == SubdivKind::SitePlan).then(|| {
        let blocks: Vec<_> = lots
            .iter()
            .map(|(id, p)| match doc.get(*id) {
                Some(o) => doc_block(o, p.clone()),
                None => subdivision::Block::untagged(p.clone()),
            })
            .collect();
        let refs: Vec<_> = blocks.iter().collect();
        lot_rules(link, &refs, k)
    });
    let floors = rules.as_ref().map(|r| r.floors);
    let floors_pinned = rules
        .as_ref()
        .is_some_and(|r| r.is_overridden(crate::LotField::Floors));

    let buildings: Vec<ObjectId> = link
        .produced
        .iter()
        .filter_map(|id| doc.get(*id))
        .filter(|o| o.layer == BUILDINGS_LAYER && plan_probe(o).is_some_and(|p| poly.contains(p)))
        .map(|o| o.id)
        .collect();
    let gfas: Vec<f64> = buildings
        .iter()
        .filter_map(|id| parse_mass_name(doc.get(*id)?.name.as_deref()?).map(|(g, _)| g))
        .collect();

    Some(LotView {
        id: lot,
        source,
        area: poly.area(),
        frontage,
        floors,
        floors_pinned,
        rules,
        gfa: (!gfas.is_empty()).then(|| gfas.iter().sum()),
        buildings,
        edges: obj.valid_edge_tags().map(<[EdgeTag]>::to_vec),
    })
}

/// One generated street, as the Street inspector shows it.
#[derive(Clone, Debug, PartialEq)]
pub struct StreetView {
    pub source: ObjectId,
    pub id: u32,
    pub name: Option<String>,
    pub class: StreetClass,
    /// Right-of-way width (m).
    pub width: f64,
    /// Total centreline length of its road objects (m).
    pub length: f64,
    /// A road was moved or reshaped by hand since the bake.
    pub edited: bool,
    /// `streetset` overrides stored on the plan.
    pub name_overridden: bool,
    pub class_overridden: bool,
    pub width_overridden: bool,
    /// Hand-edited centreline (`streetmove` / `streetnode`, #157).
    pub geometry_overridden: bool,
    /// Its road objects.
    pub roads: Vec<ObjectId>,
    /// Lots with an edge tagged as fronting this street.
    pub lots: Vec<ObjectId>,
}

/// The Street inspector data for street `street_id` of subdivision `source`.
pub fn street_view(doc: &Document, source: ObjectId, street_id: u32) -> Option<StreetView> {
    let link = doc.subdiv_link(source)?;
    let roads: Vec<&SceneObject> = link
        .produced
        .iter()
        .filter_map(|id| doc.get(*id))
        .filter(|o| o.street.as_ref().is_some_and(|s| s.id == street_id))
        .collect();
    let info = roads.first()?.street.as_ref()?;
    let length = roads
        .iter()
        .filter_map(|o| match &o.geometry {
            Geometry::Curve(c) => Some(c),
            _ => None,
        })
        .map(|c| {
            let mut pts = c.tessellate(0.01);
            if c.is_closed() && pts.len() > 2 {
                pts.push(pts[0]);
            }
            pts.windows(2).map(|w| w[0].distance(w[1])).sum::<f64>()
        })
        .sum();
    let lots = link_lots(doc, link)
        .into_iter()
        .filter(|(id, _)| {
            doc.get(*id)
                .and_then(SceneObject::valid_edge_tags)
                .is_some_and(|t| t.iter().any(|t| t.street_id == Some(street_id)))
        })
        .map(|(id, _)| id)
        .collect();
    let edit = link.streets.get(&street_id);
    Some(StreetView {
        source,
        id: street_id,
        name: info.name.clone(),
        class: info.class,
        width: info.width,
        length,
        edited: roads.iter().any(|o| o.street_edited()),
        name_overridden: edit.is_some_and(|e| e.name.is_some()),
        class_overridden: edit.is_some_and(|e| e.class.is_some()),
        width_overridden: edit.is_some_and(|e| e.width.is_some()),
        geometry_overridden: edit.is_some_and(|e| e.centerline.is_some()),
        roads: roads.iter().map(|o| o.id).collect(),
        lots,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Command, Session, parse};

    fn site_plan() -> Session {
        let mut s = Session::default();
        for line in ["rect 0,0,0 600 450", "siteplan last"] {
            s.run(parse(line).unwrap()).unwrap();
        }
        s
    }

    fn on(doc: &Document, layer: &str) -> ObjectId {
        doc.objects().find(|o| o.layer == layer).unwrap().id
    }

    #[test]
    fn a_lot_focuses_the_lot() {
        let s = site_plan();
        let lot = on(&s.doc, LOTS_LAYER);
        let source = s.doc.subdiv_source_of(lot).unwrap();
        assert_eq!(
            focus_target(&s.doc, &[lot]),
            Focus::Lots {
                source,
                lots: vec![lot]
            }
        );
        assert_eq!(focus_target(&s.doc, &[]), Focus::None);
    }

    #[test]
    fn a_building_focuses_the_lot_it_stands_on() {
        let s = site_plan();
        let doc = &s.doc;
        let mass = doc
            .objects()
            .find(|o| o.layer == BUILDINGS_LAYER && matches!(o.geometry, Geometry::Mesh(_)))
            .unwrap();
        let Focus::Lots { lots, .. } = focus_target(doc, &[mass.id]) else {
            panic!("a building focuses a lot");
        };
        let lot = lots[0];
        let view = lot_view(doc, lot).unwrap();
        assert!(view.buildings.contains(&mass.id));
        // Building + its lot are one lot, not two.
        assert_eq!(
            focus_target(doc, &[mass.id, lot]),
            Focus::Lots {
                source: view.source,
                lots: vec![lot]
            }
        );
    }

    #[test]
    fn a_road_focuses_its_street() {
        let s = site_plan();
        let road = s.doc.get(on(&s.doc, crate::lot::ROADS_LAYER)).unwrap();
        let street_id = road.street.as_ref().unwrap().id;
        let source = s.doc.subdiv_source_of(road.id).unwrap();
        assert_eq!(
            focus_target(&s.doc, &[road.id]),
            Focus::Street { source, street_id }
        );
        let view = street_view(&s.doc, source, street_id).unwrap();
        assert!(view.roads.contains(&road.id));
        assert!(view.length > 0.0 && view.width > 0.0);
        assert!(!view.lots.is_empty(), "a street has fronting lots");
    }

    #[test]
    fn boundary_or_mixed_selection_focuses_the_site() {
        let s = site_plan();
        let lot = on(&s.doc, LOTS_LAYER);
        let road = on(&s.doc, crate::lot::ROADS_LAYER);
        let source = s.doc.subdiv_source_of(lot).unwrap();
        assert_eq!(focus_target(&s.doc, &[source]), Focus::Site(source));
        assert_eq!(focus_target(&s.doc, &[lot, road]), Focus::Site(source));
        assert_eq!(focus_target(&s.doc, &[source, lot]), Focus::Site(source));
    }

    #[test]
    fn lot_view_reports_pinned_floors() {
        let mut s = site_plan();
        let lot = on(&s.doc, LOTS_LAYER);
        let before = lot_view(&s.doc, lot).unwrap();
        assert!(before.area > 0.0 && !before.floors_pinned && before.floors.is_some());
        s.run(Command::LotHeight {
            targets: crate::Selector::Ids { ids: vec![lot] },
            floors: Some(12),
            ids: None,
        })
        .unwrap();
        let pinned: Vec<LotView> = link_lots(&s.doc, s.doc.subdiv_link(before.source).unwrap())
            .into_iter()
            .filter_map(|(id, _)| lot_view(&s.doc, id))
            .filter(|v| v.floors_pinned)
            .collect();
        assert_eq!(pinned.len(), 1);
        assert_eq!(pinned[0].floors, Some(12));
        assert!(pinned[0].gfa.unwrap() > 0.0);
    }

    #[test]
    fn lot_view_reports_typology_and_setback_overrides() {
        use crate::LotField as F;
        let mut s = site_plan();
        let lot = on(&s.doc, LOTS_LAYER);
        let before = lot_view(&s.doc, lot).unwrap();
        let rules = before.rules.as_ref().unwrap();
        assert!(rules.overridden.is_empty() && !before.overridden());
        assert_eq!(rules.effective, rules.rule);
        let anchor = focus_anchor(&s.doc, &focus_target(&s.doc, &[lot])).unwrap();
        s.run(
            parse(&format!(
                "lotset id:{lot} typology=courtyard setback_side=4.5"
            ))
            .unwrap(),
        )
        .unwrap();
        let lot = *refocus(&s.doc, &anchor).first().unwrap();
        let v = lot_view(&s.doc, lot).unwrap();
        let r = v.rules.as_ref().unwrap();
        assert_eq!(r.overridden, vec![F::Typology, F::SetbackSide]);
        assert!(v.overridden() && !v.floors_pinned);
        assert_eq!(r.effective.typology, subdivision::Typology::Courtyard);
        assert_eq!(r.effective.setback_side, 4.5);
        assert_eq!(r.effective.setback_front, r.rule.setback_front);
    }

    #[test]
    fn lotreset_clears_every_override_and_roles_in_one_undo_step() {
        let mut s = site_plan();
        let lots: Vec<ObjectId> = s
            .doc
            .objects()
            .filter(|o| o.layer == LOTS_LAYER)
            .take(2)
            .map(|o| o.id)
            .collect();
        let cursor = s.undo_cursor();
        s.run(Command::LotHeight {
            targets: crate::Selector::Ids { ids: lots },
            floors: Some(9),
            ids: None,
        })
        .unwrap();
        assert_eq!(s.undo_cursor(), cursor + 1, "multi-lot pin is one step");
        let source = s
            .doc
            .subdiv_source_of(s.doc.objects().find(|o| o.layer == LOTS_LAYER).unwrap().id)
            .unwrap();
        assert_eq!(s.doc.subdiv_link(source).unwrap().lot_overrides.len(), 2);
        let first = s.doc.objects().find(|o| o.layer == LOTS_LAYER).unwrap().id;
        s.run(
            parse(&format!(
                "lotset id:{first} typology=row setback_front=7 setback_rear=2"
            ))
            .unwrap(),
        )
        .unwrap();
        assert!(
            s.doc
                .subdiv_link(source)
                .unwrap()
                .lot_overrides
                .iter()
                .any(|o| o.typology.is_some()
                    && o.setback_front == Some(7.0)
                    && o.setback_rear == Some(2.0)),
            "lotset stored typology and setbacks"
        );
        let lot = s
            .doc
            .objects()
            .find(|o| o.layer == LOTS_LAYER && o.valid_edge_tags().is_some())
            .unwrap()
            .id;
        s.run(parse(&format!("edgerole id:{lot} edge=1 role=rear")).unwrap())
            .unwrap();
        assert_eq!(s.doc.subdiv_link(source).unwrap().edge_roles.len(), 1);
        let all: Vec<ObjectId> = s
            .doc
            .objects()
            .filter(|o| o.layer == LOTS_LAYER)
            .map(|o| o.id)
            .collect();
        let cursor = s.undo_cursor();
        s.run(Command::LotReset {
            targets: crate::Selector::Ids { ids: all },
            ids: None,
        })
        .unwrap();
        assert_eq!(s.undo_cursor(), cursor + 1);
        let link = s.doc.subdiv_link(source).unwrap();
        assert!(link.lot_overrides.is_empty() && link.edge_roles.is_empty());
        assert!(
            link_lots(&s.doc, link)
                .iter()
                .filter_map(|(id, _)| lot_view(&s.doc, *id))
                .all(|v| !v.overridden()),
            "no lot keeps an override"
        );
        assert!(parse("lotreset selected").is_ok() && parse("lotreset").is_ok());
        assert!(parse("lotreset selected x=1").is_err());
    }

    #[test]
    fn refocus_follows_a_rederive() {
        let mut s = site_plan();
        let lot = on(&s.doc, LOTS_LAYER);
        let focus = focus_target(&s.doc, &[lot]);
        let anchor = focus_anchor(&s.doc, &focus).unwrap();
        s.run(parse(&format!("lotheight id:{lot} floors=5")).unwrap())
            .unwrap();
        assert!(s.doc.get(lot).is_none(), "the edit re-derived the lots");
        let sel = refocus(&s.doc, &anchor);
        assert_eq!(sel.len(), 1);
        assert!(matches!(focus_target(&s.doc, &sel), Focus::Lots { lots, .. } if lots.len() == 1));

        let road = on(&s.doc, crate::lot::ROADS_LAYER);
        let focus = focus_target(&s.doc, &[road]);
        let anchor = focus_anchor(&s.doc, &focus).unwrap();
        s.run(parse(&format!("streetset id:{road} width=18")).unwrap())
            .unwrap();
        assert_eq!(focus_target(&s.doc, &refocus(&s.doc, &anchor)), focus);
    }
}
