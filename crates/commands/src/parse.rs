use glam::DVec3;
use mydrafter_doc::{HatchPattern, PaperSize, ViewDirection};

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
            let [path] = take::<1>("export", "an output path (.dxf)", &args)?;
            Ok(Command::Export { path: path.to_string() })
        }
        "select" => {
            let (sel, rest) = selector(&args, "select")?;
            expect_empty("select", rest, &args)?;
            Ok(Command::Select { targets: sel })
        }
        "selectnone" | "deselect" => Ok(Command::SelectNone),
        "undo" => Ok(Command::Undo),
        "redo" => Ok(Command::Redo),
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

/// Numbers accept unit suffixes; bare numbers are meters.
pub fn number(s: &str) -> Result<f64, ParseError> {
    let (num, factor) = if let Some(v) = s.strip_suffix("mm") {
        (v, 0.001)
    } else if let Some(v) = s.strip_suffix("cm") {
        (v, 0.01)
    } else if let Some(v) = s.strip_suffix('m') {
        (v, 1.0)
    } else {
        (s, 1.0)
    };
    num.parse::<f64>()
        .map(|v| v * factor)
        .map_err(|_| ParseError::BadNumber(s.to_string()))
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
    fn parse_units() {
        assert_eq!(number("250cm").unwrap(), 2.5);
        assert_eq!(number("500mm").unwrap(), 0.5);
        assert_eq!(number("3m").unwrap(), 3.0);
        assert_eq!(number("4.5").unwrap(), 4.5);
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
