// SPDX-License-Identifier: AGPL-3.0-only
// Copyright © 2026 Hector Tarrido-Picart

//! Object snaps, grid and construction plane for host draw tools (#165).
//!
//! Commands take construction-plane coordinates: a host draw tool snaps the
//! cursor with [`ijc_osnap`] and sends the returned `cplane` coordinates (or
//! uses [`ijc_screen_to_cplane`] / [`ijc_cplane_json`] to convert itself).

use std::ffi::c_char;

use glam::{DVec3, Vec2};
use itsjustcad_commands::osnap::{self, SnapKind, SnapSettings};
use itsjustcad_doc::{CPlane, Document};

use super::{
    AppHandle, apply_pending, c_str, guard_ffi, handle_mut, handle_ref, into_c_string, ray_plane,
    screen_ray, viewport_size,
};

/// Default grid spacing in model units (metres): the desktop's 10 cm grid.
pub(crate) const DEFAULT_GRID: f64 = 0.1;

/// A snap kind by its short key (`end`, `int`, `mid`, `cen`, `qua`, `perp`,
/// `tan`, `nod`, `vtx`, `near`) or full name (`endpoint`, `intersection`,
/// `midpoint`, `center`, `quadrant`, `perpendicular`, `tangent`, `node`,
/// `vertex`, `nearest`).
fn kind_by_name(name: &str) -> Option<SnapKind> {
    let key = match name.to_ascii_lowercase().as_str() {
        "endpoint" => "end",
        "intersection" => "int",
        "midpoint" => "mid",
        "center" | "centre" => "cen",
        "quadrant" => "qua",
        "perpendicular" => "perp",
        "tangent" => "tan",
        "node" => "nod",
        "vertex" => "vtx",
        "nearest" => "near",
        other => return SnapKind::from_key(other),
    };
    SnapKind::from_key(key)
}

/// Parsed `modes_json` of [`ijc_osnap`].
struct Modes {
    settings: SnapSettings,
    last: Option<DVec3>,
    radius_px: f32,
}

fn parse_modes(json: Option<&str>) -> Result<Modes, String> {
    let mut m = Modes {
        settings: SnapSettings::default(),
        last: None,
        radius_px: osnap::SNAP_RADIUS_PX,
    };
    let v: serde_json::Value = match json {
        None => return Ok(m),
        Some(s) => serde_json::from_str(s).map_err(|e| format!("modes: {e}"))?,
    };
    let kinds = match &v {
        serde_json::Value::Null => return Ok(m),
        serde_json::Value::Array(_) => Some(&v),
        serde_json::Value::Object(o) => {
            if let Some(g) = o.get("grid") {
                m.settings.grid = g.as_bool().ok_or("grid: expected true or false")?;
            }
            if let Some(l) = o.get("last").filter(|l| !l.is_null()) {
                let p: [f64; 3] = serde_json::from_value(l.clone())
                    .map_err(|_| "last: expected [x,y,z]".to_string())?;
                m.last = Some(DVec3::from_array(p));
            }
            if let Some(r) = o.get("radius_px") {
                m.radius_px =
                    r.as_f64()
                        .filter(|r| r.is_finite() && *r > 0.0)
                        .ok_or("radius_px: expected a number > 0")? as f32;
            }
            o.get("kinds")
        }
        _ => return Err("modes: expected an array of kinds or an object".into()),
    };
    if let Some(kinds) = kinds {
        let kinds = kinds.as_array().ok_or("kinds: expected an array")?;
        for k in SnapKind::ALL {
            m.settings.set(k, false);
        }
        for k in kinds {
            let name = k.as_str().ok_or("kinds: expected names")?;
            let kind = kind_by_name(name).ok_or(format!("unknown snap kind '{name}'"))?;
            m.settings.set(kind, true);
        }
    }
    Ok(m)
}

/// World point → screen pixels (y down), `None` behind the camera.
fn project(vp: glam::Mat4, w: f32, h: f32, p: DVec3) -> Option<Vec2> {
    let clip = vp * glam::Vec4::new(p.x as f32, p.y as f32, p.z as f32, 1.0);
    if clip.w.is_nan() || clip.w <= 0.0 {
        return None;
    }
    let ndc = clip.truncate() / clip.w;
    let s = Vec2::new((ndc.x + 1.0) * 0.5 * w, (1.0 - ndc.y) * 0.5 * h);
    s.is_finite().then_some(s)
}

