// SPDX-License-Identifier: AGPL-3.0-only
// Copyright © 2026 Hector Tarrido-Picart

//! The desktop and headless geocoder for `geocode` (#215): OpenStreetMap
//! Nominatim, installed on the session as the core's host hook
//! ([`itsjustcad_commands::geocode::Geocoder`]), plus the Daylight section's
//! address-field state.
//!
//! The address goes to a Nominatim search endpoint (the public one by
//! default; `geocoder_url` in `ui.json` switches it without an update)
//! through [`crate::download::fetch_bytes`], which sends the app's
//! User-Agent. Usage policy: one request per user action (a button press, a
//! typed command or one Deck command, never per keystroke), answers cached
//! for the session so a repeated search sends nothing, and the attribution
//! shown with the result. On desktop nothing is sent until the first-use
//! privacy notice is accepted.
//!
//! The UTC offset comes offline from the coordinates ([`tz_offset`]). App
//! crate only: the iOS FFI does not link any of this (MapKit geocodes there
//! through `ijc_set_geocoder`, itsjustcad-ios#165).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use itsjustcad_commands::geocode::{cache_key, GeoError, GeoPlace, Geocoder};

/// The public Nominatim search endpoint (the default geocoder).
pub const NOMINATIM_URL: &str = "https://nominatim.openstreetmap.org/search";

/// Data attribution the Nominatim usage policy asks us to show.
pub const ATTRIBUTION: &str = "© OpenStreetMap contributors";

/// `ui.json` key: the first-use privacy notice was accepted.
pub const PRIVACY_ACK: &str = "geocode_privacy_ack";
/// `ui.json` key: a Nominatim-compatible search endpoint replacing the public
/// one (the usage policy asks that apps can switch without an update).
pub const ENDPOINT_KEY: &str = "geocoder_url";

/// Matches asked for per search: enough to pick from, short enough to read.
const LIMIT: &str = "5";

/// How long one request may take before it counts as unreachable.
const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

/// The error when the gate refuses because the notice is not accepted yet.
pub const NOT_SENT: &str = "geocode: the address was not sent: accept the privacy notice \
    (it is open now), then run the command again; location unchanged";

/// The search URL for `query` on a Nominatim-compatible `endpoint`.
pub fn search_url(endpoint: &str, query: &str) -> Result<String, String> {
    reqwest::Url::parse_with_params(
        endpoint,
        &[("q", query), ("format", "jsonv2"), ("limit", LIMIT)],
    )
    .map(String::from)
    .map_err(|e| format!("bad geocoder URL '{endpoint}': {e}"))
}

/// Parse a Nominatim `jsonv2` search answer: an array of objects whose `lat`
/// and `lon` are decimal strings. Entries without usable coordinates are
/// skipped. The offsets are left unset ([`Nominatim`] fills them).
pub fn parse_results(body: &[u8]) -> Result<Vec<GeoPlace>, String> {
    let v: serde_json::Value =
        serde_json::from_slice(body).map_err(|e| format!("unreadable answer: {e}"))?;
    let items = v.as_array().ok_or("unexpected answer (not a list)")?;
    let coord = |item: &serde_json::Value, key: &str| -> Option<f64> {
        match &item[key] {
            serde_json::Value::String(s) => s.parse().ok(),
            n => n.as_f64(),
        }
    };
    Ok(items
        .iter()
        .filter_map(|item| {
            let lat = coord(item, "lat")?;
            let lon = coord(item, "lon")?;
            ((-90.0..=90.0).contains(&lat) && (-180.0..=180.0).contains(&lon)).then(|| {
                let name = item["display_name"].as_str().unwrap_or_default();
                // One line: the name is logged as trailing words of `location`.
                let name = name.split_whitespace().collect::<Vec<_>>().join(" ");
                GeoPlace { name, lat_deg: lat, lon_deg: lon, tz_hours: None, zone: None }
            })
        })
        .collect())
}

