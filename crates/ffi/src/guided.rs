// SPDX-License-Identifier: AGPL-3.0-only
// Copyright © 2026 Hector Tarrido-Picart

//! Guided verbs over the FFI (itsjustcad-ios#121): the desktop's guided step
//! engine ([`itsjustcad_guided`]) driven by a touch/pointer host.
//!
//! A host starts a run with [`ijc_guided_start`] (`"offset"`), reads the current
//! step from [`ijc_guided_state_json`], renders a native control for its `kind`,
//! and feeds answers back (`ijc_guided_answer_*`, or [`ijc_guided_pick`] for a
//! viewport tap). When the last step is answered the engine emits the canonical
//! command string — the same one typing the full command produces — and it runs
//! through the normal [`super::ijc_run_command_result`] path: one command, one
//! undo step. [`ijc_guided_preview_json`] returns ghost polylines (rubber band
//! plus, for offset/fillet/chamfer/array/polararray, the outcome computed in a
//! scratch copy of the document) for the host to draw over the viewport, like
//! the gumball's amber ghost.
//!
//! Every answer call returns the same JSON envelope:
//! `{"status":"more"|"error"|"done"|"idle", "error":str|null, "command":str|null,
//!   "run":{ok,message,error,created}|null, "state":<state json>}`.

use std::ffi::{CStr, c_char};

use glam::{DVec3, Vec2};
use itsjustcad_commands::Session;
use itsjustcad_doc::{Document, Geometry, ObjectId};
use itsjustcad_guided::{GuidedTool, ObjFilter, StartResult, Step, StepResult};
use kernel_curve::Curve;
use serde_json::{Value, json};

use super::{
    AppHandle, PendingOp, apply_camera, apply_pending, boxsel, check_line, guard_ffi, handle_mut,
    handle_ref, into_c_string, object_hit, object_wireframe_world, ray_plane, run_cmd_json,
    run_result_json, screen_ray, viewport_size,
};

/// Verbs whose outcome [`ijc_guided_preview_json`] computes in a scratch
/// document. All pure geometry edits (no files, no network).
const RESULT_PREVIEW_VERBS: &[&str] = &["offset", "fillet", "chamfer", "array", "polararray"];

/// Grid fallback for taps in empty space (the desktop's 10 cm grid snap).
fn grid_snap(p: DVec3) -> DVec3 {
    (p * 10.0).round() / 10.0
}

/// Per-handle guided run: the engine plus the last result preview.
#[derive(Default)]
pub(crate) struct GuidedState {
    tool: GuidedTool,
    /// Last result-preview command and its outcome segments (re-used while the
    /// finger moves without changing the command).
    cached: Option<(String, Vec<Vec<DVec3>>)>,
}

impl GuidedState {
    fn clear_preview(&mut self) {
        self.cached = None;
    }
}

fn v3(p: DVec3) -> Value {
    json!([p.x, p.y, p.z])
}

/// The current step as JSON (see [`ijc_guided_state_json`]).
fn state_value(tool: &GuidedTool) -> Value {
    let (Some(verb), Some(step)) = (tool.active_verb(), tool.current()) else {
        return json!({ "active": false });
    };
    let mut v = json!({
        "active": true,
        "verb": verb,
        "step": tool.step_index(),
        "steps": tool.step_count(),
        "canBack": tool.can_back(),
        "default": Value::Null,
        "options": [],
        "filter": Value::Null,
        "capturePoint": false,
        "min": Value::Null,
        "connect": false,
        "points": tool.list_points().iter().map(|p| v3(*p)).collect::<Vec<_>>(),
        "selected": tool.selected_objects(),
        "lastPoint": tool.last_point().map(v3),
    });
    let (kind, prompt) = match *step {
        Step::PickPoint { prompt } => ("pickPoint", prompt),
        Step::Vector { prompt } => ("vector", prompt),
        Step::Number { prompt, default } => {
            v["default"] = json!(default);
            ("number", prompt)
        }
        Step::Integer { prompt, default } => {
            v["default"] = json!(default);
            ("integer", prompt)
        }
        Step::Keyword {
            prompt,
            options,
            default,
        } => {
            v["options"] = json!(options);
            v["default"] = json!(default);
            ("keyword", prompt)
        }
        Step::Branch { prompt, arms } => {
            let keys: Vec<&str> = arms.iter().map(|a| a.key).collect();
            v["default"] = json!(keys.first());
            v["options"] = json!(keys);
            ("branch", prompt)
        }
        Step::SelectObject {
            prompt,
            filter,
            capture_point,
        } => {
            v["filter"] = json!(filter_name(filter));
            v["capturePoint"] = json!(capture_point);
            ("selectObject", prompt)
        }
        Step::SelectObjects {
            prompt,
            filter,
            min,
        } => {
            v["filter"] = json!(filter_name(filter));
            v["min"] = json!(min);
            ("selectObjects", prompt)
        }
        Step::Text { prompt } => ("text", prompt),
        Step::PointList {
            prompt,
            min,
            connect,
        } => {
            v["min"] = json!(min);
            v["connect"] = json!(connect);
            ("pointList", prompt)
        }
    };
    v["kind"] = json!(kind);
    v["prompt"] = json!(prompt);
    v
}

