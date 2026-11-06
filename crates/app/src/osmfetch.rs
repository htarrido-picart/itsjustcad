// SPDX-License-Identifier: AGPL-3.0-only
// Copyright © 2026 Hector Tarrido-Picart

//! The desktop and headless Overpass fetcher for `osmfetch` (#343),
//! installed on the session as the core's host hook
//! ([`itsjustcad_commands::osmfetch::OsmFetcher`]).
//!
//! The core builds the Overpass QL (with explicit `[timeout:…]` and
//! `[maxsize:…]`); this sends it to an Overpass endpoint (the public one by
//! default; `overpass_url` in `ui.json` switches it without an update)
//! through [`crate::download::fetch_bytes`], which sends the app's
//! User-Agent. Usage policy: one request per user action (a typed command,
//! one Deck command or one script line), never retried automatically; a
//! busy server (HTTP 429, or 502 "at capacity") is reported, not hammered.
//!
//! Privacy (#348): the request carries only the site's coordinates and the
//! radius, never the address or the document. The desktop hook is gated:
//! it sends nothing unless the user approved that exact request
//! ([`Overpass::approve`]) in the approval step (typed and Deck alike),
//! which also shows the first-use privacy notice until it is accepted
//! ([`PRIVACY_ACK`] in `ui.json`). Headless, the explicit script line is the
//! approval, and the hook says on stderr what it sends.
//!
//! App crate only: the iOS FFI links none of this (`URLSession` fetches there
//! through `ijc_set_osm_fetcher`).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use itsjustcad_commands::osmfetch::{
    OsmError, OsmFetcher, OsmRequest, DEFAULT_RADIUS_M, MAX_RADIUS_M, MIN_RADIUS_M,
};
use itsjustcad_commands::{Command, Session};

/// The public Overpass API endpoint (the default).
pub const OVERPASS_URL: &str = "https://overpass-api.de/api/interpreter";

/// `ui.json` key: an Overpass-compatible endpoint replacing the public one.
pub const ENDPOINT_KEY: &str = "overpass_url";

/// `ui.json` key: the first-use privacy notice was accepted.
pub const PRIVACY_ACK: &str = "osmfetch_privacy_ack";

/// How long one request may take: the server-side query limit plus margin.
const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(
    itsjustcad_commands::osmfetch::QUERY_TIMEOUT_S as u64 + 30,
);

/// Largest answer accepted (the server-side `maxsize`).
const MAX_BYTES: usize = itsjustcad_commands::osmfetch::QUERY_MAXSIZE as usize;

type EndpointFn = Box<dyn Fn() -> String + Send + Sync>;
type FetchFn = Box<dyn Fn(&str) -> Result<Vec<u8>, String> + Send + Sync>;
type Answer = Result<Vec<u8>, OsmError>;

/// The request URL for `req` on an Overpass-compatible `endpoint`: the QL
/// in the `data` parameter.
pub fn request_url(endpoint: &str, req: &OsmRequest) -> Result<String, String> {
    reqwest::Url::parse_with_params(endpoint, &[("data", req.query.as_str())])
        .map(String::from)
        .map_err(|e| format!("bad Overpass URL '{endpoint}': {e}"))
}

/// The request an `osmfetch` command would send on `session`, or `None`
/// when it fails before sending (no location, a radius out of range) or is
/// not an `osmfetch`: such a command runs at once and reports why.
pub fn request_for(cmd: &Command, session: &Session) -> Option<OsmRequest> {
    let (Command::OsmFetch { radius_m }, Some(loc)) = (cmd, session.doc.location) else {
        return None;
    };
    let r = radius_m.unwrap_or(DEFAULT_RADIUS_M);
    (MIN_RADIUS_M..=MAX_RADIUS_M).contains(&r).then(|| OsmRequest::new(loc.lat_deg, loc.lon_deg, r))
}

/// Save the notice's acceptance in `ui.json` (never from tests: that is the
/// user's real config).
pub fn persist_ack() {
    #[cfg(not(test))]
    {
        let mut v = crate::app::load_ui_json();
        v[PRIVACY_ACK] = serde_json::json!(true);
        crate::app::save_ui_json(&v);
    }
}