/// A site's UTC offset.
#[derive(Clone, Debug, PartialEq)]
pub struct TzOffset {
    /// Hours east of UTC on the date asked for (DST included).
    pub hours: f64,
    /// The IANA zone it came from; `None` when it is the `round(lon / 15)`
    /// fallback.
    pub zone: Option<String>,
}

/// The UTC offset at `lat`/`lon` on `date` (noon local), offline: the zone
/// from tzf-rs' bundled boundaries, its rules from the tz database. Falls back
/// to `round(lon / 15)` when no named zone is known there (open sea, or a zone
/// missing from the system database).
pub fn tz_offset(lat: f64, lon: f64, date: jiff::civil::Date) -> TzOffset {
    use std::sync::OnceLock;
    static FINDER: OnceLock<tzf_rs::DefaultFinder> = OnceLock::new();
    let finder = FINDER.get_or_init(tzf_rs::DefaultFinder::new);
    let name = finder.get_tz_name(lon, lat);
    let named = (!name.is_empty() && !name.starts_with("Etc/"))
        .then(|| jiff::tz::TimeZone::get(name).ok())
        .flatten()
        .and_then(|tz| date.at(12, 0, 0, 0).to_zoned(tz).ok());
    match named {
        Some(z) => TzOffset {
            hours: f64::from(z.offset().seconds()) / 3600.0,
            zone: Some(name.to_string()),
        },
        None => TzOffset { hours: (lon / 15.0).round(), zone: None },
    }
}

/// The address in `geocode <address>`: the rest of the line, one pair of
/// surrounding quotes dropped.
pub fn address_arg(rest: &str) -> String {
    let rest = rest.trim();
    rest.strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .unwrap_or(rest)
        .trim()
        .to_string()
}

/// What a lookup may send, read once per lookup.
pub struct Settings {
    /// The privacy notice was accepted (always true headless: a script line
    /// is the user's explicit request).
    pub acked: bool,
    /// The Nominatim-compatible search endpoint.
    pub endpoint: String,
}

type SettingsFn = Box<dyn Fn() -> Settings + Send + Sync>;
type FetchFn = Box<dyn Fn(&str) -> Result<Vec<u8>, String> + Send + Sync>;
type Answer = Result<Vec<GeoPlace>, GeoError>;

