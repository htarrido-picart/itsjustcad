//! Precise numeric input while drawing (Rhino habit): "5.2,3" absolute,
//! "@2,3" relative to the last picked point, bare "5" a distance from the
//! last point along the current cursor direction. Pure functions — the app
//! layer owns the buffer and feeds resolved points to the draw tool.

use glam::DVec3;

/// One parsed line from the drawing input buffer.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PointInput {
    /// "x,y" or "x,y,z" — absolute document coordinates.
    Absolute(DVec3),
    /// "@dx,dy" or "@dx,dy,dz" — offset from the last picked point.
    Relative(DVec3),
    /// "5" — distance from the last point along the cursor direction.
    Distance(f64),
}

/// Characters the drawing input buffer accepts (everything parse can use).
pub fn accepts_char(c: char) -> bool {
    c.is_ascii_digit() || matches!(c, '.' | ',' | '@' | '-' | '+')
}

fn coords(s: &str, what: &str) -> Result<DVec3, String> {
    let parts: Vec<&str> = s.split(',').collect();
    if parts.len() < 2 || parts.len() > 3 {
        return Err(format!("{what}: expected x,y or x,y,z — got '{s}'"));
    }
    let mut v = [0.0f64; 3];
    for (i, p) in parts.iter().enumerate() {
        v[i] = p
            .trim()
            .parse::<f64>()
            .map_err(|_| format!("{what}: '{p}' is not a number"))?;
    }
    Ok(DVec3::new(v[0], v[1], v[2]))
}

/// Parse one buffer line. Empty input is the caller's case (fall through to
/// the tool's own Enter handling), so it errors here.
pub fn parse(input: &str) -> Result<PointInput, String> {
    let s = input.trim();
    if s.is_empty() {
        return Err("empty input".into());
    }
    if let Some(rest) = s.strip_prefix('@') {
        return coords(rest, "relative point").map(PointInput::Relative);
    }
    if s.contains(',') {
        return coords(s, "point").map(PointInput::Absolute);
    }
    let d = s
        .parse::<f64>()
        .map_err(|_| format!("'{s}' is not a number or point (try 5.2,3 or @2,3)"))?;
    Ok(PointInput::Distance(d))
}

/// Turn a parsed input into a world point. `last` is the previous picked
/// point, `cursor` the current (already snap/ortho-resolved) cursor position
/// which supplies the direction for bare distances.
pub fn resolve(
    input: PointInput,
    last: Option<DVec3>,
    cursor: Option<DVec3>,
) -> Result<DVec3, String> {
    match input {
        PointInput::Absolute(p) => Ok(p),
        PointInput::Relative(d) => {
            let last = last.ok_or("relative input (@) needs a previous point")?;
            Ok(last + d)
        }
        PointInput::Distance(d) => {
            let last = last.ok_or("a bare distance needs a previous point")?;
            let cursor = cursor.ok_or("move the cursor to set a direction")?;
            let dir = cursor - last;
            if dir.length() < 1e-9 {
                return Err("move the cursor away from the last point to set a direction".into());
            }
            Ok(last + dir.normalize() * d)
        }
    }
}

/// Parse + resolve in one step (what the app calls on Enter).
pub fn resolve_input(
    input: &str,
    last: Option<DVec3>,
    cursor: Option<DVec3>,
) -> Result<DVec3, String> {
    resolve(parse(input)?, last, cursor)
}

