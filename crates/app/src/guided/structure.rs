// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! Guided verb scripts — AEC / structural group.
//!
//! These verbs build frame members, area members, form-found shells and the
//! model's story/room metadata. Every one maps onto the existing engine step
//! kinds (no engine or parser changes); each assembles the exact canonical
//! command string the parser already accepts.
//!
//!   beam        — pick start, pick end, type a section name → `beam <a> <b> <section>`
//!   column      — pick base, pick top, type a section name  → `column <a> <b> <section>`
//!   wall        — pick centerline points, type a thickness  → `wall <p…> thick <t>`
//!   slab        — pick outline points, type a thickness     → `slab <p…> thick <t>`
//!   support     — pick a location, choose a restraint       → `support <pt> <kind>`
//!   story       — type a name, an elevation, a height       → `story <name> <elev> height <h>`
//!   room        — pick a closed boundary, choose occupancy  → `room #<id> <occupancy>`
//!   minsurf     — pick a closed boundary curve              → `minsurf #<id>`
//!   funicular   — pick two supports                         → `funicular <a> <b>`
//!   cablenet    — pick four corners                         → `cablenet <c0> <c1> <c2> <c3>`
//!   tensegrity  — type a strut count                        → `tensegrity <n>`
//!   spaceframe  — type nx, ny, bay spacing, depth           → `spaceframe <nx> <ny> <bay> <depth>`
//!   geodesic    — type frequency, radius, extent            → `geodesic <f> <r> <mode>`

use super::assemble::{int_at, key_at, num_at, obj_at, point_at, points_at, text_at};
use super::{fmt, num, Input, ObjFilter, Step, VerbScript};

/// Built-in structural-section names offered by the guided beam/column section
/// step. Kept in step with the kernel catalog (`kernel_mesh::builtin_section`);
/// a test asserts every option here resolves through the catalog so they can't
/// drift. Any of these resolves at exec without a prior `section` define.
const SECTION_NAMES: &[&str] = &[
    "IPE200", "IPE300", "IPE400", "HEA200", "HEA300", "HEB200", "W12x26", "W16x40", "W21x50",
    "T150x150", "T200x200", "WT8x25", "UPN200", "UPN300", "C10x15", "L100x100", "L75x75", "L4x4",
    "SHS150", "RHS200x100", "HSS6x6", "CHS168", "CHS219", "PIPE6", "RC300x600", "RC400x400",
    "CIRC400", "GLT200x400", "GLT160x320", "GUADUA100",
];