fn filter_name(f: ObjFilter) -> &'static str {
    match f {
        ObjFilter::Any => "any",
        ObjFilter::Curve => "curve",
        ObjFilter::Solid => "solid",
    }
}

/// Does `geom` satisfy a step's [`ObjFilter`]? Same rule as the desktop's
/// `guided_pick_matches`.
fn filter_matches(filter: ObjFilter, geom: &Geometry) -> bool {
    match filter {
        ObjFilter::Any => true,
        ObjFilter::Curve => matches!(geom, Geometry::Curve(_)),
        ObjFilter::Solid => matches!(geom, Geometry::Mesh(_)),
    }
}

/// Turn a step result into the answer envelope. An emitted command runs now
/// through the normal command path (route + side-effect gate + `Session::run`).
fn finish(app: &mut AppHandle, r: StepResult) -> String {
    app.guided.clear_preview();
    let (status, error, command, run) = match r {
        StepResult::NeedMore => ("more", None, None, Value::Null),
        StepResult::Error(e) => ("error", Some(e), None, Value::Null),
        StepResult::Emit(cmd) => {
            let run = match check_line(&cmd) {
                Ok(PendingOp::Cmd(c)) => {
                    apply_pending(app);
                    run_cmd_json(&mut app.session, c)
                }
                Ok(PendingOp::Camera(k)) => {
                    apply_camera(app, k);
                    run_result_json(true, None, None, &[])
                }
                Err((reason, _)) => run_result_json(false, None, Some(&reason), &[]),
            };
            let run: Value = serde_json::from_str(&run).unwrap_or(Value::Null);
            ("done", None, Some(cmd), run)
        }
    };
    json!({
        "status": status,
        "error": error,
        "command": command,
        "run": run,
        "state": state_value(&app.guided.tool),
    })
    .to_string()
}

fn idle(app: Option<&AppHandle>, error: &str) -> *mut c_char {
    let state = app.map_or_else(
        || json!({ "active": false }),
        |a| state_value(&a.guided.tool),
    );
    into_c_string(
        json!({ "status": "idle", "error": error, "command": null, "run": null, "state": state })
            .to_string(),
    )
}

/// Run `body` with the mutable handle and an active guided run, returning the
/// answer envelope. Null/busy handle or no active run → `"idle"`.
fn with_active(h: *mut AppHandle, body: impl FnOnce(&mut AppHandle) -> String) -> *mut c_char {
    guard_ffi(std::ptr::null_mut(), || {
        let Some(mut guard) = (unsafe { handle_mut(h) }) else {
            return idle(None, "invalid or busy handle");
        };
        let app: &mut AppHandle = &mut guard;
        if !app.guided.tool.active() {
            return idle(Some(app), "no guided command is running");
        }
        into_c_string(body(app))
    })
}

fn c_str<'a>(s: *const c_char) -> Option<&'a str> {
    if s.is_null() {
        return None;
    }
    unsafe { CStr::from_ptr(s) }.to_str().ok()
}

/// True when `verb` (exactly, no arguments) has a guided script. Hosts use it
/// to route a bare verb to [`ijc_guided_start`] instead of the parser.
///
/// # Safety
/// `verb` must be null or a valid NUL-terminated C string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ijc_guided_is_verb(verb: *const c_char) -> bool {
    guard_ffi(false, || {
        c_str(verb).is_some_and(itsjustcad_guided::is_guided_verb)
    })
}

/// Every guided verb as a JSON array of strings. Caller must
/// [`super::ijc_string_free`] the result.
#[unsafe(no_mangle)]
pub extern "C" fn ijc_guided_verbs_json() -> *mut c_char {
    guard_ffi(std::ptr::null_mut(), || {
        into_c_string(json!(itsjustcad_guided::guided_verbs().collect::<Vec<_>>()).to_string())
    })
}

