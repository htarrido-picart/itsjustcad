// SPDX-License-Identifier: AGPL-3.0-only

//! The Shadows panel's time-of-day and date (#357, SketchUp's Shadows
//! sliders).
//!
//! The panel keeps a local clock (day of the year and minute of the day,
//! at the document location's time zone) as view state in `ui.json`. The
//! sun itself is document state: releasing a slider runs the logged,
//! undoable `sun <lat> <lon> <date> <utc time>` command. `sun` takes UTC and
//! resets the location's time zone (and place name), so when the document
//! had either, a `location` line restoring them follows; that is two undo
//! steps. While a slider is dragged the viewport previews the sun from
//! [`ShadowClock::sun_dir`] without touching the document.

use serde::{Deserialize, Serialize};

/// Days before each month in a common year.
const MONTH_START: [u16; 12] = [0, 31, 59, 90, 120, 151, 181, 212, 243, 273, 304, 334];
const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];

/// The panel's clock: local time at the document location.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ShadowClock {
    /// Day of a common year, 1..=365.
    pub day: u16,
    /// Minute of the day, 0..=1439, local.
    pub minute: u16,
}

impl Default for ShadowClock {
    /// SketchUp's default: 8 November, 13:30.
    fn default() -> Self {
        ShadowClock { day: 312, minute: 13 * 60 + 30 }
    }
}

/// Month (1-based) and day of month of day-of-year `day` in a common year.
pub fn month_day(day: u16) -> (u32, u32) {
    let day = day.clamp(1, 365);
    let m = MONTH_START.iter().rposition(|&s| s < day).unwrap_or(0);
    (m as u32 + 1, (day - MONTH_START[m]) as u32)
}

/// `Nov 8`.
pub fn date_label(day: u16) -> String {
    let (m, d) = month_day(day);
    format!("{} {d}", MONTHS[m as usize - 1])
}

/// `13:30`.
pub fn time_label(minute: u16) -> String {
    let m = minute.min(1439);
    format!("{:02}:{:02}", m / 60, m % 60)
}

/// The current year (UTC), for the `sun` date.
pub fn this_year() -> i32 {
    let days = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() / 86_400) as i64;
    // Civil-from-days (Howard Hinnant), year only.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    (yoe + era * 400 + i64::from(mp >= 10)) as i32
}

impl ShadowClock {
    /// The clock in UTC for a zone `tz_hours` east: `(day, minute)`, the day
    /// wrapping within the year.
    pub fn utc(self, tz_hours: f64) -> (u16, u16) {
        let t = i32::from(self.minute.min(1439)) - (tz_hours * 60.0).round() as i32;
        let shift = t.div_euclid(1440);
        let day = (i32::from(self.day.clamp(1, 365)) - 1 + shift).rem_euclid(365) + 1;
        (day as u16, t.rem_euclid(1440) as u16)
    }

    /// The lines that set the document sun to this clock at `loc` in `year`:
    /// `sun`, then `location` restoring the time zone and place `sun` resets.
    pub fn sun_lines(self, year: i32, loc: &itsjustcad_doc::GeoLocation, place: Option<&str>) -> Vec<String> {
        let (day, minute) = self.utc(loc.tz_hours);
        let (m, d) = month_day(day);
        let (lat, lon) = (loc.lat_deg, loc.lon_deg);
        let mut lines = vec![format!("sun {lat} {lon} {year:04}-{m:02}-{d:02} {}", time_label(minute))];
        if loc.tz_hours != 0.0 || place.is_some() {
            let place = place.map(|p| format!(" \"{p}\"")).unwrap_or_default();
            lines.push(format!("location {lat} {lon} {}{place}", loc.tz_hours));
        }
        lines
    }

    /// The direction toward the sun at this clock (the drag preview), as
    /// `sun` would compute it.
    pub fn sun_dir(self, year: i32, loc: &itsjustcad_doc::GeoLocation) -> [f32; 3] {
        let (day, minute) = self.utc(loc.tz_hours);
        let (m, d) = month_day(day);
        let p = itsjustcad_solar::solar_position(
            year,
            m,
            d,
            u32::from(minute / 60),
            u32::from(minute % 60),
            loc.lat_deg,
            loc.lon_deg,
        );
        itsjustcad_solar::sun_direction(p.azimuth_deg, p.altitude_deg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use itsjustcad_doc::GeoLocation;

    #[test]
    fn dates_and_times_read_back() {
        assert_eq!(month_day(1), (1, 1));
        assert_eq!(month_day(59), (2, 28));
        assert_eq!(month_day(60), (3, 1));
        assert_eq!(month_day(365), (12, 31));
        assert_eq!(date_label(ShadowClock::default().day), "Nov 8");
        assert_eq!(time_label(ShadowClock::default().minute), "13:30");
        assert_eq!(time_label(5000), "23:59");
        assert!(this_year() >= 2024);
        let c: ShadowClock = serde_json::from_str("{}").unwrap();
        assert_eq!(c, ShadowClock::default());
    }

    #[test]
    fn utc_shifts_by_the_zone_and_wraps_the_day() {
        let c = ShadowClock { day: 100, minute: 60 };
        assert_eq!(c.utc(0.0), (100, 60));
        assert_eq!(c.utc(2.0), (99, 23 * 60), "east of UTC: the day before");
        assert_eq!(ShadowClock { day: 365, minute: 23 * 60 }.utc(-5.0), (1, 4 * 60));
        assert_eq!(ShadowClock { day: 1, minute: 0 }.utc(5.5), (365, 1440 - 330));
    }

    #[test]
    fn sun_lines_set_the_sun_then_restore_the_zone_and_place() {
        let loc = GeoLocation { lat_deg: 41.5, lon_deg: 2.25, tz_hours: 1.0 };
        let c = ShadowClock { day: 172, minute: 12 * 60 };
        let lines = c.sun_lines(2026, &loc, Some("Some Square"));
        assert_eq!(lines, ["sun 41.5 2.25 2026-06-21 11:00", "location 41.5 2.25 1 \"Some Square\""]);
        for l in &lines {
            assert!(itsjustcad_commands::parse(l).is_ok(), "{l}");
        }
        let utc = GeoLocation { tz_hours: 0.0, ..loc };
        assert_eq!(c.sun_lines(2026, &utc, None), ["sun 41.5 2.25 2026-06-21 12:00"]);
        // The preview matches what `sun` will store: noon in June is high.
        let d = c.sun_dir(2026, &utc);
        assert!(d[2] > 0.9, "{d:?}");
        let night = ShadowClock { minute: 0, ..c }.sun_dir(2026, &utc);
        assert!(night[2] < 0.0, "{night:?}");
    }
}