/// Snap a world point on `plane` to the grid, in plane coordinates.
fn grid_snap(plane: &CPlane, world: DVec3, step: f64) -> DVec3 {
    let l = plane.to_cplane(world);
    let r = |v: f64| (v / step).round() * step;
    plane.to_world(DVec3::new(r(l.x), r(l.y), 0.0))
}

/// The snap at a cursor: the winning object snap, else the cplane point
/// (grid-snapped when on). `(point, kind, objects)`; `None` when the ray
/// misses the plane and no object snap hit.
fn snap_at(
    doc: &Document,
    vp: glam::Mat4,
    (w, h): (f32, f32),
    cursor: Vec2,
    modes: &Modes,
    grid: f64,
) -> Option<(DVec3, &'static str, Vec<String>)> {
    let keep = |bb: kernel_mesh::Aabb| {
        let mut lo = Vec2::splat(f32::INFINITY);
        let mut hi = Vec2::splat(f32::NEG_INFINITY);
        for i in 0..8 {
            let c = DVec3::new(
                if i & 1 == 0 { bb.min.x } else { bb.max.x },
                if i & 2 == 0 { bb.min.y } else { bb.max.y },
                if i & 4 == 0 { bb.min.z } else { bb.max.z },
            );
            let Some(s) = project(vp, w, h, c) else {
                return true; // straddles the eye: keep
            };
            lo = lo.min(s);
            hi = hi.max(s);
        }
        let r = modes.radius_px;
        cursor.cmpge(lo - r).all() && cursor.cmple(hi + r).all()
    };
    let cands = osnap::candidates_tagged(doc, &modes.settings, modes.last, keep);
    let plain: Vec<(DVec3, SnapKind)> = cands.iter().map(|c| (c.point, c.kind)).collect();
    let hit = osnap::resolve_index(&plain, cursor, modes.radius_px, &modes.settings, |p| {
        project(vp, w, h, p)
    });
    if let Some(i) = hit {
        let c = &cands[i];
        let ids = c
            .objects
            .iter()
            .flatten()
            .map(|id| id.to_string())
            .collect();
        return Some((c.point, c.kind.key(), ids));
    }
    let (origin, dir) = screen_ray(vp, w, h, cursor.x, cursor.y)?;
    let p = origin + dir * ray_plane(&doc.cplane, origin, dir)?;
    if modes.settings.grid {
        Some((grid_snap(&doc.cplane, p, grid), "grid", Vec::new()))
    } else {
        Some((p, "plane", Vec::new()))
    }
}

/// Snap the cursor `(x_px, y_px)` (physical drawable pixels, as
/// [`super::ijc_pick`]) with the core's object snaps, through the active
/// camera. `modes_json` may be null (defaults: endpoint, intersection,
/// midpoint, center; grid on), an array of kinds, or an object
/// `{"kinds":[…], "grid":bool, "last":[x,y,z], "radius_px":n}`. Kinds:
/// `end`/`endpoint`, `int`/`intersection`, `mid`/`midpoint`, `cen`/`center`,
/// `qua`/`quadrant`, `perp`/`perpendicular`, `tan`/`tangent`, `nod`/`node`,
/// `vtx`/`vertex`, `near`/`nearest`. `last` is the previous placed point
/// (world), needed by perpendicular and tangent. `radius_px` defaults to 10;
/// pass it in physical pixels (10 × the screen scale).
///
/// Returns `{"point":[x,y,z],"cplane":[u,v,w],"kind","object","objects":[…]}`:
/// `point` in world coordinates, `cplane` the same point in the active
/// construction plane (what commands take); `kind` is the snap key above, or
/// `"grid"` (on the cplane, rounded to [`ijc_set_grid_spacing`] in plane
/// coordinates) or `"plane"` (on the cplane, grid off); `object` the short id
/// of the snapped object (`null` for grid/plane) and `objects` every object
/// it comes from (two for an intersection). `{"point":null,…}` when the ray
/// misses the plane and nothing snaps; `{"error":"…"}` for bad `modes_json`
/// or no viewport (headless: set one with [`super::ijc_resize`]); `null` for a
/// null/invalid/busy handle. Queued ops are applied first. Free with
/// [`super::ijc_string_free`].
///
/// # Safety
/// `h` must be null or a live handle; `modes_json` must be null or a valid
/// NUL-terminated C string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ijc_osnap(
    h: *mut AppHandle,
    x_px: f32,
    y_px: f32,
    modes_json: *const c_char,
) -> *mut c_char {
    guard_ffi(std::ptr::null_mut(), || {
        let err = |e: &str| into_c_string(serde_json::json!({ "error": e }).to_string());
        let Some(mut guard) = (unsafe { handle_mut(h) }) else {
            return into_c_string(String::from("null"));
        };
        let app: &mut AppHandle = &mut guard;
        let json = if modes_json.is_null() {
            None
        } else {
            match unsafe { c_str(modes_json) } {
                Some(s) => Some(s),
                None => return err("modes_json is not valid UTF-8"),
            }
        };
        let modes = match parse_modes(json) {
            Ok(m) => m,
            Err(e) => return err(&e),
        };
        apply_pending(app);
        let Some(size) = viewport_size(app) else {
            return err("no viewport");
        };
        let vp = app.camera.view_proj(size.0 / size.1);
        let doc = &app.session.doc;
        let cursor = Vec2::new(x_px, y_px);
        let v3 = |p: DVec3| [p.x, p.y, p.z];
        let out = match snap_at(doc, vp, size, cursor, &modes, app.grid_spacing) {
            Some((p, kind, objects)) => serde_json::json!({
                "point": v3(p),
                "cplane": v3(doc.cplane.to_cplane(p)),
                "kind": kind,
                "object": objects.first(),
                "objects": objects,
            }),
            None => serde_json::json!({
                "point": null, "cplane": null, "kind": null, "object": null, "objects": [],
            }),
        };
        into_c_string(out.to_string())
    })
}

