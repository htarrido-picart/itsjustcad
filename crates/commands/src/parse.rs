use glam::DVec3;
use mydrafter_doc::{
    HatchPattern, PaperSize, Units, ViewDirection, METERS_PER_FOOT, METERS_PER_INCH,
};

use crate::error::ParseError;
use crate::registry::registry;
use crate::{Command, MirrorPlane, Selector};

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
                _ => {
                    return wrong(
                        "hatch",
                        "an optional pattern: solid, lines, or lines <angle> <spacing>",
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
        "section" => {
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
            let [name] = take::<1>("layer", "a layer name", &args)?;
            Ok(Command::Layer { name: name.to_string() })
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
            let [layer, c] = take::<2>("layercolor", "a layer name and an r,g,b color", &args)?;
            Ok(Command::LayerColor {
                layer: layer.to_string(),
                color: color3(c)?,
            })
        }
        "hide" => {
            let [layer] = take::<1>("hide", "a layer name", &args)?;
            Ok(Command::Hide { layer: layer.to_string() })
        }
        "show" => {
            let [layer] = take::<1>("show", "a layer name", &args)?;
            Ok(Command::Show { layer: layer.to_string() })
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
        "units" => {
            let [u] = take::<1>("units", "a unit: m, cm, mm, ft, in or ftin", &args)?;
            let units = Units::parse(u)
                .ok_or_else(|| wrong_err("units", "one of m, cm, mm, ft, in, ftin", &args))?;
            Ok(Command::Units { units })
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
            let [path] = take::<1>("export", "an output path (.dxf/.stl/.obj/.gltf/.glb)", &args)?;
            Ok(Command::Export { path: path.to_string() })
        }
        "import" => {
            let [path] = take::<1>("import", "an input path (.dxf)", &args)?;
            Ok(Command::Import { path: path.to_string() })
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

fn take<'a, const N: usize>(
    command: &'static str,
    expected: &'static str,
    args: &[&'a str],
) -> Result<[&'a str; N], ParseError> {
    <[&str; N]>::try_from(args.to_vec()).map_err(|_| wrong_err(command, expected, args))
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
        // errors carry hints
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
    fn section_command_json_roundtrip() {
        for line in ["section all 0,0,1.5 0,0,1", "section last 2 5,0,0 1,0,0", "plan 1.2"] {
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
        use mydrafter_doc::HatchPattern;
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
        use mydrafter_doc::{PaperSize, ViewDirection};
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
}
