// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

use glam::DVec3;
use itsjustcad_doc::{
    AreaKind, FrameKind, HatchPattern, LoadGeometry, PaperSize, RestraintKind,
    Section as StructSection, Units, ViewDirection, METERS_PER_FOOT, METERS_PER_INCH,
};

use crate::error::ParseError;
use crate::registry::registry;
use crate::{BoolKind, Command, CompassDir, MirrorPlane, OptionOp, Selector};

/// Hand-rolled `verb arg arg...` parser. Chosen over a combinator library
/// because error message quality feeds the LLM retry loop.
pub fn parse(input: &str) -> Result<Command, ParseError> {
    let mut tokens = input.split_whitespace();
    let verb = tokens.next().ok_or(ParseError::Empty)?.to_lowercase();
    let args: Vec<&str> = tokens.collect();

    match verb.as_str() {
        "box" => {
            let [corner, size] = take::<2>("box", "a corner point and a size", &args)?;
            Ok(Command::Box {
                id: None,
                corner: point(corner)?,
                size: point(size)?,
            })
        }
        "extrude" => {
            let [sel, height] = take::<2>("extrude", "a selector and a height", &args)?;
            Ok(Command::Extrude {
                id: None,
                profile: selector_one(sel)?,
                height: number(height)?,
            })
        }
        "revolve" => {
            // Exactly one profile, so 'last' never takes a count: a bare
            // number after it is the angle, not an object count.
            let (sel, rest) = match args.split_first() {
                Some((&"last", rest)) => (Selector::Last { n: 1 }, rest),
                _ => selector(&args, "revolve")?,
            };
            let (axis_point, axis_dir, rest) = match rest {
                [p, d, rest2 @ ..] if p.contains(',') && d.contains(',') => {
                    (Some(point(p)?), Some(point(d)?), rest2)
                }
                _ => (None, None, rest),
            };
            let angle_deg = match rest {
                [] => None,
                [a] => Some(number(a)?),
                _ => {
                    return wrong(
                        "revolve",
                        "optionally an axis point, axis direction and an angle",
                        &args,
                    )
                }
            };
            Ok(Command::Revolve { id: None, profile: sel, axis_point, axis_dir, angle_deg })
        }
        "loft" => {
            let (sel, rest) = selector(&args, "loft")?;
            expect_empty("loft", rest, &args)?;
            Ok(Command::Loft { id: None, targets: sel })
        }
        "sweep" => {
            let (profile, rest) = selector(&args, "sweep")?;
            let (rail, rest) = selector(rest, "sweep")?;
            expect_empty("sweep", rest, &args)?;
            Ok(Command::Sweep { id: None, profile, rail })
        }
        "sweep2" => {
            let (profile, rest) = selector(&args, "sweep2")?;
            let (rail_a, rest) = selector(rest, "sweep2")?;
            let (rail_b, rest) = selector(rest, "sweep2")?;
            expect_empty("sweep2", rest, &args)?;
            Ok(Command::Sweep2 { id: None, profile, rail_a, rail_b })
        }
        "railrevolve" => {
            let (profile, rest) = selector(&args, "railrevolve")?;
            let (rail, rest) = selector(rest, "railrevolve")?;
            let [p, d] = match rest {
                [p, d] => [*p, *d],
                _ => {
                    return wrong(
                        "railrevolve",
                        "a profile selector, a rail selector, an axis point and an axis direction",
                        &args,
                    )
                }
            };
            Ok(Command::RailRevolve {
                id: None,
                profile,
                rail,
                axis_point: point(p)?,
                axis_dir: point(d)?,
            })
        }
        "pipe" => {
            // A bare 'last' selects one curve; the next number is the radius,
            // not an object count (like revolve).
            let (curve, rest) = match args.split_first() {
                Some((&"last", rest)) => (Selector::Last { n: 1 }, rest),
                _ => selector(&args, "pipe")?,
            };
            let (radius, end_radius) = match rest {
                [r] => (number(r)?, None),
                [r, e] => (number(r)?, Some(number(e)?)),
                _ => {
                    return wrong("pipe", "a curve selector, a radius and an optional end radius", &args)
                }
            };
            Ok(Command::Pipe { id: None, curve, radius, end_radius })
        }
        "geodesic" | "geodome" => {
            // geodesic <frequency> <radius> [dome|full]   (dome is the default)
            let (freq, radius, mode) = match args.as_slice() {
                [f, r] => (f, r, None),
                [f, r, m] => (f, r, Some(*m)),
                _ => {
                    return wrong(
                        "geodesic",
                        "a frequency, a radius and an optional dome|full",
                        &args,
                    )
                }
            };
            let full = match mode {
                None | Some("dome") => false,
                Some("full") | Some("sphere") => true,
                Some(_) => return wrong("geodesic", "dome or full as the third argument", &args),
            };
            Ok(Command::Geodesic {
                id: None,
                frequency: integer(freq, "geodesic")?,
                radius: number(radius)?,
                full,
            })
        }
        "spaceframe" => {
            let [nx, ny, bay, depth] =
                take::<4>("spaceframe", "nx, ny, a bay spacing and a depth", &args)?;
            Ok(Command::SpaceFrame {
                id: None,
                nx: integer(nx, "spaceframe")?,
                ny: integer(ny, "spaceframe")?,
                bay: number(bay)?,
                depth: number(depth)?,
            })
        }
        "hypar" => {
            // hypar <a> <b> <c> [nu] [nv]
            let (a, b, c, nu, nv) = match args.as_slice() {
                [a, b, c] => (a, b, c, None, None),
                [a, b, c, nu] => (a, b, c, Some(integer(nu, "hypar")?), None),
                [a, b, c, nu, nv] => {
                    (a, b, c, Some(integer(nu, "hypar")?), Some(integer(nv, "hypar")?))
                }
                _ => return wrong("hypar", "a, b, c and optional nu, nv", &args),
            };
            Ok(Command::Hypar {
                id: None,
                a: number(a)?,
                b: number(b)?,
                c: number(c)?,
                nu,
                nv,
            })
        }
        "gaussvault" => {
            // gaussvault <span> <length> <rise> [curve]   (curve = undulate)
            let (span, length, rise, mode) = match args.as_slice() {
                [s, l, r] => (s, l, r, None),
                [s, l, r, m] => (s, l, r, Some(*m)),
                _ => {
                    return wrong(
                        "gaussvault",
                        "a span, a length, a rise and an optional undulate|straight",
                        &args,
                    )
                }
            };
            let undulate = match mode {
                None | Some("straight") | Some("catenary") => false,
                Some("undulate") | Some("curve") | Some("wavy") => true,
                Some(_) => {
                    return wrong("gaussvault", "undulate or straight as the fourth argument", &args)
                }
            };
            Ok(Command::GaussVault {
                id: None,
                span: number(span)?,
                length: number(length)?,
                rise: number(rise)?,
                undulate,
            })
        }
        "gridshell" => parse_gridshell(&args),
        "funicular" | "hangchain" => parse_funicular(&args),
        "tensegrity" => parse_tensegrity(&args),
        "cablenet" | "minimalsurface" => parse_cablenet(&args),
        "line" => {
            let [a, b] = take::<2>("line", "two points", &args)?;
            Ok(Command::Line {
                id: None,
                a: point(a)?,
                b: point(b)?,
            })
        }
        "polyline" | "pline" => {
            let (closed, pts) = match args.split_last() {
                Some((&"closed", rest)) => (true, rest),
                _ => (false, &args[..]),
            };
            if pts.len() < 2 {
                return wrong("polyline", "at least 2 points", &args);
            }
            Ok(Command::Polyline {
                id: None,
                points: pts.iter().map(|p| point(p)).collect::<Result<_, _>>()?,
                closed,
            })
        }
        "rect" | "rectangle" => {
            let [corner, w, h] = take::<3>("rect", "a corner point, width and height", &args)?;
            Ok(Command::Rectangle {
                id: None,
                corner: point(corner)?,
                width: number(w)?,
                height: number(h)?,
            })
        }
        "circle" => {
            let [center, r] = take::<2>("circle", "a center point and a radius", &args)?;
            Ok(Command::Circle {
                id: None,
                center: point(center)?,
                radius: number(r)?,
            })
        }
        "arc" => {
            let [center, r, s, e] =
                take::<4>("arc", "center, radius, start and end angles (deg)", &args)?;
            Ok(Command::Arc {
                id: None,
                center: point(center)?,
                radius: number(r)?,
                start_deg: number(s)?,
                end_deg: number(e)?,
            })
        }
        "ellipse" => {
            let [center, rx, ry] = take::<3>("ellipse", "a center point, rx and ry", &args)?;
            Ok(Command::Ellipse {
                id: None,
                center: point(center)?,
                rx: number(rx)?,
                ry: number(ry)?,
            })
        }
        "polygon" => {
            let [center, r, sides] = take::<3>("polygon", "a center point, radius and side count", &args)?;
            Ok(Command::Polygon {
                id: None,
                center: point(center)?,
                radius: number(r)?,
                sides: sides
                    .parse()
                    .map_err(|_| ParseError::BadNumber(sides.to_string()))?,
            })
        }
        "curve" => {
            // Optional trailing: degree N
            let (degree, pts) = match args.as_slice() {
                [rest @ .., "degree", d] => (
                    d.parse::<u32>()
                        .map_err(|_| ParseError::BadNumber(d.to_string()))?,
                    rest,
                ),
                _ => (3, &args[..]),
            };
            if pts.len() < 2 {
                return wrong("curve", "at least 2 control points", &args);
            }
            Ok(Command::Curve {
                id: None,
                points: pts.iter().map(|p| point(p)).collect::<Result<_, _>>()?,
                degree,
            })
        }
        "interpcurve" | "interp" => {
            let (closed, pts) = match args.split_last() {
                Some((&"closed", rest)) => (true, rest),
                _ => (false, &args[..]),
            };
            if pts.len() < 3 {
                return wrong("interpcurve", "at least 3 points", &args);
            }
            Ok(Command::InterpCurve {
                id: None,
                points: pts.iter().map(|p| point(p)).collect::<Result<_, _>>()?,
                closed,
            })
        }
        "helix" => {
            let [center, r, h, turns] =
                take::<4>("helix", "a center point, radius, height and turns", &args)?;
            Ok(Command::Helix {
                id: None,
                center: point(center)?,
                radius: number(r)?,
                height: number(h)?,
                turns: number(turns)?,
            })
        }
        "setpoint" => {
            // Single-token selector so a trailing "last" cannot swallow the
            // index (setpoint targets exactly one curve).
            let [sel, idx, pos] =
                take::<3>("setpoint", "a curve selector, point index and new x,y,z", &args)?;
            Ok(Command::SetPoint {
                target: selector_one(sel)?,
                index: idx.parse().map_err(|_| ParseError::BadNumber(idx.to_string()))?,
                position: point(pos)?,
            })
        }
        "rebuild" => {
            let [sel, n] = take::<2>("rebuild", "a curve selector and a point count", &args)?;
            Ok(Command::Rebuild {
                id: None,
                target: selector_one(sel)?,
                count: n.parse().map_err(|_| ParseError::BadNumber(n.to_string()))?,
            })
        }
        "dim" => {
            let (offset, pts) = match args.as_slice() {
                [a, b] => (DEFAULT_DIM_OFFSET, [*a, *b]),
                [a, b, off] => (number(off)?, [*a, *b]),
                _ => return wrong("dim", "two points and an optional offset", &args),
            };
            Ok(Command::Dim {
                id: None,
                a: point(pts[0])?,
                b: point(pts[1])?,
                offset,
            })
        }
        "text" => {
            let (&pos, rest) = args
                .split_first()
                .ok_or_else(|| wrong_err("text", "a position and a string", &args))?;
            // Optional trailing height: only when it leaves at least one word.
            let (height, words) = match rest.split_last() {
                Some((last, init)) if !init.is_empty() && number(last).is_ok() => {
                    (number(last)?, init)
                }
                _ => (DEFAULT_TEXT_HEIGHT, rest),
            };
            if words.is_empty() {
                return wrong("text", "a position and a string", &args);
            }
            Ok(Command::Text {
                id: None,
                pos: point(pos)?,
                text: words.join(" "),
                height,
            })
        }
        "hatch" => {
            let (sel, rest) = selector(&args, "hatch")?;
            let pattern = match rest {
                [] | ["solid"] => HatchPattern::Solid,
                ["lines"] => HatchPattern::Lines { angle_deg: 45.0, spacing: 0.25 },
                ["lines", angle, spacing] => HatchPattern::Lines {
                    angle_deg: number(angle)?,
                    spacing: number(spacing)?,
                },
                ["crosshatch"] => HatchPattern::Crosshatch { angle_deg: 45.0, spacing: 0.25 },
                ["crosshatch", angle, spacing] => HatchPattern::Crosshatch {
                    angle_deg: number(angle)?,
                    spacing: number(spacing)?,
                },
                ["brick"] => HatchPattern::Brick { spacing: 0.25 },
                ["brick", spacing] => HatchPattern::Brick { spacing: number(spacing)? },
                ["concrete"] => HatchPattern::Concrete { spacing: 0.3 },
                ["concrete", spacing] => HatchPattern::Concrete { spacing: number(spacing)? },
                ["insulation"] => HatchPattern::Insulation { spacing: 0.3 },
                ["insulation", spacing] => HatchPattern::Insulation { spacing: number(spacing)? },
                ["earth"] => HatchPattern::Earth { spacing: 0.25 },
                ["earth", spacing] => HatchPattern::Earth { spacing: number(spacing)? },
                _ => {
                    return wrong(
                        "hatch",
                        "an optional pattern: solid, lines [a s], crosshatch [a s], brick [s], concrete [s], insulation [s], earth [s]",
                        &args,
                    )
                }
            };
            Ok(Command::Hatch { id: None, target: sel, pattern })
        }
        "union" => {
            let (sel, rest) = selector(&args, "union")?;
            expect_empty("union", rest, &args)?;
            Ok(Command::Union { id: None, targets: sel })
        }
        "difference" | "diff" | "subtract" => {
            let (target, rest) = selector(&args, "difference")?;
            let (tools, rest) = selector(rest, "difference")?;
            expect_empty("difference", rest, &args)?;
            Ok(Command::Difference { id: None, target, tools })
        }
        "intersect" | "intersection" => {
            let (sel, rest) = selector(&args, "intersect")?;
            expect_empty("intersect", rest, &args)?;
            Ok(Command::Intersect { id: None, targets: sel })
        }
        "exact_boolean" | "exactbool" => {
            // exact_boolean <union|difference|intersection> <a_corner> <a_size> <b_corner> <b_size>
            let [op, ac, asz, bc, bsz] =
                take::<5>("exact_boolean", "an op and two box corner+size pairs", &args)?;
            let op = match op.to_lowercase().as_str() {
                "union" | "u" => BoolKind::Union,
                "difference" | "diff" | "subtract" | "d" => BoolKind::Difference,
                "intersection" | "intersect" | "i" => BoolKind::Intersection,
                other => {
                    return Err(ParseError::WrongArgs {
                        command: "exact_boolean",
                        expected: "op = union|difference|intersection",
                        got: other.to_string(),
                        usage: "exact_boolean union 0,0,0 10,10,10 3,3,-5 4,4,20",
                    });
                }
            };
            Ok(Command::ExactBoolean {
                id: None,
                op,
                a_corner: point(ac)?,
                a_size: point(asz)?,
                b_corner: point(bc)?,
                b_size: point(bsz)?,
            })
        }
        "section" => {
            // Two commands share the "section" verb: the structural section
            // definition ("section <name> rect|circle|iwf|pipe ...") and the
            // mesh plane-cut ("section <selector> <point> <normal>"). Disambiguate
            // on the shape keyword in the second position.
            const SHAPES: [&str; 13] = [
                "rect", "rectangular", "circle", "circular", "iwf", "wideflange", "pipe",
                "square", "timber", "glulam", "clt", "guadua", "bamboo",
            ];
            if args.get(1).is_some_and(|t| SHAPES.contains(t)) {
                return parse_section(&args);
            }
            let (sel, rest) = selector(&args, "section")?;
            let [p, n] = take::<2>("section", "a plane point and a normal after the selector", rest)
                .map_err(|_| {
                    wrong_err("section", "a plane point and a normal after the selector", &args)
                })?;
            Ok(Command::Section {
                ids: None,
                targets: sel,
                point: point(p)?,
                normal: point(n)?,
            })
        }
        "plan" => {
            let [h] = take::<1>("plan", "a cut height", &args)?;
            Ok(Command::Plan { ids: None, height: number(h)? })
        }
        "elevation" => {
            let direction = match args.first().copied() {
                Some("north") => CompassDir::North,
                Some("south") => CompassDir::South,
                Some("east") => CompassDir::East,
                Some("west") => CompassDir::West,
                _ => {
                    return Err(wrong_err(
                        "elevation",
                        "a direction (north, south, east, or west) and an optional depth",
                        &args,
                    ))
                }
            };
            let depth = match args.get(1) {
                Some(d) => number(d)?,
                None => 0.0,
            };
            if args.len() > 2 {
                return Err(wrong_err(
                    "elevation",
                    "a direction (north, south, east, or west) and an optional depth",
                    &args,
                ));
            }
            Ok(Command::Elevation { ids: None, direction, depth })
        }
        "move" => {
            let (sel, rest) = selector(&args, "move")?;
            let [delta] = take::<1>("move", "a delta point after the selector", rest)?;
            Ok(Command::Move {
                targets: sel,
                delta: point(delta)?,
            })
        }
        "copy" => {
            let (sel, rest) = selector(&args, "copy")?;
            let [delta] = take::<1>("copy", "a delta point after the selector", rest)?;
            Ok(Command::Copy {
                ids: None,
                targets: sel,
                delta: point(delta)?,
            })
        }
        "array" => {
            let (sel, rest) = selector(&args, "array")?;
            let [counts, delta] =
                take::<2>("array", "counts nx,ny,nz and a delta dx,dy,dz", rest)
                    .map_err(|_| {
                        wrong_err("array", "counts nx,ny,nz and a delta dx,dy,dz", &args)
                    })?;
            Ok(Command::Array {
                ids: None,
                targets: sel,
                counts: counts3(counts)?,
                delta: point(delta)?,
            })
        }
        "polararray" | "parray" => with_last_backtrack(&args, "polararray", |sel, rest, args| {
            let (&count, rest) = rest
                .split_first()
                .ok_or_else(|| wrong_err("polararray", "a copy count", args))?;
            let count = count
                .parse::<u32>()
                .map_err(|_| ParseError::BadNumber(count.to_string()))?;
            let (center, rest) = match rest.first() {
                Some(t) if t.contains(',') => (Some(point(t)?), &rest[1..]),
                _ => (None, rest),
            };
            let total_angle_deg = match rest {
                [] => None,
                [a] => Some(number(a)?),
                _ => {
                    return Err(wrong_err(
                        "polararray",
                        "optionally a center point and a total angle",
                        args,
                    ))
                }
            };
            Ok(Command::PolarArray {
                ids: None,
                targets: sel,
                count,
                center,
                total_angle_deg,
            })
        }),
        "rotate" => with_last_backtrack(&args, "rotate", |sel, rest, args| {
            let (&angle, rest) = rest
                .split_first()
                .ok_or_else(|| wrong_err("rotate", "an angle in degrees", args))?;
            let angle_deg = number(angle)?;
            let (axis, rest) = match rest.split_first() {
                Some((&"x", r)) => (DVec3::X, r),
                Some((&"y", r)) => (DVec3::Y, r),
                Some((&"z", r)) => (DVec3::Z, r),
                _ => (DVec3::Z, rest),
            };
            let center = about(rest, "rotate", args)?;
            Ok(Command::Rotate { targets: sel, angle_deg, axis, center })
        }),
        "scale" => with_last_backtrack(&args, "scale", |sel, rest, args| {
            let (&factor, rest) = rest
                .split_first()
                .ok_or_else(|| wrong_err("scale", "a factor or fx,fy,fz", args))?;
            let factors = if factor.contains(',') {
                point(factor)?
            } else {
                DVec3::splat(number(factor)?)
            };
            let center = about(rest, "scale", args)?;
            Ok(Command::Scale { targets: sel, factors, center })
        }),
        "offset" => with_last_backtrack(&args, "offset", |sel, rest, args| {
            let [dist] = take::<1>("offset", "a distance after the selector", rest)
                .map_err(|_| wrong_err("offset", "a distance after the selector", args))?;
            Ok(Command::Offset {
                id: None,
                target: sel,
                distance: number(dist)?,
            })
        }),
        "split" => {
            let (sel, rest) = selector(&args, "split")?;
            let [p] = take::<1>("split", "a point after the selector", rest)
                .map_err(|_| wrong_err("split", "a point after the selector", &args))?;
            Ok(Command::Split { ids: None, target: sel, point: point(p)? })
        }
        "trim" => {
            let (target, rest) = selector(&args, "trim")?;
            let (cutter, rest) = selector(rest, "trim")?;
            let [keep] = take::<1>("trim", "a keep point after the two selectors", rest)
                .map_err(|_| wrong_err("trim", "a keep point after the two selectors", &args))?;
            Ok(Command::Trim { id: None, target, cutter, keep: point(keep)? })
        }
        "extend" => with_last_backtrack(&args, "extend", |sel, rest, args| {
            let [dist] = take::<1>("extend", "a distance after the selector", rest)
                .map_err(|_| wrong_err("extend", "a distance after the selector", args))?;
            Ok(Command::Extend { targets: sel, distance: number(dist)? })
        }),
        "join" => {
            let (sel, rest) = selector(&args, "join")?;
            expect_empty("join", rest, &args)?;
            Ok(Command::Join { id: None, targets: sel })
        }
        "fillet" => {
            let (a, rest) = selector(&args, "fillet")?;
            match rest {
                // "fillet last 2 0.5": one selector naming both curves.
                [r] => Ok(Command::Fillet { id: None, a: a.clone(), b: a, radius: number(r)? }),
                _ => {
                    let (b, rest) = selector(rest, "fillet")?;
                    let [r] = take::<1>("fillet", "a radius after the selectors", rest)
                        .map_err(|_| {
                            wrong_err("fillet", "a radius after the selectors", &args)
                        })?;
                    Ok(Command::Fillet { id: None, a, b, radius: number(r)? })
                }
            }
        }
        "mirror" => {
            let (sel, rest) = selector(&args, "mirror")?;
            let plane = match rest {
                ["xy"] => MirrorPlane::Xy,
                ["yz"] => MirrorPlane::Yz,
                ["xz"] => MirrorPlane::Xz,
                [p, n] => MirrorPlane::PointNormal {
                    point: point(p)?,
                    normal: point(n)?,
                },
                _ => {
                    return wrong(
                        "mirror",
                        "a plane: xy, yz, xz, or <point> <normal>",
                        &args,
                    )
                }
            };
            Ok(Command::Mirror { targets: sel, plane })
        }
        "delete" | "del" => {
            let (sel, rest) = selector(&args, "delete")?;
            expect_empty("delete", rest, &args)?;
            Ok(Command::Delete { targets: sel })
        }
        "group" => {
            let (sel, rest) = selector(&args, "group")?;
            let name = match rest {
                [] => None,
                [n] => Some((*n).to_string()),
                _ => return wrong("group", "a selector and an optional name", &args),
            };
            Ok(Command::Group { targets: sel, name })
        }
        "ungroup" => {
            let (sel, rest) = selector(&args, "ungroup")?;
            expect_empty("ungroup", rest, &args)?;
            Ok(Command::Ungroup { targets: sel })
        }
        "name" => {
            let (sel, rest) = selector(&args, "name")?;
            let [name] = take::<1>("name", "a name after the selector", rest)?;
            Ok(Command::Name {
                targets: sel,
                name: name.to_string(),
            })
        }
        "layer" => {
            Ok(Command::Layer { name: layer_name_all("layer", &args)? })
        }
        "tolayer" => {
            let (sel, rest) = selector(&args, "tolayer")?;
            let [layer] = take::<1>("tolayer", "a layer name after the selector", rest)?;
            Ok(Command::ToLayer {
                targets: sel,
                layer: layer.to_string(),
            })
        }
        "layercolor" => {
            let (layer, c) =
                layer_name_and_value("layercolor", "a layer name and an r,g,b color", &args)?;
            Ok(Command::LayerColor { layer, color: color3(c)? })
        }
        "layerweight" => {
            let (layer, mm) =
                layer_name_and_value("layerweight", "a layer name and a lineweight in mm", &args)?;
            let mm = number(mm)?;
            if mm <= 0.0 {
                return wrong("layerweight", "a positive lineweight in mm", &args);
            }
            Ok(Command::LayerWeight { layer, mm })
        }
        "layerrename" => {
            let [from, to] = take::<2>("layerrename", "the current layer name and the new name", &args)?;
            Ok(Command::LayerRename { from: from.to_string(), to: to.to_string() })
        }
        "layerdelete" => {
            Ok(Command::LayerDelete { layer: layer_name_all("layerdelete", &args)? })
        }
        "layerorder" => {
            let (layer, order) =
                layer_name_and_value("layerorder", "a layer name and an integer order", &args)?;
            let order = number(order)? as i32;
            Ok(Command::LayerOrder { layer, order })
        }
        "layerlock" => {
            let (layer, state) =
                layer_name_and_value("layerlock", "a layer name and 'on' or 'off'", &args)?;
            let locked = match state.to_ascii_lowercase().as_str() {
                "on" => true,
                "off" => false,
                _ => return wrong("layerlock", "'on' or 'off'", &args),
            };
            Ok(Command::LayerLock { layer, locked })
        }
        "layerlinetype" => {
            let (layer, lt) = layer_name_and_value(
                "layerlinetype",
                "a layer name and one of continuous|dashed|dotted|dashdot",
                &args,
            )?;
            let Some(linetype) = itsjustcad_doc::LineType::parse_token(lt) else {
                return wrong(
                    "layerlinetype",
                    "one of continuous|dashed|dotted|dashdot",
                    &args,
                );
            };
            Ok(Command::LayerLinetype { layer, linetype })
        }
        "hide" => {
            Ok(Command::Hide { layer: layer_name_all("hide", &args)? })
        }
        "show" => {
            Ok(Command::Show { layer: layer_name_all("show", &args)? })
        }
        "hideobj" => {
            let (sel, rest) = selector(&args, "hideobj")?;
            expect_empty("hideobj", rest, &args)?;
            Ok(Command::HideObj { targets: sel })
        }
        "showobj" => {
            let (sel, rest) = selector(&args, "showobj")?;
            expect_empty("showobj", rest, &args)?;
            Ok(Command::ShowObj { targets: sel })
        }
        "color" => {
            let (sel, rest) = selector(&args, "color")?;
            match rest {
                ["off"] => Ok(Command::ColorOff { targets: sel }),
                [c] => Ok(Command::Color { targets: sel, color: color3(c)? }),
                _ => wrong("color", "a selector then an r,g,b color or 'off'", &args),
            }
        }
        "coloroff" => {
            let (sel, rest) = selector(&args, "coloroff")?;
            expect_empty("coloroff", rest, &args)?;
            Ok(Command::ColorOff { targets: sel })
        }
        "lineweight" => {
            let (sel, rest) = selector(&args, "lineweight")?;
            match rest {
                ["off"] => Ok(Command::LinweightOff { targets: sel }),
                [mm_or_name] => {
                    let mm = iso_pen_mm(mm_or_name).ok_or_else(|| {
                        wrong_err("lineweight", "a positive mm value or ISO pen name (0.13/0.18/0.25/0.35/0.50/0.70/1.00/1.40/2.00) or 'off'", &args)
                    })?;
                    Ok(Command::Lineweight { targets: sel, mm })
                }
                _ => wrong("lineweight", "a selector then a mm value or 'off'", &args),
            }
        }
        "linweightoff" | "lineweightoff" => {
            let (sel, rest) = selector(&args, "lineweightoff")?;
            expect_empty("lineweightoff", rest, &args)?;
            Ok(Command::LinweightOff { targets: sel })
        }
        "showweights" => {
            match args.as_slice() {
                ["on"] => Ok(Command::ShowWeights { on: true }),
                ["off"] => Ok(Command::ShowWeights { on: false }),
                _ => wrong("showweights", "'on' or 'off'", &args),
            }
        }
        "material2" => {
            let (sel, rest) = selector(&args, "material2")?;
            material2_body(sel, rest, &args)
        }
        "material2off" => {
            let (sel, rest) = selector(&args, "material2off")?;
            expect_empty("material2off", rest, &args)?;
            Ok(Command::Material2Off { targets: sel })
        }
        "units" => {
            let [u] = take::<1>("units", "a unit: m, cm, mm, ft, in or ftin", &args)?;
            let units = Units::parse(u)
                .ok_or_else(|| wrong_err("units", "one of m, cm, mm, ft, in, ftin", &args))?;
            Ok(Command::Units { units })
        }
        "underlay" => match args.as_slice() {
            [path] => Ok(Command::Underlay {
                path: (*path).to_string(),
                corner: None,
                width: None,
                height: None,
            }),
            [path, corner] => Ok(Command::Underlay {
                path: (*path).to_string(),
                corner: Some(point(corner)?),
                width: None,
                height: None,
            }),
            [path, corner, width] => Ok(Command::Underlay {
                path: (*path).to_string(),
                corner: Some(point(corner)?),
                width: Some(number(width)?),
                height: None,
            }),
            _ => wrong(
                "underlay",
                "an image path and an optional corner x,y and width",
                &args,
            ),
        },
        "underlayopacity" => {
            let [o] = take::<1>("underlayopacity", "an opacity 0..1", &args)?;
            let opacity = number(o)? as f32;
            if !(0.0..=1.0).contains(&opacity) {
                return wrong("underlayopacity", "an opacity between 0 and 1", &args);
            }
            Ok(Command::UnderlayOpacity { opacity })
        }
        "underlayoff" => {
            expect_empty("underlayoff", &args, &args)?;
            Ok(Command::UnderlayOff)
        }
        "sheet" => {
            let (name, paper) = match args.as_slice() {
                [name] => (*name, PaperSize::A3),
                [name, size] => (*name, paper_size(size)?),
                _ => return wrong("sheet", "a name and an optional paper size", &args),
            };
            Ok(Command::Sheet { name: name.to_string(), paper })
        }
        "sheetview" => {
            let [sheet, dir, scale] =
                take::<3>("sheetview", "a sheet name, a direction and a scale", &args)?;
            Ok(Command::SheetView {
                sheet: sheet.to_string(),
                direction: view_direction(dir)?,
                scale: scale_denominator(scale)?,
            })
        }
        "print" => {
            let [sheet, path] = take::<2>("print", "a sheet name and an output path", &args)?;
            Ok(Command::Print {
                sheet: sheet.to_string(),
                path: path.to_string(),
            })
        }
        "export" => {
            let [path] = take::<1>("export", "an output path (.dxf/.stl/.obj/.gltf/.glb/.svg/.csv)", &args)?;
            Ok(Command::Export { path: path.to_string() })
        }
        "controlimages" => {
            let [prefix] =
                take::<1>("controlimages", "a path prefix (writes <prefix>_depth/edge/mask.png)", &args)?;
            Ok(Command::ControlImages { prefix: prefix.to_string() })
        }
        "import" => {
            let [path] = take::<1>("import", "an input path (.dxf/.obj/.stl/.gltf/.glb/.dae/.geojson/.las/.e57)", &args)?;
            Ok(Command::Import { path: path.to_string() })
        }
        "terrain" => {
            let [path] = take::<1>("terrain", "a .csv or .geojson path", &args)?;
            Ok(Command::Terrain { path: path.to_string() })
        }
        "osmfile" => {
            let [path] = take::<1>("osmfile", "an Overpass .json path", &args)?;
            Ok(Command::OsmFile { path: path.to_string() })
        }
        "view" => match args.as_slice() {
            ["save", name] => Ok(Command::ViewSave {
                name: (*name).to_string(),
                camera: None,
            }),
            ["list"] => Ok(Command::ViewList),
            [name] if *name != "save" => Ok(Command::ViewRestore { name: (*name).to_string() }),
            _ => wrong("view", "'save <name>', a saved view name, or 'list'", &args),
        },
        "option" | "opt" => match args.as_slice() {
            ["save", name] => Ok(Command::Option(OptionOp::Save { name: (*name).to_string() })),
            ["list"] => Ok(Command::Option(OptionOp::List)),
            ["delete", name] => Ok(Command::Option(OptionOp::Delete { name: (*name).to_string() })),
            [name] if !matches!(*name, "save" | "list" | "delete") => {
                Ok(Command::Option(OptionOp::Switch { name: (*name).to_string() }))
            }
            _ => wrong(
                "option",
                "'save <name>', a branch name to switch to, 'list', or 'delete <name>'",
                &args,
            ),
        },
        "select" => {
            let (sel, rest) = selector(&args, "select")?;
            expect_empty("select", rest, &args)?;
            Ok(Command::Select { targets: sel })
        }
        "selectnone" | "deselect" => Ok(Command::SelectNone),
        "distance" | "dist" => {
            let [a, b] = take::<2>("distance", "two points", &args)?;
            Ok(Command::Distance { a: point(a)?, b: point(b)? })
        }
        "area" => {
            let (sel, rest) = selector(&args, "area")?;
            expect_empty("area", rest, &args)?;
            Ok(Command::Area { targets: sel })
        }
        "volume" | "vol" => {
            let (sel, rest) = selector(&args, "volume")?;
            expect_empty("volume", rest, &args)?;
            Ok(Command::Volume { targets: sel })
        }
        "bbox" => {
            let (sel, rest) = selector(&args, "bbox")?;
            expect_empty("bbox", rest, &args)?;
            Ok(Command::Bbox { targets: sel })
        }
        "schedule" => {
            let layer = match args.as_slice() {
                [] => None,
                [name] => Some((*name).to_string()),
                _ => return wrong("schedule", "an optional layer name", &args),
            };
            Ok(Command::Schedule { layer })
        }
        "sheettable" => {
            let (sheet, layer) = match args.as_slice() {
                [sheet] => ((*sheet).to_string(), None),
                [sheet, layer] => ((*sheet).to_string(), Some((*layer).to_string())),
                _ => return wrong("sheettable", "a sheet name and an optional layer name", &args),
            };
            Ok(Command::SheetTable { sheet, layer })
        }
        "sheetdim" => {
            // sheetdim <sheet> <x1,y1> <x2,y2> [offset_mm]
            match args.as_slice() {
                [sheet, a, b] => Ok(Command::SheetDim {
                    sheet: (*sheet).to_string(),
                    a: paper_point(a)?,
                    b: paper_point(b)?,
                    offset: None,
                    view_index: None,
                }),
                [sheet, a, b, off] => Ok(Command::SheetDim {
                    sheet: (*sheet).to_string(),
                    a: paper_point(a)?,
                    b: paper_point(b)?,
                    offset: Some(number(off)?),
                    view_index: None,
                }),
                _ => wrong(
                    "sheetdim",
                    "a sheet name, two paper points (mm) and an optional offset (mm)",
                    &args,
                ),
            }
        }
        // sun <lat> <lon> <YYYY-MM-DD> <HH:MM>
        // Computes solar position via NOAA SPA and stores az+alt in the command.
        "sun" => match args.as_slice() {
            [lat, lon, date, time] => {
                let lat_deg = number(lat)?;
                let lon_deg = number(lon)?;
                let (year, month, day) = parse_date(date)?;
                let (hour, minute) = parse_hhmm(time)?;
                let pos = itsjustcad_solar::solar_position(
                    year, month, day, hour, minute, lat_deg, lon_deg,
                );
                Ok(Command::Sun {
                    azimuth_deg: pos.azimuth_deg,
                    altitude_deg: pos.altitude_deg,
                    lat_deg,
                    lon_deg,
                })
            }
            _ => wrong("sun", "lat lon YYYY-MM-DD HH:MM", &args),
        },
        "sunoff" => {
            expect_empty("sunoff", &args, &args)?;
            Ok(Command::SunOff)
        }
        // location <lat> <lon> [tz-hours]
        "location" => match args.as_slice() {
            [lat, lon] => Ok(Command::Location {
                lat_deg: number(lat)?,
                lon_deg: number(lon)?,
                tz_hours: 0.0,
            }),
            [lat, lon, tz] => Ok(Command::Location {
                lat_deg: number(lat)?,
                lon_deg: number(lon)?,
                tz_hours: number(tz)?,
            }),
            _ => wrong("location", "lat lon [tz-hours]", &args),
        },
        // shadowstudy <YYYY-MM-DD> <from-HH:MM> <to-HH:MM> <step-min>
        "shadowstudy" => match args.as_slice() {
            [date, from, to, step] => {
                let (year, month, day) = parse_date(date)?;
                let (fh, fm) = parse_hhmm(from)?;
                let (th, tm) = parse_hhmm(to)?;
                let step_min = number(step)?;
                if step_min <= 0.0 {
                    return Err(ParseError::BadNumber(step.to_string()));
                }
                let from_min = fh * 60 + fm;
                let to_min = th * 60 + tm;
                if to_min < from_min {
                    return Err(ParseError::BadNumber(to.to_string()));
                }
                Ok(Command::ShadowStudy {
                    ids: None,
                    year,
                    month,
                    day,
                    from_min,
                    to_min,
                    step_min: step_min as u32,
                })
            }
            _ => wrong("shadowstudy", "YYYY-MM-DD from-HH:MM to-HH:MM step-min", &args),
        },
        // sunhours <YYYY-MM-DD> [grid-spacing]
        "sunhours" => {
            let (date, spacing) = match args.as_slice() {
                [date] => (*date, 2.0),
                [date, sp] => {
                    let s = number(sp)?;
                    if s <= 0.0 {
                        return Err(ParseError::BadNumber(sp.to_string()));
                    }
                    (*date, s)
                }
                _ => return wrong("sunhours", "YYYY-MM-DD [grid-spacing]", &args),
            };
            let (year, month, day) = parse_date(date)?;
            Ok(Command::SunHours { ids: None, year, month, day, spacing })
        }
        // facesunhours <selector> <YYYY-MM-DD>
        "facesunhours" => {
            let (targets, rest) = selector(&args, "facesunhours")?;
            let [date] = take::<1>("facesunhours", "a YYYY-MM-DD date after the selector", rest)?;
            let (year, month, day) = parse_date(date)?;
            Ok(Command::FaceSunHours { targets, ids: None, year, month, day })
        }
        // -- blocks --
        // block <selector> <name>
        "block" => {
            let (sel, rest) = selector(&args, "block")?;
            let [name] = take::<1>("block", "a block name after the selector", rest)
                .map_err(|_| wrong_err("block", "a selector and a block name", &args))?;
            Ok(Command::BlockDefine {
                targets: sel,
                name: name.to_string(),
                geometries: None,
            })
        }
        // insert <name> <point> [rotation_deg] [scale] [key=value ...]
        "insert" => {
            let (name, pos, rest) = match args.as_slice() {
                [name, pos, rest @ ..] => (*name, *pos, rest),
                _ => return wrong(
                    "insert",
                    "a block name, an insertion point, optional rotation (deg), scale and key=value params",
                    &args,
                ),
            };
            // Split trailing tokens: `key=value` are params; bare numbers fill
            // rotation then scale positionally.
            let mut params = std::collections::BTreeMap::new();
            let mut nums: Vec<f64> = Vec::new();
            for tok in rest {
                if let Some((k, v)) = tok.split_once('=') {
                    if k.is_empty() {
                        return wrong("insert", "a non-empty param name in key=value", &args);
                    }
                    params.insert(k.to_string(), v.to_string());
                } else {
                    nums.push(number(tok)?);
                }
            }
            if nums.len() > 2 {
                return wrong(
                    "insert",
                    "at most a rotation and a scale before key=value params",
                    &args,
                );
            }
            Ok(Command::BlockInsert {
                id: None,
                name: name.to_string(),
                position: point(pos)?,
                rotation_deg: nums.first().copied(),
                scale: nums.get(1).copied(),
                params,
            })
        }
        // pblock <name> [pname=default ...] : <body ; separated>
        "pblock" => parse_pblock(input),
        // param <selector> <key=value ...>
        "param" => {
            let (sel, rest) = selector(&args, "param")?;
            if rest.is_empty() {
                return wrong("param", "a selector and one or more key=value params", &args);
            }
            let mut params = std::collections::BTreeMap::new();
            for tok in rest {
                let Some((k, v)) = tok.split_once('=') else {
                    return wrong("param", "key=value params", &args);
                };
                if k.is_empty() {
                    return wrong("param", "a non-empty param name in key=value", &args);
                }
                params.insert(k.to_string(), v.to_string());
            }
            Ok(Command::BlockParamSet { target: sel, params })
        }
        "blocks" => {
            expect_empty("blocks", &args, &args)?;
            Ok(Command::BlocksList)
        }
        // -- block content library --
        // blocklib list   (same as "blocklib" with no args)
        "blocklib" => {
            // Allow both "blocklib" and "blocklib list"
            match args.as_slice() {
                [] | ["list"] => Ok(Command::BlockLibList),
                _ => wrong("blocklib", "'list' or no arguments", &args),
            }
        }
        // blockload <name>
        "blockload" => match args.as_slice() {
            [name] => Ok(Command::BlockLibLoad {
                name: (*name).to_string(),
                geometries: None,
            }),
            _ => wrong("blockload", "a library block name", &args),
        },
        // blocksave <name> [description...]
        "blocksave" => match args.as_slice() {
            [name] => Ok(Command::BlockLibSave {
                name: (*name).to_string(),
                description: String::new(),
            }),
            [name, desc @ ..] => Ok(Command::BlockLibSave {
                name: (*name).to_string(),
                description: desc.join(" "),
            }),
            _ => wrong("blocksave", "a block name", &args),
        },
        "material" => {
            let [name, e, d] =
                take::<3>("material", "a name, elastic modulus E and density", &args)?;
            Ok(Command::DefMaterial {
                name: name.to_string(),
                elastic_modulus_e: number(e)?,
                density: number(d)?,
            })
        }
        "grid" => parse_grid(&args),
        "story" | "level" => {
            let [name, elev] = take::<2>("story", "a name and an elevation", &args)?;
            Ok(Command::DefStory { name: name.to_string(), elevation: number(elev)? })
        }
        "beam" => parse_frame(FrameKind::Beam, &args),
        "column" => parse_frame(FrameKind::Column, &args),
        "slab" => parse_area(AreaKind::Slab, &args),
        "wall" => parse_area(AreaKind::Wall, &args),
        "load" => parse_load(&args),
        "support" => parse_support(&args),
        "undo" => Ok(Command::Undo),
        "redo" => Ok(Command::Redo),
        "amend" => match &args[..] {
            [step, rest @ ..] if !rest.is_empty() => {
                let step = step
                    .parse::<usize>()
                    .map_err(|_| wrong_err("amend", "a step number then a command", &args))?;
                Ok(Command::Amend {
                    step,
                    with: Box::new(parse(&rest.join(" "))?),
                })
            }
            _ => wrong("amend", "a step number then a command", &args),
        },
        other => Err(ParseError::UnknownCommand {
            name: other.to_string(),
            suggestion: closest_command(other),
        }),
    }
}

