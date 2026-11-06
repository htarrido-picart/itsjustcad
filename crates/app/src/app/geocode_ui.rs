// SPDX-License-Identifier: AGPL-3.0-only
// Copyright © 2026 Hector Tarrido-Picart

//! The `geocode` command's desktop side and the Daylight section's address
//! field (#215): the host geocoder on the session, the first-use privacy
//! notice, the lookups off the UI thread, and the field's pick list.
//!
//! A typed `geocode` (and a Deck one) is the core command: it looks the
//! address up through the session's hook and applies the logged `location`
//! op. A typed line is looked up on a worker thread first, so the UI never
//! waits on the network; the command then runs with the answer in hand (one
//! request); a Deck one is prefetched the same way ([`crate::deck_net`],
//! #346). The field's lookup runs the `location` op on the session
//! directly, so it never shows in the command line.

use super::App;
use crate::geocode::{Outcome, PRIVACY_ACK};
use crate::i18n::t;
use itsjustcad_commands::geocode::{location_op, GeoPlace};

impl App {
    /// Install `hook` as the session's geocoder (and the one the field uses).
    #[cfg(test)]
    pub(in crate::app) fn set_geocoder(&mut self, hook: std::sync::Arc<crate::geocode::Nominatim>) {
        self.geo_hook = hook;
        self.session.set_geocoder(Some(self.geo_hook.clone()));
    }

    /// Put the geocoder on a session that lacks it (a new or opened one).
    fn ensure_geocoder(&mut self) {
        if !self.session.has_geocoder() {
            self.session.set_geocoder(Some(self.geo_hook.clone()));
        }
    }

    /// The Daylight field's Find: one lookup. The first time, the privacy
    /// notice asks before anything is sent. A field lookup never touches the
    /// command line: no echo, no recall entry, its messages under the field.
    pub(in crate::app) fn geocode_request(&mut self, address: &str) {
        self.ensure_geocoder();
        self.geocode.from_field = true;
        let address = crate::geocode::address_arg(address);
        if address.is_empty() {
            self.geocode.notice = Some("usage: geocode \"<address>\" [#n]".into());
            return;
        }
        self.geocode.address.clone_from(&address);
        if !self.geo_hook.acked() {
            self.geocode.privacy_resend = true;
            self.geocode.privacy_pending = Some(address);
            return;
        }
        if let Some(outcome) = self.geocode.start_field(&address, self.geo_hook.clone()) {
            self.geocode_finish(outcome);
        }
    }

    /// A typed `geocode` line: looked up on a worker thread, then run as the
    /// core command ([`Outcome::Run`]). Without the notice accepted (or with
    /// a line that does not parse) it runs at once: the hook sends nothing,
    /// asks for the notice, and the command reports why; accepting the
    /// notice then runs the line again.
    pub(in crate::app) fn geocode_typed(&mut self, line: &str) {
        self.ensure_geocoder();
        self.geocode.from_field = false;
        match itsjustcad_commands::parse(line) {
            Ok(itsjustcad_commands::Command::Geocode { query, .. }) if self.geo_hook.acked() => {
                if let Some(outcome) = self.geocode.start_typed(line, &query, self.geo_hook.clone()) {
                    self.geocode_finish(outcome);
                }
            }
            Ok(itsjustcad_commands::Command::Geocode { .. }) => {
                self.command_line.execute(&mut self.session, line);
                if self.geo_hook_requested_notice() {
                    self.geocode.privacy_rerun = Some(line.to_string());
                }
            }
            _ => {
                self.command_line.execute(&mut self.session, line);
            }
        }
    }

    /// Per frame: keep the geocoder on the session with the Daylight date,
    /// fold a finished lookup in, and show the privacy notice or the pick
    /// list when one is waiting.
    pub(in crate::app) fn geocode_frame(&mut self, ctx: &egui::Context) {
        self.ensure_geocoder();
        self.geo_hook.set_custom_day(match self.optimize.form.daylight_period {
            itsjustcad_commands::site_metrics::DaylightPeriod::Custom { month, day } => {
                Some((month as u8, day as u8))
            }
            _ => None,
        });
        if let Some(outcome) = self.geocode.poll() {
            self.geocode_finish(outcome);
        }
        // A Deck command the gate refused: show the notice; accepting it
        // sends nothing for that command.
        if let Some(address) = self.geo_hook.take_notice_request() {
            self.geocode_command_notice(address);
        }
        if self.geocode.busy() {
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
        }
        if self.geocode.privacy_pending.is_some() {
            self.geocode_privacy_ui(ctx);
        } else if !self.geocode.matches.is_empty() {
            self.geocode_choose_ui(ctx);
        }
    }

    /// Whether the last command's lookup was refused for want of the notice;
    /// if so, the notice is now showing for it.
    fn geo_hook_requested_notice(&mut self) -> bool {
        match self.geo_hook.take_notice_request() {
            Some(address) => {
                self.geocode_command_notice(address);
                true
            }
            None => false,
        }
    }

    /// Show the privacy notice for a command's (not the field's) address.
    fn geocode_command_notice(&mut self, address: String) {
        if self.geocode.privacy_pending.is_none() {
            self.geocode.from_field = false;
            self.geocode.privacy_resend = false;
            self.geocode.privacy_rerun = None;
            self.geocode.privacy_pending = Some(address);
        }
    }

