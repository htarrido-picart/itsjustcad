// SPDX-License-Identifier: AGPL-3.0-only

//! Headless runs of the background undo-history rebuild (#224): after a fast
//! open, edits need no rebuild, undo past the open boundary is refused until
//! the rebuild is taken, and edits made while it runs are kept.

use std::ffi::{CStr, CString, c_char};
use std::time::{Duration, Instant};

use itsjustcad_commands::{Session, io, parse};
use itsjustcad_ffi::history_job::*;
use itsjustcad_ffi::open_job::{ijc_open_start, ijc_open_take};
use itsjustcad_ffi::{
    AppHandle, ijc_free, ijc_init_headless, ijc_save_json, ijc_scene_digest, ijc_string_free,
    ijc_undo_cursor, ijc_undo_len,
};
use serde_json::Value;

fn text(p: *mut c_char) -> String {
    assert!(!p.is_null());
    let s = unsafe { CStr::from_ptr(p) }.to_str().unwrap().to_string();
    unsafe { ijc_string_free(p) };
    s
}

fn take_json(p: *mut c_char) -> Value {
    serde_json::from_str(&text(p)).unwrap()
}

fn run_result(h: *mut AppHandle, line: &str) -> Value {
    let line = CString::new(line).unwrap();
    take_json(unsafe { itsjustcad_ffi::ijc_run_command_result(h, line.as_ptr()) })
}

fn run(h: *mut AppHandle, line: &str) {
    let v = run_result(h, line);
    assert_eq!(v["ok"], true, "{v}");
}

fn digest(h: *mut AppHandle) -> String {
    text(unsafe { ijc_scene_digest(h) })
}

