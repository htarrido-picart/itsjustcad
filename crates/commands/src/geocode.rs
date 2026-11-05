// SPDX-License-Identifier: AGPL-3.0-only
// Copyright © 2026 Hector Tarrido-Picart

//! The `geocode "<address>" [#n]` command's host hook (#215).
//!
//! The core has no network or time-zone code: a host installs a [`Geocoder`]
//! on the [`crate::Session`] ([`crate::Session::set_geocoder`]). The desktop
//! app and the headless CLI install an OpenStreetMap Nominatim one; the iOS
//! app supplies MapKit through the FFI (`ijc_set_geocoder`). With none
//! installed, `geocode` fails and changes nothing.
//!
//! The command applies the match through the logged `location` op, so the op
//! log never holds the lookup and a saved file replays offline.

use crate::Command;

/// One geocoder match, as the host resolved it.
#[derive(Clone, Debug, PartialEq)]
pub struct GeoPlace {
    /// Display name ("…, Mountain View, California, …"); may be empty.
    pub name: String,
    /// Latitude, degrees (north positive).
    pub lat_deg: f64,
    /// Longitude, degrees (east positive).
    pub lon_deg: f64,
    /// UTC offset in hours at the site (DST included for the date the host
    /// chose). `None`: the core guesses `round(lon / 15)` and says so.
    pub tz_hours: Option<f64>,
    /// The IANA zone the offset came from, for the report.
    pub zone: Option<String>,
}

/// Why a lookup gave no places. The location stays unchanged.
#[derive(Clone, Debug, PartialEq)]
pub enum GeoError {
    /// The geocoder could not be reached or answered badly.
    Unreachable(String),
    /// The host did not send the address (e.g. the privacy notice is not yet
    /// accepted). The message says what to do.
    Declined(String),
    /// The host started an asynchronous lookup (iOS MapKit): run the command
    /// again once it has answered.
    Pending(String),
}

impl std::fmt::Display for GeoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unreachable(e) => {
                write!(f, "geocode: cannot reach the geocoder ({e}); location unchanged")
            }
            Self::Declined(m) | Self::Pending(m) => f.write_str(m),
        }
    }
}

/// A host's address lookup. One call is one request: the core calls
/// [`Geocoder::search`] once per `geocode` command.
pub trait Geocoder: Send + Sync {
    /// The matches for `query`, best first. An empty list means no match.
    fn search(&self, query: &str) -> Result<Vec<GeoPlace>, GeoError>;

    /// Data attribution to show with a result (e.g. "© OpenStreetMap
    /// contributors").
    fn attribution(&self) -> Option<String> {
        None
    }
}

/// The logged `location` op for `place`, and a note on where its UTC offset
/// came from. Shared by the command and hosts that apply a picked match
/// themselves (the desktop Daylight field).
pub fn location_op(place: &GeoPlace) -> (Command, String) {
    let (tz_hours, note) = match (place.tz_hours.filter(|h| h.is_finite()), &place.zone) {
        (Some(h), Some(z)) => (h, format!("time zone {z} (UTC{h:+})")),
        (Some(h), None) => (h, format!("UTC{h:+}")),
        (None, _) => {
            let h = (place.lon_deg / 15.0).round();
            (h, format!("no time zone known here; UTC{h:+} guessed from longitude (lon / 15)"))
        }
    };
    // One line: the name is logged (and shown) as the `location` place words.
    let name = place.name.split_whitespace().collect::<Vec<_>>().join(" ");
    let cmd = Command::Location {
        lat_deg: place.lat_deg,
        lon_deg: place.lon_deg,
        tz_hours,
        place: (!name.is_empty()).then_some(name),
    };
    (cmd, note)
}

