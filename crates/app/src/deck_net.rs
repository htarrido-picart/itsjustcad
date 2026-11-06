// SPDX-License-Identifier: AGPL-3.0-only
// Copyright © 2026 Hector Tarrido-Picart

//! A Deck-emitted network command off the UI thread on desktop (#346):
//! `geocode` (Nominatim) and `osmfetch` (Overpass).
//!
//! The request runs on a worker thread through the same host hook the
//! session runs the command with ([`crate::geocode::Nominatim::prefetch`],
//! [`crate::osmfetch::Overpass::prefetch`]), as a typed line does; once the
//! answer is in, the Deck runs the command, which reads the prefetched answer
//! (one request) and logs the same ops and transcript as the synchronous path.
//!
//! The draft pauses at the command: lines emitted after it wait and run in
//! order once it is done, so one that needs the location it sets (`sunhours`,
//! `osmfetch` after `geocode`, …) sees it. Pausing every later line, not just
//! the location-dependent ones, trades overlap for an order that matches the
//! synchronous path exactly. A command that sends nothing (the privacy notice
//! not yet accepted, no location, a radius out of range) is not prefetched: it
//! runs at once and reports why, unchanged.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use itsjustcad_commands::{Command, Session};

/// The desktop hooks the Deck prefetches through: the ones the app installs
/// on the session, so the command's own request is answered from the cache.
#[derive(Clone)]
pub struct NetHooks {
    pub geo: Arc<crate::geocode::Nominatim>,
    pub osm: Arc<crate::osmfetch::Overpass>,
}

/// A Deck network command whose request is in flight.
pub struct InFlight {
    pub line: String,
    pub cmd: Command,
    /// Approved through the side-effect prompt (its own transcript entry)
    /// rather than run within the draft.
    pub confirmed: bool,
    /// What the Deck pane shows while it waits.
    pub status: String,
    done: Arc<AtomicBool>,
}

impl InFlight {
    /// The request is answered; the command can run.
    pub fn done(&self) -> bool {
        self.done.load(Ordering::Acquire)
    }
}

impl NetHooks {
    /// Start `cmd`'s request on a worker thread, or hand `cmd` back when it
    /// sends nothing and runs at once (not a network command, or one that
    /// fails or is refused before sending).
    pub fn start(
        &self,
        line: &str,
        cmd: Command,
        session: &Session,
        confirmed: bool,
    ) -> Result<InFlight, Box<Command>> {
        type Job = Box<dyn FnOnce() + Send>;
        let (status, job): (String, Job) = match &cmd {
            // Privacy gate unchanged: before the notice is accepted nothing is
            // prefetched; the command's hook refuses and asks for the notice.
            Command::Geocode { query, .. } if session.has_geocoder() && self.geo.acked() => {
                let (hook, q) = (self.geo.clone(), query.clone());
                (format!("geocode: looking up “{query}” on OpenStreetMap…"), Box::new(move || hook.prefetch(&q)))
            }
            Command::OsmFetch { .. } if session.has_osm_fetcher() => {
                let Some(req) = crate::osmfetch::request_for(&cmd, session) else { return Err(Box::new(cmd)) };
                let hook = self.osm.clone();
                (
                    "osmfetch: fetching OpenStreetMap context from the Overpass API…".into(),
                    Box::new(move || hook.prefetch(&req)),
                )
            }
            _ => return Err(Box::new(cmd)),
        };
        let done = Arc::new(AtomicBool::new(false));
        let flag = done.clone();
        itsjustcad_commands::qos::spawn_utility(move || {
            job();
            flag.store(true, Ordering::Release);
        });
        Ok(InFlight { line: line.to_string(), cmd, confirmed, status, done })
    }
}