/// Start a guided run for a bare `verb`. Verbs that act on a pre-selection
/// (offset, array…) seed it as `sel`. Returns the answer envelope with
/// `status`: `"more"` (started; render `state`), `"done"` (a zero-step verb ran
/// at once), `"need_selection"` (select objects first) or `"not_guided"`. A
/// running flow is replaced. Caller must [`super::ijc_string_free`] the result.
///
/// # Safety
/// `h` must be null or a live handle; `verb` null or a valid C string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ijc_guided_start(h: *mut AppHandle, verb: *const c_char) -> *mut c_char {
    guard_ffi(std::ptr::null_mut(), || {
        let Some(mut guard) = (unsafe { handle_mut(h) }) else {
            return idle(None, "invalid or busy handle");
        };
        let app: &mut AppHandle = &mut guard;
        let Some(verb) = c_str(verb) else {
            return idle(Some(app), "verb is null or not UTF-8");
        };
        apply_pending(app);
        app.guided.tool.cancel();
        app.guided.clear_preview();
        let selection = (!app.session.doc.selection.is_empty()).then_some("sel");
        let status = match app.guided.tool.try_start(verb, selection) {
            StartResult::Started => match app.guided.tool.emit_if_ready() {
                Some(r) => return into_c_string(finish(app, r)),
                None => "more",
            },
            StartResult::NeedSelection => "need_selection",
            StartResult::NotGuided => "not_guided",
        };
        into_c_string(
            json!({
                "status": status, "error": null, "command": null, "run": null,
                "state": state_value(&app.guided.tool),
            })
            .to_string(),
        )
    })
}

/// The current step as JSON: `{"active":false}` when idle, else
/// `{"active":true,"verb","step","steps"(null while a branch is unchosen),
/// "kind":"pickPoint"|"vector"|"number"|"integer"|"keyword"|"branch"|
/// "selectObject"|"selectObjects"|"text"|"pointList","prompt","default",
/// "options":[…],"filter":"any"|"curve"|"solid"|null,"capturePoint","min",
/// "connect","points":[[x,y,z]…],"selected":["#id"…],"lastPoint","canBack"}`.
/// Caller must [`super::ijc_string_free`] the result.
///
/// # Safety
/// `h` must be null or a live handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ijc_guided_state_json(h: *mut AppHandle) -> *mut c_char {
    guard_ffi(std::ptr::null_mut(), || {
        let state = (unsafe { handle_ref(h) }).map_or_else(
            || json!({ "active": false }),
            |a| state_value(&a.guided.tool),
        );
        into_c_string(state.to_string())
    })
}

/// Answer a `pickPoint` / `vector` step, or add one point to a `pointList`
/// (finish the list with [`ijc_guided_answer_default`]). World coordinates;
/// the numeric fallback for a typed `x,y,z`. Returns the answer envelope.
///
/// # Safety
/// `h` must be null or a live handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ijc_guided_answer_point(
    h: *mut AppHandle,
    x: f64,
    y: f64,
    z: f64,
) -> *mut c_char {
    with_active(h, |app| {
        let p = DVec3::new(x, y, z);
        if !p.is_finite() {
            return finish(app, StepResult::Error("point must be finite".into()));
        }
        let r = match app.guided.tool.current() {
            Some(Step::PointList { .. }) => app.guided.tool.push_list_point(p),
            Some(Step::PickPoint { .. } | Step::Vector { .. }) => app.guided.tool.on_click(p),
            _ => StepResult::Error("this step does not take a point".into()),
        };
        finish(app, r)
    })
}

/// Answer a `number` step. Non-finite values are refused.
///
/// # Safety
/// `h` must be null or a live handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ijc_guided_answer_number(h: *mut AppHandle, value: f64) -> *mut c_char {
    with_active(h, |app| {
        let r = match app.guided.tool.current() {
            Some(Step::Number { .. }) if value.is_finite() => {
                app.guided.tool.commit_typed(&value.to_string())
            }
            Some(Step::Number { .. }) => StepResult::Error("type a finite number".into()),
            _ => StepResult::Error("this step does not take a number".into()),
        };
        finish(app, r)
    })
}

