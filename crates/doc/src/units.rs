// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

use serde::{Deserialize, Serialize};

pub const METERS_PER_FOOT: f64 = 0.3048;
pub const METERS_PER_INCH: f64 = 0.0254;

/// Display unit for lengths. Geometry always stores meters; units only change
/// how lengths are parsed by default and formatted for display. Carried on the
/// document via the logged `units` command so files keep their unit on replay.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Units {
    #[default]
    M,
    Cm,
    Mm,
    Ft,
    In,
    /// Feet-and-inches, formatted 12'-6".
    FtIn,
}

impl Units {
    pub fn parse(s: &str) -> Option<Units> {
        match s.to_lowercase().as_str() {
            "m" | "meters" => Some(Units::M),
            "cm" => Some(Units::Cm),
            "mm" => Some(Units::Mm),
            "ft" | "feet" => Some(Units::Ft),
            "in" | "inch" | "inches" => Some(Units::In),
            "ftin" => Some(Units::FtIn),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Units::M => "m",
            Units::Cm => "cm",
            Units::Mm => "mm",
            Units::Ft => "ft",
            Units::In => "in",
            Units::FtIn => "ftin",
        }
    }

    /// Display units per meter plus the symbol used for derived quantities
    /// (areas, volumes). FtIn reads best in decimal feet there.
    pub fn per_meter(self) -> (f64, &'static str) {
        match self {
            Units::M => (1.0, "m"),
            Units::Cm => (100.0, "cm"),
            Units::Mm => (1000.0, "mm"),
            Units::Ft | Units::FtIn => (1.0 / METERS_PER_FOOT, "ft"),
            Units::In => (1.0 / METERS_PER_INCH, "in"),
        }
    }
}

/// Format a length stored in meters for display in the document's unit.
/// Used by command messages, dimension text and drag readouts.
pub fn format_length(units: Units, meters: f64) -> String {
    match units {
        Units::M => format!("{meters:.2} m"),
        Units::Cm => format!("{:.1} cm", meters * 100.0),
        Units::Mm => format!("{:.0} mm", meters * 1000.0),
        Units::Ft => format!("{:.2}'", meters / METERS_PER_FOOT),
        Units::In => format!("{:.1}\"", meters / METERS_PER_INCH),
        Units::FtIn => format_feet_inches(meters),
    }
}

/// Format an area stored in square meters for display in the document's unit.
pub fn format_area(units: Units, sq_meters: f64) -> String {
    let (per_m, label) = units.per_meter();
    format!("{:.2} {label}²", sq_meters * per_m * per_m)
}

/// Format a volume stored in cubic meters for display in the document's unit.
pub fn format_volume(units: Units, cu_meters: f64) -> String {
    let (per_m, label) = units.per_meter();
    format!("{:.2} {label}³", cu_meters * per_m * per_m * per_m)
}

/// Architectural feet-and-inches: 12'-6", inches to the nearest 1/16.
fn format_feet_inches(meters: f64) -> String {
    let sign = if meters < 0.0 { "-" } else { "" };
    // Round to whole sixteenths of an inch first so carries are exact.
    let sixteenths = (meters.abs() / METERS_PER_INCH * 16.0).round() as i64;
    let feet = sixteenths / (12 * 16);
    let rem = sixteenths % (12 * 16);
    let (inches, frac) = (rem / 16, rem % 16);
    if frac == 0 {
        return format!("{sign}{feet}'-{inches}\"");
    }
    // Reduce the fraction: 8/16 -> 1/2 etc. (gcd of frac and 16).
    let mut g = 1;
    for d in [2, 4, 8] {
        if frac % d == 0 {
            g = d;
        }
    }
    format!("{sign}{feet}'-{inches} {}/{}\"", frac / g, 16 / g)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_and_label_round_trip() {
        for u in [Units::M, Units::Cm, Units::Mm, Units::Ft, Units::In, Units::FtIn] {
            assert_eq!(Units::parse(u.label()), Some(u));
        }
        assert_eq!(Units::parse("feet"), Some(Units::Ft));
        assert_eq!(Units::parse("furlongs"), None);
    }

    #[test]
    fn default_is_meters() {
        assert_eq!(Units::default(), Units::M);
    }

    #[test]
    fn units_json_round_trip() {
        for u in [Units::M, Units::Cm, Units::Mm, Units::Ft, Units::In, Units::FtIn] {
            let json = serde_json::to_string(&u).unwrap();
            let back: Units = serde_json::from_str(&json).unwrap();
            assert_eq!(u, back);
        }
        assert_eq!(serde_json::to_string(&Units::FtIn).unwrap(), "\"ft_in\"");
    }

    #[test]
    fn format_metric() {
        assert_eq!(format_length(Units::M, 12.5), "12.50 m");
        assert_eq!(format_length(Units::Cm, 0.255), "25.5 cm");
        assert_eq!(format_length(Units::Mm, 0.5), "500 mm");
    }

    #[test]
    fn format_area_and_volume() {
        assert_eq!(format_area(Units::M, 12.5), "12.50 m²");
        assert_eq!(format_area(Units::Cm, 0.5), "5000.00 cm²");
        assert_eq!(format_volume(Units::M, 75.0), "75.00 m³");
        assert_eq!(format_volume(Units::Mm, 1e-6), "1000.00 mm³");
        // 1 ft = 0.3048 m, so 1 m² = ~10.7639 ft²; ftin falls back to decimal feet
        assert_eq!(
            format_area(Units::FtIn, METERS_PER_FOOT * METERS_PER_FOOT),
            "1.00 ft²"
        );
        assert_eq!(
            format_volume(Units::Ft, METERS_PER_FOOT.powi(3) * 27.0),
            "27.00 ft³"
        );
    }

    #[test]
    fn format_imperial() {
        assert_eq!(format_length(Units::Ft, 12.5 * METERS_PER_FOOT), "12.50'");
        assert_eq!(format_length(Units::In, 6.0 * METERS_PER_INCH), "6.0\"");
        assert_eq!(
            format_length(Units::FtIn, 12.0 * METERS_PER_FOOT + 6.0 * METERS_PER_INCH),
            "12'-6\""
        );
        assert_eq!(format_length(Units::FtIn, 12.0 * METERS_PER_FOOT), "12'-0\"");
        assert_eq!(
            format_length(Units::FtIn, 6.5 * METERS_PER_INCH),
            "0'-6 1/2\""
        );
        assert_eq!(
            format_length(Units::FtIn, 0.25 * METERS_PER_INCH),
            "0'-0 1/4\""
        );
        assert_eq!(
            format_length(Units::FtIn, -(3.0 * METERS_PER_FOOT + 2.0 * METERS_PER_INCH)),
            "-3'-2\""
        );
        // 11.999" rounds up and carries into the foot
        assert_eq!(
            format_length(Units::FtIn, 11.999 * METERS_PER_INCH),
            "1'-0\""
        );
    }
}
