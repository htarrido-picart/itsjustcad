// SPDX-License-Identifier: AGPL-3.0-only
// Copyright © 2026 Hector Tarrido-Picart

//! Multi-id selection and refocus after plan edits (#165).
//!
//! A lot or street edit re-derives the site plan, which mints fresh lot and
//! road ids. The edit entry points ([`super::ijc_lot_set`],
//! [`super::ijc_lot_height_set`], [`super::ijc_lot_reset`], [`super::ijc_edge_role_set`],
//! [`super::ijc_street_set`], [`super::ijc_street_edit`]) therefore re-select
//! what they edited, as the desktop inspector does with `lot::focus_anchor` +
//! `lot::refocus`, and report the new ids in their run-result JSON under
//! `"selected"`.

use std::ffi::c_char;

use itsjustcad_commands::lot::{self, Focus};
use itsjustcad_commands::{Command, Session};
use itsjustcad_doc::ObjectId;

use super::{AppHandle, apply_pending, c_str, guard_ffi, handle_mut, object_by_id};

/// Run `cmd` (a plan edit of the objects `focus` names), then re-select the
/// same lots or street in the re-derived plan. Returns the run-result JSON
/// with an extra `"selected":[id…]`: the new ids now selected, or `[]` when
/// the edit failed or nothing could be refocused (the selection is then left
/// alone).
pub(crate) fn run_refocus_json(session: &mut Session, focus: &Focus, cmd: Command) -> String {
    let anchor = lot::focus_anchor(&session.doc, focus);
    let out = match session.run(cmd) {
        Ok(out) => out,
        Err(e) => {
            return serde_json::json!({
                "ok": false, "message": null, "error": e.to_string(),
                "created": [], "selected": [],
            })
            .to_string();
        }
    };
    let sel = anchor
        .map(|a| lot::refocus(&session.doc, &a))
        .unwrap_or_default();
    let selected: Vec<String> = sel.iter().map(ObjectId::to_string).collect();
    if !sel.is_empty() {
        session.doc.selection = sel;
        session.doc.generation += 1;
    }
    let created: Vec<String> = out.created.iter().map(ObjectId::to_string).collect();
    serde_json::json!({
        "ok": true, "message": out.message, "error": null,
        "created": created, "selected": selected,
    })
    .to_string()
}

/// The focus of an edit on `ids` (lots, or buildings standing on lots).
pub(crate) fn focus_of(doc: &itsjustcad_doc::Document, ids: &[ObjectId]) -> Focus {
    lot::focus_target(doc, ids)
}

/// Resolve a comma-separated list of short ids / uuids. `None` if any is
/// unknown or the list is empty.
pub(crate) fn ids_by_list(doc: &itsjustcad_doc::Document, list: &str) -> Option<Vec<ObjectId>> {
    let ids: Option<Vec<ObjectId>> = list
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| object_by_id(doc, s))
        .collect();
    ids.filter(|v| !v.is_empty())
}

/// Select objects by id, unlogged (no undo step), like a tap. `ids_json` is a
/// JSON array of short ids or full uuids. Replaces the selection, or adds to
/// it when `additive`. Unknown ids are skipped. Queued ops are applied first,
/// so ids created by queued lines resolve. Returns how many objects are now
/// selected, or -1 on a null/invalid/busy handle, a null `ids_json`, or JSON
/// that is not an array of strings.
///
/// Use it to restore a multi-selection (e.g. several lots) and then run one
/// `lotset selected …` or [`super::ijc_lot_set`] with a comma-separated id
/// list, so a multi-lot edit is one undo step.
///
/// # Safety
/// `h` must be null or a live handle; `ids_json` must be null or a valid
/// NUL-terminated C string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ijc_select_ids(
    h: *mut AppHandle,
    ids_json: *const c_char,
    additive: bool,
) -> i32 {
    guard_ffi(-1, || {
        let Some(mut guard) = (unsafe { handle_mut(h) }) else {
            return -1;
        };
        let app: &mut AppHandle = &mut guard;
        let Some(json) = (unsafe { c_str(ids_json) }) else {
            return -1;
        };
        let Ok(list) = serde_json::from_str::<Vec<String>>(json) else {
            return -1;
        };
        apply_pending(app);
        let doc = &mut app.session.doc;
        let ids: Vec<ObjectId> = list.iter().filter_map(|s| object_by_id(doc, s)).collect();
        let before = doc.selection.clone();
        if !additive {
            doc.selection.clear();
        }
        doc.selection.extend(ids);
        if doc.selection != before {
            doc.generation += 1;
        }
        doc.selection.len() as i32
    })
}

#[cfg(test)]
mod tests {
    use super::super::*;
    use super::ijc_select_ids;
    use std::ffi::{CStr, CString};

    fn c(s: &str) -> CString {
        CString::new(s).unwrap()
    }

    unsafe fn r(p: *mut c_char) -> serde_json::Value {
        let s = unsafe { CStr::from_ptr(p) }.to_str().unwrap().to_owned();
        unsafe { ijc_string_free(p) };
        serde_json::from_str(&s).unwrap()
    }

    fn site(h: *mut AppHandle) {
        for line in ["rect 0,0,0 600 450", "siteplan last"] {
            assert_eq!(
                unsafe { r(ijc_run_command_result(h, c(line).as_ptr())) }["ok"],
                true
            );
        }
    }