/// Default dimension-line offset (meters) when the user omits it.
const DEFAULT_DIM_OFFSET: f64 = 0.5;
/// Default annotation text height (meters).
const DEFAULT_TEXT_HEIGHT: f64 = 0.2;

/// `section <name> rect|circle|iwf|pipe <dims...>`
fn parse_section(args: &[&str]) -> Result<Command, ParseError> {
    let (name, rest) = args
        .split_first()
        .ok_or_else(|| wrong_err("section", "a name then a profile shape", args))?;
    let section = match rest {
        ["rect", w, h] | ["rectangular", w, h] => {
            StructSection::Rectangular { w: number(w)?, h: number(h)? }
        }
        ["circle", d] | ["circular", d] => StructSection::Circular { d: number(d)? },
        ["iwf", d, bf, tf, tw] | ["wideflange", d, bf, tf, tw] => StructSection::IWideFlange {
            d: number(d)?,
            bf: number(bf)?,
            tf: number(tf)?,
            tw: number(tw)?,
        },
        ["pipe", d, t] => StructSection::Pipe { d: number(d)?, t: number(t)? },
        ["timber", w, h] | ["glulam", w, h] | ["clt", w, h] => {
            StructSection::Timber { w: number(w)?, h: number(h)? }
        }
        ["guadua", d, t] | ["bamboo", d, t] => {
            StructSection::Guadua { d: number(d)?, t: number(t)? }
        }
        _ => {
            return wrong(
                "section",
                "a name then rect <w> <h> | circle <d> | iwf <d> <bf> <tf> <tw> | pipe <d> <t> | timber <w> <h> | guadua <d> <t>",
                args,
            )
        }
    };
    Ok(Command::DefSection { name: (*name).to_string(), section })
}

