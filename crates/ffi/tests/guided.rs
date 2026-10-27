// SPDX-License-Identifier: AGPL-3.0-only

//! Headless end-to-end runs of the guided-verb FFI (itsjustcad-ios#121): each
//! run emits the same command string as typing it, and lands as one command /
//! one undo step.

use std::ffi::{CString, c_char};

use itsjustcad_ffi::guided::*;
use itsjustcad_ffi::{
    AppHandle, ijc_free, ijc_init_headless, ijc_run_command_result, ijc_scene_digest,
    ijc_string_free, ijc_undo_len,
};
use serde_json::Value;

fn c(s: &str) -> CString {
    CString::new(s).unwrap()
}

fn take(p: *mut c_char) -> Value {
    assert!(!p.is_null());
    let s = unsafe { std::ffi::CStr::from_ptr(p) }
        .to_str()
        .unwrap()
        .to_string();
    unsafe { ijc_string_free(p) };
    serde_json::from_str(&s).unwrap()
}

fn run(h: *mut AppHandle, line: &str) -> Value {
    let v = take(unsafe { ijc_run_command_result(h, c(line).as_ptr()) });
    assert_eq!(v["ok"], true, "{line}: {v}");
    v
}

fn digest(h: *mut AppHandle) -> String {
    let p = unsafe { ijc_scene_digest(h) };
    let s = unsafe { std::ffi::CStr::from_ptr(p) }
        .to_str()
        .unwrap()
        .to_string();
    unsafe { ijc_string_free(p) };
    s
}

fn object_count(h: *mut AppHandle) -> usize {
    // One digest line per object after the header (`#…` / blank lines skipped).
    digest(h)
        .lines()
        .filter(|l| l.contains(" id:") || l.starts_with("- "))
        .count()
}

/// The run's message with its created ids masked (fresh ids differ per run).
fn masked(run: &Value) -> String {
    let mut m = run["message"].as_str().unwrap_or_default().to_string();
    for id in run["created"].as_array().unwrap() {
        m = m.replace(id.as_str().unwrap(), "<id>");
    }
    m
}

fn start(h: *mut AppHandle, verb: &str) -> Value {
    take(unsafe { ijc_guided_start(h, c(verb).as_ptr()) })
}

/// Undo the guided run and type `typed` instead: the message and the
/// object count must match, and the typed run is one undo step too.
fn assert_matches_typed(h: *mut AppHandle, done: &Value, typed: &str) {
    assert_eq!(done["status"], "done", "{done}");
    assert_eq!(done["command"], typed, "same string as typing it");
    assert_eq!(done["run"]["ok"], true, "{done}");
    let objects = object_count(h);
    let undo_before = unsafe { ijc_undo_len(h) };
    run(h, "undo");
    let typed_run = run(h, typed);
    assert_eq!(masked(&typed_run), masked(&done["run"]), "same outcome");
    assert_eq!(object_count(h), objects);
    assert_eq!(
        unsafe { ijc_undo_len(h) },
        undo_before,
        "typed form is one undo step as well"
    );
}

#[test]
fn offset_end_to_end_matches_typed() {
    let h = ijc_init_headless();
    run(h, "line 0,0 10,0");
    run(h, "select all");
    let undo0 = unsafe { ijc_undo_len(h) };

    let s = start(h, "offset");
    assert_eq!(s["status"], "more");
    assert_eq!(s["state"]["kind"], "number");
    assert_eq!(s["state"]["prompt"], "Offset distance");
    assert_eq!(s["state"]["default"], 1.0);
    assert_eq!(s["state"]["canBack"], false);

    let r = take(unsafe { ijc_guided_answer_number(h, 2.0) });
    assert_eq!(r["status"], "more");
    assert_eq!(r["state"]["kind"], "pickPoint");

    // Live preview at the finger: the outcome ghost and its command.
    let p = take(unsafe { ijc_guided_preview_json(h, true, 5.0, 3.0, 0.0, std::ptr::null()) });
    assert_eq!(p["command"], "offset sel 2 5,3");
    assert!(!p["ghosts"].as_array().unwrap().is_empty(), "{p}");
    assert_eq!(
        unsafe { ijc_undo_len(h) },
        undo0,
        "preview does not touch the document"
    );

    let done = take(unsafe { ijc_guided_answer_point(h, 5.0, 3.0, 0.0) });
    assert_eq!(done["state"]["active"], false);
    assert_eq!(unsafe { ijc_undo_len(h) }, undo0 + 1, "one undo step");
    assert_matches_typed(h, &done, "offset sel 2 5,3");
    unsafe { ijc_free(h) };
}

