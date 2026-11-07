// SPDX-License-Identifier: AGPL-3.0-only
// Copyright © 2026 Hector Tarrido-Picart

//! The `osmfetch` command's desktop side (#343): the Overpass fetcher on the
//! session, the approval step, and a typed line's request off the UI thread.
//!
//! A typed `osmfetch` first shows the approval step (#348): the centre, the
//! radius and the endpoint, with the first-use privacy notice until it is
//! accepted; nothing is sent before Send, and Cancel changes nothing. On
//! Send it is fetched on a worker thread, so the UI never waits on the
//! network; the core command then runs with the answer in hand (one request)
//! and logs the generated geometry. A Deck `osmfetch` is approved in the
//! Deck's own confirmation card (one ask), then fetched the same way
//! ([`crate::deck_net`], #346).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use super::App;
use crate::i18n::t;

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
        let req = itsjustcad_commands::parse(line)
            .ok()
            .and_then(|cmd| crate::osmfetch::request_for(&cmd, &self.session));
        let Some(req) = req else {
            self.command_line.execute(&mut self.session, line);
            return;
        };
        if self.osm_pending.is_some() || self.osm_approval.is_some() {
            self.command_line.push_line("osmfetch: a request is already running");
            return;
        }
        self.osm_approval = Some((line.to_string(), req));
    }

    /// The approval step's answer: Send approves exactly the shown request
    /// (accepting the notice on first use) and fetches it; Cancel sends
    /// nothing, logs nothing and changes nothing.
    pub(in crate::app) fn osmfetch_decide(&mut self, send: bool) {
        let Some((line, req)) = self.osm_approval.take() else { return };
        if !send {
            self.command_line.push_line("osmfetch: cancelled; nothing was sent");
            return;
        }
        if !self.osm_hook.acked() {
            self.osm_hook.set_acked();
            crate::osmfetch::persist_ack();
        }
        self.osm_hook.approve(&req);
        let done = Arc::new(AtomicBool::new(false));
        let (hook, flag) = (self.osm_hook.clone(), done.clone());
        itsjustcad_commands::qos::spawn_utility(move || {
            hook.prefetch(&req);
            flag.store(true, Ordering::Release);
        });
        self.command_line
            .push_line("osmfetch: fetching OpenStreetMap context from the Overpass API…");
        self.osm_pending = Some((line, done));
    }

    /// A typed `osmfetch` waiting for Send / Cancel.
    #[cfg(test)]
    pub(in crate::app) fn osmfetch_awaiting_approval(&self) -> bool {
        self.osm_approval.is_some()
    }

    /// A typed `osmfetch` request in flight.
    #[cfg(test)]
    pub(in crate::app) fn osmfetch_busy(&self) -> bool {
        self.osm_pending.is_some()
    }

    /// Per frame: keep the fetcher on the session; show the approval step;
    /// run a typed line once its answer is in.
    pub(in crate::app) fn osmfetch_frame(&mut self, ctx: &egui::Context) {
        self.ensure_osm_fetcher();
        if self.osm_approval.is_some() {
            self.osmfetch_approval_ui(ctx);
        }
        let Some((_, done)) = &self.osm_pending else { return };
        if !done.load(Ordering::Acquire) {
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
            self.osmfetch_progress_ui(ctx);
            return;
        }
        let (line, _) = self.osm_pending.take().expect("pending");
        self.command_line.execute(&mut self.session, &line);
    }

    /// Drop the in-flight typed request: its late answer only reaches the
    /// hook's cache, never the document.
    pub(in crate::app) fn osmfetch_cancel_pending(&mut self) {
        if self.osm_pending.take().is_some() {
            self.command_line.push_line("osmfetch: cancelled; nothing changed");
        }
    }

    /// While the request is in flight: a busy indicator and Cancel, so the
    /// box stays up until the context lands.
    fn osmfetch_progress_ui(&mut self, ctx: &egui::Context) {
        let dark = ctx.theme() == egui::Theme::Dark;
        let roles = self.live_roles(dark);
        let reduce_motion = self.reduce_motion;
        let mut cancel = false;
        egui::Modal::new(egui::Id::new("itsjustcad_osmfetch_progress"))
            .frame(crate::widgets::dialog_modal_frame(&roles))
            .show(ctx, |ui| {
                crate::widgets::dialog_body(ui, &roles, 420.0, |ui| {
                    crate::widgets::dialog_title(ui, &roles, t("osmfetch.approve.title"));
                    ui.add_space(crate::theme::Spacing::SM);
                    ui.horizontal(|ui| {
                        if reduce_motion {
                            ui.label("●");
                        } else {
                            ui.spinner();
                        }
                        crate::widgets::dialog_text(ui, &roles, t("osmfetch.fetching"));
                    });
                    ui.add_space(crate::theme::Spacing::L);
                    ui.horizontal(|ui| {
                        cancel = crate::widgets::role_button(
                            ui,
                            &roles,
                            dark,
                            crate::widgets::ButtonRole::Normal,
                            t("geocode.cancel"),
                        )
                        .clicked();
                    });
                });
            });
        if cancel || ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.osmfetch_cancel_pending();
        }
    }

    /// The approval step: what is sent and where; the privacy notice too
    /// until it is accepted.
    fn osmfetch_approval_ui(&mut self, ctx: &egui::Context) {
        let Some((_, req)) = &self.osm_approval else { return };
        let details = crate::osmfetch::approval_details(req, &self.osm_hook.endpoint());
        let first_use = !self.osm_hook.acked();
        let dark = ctx.theme() == egui::Theme::Dark;
        let roles = self.live_roles(dark);
        let (mut send, mut cancel) = (false, false);
        egui::Modal::new(egui::Id::new("itsjustcad_osmfetch_approval"))
            .frame(crate::widgets::dialog_modal_frame(&roles))
            .show(ctx, |ui| {
                crate::widgets::dialog_body(ui, &roles, 420.0, |ui| {
                    crate::widgets::dialog_title(ui, &roles, t("osmfetch.approve.title"));
                    ui.add_space(crate::theme::Spacing::SM);
                    if first_use {
                        crate::widgets::dialog_text(ui, &roles, t("osmfetch.privacy.body"));
                        ui.add_space(crate::theme::Spacing::SM);
                    }
                    crate::widgets::dialog_text(ui, &roles, t("osmfetch.approve.body"));
                    ui.add_space(crate::theme::Spacing::SM);
                    egui::Frame::group(ui.style())
                        .inner_margin(egui::Margin::same(crate::theme::Spacing::SM as i8))
                        .show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            ui.monospace(&details);
                        });
                    ui.add_space(crate::theme::Spacing::L);
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = crate::theme::Spacing::SM;
                        send = crate::widgets::role_button(
                            ui,
                            &roles,
                            dark,
                            crate::widgets::ButtonRole::Prominent,
                            t("osmfetch.send"),
                        )
                        .clicked();
                        cancel = crate::widgets::role_button(
                            ui,
                            &roles,
                            dark,
                            crate::widgets::ButtonRole::Normal,
                            t("geocode.cancel"),
                        )
                        .clicked();
                    });
                });
            });
        if send {
            self.osmfetch_decide(true);
        } else if cancel || ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.osmfetch_decide(false);
        }
    }
}