    fn lots(h: *mut AppHandle) -> Vec<String> {
        let app = unsafe { handle_ref(h) }.unwrap();
        let doc = &app.session.doc;
        doc.objects()
            .filter(|o| o.layer == "lots")
            .map(|o| o.id.to_string())
            .collect()
    }

    fn selection(h: *mut AppHandle) -> Vec<String> {
        let app = unsafe { handle_ref(h) }.unwrap();
        app.session
            .doc
            .selection
            .iter()
            .map(|i| i.to_string())
            .collect()
    }

    #[test]
    fn select_ids_replaces_adds_and_skips_unknown() {
        let h = ijc_init_headless();
        unsafe {
            let a =
                r(ijc_run_command_result(h, c("box 0,0,0 1,1,1").as_ptr()))["created"][0].clone();
            let b =
                r(ijc_run_command_result(h, c("box 3,0,0 1,1,1").as_ptr()))["created"][0].clone();
            let (a, b) = (
                a.as_str().unwrap().to_string(),
                b.as_str().unwrap().to_string(),
            );
            let undo = ijc_undo_cursor(h);
            let both = format!(r#"["{a}","{b}","nope"]"#);
            assert_eq!(ijc_select_ids(h, c(&both).as_ptr(), false), 2);
            assert_eq!(ijc_undo_cursor(h), undo, "unlogged");
            assert_eq!(
                ijc_select_ids(h, c(&format!(r#"["{a}"]"#)).as_ptr(), false),
                1
            );
            assert_eq!(selection(h), vec![a.clone()]);
            assert_eq!(
                ijc_select_ids(h, c(&format!(r#"["{b}"]"#)).as_ptr(), true),
                2
            );
            assert_eq!(ijc_select_ids(h, c("[]").as_ptr(), false), 0);
            assert_eq!(ijc_select_ids(h, c("{}").as_ptr(), false), -1);
            assert_eq!(ijc_select_ids(h, std::ptr::null(), false), -1);
            assert_eq!(
                ijc_select_ids(std::ptr::null_mut(), c("[]").as_ptr(), false),
                -1
            );
            ijc_free(h);
        }
    }

    /// Several lots edited in one `ijc_lot_set` call are one undo step, and the
    /// re-derived lots come back selected (new ids) in `"selected"`.
    #[test]
    fn multi_lot_set_is_one_undo_and_refocuses() {
        let h = ijc_init_headless();
        unsafe {
            site(h);
            let all = lots(h);
            let pick = format!("{},{}", all[0], all[1]);
            let json = format!(r#"["{}","{}"]"#, all[0], all[1]);
            assert_eq!(ijc_select_ids(h, c(&json).as_ptr(), false), 2);
            let undo = ijc_undo_cursor(h);
            let out = r(ijc_lot_set(
                h,
                c(&pick).as_ptr(),
                c(r#"{"floors": 7}"#).as_ptr(),
            ));
            assert_eq!(out["ok"], true, "{out}");
            assert_eq!(ijc_undo_cursor(h), undo + 1, "one undo step");
            let sel: Vec<String> = out["selected"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_str().unwrap().to_string())
                .collect();
            assert_eq!(sel.len(), 2, "{out}");
            assert_eq!(selection(h), sel, "core selection follows");
            let now = lots(h);
            for id in &sel {
                assert!(now.contains(id), "{id} is a live lot");
                let v = r(ijc_lot_overrides_json(h, c(id).as_ptr()));
                assert_eq!(v["effective"]["floors"], 7, "{v}");
            }
            // An unknown id in the list fails the whole edit.
            let bad = format!("{},nope", sel[0]);
            let out = r(ijc_lot_set(
                h,
                c(&bad).as_ptr(),
                c(r#"{"floors": 3}"#).as_ptr(),
            ));
            assert_eq!(out["ok"], false);
            ijc_free(h);
        }
    }

    #[test]
    fn street_edits_refocus_the_street() {
        let h = ijc_init_headless();
        unsafe {
            site(h);
            let streets = r(ijc_streets_json(h));
            let source = streets[0]["source"].as_str().unwrap().to_string();
            let id = streets[0]["id"].as_u64().unwrap() as u32;
            let out = r(ijc_street_set(
                h,
                c(&source).as_ptr(),
                id,
                c(r#"{"width":18}"#).as_ptr(),
            ));
            assert_eq!(out["ok"], true, "{out}");
            let roads = r(ijc_street_json(h, c(&source).as_ptr(), id))["objects"].clone();
            assert!(!roads.as_array().unwrap().is_empty());
            assert_eq!(out["selected"], roads, "roads re-selected");
            assert_eq!(r(ijc_focus_json(h))["street_id"], id);

            let edit = format!(r#"{{"op":"move","street":{id},"delta":[1,0]}}"#);
            let out = r(ijc_street_edit(h, c(&source).as_ptr(), c(&edit).as_ptr()));
            if out["ok"] == true {
                assert_eq!(r(ijc_focus_json(h))["street_id"], id, "{out}");
                assert!(!out["selected"].as_array().unwrap().is_empty());
            } else {
                assert_eq!(out["selected"], serde_json::json!([]), "{out}");
            }

            let lot = lots(h)[0].clone();
            let out = r(ijc_lot_height_set(h, c(&lot).as_ptr(), 4));
            assert_eq!(out["ok"], true, "{out}");
            assert_eq!(out["selected"].as_array().unwrap().len(), 1, "{out}");
            assert_eq!(r(ijc_focus_json(h))["kind"], "lot");
            ijc_free(h);
        }
    }
}