#[test]
fn fillet_end_to_end_matches_typed() {
    let h = ijc_init_headless();
    let a = run(h, "line 0,0 10,0")["created"][0]
        .as_str()
        .unwrap()
        .to_string();
    let b = run(h, "line 0,0 0,10")["created"][0]
        .as_str()
        .unwrap()
        .to_string();
    let undo0 = unsafe { ijc_undo_len(h) };

    let s = start(h, "fillet");
    assert_eq!(s["state"]["kind"], "selectObject");
    assert_eq!(s["state"]["filter"], "curve");
    assert_eq!(s["state"]["capturePoint"], true);

    let r = take(unsafe { ijc_guided_answer_object(h, c(&a).as_ptr(), true, 2.0, 0.0, 0.0) });
    assert_eq!(r["status"], "more", "{r}");
    let r = take(unsafe { ijc_guided_answer_object(h, c(&b).as_ptr(), true, 0.0, 2.0, 0.0) });
    assert_eq!(r["state"]["kind"], "number");
    assert_eq!(r["state"]["default"], 0.5);

    let typed = format!("fillet #{a} #{b} 1 at 2,0 0,2 trim yes join no");
    // Pending radius in the field → outcome preview with Trim/Join defaults.
    let p = take(unsafe { ijc_guided_preview_json(h, false, 0.0, 0.0, 0.0, c("1").as_ptr()) });
    assert_eq!(p["command"], typed.as_str());
    assert!(!p["ghosts"].as_array().unwrap().is_empty(), "{p}");

    take(unsafe { ijc_guided_answer_number(h, 1.0) });
    let r = take(unsafe { ijc_guided_answer_default(h) }); // Trim <Yes>
    assert_eq!(r["state"]["kind"], "keyword");
    assert_eq!(r["state"]["options"], serde_json::json!(["No", "Yes"]));
    let done = take(unsafe { ijc_guided_answer_default(h) }); // Join <No>
    assert_eq!(unsafe { ijc_undo_len(h) }, undo0 + 1, "one undo step");
    assert_matches_typed(h, &done, &typed);
    unsafe { ijc_free(h) };
}

#[test]
fn array_end_to_end_matches_typed() {
    let h = ijc_init_headless();
    run(h, "box 0,0,0 1,1,1");
    run(h, "select all");
    let undo0 = unsafe { ijc_undo_len(h) };

    assert_eq!(start(h, "array")["state"]["kind"], "integer");
    take(unsafe { ijc_guided_answer_integer(h, 3) });
    take(unsafe { ijc_guided_answer_default(h) }); // Number in Y <2>
    take(unsafe { ijc_guided_answer_number(h, 2.0) });
    let p = take(unsafe { ijc_guided_preview_json(h, false, 0.0, 0.0, 0.0, c("3").as_ptr()) });
    assert_eq!(p["command"], "array sel 3,2,1 2,3,0");
    assert!(!p["ghosts"].as_array().unwrap().is_empty());

    let done = take(unsafe { ijc_guided_answer_number(h, 3.0) });
    assert_eq!(unsafe { ijc_undo_len(h) }, undo0 + 1, "one undo step");
    assert_matches_typed(h, &done, "array sel 3,2,1 2,3,0");
    unsafe { ijc_free(h) };
}

#[test]
fn back_cancel_and_need_selection() {
    let h = ijc_init_headless();
    run(h, "line 0,0 10,0");
    assert_eq!(start(h, "offset")["status"], "need_selection");
    assert_eq!(start(h, "sphere")["status"], "not_guided");
    assert!(unsafe { ijc_guided_is_verb(c("fillet").as_ptr()) });
    assert!(!unsafe { ijc_guided_is_verb(c("fillet #a #b 1").as_ptr()) });
    let verbs = take(ijc_guided_verbs_json());
    assert!(verbs.as_array().unwrap().len() >= 80, "{verbs}");

    run(h, "select all");
    start(h, "offset");
    let r = take(unsafe { ijc_guided_back(h) });
    assert_eq!(r["status"], "error", "nothing to go back to");
    take(unsafe { ijc_guided_answer_number(h, 4.0) });
    let r = take(unsafe { ijc_guided_back(h) });
    assert_eq!(r["status"], "more");
    assert_eq!(r["state"]["kind"], "number", "re-opened the distance step");
    // Wrong answer kind stays on the step.
    let r = take(unsafe { ijc_guided_answer_point(h, 1.0, 1.0, 0.0) });
    assert_eq!(r["status"], "error");
    assert_eq!(r["state"]["kind"], "number");
    let undo0 = unsafe { ijc_undo_len(h) };
    assert!(unsafe { ijc_guided_cancel(h) });
    assert!(!unsafe { ijc_guided_cancel(h) });
    assert_eq!(unsafe { ijc_undo_len(h) }, undo0, "cancel runs nothing");
    let r = take(unsafe { ijc_guided_answer_default(h) });
    assert_eq!(r["status"], "idle");
    unsafe { ijc_free(h) };
}