/// Set the grid spacing [`ijc_osnap`] rounds to, in model units (metres;
/// default 0.1). False for a non-finite or non-positive spacing, or a
/// null/invalid/busy handle.
///
/// # Safety
/// `h` must be null or a live handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ijc_set_grid_spacing(h: *mut AppHandle, spacing: f64) -> bool {
    guard_ffi(false, || {
        if !spacing.is_finite() || spacing <= 0.0 {
            return false;
        }
        let Some(mut guard) = (unsafe { handle_mut(h) }) else {
            return false;
        };
        guard.grid_spacing = spacing;
        true
    })
}

/// The active construction plane:
/// `{"origin":[x,y,z],"x_axis":[…],"y_axis":[…],"normal":[…],"is_world":bool,
/// "grid_spacing":n}` in world coordinates (axes are unit vectors). A point
/// `(u,v,w)` in plane coordinates is `origin + u·x_axis + v·y_axis +
/// w·normal`; commands take plane coordinates. `null` for a null/invalid
/// handle. Free with [`super::ijc_string_free`].
///
/// # Safety
/// `h` must be null or a live handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ijc_cplane_json(h: *mut AppHandle) -> *mut c_char {
    guard_ffi(std::ptr::null_mut(), || {
        let Some(app) = (unsafe { handle_ref(h) }) else {
            return into_c_string(String::from("null"));
        };
        let p = &app.session.doc.cplane;
        let v3 = |v: DVec3| [v.x, v.y, v.z];
        into_c_string(
            serde_json::json!({
                "origin": v3(p.origin),
                "x_axis": v3(p.x_axis),
                "y_axis": v3(p.y_axis),
                "normal": v3(p.normal),
                "is_world": p.is_world(),
                "grid_spacing": app.grid_spacing,
            })
            .to_string(),
        )
    })
}

/// Cast a ray from `(x_px, y_px)` (physical drawable pixels) through the
/// active camera onto the construction plane, ignoring objects, and write
/// the hit in PLANE coordinates `(u, v, 0)` to `out_uvw` (3 `f64`) — what
/// commands take. With `world_xyz` non-null, also writes the world point
/// there. False, writing nothing, when the ray misses the plane, there is no
/// viewport (headless: [`super::ijc_resize`] first), a null `out_uvw`, or a
/// null/invalid handle.
///
/// # Safety
/// `h` must be null or a live handle; `out_uvw` (and `world_xyz` if
/// non-null) must be valid for writes of 3 `f64`s.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ijc_screen_to_cplane(
    h: *mut AppHandle,
    x_px: f32,
    y_px: f32,
    out_uvw: *mut f64,
    world_xyz: *mut f64,
) -> bool {
    guard_ffi(false, || {
        if out_uvw.is_null() {
            return false;
        }
        let Some(app) = (unsafe { handle_ref(h) }) else {
            return false;
        };
        let Some((w, h_px)) = viewport_size(app) else {
            return false;
        };
        let vp = app.camera.view_proj(w / h_px);
        let Some((origin, dir)) = screen_ray(vp, w, h_px, x_px, y_px) else {
            return false;
        };
        let plane = &app.session.doc.cplane;
        let Some(t) = ray_plane(plane, origin, dir) else {
            return false;
        };
        let p = origin + dir * t;
        let mut l = plane.to_cplane(p);
        l.z = 0.0;
        unsafe { std::slice::from_raw_parts_mut(out_uvw, 3) }.copy_from_slice(&l.to_array());
        if !world_xyz.is_null() {
            unsafe { std::slice::from_raw_parts_mut(world_xyz, 3) }.copy_from_slice(&p.to_array());
        }
        true
    })
}

