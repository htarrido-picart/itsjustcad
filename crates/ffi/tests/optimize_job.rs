// SPDX-License-Identifier: AGPL-3.0-only

//! Headless runs of the background site optimizer job (#237): a late user
//! Cancel discards the result, and a job never writes into the document that
//! replaced the one it ran on.

use std::ffi::{CStr, CString, c_char};
use std::time::{Duration, Instant};

use itsjustcad_ffi::optimize_job::*;
use itsjustcad_ffi::{
    AppHandle, ijc_free, ijc_init_headless, ijc_new_document, ijc_open_json,
    ijc_run_command_result, ijc_save_json, ijc_string_free,
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

fn run(h: *mut AppHandle, line: &str) -> Value {
    let line = CString::new(line).unwrap();
    let v = take_json(unsafe { ijc_run_command_result(h, line.as_ptr()) });
    assert_eq!(v["ok"], true, "{v}");
    v
}

/// A site boundary with a site plan; returns its short id.
fn site(h: *mut AppHandle) -> CString {
    let v = run(h, "rect 0,0,0 200 150");
    run(h, "lotsettings depth=20");
    run(h, "siteplan last");
    CString::new(v["created"][0].as_str().unwrap()).unwrap()
}

fn saved(h: *mut AppHandle, site: &CString) -> Value {
    take_json(unsafe { ijc_site_optimize_saved_json(h, site.as_ptr()) })
}

fn progress(h: *mut AppHandle, id: u64) -> Value {
    take_json(unsafe { ijc_site_optimize_progress(h, id) })
}

fn start(h: *mut AppHandle, site: &CString, request: &str) -> u64 {
    let req = CString::new(request).unwrap();
    let id = unsafe { ijc_site_optimize_start(h, site.as_ptr(), req.as_ptr()) };
    assert!(id > 0);
    id
}

/// Poll until the job ends; returns its last progress.
fn wait(h: *mut AppHandle, id: u64) -> Value {
    let t0 = Instant::now();
    loop {
        let p = progress(h, id);
        if !matches!(
            p["state"].as_str(),
            Some("running" | "cancelling" | "stopping")
        ) {
            return p;
        }
        assert!(t0.elapsed() < Duration::from_secs(120), "{p}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Request a checkpoint and poll until it is saved.
fn checkpoint(h: *mut AppHandle, id: u64) {
    assert!(unsafe { ijc_site_optimize_checkpoint(h, id) });
    let t0 = Instant::now();
    while !progress(h, id)["checkpoint_at"].is_u64() {
        assert!(t0.elapsed() < Duration::from_secs(120));
        std::thread::sleep(Duration::from_millis(5));
    }
}

const BIG: &str = r#"{"objective":"lots","budget":5000}"#;

/// A saved run from a small finished search.
fn save_a_run(h: *mut AppHandle, site: &CString) -> Value {
    let id = start(h, site, r#"{"objective":"lots","budget":8,"seed":1}"#);
    assert_eq!(wait(h, id)["state"], "done");
    let got = take_json(unsafe { ijc_site_optimize_result(h, id) });
    assert_eq!(got["saved"], true, "{got}");
    saved(h, site)
}

#[test]
fn late_cancel_discards_the_result_and_restores_the_run() {
    let h = ijc_init_headless();
    let site = site(h);
    let before = save_a_run(h, &site);

    // Finished without a checkpoint, then cancelled.
    let id = start(h, &site, r#"{"objective":"lots","budget":12,"seed":2}"#);
    assert_eq!(wait(h, id)["state"], "done");
    assert!(unsafe { ijc_site_optimize_cancel(h, id) });
    assert_eq!(progress(h, id)["state"], "cancelled");
    let out = take_json(unsafe { ijc_site_optimize_result(h, id) });
    assert_eq!(out["error"], "siteoptimize: cancelled", "{out}");
    assert_eq!(saved(h, &site), before);

    // Checkpointed, finished (stopped), then cancelled: the run before it.
    let id = start(h, &site, BIG);
    checkpoint(h, id);
    assert_ne!(saved(h, &site), before);
    assert!(unsafe { ijc_site_optimize_stop(h, id) });
    assert_eq!(wait(h, id)["state"], "done");
    assert!(unsafe { ijc_site_optimize_cancel(h, id) });
    assert_eq!(saved(h, &site), before, "cancel restores");
    let out = take_json(unsafe { ijc_site_optimize_result(h, id) });
    assert_eq!(out["error"], "siteoptimize: cancelled", "{out}");
    assert_eq!(saved(h, &site), before);
    unsafe { ijc_free(h) };
}

#[test]
fn reopening_the_document_cancels_its_jobs() {
    let h = ijc_init_headless();
    let site = site(h);
    let before = save_a_run(h, &site);
    let file = text(unsafe { ijc_save_json(h) });

    // A checkpoint replaces the run, then the same file is reopened.
    let id = start(h, &site, BIG);
    checkpoint(h, id);
    assert_ne!(saved(h, &site), before);
    assert!(unsafe { ijc_open_json(h, file.as_ptr(), file.len()) });
    assert_eq!(saved(h, &site), before, "the reopened run");
    // Nothing the old job does reaches the reopened document.
    unsafe { ijc_site_optimize_checkpoint(h, id) };
    progress(h, id);
    assert!(unsafe { ijc_site_optimize_stop(h, id) });
    assert_eq!(wait(h, id)["state"], "cancelled");
    assert!(unsafe { ijc_site_optimize_cancel(h, id) });
    let out = take_json(unsafe { ijc_site_optimize_result(h, id) });
    assert_eq!(out["error"], "siteoptimize: cancelled", "{out}");
    assert_eq!(saved(h, &site), before);
    unsafe { ijc_free(h) };
}

#[test]
fn new_document_cancels_its_jobs() {
    let h = ijc_init_headless();
    let site = site(h);
    let before = save_a_run(h, &site);
    let file = text(unsafe { ijc_save_json(h) });
    let id = start(h, &site, BIG);
    checkpoint(h, id);
    unsafe { ijc_new_document(h) };
    assert_eq!(wait(h, id)["state"], "cancelled");
    // The old document comes back with its run; the job never touches it.
    assert!(unsafe { ijc_open_json(h, file.as_ptr(), file.len()) });
    assert_eq!(saved(h, &site), before);
    let out = take_json(unsafe { ijc_site_optimize_result(h, id) });
    assert_eq!(out["error"], "siteoptimize: cancelled", "{out}");
    assert_eq!(saved(h, &site), before);
    unsafe { ijc_free(h) };
}
