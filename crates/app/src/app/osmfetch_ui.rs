// SPDX-License-Identifier: AGPL-3.0-only
// Copyright © 2026 Hector Tarrido-Picart

//! The `osmfetch` command's desktop side (#343): the Overpass fetcher on the
//! session, and a typed line's request off the UI thread.
//!
//! A typed `osmfetch` is fetched on a worker thread first, so the UI never
//! waits on the network; the core command then runs with the answer in hand
//! (one request) and logs the generated geometry. A Deck `osmfetch` goes
//! through the normal Deck command path (side-effecting, so confirmed first).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use itsjustcad_commands::osmfetch::{OsmRequest, DEFAULT_RADIUS_M, MAX_RADIUS_M, MIN_RADIUS_M};
use itsjustcad_commands::Command;

use super::App;

impl App {
    /// Put the Overpass fetcher on a session that lacks it (a new or opened one).
    fn ensure_osm_fetcher(&mut self) {
        if !self.session.has_osm_fetcher() {
            self.session.set_osm_fetcher(Some(self.osm_hook.clone()));
        }
    }

    /// Install `hook` as the session's fetcher.
    #[cfg(test)]
    pub(in crate::app) fn set_osm_fetcher(&mut self, hook: Arc<crate::osmfetch::Overpass>) {
        self.osm_hook = hook;
        self.session.set_osm_fetcher(Some(self.osm_hook.clone()));
    }

    /// A typed `osmfetch` line: fetched on a worker thread, then run as the
    /// core command. A line that would fail before sending (no location, a
    /// radius out of range, a parse error) runs at once and reports why.
    pub(in crate::app) fn osmfetch_typed(&mut self, line: &str) {
        self.ensure_osm_fetcher();
        let req = match (itsjustcad_commands::parse(line), self.session.doc.location) {
            (Ok(Command::OsmFetch { radius_m }), Some(loc)) => {
                let r = radius_m.unwrap_or(DEFAULT_RADIUS_M);
                (MIN_RADIUS_M..=MAX_RADIUS_M)
                    .contains(&r)
                    .then(|| OsmRequest::new(loc.lat_deg, loc.lon_deg, r))
            }
            _ => None,
        };
        let Some(req) = req else {
            self.command_line.execute(&mut self.session, line);
            return;
        };
        if self.osm_pending.is_some() {
            self.command_line.push_line("osmfetch: a request is already running");
            return;
        }
        let done = Arc::new(AtomicBool::new(false));
        let (hook, flag) = (self.osm_hook.clone(), done.clone());
        itsjustcad_commands::qos::spawn_utility(move || {
            hook.prefetch(&req);
            flag.store(true, Ordering::Release);
        });
        self.command_line
            .push_line("osmfetch: fetching OpenStreetMap context from the Overpass API…");
        self.osm_pending = Some((line.to_string(), done));
    }

    /// A typed `osmfetch` request in flight.
    #[cfg(test)]
    pub(in crate::app) fn osmfetch_busy(&self) -> bool {
        self.osm_pending.is_some()
    }

    /// Per frame: keep the fetcher on the session; run a typed line once its
    /// answer is in.
    pub(in crate::app) fn osmfetch_frame(&mut self, ctx: &egui::Context) {
        self.ensure_osm_fetcher();
        let Some((_, done)) = &self.osm_pending else { return };
        if !done.load(Ordering::Acquire) {
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
            return;
        }
        let (line, _) = self.osm_pending.take().expect("pending");
        self.command_line.execute(&mut self.session, &line);
    }
}