/// [`digest`] without object ids (they differ between two builds).
fn shape(h: *mut AppHandle) -> String {
    digest(h)
        .lines()
        .map(|l| {
            l.split(' ')
                .enumerate()
                .filter(|(i, _)| *i != 2)
                .map(|(_, w)| w)
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Poll until the job ends; returns every progress seen.
fn wait(h: *mut AppHandle, id: u64) -> Vec<Value> {
    let t0 = Instant::now();
    let mut seen = Vec::new();
    loop {
        let p = take_json(unsafe { ijc_history_progress(h, id) });
        let end = !matches!(p["state"].as_str(), Some("running" | "cancelling"));
        seen.push(p);
        if end {
            return seen;
        }
        assert!(t0.elapsed() < Duration::from_secs(120), "job never ended");
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// A handle with `n` boxes, its state digests after each op, and a second
/// handle fast-opened from its checkpoint.
fn fast_opened(n: usize) -> (*mut AppHandle, Vec<String>, String) {
    let mut src = Session::default();
    for i in 0..n {
        src.run(parse(&format!("box {i},0,0 0.5,0.5,0.5")).unwrap())
            .unwrap();
    }
    let cp = io::checkpoint_json(&src);
    let json = io::to_json(&src);
    // Digests of the states after 0..=n ops (small documents only).
    let mut digests = Vec::new();
    if n <= 50 {
        let h = ijc_init_headless();
        digests.push(shape(h));
        for i in 0..n {
            run(h, &format!("box {i},0,0 0.5,0.5,0.5"));
            digests.push(shape(h));
        }
        unsafe { ijc_free(h) };
    }

    let h = ijc_init_headless();
    let id = unsafe { ijc_open_start(h, json.as_ptr(), json.len(), cp.as_ptr(), cp.len()) };
    let t0 = Instant::now();
    let out = loop {
        let out = take_json(unsafe { ijc_open_take(h, id) });
        if !out.is_null() {
            break out;
        }
        assert!(t0.elapsed() < Duration::from_secs(120));
        std::thread::sleep(Duration::from_millis(1));
    };
    assert_eq!(
        out,
        serde_json::json!({ "ok": true, "from_checkpoint": true })
    );
    (h, digests, json)
}

#[test]
fn edits_and_their_undo_need_no_rebuild_then_take_reaches_pre_open_states() {
    let (h, digests, _) = fast_opened(5);
    assert!(unsafe { ijc_history_pending(h) });
    run(h, "box 20,20,0 1,1,1");
    run(h, "box 30,20,0 1,1,1");
    assert!(unsafe { ijc_history_pending(h) }, "edits did not rebuild");
    assert_eq!(unsafe { ijc_undo_len(h) }, 7);
    run(h, "undo");
    run(h, "redo");
    run(h, "undo");
    run(h, "undo");
    assert_eq!(shape(h), digests[5]);

    // Past the boundary: refused, document untouched.
    let v = run_result(h, "undo");
    assert_eq!(v["ok"], false);
    assert_eq!(v["error"], "Preparing undo history…");
    assert_eq!(shape(h), digests[5]);

    let id = unsafe { ijc_history_start(h) };
    assert!(id > 0);
    assert_eq!(unsafe { ijc_history_start(h) }, id, "running job reused");
    let last = wait(h, id).pop().unwrap();
    assert_eq!(last["state"], "done", "{last}");
    assert_eq!(
        (last["done"].as_u64(), last["total"].as_u64()),
        (Some(5), Some(5))
    );
    let out = take_json(unsafe { ijc_history_take(h, id) });
    assert_eq!(out, serde_json::json!({ "ok": true, "installed": true }));
    assert!(!unsafe { ijc_history_pending(h) });
    assert_eq!(unsafe { ijc_history_start(h) }, 0, "nothing to rebuild");
    assert_eq!(
        text(unsafe { ijc_history_take(h, id) }),
        "null",
        "forgotten"
    );

    for k in (0..5).rev() {
        run(h, "undo");
        assert_eq!(shape(h), digests[k], "state after {k} ops");
    }
    assert_eq!(unsafe { ijc_undo_cursor(h) }, 0);
    for _ in 0..7 {
        run(h, "redo");
    }
    assert_eq!(unsafe { ijc_undo_cursor(h) }, 7);
    unsafe { ijc_free(h) };
}

#[test]
fn edit_while_the_rebuild_runs_is_kept() {
    let (h, _, json) = fast_opened(3000);
    let opened = digest(h);
    let id = unsafe { ijc_history_start(h) };
    assert!(id > 0);
    // The worker never holds the handle: edits go through meanwhile.
    run(h, "box 9000,0,0 1,1,1");
    let edited = digest(h);
    assert_eq!(wait(h, id).last().unwrap()["state"], "done");
    assert_eq!(
        take_json(unsafe { ijc_history_take(h, id) })["installed"],
        true
    );
    assert_eq!(digest(h), edited, "install keeps the document");
    assert_eq!(unsafe { ijc_undo_len(h) }, 3001);
    run(h, "undo");
    assert_eq!(digest(h), opened);
    run(h, "undo");
    assert_eq!(unsafe { ijc_undo_cursor(h) }, 2999);
    // The forward log is still the opened file's (the edit was undone).
    run(h, "redo");
    assert_eq!(text(unsafe { ijc_save_json(h) }), json);
    unsafe { ijc_free(h) };
}

#[test]
fn cancel_keeps_the_history_pending() {
    let (h, _, _) = fast_opened(20000);
    let id = unsafe { ijc_history_start(h) };
    assert!(unsafe { ijc_history_cancel(h, id) });
    assert_eq!(wait(h, id).pop().unwrap()["state"], "cancelled");
    let out = take_json(unsafe { ijc_history_take(h, id) });
    assert_eq!(out["error"], "history: cancelled");
    assert!(unsafe { ijc_history_pending(h) });
    assert!(
        !unsafe { ijc_history_cancel(h, id) },
        "forgotten after take"
    );
    assert_eq!(unsafe { ijc_history_start(std::ptr::null_mut()) }, 0);
    unsafe { ijc_free(h) };
}

#[test]
fn a_new_open_makes_a_running_rebuild_stale() {
    let (h, _, json) = fast_opened(5);
    let id = unsafe { ijc_history_start(h) };
    assert_eq!(wait(h, id).last().unwrap()["state"], "done");
    // Another open replaces the session before the take.
    let id2 = unsafe { ijc_open_start(h, json.as_ptr(), json.len(), std::ptr::null(), 0) };
    let t0 = Instant::now();
    while take_json(unsafe { ijc_open_take(h, id2) }).is_null() {
        assert!(t0.elapsed() < Duration::from_secs(60));
        std::thread::sleep(Duration::from_millis(1));
    }
    let out = take_json(unsafe { ijc_history_take(h, id) });
    assert_eq!(out, serde_json::json!({ "ok": true, "installed": false }));
    unsafe { ijc_free(h) };
}