/// Answer an `integer` step.
///
/// # Safety
/// `h` must be null or a live handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ijc_guided_answer_integer(h: *mut AppHandle, value: i64) -> *mut c_char {
    with_active(h, |app| {
        let r = match app.guided.tool.current() {
            Some(Step::Integer { .. }) => app.guided.tool.commit_typed(&value.to_string()),
            _ => StepResult::Error("this step does not take a whole number".into()),
        };
        finish(app, r)
    })
}

/// Answer a `keyword` or `branch` step with an option (exact or unique prefix,
/// case-insensitive).
///
/// # Safety
/// `h` must be null or a live handle; `key` null or a valid C string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ijc_guided_answer_keyword(
    h: *mut AppHandle,
    key: *const c_char,
) -> *mut c_char {
    with_active(h, |app| {
        let r = match (app.guided.tool.current(), c_str(key)) {
            (Some(Step::Keyword { .. } | Step::Branch { .. }), Some(k)) if !k.trim().is_empty() => {
                app.guided.tool.commit_typed(k)
            }
            (Some(Step::Keyword { .. } | Step::Branch { .. }), _) => {
                StepResult::Error("choose an option".into())
            }
            _ => StepResult::Error("this step does not take an option".into()),
        };
        finish(app, r)
    })
}

/// Answer a `text` step (one name token: letters, digits, `_`, `-`).
///
/// # Safety
/// `h` must be null or a live handle; `text` null or a valid C string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ijc_guided_answer_text(
    h: *mut AppHandle,
    text: *const c_char,
) -> *mut c_char {
    with_active(h, |app| {
        if !matches!(app.guided.tool.current(), Some(Step::Text { .. })) {
            return finish(
                app,
                StepResult::Error("this step does not take a name".into()),
            );
        }
        let t = c_str(text).unwrap_or("").trim();
        if !t
            .chars()
            .all(|c| c.is_alphanumeric() || c == '_' || c == '-')
        {
            return finish(
                app,
                StepResult::Error("a name is letters, digits, _ and - only".into()),
            );
        }
        app.guided.tool.take_input();
        for c in t.chars() {
            app.guided.tool.push_input(c);
        }
        let r = app.guided.tool.commit_text();
        finish(app, r)
    })
}

/// Find a document object by short id (8 hex), full id or uuid, `#` optional.
fn find_object(doc: &Document, id: &str) -> Option<ObjectId> {
    let id = id.trim().trim_start_matches('#');
    doc.objects()
        .find(|o| {
            o.id.short() == id || o.id.0.simple().to_string() == id || o.id.0.to_string() == id
        })
        .map(|o| o.id)
}

/// Commit a picked object to a `selectObject` / `selectObjects` step after
/// checking the step's filter. Highlights it (adds it to the selection) as the
/// desktop does; the emitted command names it by id, so that is cosmetic.
fn commit_object(app: &mut AppHandle, id: ObjectId, point: Option<DVec3>) -> StepResult {
    let Some(filter) = app.guided.tool.current_filter() else {
        return StepResult::Error("this step does not take an object".into());
    };
    let Some(obj) = app.session.doc.get(id) else {
        return StepResult::Error("no such object".into());
    };
    if !filter_matches(filter, &obj.geometry) {
        return StepResult::Error(format!("pick a {} object", filter_name(filter)));
    }
    app.session.doc.selection.insert(id);
    app.session.doc.generation += 1;
    let short = id.short();
    if app.guided.tool.current_wants_objects() {
        app.guided.tool.push_selected_object(&short)
    } else {
        app.guided.tool.commit_object_at(&short, point)
    }
}

/// Answer a `selectObject` step (or add to a `selectObjects` set) by object id
/// (short id, full id or uuid). When `has_point`, `(x,y,z)` is the pick
/// location (fillet uses it to choose the corner). The object must match the
/// step's filter.
///
/// # Safety
/// `h` must be null or a live handle; `id` null or a valid C string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ijc_guided_answer_object(
    h: *mut AppHandle,
    id: *const c_char,
    has_point: bool,
    x: f64,
    y: f64,
    z: f64,
) -> *mut c_char {
    with_active(h, |app| {
        let Some(oid) = c_str(id).and_then(|s| find_object(&app.session.doc, s)) else {
            return finish(app, StepResult::Error("no such object".into()));
        };
        let p = DVec3::new(x, y, z);
        let r = commit_object(app, oid, (has_point && p.is_finite()).then_some(p));
        finish(app, r)
    })
}