/// The Nominatim host hook: one `fetch` per lookup, a session cache, the
/// privacy gate, and the offset for the Daylight date in use.
pub struct Nominatim {
    settings: SettingsFn,
    fetch: FetchFn,
    /// The Daylight section's custom (month, day), for the offset's DST;
    /// `None` = today.
    custom_day: Mutex<Option<(u8, u8)>>,
    cache: Mutex<HashMap<String, Vec<GeoPlace>>>,
    /// A [`Self::prefetch`]ed answer that is not cached (a failure, no
    /// match), handed once to the next search of the same address so the
    /// command does not send a second request.
    stashed: Mutex<Option<(String, Answer)>>,
    /// An address the gate refused because the notice is not accepted yet;
    /// the app shows the notice ([`Self::take_notice_request`]).
    notice_wanted: Mutex<Option<String>>,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Nominatim {
    /// A hook with injected settings and network (tests stub both).
    pub fn new(
        settings: impl Fn() -> Settings + Send + Sync + 'static,
        fetch: impl Fn(&str) -> Result<Vec<u8>, String> + Send + Sync + 'static,
    ) -> Self {
        Self {
            settings: Box::new(settings),
            fetch: Box::new(fetch),
            custom_day: Mutex::default(),
            cache: Mutex::default(),
            stashed: Mutex::default(),
            notice_wanted: Mutex::default(),
        }
    }

    /// The desktop app's hook: settings from `ui.json`, gated on the notice.
    pub fn desktop() -> Self {
        Self::new(
            || {
                let prefs = crate::app::load_ui_json();
                Settings {
                    acked: prefs[PRIVACY_ACK].as_bool() == Some(true),
                    endpoint: prefs[ENDPOINT_KEY].as_str().unwrap_or(NOMINATIM_URL).to_string(),
                }
            },
            http_fetch,
        )
    }

    /// The headless CLI's hook: a `geocode` script line is the explicit
    /// request, so it sends without the GUI notice and says so on stderr.
    pub fn headless() -> Self {
        Self::new(
            || Settings {
                acked: true,
                endpoint: crate::app::load_ui_json()[ENDPOINT_KEY]
                    .as_str()
                    .unwrap_or(NOMINATIM_URL)
                    .to_string(),
            },
            |url: &str| {
                eprintln!("geocode: sending the address to {url} (OpenStreetMap Nominatim)");
                http_fetch(url)
            },
        )
    }

    /// Whether a lookup may send now (the privacy notice was accepted).
    pub fn acked(&self) -> bool {
        (self.settings)().acked
    }

    /// The Daylight section's custom date (month, day), or `None` for today.
    pub fn set_custom_day(&self, day: Option<(u8, u8)>) {
        *lock(&self.custom_day) = day;
    }

    /// The address the gate refused for want of the notice, once.
    pub fn take_notice_request(&self) -> Option<String> {
        lock(&self.notice_wanted).take()
    }

    /// Look `query` up ahead of the command (on a worker thread, so the UI
    /// never waits on the network): a match lands in the cache, anything
    /// else is stashed for the command's own search. One request either way.
    pub fn prefetch(&self, query: &str) {
        let answer = self.lookup(query);
        if !matches!(&answer, Ok(p) if !p.is_empty()) {
            *lock(&self.stashed) = Some((cache_key(query), answer));
        }
    }

    fn date(&self) -> jiff::civil::Date {
        let today = jiff::Zoned::now().date();
        match *lock(&self.custom_day) {
            Some((month, day)) => {
                jiff::civil::Date::new(today.year(), month as i8, day as i8).unwrap_or(today)
            }
            None => today,
        }
    }

    /// The cached answer, else (notice accepted) one request.
    fn lookup(&self, query: &str) -> Answer {
        let key = cache_key(query);
        if let Some(hit) = lock(&self.cache).get(&key) {
            return Ok(hit.clone());
        }
        let settings = (self.settings)();
        if !settings.acked {
            *lock(&self.notice_wanted) = Some(query.to_string());
            return Err(GeoError::Declined(NOT_SENT.into()));
        }
        let url = search_url(&settings.endpoint, query).map_err(GeoError::Unreachable)?;
        let body = (self.fetch)(&url).map_err(GeoError::Unreachable)?;
        let mut places = parse_results(&body).map_err(GeoError::Unreachable)?;
        let date = self.date();
        for p in &mut places {
            let tz = tz_offset(p.lat_deg, p.lon_deg, date);
            p.tz_hours = Some(tz.hours);
            p.zone = tz.zone;
        }
        // Failures and "no match" are not cached: the user may retry.
        if !places.is_empty() {
            lock(&self.cache).insert(key, places.clone());
        }
        Ok(places)
    }
}

impl Geocoder for Nominatim {
    fn search(&self, query: &str) -> Answer {
        let key = cache_key(query);
        let stashed = {
            let mut slot = lock(&self.stashed);
            match slot.take() {
                Some((k, answer)) if k == key => Some(answer),
                other => {
                    *slot = other;
                    None
                }
            }
        };
        stashed.unwrap_or_else(|| self.lookup(query))
    }

