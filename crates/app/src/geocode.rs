// SPDX-License-Identifier: AGPL-3.0-only
// Copyright © 2026 Hector Tarrido-Picart

//! Site location from a street address (#215, the `geocode` verb and the
//! Daylight section's address field).
//!
//! The address goes to an OpenStreetMap Nominatim search endpoint (the public
//! one by default; `geocoder_url` in `ui.json` switches it without an update)
//! through [`crate::download::fetch_bytes`], which sends the app's
//! User-Agent. Usage policy: one request per user action (a button press or
//! a typed command, never per keystroke), answers cached for the session so a
//! repeated search sends nothing, and the attribution shown with the result.
//!
//! The chosen match sets the location through the logged
//! `location <lat> <lon> <tz> <place>` op, so a saved file replays numbers,
//! never a lookup. The UTC offset comes offline from the coordinates
//! ([`tz_offset`]). App crate only: the iOS FFI does not link any of this
//! (MapKit geocodes there, itsjustcad-ios#165).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// The public Nominatim search endpoint (the default geocoder).
pub const NOMINATIM_URL: &str = "https://nominatim.openstreetmap.org/search";

/// Data attribution the Nominatim usage policy asks us to show.
pub const ATTRIBUTION: &str = "© OpenStreetMap contributors";

/// Matches asked for per search: enough to pick from, short enough to read.
const LIMIT: &str = "5";

/// One geocoder match.
#[derive(Clone, Debug, PartialEq)]
pub struct Place {
    /// The geocoder's display name ("…, Mountain View, California, …").
    pub name: String,
    /// Latitude, degrees (north positive).
    pub lat: f64,
    /// Longitude, degrees (east positive).
    pub lon: f64,
}

/// Why a lookup produced no location. The location stays unchanged.
#[derive(Clone, Debug, PartialEq)]
pub enum GeocodeError {
    /// The geocoder could not be reached or answered badly.
    Network(String),
    /// The geocoder answered, with no match.
    NoMatch(String),
}

impl std::fmt::Display for GeocodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Network(e) => {
                write!(f, "geocode: cannot reach the geocoder ({e}); location unchanged")
            }
            Self::NoMatch(q) => write!(f, "no place found for '{q}'"),
        }
    }
}

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
/// skipped.
pub fn parse_results(body: &[u8]) -> Result<Vec<Place>, String> {
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
                Place { name, lat, lon }
            })
        })
        .collect())
}

