// SPDX-License-Identifier: AGPL-3.0-only
// Copyright © 2026 Hector Tarrido-Picart

//! The host Overpass fetcher for the core `osmfetch [radius_m]` command
//! (#343). The core has no HTTP stack on iOS: the app sends the request
//! (`URLSession`) through a request/answer pair, like the geocoder
//! ([`super::geocoder`]).
//!
//! - [`ijc_set_osm_fetcher`] registers `cb(ctx, query)`. `NULL` removes it;
//!   then `osmfetch` fails with "no OpenStreetMap fetcher" and changes
//!   nothing.
//! - When an `osmfetch` has no answer for its request yet, the core calls
//!   `cb` once (inside the call that runs the command) with the Overpass QL
//!   to send, and the command fails with "fetching … with the device;
//!   nothing added". The callback must copy `query`, start the request and
//!   return at once; it must not call any `ijc_*` function. The app sends
//!   `query` as the `data` form field (POST, or GET `?data=`) to its Overpass
//!   endpoint (default `https://overpass-api.de/api/interpreter`) with the
//!   app's User-Agent and a timeout of about 90 s.
//! - When the request finishes, the app calls [`ijc_osm_answer`] with the
//!   same `query` and either the response body (UTF-8 JSON) or an error text
//!   (an HTTP status such as "429" or "504", or "timed out", is classified),
//!   then runs the same command line again. That run uses the answer once:
//!   it logs the generated geometry, never the request.
//! - The callback is never called after [`super::ijc_free`].
//!
//! Privacy and approval (#348): the callback is the only point where
//! anything can leave the device, and the core sends nothing itself. The
//! app MUST NOT send `query` until the user approves it, as on desktop:
//! - First use: a privacy notice saying what is sent (the site's latitude
//!   and longitude and the radius, never the address or the document), to
//!   whom (the configured Overpass endpoint; OpenStreetMap community servers
//!   by default), and that the result is OpenStreetMap data (© OpenStreetMap
//!   contributors, ODbL 1.0, attribution kept). Remember its acceptance.
//! - Every fetch: an approval step showing the centre, the radius and the
//!   endpoint, with Send / Cancel. The centre and radius are the document's
//!   location and the command's radius (both also in `query`, `(around:…)`).
//! - Send: start the request; answer with [`ijc_osm_answer`] as above.
//! - Cancel: send nothing and call [`ijc_osm_cancel`] with the same `query`.
//!   Nothing was added or logged (the command already failed without
//!   changing the document); the next `osmfetch` asks again.

use std::collections::{HashMap, HashSet};
use std::ffi::{CStr, CString, c_char, c_void};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use itsjustcad_commands::osmfetch::{OsmError, OsmFetcher, OsmRequest};

use super::job::lock;
use super::{AppHandle, guard_ffi, handle_mut};

/// `extern fn(ctx, query_utf8)`: send the Overpass QL `query`, answer later
/// with [`ijc_osm_answer`]. `query` is only valid during the call.
pub type OsmFetchCallback = extern "C" fn(*mut c_void, *const c_char);

type Answer = Result<Vec<u8>, OsmError>;

/// The host's fetcher as the session sees it.
pub(crate) struct HostOsmFetcher {
    cb: OsmFetchCallback,
    /// The host's opaque context, kept as an address so the hook is
    /// `Send + Sync`; only ever handed back to `cb`.
    ctx: usize,
    /// The handle's liveness flag: no callback after `ijc_free`.
    alive: Arc<AtomicBool>,
    /// Answers by query, each used once.
    answers: Mutex<HashMap<String, Answer>>,
    /// Queries sent and not answered yet: sent once only.
    asked: Mutex<HashSet<String>>,
}

impl OsmFetcher for HostOsmFetcher {
    fn fetch(&self, req: &OsmRequest) -> Answer {
        if let Some(answer) = lock(&self.answers).remove(req.key()) {
            return answer;
        }
        let first = lock(&self.asked).insert(req.key().to_string());
        if first
            && self.alive.load(Ordering::Acquire)
            && let Ok(q) = CString::new(req.key())
        {
            (self.cb)(self.ctx as *mut c_void, q.as_ptr());
        }
        Err(OsmError::Pending(format!(
            "osmfetch: fetching OpenStreetMap context ({} m) with the device; nothing added \
             (run the command again once it answers)",
            req.radius_m
        )))
    }
}

