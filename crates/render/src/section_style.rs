// SPDX-License-Identifier: AGPL-3.0-only

//! Section display (#359, SketchUp's Section Planes / Section Cuts / Section
//! Fill toggles): whether the plane widgets show, whether the active plane
//! cuts the view, and whether the cut solids are capped in the fill colour.
//!
//! One [`SectionStyle`] holds them so a named style (#362) serializes it
//! whole. View state only (`ui.json`), never logged: the planes and the
//! active cut are document state (`sectionplane`).
//!
//! The cut is GPU-only: the camera uniform carries the active plane
//! ([`crate::CameraUniform::with_section`]) and every scene shader discards
//! the cut side, so moving or toggling a plane re-sends no geometry. The
//! fill draws the back faces seen through the cut in a flat colour, the
//! cheap capping trick: exact for closed solids; an open mesh seen from
//! behind on the cut side fills too.

use serde::{Deserialize, Serialize};

/// Every section display setting of the viewport (#359). Serialized into
/// `ui.json` and named styles; missing keys take the defaults.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SectionStyle {
    /// Draw the section plane widgets (frame, arrows, name).
    pub planes: bool,
    /// The active plane cuts the viewport.
    pub cuts: bool,
    /// Cap the cut solids in [`Self::fill_rgb`].
    pub fill: bool,
    pub fill_rgb: [u8; 3],
}

/// Default section fill: SketchUp's near-black cut face.
pub const SECTION_FILL_RGB: [u8; 3] = [0x30, 0x30, 0x30];

impl Default for SectionStyle {
    fn default() -> Self {
        SectionStyle { planes: true, cuts: true, fill: true, fill_rgb: SECTION_FILL_RGB }
    }
}

impl SectionStyle {
    /// The fill colour as linear-ish RGBA (as the other style colours).
    pub fn fill_rgba(self) -> [f32; 4] {
        let [r, g, b] = self.fill_rgb.map(|c| c as f32 / 255.0);
        [r, g, b, 1.0]
    }

    /// One-line summary for the command line.
    pub fn summary(self) -> String {
        let on = |b: bool| if b { "on" } else { "off" };
        let [r, g, b] = self.fill_rgb;
        format!(
            "sections: planes {} · cuts {} · fill {} (#{r:02x}{g:02x}{b:02x})",
            on(self.planes),
            on(self.cuts),
            on(self.fill),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_show_everything_and_fill_missing_keys() {
        let d = SectionStyle::default();
        assert!(d.planes && d.cuts && d.fill);
        let empty: SectionStyle = serde_json::from_str("{}").unwrap();
        assert_eq!(empty, d);
        let partial: SectionStyle = serde_json::from_str(r#"{ "fill": false }"#).unwrap();
        assert_eq!(partial, SectionStyle { fill: false, ..d });
        let s = SectionStyle { fill_rgb: [255, 0, 16], ..d };
        assert_eq!(serde_json::from_value::<SectionStyle>(serde_json::to_value(s).unwrap()).unwrap(), s);
        assert!(s.summary().contains("#ff0010"), "{}", s.summary());
    }
}