pub static SCRIPTS: &[VerbScript] = &[
    VerbScript {
        verb: "beam",
        needs_selection: false,
        steps: &[
            Step::PickPoint { prompt: "Start" },
            Step::PickPoint { prompt: "End" },
            Step::Keyword { prompt: "Section", options: SECTION_NAMES, default: "IPE300" },
        ],
        assemble: assemble_beam,
    },
    VerbScript {
        verb: "column",
        needs_selection: false,
        steps: &[
            Step::PickPoint { prompt: "Base" },
            Step::PickPoint { prompt: "Top" },
            Step::Keyword { prompt: "Section", options: SECTION_NAMES, default: "IPE300" },
        ],
        assemble: assemble_column,
    },
    VerbScript {
        verb: "wall",
        needs_selection: false,
        steps: &[
            Step::PointList { prompt: "Wall centerline points (Enter to finish)", min: 3 },
            Step::Number { prompt: "Thickness", default: Some(0.2) },
        ],
        assemble: assemble_wall,
    },
    VerbScript {
        verb: "slab",
        needs_selection: false,
        steps: &[
            Step::PointList { prompt: "Slab outline points (Enter to finish)", min: 3 },
            Step::Number { prompt: "Thickness", default: Some(0.2) },
        ],
        assemble: assemble_slab,
    },
    VerbScript {
        verb: "support",
        needs_selection: false,
        steps: &[
            Step::PickPoint { prompt: "Support location" },
            Step::Keyword {
                // `roller` needs an axis vector the guided engine can't collect
                // yet (no vector step) — it would error at exec — so offer only
                // pinned/fixed here; roller stays available via the typed command.
                prompt: "Support type",
                options: &["pinned", "fixed"],
                default: "pinned",
            },
        ],
        assemble: assemble_support,
    },
    VerbScript {
        verb: "story",
        needs_selection: false,
        steps: &[
            Step::Text { prompt: "Story name" },
            Step::Number { prompt: "Elevation", default: None },
            Step::Number { prompt: "Story height", default: Some(3.0) },
        ],
        assemble: assemble_story,
    },
    VerbScript {
        verb: "room",
        needs_selection: false,
        steps: &[
            Step::SelectObject {
                prompt: "Select the room's closed boundary curve",
                filter: ObjFilter::Curve,
            },
            Step::Keyword {
                prompt: "Occupancy",
                options: &[
                    "office",
                    "residential",
                    "assembly",
                    "mercantile",
                    "storage",
                    "educational",
                    "industrial",
                ],
                default: "office",
            },
        ],
        assemble: assemble_room,
    },
    VerbScript {
        verb: "minsurf",
        needs_selection: false,
        steps: &[Step::SelectObject {
            prompt: "Select the closed boundary curve",
            filter: ObjFilter::Curve,
        }],
        assemble: assemble_minsurf,
    },
    VerbScript {
        verb: "funicular",
        needs_selection: false,
        steps: &[
            Step::PickPoint { prompt: "Support A" },
            Step::PickPoint { prompt: "Support B" },
        ],
        assemble: assemble_funicular,
    },
    VerbScript {
        verb: "cablenet",
        needs_selection: false,
        steps: &[
            Step::PickPoint { prompt: "Corner 0" },
            Step::PickPoint { prompt: "Corner 1" },
            Step::PickPoint { prompt: "Corner 2" },
            Step::PickPoint { prompt: "Corner 3" },
        ],
        assemble: assemble_cablenet,
    },
    VerbScript {
        verb: "tensegrity",
        needs_selection: false,
        steps: &[Step::Integer { prompt: "Number of struts", default: Some(3) }],
        assemble: assemble_tensegrity,
    },
    VerbScript {
        verb: "spaceframe",
        needs_selection: false,
        steps: &[
            Step::Integer { prompt: "Bays in X", default: Some(6) },
            Step::Integer { prompt: "Bays in Y", default: Some(4) },
            Step::Number { prompt: "Bay spacing", default: Some(3.0) },
            Step::Number { prompt: "Depth", default: Some(1.5) },
        ],
        assemble: assemble_spaceframe,
    },
    VerbScript {
        verb: "geodesic",
        needs_selection: false,
        steps: &[
            Step::Integer { prompt: "Frequency", default: Some(3) },
            Step::Number { prompt: "Radius", default: Some(5.0) },
            Step::Keyword { prompt: "Extent", options: &["dome", "full"], default: "dome" },
        ],
        assemble: assemble_geodesic,
    },
];

/// `[Point(a), Point(b), Key(section)] -> "beam <a> <b> <section>"`
fn assemble_beam(args: &[Input]) -> Result<String, String> {
    let a = point_at(args, 0, "beam")?;
    let b = point_at(args, 1, "beam")?;
    let section = key_at(args, 2, "beam")?;
    Ok(format!("beam {} {} {section}", fmt(a), fmt(b)))
}

/// `[Point(a), Point(b), Key(section)] -> "column <a> <b> <section>"`
fn assemble_column(args: &[Input]) -> Result<String, String> {
    let a = point_at(args, 0, "column")?;
    let b = point_at(args, 1, "column")?;
    let section = key_at(args, 2, "column")?;
    Ok(format!("column {} {} {section}", fmt(a), fmt(b)))
}

/// `[Points(p…), Num(t)] -> "wall <p1> <p2> … thick <t>"`
fn assemble_wall(args: &[Input]) -> Result<String, String> {
    let pts = points_at(args, 0, "wall")?;
    let thickness = num_at(args, 1, "wall")?;
    let joined = pts.iter().map(|p| fmt(*p)).collect::<Vec<_>>().join(" ");
    Ok(format!("wall {joined} thick {}", num(thickness)))
}