/// Install the handle's fetcher (if any) on its current session. Called
/// whenever the handle gets a new session.
pub(crate) fn install(app: &mut AppHandle) {
    let hook = app.osm_fetcher.clone().map(|f| f as Arc<dyn OsmFetcher>);
    app.session.set_osm_fetcher(hook);
}

/// Register the host fetcher `cb` with its `ctx` (`cb` `NULL` removes it).
/// See the module docs for the request/answer flow. Returns false for a
/// null/invalid/busy handle.
///
/// # Safety
/// `h` must be null or a live handle. `ctx` must stay valid for `cb` until
/// the fetcher is replaced or removed, or the handle is freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ijc_set_osm_fetcher(
    h: *mut AppHandle,
    cb: Option<OsmFetchCallback>,
    ctx: *mut c_void,
) -> bool {
    guard_ffi(false, || {
        let Some(mut app) = (unsafe { handle_mut(h) }) else {
            return false;
        };
        app.osm_fetcher = cb.map(|cb| {
            Arc::new(HostOsmFetcher {
                cb,
                ctx: ctx as usize,
                alive: app.alive.clone(),
                answers: Mutex::default(),
                asked: Mutex::default(),
            })
        });
        install(&mut app);
        true
    })
}

/// Hand the core the host's answer for `query` (the string the callback
/// got): `body`, the Overpass response (UTF-8 JSON), or `error`, why the
/// request failed (exactly one non-null). Then run the `osmfetch` line
/// again. Returns false for a null/invalid/busy handle, no fetcher, or bad
/// input.
///
/// # Safety
/// `h` must be null or a live handle; `query`, `body` and `error` null or
/// valid NUL-terminated C strings.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ijc_osm_answer(
    h: *mut AppHandle,
    query: *const c_char,
    body: *const c_char,
    error: *const c_char,
) -> bool {
    guard_ffi(false, || {
        if query.is_null() || body.is_null() == error.is_null() {
            return false;
        }
        let Ok(query) = unsafe { CStr::from_ptr(query) }.to_str() else {
            return false;
        };
        let answer = if body.is_null() {
            match unsafe { CStr::from_ptr(error) }.to_str() {
                Ok(e) => Err(OsmError::classify(e)),
                Err(_) => return false,
            }
        } else {
            Ok(unsafe { CStr::from_ptr(body) }.to_bytes().to_vec())
        };
        let Some(app) = (unsafe { handle_mut(h) }) else {
            return false;
        };
        let Some(f) = app.osm_fetcher.as_ref() else {
            return false;
        };
        lock(&f.asked).remove(query);
        lock(&f.answers).insert(query.to_string(), answer);
        true
    })
}