/// Enter: accept the current step's default (`number`/`integer`/`keyword`/
/// `branch`), or finish a `pointList` / `selectObjects` once it has `min`
/// items. Point and single-object steps have no default (status `"error"`).
///
/// # Safety
/// `h` must be null or a live handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ijc_guided_answer_default(h: *mut AppHandle) -> *mut c_char {
    with_active(h, |app| {
        app.guided.tool.take_input();
        let r = app.guided.tool.commit_typed("");
        finish(app, r)
    })
}

/// Back: drop the last point/object of an in-progress list, else re-open the
/// previous step. Returns the answer envelope (`"more"`; `error` set when
/// there was nothing to undo).
///
/// # Safety
/// `h` must be null or a live handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ijc_guided_back(h: *mut AppHandle) -> *mut c_char {
    with_active(h, |app| {
        if app.guided.tool.back() {
            finish(app, StepResult::NeedMore)
        } else {
            finish(app, StepResult::Error("nothing to go back to".into()))
        }
    })
}

/// Cancel the running guided command (nothing runs). Returns whether one was
/// running.
///
/// # Safety
/// `h` must be null or a live handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ijc_guided_cancel(h: *mut AppHandle) -> bool {
    guard_ffi(false, || {
        let Some(mut app) = (unsafe { handle_mut(h) }) else {
            return false;
        };
        let was = app.guided.tool.active();
        app.guided.tool.cancel();
        app.guided.clear_preview();
        was
    })
}

/// A snapped world point under a screen position, and the snap kind.
struct Snap {
    point: DVec3,
    kind: &'static str,
}