    /// A message for the user: under the field for a field lookup, else on
    /// the command line.
    fn geocode_say(&mut self, line: String) {
        if self.geocode.from_field {
            self.geocode.notice = Some(line);
        } else {
            self.command_line.push_line(line);
        }
    }

    fn geocode_finish(&mut self, outcome: Outcome) {
        match outcome {
            Outcome::Apply(place) => self.geocode_apply(&place),
            // The pick list shows the matches.
            Outcome::Choose(_) => {}
            // `GeocodeUi` already holds a field failure as `error`.
            Outcome::Failed(_) if self.geocode.from_field => {}
            Outcome::Failed(msg) => self.command_line.push_line(msg),
            Outcome::Run(line) => {
                self.command_line.execute(&mut self.session, &line);
            }
        }
    }

    /// Set the location to the field's `place` through the logged `location`
    /// op (one undo step), run on the session directly: op log and undo
    /// only, never the command line.
    pub(in crate::app) fn geocode_apply(&mut self, place: &GeoPlace) {
        self.geocode.matches.clear();
        let (cmd, tz_note) = location_op(place);
        match self.session.run(cmd) {
            // The field already shows the attribution line.
            Ok(_) => self.geocode.notice = Some(tz_note),
            Err(e) => self.geocode.error = Some(format!("geocode: {e}")),
        }
    }

    /// First use: say the address goes to OpenStreetMap before sending it.
    fn geocode_privacy_ui(&mut self, ctx: &egui::Context) {
        let dark = ctx.theme() == egui::Theme::Dark;
        let roles = self.live_roles(dark);
        let (mut send, mut cancel) = (false, false);
        // A Deck command's notice only records the acceptance.
        let sends = self.geocode.privacy_resend || self.geocode.privacy_rerun.is_some();
        let accept_label = if sends { t("geocode.privacy.send") } else { t("geocode.privacy.accept") };
        egui::Modal::new(egui::Id::new("itsjustcad_geocode_privacy"))
            .frame(crate::widgets::dialog_modal_frame(&roles))
            .show(ctx, |ui| {
                crate::widgets::dialog_body(ui, &roles, 400.0, |ui| {
                    crate::widgets::dialog_title(ui, &roles, t("geocode.privacy.title"));
                    ui.add_space(crate::theme::Spacing::SM);
                    crate::widgets::dialog_text(ui, &roles, t("geocode.privacy.body"));
                    ui.add_space(crate::theme::Spacing::L);
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = crate::theme::Spacing::SM;
                        send = crate::widgets::role_button(
                            ui,
                            &roles,
                            dark,
                            crate::widgets::ButtonRole::Prominent,
                            accept_label,
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
            let mut v = super::load_ui_json();
            v[PRIVACY_ACK] = serde_json::json!(true);
            super::save_ui_json(&v);
            if let Some(address) = self.geocode.privacy_pending.take() {
                if self.geocode.privacy_resend {
                    self.geocode_request(&address);
                } else if let Some(line) = self.geocode.privacy_rerun.take() {
                    self.geocode_typed(&line);
                } else {
                    self.command_line.push_line(
                        "geocode: privacy notice accepted; nothing was sent: run the command again",
                    );
                }
            }
        } else if cancel || ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.geocode.privacy_pending = None;
            self.geocode.privacy_rerun = None;
            self.geocode_say("geocode: cancelled; nothing was sent".into());
        }
    }

    /// Several matches: a short list to pick the site from.
    fn geocode_choose_ui(&mut self, ctx: &egui::Context) {
        let dark = ctx.theme() == egui::Theme::Dark;
        let roles = self.live_roles(dark);
        let (mut picked, mut cancel) = (None, false);
        let matches = &self.geocode.matches;
        egui::Modal::new(egui::Id::new("itsjustcad_geocode_choose"))
            .frame(crate::widgets::dialog_modal_frame(&roles))
            .show(ctx, |ui| {
                crate::widgets::dialog_body(ui, &roles, 440.0, |ui| {
                    crate::widgets::dialog_title(ui, &roles, t("geocode.choose.title"));
                    ui.add_space(crate::theme::Spacing::SM);
                    crate::widgets::dialog_text(ui, &roles, t("geocode.choose.body"));
                    ui.add_space(crate::theme::Spacing::SM);
                    ui.spacing_mut().item_spacing.y = crate::theme::Spacing::XS;
                    for (i, p) in matches.iter().enumerate() {
                        let label = format!("{}\n{:.4}, {:.4}", p.name, p.lat_deg, p.lon_deg);
                        let row = egui::Button::selectable(false, label)
                            .wrap()
                            .min_size(egui::vec2(ui.available_width(), 0.0));
                        if ui.add(row).clicked() {
                            picked = Some(i);
                        }
                    }
                    ui.add_space(crate::theme::Spacing::SM);
                    ui.weak(crate::geocode::ATTRIBUTION);
                    ui.add_space(crate::theme::Spacing::M);
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
        if let Some(i) = picked {
            let place = self.geocode.matches[i].clone();
            self.geocode_apply(&place);
        } else if cancel || ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.geocode.matches.clear();
            self.geocode_say("geocode: no place picked; location unchanged".into());
        }
    }
}