/// Shift ortho lock: constrain `cursor` to 0°/90° from `last` in the drawing
/// plane — the dominant axis keeps its cursor coordinate, the other snaps
/// back to the last point (ties go horizontal, matching Rhino).
pub fn ortho_lock(last: DVec3, cursor: DVec3) -> DVec3 {
    let d = cursor - last;
    if d.x.abs() >= d.y.abs() {
        DVec3::new(cursor.x, last.y, last.z)
    } else {
        DVec3::new(last.x, cursor.y, last.z)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_absolute_2d_and_3d() {
        assert_eq!(
            parse("5.2,3").unwrap(),
            PointInput::Absolute(DVec3::new(5.2, 3.0, 0.0))
        );
        assert_eq!(
            parse(" 1,-2.5,4 ").unwrap(),
            PointInput::Absolute(DVec3::new(1.0, -2.5, 4.0))
        );
    }

    #[test]
    fn parse_relative() {
        assert_eq!(
            parse("@2,3").unwrap(),
            PointInput::Relative(DVec3::new(2.0, 3.0, 0.0))
        );
        assert_eq!(
            parse("@-2,0,1.5").unwrap(),
            PointInput::Relative(DVec3::new(-2.0, 0.0, 1.5))
        );
    }

    #[test]
    fn parse_bare_distance() {
        assert_eq!(parse("5").unwrap(), PointInput::Distance(5.0));
        assert_eq!(parse("-3.25").unwrap(), PointInput::Distance(-3.25));
    }

    #[test]
    fn parse_errors_are_friendly() {
        assert!(parse("").is_err());
        assert!(parse("abc").unwrap_err().contains("abc"));
        assert!(parse("1,2,3,4").unwrap_err().contains("x,y"));
        assert!(parse("1,foo").unwrap_err().contains("foo"));
        assert!(parse("@5").is_err(), "@ needs coords, not a distance");
    }

    #[test]
    fn resolve_absolute_ignores_context() {
        let p = resolve(PointInput::Absolute(DVec3::new(1.0, 2.0, 0.0)), None, None).unwrap();
        assert_eq!(p, DVec3::new(1.0, 2.0, 0.0));
    }

    #[test]
    fn resolve_relative_offsets_last() {
        let last = DVec3::new(10.0, 5.0, 0.0);
        let p = resolve(PointInput::Relative(DVec3::new(2.0, -3.0, 0.0)), Some(last), None)
            .unwrap();
        assert_eq!(p, DVec3::new(12.0, 2.0, 0.0));
        assert!(resolve(PointInput::Relative(DVec3::ZERO), None, None)
            .unwrap_err()
            .contains("previous point"));
    }

    #[test]
    fn resolve_distance_along_cursor_direction() {
        let last = DVec3::new(1.0, 1.0, 0.0);
        let cursor = DVec3::new(9.0, 1.0, 0.0); // +X direction
        let p = resolve(PointInput::Distance(5.0), Some(last), Some(cursor)).unwrap();
        assert!((p - DVec3::new(6.0, 1.0, 0.0)).length() < 1e-12);
        // negative distance goes backward
        let q = resolve(PointInput::Distance(-2.0), Some(last), Some(cursor)).unwrap();
        assert!((q - DVec3::new(-1.0, 1.0, 0.0)).length() < 1e-12);
    }

    #[test]
    fn resolve_distance_needs_last_and_direction() {
        let last = DVec3::new(1.0, 1.0, 0.0);
        assert!(resolve(PointInput::Distance(5.0), None, None).is_err());
        assert!(resolve(PointInput::Distance(5.0), Some(last), None).is_err());
        // cursor sitting on the last point: no direction
        assert!(resolve(PointInput::Distance(5.0), Some(last), Some(last)).is_err());
    }

    #[test]
    fn resolve_input_end_to_end() {
        let last = Some(DVec3::new(0.0, 0.0, 0.0));
        let cursor = Some(DVec3::new(0.0, 7.0, 0.0)); // +Y
        assert_eq!(
            resolve_input("5.2,3", None, None).unwrap(),
            DVec3::new(5.2, 3.0, 0.0)
        );
        assert_eq!(
            resolve_input("@2,3", last, None).unwrap(),
            DVec3::new(2.0, 3.0, 0.0)
        );
        let p = resolve_input("4", last, cursor).unwrap();
        assert!((p - DVec3::new(0.0, 4.0, 0.0)).length() < 1e-12);
        assert!(resolve_input("nonsense", last, cursor).is_err());
    }

    #[test]
    fn ortho_snaps_to_dominant_axis() {
        let last = DVec3::new(2.0, 2.0, 0.0);
        // mostly horizontal -> y locks to last
        assert_eq!(
            ortho_lock(last, DVec3::new(7.0, 3.0, 0.0)),
            DVec3::new(7.0, 2.0, 0.0)
        );
        // mostly vertical -> x locks to last
        assert_eq!(
            ortho_lock(last, DVec3::new(2.5, 8.0, 0.0)),
            DVec3::new(2.0, 8.0, 0.0)
        );
        // exact tie goes horizontal
        assert_eq!(
            ortho_lock(last, DVec3::new(5.0, 5.0, 0.0)),
            DVec3::new(5.0, 2.0, 0.0)
        );
        // z always flattens to the last point's plane
        assert_eq!(
            ortho_lock(DVec3::new(0.0, 0.0, 1.0), DVec3::new(4.0, 1.0, 3.0)),
            DVec3::new(4.0, 0.0, 1.0)
        );
    }

    #[test]
    fn accepts_only_numeric_chars() {
        for c in "0123456789.,@-+".chars() {
            assert!(accepts_char(c), "{c} should be accepted");
        }
        for c in "abz L?*/ ".chars() {
            assert!(!accepts_char(c), "{c} should be rejected");
        }
    }
}