/// Snap candidates of one object (end/mid/center of curves, small-mesh
/// vertices) — the touch subset of the desktop's osnap.
fn snap_candidates(geom: &Geometry, out: &mut Vec<(DVec3, &'static str)>) {
    match geom {
        Geometry::Curve(c) => match c {
            Curve::Line { a, b } => {
                out.extend([(*a, "end"), (*b, "end"), ((*a + *b) / 2.0, "mid")])
            }
            Curve::Polyline { points, closed } => {
                out.extend(points.iter().map(|p| (*p, "end")));
                let segs = if *closed {
                    points.len()
                } else {
                    points.len().saturating_sub(1)
                };
                for i in 0..segs {
                    out.push(((points[i] + points[(i + 1) % points.len()]) / 2.0, "mid"));
                }
            }
            Curve::Arc {
                center,
                radius,
                start,
                end,
            } => {
                out.push((*center, "cen"));
                if !c.is_closed() {
                    let at = |t: f64| *center + DVec3::new(radius * t.cos(), radius * t.sin(), 0.0);
                    out.extend([
                        (at(*start), "end"),
                        (at(*end), "end"),
                        (at((start + end) / 2.0), "mid"),
                    ]);
                }
            }
            Curve::Ellipse { center, .. } => out.push((*center, "cen")),
            Curve::Nurbs { .. } => {
                let pts = c.tessellate(0.05);
                if let (Some(a), Some(b)) = (pts.first(), pts.last()) {
                    out.extend([(*a, "end"), (*b, "end")]);
                }
            }
        },
        Geometry::Mesh(m) if m.positions().len() <= 5_000 => {
            out.extend(m.positions().iter().map(|p| (*p, "vtx")));
        }
        _ => {}
    }
}

/// Resolve a screen position (drawable pixels) to a world point: the nearest
/// snap candidate within `radius_px`, else the surface under the ray, else the
/// construction plane rounded to the 10 cm grid.
fn snap_at(app: &AppHandle, x_px: f32, y_px: f32, radius_px: f32) -> Option<Snap> {
    let (w, h) = viewport_size(app)?;
    let vp = app.camera.view_proj(w / h);
    let (origin, dir) = screen_ray(vp, w, h, x_px, y_px)?;
    let doc = &app.session.doc;
    let tap = Vec2::new(x_px, y_px);
    let mut cands = Vec::new();
    for o in doc
        .objects()
        .filter(|o| o.visible && doc.layer_visible(&o.layer))
    {
        snap_candidates(&o.geometry, &mut cands);
    }
    let best = cands
        .into_iter()
        .filter_map(|(p, k)| Some((boxsel::project(vp, w, h, p)?.distance(tap), p, k)))
        .filter(|(d, ..)| *d <= radius_px.max(0.0))
        .min_by(|a, b| a.0.total_cmp(&b.0));
    if let Some((_, point, kind)) = best {
        return Some(Snap { point, kind });
    }
    let surface = doc
        .objects()
        .filter(|o| o.visible && doc.layer_visible(&o.layer))
        .filter_map(|o| object_hit(doc, &o.geometry, origin, dir, false))
        .min_by(f64::total_cmp);
    if let Some(t) = surface {
        return Some(Snap {
            point: origin + dir * t,
            kind: "surface",
        });
    }
    let t = ray_plane(&doc.cplane, origin, dir)?;
    Some(Snap {
        point: grid_snap(origin + dir * t),
        kind: "grid",
    })
}

/// The point of segment `a`–`b` closest to the ray `origin + t·dir` (the pick
/// location on a tapped curve; screen-space interpolation would skew it under
/// perspective).
fn closest_on_segment(a: DVec3, b: DVec3, origin: DVec3, dir: DVec3) -> DVec3 {
    let u = b - a;
    let w = a - origin;
    let (uu, ud, dd) = (u.dot(u), u.dot(dir), dir.dot(dir));
    let denom = uu * dd - ud * ud;
    if uu < 1e-18 || denom.abs() < 1e-18 {
        return a;
    }
    let s = ((ud * dir.dot(w) - dd * u.dot(w)) / denom).clamp(0.0, 1.0);
    a + u * s
}

/// The object a tap at `(x_px, y_px)` picks for an object step: the nearest
/// curve (within `radius_px` of its projected line) or surface that passes
/// `filter`, with the world pick point (on the curve / the surface).
fn pick_object_at(
    app: &AppHandle,
    filter: ObjFilter,
    x_px: f32,
    y_px: f32,
    radius_px: f32,
) -> Option<(ObjectId, DVec3)> {
    let (w, h) = viewport_size(app)?;
    let vp = app.camera.view_proj(w / h);
    let (origin, dir) = screen_ray(vp, w, h, x_px, y_px)?;
    let doc = &app.session.doc;
    let tap = Vec2::new(x_px, y_px);
    let visible = || {
        doc.objects().filter(|o| {
            o.visible && doc.layer_visible(&o.layer) && filter_matches(filter, &o.geometry)
        })
    };
    // Curves: screen distance to the tessellated, projected polyline.
    let mut best: Option<(f32, ObjectId, DVec3)> = None;
    for o in visible() {
        let Geometry::Curve(c) = &o.geometry else {
            continue;
        };
        let mut pts = c.tessellate(0.05);
        if c.is_closed()
            && let Some(first) = pts.first().copied()
        {
            pts.push(first);
        }
        for seg in pts.windows(2) {
            let (Some(a), Some(b)) = (
                boxsel::project(vp, w, h, seg[0]),
                boxsel::project(vp, w, h, seg[1]),
            ) else {
                continue;
            };
            let ab = b - a;
            let t = if ab.length_squared() > 0.0 {
                ((tap - a).dot(ab) / ab.length_squared()).clamp(0.0, 1.0)
            } else {
                0.0
            };
            let d = (a + ab * t).distance(tap);
            if d <= radius_px && best.is_none_or(|(bd, ..)| d < bd) {
                best = Some((d, o.id, closest_on_segment(seg[0], seg[1], origin, dir)));
            }
        }
    }
    if let Some((_, id, p)) = best {
        return Some((id, p));
    }
    // Surfaces: nearest exact hit along the ray.
    visible()
        .filter_map(|o| Some((object_hit(doc, &o.geometry, origin, dir, true)?, o.id)))
        .min_by(|a, b| a.0.total_cmp(&b.0))
        .map(|(t, id)| (id, origin + dir * t))
}

/// Resolve a screen position (drawable pixels, as [`super::ijc_pick`]) to a
/// snapped world point for a point step, without answering:
/// `{"point":[x,y,z],"snap":"end"|"mid"|"cen"|"vtx"|"surface"|"grid"}` or
/// `{"point":null}`. Snaps to curve end/mid/center points and small-mesh
/// vertices within `radius_px`. Hosts use it while a finger drags, feeding
/// the point to [`ijc_guided_preview_json`]. Caller must
/// [`super::ijc_string_free`] the result.
///
/// # Safety
/// `h` must be null or a live handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ijc_guided_snap_json(
    h: *mut AppHandle,
    x_px: f32,
    y_px: f32,
    radius_px: f32,
) -> *mut c_char {
    guard_ffi(std::ptr::null_mut(), || {
        let snap = (unsafe { handle_ref(h) }).and_then(|app| snap_at(app, x_px, y_px, radius_px));
        let v = match snap {
            Some(s) => json!({ "point": v3(s.point), "snap": s.kind }),
            None => json!({ "point": null, "snap": null }),
        };
        into_c_string(v.to_string())
    })
}