#[cfg(test)]
mod tests {
    use super::super::*;
    use super::*;
    use std::ffi::{CStr, CString};

    fn c(s: &str) -> CString {
        CString::new(s).unwrap()
    }

    unsafe fn r(p: *mut c_char) -> serde_json::Value {
        let s = unsafe { CStr::from_ptr(p) }.to_str().unwrap().to_owned();
        unsafe { ijc_string_free(p) };
        serde_json::from_str(&s).unwrap()
    }

    fn ok(h: *mut AppHandle, line: &str) -> serde_json::Value {
        let v = unsafe { r(ijc_run_command_result(h, c(line).as_ptr())) };
        assert_eq!(v["ok"], true, "{line}: {v}");
        v
    }

    /// Screen pixel of a world point through the handle's active camera.
    fn px(h: *mut AppHandle, p: [f64; 3]) -> (f32, f32) {
        let mut out = [0f32; 2];
        let app = unsafe { handle_ref(h) }.unwrap();
        let (w, hh) = viewport_size(app).unwrap();
        let s = project(app.camera.view_proj(w / hh), w, hh, DVec3::from_array(p)).unwrap();
        out.copy_from_slice(&[s.x, s.y]);
        (out[0], out[1])
    }

    fn close(v: &serde_json::Value, p: [f64; 3]) -> bool {
        (0..3).all(|i| (v[i].as_f64().unwrap() - p[i]).abs() < 1e-6)
    }