/// Look `query` up with one `fetch` of the search URL.
pub fn search(
    endpoint: &str,
    query: &str,
    fetch: impl FnOnce(&str) -> Result<Vec<u8>, String>,
) -> Result<Vec<Place>, GeocodeError> {
    let url = search_url(endpoint, query).map_err(GeocodeError::Network)?;
    let body = fetch(&url).map_err(GeocodeError::Network)?;
    let places = parse_results(&body).map_err(GeocodeError::Network)?;
    if places.is_empty() {
        return Err(GeocodeError::NoMatch(query.to_string()));
    }
    Ok(places)
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

/// The logged line that sets the location to `place` with offset `tz_hours`.
pub fn location_line(place: &Place, tz_hours: f64) -> String {
    let line = format!("location {} {} {tz_hours}", place.lat, place.lon);
    if place.name.is_empty() { line } else { format!("{line} {}", place.name) }
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

/// What a finished lookup asks the app to do.
#[derive(Clone, Debug, PartialEq)]
pub enum Outcome {
    /// One match: set the location to it.
    Apply(Place),
    /// Several matches: let the user pick ([`Geocoder::matches`]).
    Choose(Vec<Place>),
    /// Nothing to set; the message says why.
    Failed(String),
}

type Pending = Arc<Mutex<Option<Result<Vec<Place>, GeocodeError>>>>;

/// The app's geocoding state: the address field, the one lookup in flight,
/// the pick list, and the session cache.
#[derive(Default)]
pub struct Geocoder {
    /// The Daylight section's address field.
    pub address: String,
    /// Several matches waiting for the user's pick (empty when none).
    pub matches: Vec<Place>,
    /// The last lookup's error, shown under the field.
    pub error: Option<String>,
    /// The last lookup's note (matched zone, longitude fallback), shown under
    /// the field for a lookup started there.
    pub notice: Option<String>,
    /// The current lookup came from the Daylight field: its messages go under
    /// the field, never into the command line's transcript or recall (#215).
    pub from_field: bool,
    /// An address waiting for the first-use privacy notice to be accepted.
    pub privacy_pending: Option<String>,
    pending: Option<(String, Pending)>,
    cache: HashMap<String, Vec<Place>>,
}

impl Geocoder {
    /// A lookup is in flight.
    pub fn busy(&self) -> bool {
        self.pending.is_some()
    }

    /// Start looking `query` up: from the session cache when it was searched
    /// before (no request), else with one `fetch` on a utility thread. `None`
    /// while it runs; poll with [`Self::poll`].
    pub fn start(
        &mut self,
        query: &str,
        endpoint: &str,
        fetch: impl FnOnce(&str) -> Result<Vec<u8>, String> + Send + 'static,
    ) -> Option<Outcome> {
        let query = query.trim().to_string();
        if query.is_empty() {
            return Some(Outcome::Failed("usage: geocode \"<address>\"".into()));
        }
        if self.busy() {
            return Some(Outcome::Failed("geocode: a lookup is already running".into()));
        }
        self.error = None;
        self.notice = None;
        self.matches.clear();
        if let Some(hit) = self.cache.get(&cache_key(&query)) {
            return Some(self.outcome(&query, Ok(hit.clone())));
        }
        let slot: Pending = Arc::default();
        let (out, endpoint, q) = (slot.clone(), endpoint.to_string(), query.clone());
        itsjustcad_commands::qos::spawn_utility(move || {
            let r = search(&endpoint, &q, fetch);
            *out.lock().expect("geocode slot") = Some(r);
        });
        self.pending = Some((query, slot));
        None
    }

    /// The finished lookup's outcome, once.
    pub fn poll(&mut self) -> Option<Outcome> {
        let (_, slot) = self.pending.as_ref()?;
        let result = slot.lock().expect("geocode slot").take()?;
        let (query, _) = self.pending.take().expect("pending");
        Some(self.outcome(&query, result))
    }

    fn outcome(&mut self, query: &str, result: Result<Vec<Place>, GeocodeError>) -> Outcome {
        match result {
            Ok(mut places) => {
                self.cache.insert(cache_key(query), places.clone());
                if places.len() == 1 {
                    Outcome::Apply(places.remove(0))
                } else {
                    self.matches = places.clone();
                    Outcome::Choose(places)
                }
            }
            Err(e) => {
                // A failed request is not cached: the user may retry online.
                let msg = e.to_string();
                self.error = Some(msg.clone());
                Outcome::Failed(msg)
            }
        }
    }
}

fn cache_key(query: &str) -> String {
    query.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A trimmed real Nominatim `jsonv2` answer for the acceptance address.
    const AMPHITHEATRE: &str = r#"[{"place_id":1,"licence":"Data © OpenStreetMap contributors, ODbL 1.0.","lat":"37.4224857","lon":"-122.0855846","category":"building","type":"commercial","display_name":"Google Building 41, 1600, Amphitheatre Parkway, Mountain View, Santa Clara County, California, 94043, United States"}]"#;

    fn stub(body: &'static str) -> impl FnOnce(&str) -> Result<Vec<u8>, String> + Send + 'static {
        move |url: &str| {
            assert!(url.starts_with("https://geo.test/search?q="), "{url}");
            Ok(body.as_bytes().to_vec())
        }
    }

    fn wait(g: &mut Geocoder) -> Outcome {
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
        assert!((places[0].lat - 37.42).abs() < 0.01 && (places[0].lon + 122.08).abs() < 0.01);
        assert!(places[0].name.contains("Mountain View"));
        assert!(parse_results(b"[]").unwrap().is_empty());
        assert!(parse_results(b"<html>").is_err());
        // Entries without coordinates are skipped, not errors.
        let odd = br#"[{"display_name":"x"},{"lat":"1.5","lon":2,"display_name":"a\n b"}]"#;
        assert_eq!(parse_results(odd).unwrap(), vec![Place { name: "a b".into(), lat: 1.5, lon: 2.0 }]);
    }

    #[test]
    fn errors_say_why_and_name_the_address() {
        let none = search("https://geo.test/search", "nowhere at all", stub("[]")).unwrap_err();
        assert_eq!(none.to_string(), "no place found for 'nowhere at all'");
        let offline = search("https://geo.test/search", "x", |_: &str| {
            Err("request failed: dns error".to_string())
        })
        .unwrap_err();
        assert!(matches!(offline, GeocodeError::Network(_)));
        assert!(offline.to_string().contains("location unchanged"), "{offline}");
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
    fn location_line_replays_through_the_parser() {
        let place = parse_results(AMPHITHEATRE.as_bytes()).unwrap().remove(0);
        let line = location_line(&place, -7.0);
        assert!(line.starts_with("location 37.4224857 -122.0855846 -7 Google Building 41"), "{line}");
        match itsjustcad_commands::parse(&line).unwrap() {
            itsjustcad_commands::Command::Location { lat_deg, lon_deg, tz_hours, place: name } => {
                assert_eq!((lat_deg, lon_deg, tz_hours), (place.lat, place.lon, -7.0));
                assert_eq!(name.as_deref(), Some(place.name.as_str()));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn address_arg_drops_quotes() {
        assert_eq!(address_arg(" \"1600 Amphitheatre Pkwy, Mountain View\" "), "1600 Amphitheatre Pkwy, Mountain View");
        assert_eq!(address_arg("Quito"), "Quito");
    }

    #[test]
    fn one_request_per_search_then_cached() {
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut g = Geocoder::default();
        let fetch = |calls: &Arc<std::sync::atomic::AtomicUsize>| {
            let calls = calls.clone();
            move |_: &str| {
                calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(AMPHITHEATRE.as_bytes().to_vec())
            }
        };
        assert_eq!(g.start("1600 Amphitheatre Pkwy", "https://geo.test/search", fetch(&calls)), None);
        assert!(g.busy());
        // A second action while one runs sends nothing.
        assert!(matches!(
            g.start("elsewhere", "https://geo.test/search", fetch(&calls)),
            Some(Outcome::Failed(_))
        ));
        assert!(matches!(wait(&mut g), Outcome::Apply(p) if p.name.contains("Mountain View")));
        // The same address again (any spacing/case) comes from the cache.
        let again = g.start(" 1600  amphitheatre pkwy ", "https://geo.test/search", fetch(&calls));
        assert!(matches!(again, Some(Outcome::Apply(_))));
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[test]
    fn several_matches_wait_for_a_pick_and_failures_are_not_cached() {
        let two = r#"[{"lat":"-2.19","lon":"-79.88","display_name":"Guayaquil, Ecuador"},{"lat":"3.45","lon":"-76.53","display_name":"Cali, Colombia"}]"#;
        let mut g = Geocoder::default();
        assert_eq!(g.start("x", "https://geo.test/search", stub(two)), None);
        assert!(matches!(wait(&mut g), Outcome::Choose(p) if p.len() == 2));
        assert_eq!(g.matches.len(), 2);

        let mut g = Geocoder::default();
        g.start("y", "https://geo.test/search", |_: &str| Err("offline".to_string()));
        assert!(matches!(wait(&mut g), Outcome::Failed(_)));
        assert!(g.error.is_some());
        g.start("y", "https://geo.test/search", stub(AMPHITHEATRE));
        assert!(matches!(wait(&mut g), Outcome::Apply(_)), "a retry goes out again");
        assert_eq!(g.error, None);
    }
}