/// `pblock <name> [pname=default ...] : templ line 1 ; templ line 2 ; ...`
///
/// The `:` separates the param header from the command-template body; body
/// lines are separated by `;`. Reuses the raw input so body lines keep spaces.
fn parse_pblock(input: &str) -> Result<Command, ParseError> {
    // Strip the leading verb.
    let after = input
        .trim_start()
        .split_once(char::is_whitespace)
        .map(|(_, r)| r)
        .unwrap_or("");
    let Some((header, body_str)) = after.split_once(':') else {
        return Err(wrong_err(
            "pblock",
            "a name, key=default params, ':' then a ';'-separated template body",
            &[],
        ));
    };
    let mut header_toks = header.split_whitespace();
    let name = header_toks
        .next()
        .ok_or_else(|| wrong_err("pblock", "a block name", &[]))?
        .to_string();
    let mut params = Vec::new();
    for tok in header_toks {
        let Some((k, v)) = tok.split_once('=') else {
            return Err(wrong_err("pblock", "params as name=default", &[tok]));
        };
        if k.is_empty() {
            return Err(wrong_err("pblock", "a non-empty param name", &[tok]));
        }
        params.push(itsjustcad_doc::ParamBlockParam {
            name: k.to_string(),
            default: v.to_string(),
        });
    }
    let body: Vec<String> = body_str
        .split(';')
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();
    if body.is_empty() {
        return Err(wrong_err("pblock", "at least one template body line", &[]));
    }
    Ok(Command::BlockParamDefine { name, params, body })
}

/// `grid <name> x A:0 B:5 ... y 1:0 2:4 ... [levels 0,3,6]`
fn parse_grid(args: &[&str]) -> Result<Command, ParseError> {
    let (name, mut rest) = args
        .split_first()
        .ok_or_else(|| wrong_err("grid", "a name, x axes and y axes", args))?;
    let mut x_axes = Vec::new();
    let mut y_axes = Vec::new();
    let mut levels = Vec::new();
    // Walk tokens; keywords x/y/levels switch the active bucket.
    #[derive(PartialEq)]
    enum Bucket {
        None,
        X,
        Y,
        Levels,
    }
    let mut bucket = Bucket::None;
    while let Some((&tok, tail)) = rest.split_first() {
        rest = tail;
        match tok {
            "x" => bucket = Bucket::X,
            "y" => bucket = Bucket::Y,
            "levels" | "level" => bucket = Bucket::Levels,
            _ => match bucket {
                Bucket::X | Bucket::Y => {
                    let (label, coord) = tok
                        .split_once(':')
                        .ok_or_else(|| wrong_err("grid", "axes as label:coord (e.g. A:0)", args))?;
                    let entry = (label.to_string(), number(coord)?);
                    if bucket == Bucket::X {
                        x_axes.push(entry);
                    } else {
                        y_axes.push(entry);
                    }
                }
                Bucket::Levels => {
                    for c in tok.split(',') {
                        levels.push(number(c)?);
                    }
                }
                Bucket::None => {
                    return wrong("grid", "'x' or 'y' before axis entries", args)
                }
            },
        }
    }
    if x_axes.is_empty() && y_axes.is_empty() {
        return wrong("grid", "at least one x or y axis (e.g. x A:0 B:5)", args);
    }
    Ok(Command::DefGrid { name: (*name).to_string(), x_axes, y_axes, levels })
}

/// Shared trailing `[material <m>]` and `[rot <deg>]` options for members.
fn member_options<'a>(
    mut rest: &'a [&'a str],
    command: &'static str,
    args: &[&str],
) -> Result<(Option<String>, Option<f64>), ParseError> {
    let mut material = None;
    let mut rot = None;
    while let Some((&tok, tail)) = rest.split_first() {
        match tok {
            "material" | "mat" => {
                let (&m, t2) = tail
                    .split_first()
                    .ok_or_else(|| wrong_err(command, "a material name after 'material'", args))?;
                material = Some(m.to_string());
                rest = t2;
            }
            "rot" | "orientation" => {
                let (&d, t2) = tail
                    .split_first()
                    .ok_or_else(|| wrong_err(command, "an angle after 'rot'", args))?;
                rot = Some(number(d)?);
                rest = t2;
            }
            _ => return Err(wrong_err(command, "optional 'material <m>' and 'rot <deg>'", args)),
        }
    }
    Ok((material, rot))
}

