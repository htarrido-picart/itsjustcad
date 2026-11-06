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
//! Privacy: the request carries only the site's coordinates and the
//! radius, never the address or the document. Typing `osmfetch` is the
//! explicit request (its usage text says it sends the coordinates), and a
//! Deck-proposed `osmfetch` is side-effecting, so the Deck asks before it
//! runs; there is no separate first-use notice (unlike `geocode`, which
//! sends the address the user typed).
//!
//! App crate only: the iOS FFI links none of this (`URLSession` fetches there
//! through `ijc_set_osm_fetcher`).

use std::sync::Mutex;

use itsjustcad_commands::osmfetch::{OsmError, OsmFetcher, OsmRequest};

/// The public Overpass API endpoint (the default).
pub const OVERPASS_URL: &str = "https://overpass-api.de/api/interpreter";

/// `ui.json` key: an Overpass-compatible endpoint replacing the public one.
pub const ENDPOINT_KEY: &str = "overpass_url";

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

/// The Overpass host hook: one `fetch` per request, plus a prefetched
/// answer handed once to the command that asked for it.
pub struct Overpass {
    endpoint: EndpointFn,
    fetch: FetchFn,
    /// A [`Self::prefetch`]ed answer, keyed by the request's QL, used once
    /// by the command's own fetch so it does not send a second request.
    stashed: Mutex<Option<(String, Answer)>>,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Overpass {
    /// A hook with injected endpoint and network (tests stub both).
    pub fn new(
        endpoint: impl Fn() -> String + Send + Sync + 'static,
        fetch: impl Fn(&str) -> Result<Vec<u8>, String> + Send + Sync + 'static,
    ) -> Self {
        Self { endpoint: Box::new(endpoint), fetch: Box::new(fetch), stashed: Mutex::default() }
    }

    fn configured_endpoint() -> String {
        crate::app::load_ui_json()[ENDPOINT_KEY].as_str().unwrap_or(OVERPASS_URL).to_string()
    }

    /// The desktop app's hook: endpoint from `ui.json`.
    pub fn desktop() -> Self {
        Self::new(Self::configured_endpoint, http_fetch)
    }

    /// The headless CLI's hook: says on stderr what it sends.
    pub fn headless() -> Self {
        Self::new(Self::configured_endpoint, |url: &str| {
            let host = reqwest::Url::parse(url).ok().and_then(|u| u.host_str().map(str::to_string));
            eprintln!(
                "osmfetch: sending the site coordinates to {} (OpenStreetMap Overpass API)",
                host.unwrap_or_default()
            );
            http_fetch(url)
        })
    }

    /// Fetch `req` ahead of the command (on a worker thread, so the UI never
    /// waits on the network); the command's fetch then uses the answer.
    pub fn prefetch(&self, req: &OsmRequest) {
        let answer = self.request(req);
        *lock(&self.stashed) = Some((req.key().to_string(), answer));
    }

    fn request(&self, req: &OsmRequest) -> Answer {
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
        hook.prefetch(&OsmRequest::new(40.0, 0.0, 400.0));
        let out = s.run(itsjustcad_commands::parse("osmfetch").unwrap()).unwrap();
        assert!(out.message.contains("1 tree(s) on 'context-trees'"), "{}", out.message);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        // No stash left: the next command sends its own request.
        s.run(itsjustcad_commands::parse("osmfetch").unwrap()).unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 2);
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
            let err = hook.fetch(&req).unwrap_err().to_string();
            assert!(err.contains(want), "{msg}: {err}");
        }
    }
}