/// A viewport tap during a guided run, in drawable pixels: point steps take the
/// snapped point ([`ijc_guided_snap_json`]); object steps take the nearest
/// matching object within `radius_px` (curves by their projected line, solids
/// by their surface) and, for fillet-style steps, the pick point on it. A tap
/// that hits nothing usable returns `"error"` and stays on the step.
///
/// # Safety
/// `h` must be null or a live handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ijc_guided_pick(
    h: *mut AppHandle,
    x_px: f32,
    y_px: f32,
    radius_px: f32,
) -> *mut c_char {
    with_active(h, |app| {
        let r = match app.guided.tool.current() {
            Some(Step::SelectObject { filter, .. } | Step::SelectObjects { filter, .. }) => {
                match pick_object_at(app, *filter, x_px, y_px, radius_px) {
                    Some((id, p)) => commit_object(app, id, Some(p)),
                    None => StepResult::Error(format!(
                        "no {} object there — tap again",
                        filter_name(*filter)
                    )),
                }
            }
            Some(Step::PickPoint { .. } | Step::Vector { .. } | Step::PointList { .. }) => {
                match snap_at(app, x_px, y_px, radius_px) {
                    Some(s)
                        if matches!(app.guided.tool.current(), Some(Step::PointList { .. })) =>
                    {
                        app.guided.tool.push_list_point(s.point)
                    }
                    Some(s) => app.guided.tool.on_click(s.point),
                    None => StepResult::Error("no point there — tap again".into()),
                }
            }
            _ => StepResult::Error("this step is answered in the guided bar".into()),
        };
        finish(app, r)
    })
}

/// The outcome of running `cmd` in a scratch copy of the document: wireframe
/// polylines of every object it created or changed. `None` when it fails.
fn result_ghost(doc: &Document, cmd: &str) -> Option<Vec<Vec<DVec3>>> {
    let Ok(PendingOp::Cmd(c)) = check_line(cmd) else {
        return None;
    };
    let mut scratch = Session::default();
    scratch.doc = doc.clone();
    scratch.run(c).ok()?;
    let lines = scratch
        .doc
        .objects()
        .filter(|o| doc.get(o.id).is_none_or(|old| old.geometry != o.geometry))
        .flat_map(object_wireframe_world)
        .map(|[a, b]| vec![a, b])
        .collect();
    Some(lines)
}