/// `beam|column <a> <b> <section> [material <m>] [rot <deg>]`
fn parse_frame(kind: FrameKind, args: &[&str]) -> Result<Command, ParseError> {
    let command = kind.label();
    let [a, b, section] = match args {
        [a, b, s, ..] => [*a, *b, *s],
        _ => {
            return wrong(command, "two endpoints and a section name", args)
        }
    };
    let (material, orientation_deg) = member_options(&args[3..], command, args)?;
    Ok(Command::FrameMember {
        id: None,
        kind,
        a: point(a)?,
        b: point(b)?,
        section: section.to_string(),
        material,
        orientation_deg,
    })
}

/// `slab|wall <p1> <p2> <p3> ... thick <t> [material <m>]`
fn parse_area(kind: AreaKind, args: &[&str]) -> Result<Command, ParseError> {
    let command = kind.label();
    // Split at the 'thick' keyword: everything before is boundary points.
    let Some(tpos) = args.iter().position(|&t| t == "thick" || t == "thickness") else {
        return wrong(command, "boundary points then 'thick <t>'", args);
    };
    let pts = &args[..tpos];
    if pts.len() < 3 {
        return wrong(command, "at least 3 boundary points before 'thick'", args);
    }
    let after = &args[tpos + 1..];
    let (&t, rest) = after
        .split_first()
        .ok_or_else(|| wrong_err(command, "a thickness after 'thick'", args))?;
    let thickness = number(t)?;
    let (material, _rot) = member_options(rest, command, args)?;
    let boundary = pts.iter().map(|p| point(p)).collect::<Result<_, _>>()?;
    Ok(Command::AreaMember { id: None, kind, boundary, thickness, material })
}

fn take<'a, const N: usize>(
    command: &'static str,
    expected: &'static str,
    args: &[&'a str],
) -> Result<[&'a str; N], ParseError> {
    <[&str; N]>::try_from(args.to_vec()).map_err(|_| wrong_err(command, expected, args))
}

/// All args joined with single spaces as one layer name. Lets layer names
/// contain spaces (Rhino allows this, e.g. the seeded "Layer 01"). Rejects an
/// empty name. Round-trips: a name is re-parsed to the same joined string.
fn layer_name_all(
    command: &'static str,
    args: &[&str],
) -> Result<String, ParseError> {
    if args.is_empty() {
        return Err(wrong_err(command, "a layer name", args));
    }
    Ok(args.join(" "))
}

/// Split a `<layer…> <value>` argument list: the last token is the value, the
/// rest joins into the (possibly multi-word) layer name. Both must be present.
fn layer_name_and_value<'a>(
    command: &'static str,
    expected: &'static str,
    args: &[&'a str],
) -> Result<(String, &'a str), ParseError> {
    match args.split_last() {
        Some((value, head)) if !head.is_empty() => Ok((head.join(" "), value)),
        _ => Err(wrong_err(command, expected, args)),
    }
}

fn wrong<T>(command: &'static str, expected: &'static str, args: &[&str]) -> Result<T, ParseError> {
    Err(wrong_err(command, expected, args))
}

fn wrong_err(command: &'static str, expected: &'static str, args: &[&str]) -> ParseError {
    let usage = registry()
        .iter()
        .find(|s| s.name == command)
        .map(|s| s.usage)
        .unwrap_or("");
    ParseError::WrongArgs {
        command,
        expected,
        got: if args.is_empty() {
            "nothing".to_string()
        } else {
            format!("'{}'", args.join(" "))
        },
        usage,
    }
}

fn expect_empty(command: &'static str, rest: &[&str], args: &[&str]) -> Result<(), ParseError> {
    if rest.is_empty() {
        Ok(())
    } else {
        wrong(command, "only a selector", args)
    }
}

/// Parse "<selector> <numeric args...>" where a bare number follows the
/// selector. "last 90" is ambiguous (last-90-objects vs last + angle 90):
/// try the greedy selector first, and when the numeric tail fails to parse,
/// retry with `last` meaning one object so the count becomes the number.
fn with_last_backtrack(
    args: &[&str],
    command: &'static str,
    build: impl Fn(Selector, &[&str], &[&str]) -> Result<Command, ParseError>,
) -> Result<Command, ParseError> {
    let (sel, rest) = selector(args, command)?;
    match build(sel, rest, args) {
        Ok(cmd) => Ok(cmd),
        Err(e) => {
            if args.first() == Some(&"last") && args.len() >= 2 {
                build(Selector::Last { n: 1 }, &args[1..], args).map_err(|_| e)
            } else {
                Err(e)
            }
        }
    }
}

/// Optional trailing `about <point>` clause for rotate/scale.
fn about(
    rest: &[&str],
    command: &'static str,
    args: &[&str],
) -> Result<Option<DVec3>, ParseError> {
    match rest {
        [] => Ok(None),
        ["about", p] => Ok(Some(point(p)?)),
        _ => Err(wrong_err(command, "optionally 'about <point>' at the end", args)),
    }
}

/// Parse a non-negative integer count (frequency, grid divisions). Reuses the
/// `BadNumber` error kind so command error messages stay uniform.
fn integer(s: &str, _command: &'static str) -> Result<u32, ParseError> {
    s.parse::<u32>().map_err(|_| ParseError::BadNumber(s.to_string()))
}

/// `gridshell hypar <a> <b> <c> [nu] [nv]` | `gridshell vault <span> <length>
/// <rise> [undulate] [nu] [nv]`.
fn parse_gridshell(args: &[&str]) -> Result<Command, ParseError> {
    use crate::GridshellSurfaceSpec;
    let expected =
        "hypar <a> <b> <c> [nu] [nv] | vault <span> <length> <rise> [undulate] [nu] [nv]";
    let (kind, rest) = args
        .split_first()
        .ok_or_else(|| wrong_err("gridshell", expected, args))?;
    match *kind {
        "hypar" => {
            let (a, b, c, nu, nv) = match rest {
                [a, b, c] => (a, b, c, None, None),
                [a, b, c, nu] => (a, b, c, Some(integer(nu, "gridshell")?), None),
                [a, b, c, nu, nv] => (
                    a,
                    b,
                    c,
                    Some(integer(nu, "gridshell")?),
                    Some(integer(nv, "gridshell")?),
                ),
                _ => return wrong("gridshell", expected, args),
            };
            Ok(Command::Gridshell {
                id: None,
                surface: GridshellSurfaceSpec::Hypar {
                    a: number(a)?,
                    b: number(b)?,
                    c: number(c)?,
                },
                nu,
                nv,
            })
        }
        "vault" => {
            // Optional 'undulate' flag may appear right after the three numbers.
            let (span, length, rise, tail) = match rest {
                [s, l, r, tail @ ..] => (s, l, r, tail),
                _ => return wrong("gridshell", expected, args),
            };
            let (undulate, tail) = match tail.split_first() {
                Some((&"undulate", t)) | Some((&"wavy", t)) => (true, t),
                Some((&"straight", t)) => (false, t),
                _ => (false, tail),
            };
            let (nu, nv) = match tail {
                [] => (None, None),
                [nu] => (Some(integer(nu, "gridshell")?), None),
                [nu, nv] => (
                    Some(integer(nu, "gridshell")?),
                    Some(integer(nv, "gridshell")?),
                ),
                _ => return wrong("gridshell", expected, args),
            };
            Ok(Command::Gridshell {
                id: None,
                surface: GridshellSurfaceSpec::Vault {
                    span: number(span)?,
                    length: number(length)?,
                    rise: number(rise)?,
                    undulate,
                },
                nu,
                nv,
            })
        }
        _ => wrong("gridshell", expected, args),
    }
}

/// `funicular <support a> <support b> [segments] [load] [slack] [invert]`.
/// Keyword `invert` (or `arch`) anywhere in the tail flips the found tension
/// form into the pure-compression arch.
fn parse_funicular(args: &[&str]) -> Result<Command, ParseError> {
    let expected = "<support a x,y,z> <support b x,y,z> [segments] [load] [slack] [invert]";
    // Pull off the optional trailing keyword.
    let mut rest: Vec<&str> = args.to_vec();
    let mut invert = false;
    rest.retain(|&a| {
        if a == "invert" || a == "arch" || a == "compress" {
            invert = true;
            false
        } else {
            true
        }
    });
    let (a, b, seg, load, slack) = match rest.as_slice() {
        [a, b] => (a, b, None, None, None),
        [a, b, s] => (a, b, Some(integer(s, "funicular")?), None, None),
        [a, b, s, l] => (a, b, Some(integer(s, "funicular")?), Some(number(l)?), None),
        [a, b, s, l, sl] => (
            a,
            b,
            Some(integer(s, "funicular")?),
            Some(number(l)?),
            Some(number(sl)?),
        ),
        _ => return wrong("funicular", expected, args),
    };
    Ok(Command::Funicular {
        id: None,
        support_a: point(a)?,
        support_b: point(b)?,
        segments: seg,
        load,
        slack,
        invert,
    })
}

/// `tensegrity <struts> [radius] [height] [twist_deg]` (preset: struts=3).
fn parse_tensegrity(args: &[&str]) -> Result<Command, ParseError> {
    let expected = "<struts> [radius] [height] [twist degrees]  (e.g. tensegrity 3)";
    // Accept a named preset that expands to a strut count.
    let (struts, radius, height, twist) = match args {
        ["tprism"] | ["t-prism"] => (3, None, None, None),
        [s] => (integer(s, "tensegrity")?, None, None, None),
        [s, r] => (integer(s, "tensegrity")?, Some(number(r)?), None, None),
        [s, r, h] => (
            integer(s, "tensegrity")?,
            Some(number(r)?),
            Some(number(h)?),
            None,
        ),
        [s, r, h, t] => (
            integer(s, "tensegrity")?,
            Some(number(r)?),
            Some(number(h)?),
            Some(number(t)?),
        ),
        _ => return wrong("tensegrity", expected, args),
    };
    Ok(Command::Tensegrity {
        id: None,
        struts,
        radius,
        height,
        twist_deg: twist,
    })
}

/// `cablenet <c0> <c1> <c2> <c3> [n] [sag]` — four corner anchors in order.
fn parse_cablenet(args: &[&str]) -> Result<Command, ParseError> {
    let expected = "<c0> <c1> <c2> <c3> [n] [sag]  (four corner points in CCW order)";
    let (c0, c1, c2, c3, n, sag) = match args {
        [a, b, c, d] => (a, b, c, d, None, None),
        [a, b, c, d, n] => (a, b, c, d, Some(integer(n, "cablenet")?), None),
        [a, b, c, d, n, s] => (a, b, c, d, Some(integer(n, "cablenet")?), Some(number(s)?)),
        _ => return wrong("cablenet", expected, args),
    };
    Ok(Command::Cablenet {
        id: None,
        corners: [point(c0)?, point(c1)?, point(c2)?, point(c3)?],
        n,
        sag,
    })
}

/// Numbers accept unit suffixes (mm, cm, m, ft, in, and feet-inches as
/// "12ft6in"); bare numbers are meters. Results are always meters.
pub fn number(s: &str) -> Result<f64, ParseError> {
    let bad = || ParseError::BadNumber(s.to_string());
    let val = |v: &str| v.parse::<f64>().map_err(|_| bad());
    if let Some(body) = s.strip_suffix("in") {
        // "6in", or feet-inches "12ft6in" (sign applies to the whole length).
        if let Some((ft, inches)) = body.split_once("ft") {
            let (sign, ft) = match ft.strip_prefix('-') {
                Some(rest) => (-1.0, rest),
                None => (1.0, ft),
            };
            return Ok(sign * (val(ft)? * 12.0 + val(inches)?) * METERS_PER_INCH);
        }
        return Ok(val(body)? * METERS_PER_INCH);
    }
    if let Some(v) = s.strip_suffix("ft") {
        return Ok(val(v)? * METERS_PER_FOOT);
    }
    let (num, factor) = if let Some(v) = s.strip_suffix("mm") {
        (v, 0.001)
    } else if let Some(v) = s.strip_suffix("cm") {
        (v, 0.01)
    } else if let Some(v) = s.strip_suffix('m') {
        (v, 1.0)
    } else {
        (s, 1.0)
    };
    val(num).map(|v| v * factor)
}

/// `x,y` (z=0) or `x,y,z`, each component with optional units.
pub fn point(s: &str) -> Result<DVec3, ParseError> {
    let parts: Vec<&str> = s.split(',').collect();
    let bad = || ParseError::BadPoint(s.to_string());
    match parts.as_slice() {
        [x, y] => Ok(DVec3::new(
            number(x).map_err(|_| bad())?,
            number(y).map_err(|_| bad())?,
            0.0,
        )),
        [x, y, z] => Ok(DVec3::new(
            number(x).map_err(|_| bad())?,
            number(y).map_err(|_| bad())?,
            number(z).map_err(|_| bad())?,
        )),
        _ => Err(bad()),
    }
}

/// Array counts `nx,ny,nz` (or `nx,ny` with nz=1) as whole numbers.
fn counts3(s: &str) -> Result<[u32; 3], ParseError> {
    let bad = || ParseError::BadNumber(s.to_string());
    let parts: Vec<u32> = s
        .split(',')
        .map(|p| p.parse::<u32>())
        .collect::<Result<_, _>>()
        .map_err(|_| bad())?;
    match parts.as_slice() {
        [x, y] => Ok([*x, *y, 1]),
        [x, y, z] => Ok([*x, *y, *z]),
        _ => Err(bad()),
    }
}

/// `r,g,b` color; values above 1 are read as a 0-255 byte triple.
/// Parse the tail of a `material2 <sel> ...` command: either `off`, a single
/// preset keyword (`concrete`/`glass`/`metal`/`wood`), or any mix of
/// `roughness=..`, `metallic=..`, `color=r,g,b` key/value tokens (a custom
/// material). Unspecified custom scalars default to a mid dielectric.
fn material2_body(
    sel: Selector,
    rest: &[&str],
    args: &[&str],
) -> Result<Command, ParseError> {
    use itsjustcad_doc::{MaterialPreset, ObjectMaterial};
    match rest {
        [] => wrong("material2", "a preset, key=value list, or 'off'", args),
        ["off"] => Ok(Command::Material2Off { targets: sel }),
        [one] if MaterialPreset::parse(one).is_some() => {
            let preset = MaterialPreset::parse(one).unwrap();
            Ok(Command::Material2 { targets: sel, material: ObjectMaterial::Preset { preset } })
        }
        tokens => {
            // Custom: key=value pairs. Defaults keep a plausible matte dielectric.
            let mut color = [0.7f32, 0.7, 0.7];
            let mut roughness = 0.5f32;
            let mut metallic = 0.0f32;
            let mut saw_any = false;
            for t in tokens {
                let (key, val) = t.split_once('=').ok_or_else(|| {
                    wrong_err("material2", "key=value pairs (roughness= metallic= color=)", args)
                })?;
                saw_any = true;
                match key {
                    "color" | "c" => color = color3(val)?,
                    "roughness" | "rough" | "r" => {
                        roughness = val
                            .parse::<f32>()
                            .map_err(|_| wrong_err("material2", "roughness=<0..1>", args))?
                            .clamp(0.0, 1.0)
                    }
                    "metallic" | "metal" | "m" => {
                        metallic = val
                            .parse::<f32>()
                            .map_err(|_| wrong_err("material2", "metallic=<0..1>", args))?
                            .clamp(0.0, 1.0)
                    }
                    _ => {
                        return wrong(
                            "material2",
                            "keys color=, roughness=, metallic= (or a preset name)",
                            args,
                        )
                    }
                }
            }
            if !saw_any {
                return wrong("material2", "a preset, key=value list, or 'off'", args);
            }
            Ok(Command::Material2 {
                targets: sel,
                material: ObjectMaterial::Custom { color, roughness, metallic },
            })
        }
    }
}