/// The user cancelled the approval step for `query` (the string the
/// callback got): nothing was sent. Forget the request, so the next
/// `osmfetch` calls the callback (and so the approval) again. Returns false
/// for a null/invalid/busy handle, no fetcher, or bad input.
///
/// # Safety
/// `h` must be null or a live handle; `query` null or a valid
/// NUL-terminated C string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ijc_osm_cancel(h: *mut AppHandle, query: *const c_char) -> bool {
    guard_ffi(false, || {
        if query.is_null() {
            return false;
        }
        let Ok(query) = unsafe { CStr::from_ptr(query) }.to_str() else {
            return false;
        };
        let Some(app) = (unsafe { handle_mut(h) }) else {
            return false;
        };
        let Some(f) = app.osm_fetcher.as_ref() else {
            return false;
        };
        lock(&f.asked).remove(query);
        lock(&f.answers).remove(query);
        true
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ijc_free, ijc_init_headless, ijc_run_command_result, ijc_string_free};

    static ASKED: Mutex<Vec<String>> = Mutex::new(Vec::new());

    extern "C" fn record(ctx: *mut c_void, query: *const c_char) {
        assert_eq!(ctx as usize, 0xBEEF);
        let q = unsafe { CStr::from_ptr(query) }.to_str().unwrap().to_string();
        ASKED.lock().unwrap().push(q);
    }

    fn c(s: &str) -> CString {
        CString::new(s).unwrap()
    }

    unsafe fn run(h: *mut AppHandle, line: &str) -> serde_json::Value {
        let p = unsafe { ijc_run_command_result(h, c(line).as_ptr()) };
        let s = unsafe { CStr::from_ptr(p) }.to_str().unwrap().to_owned();
        unsafe { ijc_string_free(p) };
        serde_json::from_str(&s).unwrap()
    }

    #[test]
    fn osm_fetcher_request_answer_round_trip() {
        const TREE: &str = r#"{"elements":[{"type":"node","id":1,"lat":40.0001,"lon":0.0,"tags":{"natural":"tree"}}]}"#;
        let h = ijc_init_headless();
        unsafe {
            run(h, "location 40 0 0");
            let r = run(h, "osmfetch");
            assert!(r["error"].as_str().unwrap().contains("no OpenStreetMap fetcher"), "{r}");

            assert!(ijc_set_osm_fetcher(h, Some(record), 0xBEEF as *mut c_void));
            let r = run(h, "osmfetch");
            assert_eq!(r["ok"], false);
            assert!(r["error"].as_str().unwrap().contains("with the device"), "{r}");
            run(h, "osmfetch");
            let asked = ASKED.lock().unwrap().clone();
            assert_eq!(asked.len(), 1, "sent once");
            assert!(asked[0].starts_with("[out:json]") && asked[0].contains("(around:400,"), "{}", asked[0]);

            let q = c(&asked[0]);
            assert!(!ijc_osm_answer(h, q.as_ptr(), std::ptr::null(), std::ptr::null()), "one of body/error");
            assert!(ijc_osm_answer(h, q.as_ptr(), c(TREE).as_ptr(), std::ptr::null()));
            let r = run(h, "osmfetch");
            assert_eq!(r["ok"], true, "{r}");
            assert!(r["message"].as_str().unwrap().contains("1 tree(s)"), "{r}");
            let log = crate::handle_ref(h).unwrap().session.save_log();
            assert!(matches!(log.last(), Some(itsjustcad_commands::Command::OsmContext { .. })), "{log:?}");
            assert_eq!(run(h, "undo")["ok"], true);
            assert_eq!(crate::handle_ref(h).unwrap().session.doc.len(), 0);

            // Cancel (#348): nothing added or logged; the next run asks again.
            let n_log = crate::handle_ref(h).unwrap().session.save_log().len();
            run(h, "osmfetch");
            assert_eq!(ASKED.lock().unwrap().len(), 2, "asked again after the answer was used");
            assert!(!ijc_osm_cancel(h, std::ptr::null()));
            assert!(ijc_osm_cancel(h, q.as_ptr()));
            assert_eq!(crate::handle_ref(h).unwrap().session.save_log().len(), n_log, "cancel logs nothing");
            run(h, "osmfetch");
            assert_eq!(ASKED.lock().unwrap().len(), 3, "the next osmfetch asks again");
            assert!(ijc_osm_cancel(h, q.as_ptr()));

            // An error answer is used once and classified.
            run(h, "osmfetch");
            assert!(ijc_osm_answer(h, q.as_ptr(), std::ptr::null(), c("HTTP 504").as_ptr()));
            let r = run(h, "osmfetch");
            assert!(r["error"].as_str().unwrap().contains("timed out"), "{r}");

            crate::ijc_new_document(h);
            assert!(crate::handle_ref(h).unwrap().session.has_osm_fetcher(), "kept across new documents");
            assert!(ijc_set_osm_fetcher(h, None, std::ptr::null_mut()));
            assert!(!crate::handle_ref(h).unwrap().session.has_osm_fetcher());
            ijc_free(h);
        }
    }
}