/// Live preview for the current step, given the cursor / finger world point
/// (`has_cursor`; e.g. from [`ijc_guided_snap_json`]) and `pending`, the text
/// in the step's field (a number or option not yet answered; null or empty
/// means the default). Returns `{"ghosts":[[[x,y,z]…]…],"command":str|null}`:
/// rubber-band polylines from the picked points to the cursor, plus — for
/// offset, fillet, chamfer, array and polararray, once every remaining input
/// is known or defaulted — the outcome's wireframe, computed by running
/// `command` in a scratch copy of the document. The host draws them over the
/// viewport. Caller must [`super::ijc_string_free`] the result.
///
/// # Safety
/// `h` must be null or a live handle; `pending` null or a valid C string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ijc_guided_preview_json(
    h: *mut AppHandle,
    has_cursor: bool,
    x: f64,
    y: f64,
    z: f64,
    pending: *const c_char,
) -> *mut c_char {
    guard_ffi(std::ptr::null_mut(), || {
        let empty = || into_c_string(json!({ "ghosts": [], "command": null }).to_string());
        let Some(mut guard) = (unsafe { handle_mut(h) }) else {
            return empty();
        };
        let app: &mut AppHandle = &mut guard;
        let cursor = DVec3::new(x, y, z);
        let cursor = (has_cursor && cursor.is_finite()).then_some(cursor);
        let g = &mut app.guided;
        let mut ghosts = g.tool.preview(cursor);
        let command = g
            .tool
            .active_verb()
            .filter(|v| RESULT_PREVIEW_VERBS.contains(v))
            .and_then(|_| g.tool.hypothetical_command(cursor, c_str(pending)));
        if let Some(cmd) = &command {
            let lines = match &g.cached {
                Some((c, lines)) if c == cmd => lines.clone(),
                _ => {
                    let lines = result_ghost(&app.session.doc, cmd).unwrap_or_default();
                    g.cached = Some((cmd.clone(), lines.clone()));
                    lines
                }
            };
            ghosts.extend(lines);
        }
        let out: Vec<Vec<Value>> = ghosts
            .iter()
            .map(|l| l.iter().map(|p| v3(*p)).collect())
            .collect();
        into_c_string(json!({ "ghosts": out, "command": command }).to_string())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ijc_free, ijc_init_headless, ijc_resize, ijc_run_command_result, ijc_string_free};
    use std::ffi::CString;

    fn take(p: *mut c_char) -> Value {
        let s = unsafe { CStr::from_ptr(p) }.to_str().unwrap().to_string();
        unsafe { ijc_string_free(p) };
        serde_json::from_str(&s).unwrap()
    }

    fn run(h: *mut AppHandle, line: &str) -> Value {
        let line = CString::new(line).unwrap();
        take(unsafe { ijc_run_command_result(h, line.as_ptr()) })
    }

    /// Screen position (drawable pixels) of a world point on a headless handle.
    fn screen(h: *mut AppHandle, p: DVec3) -> (f32, f32) {
        let app = unsafe { handle_ref(h) }.unwrap();
        let (w, hh) = viewport_size(app).unwrap();
        let s = boxsel::project(app.camera.view_proj(w / hh), w, hh, p).unwrap();
        (s.x, s.y)
    }

    #[test]
    fn viewport_taps_pick_filtered_objects_and_snap_points() {
        let h = ijc_init_headless();
        unsafe { ijc_resize(h, 1200, 900) };
        let a = run(h, "line 0,0 10,0")["created"][0]
            .as_str()
            .unwrap()
            .to_string();
        let b = run(h, "line 0,0 0,10")["created"][0]
            .as_str()
            .unwrap()
            .to_string();
        run(h, "ze");

        let verb = CString::new("fillet").unwrap();
        take(unsafe { ijc_guided_start(h, verb.as_ptr()) });
        let (x, y) = screen(h, DVec3::new(2.0, 0.0, 0.0));
        let r = take(unsafe { ijc_guided_pick(h, x, y + 3.0, 24.0) });
        assert_eq!(r["status"], "more", "{r}");
        assert_eq!(r["state"]["prompt"], "Select second curve to fillet");
        // Empty space: stays on the step.
        let (x, y) = screen(h, DVec3::new(6.0, 6.0, 0.0));
        assert_eq!(
            take(unsafe { ijc_guided_pick(h, x, y, 24.0) })["status"],
            "error"
        );
        let (x, y) = screen(h, DVec3::new(0.0, 2.0, 0.0));
        take(unsafe { ijc_guided_pick(h, x + 3.0, y, 24.0) });
        take(unsafe { ijc_guided_answer_default(h) });
        take(unsafe { ijc_guided_answer_default(h) });
        let done = take(unsafe { ijc_guided_answer_default(h) });
        let cmd = done["command"].as_str().unwrap();
        // `at` carries the pick points (near the corner) on each line.
        let at: Vec<f64> = cmd
            .strip_prefix(&format!("fillet #{a} #{b} 0.5 at "))
            .and_then(|r| r.strip_suffix(" trim yes join no"))
            .unwrap_or_else(|| panic!("{cmd}"))
            .split([' ', ','])
            .map(|n| n.parse().unwrap())
            .collect();
        assert!((at[0] - 2.0).abs() < 0.2 && at[1] == 0.0, "{cmd}");
        assert!(at[2] == 0.0 && (at[3] - 2.0).abs() < 0.2, "{cmd}");
        assert_eq!(done["run"]["ok"], true, "{done}");

        // Point steps snap to curve endpoints within the radius, else the grid.
        let (x, y) = screen(h, DVec3::new(10.0, 0.0, 0.0));
        let s = take(unsafe { ijc_guided_snap_json(h, x + 4.0, y - 4.0, 24.0) });
        assert_eq!(s["snap"], "end", "{s}");
        assert_eq!(s["point"], json!([10.0, 0.0, 0.0]));
        let (x, y) = screen(h, DVec3::new(5.03, 5.04, 0.0));
        let s = take(unsafe { ijc_guided_snap_json(h, x, y, 4.0) });
        assert_eq!(s["snap"], "grid", "{s}");
        unsafe { ijc_free(h) };
    }
}