    fn attribution(&self) -> Option<String> {
        Some(ATTRIBUTION.into())
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

/// What a finished lookup asks the app to do.
#[derive(Clone, Debug, PartialEq)]
pub enum Outcome {
    /// A field lookup with one match: set the location to it.
    Apply(GeoPlace),
    /// A field lookup with several matches: let the user pick
    /// ([`GeocodeUi::matches`]).
    Choose(Vec<GeoPlace>),
    /// Nothing to set; the message says why.
    Failed(String),
    /// A typed `geocode` line whose lookup is done (answer cached or
    /// stashed): run it as a command now.
    Run(String),
}

type Pending = Arc<Mutex<Option<Answer>>>;

/// Which action started the lookup in flight.
enum Job {
    /// The Daylight field, for this address.
    Field(String),
    /// A typed command line, run once the answer is in.
    Typed(String),
}

/// The app's geocoding state: the address field, the one lookup in flight,
/// the pick list and the privacy notice.
#[derive(Default)]
pub struct GeocodeUi {
    /// The Daylight section's address field.
    pub address: String,
    /// Several matches waiting for the user's pick (empty when none).
    pub matches: Vec<GeoPlace>,
    /// The last field lookup's error, shown under the field.
    pub error: Option<String>,
    /// The last lookup's note (matched zone, longitude fallback), shown under
    /// the field for a lookup started there.
    pub notice: Option<String>,
    /// The current lookup came from the Daylight field: its messages go under
    /// the field, never into the command line's transcript or recall (#215).
    pub from_field: bool,
    /// The first-use privacy notice is showing, for this address.
    pub privacy_pending: Option<String>,
    /// Accepting the notice sends `privacy_pending` (a field lookup).
    pub privacy_resend: bool,
    /// A typed `geocode` line to run again once the notice is accepted. A
    /// Deck command is not re-run: accepting sends nothing for it.
    pub privacy_rerun: Option<String>,
    pending: Option<(Job, Pending)>,
}

impl GeocodeUi {
    /// A lookup is in flight.
    pub fn busy(&self) -> bool {
        self.pending.is_some()
    }

    /// Start the field's lookup of `query` on a utility thread. `None` while
    /// it runs; poll with [`Self::poll`].
    pub fn start_field(&mut self, query: &str, hook: Arc<Nominatim>) -> Option<Outcome> {
        let query = query.trim().to_string();
        if query.is_empty() {
            return Some(Outcome::Failed("usage: geocode \"<address>\" [#n]".into()));
        }
        if self.busy() {
            return Some(Outcome::Failed("geocode: a lookup is already running".into()));
        }
        self.error = None;
        self.notice = None;
        self.matches.clear();
        let q = query.clone();
        self.spawn(Job::Field(query), move || hook.search(&q));
        None
    }

    /// Look a typed `line`'s `query` up on a utility thread, then hand the
    /// line back ([`Outcome::Run`]) to run as a command with the answer in.
    pub fn start_typed(&mut self, line: &str, query: &str, hook: Arc<Nominatim>) -> Option<Outcome> {
        if self.busy() {
            return Some(Outcome::Failed("geocode: a lookup is already running".into()));
        }
        let q = query.to_string();
        self.spawn(Job::Typed(line.to_string()), move || {
            hook.prefetch(&q);
            Ok(Vec::new())
        });
        None
    }

    fn spawn(&mut self, job: Job, f: impl FnOnce() -> Answer + Send + 'static) {
        let slot: Pending = Arc::default();
        let out = slot.clone();
        itsjustcad_commands::qos::spawn_utility(move || {
            let r = f();
            *lock(&out) = Some(r);
        });
        self.pending = Some((job, slot));
    }

    /// The finished lookup's outcome, once.
    pub fn poll(&mut self) -> Option<Outcome> {
        let (_, slot) = self.pending.as_ref()?;
        let result = lock(slot).take()?;
        match self.pending.take().expect("pending").0 {
            Job::Typed(line) => Some(Outcome::Run(line)),
            Job::Field(query) => Some(self.field_outcome(&query, result)),
        }
    }

    fn field_outcome(&mut self, query: &str, result: Answer) -> Outcome {
        match result {
            Ok(places) if places.is_empty() => {
                let msg = format!("no place found for '{query}'; location unchanged");
                self.error = Some(msg.clone());
                Outcome::Failed(msg)
            }
            Ok(mut places) if places.len() == 1 => Outcome::Apply(places.remove(0)),
            Ok(places) => {
                self.matches = places.clone();
                Outcome::Choose(places)
            }
            Err(e) => {
                let msg = e.to_string();
                self.error = Some(msg.clone());
                Outcome::Failed(msg)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A trimmed real Nominatim `jsonv2` answer for the acceptance address.
    const AMPHITHEATRE: &str = r#"[{"place_id":1,"licence":"Data © OpenStreetMap contributors, ODbL 1.0.","lat":"37.4224857","lon":"-122.0855846","category":"building","type":"commercial","display_name":"Google Building 41, 1600, Amphitheatre Parkway, Mountain View, Santa Clara County, California, 94043, United States"}]"#;

    const STUB_URL: &str = "https://geo.test/search";

    fn settings(acked: bool) -> impl Fn() -> Settings + Send + Sync + 'static {
        move || Settings { acked, endpoint: STUB_URL.into() }
    }

    /// A hook answering `body`, counting its requests.
    fn counting(body: &'static str) -> (Nominatim, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        let c = calls.clone();
        let hook = Nominatim::new(settings(true), move |url: &str| {
            assert!(url.starts_with("https://geo.test/search?q="), "{url}");
            c.fetch_add(1, Ordering::SeqCst);
            Ok(body.as_bytes().to_vec())
        });
        (hook, calls)
    }

    fn wait(g: &mut GeocodeUi) -> Outcome {
        for _ in 0..500 {
            if let Some(o) = g.poll() {
                return o;
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        panic!("lookup never finished");
    }

    #[test]
    fn search_url_encodes_the_address() {
        let url = search_url(NOMINATIM_URL, "1600 Amphitheatre Pkwy, Mountain View").unwrap();
        assert_eq!(
            url,
            "https://nominatim.openstreetmap.org/search?q=1600+Amphitheatre+Pkwy%2C+Mountain+View&format=jsonv2&limit=5"
        );
        assert!(search_url("not a url", "x").is_err());
    }

    #[test]
    fn parses_nominatim_matches() {
        let places = parse_results(AMPHITHEATRE.as_bytes()).unwrap();
        assert_eq!(places.len(), 1);
        assert!((places[0].lat_deg - 37.42).abs() < 0.01 && (places[0].lon_deg + 122.08).abs() < 0.01);
        assert!(places[0].name.contains("Mountain View"));
        assert!(parse_results(b"[]").unwrap().is_empty());
        assert!(parse_results(b"<html>").is_err());
        // Entries without coordinates are skipped, not errors.
        let odd = br#"[{"display_name":"x"},{"lat":"1.5","lon":2,"display_name":"a\n b"}]"#;
        let want = GeoPlace { name: "a b".into(), lat_deg: 1.5, lon_deg: 2.0, tz_hours: None, zone: None };
        assert_eq!(parse_results(odd).unwrap(), vec![want]);
    }

    #[test]
    fn hook_fills_the_offset_and_reports_failures() {
        let (hook, _) = counting(AMPHITHEATRE);
        hook.set_custom_day(Some((6, 21)));
        let p = hook.search("1600 Amphitheatre Pkwy").unwrap().remove(0);
        assert_eq!((p.tz_hours, p.zone.as_deref()), (Some(-7.0), Some("America/Los_Angeles")));
        assert_eq!(hook.attribution().as_deref(), Some(ATTRIBUTION));

        let down = Nominatim::new(settings(true), |_: &str| Err("request failed: dns error".to_string()));
        let err = down.search("x").unwrap_err();
        assert!(matches!(err, GeoError::Unreachable(_)));
        assert!(err.to_string().contains("location unchanged"), "{err}");
    }

    #[test]
    fn nothing_is_sent_before_the_notice_is_accepted() {
        let hook = Nominatim::new(settings(false), |_: &str| -> Result<Vec<u8>, String> {
            panic!("sent before the privacy notice was accepted")
        });
        assert_eq!(hook.search("Quito"), Err(GeoError::Declined(NOT_SENT.into())));
        assert_eq!(hook.take_notice_request().as_deref(), Some("Quito"));
        assert_eq!(hook.take_notice_request(), None);
    }

    #[test]
    fn mountain_view_is_pacific_time_with_dst() {
        let (lat, lon) = (37.4224857, -122.0855846);
        let summer = tz_offset(lat, lon, jiff::civil::date(2026, 6, 21));
        let winter = tz_offset(lat, lon, jiff::civil::date(2026, 12, 21));
        assert_eq!(summer.zone.as_deref(), Some("America/Los_Angeles"));
        assert_eq!((summer.hours, winter.hours), (-7.0, -8.0));
        // Half-hour zones keep their fraction.
        assert_eq!(tz_offset(28.61, 77.21, jiff::civil::date(2026, 1, 1)).hours, 5.5);
    }

    #[test]
    fn open_sea_falls_back_to_longitude() {
        let mid_pacific = tz_offset(0.0, -140.0, jiff::civil::date(2026, 1, 1));
        assert_eq!(mid_pacific, TzOffset { hours: -9.0, zone: None });
    }

    #[test]
    fn address_arg_drops_quotes() {
        assert_eq!(address_arg(" \"1600 Amphitheatre Pkwy, Mountain View\" "), "1600 Amphitheatre Pkwy, Mountain View");
        assert_eq!(address_arg("Quito"), "Quito");
    }

    #[test]
    fn one_request_per_search_then_cached() {
        let (hook, calls) = counting(AMPHITHEATRE);
        let hook = Arc::new(hook);
        let mut g = GeocodeUi::default();
        assert_eq!(g.start_field("1600 Amphitheatre Pkwy", hook.clone()), None);
        assert!(g.busy());
        // A second action while one runs sends nothing.
        assert!(matches!(g.start_field("elsewhere", hook.clone()), Some(Outcome::Failed(_))));
        assert!(matches!(wait(&mut g), Outcome::Apply(p) if p.name.contains("Mountain View")));
        // The same address again (any spacing/case) comes from the cache.
        assert!(hook.search(" 1600  amphitheatre pkwy ").is_ok());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn several_matches_wait_for_a_pick_and_failures_are_not_cached() {
        let two = r#"[{"lat":"-2.19","lon":"-79.88","display_name":"Guayaquil, Ecuador"},{"lat":"3.45","lon":"-76.53","display_name":"Cali, Colombia"}]"#;
        let mut g = GeocodeUi::default();
        assert_eq!(g.start_field("x", Arc::new(counting(two).0)), None);
        assert!(matches!(wait(&mut g), Outcome::Choose(p) if p.len() == 2));
        assert_eq!(g.matches.len(), 2);

        let flaky = Arc::new(AtomicUsize::new(0));
        let f = flaky.clone();
        let hook = Arc::new(Nominatim::new(settings(true), move |_: &str| {
            if f.fetch_add(1, Ordering::SeqCst) == 0 {
                Err("offline".to_string())
            } else {
                Ok(AMPHITHEATRE.as_bytes().to_vec())
            }
        }));
        let mut g = GeocodeUi::default();
        g.start_field("y", hook.clone());
        assert!(matches!(wait(&mut g), Outcome::Failed(_)));
        assert!(g.error.is_some());
        g.start_field("y", hook);
        assert!(matches!(wait(&mut g), Outcome::Apply(_)), "a retry goes out again");
        assert_eq!(g.error, None);
    }

    #[test]
    fn a_typed_lookup_prefetches_once_and_hands_back_the_line() {
        let calls = Arc::new(AtomicUsize::new(0));
        let c = calls.clone();
        let hook = Arc::new(Nominatim::new(settings(true), move |_: &str| {
            c.fetch_add(1, Ordering::SeqCst);
            Err("offline".to_string())
        }));
        let mut g = GeocodeUi::default();
        assert_eq!(g.start_typed("geocode Quito", "Quito", hook.clone()), None);
        assert_eq!(wait(&mut g), Outcome::Run("geocode Quito".into()));
        // The command's own search gets the stashed failure, once: no resend.
        assert!(matches!(hook.search("quito"), Err(GeoError::Unreachable(_))));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(hook.search("quito").is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 2, "a later command retries");
    }
}
