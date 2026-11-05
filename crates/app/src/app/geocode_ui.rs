// SPDX-License-Identifier: AGPL-3.0-only
// Copyright © 2026 Hector Tarrido-Picart

//! The `geocode "<address>"` verb and the Daylight section's address field
//! (#215): the first-use privacy notice, the lookup, the pick list, and
//! setting the location through the logged `location` op.

use super::App;
use crate::geocode::{location_line, tz_offset, Outcome, Place, NOMINATIM_URL};
use crate::i18n::t;

/// `ui.json` key: the first-use privacy notice was accepted.
const PRIVACY_ACK: &str = "geocode_privacy_ack";
/// `ui.json` key: a Nominatim-compatible search endpoint replacing the public
/// one (the usage policy asks that apps can switch without an update).
const ENDPOINT_KEY: &str = "geocoder_url";

impl App {
    /// `geocode <address>` (`from_field` false) or the Daylight field's Find
    /// (`from_field` true): one lookup per call. The first time, the privacy
    /// notice asks before anything is sent. A field lookup never touches the
    /// command line: no echo, no recall entry, its messages under the field.
    pub(in crate::app) fn geocode_request(&mut self, address: &str, from_field: bool) {
        let prefs = super::load_ui_json();
        let acked = prefs[PRIVACY_ACK].as_bool() == Some(true);
        let endpoint = prefs[ENDPOINT_KEY].as_str().unwrap_or(NOMINATIM_URL).to_string();
        let handle = self.tokio.clone();
        let fetch = move |url: &str| handle.block_on(crate::download::fetch_bytes(url));
        self.geocode_request_with(address, from_field, acked, &endpoint, fetch);
    }

    /// [`Self::geocode_request`] with the notice state, endpoint and network
    /// injected (tests stub the geocoder; CI never reaches the network).
    pub(in crate::app) fn geocode_request_with(
        &mut self,
        address: &str,
        from_field: bool,
        acked: bool,
        endpoint: &str,
        fetch: impl FnOnce(&str) -> Result<Vec<u8>, String> + Send + 'static,
    ) {
        self.geocode.from_field = from_field;
        let address = crate::geocode::address_arg(address);
        if address.is_empty() {
            self.geocode_say("usage: geocode \"<address>\"".into());
            return;
        }
        self.geocode.address.clone_from(&address);
        if !acked {
            self.geocode.privacy_pending = Some(address);
            return;
        }
        if let Some(outcome) = self.geocode.start(&address, endpoint, fetch) {
            self.geocode_finish(outcome);
        }
    }

    /// Per frame: fold a finished lookup in, and show the privacy notice or
    /// the pick list when one is waiting.
    pub(in crate::app) fn geocode_frame(&mut self, ctx: &egui::Context) {
        if let Some(outcome) = self.geocode.poll() {
            self.geocode_finish(outcome);
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

    /// A message for the user: under the field for a field lookup (as the
    /// error when it is one), else on the command line.
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
            // The pick list shows the matches; the field needs no transcript.
            Outcome::Choose(_) if self.geocode.from_field => {}
            Outcome::Choose(places) => {
                self.command_line.push_line(format!(
                    "geocode: {} places match; pick one in the list",
                    places.len()
                ));
                for (i, p) in places.iter().enumerate() {
                    self.command_line
                        .push_line(format!("  {}. {} ({:.4}, {:.4})", i + 1, p.name, p.lat, p.lon));
                }
            }
            // `Geocoder` already holds a failure as `error` for the field.
            Outcome::Failed(_) if self.geocode.from_field => {}
            Outcome::Failed(msg) => self.command_line.push_line(msg),
        }
    }

    /// Set the location to `place` through the logged `location` op (one undo
    /// step), with the offset for the Daylight date in use. A field lookup
    /// runs the op on the session directly (op log and undo only); a typed
    /// one goes through the command line like any command.
    pub(in crate::app) fn geocode_apply(&mut self, place: &Place) {
        self.geocode.matches.clear();
        let tz = tz_offset(place.lat, place.lon, self.geocode_date());
        let line = location_line(place, tz.hours);
        let cmd = match itsjustcad_commands::parse(&line) {
            Ok(cmd) => cmd,
            Err(e) => {
                self.geocode_say(format!("geocode: {e}"));
                return;
            }
        };
        let applied = if self.geocode.from_field {
            match self.session.run(cmd) {
                Ok(_) => true,
                Err(e) => {
                    self.geocode.error = Some(format!("geocode: {e}"));
                    false
                }
            }
        } else {
            self.command_line.execute_command(&mut self.session, &line, cmd)
        };
        if applied {
            let zone = match &tz.zone {
                Some(z) => format!("time zone {z} (UTC{:+})", tz.hours),
                None => format!(
                    "no time zone known here; UTC{:+} guessed from longitude (lon / 15)",
                    tz.hours
                ),
            };
            if self.geocode.from_field {
                // The field already shows the attribution line.
                self.geocode.notice = Some(zone);
            } else {
                self.command_line
                    .push_line(format!("geocode: {zone} · {}", crate::geocode::ATTRIBUTION));
            }
        }
    }

    /// The date whose UTC offset (DST) the location gets: the Daylight
    /// section's custom date this year, else today.
    fn geocode_date(&self) -> jiff::civil::Date {
        let today = jiff::Zoned::now().date();
        match self.optimize.form.daylight_period {
            itsjustcad_commands::site_metrics::DaylightPeriod::Custom { month, day } => {
                jiff::civil::Date::new(today.year(), month as i8, day as i8).unwrap_or(today)
            }
            _ => today,
        }
    }

    /// First use: say the address goes to OpenStreetMap before sending it.
    fn geocode_privacy_ui(&mut self, ctx: &egui::Context) {
        let dark = ctx.theme() == egui::Theme::Dark;
        let roles = self.live_roles(dark);
        let (mut send, mut cancel) = (false, false);
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
                            t("geocode.privacy.send"),
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
                self.geocode_request(&address, self.geocode.from_field);
            }
        } else if cancel || ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.geocode.privacy_pending = None;
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
                        let label = format!("{}\n{:.4}, {:.4}", p.name, p.lat, p.lon);
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