/// Parse an ISO 128 pen width by name or raw mm value.
/// Named picks: 0.13 / 0.18 / 0.25 / 0.35 / 0.50 / 0.70 / 1.00 / 1.40 / 2.00.
/// Also accepts the alternate "iso13", "iso18", … form.
/// Returns `None` if the string is not a valid ISO pen or positive number.
fn iso_pen_mm(s: &str) -> Option<f64> {
    const ISO_PENS: [f64; 9] = [0.13, 0.18, 0.25, 0.35, 0.50, 0.70, 1.00, 1.40, 2.00];
    // Strip optional "iso" prefix for "iso13", "iso18", "iso25" etc.
    let bare = s.strip_prefix("iso").unwrap_or(s);
    // Try named ISO pen: integer hundredths ("13" → 0.13, "25" → 0.25, "100" → 1.00)
    if let Ok(hundredths) = bare.parse::<u32>() {
        let mm = hundredths as f64 / 100.0;
        if ISO_PENS.iter().any(|&p| (p - mm).abs() < 1e-9) {
            return Some(mm);
        }
    }
    // Try as a raw decimal mm value.
    if let Ok(mm) = bare.parse::<f64>()
        && mm > 0.0
    {
        // Snap to nearest ISO pen if within 5% tolerance; otherwise accept raw value.
        let nearest = ISO_PENS.iter().copied().min_by(|a, b| {
            (a - mm).abs().partial_cmp(&(b - mm).abs()).unwrap()
        });
        if let Some(n) = nearest
            && (n - mm).abs() / mm < 0.05
        {
            return Some(n);
        }
        return Some(mm);
    }
    None
}

fn color3(s: &str) -> Result<[f32; 3], ParseError> {
    let bad = || ParseError::BadColor(s.to_string());
    let parts: Vec<f64> = s
        .split(',')
        .map(|p| p.trim().parse::<f64>())
        .collect::<Result<_, _>>()
        .map_err(|_| bad())?;
    let [r, g, b] = parts.as_slice() else {
        return Err(bad());
    };
    if [r, g, b].iter().any(|&&v| !(0.0..=255.0).contains(&v)) {
        return Err(bad());
    }
    let scale = if *r > 1.0 || *g > 1.0 || *b > 1.0 { 255.0 } else { 1.0 };
    Ok([
        (r / scale) as f32,
        (g / scale) as f32,
        (b / scale) as f32,
    ])
}

/// `x,y` paper-space point in mm; bare numbers are millimetres (unlike
/// `point()` where bare numbers are metres).
fn paper_point(s: &str) -> Result<[f64; 2], ParseError> {
    let bad = || ParseError::BadPoint(s.to_string());
    let parts: Vec<&str> = s.split(',').collect();
    let mm_val = |t: &str| -> Result<f64, ParseError> {
        // honour explicit unit suffixes via number(), but default to mm.
        if t.ends_with("mm") || t.ends_with("cm") || t.ends_with('m') || t.ends_with("in")
            || t.ends_with("ft")
        {
            number(t).map_err(|_| bad())
        } else {
            t.parse::<f64>().map_err(|_| bad())
        }
    };
    match parts.as_slice() {
        [x, y] => Ok([mm_val(x)?, mm_val(y)?]),
        _ => Err(bad()),
    }
}

fn paper_size(s: &str) -> Result<PaperSize, ParseError> {
    match s.to_lowercase().as_str() {
        "a4" => Ok(PaperSize::A4),
        "a3" => Ok(PaperSize::A3),
        "a2" => Ok(PaperSize::A2),
        "a1" => Ok(PaperSize::A1),
        "a0" => Ok(PaperSize::A0),
        _ => Err(ParseError::BadPaperSize(s.to_string())),
    }
}

fn view_direction(s: &str) -> Result<ViewDirection, ParseError> {
    match s.to_lowercase().as_str() {
        "top" | "plan" => Ok(ViewDirection::Top),
        "front" => Ok(ViewDirection::Front),
        "right" | "side" => Ok(ViewDirection::Right),
        "persp" | "iso" | "axo" => Ok(ViewDirection::Iso),
        _ => Err(ParseError::BadViewDirection(s.to_string())),
    }
}

/// Drawing scale as a denominator: "1:100", "100" or "1/50" all work.
fn scale_denominator(s: &str) -> Result<f64, ParseError> {
    let denom = s
        .strip_prefix("1:")
        .or_else(|| s.strip_prefix("1/"))
        .unwrap_or(s);
    denom
        .parse::<f64>()
        .ok()
        .filter(|d| *d > 0.0)
        .ok_or_else(|| ParseError::BadScale(s.to_string()))
}

/// Parse a selector from the front of `args`, returning the rest.
fn selector<'a>(
    args: &'a [&'a str],
    command: &'static str,
) -> Result<(Selector, &'a [&'a str]), ParseError> {
    let (&first, rest) = args
        .split_first()
        .ok_or_else(|| wrong_err(command, "a selector", args))?;
    match first {
        "last" => {
            // optional count: last 3
            if let Some((&count, rest2)) = rest.split_first()
                && let Ok(n) = count.parse::<usize>()
            {
                return Ok((Selector::Last { n }, rest2));
            }
            Ok((Selector::Last { n: 1 }, rest))
        }
        "all" => Ok((Selector::All, rest)),
        "sel" | "selected" => Ok((Selector::Selected, rest)),
        name if name.chars().next().is_some_and(|c| c.is_alphabetic() || c == '#') => Ok((
            Selector::Named {
                name: name.trim_start_matches('#').to_string(),
            },
            rest,
        )),
        other => Err(ParseError::BadSelector(other.to_string())),
    }
}

fn selector_one(s: &str) -> Result<Selector, ParseError> {
    let args = [s];
    selector(&args, "extrude").map(|(sel, _)| sel)
}

fn closest_command(input: &str) -> Option<String> {
    registry()
        .iter()
        .map(|s| s.name)
        .min_by_key(|name| levenshtein(input, name))
        .filter(|name| levenshtein(input, name) <= 2)
        .map(String::from)
}

/// Parse `YYYY-MM-DD` into `(year, month, day)`.
fn parse_date(s: &str) -> Result<(i32, u32, u32), ParseError> {
    let bad = || ParseError::BadNumber(s.to_string());
    let parts: Vec<&str> = s.split('-').collect();
    match parts.as_slice() {
        [y, m, d] => {
            let year = y.parse::<i32>().map_err(|_| bad())?;
            let month = m.parse::<u32>().map_err(|_| bad())?;
            let day = d.parse::<u32>().map_err(|_| bad())?;
            if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
                return Err(bad());
            }
            Ok((year, month, day))
        }
        _ => Err(bad()),
    }
}

/// Parse `HH:MM` into `(hour, minute)`.
fn parse_hhmm(s: &str) -> Result<(u32, u32), ParseError> {
    let bad = || ParseError::BadNumber(s.to_string());
    let parts: Vec<&str> = s.split(':').collect();
    match parts.as_slice() {
        [h, m] => {
            let hour = h.parse::<u32>().map_err(|_| bad())?;
            let minute = m.parse::<u32>().map_err(|_| bad())?;
            if hour > 23 || minute > 59 {
                return Err(bad());
            }
            Ok((hour, minute))
        }
        _ => Err(bad()),
    }
}

// ---------------------------------------------------------------------------
// load <kind> <target/point> <magnitude> <direction>
//
// Syntax:
//   load point <x,y,z> <magnitude> <dx,dy,dz>
//   load line  <ax,ay,az> <bx,by,bz> <magnitude> <dx,dy,dz>
//   load area  <p1> <p2> <p3> ... end <magnitude> <dx,dy,dz>
//
// The name defaults to "load" when not provided (may be overridden by a name
// qualifier later).  For now we use the kind as the name.
// ---------------------------------------------------------------------------
fn parse_load(args: &[&str]) -> Result<Command, ParseError> {
    match args.split_first() {
        Some((&"point", rest)) => match rest {
            [pos, mag, dir] => Ok(Command::AddLoad {
                name: "load".into(),
                geometry: LoadGeometry::Point { position: point(pos)? },
                magnitude: number(mag)?,
                direction: point(dir)?,
                index: None,
            }),
            [name, pos, mag, dir] => Ok(Command::AddLoad {
                name: name.to_string(),
                geometry: LoadGeometry::Point { position: point(pos)? },
                magnitude: number(mag)?,
                direction: point(dir)?,
                index: None,
            }),
            _ => wrong(
                "load",
                "load point [name] <x,y,z> <magnitude> <dx,dy,dz>",
                args,
            ),
        },
        Some((&"line", rest)) => match rest {
            [a, b, mag, dir] => Ok(Command::AddLoad {
                name: "load".into(),
                geometry: LoadGeometry::Line { a: point(a)?, b: point(b)? },
                magnitude: number(mag)?,
                direction: point(dir)?,
                index: None,
            }),
            [name, a, b, mag, dir] => Ok(Command::AddLoad {
                name: name.to_string(),
                geometry: LoadGeometry::Line { a: point(a)?, b: point(b)? },
                magnitude: number(mag)?,
                direction: point(dir)?,
                index: None,
            }),
            _ => wrong(
                "load",
                "load line [name] <ax,ay,az> <bx,by,bz> <magnitude> <dx,dy,dz>",
                args,
            ),
        },
        Some((&"area", rest)) => {
            // area [name] <p1> <p2> ... end <magnitude> <dx,dy,dz>
            // tokens until "end" are boundary points; name is detected by
            // checking whether the first token looks like a coordinate (contains ',')
            // or not (a bare word = name).
            let (name, pts_and_rest) = if rest.first().is_some_and(|t| !t.contains(',')) {
                (rest[0].to_string(), &rest[1..])
            } else {
                ("load".into(), rest)
            };
            // split on "end"
            let end_pos = pts_and_rest
                .iter()
                .position(|&t| t.eq_ignore_ascii_case("end"))
                .ok_or_else(|| {
                    wrong_err("load", "load area <p1> <p2> ... end <magnitude> <dir>", args)
                })?;
            let (boundary_tokens, after_end) = pts_and_rest.split_at(end_pos);
            let after_end = &after_end[1..]; // skip "end"
            if boundary_tokens.len() < 3 {
                return wrong(
                    "load",
                    "load area needs at least 3 boundary points before 'end'",
                    args,
                );
            }
            let boundary: Result<Vec<_>, _> = boundary_tokens.iter().map(|t| point(t)).collect();
            match after_end {
                [mag, dir] => Ok(Command::AddLoad {
                    name,
                    geometry: LoadGeometry::Area { boundary: boundary? },
                    magnitude: number(mag)?,
                    direction: point(dir)?,
                    index: None,
                }),
                _ => wrong("load", "load area ... end <magnitude> <dx,dy,dz>", args),
            }
        }
        _ => wrong(
            "load",
            "load <point|line|area> [name] <target> <magnitude> <direction>",
            args,
        ),
    }
}

// ---------------------------------------------------------------------------
// support <x,y,z> <pinned|fixed|roller> [axis dx,dy,dz]
// ---------------------------------------------------------------------------
fn parse_support(args: &[&str]) -> Result<Command, ParseError> {
    match args {
        [pos, kind] => {
            let k = restraint_kind(kind)?;
            if k == RestraintKind::Roller {
                return wrong("support", "roller supports require an axis: support <pos> roller <dx,dy,dz>", args);
            }
            Ok(Command::AddSupport {
                position: point(pos)?,
                kind: k,
                roller_axis: None,
                index: None,
            })
        }
        [pos, kind, axis] => {
            let k = restraint_kind(kind)?;
            let ax = if k == RestraintKind::Roller { Some(point(axis)?) } else { None };
            Ok(Command::AddSupport {
                position: point(pos)?,
                kind: k,
                roller_axis: ax,
                index: None,
            })
        }
        _ => wrong("support", "support <x,y,z> <pinned|fixed|roller> [axis dx,dy,dz]", args),
    }
}

fn restraint_kind(s: &str) -> Result<RestraintKind, ParseError> {
    match s.to_lowercase().as_str() {
        "pinned" | "pin" => Ok(RestraintKind::Pinned),
        "fixed" | "fix" => Ok(RestraintKind::Fixed),
        "roller" | "roll" => Ok(RestraintKind::Roller),
        _ => Err(ParseError::WrongArgs {
            command: "support",
            expected: "pinned, fixed or roller",
            got: s.to_string(),
            usage: "support <x,y,z> <pinned|fixed|roller> [axis dx,dy,dz]",
        }),
    }
}

