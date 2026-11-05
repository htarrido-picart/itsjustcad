// SPDX-License-Identifier: AGPL-3.0-only
// Copyright © 2026 Hector Tarrido-Picart

//! The host geocoder for the core `geocode "<address>" [#n]` command (#215,
//! itsjustcad-ios#165). The core has no network geocoder on iOS: the app
//! supplies one (MapKit) through a request/answer pair suited to its async
//! API.
//!
//! - [`ijc_set_geocoder`] registers `cb(ctx, query)`. `NULL` removes it; then
//!   `geocode` fails with "no geocoder" and changes nothing.
//! - When a `geocode` command has no answer for its address yet, the core
//!   calls `cb` once (on the thread running the command, inside the call
//!   that runs it) and the command fails with "looking … up with the device
//!   geocoder; location unchanged". The callback must copy `query`, start
//!   the lookup and return at once. It must not call any `ijc_*` function
//!   (the handle is busy then).
//! - When MapKit answers, the app calls [`ijc_geocode_answer`] with the
//!   places (best first, each with its UTC offset from
//!   `CLPlacemark.timeZone`), then runs the same command line again (for
//!   example through `ijc_run_command_result`, keeping `#n`). That run uses
//!   the answer: no second callback. The command applies the logged
//!   `location <lat> <lon> <tz> <place>` op, like a typed `location`.
//! - A successful answer stays cached for the handle's life (a later `#2`
//!   needs no lookup). An error or empty answer is used once.
//! - The callback is never called after [`super::ijc_free`].

use std::collections::{HashMap, HashSet};
use std::ffi::{CStr, CString, c_char, c_void};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use itsjustcad_commands::geocode::{GeoError, GeoPlace, Geocoder, cache_key};

use super::job::lock;
use super::{AppHandle, guard_ffi, handle_mut};

/// `extern fn(ctx, query_utf8)`: look `query` up, answer later with
/// [`ijc_geocode_answer`]. `query` is only valid during the call.
pub type GeocodeCallback = extern "C" fn(*mut c_void, *const c_char);

type Answer = Result<Vec<GeoPlace>, GeoError>;

/// The host's geocoder as the session sees it.
pub(crate) struct HostGeocoder {
    cb: GeocodeCallback,
    /// The host's opaque context, kept as an address so the hook is
    /// `Send + Sync`; only ever handed back to `cb`.
    ctx: usize,
    /// The handle's liveness flag: no callback after `ijc_free`.
    alive: Arc<AtomicBool>,
    answers: Mutex<HashMap<String, Answer>>,
    /// Addresses asked for and not answered yet: asked once only.
    asked: Mutex<HashSet<String>>,
}

impl Geocoder for HostGeocoder {
    fn search(&self, query: &str) -> Answer {
        let key = cache_key(query);
        {
            let mut answers = lock(&self.answers);
            match answers.get(&key) {
                Some(Ok(places)) if !places.is_empty() => return Ok(places.clone()),
                Some(_) => return answers.remove(&key).expect("present"),
                None => {}
            }
        }
        let first = lock(&self.asked).insert(key);
        if first
            && self.alive.load(Ordering::Acquire)
            && let Ok(q) = CString::new(query)
        {
            (self.cb)(self.ctx as *mut c_void, q.as_ptr());
        }
        Err(GeoError::Pending(format!(
            "geocode: looking '{query}' up with the device geocoder; location unchanged \
             (run the command again once it answers)"
        )))
    }
}

/// Install the handle's geocoder (if any) on its current session. Called
/// whenever the handle gets a new session.
pub(crate) fn install(app: &mut AppHandle) {
    let hook = app.geocoder.clone().map(|g| g as Arc<dyn Geocoder>);
    app.session.set_geocoder(hook);
}

/// Parse an answer: an array of `{"name","lat","lon","tzHours","zone"}`
/// (`name`, `tzHours`, `zone` optional), or `{"error":"…"}`.
fn parse_answer(json: &str) -> Option<Answer> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    if let Some(e) = v.get("error") {
        return Some(Err(GeoError::Unreachable(e.as_str()?.to_string())));
    }
    let places = v
        .as_array()?
        .iter()
        .map(|p| {
            Some(GeoPlace {
                name: p["name"].as_str().unwrap_or_default().to_string(),
                lat_deg: p["lat"].as_f64()?,
                lon_deg: p["lon"].as_f64()?,
                tz_hours: p["tzHours"].as_f64(),
                zone: p["zone"].as_str().map(str::to_string),
            })
        })
        .collect::<Option<Vec<_>>>()?;
    Some(Ok(places))
}