/// Whitespace-collapsed, lower-cased `query`: the key hosts cache answers by.
pub fn cache_key(query: &str) -> String {
    query.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{parse, Session};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    /// A geocoder answering from a fixed list, counting its requests.
    struct Stub {
        places: Vec<GeoPlace>,
        calls: AtomicUsize,
    }

    impl Geocoder for Stub {
        fn search(&self, _query: &str) -> Result<Vec<GeoPlace>, GeoError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(self.places.clone())
        }
        fn attribution(&self) -> Option<String> {
            Some("© stub".into())
        }
    }

    fn place(name: &str, lat_deg: f64, lon_deg: f64, tz: Option<f64>) -> GeoPlace {
        GeoPlace { name: name.into(), lat_deg, lon_deg, tz_hours: tz, zone: None }
    }

    fn san_jose() -> Arc<Stub> {
        Arc::new(Stub {
            places: vec![
                place("San Jose, California, United States", 37.3362, -121.8906, Some(-7.0)),
                place("San Jose, Costa Rica", 9.9325, -84.0796, Some(-6.0)),
            ],
            calls: AtomicUsize::new(0),
        })
    }

    fn session_with(g: Arc<Stub>) -> Session {
        let mut s = Session::default();
        s.run(parse("location 40.71 -74.01 -5").unwrap()).unwrap();
        s.set_geocoder(Some(g));
        s
    }

    #[test]
    fn registry_lists_geocode_with_network_note() {
        let spec = crate::registry().iter().find(|s| s.name == "geocode").expect("geocode");
        assert!(spec.usage.contains("#n"));
        assert!(spec.summary.contains("network") && spec.summary.contains("location"));
    }

    #[test]
    fn parses_address_and_pick() {
        let cmd = |l| parse(l).unwrap();
        assert_eq!(
            cmd("geocode \"1600 Amphitheatre Pkwy, Mountain View\""),
            Command::Geocode { query: "1600 Amphitheatre Pkwy, Mountain View".into(), pick: None }
        );
        assert_eq!(cmd("geocode Quito #2"), Command::Geocode { query: "Quito".into(), pick: Some(2) });
        assert!(parse("geocode").is_err());
        assert!(parse("geocode Quito #0").is_err());
        assert!(!cmd("geocode Quito").is_logged());
    }

    #[test]
    fn without_a_geocoder_it_fails_and_changes_nothing() {
        let mut s = Session::default();
        s.run(parse("location 40.71 -74.01 -5").unwrap()).unwrap();
        let (before, log) = (s.doc.location, s.save_log().len());
        let err = s.run(parse("geocode Quito").unwrap()).unwrap_err().to_string();
        assert!(err.contains("no geocoder") && err.contains("location unchanged"), "{err}");
        assert_eq!((s.doc.location, s.save_log().len()), (before, log));
    }

    #[test]
    fn best_match_is_applied_and_the_others_listed() {
        let g = san_jose();
        let mut s = session_with(g.clone());
        let out = s.run(parse("geocode \"San Jose\"").unwrap()).unwrap();
        let loc = s.doc.location.unwrap();
        assert_eq!((loc.lat_deg, loc.lon_deg, loc.tz_hours), (37.3362, -121.8906, -7.0));
        assert_eq!(s.doc.place_name.as_deref(), Some("San Jose, California, United States"));
        assert!(out.message.contains("© stub"), "{}", out.message);
        assert!(out.message.contains("#2 San Jose, Costa Rica"), "{}", out.message);
        assert_eq!(g.calls.load(Ordering::SeqCst), 1, "one request per command");

        s.run(parse("geocode \"San Jose\" #2").unwrap()).unwrap();
        let loc = s.doc.location.unwrap();
        assert_eq!((loc.lat_deg, loc.tz_hours), (9.9325, -6.0));
        let err = s.run(parse("geocode \"San Jose\" #3").unwrap()).unwrap_err().to_string();
        assert!(err.contains("no match #3") && err.contains("2 found"), "{err}");
        assert_eq!(s.doc.location.unwrap().lat_deg, 9.9325, "unchanged by the failure");
    }

    #[test]
    fn no_match_and_host_errors_change_nothing() {
        struct Down;
        impl Geocoder for Down {
            fn search(&self, _: &str) -> Result<Vec<GeoPlace>, GeoError> {
                Err(GeoError::Unreachable("dns error".into()))
            }
        }
        let mut s = session_with(Arc::new(Stub { places: vec![], calls: AtomicUsize::new(0) }));
        let before = s.doc.location;
        let err = s.run(parse("geocode nowhere").unwrap()).unwrap_err().to_string();
        assert_eq!(err, "no place found for 'nowhere'; location unchanged");
        s.set_geocoder(Some(Arc::new(Down)));
        let err = s.run(parse("geocode nowhere").unwrap()).unwrap_err().to_string();
        assert!(err.contains("cannot reach the geocoder (dns error)"), "{err}");
        assert_eq!(s.doc.location, before);
    }

    #[test]
    fn missing_offset_falls_back_to_longitude() {
        let g = Arc::new(Stub { places: vec![place("", 0.0, -140.0, None)], calls: AtomicUsize::new(0) });
        let mut s = session_with(g);
        let out = s.run(parse("geocode sea").unwrap()).unwrap();
        assert_eq!(s.doc.location.unwrap().tz_hours, -9.0);
        assert_eq!(s.doc.place_name, None);
        assert!(out.message.contains("guessed from longitude"), "{}", out.message);
    }

    #[test]
    fn op_log_holds_only_location_and_replays_offline_and_undo_restores() {
        let mut s = session_with(san_jose());
        s.run(parse("geocode \"San Jose\" #2").unwrap()).unwrap();
        let log = s.save_log();
        assert!(log.iter().all(|c| matches!(c, Command::Location { .. })), "{log:?}");
        assert_eq!(log.len(), 2);

        // Replay: a reopened file has no geocoder and needs none.
        let reopened = crate::io::from_json(&crate::io::to_json(&s)).unwrap();
        assert!(!reopened.has_geocoder());
        assert_eq!(reopened.doc.location, s.doc.location);
        assert_eq!(reopened.doc.place_name.as_deref(), Some("San Jose, Costa Rica"));

        s.run(Command::Undo).unwrap();
        let loc = s.doc.location.unwrap();
        assert_eq!((loc.lat_deg, loc.lon_deg, loc.tz_hours), (40.71, -74.01, -5.0));
        assert_eq!(s.doc.place_name, None);
    }
}