/// `[Points(p…), Num(t)] -> "slab <p1> <p2> … thick <t>"`
fn assemble_slab(args: &[Input]) -> Result<String, String> {
    let pts = points_at(args, 0, "slab")?;
    let thickness = num_at(args, 1, "slab")?;
    let joined = pts.iter().map(|p| fmt(*p)).collect::<Vec<_>>().join(" ");
    Ok(format!("slab {joined} thick {}", num(thickness)))
}

/// `[Point(pt), Key(kind)] -> "support <pt> <kind>"`
fn assemble_support(args: &[Input]) -> Result<String, String> {
    let pt = point_at(args, 0, "support")?;
    let kind = key_at(args, 1, "support")?;
    Ok(format!("support {} {kind}", fmt(pt)))
}

/// `[Text(name), Num(elev), Num(height)] -> "story <name> <elev> height <h>"`
fn assemble_story(args: &[Input]) -> Result<String, String> {
    let name = text_at(args, 0, "story")?;
    let elev = num_at(args, 1, "story")?;
    let height = num_at(args, 2, "story")?;
    Ok(format!("story {name} {} height {}", num(elev), num(height)))
}

/// `[Objects(#curve), Key(occ)] -> "room #curve <occupancy>"`
fn assemble_room(args: &[Input]) -> Result<String, String> {
    let boundary = obj_at(args, 0, "room")?;
    let occupancy = key_at(args, 1, "room")?;
    Ok(format!("room {boundary} {occupancy}"))
}

/// `[Objects(#curve)] -> "minsurf #curve"`
fn assemble_minsurf(args: &[Input]) -> Result<String, String> {
    let boundary = obj_at(args, 0, "minsurf")?;
    Ok(format!("minsurf {boundary}"))
}

/// `[Point(a), Point(b)] -> "funicular <a> <b>"`
fn assemble_funicular(args: &[Input]) -> Result<String, String> {
    let a = point_at(args, 0, "funicular")?;
    let b = point_at(args, 1, "funicular")?;
    Ok(format!("funicular {} {}", fmt(a), fmt(b)))
}

/// `[Point(c0), Point(c1), Point(c2), Point(c3)] -> "cablenet <c0> <c1> <c2> <c3>"`
fn assemble_cablenet(args: &[Input]) -> Result<String, String> {
    let c0 = point_at(args, 0, "cablenet")?;
    let c1 = point_at(args, 1, "cablenet")?;
    let c2 = point_at(args, 2, "cablenet")?;
    let c3 = point_at(args, 3, "cablenet")?;
    Ok(format!("cablenet {} {} {} {}", fmt(c0), fmt(c1), fmt(c2), fmt(c3)))
}

/// `[Int(n)] -> "tensegrity <n>"`
fn assemble_tensegrity(args: &[Input]) -> Result<String, String> {
    let struts = int_at(args, 0, "tensegrity")?;
    Ok(format!("tensegrity {struts}"))
}

/// `[Int(nx), Int(ny), Num(bay), Num(depth)] -> "spaceframe <nx> <ny> <bay> <depth>"`
fn assemble_spaceframe(args: &[Input]) -> Result<String, String> {
    let nx = int_at(args, 0, "spaceframe")?;
    let ny = int_at(args, 1, "spaceframe")?;
    let bay = num_at(args, 2, "spaceframe")?;
    let depth = num_at(args, 3, "spaceframe")?;
    Ok(format!("spaceframe {nx} {ny} {} {}", num(bay), num(depth)))
}

/// `[Int(f), Num(r), Key(mode)] -> "geodesic <f> <r> <mode>"`
fn assemble_geodesic(args: &[Input]) -> Result<String, String> {
    let frequency = int_at(args, 0, "geodesic")?;
    let radius = num_at(args, 1, "geodesic")?;
    let mode = key_at(args, 2, "geodesic")?;
    Ok(format!("geodesic {frequency} {} {mode}", num(radius)))
}

#[cfg(test)]
mod tests {
    use super::super::{GuidedTool, StartResult, StepResult};
    use super::*;
    use glam::DVec3;