    #[test]
    fn osnap_endpoint_midpoint_center_intersection_perpendicular() {
        let h = ijc_init_headless();
        unsafe {
            ijc_resize(h, 800, 600);
            let a = ok(h, "line 0,0,0 10,0,0")["created"][0]
                .as_str()
                .unwrap()
                .to_string();
            let b = ok(h, "line 5,-5,0 5,5,0")["created"][0]
                .as_str()
                .unwrap()
                .to_string();
            let circ = ok(h, "circle 20,0,0 3")["created"][0]
                .as_str()
                .unwrap()
                .to_string();
            ok(h, "top");
            ok(h, "ze");
            assert!(ijc_flush(h));

            let snap = |p: [f64; 3], modes: &str| {
                let (x, y) = px(h, p);
                r(ijc_osnap(h, x + 2.0, y + 1.0, c(modes).as_ptr()))
            };
            let v = snap([10.0, 0.0, 0.0], r#"["endpoint"]"#);
            assert_eq!(v["kind"], "end", "{v}");
            assert!(close(&v["point"], [10.0, 0.0, 0.0]), "{v}");
            assert_eq!(v["object"], a.as_str());
            assert!(close(&v["cplane"], [10.0, 0.0, 0.0]), "world plane: {v}");

            let v = snap([5.0, 5.0, 0.0], r#"["mid","end"]"#);
            assert_eq!(v["kind"], "end", "{v}");
            assert_eq!(v["object"], b.as_str());
            let v = snap([5.0, 0.0, 0.0], r#"["midpoint"]"#);
            assert_eq!(v["kind"], "mid", "{v}");
            assert!(close(&v["point"], [5.0, 0.0, 0.0]), "{v}");

            let v = snap([20.0, 0.0, 0.0], r#"["center"]"#);
            assert_eq!(v["kind"], "cen", "{v}");
            assert_eq!(v["object"], circ.as_str());

            let v = snap([5.0, 0.0, 0.0], r#"["intersection"]"#);
            assert_eq!(v["kind"], "int", "{v}");
            assert!(close(&v["point"], [5.0, 0.0, 0.0]));
            assert_eq!(v["objects"].as_array().unwrap().len(), 2, "{v}");

            let modes = r#"{"kinds":["perpendicular"],"last":[2,3,0]}"#;
            let v = snap([2.0, 0.0, 0.0], modes);
            assert_eq!(v["kind"], "perp", "{v}");

            // Empty space: the grid on the cplane, settable spacing.
            let v = snap([3.3, 2.2, 0.0], "[]");
            assert_eq!(v["kind"], "grid", "{v}");
            assert!(ijc_set_grid_spacing(h, 1.0));
            assert!(!ijc_set_grid_spacing(h, 0.0));
            assert!(!ijc_set_grid_spacing(h, f64::NAN));
            let v = snap([3.3, 2.2, 0.0], "[]");
            assert!(close(&v["point"], [3.0, 2.0, 0.0]), "{v}");
            assert_eq!(v["object"], serde_json::Value::Null);
            let v = snap([3.3, 2.2, 0.0], r#"{"kinds":[],"grid":false}"#);
            assert_eq!(v["kind"], "plane", "{v}");

            // Null modes = defaults; bad modes are reported.
            let (x, y) = px(h, [10.0, 0.0, 0.0]);
            assert_eq!(r(ijc_osnap(h, x, y, std::ptr::null()))["kind"], "end");
            for bad in [r#"["warp"]"#, "7", r#"{"radius_px":-1}"#, "nope"] {
                assert!(
                    r(ijc_osnap(h, x, y, c(bad).as_ptr()))["error"].is_string(),
                    "{bad}"
                );
            }
            assert_eq!(
                r(ijc_osnap(std::ptr::null_mut(), x, y, std::ptr::null())),
                serde_json::Value::Null
            );
            ijc_free(h);

            let h = ijc_init_headless();
            assert!(
                r(ijc_osnap(h, 1.0, 1.0, std::ptr::null()))["error"].is_string(),
                "no viewport"
            );
            ijc_free(h);
        }
    }

    #[test]
    fn cplane_json_and_screen_to_cplane_follow_the_plane() {
        let h = ijc_init_headless();
        unsafe {
            let v = r(ijc_cplane_json(h));
            assert_eq!(v["is_world"], true);
            assert_eq!(v["normal"], serde_json::json!([0.0, 0.0, 1.0]));
            assert_eq!(v["grid_spacing"], DEFAULT_GRID);

            ijc_resize(h, 800, 600);
            ok(h, "top");
            assert!(ijc_flush(h));
            let mut uvw = [0f64; 3];
            let mut xyz = [0f64; 3];
            assert!(!ijc_screen_to_cplane(
                h,
                400.0,
                300.0,
                std::ptr::null_mut(),
                xyz.as_mut_ptr()
            ));
            assert!(ijc_screen_to_cplane(
                h,
                400.0,
                300.0,
                uvw.as_mut_ptr(),
                xyz.as_mut_ptr()
            ));
            assert_eq!(uvw[0..2], xyz[0..2], "world plane: same coords");

            // Shift the plane: plane coords differ from world by its origin.
            ok(h, "cplane origin 5,7,0 normal 0,0,1");
            let v = r(ijc_cplane_json(h));
            assert_eq!(v["origin"], serde_json::json!([5.0, 7.0, 0.0]), "{v}");
            assert_eq!(v["is_world"], false);
            assert!(ijc_screen_to_cplane(
                h,
                400.0,
                300.0,
                uvw.as_mut_ptr(),
                xyz.as_mut_ptr()
            ));
            assert!((uvw[0] - (xyz[0] - 5.0)).abs() < 1e-6, "{uvw:?} {xyz:?}");
            assert!((uvw[1] - (xyz[1] - 7.0)).abs() < 1e-6);
            assert_eq!(uvw[2], 0.0);
            assert!(ijc_screen_to_cplane(
                h,
                400.0,
                300.0,
                uvw.as_mut_ptr(),
                std::ptr::null_mut()
            ));

            let fresh = ijc_init_headless();
            assert!(!ijc_screen_to_cplane(
                fresh,
                1.0,
                1.0,
                uvw.as_mut_ptr(),
                std::ptr::null_mut()
            ));
            ijc_free(fresh);
            assert_eq!(
                r(ijc_cplane_json(std::ptr::null_mut())),
                serde_json::Value::Null
            );
            ijc_free(h);
        }
    }
}