/// The approval step's details: centre, radius and where it goes (#348).
pub fn approval_details(req: &OsmRequest, endpoint: &str) -> String {
    use crate::i18n::t;
    format!(
        "{}: {:.5}, {:.5}\n{}: {} m\n{}: {endpoint}",
        t("osmfetch.centre"),
        req.lat_deg,
        req.lon_deg,
        t("osmfetch.radius"),
        req.radius_m,
        t("osmfetch.endpoint"),
    )
}

/// The Overpass host hook: one `fetch` per request, plus a prefetched
/// answer handed once to the command that asked for it.
pub struct Overpass {
    endpoint: EndpointFn,
    fetch: FetchFn,
    /// A [`Self::prefetch`]ed answer, keyed by the request's QL, used once
    /// by the command's own fetch so it does not send a second request.
    stashed: Mutex<Option<(String, Answer)>>,
    /// Every request counts as approved (headless: the script line is the
    /// explicit request).
    auto_approve: bool,
    /// The one request the user approved, by key, used by the next send.
    approved: Mutex<Option<String>>,
    /// The first-use privacy notice was accepted.
    acked: AtomicBool,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Overpass {
    /// A gated hook with injected endpoint and network (tests stub both):
    /// it sends only a request [`Self::approve`]d first, notice not accepted.
    pub fn new(
        endpoint: impl Fn() -> String + Send + Sync + 'static,
        fetch: impl Fn(&str) -> Result<Vec<u8>, String> + Send + Sync + 'static,
    ) -> Self {
        Self {
            endpoint: Box::new(endpoint),
            fetch: Box::new(fetch),
            stashed: Mutex::default(),
            auto_approve: false,
            approved: Mutex::default(),
            acked: AtomicBool::new(false),
        }
    }

    /// The same hook with every request approved (headless).
    fn auto_approved(mut self) -> Self {
        self.auto_approve = true;
        self.acked = AtomicBool::new(true);
        self
    }

    /// The endpoint a request goes to now (shown in the approval step).
    pub fn endpoint(&self) -> String {
        (self.endpoint)()
    }

    /// Whether the first-use privacy notice was accepted.
    pub fn acked(&self) -> bool {
        self.acked.load(Ordering::Acquire)
    }

    /// Record that the privacy notice was accepted (the caller persists it).
    pub fn set_acked(&self) {
        self.acked.store(true, Ordering::Release);
    }

    /// The user approved `req`: the next send of exactly this request may
    /// go out, once.
    pub fn approve(&self, req: &OsmRequest) {
        *lock(&self.approved) = Some(req.key().to_string());
    }

    /// The endpoint `ui.json` configures (the public one by default).
    pub fn configured_endpoint() -> String {
        crate::app::load_ui_json()[ENDPOINT_KEY].as_str().unwrap_or(OVERPASS_URL).to_string()
    }

    /// The desktop app's hook: endpoint from `ui.json`.
    pub fn desktop() -> Self {
        let hook = Self::new(Self::configured_endpoint, http_fetch);
        if crate::app::load_ui_json()[PRIVACY_ACK].as_bool() == Some(true) {
            hook.set_acked();
        }
        hook
    }

    /// The headless CLI's hook: says on stderr what it sends.
    pub fn headless() -> Self {
        Self::new(Self::configured_endpoint, |url: &str| {
            let host = reqwest::Url::parse(url).ok().and_then(|u| u.host_str().map(str::to_string));
            eprintln!(
                "osmfetch: sending the site coordinates and radius to {} (OpenStreetMap Overpass API; data © OpenStreetMap contributors, ODbL 1.0)",
                host.unwrap_or_default()
            );
            http_fetch(url)
        })
        .auto_approved()
    }

    /// Fetch `req` ahead of the command (on a worker thread, so the UI never
    /// waits on the network); the command's fetch then uses the answer.
    pub fn prefetch(&self, req: &OsmRequest) {
        let answer = self.request(req);
        *lock(&self.stashed) = Some((req.key().to_string(), answer));
    }

    /// The only place a request leaves: refused unless approved.
    fn request(&self, req: &OsmRequest) -> Answer {
        let approved = lock(&self.approved).take().is_some_and(|k| k == req.key());
        if !(self.auto_approve || approved) {
            return Err(OsmError::NotApproved);
        }
        let url = request_url(&(self.endpoint)(), req).map_err(OsmError::Unreachable)?;
        let body = (self.fetch)(&url).map_err(|e| OsmError::classify(&e))?;
        if body.len() > MAX_BYTES {
            return Err(OsmError::TooLarge(format!("answer over {} MiB", MAX_BYTES >> 20)));
        }
        Ok(body)
    }
}

impl OsmFetcher for Overpass {
    fn fetch(&self, req: &OsmRequest) -> Answer {
        let stashed = {
            let mut slot = lock(&self.stashed);
            match slot.take() {
                Some((k, answer)) if k == req.key() => Some(answer),
                other => {
                    *slot = other;
                    None
                }
            }
        };
        stashed.unwrap_or_else(|| self.request(req))
    }
}

/// One GET through the shared helper (app User-Agent), on its own thread and
/// runtime so any thread may call it, inside an async context or not.
fn http_fetch(url: &str) -> Result<Vec<u8>, String> {
    let url = url.to_string();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| format!("request failed: {e}"))?;
        rt.block_on(async {
            tokio::time::timeout(TIMEOUT, crate::download::fetch_bytes(&url))
                .await
                .unwrap_or_else(|_| Err(format!("no answer within {} s", TIMEOUT.as_secs())))
        })
    })
    .join()
    .unwrap_or_else(|_| Err("request failed (worker panicked)".into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    const BODY: &[u8] = br#"{"elements":[{"type":"node","id":1,"lat":40.0001,"lon":0.0,"tags":{"natural":"tree"}}]}"#;

    fn counting(answer: Result<&'static [u8], &'static str>) -> (Overpass, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        let c = calls.clone();
        let hook = Overpass::new(
            || "https://overpass.test/api/interpreter".into(),
            move |url: &str| {
                assert!(url.starts_with("https://overpass.test/api/interpreter?data=%5Bout%3Ajson%5D"), "{url}");
                c.fetch_add(1, Ordering::SeqCst);
                answer.map(<[u8]>::to_vec).map_err(str::to_string)
            },
        );
        (hook, calls)
    }

    #[test]
    fn prefetch_then_command_sends_one_request() {
        let (hook, calls) = counting(Ok(BODY));
        let hook = Arc::new(hook);
        let mut s = itsjustcad_commands::Session::default();
        s.run(itsjustcad_commands::parse("location 40 0 0").unwrap()).unwrap();
        s.set_osm_fetcher(Some(hook.clone()));
        let req = OsmRequest::new(40.0, 0.0, 400.0);
        hook.approve(&req);
        hook.prefetch(&req);
        let out = s.run(itsjustcad_commands::parse("osmfetch").unwrap()).unwrap();
        assert!(out.message.contains("1 tree(s) on 'context-trees'"), "{}", out.message);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        // No stash and no approval left: the next command sends nothing.
        let err = s.run(itsjustcad_commands::parse("osmfetch").unwrap()).unwrap_err();
        assert!(err.to_string().contains("needs your approval"), "{err}");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        hook.approve(&req);
        s.run(itsjustcad_commands::parse("osmfetch").unwrap()).unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    /// #348: nothing is sent unless that exact request was approved, and an
    /// approval is used once.
    #[test]
    fn gate_sends_only_the_approved_request_once() {
        let (hook, calls) = counting(Ok(BODY));
        let req = OsmRequest::new(40.0, 0.0, 400.0);
        assert_eq!(hook.fetch(&req), Err(OsmError::NotApproved));
        hook.prefetch(&req);
        assert_eq!(hook.fetch(&req), Err(OsmError::NotApproved));
        hook.approve(&OsmRequest::new(40.0, 0.0, 200.0));
        assert_eq!(hook.fetch(&req), Err(OsmError::NotApproved), "another request's approval");
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        hook.approve(&req);
        assert!(hook.fetch(&req).is_ok());
        assert_eq!(hook.fetch(&req), Err(OsmError::NotApproved), "used once");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(!hook.acked());
        assert!(Overpass::headless().acked(), "headless: the script line is the approval");
    }

    #[test]
    fn transport_errors_are_classified() {
        let req = OsmRequest::new(40.0, 0.0, 400.0);
        for (msg, want) in [
            ("server returned 429 Too Many Requests", "busy"),
            ("server returned 502 Bad Gateway", "busy"),
            ("no answer within 90 s", "timed out"),
            ("request failed: error sending request", "cannot reach"),
        ] {
            let hook = Overpass::new(|| OVERPASS_URL.into(), move |_: &str| Err(msg.to_string()));
            hook.approve(&req);
            let err = hook.fetch(&req).unwrap_err().to_string();
            assert!(err.contains(want), "{msg}: {err}");
        }
    }
}