    #[test]
    fn assemble_structure_pure() {
        // beam / column
        let frame = [
            Input::Point(DVec3::ZERO),
            Input::Point(DVec3::new(0.0, 0.0, 3.0)),
            Input::Key("IPE300".into()),
        ];
        assert_eq!(assemble_beam(&frame).unwrap(), "beam 0,0 0,0,3 IPE300");
        assert_eq!(assemble_column(&frame).unwrap(), "column 0,0 0,0,3 IPE300");
        assert!(assemble_beam(&frame[..2]).is_err());

        // wall / slab
        let area = [
            Input::Points(vec![
                DVec3::ZERO,
                DVec3::new(5.0, 0.0, 0.0),
                DVec3::new(5.0, 5.0, 0.0),
            ]),
            Input::Num(0.2),
        ];
        assert_eq!(assemble_wall(&area).unwrap(), "wall 0,0 5,0 5,5 thick 0.2");
        assert_eq!(assemble_slab(&area).unwrap(), "slab 0,0 5,0 5,5 thick 0.2");

        // support
        let sup = [Input::Point(DVec3::ZERO), Input::Key("pinned".into())];
        assert_eq!(assemble_support(&sup).unwrap(), "support 0,0 pinned");

        // story
        let story = [Input::Text("L1".into()), Input::Num(0.0), Input::Num(3.0)];
        assert_eq!(assemble_story(&story).unwrap(), "story L1 0 height 3");

        // room / minsurf
        let room = [Input::Objects("#aaaa1111".into()), Input::Key("office".into())];
        assert_eq!(assemble_room(&room).unwrap(), "room #aaaa1111 office");
        assert_eq!(
            assemble_minsurf(&[Input::Objects("#aaaa1111".into())]).unwrap(),
            "minsurf #aaaa1111"
        );

        // funicular
        let fun = [Input::Point(DVec3::ZERO), Input::Point(DVec3::new(5.0, 0.0, 0.0))];
        assert_eq!(assemble_funicular(&fun).unwrap(), "funicular 0,0 5,0");

        // cablenet
        let net = [
            Input::Point(DVec3::ZERO),
            Input::Point(DVec3::new(5.0, 0.0, 0.0)),
            Input::Point(DVec3::new(5.0, 5.0, 0.0)),
            Input::Point(DVec3::new(0.0, 5.0, 0.0)),
        ];
        assert_eq!(assemble_cablenet(&net).unwrap(), "cablenet 0,0 5,0 5,5 0,5");

        // tensegrity / spaceframe / geodesic
        assert_eq!(assemble_tensegrity(&[Input::Int(6)]).unwrap(), "tensegrity 6");
        let sf = [Input::Int(6), Input::Int(4), Input::Num(3.0), Input::Num(1.5)];
        assert_eq!(assemble_spaceframe(&sf).unwrap(), "spaceframe 6 4 3 1.5");
        let geo = [Input::Int(3), Input::Num(5.0), Input::Key("dome".into())];
        assert_eq!(assemble_geodesic(&geo).unwrap(), "geodesic 3 5 dome");
    }

    #[test]
    fn beam_walks_two_points_and_a_section() {
        let mut t = GuidedTool::default();
        assert_eq!(t.try_start("beam", None), StartResult::Started);
        assert!(t.current_is_point());
        assert_eq!(t.on_click(DVec3::ZERO), StepResult::NeedMore);
        assert_eq!(t.on_click(DVec3::new(0.0, 0.0, 3.0)), StepResult::NeedMore);
        // Section is a keyword chosen from the built-in catalog; a prefix
        // (case-insensitive) resolves to the canonical catalog name.
        assert_eq!(t.commit_typed("ipe3"), StepResult::Emit("beam 0,0 0,0,3 IPE300".into()));
        assert!(!t.active());
    }

    #[test]
    fn column_walks_two_points_and_a_section() {
        let mut t = GuidedTool::default();
        assert_eq!(t.try_start("column", None), StartResult::Started);
        assert_eq!(t.on_click(DVec3::ZERO), StepResult::NeedMore);
        assert_eq!(t.on_click(DVec3::new(0.0, 0.0, 3.0)), StepResult::NeedMore);
        // Bare Enter takes the IPE300 default.
        assert_eq!(t.commit_typed(""), StepResult::Emit("column 0,0 0,0,3 IPE300".into()));
    }