/// Register the host geocoder `cb` with its `ctx` (`cb` `NULL` removes it).
/// See the module docs for the request/answer flow. Returns false for a
/// null/invalid/busy handle.
///
/// # Safety
/// `h` must be null or a live handle. `ctx` must stay valid for `cb` until
/// the geocoder is replaced or removed, or the handle is freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ijc_set_geocoder(
    h: *mut AppHandle,
    cb: Option<GeocodeCallback>,
    ctx: *mut c_void,
) -> bool {
    guard_ffi(false, || {
        let Some(mut app) = (unsafe { handle_mut(h) }) else {
            return false;
        };
        app.geocoder = cb.map(|cb| {
            Arc::new(HostGeocoder {
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
/// got): `json` is an array of places, best first,
/// `[{"name":"…","lat":deg,"lon":deg,"tzHours":h,"zone":"America/Guayaquil"}]`
/// (`tzHours` missing: the core guesses `round(lon / 15)`), `[]` for no
/// match, or `{"error":"…"}`. Then run the `geocode` line again. Returns
/// false for a null/invalid/busy handle, no geocoder, or unreadable input.
///
/// # Safety
/// `h` must be null or a live handle; `query` and `json` null or valid
/// NUL-terminated C strings.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ijc_geocode_answer(
    h: *mut AppHandle,
    query: *const c_char,
    json: *const c_char,
) -> bool {
    guard_ffi(false, || {
        if query.is_null() || json.is_null() {
            return false;
        }
        let (Ok(query), Ok(json)) =
            (unsafe { CStr::from_ptr(query) }.to_str(), unsafe { CStr::from_ptr(json) }.to_str())
        else {
            return false;
        };
        let Some(app) = (unsafe { handle_mut(h) }) else {
            return false;
        };
        let (Some(g), Some(answer)) = (app.geocoder.as_ref(), parse_answer(json)) else {
            return false;
        };
        let key = cache_key(query);
        lock(&g.asked).remove(&key);
        lock(&g.answers).insert(key, answer);
        true
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ijc_free, ijc_init_headless, ijc_location_json, ijc_run_command_result, ijc_string_free};

    static ASKED: Mutex<Vec<String>> = Mutex::new(Vec::new());

    extern "C" fn record(ctx: *mut c_void, query: *const c_char) {
        assert_eq!(ctx as usize, 0xC0FFEE);
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

    unsafe fn location(h: *mut AppHandle) -> serde_json::Value {
        let p = unsafe { ijc_location_json(h) };
        let s = unsafe { CStr::from_ptr(p) }.to_str().unwrap().to_owned();
        unsafe { ijc_string_free(p) };
        serde_json::from_str(&s).unwrap()
    }

    #[test]
    fn geocoder_request_answer_round_trip() {
        let h = ijc_init_headless();
        unsafe {
            // No geocoder: a clear error, nothing changed.
            let r = run(h, "geocode Quito");
            assert_eq!(r["ok"], false);
            assert!(r["error"].as_str().unwrap().contains("no geocoder"), "{r}");
            assert!(location(h).is_null());

            assert!(ijc_set_geocoder(h, Some(record), 0xC0FFEE as *mut c_void));
            let r = run(h, "geocode \"Quito\" #2");
            assert_eq!(r["ok"], false);
            assert!(r["error"].as_str().unwrap().contains("device geocoder"), "{r}");
            // Asked once, even when run again before the answer.
            run(h, "geocode quito");
            assert_eq!(*ASKED.lock().unwrap(), vec!["Quito".to_string()]);
            assert!(location(h).is_null());

            let answer = r#"[{"name":"Quito, Ecuador","lat":-0.22,"lon":-78.51,"tzHours":-5,"zone":"America/Guayaquil"},
                             {"name":"Quito, Peru","lat":-12.0,"lon":-77.0,"tzHours":-5}]"#;
            assert!(!ijc_geocode_answer(h, c("Quito").as_ptr(), c("not json").as_ptr()));
            assert!(ijc_geocode_answer(h, c("Quito").as_ptr(), c(answer).as_ptr()));
            let r = run(h, "geocode \"Quito\" #2");
            assert_eq!(r["ok"], true, "{r}");
            assert!(r["message"].as_str().unwrap().contains("#1 Quito, Ecuador"), "{r}");
            let loc = location(h);
            assert_eq!((loc["lat"].as_f64(), loc["tzHours"].as_f64()), (Some(-12.0), Some(-5.0)));
            assert_eq!(loc["place"], "Quito, Peru");
            assert_eq!(ASKED.lock().unwrap().len(), 1, "the answer is reused");

            // Only the `location` op is logged; undo restores no location.
            let log = crate::handle_ref(h).unwrap().session.save_log();
            assert!(matches!(log.as_slice(), [itsjustcad_commands::Command::Location { .. }]), "{log:?}");
            assert_eq!(run(h, "undo")["ok"], true);
            assert!(location(h).is_null());

            // An error answer is reported once, then asked again.
            run(h, "geocode Atlantis");
            assert!(ijc_geocode_answer(h, c("Atlantis").as_ptr(), c(r#"{"error":"no network"}"#).as_ptr()));
            let r = run(h, "geocode Atlantis");
            assert!(r["error"].as_str().unwrap().contains("no network"), "{r}");
            run(h, "geocode Atlantis");
            assert_eq!(ASKED.lock().unwrap().len(), 3);

            // Survives a new document; removed with NULL.
            crate::ijc_new_document(h);
            assert_eq!(run(h, "geocode \"Quito\"")["ok"], true);
            assert!(ijc_set_geocoder(h, None, std::ptr::null_mut()));
            assert!(run(h, "geocode Quito")["error"].as_str().unwrap().contains("no geocoder"));
            ijc_free(h);
        }
    }
}