fn levenshtein(a: &str, b: &str) -> usize {
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut curr = vec![0; b.len() + 1];
    for i in 1..=a.len() {
        curr[0] = i;
        for j in 1..=b.len() {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            curr[j] = (prev[j] + 1).min(curr[j - 1] + 1).min(prev[j - 1] + cost);
        }
        std::mem::swap(&mut prev, &mut curr);
    }
    prev[b.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_box() {
        let cmd = parse("box 0,0,0 5,5,3").unwrap();
        assert_eq!(
            cmd,
            Command::Box {
                id: None,
                corner: DVec3::ZERO,
                size: DVec3::new(5.0, 5.0, 3.0)
            }
        );
    }

    #[test]
    fn parse_exact_boolean_round_trip() {
        assert_eq!(
            parse("exact_boolean difference 0,0,0 10,10,10 3,3,-5 4,4,20").unwrap(),
            Command::ExactBoolean {
                id: None,
                op: BoolKind::Difference,
                a_corner: DVec3::ZERO,
                a_size: DVec3::new(10.0, 10.0, 10.0),
                b_corner: DVec3::new(3.0, 3.0, -5.0),
                b_size: DVec3::new(4.0, 4.0, 20.0),
            }
        );
        // op aliases resolve
        assert!(matches!(
            parse("exact_boolean u 0,0,0 1,1,1 0,0,0 1,1,1").unwrap(),
            Command::ExactBoolean { op: BoolKind::Union, .. }
        ));
        assert!(matches!(
            parse("exactbool i 0,0,0 1,1,1 0,0,0 1,1,1").unwrap(),
            Command::ExactBoolean { op: BoolKind::Intersection, .. }
        ));
    }

    #[test]
    fn parse_exact_boolean_rejects_bad_op() {
        assert!(parse("exact_boolean nope 0,0,0 1,1,1 0,0,0 1,1,1").is_err());
    }

    #[test]
    fn parse_geodesic_variants() {
        assert_eq!(
            parse("geodesic 3 5").unwrap(),
            Command::Geodesic { id: None, frequency: 3, radius: 5.0, full: false }
        );
        assert_eq!(
            parse("geodesic 2 4 dome").unwrap(),
            Command::Geodesic { id: None, frequency: 2, radius: 4.0, full: false }
        );
        assert_eq!(
            parse("geodesic 4 10 full").unwrap(),
            Command::Geodesic { id: None, frequency: 4, radius: 10.0, full: true }
        );
        assert!(parse("geodesic 3 5 bogus").is_err());
    }

    #[test]
    fn parse_spaceframe() {
        assert_eq!(
            parse("spaceframe 6 4 3 1.5").unwrap(),
            Command::SpaceFrame { id: None, nx: 6, ny: 4, bay: 3.0, depth: 1.5 }
        );
    }

    #[test]
    fn parse_hypar_with_and_without_divisions() {
        assert_eq!(
            parse("hypar 5 5 5").unwrap(),
            Command::Hypar { id: None, a: 5.0, b: 5.0, c: 5.0, nu: None, nv: None }
        );
        assert_eq!(
            parse("hypar 5 6 2 8 10").unwrap(),
            Command::Hypar { id: None, a: 5.0, b: 6.0, c: 2.0, nu: Some(8), nv: Some(10) }
        );
    }

    #[test]
    fn parse_gaussvault_straight_and_undulate() {
        assert_eq!(
            parse("gaussvault 6 12 3").unwrap(),
            Command::GaussVault { id: None, span: 6.0, length: 12.0, rise: 3.0, undulate: false }
        );
        assert_eq!(
            parse("gaussvault 6 12 3 undulate").unwrap(),
            Command::GaussVault { id: None, span: 6.0, length: 12.0, rise: 3.0, undulate: true }
        );
    }

    #[test]
    fn parse_gridshell_hypar_and_vault() {
        assert_eq!(
            parse("gridshell hypar 5 5 5").unwrap(),
            Command::Gridshell {
                id: None,
                surface: crate::GridshellSurfaceSpec::Hypar { a: 5.0, b: 5.0, c: 5.0 },
                nu: None,
                nv: None,
            }
        );
        assert_eq!(
            parse("gridshell vault 6 12 3 undulate 10 20").unwrap(),
            Command::Gridshell {
                id: None,
                surface: crate::GridshellSurfaceSpec::Vault {
                    span: 6.0,
                    length: 12.0,
                    rise: 3.0,
                    undulate: true,
                },
                nu: Some(10),
                nv: Some(20),
            }
        );
    }

    #[test]
    fn parse_funicular_variants() {
        assert_eq!(
            parse("funicular -5,0,0 5,0,0").unwrap(),
            Command::Funicular {
                id: None,
                support_a: DVec3::new(-5.0, 0.0, 0.0),
                support_b: DVec3::new(5.0, 0.0, 0.0),
                segments: None,
                load: None,
                slack: None,
                invert: false,
            }
        );
        assert_eq!(
            parse("funicular -5,0,0 5,0,0 24 1 1.4 invert").unwrap(),
            Command::Funicular {
                id: None,
                support_a: DVec3::new(-5.0, 0.0, 0.0),
                support_b: DVec3::new(5.0, 0.0, 0.0),
                segments: Some(24),
                load: Some(1.0),
                slack: Some(1.4),
                invert: true,
            }
        );
    }

    #[test]
    fn parse_tensegrity_and_cablenet() {
        assert_eq!(
            parse("tensegrity 3 1 2").unwrap(),
            Command::Tensegrity {
                id: None,
                struts: 3,
                radius: Some(1.0),
                height: Some(2.0),
                twist_deg: None,
            }
        );
        assert_eq!(
            parse("tensegrity tprism").unwrap(),
            Command::Tensegrity {
                id: None,
                struts: 3,
                radius: None,
                height: None,
                twist_deg: None,
            }
        );
        assert_eq!(
            parse("cablenet 0,0,0 8,0,0 8,8,3 0,8,3 5 1.5").unwrap(),
            Command::Cablenet {
                id: None,
                corners: [
                    DVec3::new(0.0, 0.0, 0.0),
                    DVec3::new(8.0, 0.0, 0.0),
                    DVec3::new(8.0, 8.0, 3.0),
                    DVec3::new(0.0, 8.0, 3.0),
                ],
                n: Some(5),
                sag: Some(1.5),
            }
        );
    }

    /// Serde: new commands survive a JSON round-trip, and older logs that
    /// predate the optional fields (only the required ones present) still
    /// deserialize with the defaults.
    #[test]
    fn serde_roundtrip_and_pre_optional_fields() {
        let cmds = [
            parse("funicular -5,0,0 5,0,0 24 1 1.4 invert").unwrap(),
            parse("tensegrity 3 1 2").unwrap(),
            parse("cablenet 0,0,0 8,0,0 8,8,3 0,8,3 5 1.5").unwrap(),
        ];
        for c in &cmds {
            let js = serde_json::to_string(c).unwrap();
            let back: Command = serde_json::from_str(&js).unwrap();
            assert_eq!(*c, back);
        }
        // pre_* : a minimal funicular log (only supports) → defaults elsewhere.
        let pre = r#"{"cmd":"funicular","support_a":[-5.0,0.0,0.0],"support_b":[5.0,0.0,0.0]}"#;
        assert_eq!(
            serde_json::from_str::<Command>(pre).unwrap(),
            Command::Funicular {
                id: None,
                support_a: DVec3::new(-5.0, 0.0, 0.0),
                support_b: DVec3::new(5.0, 0.0, 0.0),
                segments: None,
                load: None,
                slack: None,
                invert: false,
            }
        );
        let pre_t = r#"{"cmd":"tensegrity","struts":3}"#;
        assert_eq!(
            serde_json::from_str::<Command>(pre_t).unwrap(),
            Command::Tensegrity { id: None, struts: 3, radius: None, height: None, twist_deg: None }
        );
    }

    #[test]
    fn parse_timber_and_guadua_sections() {
        assert_eq!(
            parse("section glb timber 0.2 0.6").unwrap(),
            Command::DefSection {
                name: "glb".into(),
                section: StructSection::Timber { w: 0.2, h: 0.6 },
            }
        );
        assert_eq!(
            parse("section culm guadua 0.1 0.01").unwrap(),
            Command::DefSection {
                name: "culm".into(),
                section: StructSection::Guadua { d: 0.1, t: 0.01 },
            }
        );
        // Aliases resolve to the same variants.
        assert_eq!(
            parse("section p bamboo 0.09 0.008").unwrap(),
            Command::DefSection {
                name: "p".into(),
                section: StructSection::Guadua { d: 0.09, t: 0.008 },
            }
        );
    }

    #[test]
    fn parse_amend_wraps_inner_command() {
        let cmd = parse("amend 0 box 0,0,0 8,8,3").unwrap();
        assert_eq!(
            cmd,
            Command::Amend {
                step: 0,
                with: Box::new(Command::Box {
                    id: None,
                    corner: DVec3::ZERO,
                    size: DVec3::new(8.0, 8.0, 3.0)
                })
            }
        );
        // Inner parse errors surface with their own hints.
        assert!(parse("amend 0 bax 0,0,0 1,1,1").is_err());
        // Missing step or missing command.
        assert!(matches!(parse("amend"), Err(ParseError::WrongArgs { .. })));
        assert!(matches!(parse("amend 2"), Err(ParseError::WrongArgs { .. })));
        assert!(matches!(parse("amend x box 0,0,0 1,1,1"), Err(ParseError::WrongArgs { .. })));
    }

    #[test]
    fn parse_units() {
        assert_eq!(number("250cm").unwrap(), 2.5);
        assert_eq!(number("500mm").unwrap(), 0.5);
        assert_eq!(number("3m").unwrap(), 3.0);
        assert_eq!(number("4.5").unwrap(), 4.5);
    }

    #[test]
    fn parse_imperial_suffixes() {
        assert_eq!(number("12ft").unwrap(), 12.0 * METERS_PER_FOOT);
        assert_eq!(number("6in").unwrap(), 6.0 * METERS_PER_INCH);
        assert_eq!(number("0.5ft").unwrap(), 0.5 * METERS_PER_FOOT);
        assert_eq!(number("-3ft").unwrap(), -3.0 * METERS_PER_FOOT);
        // feet-inches: 12ft6in = 150 inches; sign applies to the whole length
        assert_eq!(number("12ft6in").unwrap(), 150.0 * METERS_PER_INCH);
        assert_eq!(number("12ft6.5in").unwrap(), 150.5 * METERS_PER_INCH);
        assert_eq!(number("-12ft6in").unwrap(), -150.0 * METERS_PER_INCH);
        assert!(number("ft").is_err());
        assert!(number("12ftin").is_err());
        assert!(number("12ft6").is_err());
        // points accept imperial components
        assert_eq!(
            point("12ft,6in").unwrap(),
            DVec3::new(12.0 * METERS_PER_FOOT, 6.0 * METERS_PER_INCH, 0.0)
        );
    }

    #[test]
    fn parse_view_commands() {
        assert_eq!(
            parse("view save entry").unwrap(),
            Command::ViewSave { name: "entry".to_string(), camera: None }
        );
        assert_eq!(
            parse("view entry").unwrap(),
            Command::ViewRestore { name: "entry".to_string() }
        );
        assert_eq!(parse("view list").unwrap(), Command::ViewList);
        assert!(parse("view").unwrap_err().to_string().contains("save"));
        assert!(parse("view save").unwrap_err().to_string().contains("save"));
        assert!(parse("view save a b").is_err());
    }

    #[test]
    fn parse_units_command() {
        for (arg, units) in [
            ("m", Units::M),
            ("cm", Units::Cm),
            ("mm", Units::Mm),
            ("ft", Units::Ft),
            ("in", Units::In),
            ("ftin", Units::FtIn),
        ] {
            assert_eq!(
                parse(&format!("units {arg}")).unwrap(),
                Command::Units { units },
                "{arg}"
            );
        }
        assert!(parse("units furlongs").unwrap_err().to_string().contains("ftin"));
        assert!(parse("units").unwrap_err().to_string().contains("unit"));
        // logged so files carry their unit through replay
        assert!(parse("units ft").unwrap().is_logged());
    }

    #[test]
    fn units_command_json_roundtrip() {
        for line in ["units m", "units ftin", "units ft"] {
            let cmd = parse(line).unwrap();
            let json = serde_json::to_string(&cmd).unwrap();
            let back: Command = serde_json::from_str(&json).unwrap();
            assert_eq!(cmd, back, "{line}");
        }
    }

    #[test]
    fn parse_underlay_variants() {
        assert_eq!(
            parse("underlay site.png").unwrap(),
            Command::Underlay {
                path: "site.png".into(),
                corner: None,
                width: None,
                height: None,
            }
        );
        assert_eq!(
            parse("underlay /tmp/a.png 1,2").unwrap(),
            Command::Underlay {
                path: "/tmp/a.png".into(),
                corner: Some(DVec3::new(1.0, 2.0, 0.0)),
                width: None,
                height: None,
            }
        );
        assert_eq!(
            parse("underlay /tmp/a.png 1,2 20").unwrap(),
            Command::Underlay {
                path: "/tmp/a.png".into(),
                corner: Some(DVec3::new(1.0, 2.0, 0.0)),
                width: Some(20.0),
                height: None,
            }
        );
        // all logged so files carry the underlay through replay
        assert!(parse("underlay a.png").unwrap().is_logged());
    }

    #[test]
    fn parse_underlay_opacity_and_off() {
        assert_eq!(
            parse("underlayopacity 0.4").unwrap(),
            Command::UnderlayOpacity { opacity: 0.4 }
        );
        assert!(parse("underlayopacity 2").unwrap_err().to_string().contains("between 0 and 1"));
        assert_eq!(parse("underlayoff").unwrap(), Command::UnderlayOff);
        assert!(parse("underlayoff junk").is_err());
    }

    #[test]
    fn underlay_json_roundtrip() {
        for line in ["underlay a.png", "underlay a.png 1,2 5", "underlayopacity 0.3", "underlayoff"] {
            let cmd = parse(line).unwrap();
            let json = serde_json::to_string(&cmd).unwrap();
            let back: Command = serde_json::from_str(&json).unwrap();
            assert_eq!(cmd, back, "{line}");
        }
    }

    #[test]
    fn parse_2d_point_z_zero() {
        assert_eq!(point("3,4").unwrap(), DVec3::new(3.0, 4.0, 0.0));
    }

    #[test]
    fn parse_selectors() {
        assert!(matches!(
            parse("delete last").unwrap(),
            Command::Delete { targets: Selector::Last { n: 1 } }
        ));
        assert!(matches!(
            parse("move last 3 1,0,0").unwrap(),
            Command::Move { targets: Selector::Last { n: 3 }, .. }
        ));
        assert!(matches!(
            parse("delete all").unwrap(),
            Command::Delete { targets: Selector::All }
        ));
        assert!(matches!(
            parse("delete tower-a").unwrap(),
            Command::Delete { targets: Selector::Named { .. } }
        ));
    }

    #[test]
    fn parse_polyline_closed() {
        let cmd = parse("polyline 0,0 5,0 5,5 closed").unwrap();
        assert!(matches!(cmd, Command::Polyline { closed: true, ref points, .. } if points.len() == 3));
    }

    #[test]
    fn parse_curve_with_degree() {
        let cmd = parse("curve 0,0 2,4 6,4 8,0 degree 2").unwrap();
        assert!(matches!(cmd, Command::Curve { degree: 2, ref points, .. } if points.len() == 4));
    }

    #[test]
    fn parse_interpcurve_open_and_closed() {
        assert!(matches!(
            parse("interpcurve 0,0 2,4 6,4 8,0").unwrap(),
            Command::InterpCurve { closed: false, ref points, .. } if points.len() == 4
        ));
        assert!(matches!(
            parse("interpcurve 0,0 2,4 6,4 closed").unwrap(),
            Command::InterpCurve { closed: true, ref points, .. } if points.len() == 3
        ));
        assert!(parse("interpcurve 0,0 2,4").is_err()); // needs 3+
    }

    #[test]
    fn parse_helix() {
        assert!(matches!(
            parse("helix 0,0,0 3 12 4").unwrap(),
            Command::Helix { radius, height, turns, .. }
                if radius == 3.0 && height == 12.0 && turns == 4.0
        ));
    }

    #[test]
    fn parse_setpoint_and_rebuild() {
        assert!(matches!(
            parse("setpoint last 2 4,5,0").unwrap(),
            Command::SetPoint { index: 2, target: Selector::Last { n: 1 }, .. }
        ));
        assert!(matches!(
            parse("rebuild last 20").unwrap(),
            Command::Rebuild { count: 20, target: Selector::Last { n: 1 }, .. }
        ));
    }

    #[test]
    fn parse_transforms() {
        assert!(matches!(
            parse("rotate last 45").unwrap(),
            Command::Rotate { angle_deg, axis: DVec3::Z, center: None, .. } if angle_deg == 45.0
        ));
        assert!(matches!(
            parse("rotate all 90 x about 0,0,0").unwrap(),
            Command::Rotate {
                targets: Selector::All,
                axis: DVec3::X,
                center: Some(DVec3::ZERO),
                ..
            }
        ));
        assert!(matches!(
            parse("scale last 2").unwrap(),
            Command::Scale { factors, center: None, .. } if factors == DVec3::splat(2.0)
        ));
        assert!(matches!(
            parse("scale last 1,1,2").unwrap(),
            Command::Scale { factors, .. } if factors == DVec3::new(1.0, 1.0, 2.0)
        ));
        assert!(matches!(
            parse("mirror last yz").unwrap(),
            Command::Mirror { plane: MirrorPlane::Yz, .. }
        ));
        assert!(matches!(
            parse("mirror last 0,5,0 0,1,0").unwrap(),
            Command::Mirror { plane: MirrorPlane::PointNormal { .. }, .. }
        ));
        // scale rejects garbage after the center clause
        assert!(parse("scale last 2 about 0,0,0 extra").is_err());
        // offset: bare number after 'last' is the distance (backtrack), and
        // negative distances parse
        assert!(matches!(
            parse("offset last 0.2").unwrap(),
            Command::Offset { distance, target: Selector::Last { n: 1 }, .. } if distance == 0.2
        ));
        assert!(matches!(
            parse("offset walls -0.5").unwrap(),
            Command::Offset { distance, .. } if distance == -0.5
        ));
        // rotate needs an angle
        let err = parse("rotate last").unwrap_err();
        assert!(err.to_string().contains("angle"), "{err}");
    }

    #[test]
    fn parse_array_commands() {
        assert_eq!(
            parse("array last 5,3,1 3,4,0").unwrap(),
            Command::Array {
                ids: None,
                targets: Selector::Last { n: 1 },
                counts: [5, 3, 1],
                delta: DVec3::new(3.0, 4.0, 0.0),
            }
        );
        // 2-component counts default nz=1; selector counts still work
        assert!(matches!(
            parse("array last 2 4,2 6,6,0").unwrap(),
            Command::Array { targets: Selector::Last { n: 2 }, counts: [4, 2, 1], .. }
        ));
        // spacings accept unit suffixes
        assert!(matches!(
            parse("array cols 3,3 3m,400cm").unwrap(),
            Command::Array { counts: [3, 3, 1], delta, .. }
                if delta == DVec3::new(3.0, 4.0, 0.0)
        ));
        assert!(parse("array last").unwrap_err().to_string().contains("counts"));
        assert!(parse("array last 5,3,1").unwrap_err().to_string().contains("delta"));
        assert!(parse("array last 1.5,3 1,0,0").is_err()); // counts are whole numbers

        // polararray: backtrack makes "last 8" read as last + count 8
        assert_eq!(
            parse("polararray last 8").unwrap(),
            Command::PolarArray {
                ids: None,
                targets: Selector::Last { n: 1 },
                count: 8,
                center: None,
                total_angle_deg: None,
            }
        );
        assert!(matches!(
            parse("polararray last 2 6").unwrap(),
            Command::PolarArray { targets: Selector::Last { n: 2 }, count: 6, .. }
        ));
        assert!(matches!(
            parse("parray col 6 0,0,0 180").unwrap(),
            Command::PolarArray {
                targets: Selector::Named { .. },
                count: 6,
                center: Some(DVec3::ZERO),
                total_angle_deg: Some(total),
                ..
            } if total == 180.0
        ));
        assert!(matches!(
            parse("polararray col 4 90").unwrap(),
            Command::PolarArray { count: 4, center: None, total_angle_deg: Some(total), .. }
                if total == 90.0
        ));
        // greedy selector wins the ambiguous "last 4 90": 4 objects, 90 copies
        assert!(matches!(
            parse("polararray last 4 90").unwrap(),
            Command::PolarArray { targets: Selector::Last { n: 4 }, count: 90, .. }
        ));
        assert!(parse("polararray last").unwrap_err().to_string().contains("count"));
        assert!(parse("polararray last 4 0,0 90 extra").is_err());
    }

    #[test]
    fn array_command_json_roundtrip() {
        for line in [
            "array last 5,3,1 3,4,0",
            "array last 2 4,2 6,6,0",
            "polararray last 8",
            "parray col 6 0,0,0 180",
        ] {
            let cmd = parse(line).unwrap();
            let json = serde_json::to_string(&cmd).unwrap();
            let back: Command = serde_json::from_str(&json).unwrap();
            assert_eq!(cmd, back, "{line}");
        }
    }

    #[test]
    fn exact_boolean_json_roundtrip() {
        // op-log replay-stability: the command must survive JSON round-trip.
        for line in [
            "exact_boolean union 0,0,0 2,2,2 5,0,0 2,2,2",
            "exact_boolean difference 0,0,0 10,10,10 3,3,-5 4,4,20",
            "exact_boolean intersection 1,1,1 4,4,4 2,2,2 4,4,4",
        ] {
            let cmd = parse(line).unwrap();
            let json = serde_json::to_string(&cmd).unwrap();
            let back: Command = serde_json::from_str(&json).unwrap();
            assert_eq!(cmd, back, "{line}");
        }
        // The op tag is snake_case and stable.
        let cmd = parse("exact_boolean difference 0,0,0 1,1,1 0,0,0 1,1,1").unwrap();
        let json = serde_json::to_string(&cmd).unwrap();
        assert!(json.contains("\"exact_boolean\""), "cmd tag: {json}");
        assert!(json.contains("\"difference\""), "op tag: {json}");
    }

    #[test]
    fn parse_curve_edit_commands() {
        assert!(matches!(
            parse("split last 5,0").unwrap(),
            Command::Split { ids: None, target: Selector::Last { n: 1 }, point }
                if point == DVec3::new(5.0, 0.0, 0.0)
        ));
        assert!(matches!(
            parse("trim wall slab 1,1").unwrap(),
            Command::Trim {
                id: None,
                target: Selector::Named { .. },
                cutter: Selector::Named { .. },
                keep,
            } if keep == DVec3::new(1.0, 1.0, 0.0)
        ));
        // extend: backtrack makes "last 2" read as last + distance 2
        assert!(matches!(
            parse("extend last 2").unwrap(),
            Command::Extend { targets: Selector::Last { n: 1 }, distance } if distance == 2.0
        ));
        assert!(matches!(
            parse("extend last 2 0.5").unwrap(),
            Command::Extend { targets: Selector::Last { n: 2 }, distance } if distance == 0.5
        ));
        assert!(matches!(
            parse("join last 3").unwrap(),
            Command::Join { id: None, targets: Selector::Last { n: 3 } }
        ));
        // fillet: two selectors + radius, or one selector naming both curves
        assert!(matches!(
            parse("fillet l1 l2 0.5").unwrap(),
            Command::Fillet { a: Selector::Named { .. }, b: Selector::Named { .. }, radius, .. }
                if radius == 0.5
        ));
        assert!(matches!(
            parse("fillet last 2 0.5").unwrap(),
            Command::Fillet { a: Selector::Last { n: 2 }, b: Selector::Last { n: 2 }, radius, .. }
                if radius == 0.5
        ));
        // unit suffixes work
        assert!(matches!(
            parse("fillet last 2 50cm").unwrap(),
            Command::Fillet { radius, .. } if radius == 0.5
        ));
        // errors carry hints
        assert!(parse("split last").unwrap_err().to_string().contains("point"));
        assert!(parse("trim last").unwrap_err().to_string().contains("selector"));
        assert!(parse("extend last").unwrap_err().to_string().contains("distance"));
        assert!(parse("join").unwrap_err().to_string().contains("selector"));
        assert!(parse("fillet last").unwrap_err().to_string().contains("radius"));
    }

    #[test]
    fn curve_edit_command_json_roundtrip() {
        for line in [
            "split last 5,0",
            "trim wall slab 1,1",
            "extend last 0.5",
            "join last 3",
            "fillet last 2 0.5",
        ] {
            let cmd = parse(line).unwrap();
            let json = serde_json::to_string(&cmd).unwrap();
            let back: Command = serde_json::from_str(&json).unwrap();
            assert_eq!(cmd, back, "{line}");
        }
    }

    #[test]
    fn parse_solid_commands() {
        // revolve: everything optional after the selector
        assert_eq!(
            parse("revolve last").unwrap(),
            Command::Revolve {
                id: None,
                profile: Selector::Last { n: 1 },
                axis_point: None,
                axis_dir: None,
                angle_deg: None,
            }
        );
        // bare number after 'last' is the angle, never an object count
        assert!(matches!(
            parse("revolve last 180").unwrap(),
            Command::Revolve { profile: Selector::Last { n: 1 }, angle_deg: Some(a), .. }
                if a == 180.0
        ));
        assert!(matches!(
            parse("revolve vase 0,0,0 0,0,1 270").unwrap(),
            Command::Revolve {
                profile: Selector::Named { .. },
                axis_point: Some(DVec3::ZERO),
                axis_dir: Some(d),
                angle_deg: Some(a),
                ..
            } if d == DVec3::Z && a == 270.0
        ));
        assert!(matches!(
            parse("revolve last 5,0,0 0,0,1").unwrap(),
            Command::Revolve { axis_point: Some(p), axis_dir: Some(d), angle_deg: None, .. }
                if p == DVec3::new(5.0, 0.0, 0.0) && d == DVec3::Z
        ));
        assert!(matches!(
            parse("loft last 3").unwrap(),
            Command::Loft { id: None, targets: Selector::Last { n: 3 } }
        ));
        assert!(matches!(
            parse("sweep prof rail").unwrap(),
            Command::Sweep {
                id: None,
                profile: Selector::Named { .. },
                rail: Selector::Named { .. },
            }
        ));
        assert!(matches!(
            parse("sweep2 prof ra rb").unwrap(),
            Command::Sweep2 { id: None, .. }
        ));
        assert!(matches!(
            parse("railrevolve prof rail 0,0,0 0,0,1").unwrap(),
            Command::RailRevolve { id: None, axis_dir, .. } if axis_dir == DVec3::Z
        ));
        assert!(matches!(
            parse("pipe path 2 0.5").unwrap(),
            Command::Pipe { radius, end_radius: Some(e), .. } if radius == 2.0 && e == 0.5
        ));
        assert!(matches!(
            parse("pipe path 2").unwrap(),
            Command::Pipe { end_radius: None, .. }
        ));
        // errors carry hints
        assert!(parse("sweep2 prof ra").unwrap_err().to_string().contains("selector"));
        assert!(parse("railrevolve prof rail").unwrap_err().to_string().contains("axis"));
        assert!(parse("pipe path").unwrap_err().to_string().contains("radius"));
        assert!(parse("revolve").unwrap_err().to_string().contains("selector"));
        assert!(parse("revolve last 0,0,0 0,0,1 90 extra").unwrap_err().to_string().contains("axis"));
        assert!(parse("loft").unwrap_err().to_string().contains("selector"));
        assert!(parse("sweep prof").unwrap_err().to_string().contains("selector"));
        assert!(parse("loft last 2 extra").is_err());
    }

    #[test]
    fn solid_command_json_roundtrip() {
        for line in [
            "revolve last",
            "revolve last 180",
            "revolve vase 0,0,0 0,0,1 270",
            "loft last 3",
            "sweep prof rail",
            "sweep2 prof ra rb",
            "railrevolve prof rail 0,0,0 0,0,1",
            "pipe path 2",
            "pipe path 2 0.5",
        ] {
            let cmd = parse(line).unwrap();
            let json = serde_json::to_string(&cmd).unwrap();
            let back: Command = serde_json::from_str(&json).unwrap();
            assert_eq!(cmd, back, "{line}");
        }
    }

    #[test]
    fn parse_section_commands() {
        assert_eq!(
            parse("section all 0,0,1.5 0,0,1").unwrap(),
            Command::Section {
                ids: None,
                targets: Selector::All,
                point: DVec3::new(0.0, 0.0, 1.5),
                normal: DVec3::Z,
            }
        );
        // "last N" selector count still works before the two points
        assert!(matches!(
            parse("section last 2 5,0,0 1,0,0").unwrap(),
            Command::Section { targets: Selector::Last { n: 2 }, normal, .. }
                if normal == DVec3::X
        ));
        assert_eq!(parse("plan 1.2").unwrap(), Command::Plan { ids: None, height: 1.2 });
        // heights accept unit suffixes
        assert!(matches!(
            parse("plan 4ft").unwrap(),
            Command::Plan { height, .. } if (height - 4.0 * METERS_PER_FOOT).abs() < 1e-12
        ));
        // errors carry hints
        assert!(parse("section").unwrap_err().to_string().contains("selector"));
        assert!(parse("section all 0,0,1.5").unwrap_err().to_string().contains("normal"));
        assert!(parse("plan").unwrap_err().to_string().contains("height"));
        assert!(parse("plan 1 2").unwrap_err().to_string().contains("height"));
    }

    #[test]
    fn parse_elevation_commands() {
        assert_eq!(
            parse("elevation south").unwrap(),
            Command::Elevation { ids: None, direction: CompassDir::South, depth: 0.0 }
        );
        assert!(matches!(
            parse("elevation east 2.5").unwrap(),
            Command::Elevation { direction: CompassDir::East, depth, .. }
                if (depth - 2.5).abs() < 1e-12
        ));
        for (line, dir) in [
            ("elevation north", CompassDir::North),
            ("elevation west", CompassDir::West),
        ] {
            assert!(matches!(parse(line).unwrap(), Command::Elevation { direction, .. } if direction == dir));
        }
        // errors: bad direction, missing direction, too many args
        assert!(parse("elevation up").unwrap_err().to_string().contains("direction"));
        assert!(parse("elevation").unwrap_err().to_string().contains("direction"));
        assert!(parse("elevation south 1 2").unwrap_err().to_string().contains("direction"));
    }

    #[test]
    fn section_command_json_roundtrip() {
        for line in [
            "section all 0,0,1.5 0,0,1",
            "section last 2 5,0,0 1,0,0",
            "plan 1.2",
            "elevation north",
            "elevation east 2",
        ] {
            let cmd = parse(line).unwrap();
            let json = serde_json::to_string(&cmd).unwrap();
            let back: Command = serde_json::from_str(&json).unwrap();
            assert_eq!(cmd, back, "{line}");
        }
    }

    #[test]
    fn parse_booleans() {
        assert!(matches!(
            parse("union last 2").unwrap(),
            Command::Union { targets: Selector::Last { n: 2 }, .. }
        ));
        assert!(matches!(
            parse("difference tower core").unwrap(),
            Command::Difference {
                target: Selector::Named { .. },
                tools: Selector::Named { .. },
                ..
            }
        ));
        assert!(matches!(
            parse("diff last 2 last").unwrap(),
            Command::Difference {
                target: Selector::Last { n: 2 },
                tools: Selector::Last { n: 1 },
                ..
            }
        ));
        assert!(matches!(
            parse("intersect all").unwrap(),
            Command::Intersect { targets: Selector::All, .. }
        ));
        // missing tool selector → usage in the error
        let err = parse("difference last").unwrap_err();
        assert!(err.to_string().contains("selector"), "{err}");
    }

    #[test]
    fn parse_group_commands() {
        assert_eq!(
            parse("group last 2 boxes").unwrap(),
            Command::Group { targets: Selector::Last { n: 2 }, name: Some("boxes".into()) }
        );
        assert_eq!(
            parse("group last 2").unwrap(),
            Command::Group { targets: Selector::Last { n: 2 }, name: None }
        );
        assert_eq!(
            parse("group sel").unwrap(),
            Command::Group { targets: Selector::Selected, name: None }
        );
        assert_eq!(
            parse("ungroup boxes").unwrap(),
            Command::Ungroup { targets: Selector::Named { name: "boxes".into() } }
        );
        assert_eq!(
            parse("ungroup last").unwrap(),
            Command::Ungroup { targets: Selector::Last { n: 1 } }
        );
        // both are logged so groups survive save/replay
        assert!(parse("group last 2").unwrap().is_logged());
        assert!(parse("ungroup last").unwrap().is_logged());
        // errors carry hints
        assert!(parse("group").unwrap_err().to_string().contains("selector"));
        assert!(parse("group last 2 a b").unwrap_err().to_string().contains("name"));
        assert!(parse("ungroup").unwrap_err().to_string().contains("selector"));
        assert!(parse("ungroup boxes extra").is_err());
    }

    #[test]
    fn group_command_json_roundtrip() {
        for line in ["group last 2 boxes", "group all", "ungroup boxes"] {
            let cmd = parse(line).unwrap();
            let json = serde_json::to_string(&cmd).unwrap();
            let back: Command = serde_json::from_str(&json).unwrap();
            assert_eq!(cmd, back, "{line}");
        }
    }

    #[test]
    fn parse_layer_commands() {
        assert_eq!(
            parse("layer walls").unwrap(),
            Command::Layer { name: "walls".into() }
        );
        assert!(matches!(
            parse("tolayer last 2 walls").unwrap(),
            Command::ToLayer { targets: Selector::Last { n: 2 }, ref layer } if layer == "walls"
        ));
        assert!(matches!(
            parse("tolayer slab structure").unwrap(),
            Command::ToLayer { targets: Selector::Named { .. }, ref layer } if layer == "structure"
        ));
        assert_eq!(
            parse("layercolor walls 0.8,0.2,0.1").unwrap(),
            Command::LayerColor { layer: "walls".into(), color: [0.8, 0.2, 0.1] }
        );
        // byte triples scale down
        let Command::LayerColor { color, .. } = parse("layercolor walls 255,0,128").unwrap()
        else {
            panic!("expected layercolor")
        };
        assert_eq!(color, [1.0, 0.0, 128.0 / 255.0]);
        assert_eq!(parse("hide walls").unwrap(), Command::Hide { layer: "walls".into() });
        assert_eq!(parse("show walls").unwrap(), Command::Show { layer: "walls".into() });
        // Multi-word layer names (seeded "Layer 01" style) join across tokens.
        assert_eq!(
            parse("layer Layer 01").unwrap(),
            Command::Layer { name: "Layer 01".into() }
        );
        assert_eq!(
            parse("hide Layer 01").unwrap(),
            Command::Hide { layer: "Layer 01".into() }
        );
        assert_eq!(
            parse("layerdelete Layer 01").unwrap(),
            Command::LayerDelete { layer: "Layer 01".into() }
        );
        // For "<layer> <value>" the trailing token is the value; the rest is name.
        assert_eq!(
            parse("layercolor Layer 01 0.8,0.2,0.1").unwrap(),
            Command::LayerColor { layer: "Layer 01".into(), color: [0.8, 0.2, 0.1] }
        );
        assert_eq!(
            parse("layerlock Layer 01 on").unwrap(),
            Command::LayerLock { layer: "Layer 01".into(), locked: true }
        );
        assert_eq!(
            parse("layerlinetype Layer 01 dashed").unwrap(),
            Command::LayerLinetype {
                layer: "Layer 01".into(),
                linetype: itsjustcad_doc::LineType::Dashed
            }
        );
        assert!(
            (match parse("layerweight Layer 01 0.35").unwrap() {
                Command::LayerWeight { layer, mm } if layer == "Layer 01" => mm,
                _ => panic!("expected layerweight"),
            } - 0.35)
                .abs()
                < 1e-9
        );
        assert_eq!(
            parse("hideobj last 2").unwrap(),
            Command::HideObj { targets: Selector::Last { n: 2 } }
        );
        assert_eq!(
            parse("hideobj tower").unwrap(),
            Command::HideObj { targets: Selector::Named { name: "tower".into() } }
        );
        assert_eq!(parse("showobj all").unwrap(), Command::ShowObj { targets: Selector::All });
        assert!(parse("hideobj").unwrap_err().to_string().contains("selector"));
        // errors carry usage / color hints
        assert!(parse("layer").unwrap_err().to_string().contains("layer name"));
        let err = parse("layercolor walls red").unwrap_err();
        assert!(err.to_string().contains("r,g,b"), "{err}");
        assert!(parse("layercolor walls 300,0,0").is_err());
        assert!(parse("layercolor walls 1,2").is_err());
    }

    #[test]
    fn layer_command_json_roundtrip() {
        for line in [
            "layer walls",
            "tolayer last walls",
            "layercolor walls 0.5,0.5,0.5",
            "hide walls",
            "show walls",
            "hideobj last 2",
            "showobj all",
        ] {
            let cmd = parse(line).unwrap();
            let json = serde_json::to_string(&cmd).unwrap();
            let back: Command = serde_json::from_str(&json).unwrap();
            assert_eq!(cmd, back, "{line}");
        }
    }

    #[test]
    fn parse_drafting_commands() {
        assert_eq!(
            parse("dim 0,0 10,0").unwrap(),
            Command::Dim {
                id: None,
                a: DVec3::ZERO,
                b: DVec3::new(10.0, 0.0, 0.0),
                offset: DEFAULT_DIM_OFFSET,
            }
        );
        assert!(matches!(
            parse("dim 0,0 10,0 0.8").unwrap(),
            Command::Dim { offset, .. } if offset == 0.8
        ));
        assert!(matches!(
            parse("dim 0,0 10,0 80cm").unwrap(),
            Command::Dim { offset, .. } if offset == 0.8
        ));
        assert!(parse("dim 0,0").is_err());

        // text: words join, optional trailing height
        assert_eq!(
            parse("text 5,3 living room 0.3").unwrap(),
            Command::Text {
                id: None,
                pos: DVec3::new(5.0, 3.0, 0.0),
                text: "living room".into(),
                height: 0.3,
            }
        );
        assert!(matches!(
            parse("text 0,0 hello").unwrap(),
            Command::Text { ref text, height, .. }
                if text == "hello" && height == DEFAULT_TEXT_HEIGHT
        ));
        // a single numeric word is the text, not a height
        assert!(matches!(
            parse("text 0,0 42").unwrap(),
            Command::Text { ref text, height, .. }
                if text == "42" && height == DEFAULT_TEXT_HEIGHT
        ));
        assert!(parse("text 0,0").is_err());

        // hatch: default solid, explicit patterns
        use itsjustcad_doc::HatchPattern;
        assert!(matches!(
            parse("hatch last").unwrap(),
            Command::Hatch { target: Selector::Last { n: 1 }, pattern: HatchPattern::Solid, .. }
        ));
        assert!(matches!(
            parse("hatch last solid").unwrap(),
            Command::Hatch { pattern: HatchPattern::Solid, .. }
        ));
        assert!(matches!(
            parse("hatch slab lines 45 0.25").unwrap(),
            Command::Hatch {
                target: Selector::Named { .. },
                pattern: HatchPattern::Lines { angle_deg, spacing },
                ..
            } if angle_deg == 45.0 && spacing == 0.25
        ));
        assert!(matches!(
            parse("hatch last lines").unwrap(),
            Command::Hatch { pattern: HatchPattern::Lines { .. }, .. }
        ));
        assert!(parse("hatch last dots").is_err());
    }

    #[test]
    fn drafting_command_json_roundtrip() {
        for line in [
            "dim 0,0 10,0 0.8",
            "text 5,3 living room 0.3",
            "hatch last lines 45 0.25",
            "hatch last",
        ] {
            let cmd = parse(line).unwrap();
            let json = serde_json::to_string(&cmd).unwrap();
            let back: Command = serde_json::from_str(&json).unwrap();
            assert_eq!(cmd, back, "{line}");
        }
    }

    #[test]
    fn parse_sheet_commands() {
        use itsjustcad_doc::{PaperSize, ViewDirection};
        assert_eq!(
            parse("sheet plan").unwrap(),
            Command::Sheet { name: "plan".into(), paper: PaperSize::A3 }
        );
        assert_eq!(
            parse("sheet plan a1").unwrap(),
            Command::Sheet { name: "plan".into(), paper: PaperSize::A1 }
        );
        assert_eq!(
            parse("sheetview plan top 1:100").unwrap(),
            Command::SheetView {
                sheet: "plan".into(),
                direction: ViewDirection::Top,
                scale: 100.0,
            }
        );
        // bare denominator and persp alias
        assert!(matches!(
            parse("sheetview plan persp 50").unwrap(),
            Command::SheetView { direction: ViewDirection::Iso, scale, .. } if scale == 50.0
        ));
        assert!(matches!(
            parse("sheetview plan front 1:20").unwrap(),
            Command::SheetView { direction: ViewDirection::Front, scale, .. } if scale == 20.0
        ));
        assert_eq!(
            parse("print plan /tmp/plan.pdf").unwrap(),
            Command::Print { sheet: "plan".into(), path: "/tmp/plan.pdf".into() }
        );
        // errors carry hints
        assert!(parse("sheet plan b5").unwrap_err().to_string().contains("a4"));
        assert!(parse("sheetview plan back 100").unwrap_err().to_string().contains("top"));
        assert!(parse("sheetview plan top 1:0").unwrap_err().to_string().contains("1:100"));
        assert!(parse("sheet").unwrap_err().to_string().contains("name"));
        assert!(parse("print plan").unwrap_err().to_string().contains("path"));
    }

    #[test]
    fn sheet_command_json_roundtrip() {
        for line in [
            "sheet plan a1",
            "sheetview plan top 1:100",
            "sheetview plan persp 200",
            "print plan /tmp/x.pdf",
        ] {
            let cmd = parse(line).unwrap();
            let json = serde_json::to_string(&cmd).unwrap();
            let back: Command = serde_json::from_str(&json).unwrap();
            assert_eq!(cmd, back, "{line}");
        }
    }

    #[test]
    fn export_parses_and_roundtrips() {
        let cmd = parse("export /tmp/model.dxf").unwrap();
        assert_eq!(cmd, Command::Export { path: "/tmp/model.dxf".into() });
        assert!(!cmd.is_logged());
        let json = serde_json::to_string(&cmd).unwrap();
        let back: Command = serde_json::from_str(&json).unwrap();
        assert_eq!(cmd, back);
        // errors carry hints
        assert!(parse("export").unwrap_err().to_string().contains("path"));
        assert!(parse("export a b").unwrap_err().to_string().contains("path"));
    }

    #[test]
    fn import_parses_and_roundtrips() {
        let cmd = parse("import /tmp/site.dxf").unwrap();
        assert_eq!(cmd, Command::Import { path: "/tmp/site.dxf".into() });
        assert!(!cmd.is_logged());
        let json = serde_json::to_string(&cmd).unwrap();
        let back: Command = serde_json::from_str(&json).unwrap();
        assert_eq!(cmd, back);
        assert!(parse("import").unwrap_err().to_string().contains("path"));
        assert!(parse("import a b").unwrap_err().to_string().contains("path"));
    }

    #[test]
    fn parse_measure_commands() {
        assert_eq!(
            parse("distance 0,0,0 3,4,0").unwrap(),
            Command::Distance { a: DVec3::ZERO, b: DVec3::new(3.0, 4.0, 0.0) }
        );
        // dist alias and unit suffixes
        assert!(matches!(
            parse("dist 0,0 12ft,0").unwrap(),
            Command::Distance { b, .. } if (b.x - 12.0 * METERS_PER_FOOT).abs() < 1e-12
        ));
        assert!(matches!(
            parse("area last").unwrap(),
            Command::Area { targets: Selector::Last { n: 1 } }
        ));
        assert!(matches!(
            parse("volume last 2").unwrap(),
            Command::Volume { targets: Selector::Last { n: 2 } }
        ));
        assert!(matches!(
            parse("vol slab").unwrap(),
            Command::Volume { targets: Selector::Named { .. } }
        ));
        assert!(matches!(
            parse("bbox all").unwrap(),
            Command::Bbox { targets: Selector::All }
        ));
        // queries are never logged
        for line in ["distance 0,0 1,1", "area last", "volume last", "bbox all"] {
            assert!(!parse(line).unwrap().is_logged(), "{line}");
        }
        // errors carry hints
        assert!(parse("distance 0,0").unwrap_err().to_string().contains("points"));
        assert!(parse("area").unwrap_err().to_string().contains("selector"));
        assert!(parse("volume").unwrap_err().to_string().contains("selector"));
        assert!(parse("bbox").unwrap_err().to_string().contains("selector"));
    }

    #[test]
    fn measure_command_json_roundtrip() {
        for line in ["distance 0,0,0 3,4,0", "area last", "volume last 2", "bbox all"] {
            let cmd = parse(line).unwrap();
            let json = serde_json::to_string(&cmd).unwrap();
            let back: Command = serde_json::from_str(&json).unwrap();
            assert_eq!(cmd, back, "{line}");
        }
    }

    #[test]
    fn typo_suggestion() {
        let err = parse("bxo 0,0,0 1,1,1").unwrap_err();
        assert!(err.to_string().contains("box"), "{err}");
    }

    #[test]
    fn command_json_roundtrip() {
        let cmd = parse("polyline 0,0 5,0 5,5 closed").unwrap();
        let json = serde_json::to_string(&cmd).unwrap();
        let back: Command = serde_json::from_str(&json).unwrap();
        assert_eq!(cmd, back);
    }

    #[test]
    fn parse_sun_command() {
        // NY summer solstice solar noon: expected az≈180°, alt≈72.7°
        let cmd = parse("sun 40.71 -74.01 2024-06-21 16:58").unwrap();
        let Command::Sun { azimuth_deg, altitude_deg, lat_deg, lon_deg } = cmd else {
            panic!("expected Sun command");
        };
        assert!((azimuth_deg - 180.0).abs() < 0.5, "az={azimuth_deg:.2}");
        assert!((altitude_deg - 72.7).abs() < 0.5, "alt={altitude_deg:.2}");
        assert!((lat_deg - 40.71).abs() < 1e-9 && (lon_deg - (-74.01)).abs() < 1e-9);

        // sunoff parses correctly
        assert!(matches!(parse("sunoff").unwrap(), Command::SunOff));

        // bad date
        assert!(parse("sun 40.0 -74.0 2024-13-01 12:00").is_err());
        // bad time
        assert!(parse("sun 40.0 -74.0 2024-06-21 25:00").is_err());
        // wrong arg count
        assert!(parse("sun 40.0 -74.0").is_err());
    }

    #[test]
    fn sun_command_json_roundtrip() {
        let cmd = parse("sun 40.71 -74.01 2024-06-21 16:58").unwrap();
        let json = serde_json::to_string(&cmd).unwrap();
        let back: Command = serde_json::from_str(&json).unwrap();
        assert_eq!(cmd, back);
    }

    // ---- structural members ----------------------------------------------

    #[test]
    fn section_shapes_parse() {
        assert!(matches!(
            parse("section c rect 0.4 0.6").unwrap(),
            Command::DefSection { section: StructSection::Rectangular { .. }, .. }
        ));
        assert!(matches!(
            parse("section p pipe 0.2 0.01").unwrap(),
            Command::DefSection { section: StructSection::Pipe { .. }, .. }
        ));
        assert!(matches!(
            parse("section w iwf 0.31 0.2 0.013 0.008").unwrap(),
            Command::DefSection { section: StructSection::IWideFlange { .. }, .. }
        ));
        assert!(parse("section c triangle 1 1").is_err());
    }

    #[test]
    fn material_parses() {
        let Command::DefMaterial { name, elastic_modulus_e, density } =
            parse("material steel 200e9 7850").unwrap()
        else {
            panic!();
        };
        assert_eq!(name, "steel");
        assert!((elastic_modulus_e - 200e9).abs() < 1.0);
        assert!((density - 7850.0).abs() < 1e-9);
    }

    #[test]
    fn grid_parses_axes_and_levels() {
        let Command::DefGrid { name, x_axes, y_axes, levels } =
            parse("grid main x A:0 B:6 y 1:0 2:5 levels 0,3.5").unwrap()
        else {
            panic!();
        };
        assert_eq!(name, "main");
        assert_eq!(x_axes, vec![("A".to_string(), 0.0), ("B".to_string(), 6.0)]);
        assert_eq!(y_axes, vec![("1".to_string(), 0.0), ("2".to_string(), 5.0)]);
        assert_eq!(levels, vec![0.0, 3.5]);
        assert!(parse("grid empty").is_err());
    }

    #[test]
    fn story_parses() {
        assert!(matches!(
            parse("story L1 3.5").unwrap(),
            Command::DefStory { .. }
        ));
        assert!(matches!(
            parse("level L1 3.5").unwrap(),
            Command::DefStory { .. }
        ));
    }

    #[test]
    fn beam_and_column_parse_with_options() {
        let Command::FrameMember { kind, section, material, orientation_deg, .. } =
            parse("beam 0,0,3 6,0,3 W12 material steel rot 90").unwrap()
        else {
            panic!();
        };
        assert_eq!(kind, FrameKind::Beam);
        assert_eq!(section, "W12");
        assert_eq!(material.as_deref(), Some("steel"));
        assert_eq!(orientation_deg, Some(90.0));

        assert!(matches!(
            parse("column 0,0,0 0,0,3.5 c").unwrap(),
            Command::FrameMember { kind: FrameKind::Column, .. }
        ));
        assert!(parse("beam 0,0,0 c").is_err());
    }

    #[test]
    fn slab_and_wall_parse() {
        let Command::AreaMember { kind, boundary, thickness, .. } =
            parse("slab 0,0 6,0 6,4 0,4 thick 0.2").unwrap()
        else {
            panic!();
        };
        assert_eq!(kind, AreaKind::Slab);
        assert_eq!(boundary.len(), 4);
        assert!((thickness - 0.2).abs() < 1e-9);

        assert!(matches!(
            parse("wall 0,0 6,0 6,0.2 0,0.2 thick 3").unwrap(),
            Command::AreaMember { kind: AreaKind::Wall, .. }
        ));
        // too few points
        assert!(parse("slab 0,0 6,0 thick 0.2").is_err());
        // missing thickness keyword
        assert!(parse("slab 0,0 6,0 6,4 0,4").is_err());
    }

    #[test]
    fn structural_commands_json_roundtrip() {
        for line in [
            "section w iwf 0.31 0.2 0.013 0.008",
            "material steel 200e9 7850",
            "grid main x A:0 B:6 y 1:0 levels 0,3.5",
            "story L1 0",
            "beam 0,0,3 6,0,3 W12 material steel rot 45",
            "slab 0,0 6,0 6,4 0,4 thick 0.2",
        ] {
            let cmd = parse(line).unwrap();
            let json = serde_json::to_string(&cmd).unwrap();
            let back: Command = serde_json::from_str(&json).unwrap();
            assert_eq!(cmd, back, "roundtrip failed for: {line}");
        }
    }

    #[test]
    fn material2_preset_parses() {
        use itsjustcad_doc::{MaterialPreset, ObjectMaterial};
        let Command::Material2 { material, .. } = parse("material2 last glass").unwrap() else {
            panic!("expected Material2");
        };
        assert_eq!(material, ObjectMaterial::Preset { preset: MaterialPreset::Glass });

        // Each preset keyword resolves.
        for (kw, p) in [
            ("concrete", MaterialPreset::Concrete),
            ("metal", MaterialPreset::Metal),
            ("wood", MaterialPreset::Wood),
        ] {
            let Command::Material2 { material, .. } =
                parse(&format!("material2 last {kw}")).unwrap()
            else {
                panic!();
            };
            assert_eq!(material, ObjectMaterial::Preset { preset: p });
        }
    }

    #[test]
    fn material2_custom_and_off_parse() {
        use itsjustcad_doc::ObjectMaterial;
        let Command::Material2 { material, .. } =
            parse("material2 last roughness=0.9 metallic=0 color=0.6,0.6,0.6").unwrap()
        else {
            panic!("expected custom Material2");
        };
        let ObjectMaterial::Custom { color, roughness, metallic } = material else {
            panic!("expected custom variant");
        };
        assert!((roughness - 0.9).abs() < 1e-6);
        assert!((metallic - 0.0).abs() < 1e-6);
        assert!((color[0] - 0.6).abs() < 1e-6);

        assert!(matches!(parse("material2 last off").unwrap(), Command::Material2Off { .. }));
        // A bare selector with nothing else is an error.
        assert!(parse("material2 last").is_err());
        // Unknown key errors.
        assert!(parse("material2 last shininess=3").is_err());
    }

    #[test]
    fn material2_json_roundtrips() {
        for line in [
            "material2 last glass",
            "material2 all concrete",
            "material2 last roughness=0.3 metallic=1 color=200,200,210",
            "material2 last off",
        ] {
            let cmd = parse(line).unwrap();
            let json = serde_json::to_string(&cmd).unwrap();
            let back: Command = serde_json::from_str(&json).unwrap();
            assert_eq!(cmd, back, "roundtrip failed for: {line}");
        }
    }

    #[test]
    fn controlimages_parses_and_roundtrips() {
        let cmd = parse("controlimages /tmp/scene").unwrap();
        assert_eq!(cmd, Command::ControlImages { prefix: "/tmp/scene".into() });
        let json = serde_json::to_string(&cmd).unwrap();
        let back: Command = serde_json::from_str(&json).unwrap();
        assert_eq!(cmd, back);
        assert!(parse("controlimages").unwrap_err().to_string().contains("prefix"));
    }
}