    #[test]
    fn guided_section_options_all_resolve_in_catalog() {
        // Every guided section option must resolve through the kernel catalog,
        // so the guided list and the built-in catalog can't drift apart.
        for name in SECTION_NAMES {
            assert!(
                kernel_mesh::builtin_section(name).is_some(),
                "guided section option {name} must resolve via builtin_section"
            );
        }
        // And the guided list should cover the whole catalog.
        assert_eq!(SECTION_NAMES.len(), kernel_mesh::builtin_section_names().len());
    }

    #[test]
    fn wall_walks_point_list_and_thickness() {
        let mut t = GuidedTool::default();
        assert_eq!(t.try_start("wall", None), StartResult::Started);
        assert!(t.current_wants_point_list());
        t.push_list_point(DVec3::ZERO);
        t.push_list_point(DVec3::new(5.0, 0.0, 0.0));
        // Below min(3): finishing is refused.
        assert!(matches!(t.finish_list(), StepResult::Error(_)));
        t.push_list_point(DVec3::new(5.0, 5.0, 0.0));
        assert_eq!(t.finish_list(), StepResult::NeedMore);
        // Thickness: bare Enter takes the default (0.2).
        assert_eq!(t.commit_typed(""), StepResult::Emit("wall 0,0 5,0 5,5 thick 0.2".into()));
    }

    #[test]
    fn support_walks_point_and_keyword() {
        let mut t = GuidedTool::default();
        assert_eq!(t.try_start("support", None), StartResult::Started);
        assert_eq!(t.on_click(DVec3::ZERO), StepResult::NeedMore);
        // Bare Enter takes the "pinned" default.
        assert_eq!(t.commit_typed(""), StepResult::Emit("support 0,0 pinned".into()));
    }

    #[test]
    fn room_walks_object_pick_and_keyword() {
        let mut t = GuidedTool::default();
        assert_eq!(t.try_start("room", None), StartResult::Started);
        assert!(t.current_wants_object());
        assert_eq!(t.current_filter(), Some(ObjFilter::Curve));
        assert_eq!(t.commit_object("aaaa1111"), StepResult::NeedMore);
        assert_eq!(t.commit_typed("res"), StepResult::Emit("room #aaaa1111 residential".into()));
    }

    #[test]
    fn spaceframe_walks_integers_and_numbers() {
        let mut t = GuidedTool::default();
        assert_eq!(t.try_start("spaceframe", None), StartResult::Started);
        // All four steps take their defaults on bare Enter.
        assert_eq!(t.commit_typed(""), StepResult::NeedMore);
        assert_eq!(t.commit_typed(""), StepResult::NeedMore);
        assert_eq!(t.commit_typed(""), StepResult::NeedMore);
        assert_eq!(t.commit_typed(""), StepResult::Emit("spaceframe 6 4 3 1.5".into()));
    }

    #[test]
    fn geodesic_walks_to_keyword() {
        let mut t = GuidedTool::default();
        assert_eq!(t.try_start("geodesic", None), StartResult::Started);
        assert_eq!(t.commit_typed("4"), StepResult::NeedMore);
        assert_eq!(t.commit_typed("8"), StepResult::NeedMore);
        assert_eq!(t.commit_typed("full"), StepResult::Emit("geodesic 4 8 full".into()));
    }

    #[test]
    fn minsurf_picks_a_single_curve() {
        let mut t = GuidedTool::default();
        assert_eq!(t.try_start("minsurf", None), StartResult::Started);
        assert!(t.current_wants_object());
        assert_eq!(t.commit_object("aaaa1111"), StepResult::Emit("minsurf #aaaa1111".into()));
    }

    #[test]
    fn tensegrity_takes_a_strut_count() {
        let mut t = GuidedTool::default();
        assert_eq!(t.try_start("tensegrity", None), StartResult::Started);
        assert_eq!(t.commit_typed("6"), StepResult::Emit("tensegrity 6".into()));
    }
}
