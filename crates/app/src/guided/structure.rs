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
//!                 (roller branch adds an axis vector          → `support <pt> roller <dx,dy,dz>`)
//!   story       — type a name, an elevation, a height       → `story <name> <elev> height <h>`
//!   room        — pick a closed boundary, choose occupancy  → `room #<id> <occupancy>`
//!   minsurf     — pick a closed boundary curve              → `minsurf #<id>`
//!   funicular   — pick two supports                         → `funicular <a> <b>`
//!   cablenet    — pick four corners                         → `cablenet <c0> <c1> <c2> <c3>`
//!   tensegrity  — type a strut count                        → `tensegrity <n>`
//!   spaceframe  — type nx, ny, bay spacing, depth           → `spaceframe <nx> <ny> <bay> <depth>`
//!   geodesic    — type frequency, radius, extent            → `geodesic <f> <r> <mode>`
//!   gridshell   — branch hypar|vault, type three numbers    → `gridshell hypar <a> <b> <c>`
//!   load        — branch point|line|area, magnitude, dir    → `load point <pt> <mag> <dx,dy,dz>`

use super::assemble::{int_at, key_at, num_at, obj_at, point_at, points_at, text_at};
use super::{fmt, num, BranchArm, Input, ObjFilter, Step, VerbScript};

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
            // Branch on restraint type: pinned/fixed take no extra steps; roller
            // adds an axis Vector so the guided engine can finally emit rollers.
            Step::Branch {
                prompt: "Support type",
                arms: &[
                    BranchArm { key: "pinned", steps: &[] },
                    BranchArm { key: "fixed", steps: &[] },
                    BranchArm {
                        key: "roller",
                        steps: &[Step::Vector { prompt: "Roller axis (dx,dy,dz)" }],
                    },
                ],
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
        verb: "diagrid",
        needs_selection: false,
        steps: &[
            Step::Integer { prompt: "Cells in X", default: Some(6) },
            Step::Integer { prompt: "Cells in Y", default: Some(10) },
            Step::Number { prompt: "Width", default: Some(20.0) },
            Step::Number { prompt: "Height", default: Some(40.0) },
        ],
        assemble: assemble_diagrid,
    },
    VerbScript {
        verb: "reciprocal",
        needs_selection: false,
        steps: &[
            Step::Integer { prompt: "Member count", default: Some(8) },
            Step::Number { prompt: "Radius", default: Some(3.0) },
            Step::Number { prompt: "Member length", default: Some(4.0) },
        ],
        assemble: assemble_reciprocal,
    },
    VerbScript {
        verb: "waffle",
        needs_selection: false,
        steps: &[
            Step::Integer { prompt: "Ribs in X", default: Some(5) },
            Step::Integer { prompt: "Ribs in Y", default: Some(8) },
            Step::Number { prompt: "Width", default: Some(10.0) },
            Step::Number { prompt: "Length", default: Some(16.0) },
            Step::Number { prompt: "Depth", default: Some(1.0) },
        ],
        assemble: assemble_waffle,
    },
    VerbScript {
        verb: "voronoishell",
        needs_selection: false,
        steps: &[
            Step::Integer { prompt: "Cell count", default: Some(24) },
            Step::Number { prompt: "Width", default: Some(20.0) },
            Step::Number { prompt: "Length", default: Some(20.0) },
            Step::Integer { prompt: "Seed", default: Some(1) },
        ],
        assemble: assemble_voronoishell,
    },
    VerbScript {
        verb: "schwedler",
        needs_selection: false,
        steps: &[
            Step::Integer { prompt: "Meridians", default: Some(12) },
            Step::Integer { prompt: "Rings", default: Some(6) },
            Step::Number { prompt: "Radius", default: Some(8.0) },
            Step::Keyword { prompt: "Extent", options: &["dome", "full"], default: "dome" },
        ],
        assemble: assemble_schwedler,
    },
    VerbScript {
        verb: "catenaryvault",
        needs_selection: false,
        steps: &[
            Step::Number { prompt: "Span", default: Some(8.0) },
            Step::Number { prompt: "Length", default: Some(12.0) },
            Step::Number { prompt: "Rise", default: Some(4.0) },
        ],
        assemble: assemble_catenaryvault,
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
    VerbScript {
        verb: "gridshell",
        needs_selection: false,
        // Branch on shell type; each arm collects its three defining numbers.
        // Optional divisions (nu/nv, undulate) are skipped — the parser defaults
        // them, and they stay reachable via the typed command.
        steps: &[Step::Branch {
            prompt: "Shell type",
            arms: &[
                BranchArm {
                    key: "hypar",
                    steps: &[
                        Step::Number { prompt: "a (x half-extent)", default: Some(4.0) },
                        Step::Number { prompt: "b (y half-extent)", default: Some(4.0) },
                        Step::Number { prompt: "c (saddle height)", default: Some(2.0) },
                    ],
                },
                BranchArm {
                    key: "vault",
                    steps: &[
                        Step::Number { prompt: "span", default: Some(10.0) },
                        Step::Number { prompt: "length", default: Some(20.0) },
                        Step::Number { prompt: "rise", default: Some(3.0) },
                    ],
                },
            ],
        }],
        assemble: assemble_gridshell,
    },
    VerbScript {
        verb: "load",
        needs_selection: false,
        // Branch on load kind: point takes one application point; line/area take
        // a point list. Each then takes a magnitude and a direction Vector.
        steps: &[Step::Branch {
            prompt: "Load kind",
            arms: &[
                BranchArm {
                    key: "point",
                    steps: &[
                        Step::PickPoint { prompt: "Application point" },
                        Step::Number { prompt: "Magnitude", default: Some(1.0) },
                        Step::Vector { prompt: "Direction (dx,dy,dz)" },
                    ],
                },
                BranchArm {
                    key: "line",
                    steps: &[
                        Step::PointList { prompt: "Line points (Enter to finish)", min: 2 },
                        Step::Number { prompt: "Magnitude", default: Some(1.0) },
                        Step::Vector { prompt: "Direction (dx,dy,dz)" },
                    ],
                },
                BranchArm {
                    key: "area",
                    steps: &[
                        Step::PointList { prompt: "Area boundary points (Enter to finish)", min: 3 },
                        Step::Number { prompt: "Magnitude", default: Some(1.0) },
                        Step::Vector { prompt: "Direction (dx,dy,dz)" },
                    ],
                },
            ],
        }],
        assemble: assemble_load,
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

/// `[Point(pt), Key(kind)] -> "support <pt> <kind>"`, plus the roller arm
/// `[Point(pt), Key("roller"), Point(axis)] -> "support <pt> roller <dx,dy,dz>"`.
fn assemble_support(args: &[Input]) -> Result<String, String> {
    let pt = point_at(args, 0, "support")?;
    let kind = key_at(args, 1, "support")?;
    if kind == "roller" {
        let axis = point_at(args, 2, "support")?;
        Ok(format!("support {} roller {}", fmt(pt), fmt(axis)))
    } else {
        Ok(format!("support {} {kind}", fmt(pt)))
    }
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

/// `[Int(nx), Int(ny), Num(width), Num(height)] -> "diagrid <nx> <ny> <width> <height>"`
fn assemble_diagrid(args: &[Input]) -> Result<String, String> {
    let nx = int_at(args, 0, "diagrid")?;
    let ny = int_at(args, 1, "diagrid")?;
    let width = num_at(args, 2, "diagrid")?;
    let height = num_at(args, 3, "diagrid")?;
    Ok(format!("diagrid {nx} {ny} {} {}", num(width), num(height)))
}

/// `[Int(count), Num(radius), Num(length)] -> "reciprocal <count> <radius> <length>"`
fn assemble_reciprocal(args: &[Input]) -> Result<String, String> {
    let count = int_at(args, 0, "reciprocal")?;
    let radius = num_at(args, 1, "reciprocal")?;
    let length = num_at(args, 2, "reciprocal")?;
    Ok(format!("reciprocal {count} {} {}", num(radius), num(length)))
}

/// `[Int(nx), Int(ny), Num(width), Num(length), Num(depth)]`
/// `-> "waffle <nx> <ny> <width> <length> <depth>"`
fn assemble_waffle(args: &[Input]) -> Result<String, String> {
    let nx = int_at(args, 0, "waffle")?;
    let ny = int_at(args, 1, "waffle")?;
    let width = num_at(args, 2, "waffle")?;
    let length = num_at(args, 3, "waffle")?;
    let depth = num_at(args, 4, "waffle")?;
    Ok(format!("waffle {nx} {ny} {} {} {}", num(width), num(length), num(depth)))
}

/// `[Int(cells), Num(width), Num(length), Int(seed)]`
/// `-> "voronoishell <cells> <width> <length> <seed>"`
fn assemble_voronoishell(args: &[Input]) -> Result<String, String> {
    let cells = int_at(args, 0, "voronoishell")?;
    let width = num_at(args, 1, "voronoishell")?;
    let length = num_at(args, 2, "voronoishell")?;
    let seed = int_at(args, 3, "voronoishell")?;
    Ok(format!("voronoishell {cells} {} {} {seed}", num(width), num(length)))
}

/// `[Int(meridians), Int(rings), Num(radius), Key(mode)]`
/// `-> "schwedler <meridians> <rings> <radius> <mode>"`
fn assemble_schwedler(args: &[Input]) -> Result<String, String> {
    let meridians = int_at(args, 0, "schwedler")?;
    let rings = int_at(args, 1, "schwedler")?;
    let radius = num_at(args, 2, "schwedler")?;
    let mode = key_at(args, 3, "schwedler")?;
    Ok(format!("schwedler {meridians} {rings} {} {mode}", num(radius)))
}

/// `[Num(span), Num(length), Num(rise)] -> "catenaryvault <span> <length> <rise>"`
fn assemble_catenaryvault(args: &[Input]) -> Result<String, String> {
    let span = num_at(args, 0, "catenaryvault")?;
    let length = num_at(args, 1, "catenaryvault")?;
    let rise = num_at(args, 2, "catenaryvault")?;
    Ok(format!("catenaryvault {} {} {}", num(span), num(length), num(rise)))
}

/// `[Int(f), Num(r), Key(mode)] -> "geodesic <f> <r> <mode>"`
fn assemble_geodesic(args: &[Input]) -> Result<String, String> {
    let frequency = int_at(args, 0, "geodesic")?;
    let radius = num_at(args, 1, "geodesic")?;
    let mode = key_at(args, 2, "geodesic")?;
    Ok(format!("geodesic {frequency} {} {mode}", num(radius)))
}

/// `[Key(kind), Num(a), Num(b), Num(c)] -> "gridshell hypar <a> <b> <c>"` or
/// `"gridshell vault <span> <length> <rise>"`. The three numbers are positional
/// for both arms; the branch Key selects the surface form.
fn assemble_gridshell(args: &[Input]) -> Result<String, String> {
    let kind = key_at(args, 0, "gridshell")?;
    let a = num_at(args, 1, "gridshell")?;
    let b = num_at(args, 2, "gridshell")?;
    let c = num_at(args, 3, "gridshell")?;
    Ok(format!("gridshell {kind} {} {} {}", num(a), num(b), num(c)))
}

/// `load` assembler, branch-keyed:
/// - point: `[Key, Point(pt), Num(mag), Point(dir)] -> "load point <pt> <mag> <dir>"`
/// - line:  `[Key, Points(p…), Num(mag), Point(dir)] -> "load line <p1> <p2> <mag> <dir>"`
/// - area:  `[Key, Points(p…), Num(mag), Point(dir)] -> "load area <p1> … end <mag> <dir>"`
///
/// The `area` form emits the `end` sentinel the parser requires between the
/// boundary points and the magnitude/direction (see `parse_load`).
fn assemble_load(args: &[Input]) -> Result<String, String> {
    let kind = key_at(args, 0, "load")?;
    match kind {
        "point" => {
            let pt = point_at(args, 1, "load")?;
            let mag = num_at(args, 2, "load")?;
            let dir = point_at(args, 3, "load")?;
            Ok(format!("load point {} {} {}", fmt(pt), num(mag), fmt(dir)))
        }
        "line" => {
            let pts = points_at(args, 1, "load")?;
            let mag = num_at(args, 2, "load")?;
            let dir = point_at(args, 3, "load")?;
            let joined = pts.iter().map(|p| fmt(*p)).collect::<Vec<_>>().join(" ");
            Ok(format!("load line {joined} {} {}", num(mag), fmt(dir)))
        }
        "area" => {
            let pts = points_at(args, 1, "load")?;
            let mag = num_at(args, 2, "load")?;
            let dir = point_at(args, 3, "load")?;
            let joined = pts.iter().map(|p| fmt(*p)).collect::<Vec<_>>().join(" ");
            Ok(format!("load area {joined} end {} {}", num(mag), fmt(dir)))
        }
        other => Err(format!("load: unknown kind '{other}'")),
    }
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
    fn diagrid_walks_integers_and_numbers() {
        let mut t = GuidedTool::default();
        assert_eq!(t.try_start("diagrid", None), StartResult::Started);
        assert_eq!(t.commit_typed(""), StepResult::NeedMore);
        assert_eq!(t.commit_typed(""), StepResult::NeedMore);
        assert_eq!(t.commit_typed(""), StepResult::NeedMore);
        assert_eq!(t.commit_typed(""), StepResult::Emit("diagrid 6 10 20 40".into()));
    }

    #[test]
    fn reciprocal_walks_integer_and_numbers() {
        let mut t = GuidedTool::default();
        assert_eq!(t.try_start("reciprocal", None), StartResult::Started);
        assert_eq!(t.commit_typed(""), StepResult::NeedMore);
        assert_eq!(t.commit_typed(""), StepResult::NeedMore);
        assert_eq!(t.commit_typed(""), StepResult::Emit("reciprocal 8 3 4".into()));
    }

    #[test]
    fn waffle_walks_integers_and_numbers() {
        let mut t = GuidedTool::default();
        assert_eq!(t.try_start("waffle", None), StartResult::Started);
        assert_eq!(t.commit_typed(""), StepResult::NeedMore);
        assert_eq!(t.commit_typed(""), StepResult::NeedMore);
        assert_eq!(t.commit_typed(""), StepResult::NeedMore);
        assert_eq!(t.commit_typed(""), StepResult::NeedMore);
        assert_eq!(t.commit_typed(""), StepResult::Emit("waffle 5 8 10 16 1".into()));
    }

    #[test]
    fn assemble_diagrid_reciprocal_waffle_pure() {
        let dg = [Input::Int(6), Input::Int(10), Input::Num(20.0), Input::Num(40.0)];
        assert_eq!(assemble_diagrid(&dg).unwrap(), "diagrid 6 10 20 40");
        let rc = [Input::Int(8), Input::Num(3.0), Input::Num(4.0)];
        assert_eq!(assemble_reciprocal(&rc).unwrap(), "reciprocal 8 3 4");
        let wf = [
            Input::Int(5),
            Input::Int(8),
            Input::Num(10.0),
            Input::Num(16.0),
            Input::Num(1.0),
        ];
        assert_eq!(assemble_waffle(&wf).unwrap(), "waffle 5 8 10 16 1");
    }

    #[test]
    fn voronoishell_walks_integers_and_numbers() {
        let mut t = GuidedTool::default();
        assert_eq!(t.try_start("voronoishell", None), StartResult::Started);
        assert_eq!(t.commit_typed(""), StepResult::NeedMore);
        assert_eq!(t.commit_typed(""), StepResult::NeedMore);
        assert_eq!(t.commit_typed(""), StepResult::NeedMore);
        assert_eq!(t.commit_typed(""), StepResult::Emit("voronoishell 24 20 20 1".into()));
    }

    #[test]
    fn schwedler_walks_to_keyword() {
        let mut t = GuidedTool::default();
        assert_eq!(t.try_start("schwedler", None), StartResult::Started);
        assert_eq!(t.commit_typed("8"), StepResult::NeedMore);
        assert_eq!(t.commit_typed("4"), StepResult::NeedMore);
        assert_eq!(t.commit_typed("5"), StepResult::NeedMore);
        assert_eq!(t.commit_typed("full"), StepResult::Emit("schwedler 8 4 5 full".into()));
    }

    #[test]
    fn catenaryvault_walks_numbers() {
        let mut t = GuidedTool::default();
        assert_eq!(t.try_start("catenaryvault", None), StartResult::Started);
        assert_eq!(t.commit_typed(""), StepResult::NeedMore);
        assert_eq!(t.commit_typed(""), StepResult::NeedMore);
        assert_eq!(t.commit_typed(""), StepResult::Emit("catenaryvault 8 12 4".into()));
    }

    #[test]
    fn assemble_voronoishell_schwedler_catenaryvault_pure() {
        let vs = [Input::Int(24), Input::Num(20.0), Input::Num(20.0), Input::Int(1)];
        assert_eq!(assemble_voronoishell(&vs).unwrap(), "voronoishell 24 20 20 1");
        let sc = [Input::Int(12), Input::Int(6), Input::Num(8.0), Input::Key("dome".into())];
        assert_eq!(assemble_schwedler(&sc).unwrap(), "schwedler 12 6 8 dome");
        let cv = [Input::Num(8.0), Input::Num(12.0), Input::Num(4.0)];
        assert_eq!(assemble_catenaryvault(&cv).unwrap(), "catenaryvault 8 12 4");
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

    #[test]
    fn gridshell_walks_hypar_arm() {
        let mut t = GuidedTool::default();
        assert_eq!(t.try_start("gridshell", None), StartResult::Started);
        // The lone step is the shell-type branch.
        assert_eq!(t.prompt().unwrap(), "Shell type ( hypar / vault ) <hypar>:");
        assert_eq!(t.commit_typed("hypar"), StepResult::NeedMore);
        assert_eq!(t.commit_typed("4"), StepResult::NeedMore);
        assert_eq!(t.commit_typed("4"), StepResult::NeedMore);
        assert_eq!(t.commit_typed("2"), StepResult::Emit("gridshell hypar 4 4 2".into()));
    }

    #[test]
    fn gridshell_walks_vault_arm() {
        let mut t = GuidedTool::default();
        assert_eq!(t.try_start("gridshell", None), StartResult::Started);
        assert_eq!(t.commit_typed("vault"), StepResult::NeedMore);
        assert_eq!(t.commit_typed("10"), StepResult::NeedMore);
        assert_eq!(t.commit_typed("20"), StepResult::NeedMore);
        assert_eq!(t.commit_typed("3"), StepResult::Emit("gridshell vault 10 20 3".into()));
    }

    #[test]
    fn support_roller_branch_collects_axis_vector() {
        let mut t = GuidedTool::default();
        assert_eq!(t.try_start("support", None), StartResult::Started);
        assert_eq!(t.on_click(DVec3::ZERO), StepResult::NeedMore);
        // Branch on type; roller adds a Vector step.
        assert_eq!(t.commit_typed("roller"), StepResult::NeedMore);
        assert!(t.current_is_point(), "roller axis is a point-like Vector step");
        assert_eq!(
            t.on_click(DVec3::new(0.0, 0.0, 1.0)),
            StepResult::Emit("support 0,0 roller 0,0,1".into())
        );
    }

    #[test]
    fn support_pinned_branch_takes_no_extra_steps() {
        let mut t = GuidedTool::default();
        t.try_start("support", None);
        t.on_click(DVec3::ZERO);
        // Bare Enter selects the first arm (pinned); no further steps.
        assert_eq!(t.commit_typed(""), StepResult::Emit("support 0,0 pinned".into()));
    }

    #[test]
    fn load_point_branch_walks_to_emit() {
        let mut t = GuidedTool::default();
        assert_eq!(t.try_start("load", None), StartResult::Started);
        assert_eq!(t.prompt().unwrap(), "Load kind ( point / line / area ) <point>:");
        assert_eq!(t.commit_typed("point"), StepResult::NeedMore);
        assert_eq!(t.on_click(DVec3::ZERO), StepResult::NeedMore); // application point
        assert_eq!(t.commit_typed("5"), StepResult::NeedMore); // magnitude
        assert_eq!(
            t.on_click(DVec3::new(0.0, 0.0, -1.0)), // direction vector
            StepResult::Emit("load point 0,0 5 0,0,-1".into())
        );
    }

    #[test]
    fn load_area_branch_emits_end_sentinel() {
        let mut t = GuidedTool::default();
        t.try_start("load", None);
        assert_eq!(t.commit_typed("area"), StepResult::NeedMore);
        t.push_list_point(DVec3::ZERO);
        t.push_list_point(DVec3::new(5.0, 0.0, 0.0));
        t.push_list_point(DVec3::new(5.0, 5.0, 0.0));
        assert_eq!(t.finish_list(), StepResult::NeedMore);
        assert_eq!(t.commit_typed("3"), StepResult::NeedMore); // magnitude
        assert_eq!(
            t.on_click(DVec3::new(0.0, 0.0, -1.0)),
            StepResult::Emit("load area 0,0 5,0 5,5 end 3 0,0,-1".into())
        );
    }
}
