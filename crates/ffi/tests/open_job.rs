// SPDX-License-Identifier: AGPL-3.0-only

//! Headless runs of the background open job (#217): the worker replays off
//! the handle, cancel leaves the document alone, take installs the result,
//! and a checkpoint skips the ops it covers.

use std::ffi::{CStr, CString, c_char};
use std::time::{Duration, Instant};

use itsjustcad_commands::{Session, io, parse};
use itsjustcad_ffi::open_job::*;
use itsjustcad_ffi::{
    AppHandle, ijc_checkpoint_json, ijc_doc_generation, ijc_free, ijc_init_headless,
    ijc_render_frame, ijc_run_command_result, ijc_save_json, ijc_scene_digest, ijc_string_free,
    ijc_undo_len,
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

fn run(h: *mut AppHandle, line: &str) {
    let line = CString::new(line).unwrap();
    let v = take_json(unsafe { ijc_run_command_result(h, line.as_ptr()) });
    assert_eq!(v["ok"], true, "{v}");
}

fn digest(h: *mut AppHandle) -> String {
    text(unsafe { ijc_scene_digest(h) })
}

/// A document of `n` boxes, as `.itsjustcad.json`.
fn boxes(n: usize) -> Session {
    let mut s = Session::default();
    for i in 0..n {
        s.run(parse(&format!("box {i},0,0 0.5,0.5,0.5")).unwrap())
            .unwrap();
    }
    s
}

fn start(h: *mut AppHandle, json: &str, cp: Option<&str>) -> u64 {
    let (cp_ptr, cp_len) = cp.map_or((std::ptr::null(), 0), |c| (c.as_ptr(), c.len()));
    let id = unsafe { ijc_open_start(h, json.as_ptr(), json.len(), cp_ptr, cp_len) };
    assert!(id > 0);
    id
}

fn progress(h: *mut AppHandle, id: u64) -> Value {
    take_json(unsafe { ijc_open_progress(h, id) })
}

/// Poll until the job ends; returns every progress seen.
fn wait(h: *mut AppHandle, id: u64) -> Vec<Value> {
    let t0 = Instant::now();
    let mut seen = Vec::new();
    loop {
        let p = progress(h, id);
        let end = !matches!(p["state"].as_str(), Some("running" | "cancelling"));
        seen.push(p);
        if end {
            return seen;
        }
        assert!(
            t0.elapsed() < Duration::from_secs(120),
            "open job never ended"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[test]
fn open_job_progresses_and_take_installs_the_session() {
    let h = ijc_init_headless();
    let src = boxes(300);
    let json = io::to_json(&src);
    let id = start(h, &json, None);
    let seen = wait(h, id);
    let last = seen.last().unwrap();
    assert_eq!(last["state"], "done", "{last}");
    assert_eq!(last["total"], 300);
    assert_eq!(last["done"], 300);
    let dones: Vec<u64> = seen.iter().map(|p| p["done"].as_u64().unwrap()).collect();
    assert!(
        dones.windows(2).all(|w| w[0] <= w[1]),
        "progress only advances"
    );

    let out = take_json(unsafe { ijc_open_take(h, id) });
    assert_eq!(
        out,
        serde_json::json!({ "ok": true, "from_checkpoint": false })
    );
    assert_eq!(unsafe { ijc_undo_len(h) }, 300);
    assert_eq!(text(unsafe { ijc_save_json(h) }), json);
    // Taken: forgotten.
    assert_eq!(text(unsafe { ijc_open_take(h, id) }), "null");
    assert_eq!(text(unsafe { ijc_open_progress(h, id) }), "null");
    unsafe { ijc_free(h) };
}

#[test]
fn take_before_done_is_refused_and_the_handle_stays_usable() {
    let h = ijc_init_headless();
    run(h, "box 100,100,0 1,1,1");
    let before = digest(h);
    let json = io::to_json(&boxes(20000));
    let id = start(h, &json, None);
    assert_eq!(progress(h, id)["state"], "running");
    assert_eq!(
        text(unsafe { ijc_open_take(h, id) }),
        "null",
        "not done yet"
    );

    // The job never holds the handle: rendering, readers and mutating calls
    // all work while it runs, on the current document.
    let gen0 = unsafe { ijc_doc_generation(h) };
    unsafe { ijc_render_frame(h) };
    assert_eq!(digest(h), before);
    assert!(text(unsafe { ijc_save_json(h) }).contains("\"ops\""));
    run(h, "box 200,200,0 1,1,1");
    assert!(unsafe { ijc_doc_generation(h) } > gen0);
    assert_eq!(unsafe { ijc_undo_len(h) }, 2);

    assert_eq!(wait(h, id).last().unwrap()["state"], "done");
    assert_eq!(take_json(unsafe { ijc_open_take(h, id) })["ok"], true);
    assert_eq!(unsafe { ijc_undo_len(h) }, 20000);
    assert_eq!(
        text(unsafe { ijc_open_take(h, 999) }),
        "null",
        "unknown job"
    );
    unsafe { ijc_free(h) };
}

#[test]
fn cancel_stops_the_replay_and_leaves_the_document() {
    let h = ijc_init_headless();
    run(h, "box 100,100,0 1,1,1");
    let before = digest(h);
    let id = start(h, &io::to_json(&boxes(20000)), None);
    assert!(unsafe { ijc_open_cancel(h, id) });
    let last = wait(h, id).pop().unwrap();
    assert_eq!(last["state"], "cancelled", "{last}");
    assert!(last["done"].as_u64().unwrap() < 20000);
    let out = take_json(unsafe { ijc_open_take(h, id) });
    assert_eq!(out["error"], "open: cancelled");
    assert_eq!(digest(h), before, "current document untouched");
    assert_eq!(unsafe { ijc_undo_len(h) }, 1);
    assert!(!unsafe { ijc_open_cancel(h, id) }, "forgotten after take");
    unsafe { ijc_free(h) };
}

#[test]
fn bad_input_fails_the_job() {
    let h = ijc_init_headless();
    let id = unsafe { ijc_open_start(h, std::ptr::null(), 0, std::ptr::null(), 0) };
    assert_eq!(progress(h, id)["state"], "failed");
    let id = start(h, "{not json", None);
    assert_eq!(wait(h, id).last().unwrap()["state"], "failed");
    assert!(take_json(unsafe { ijc_open_take(h, id) })["error"].is_string());
    assert_eq!(
        unsafe { ijc_open_start(std::ptr::null_mut(), b"x".as_ptr(), 1, std::ptr::null(), 0) },
        0
    );
    unsafe { ijc_free(h) };
}

#[test]
fn checkpoint_replays_only_the_newer_ops() {
    // The host saves a checkpoint, edits on, and saves the log again.
    let h = ijc_init_headless();
    for i in 0..5 {
        run(h, &format!("box {i},0,0 1,1,1"));
    }
    let cp = text(unsafe { ijc_checkpoint_json(h) });
    run(h, "box 10,0,0 1,1,1");
    run(h, "box 11,0,0 1,1,1");
    let json = text(unsafe { ijc_save_json(h) });
    let want = digest(h);

    let h2 = ijc_init_headless();
    let id = start(h2, &json, Some(&cp));
    let last = wait(h2, id).pop().unwrap();
    assert_eq!(
        (last["done"].as_u64(), last["total"].as_u64()),
        (Some(2), Some(2)),
        "tail only"
    );
    let out = take_json(unsafe { ijc_open_take(h2, id) });
    assert_eq!(
        out,
        serde_json::json!({ "ok": true, "from_checkpoint": true })
    );
    assert_eq!(digest(h2), want);
    assert_eq!(text(unsafe { ijc_save_json(h2) }), json);

    // A corrupt checkpoint is ignored: full replay, same document.
    let h3 = ijc_init_headless();
    let id = start(h3, &json, Some(&cp[..cp.len() / 2]));
    let last = wait(h3, id).pop().unwrap();
    assert_eq!(last["total"], 7);
    let out = take_json(unsafe { ijc_open_take(h3, id) });
    assert_eq!(out["from_checkpoint"], false);
    assert_eq!(digest(h3), want);
    for h in [h, h2, h3] {
        unsafe { ijc_free(h) };
    }
}
