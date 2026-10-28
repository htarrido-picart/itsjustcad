//! Text to outline (#172): lay a string out with an outline (TrueType) font
//! and return each glyph contour as a closed polygon, so text can be cut.
//!
//! Font: Hack Regular, taken from the `epaint_default_fonts` crate that the
//! desktop app already ships through egui (no new font file in the tree).
//! Hack is MIT (Source Foundry) on top of Bitstream Vera (Bitstream Vera
//! License: free redistribution, embedding and modification under a new
//! name; not sold on its own). We use it unmodified.
//!
//! Layout matches the Hershey text it replaces: `origin` is the left end of
//! the baseline and `height` is the cap height. Quadratic/cubic Béziers are
//! flattened to chords within a tolerance tied to the height (at most
//! 0.01 mm), so every contour is a closed polyline. Counters (the inside of
//! "o", "8", …) come out as separate contours nested in the glyph.

use glam::{DVec2, DVec3};
use skrifa::MetadataProvider;
use skrifa::instance::{LocationRef, Size};
use skrifa::outline::{DrawSettings, OutlinePen};

/// Names accepted by `font=`.
pub const FONTS: &[&str] = &["hack"];

/// Chord tolerance (meters) used to flatten glyph curves of `height`.
pub fn flatten_tol(height: f64) -> f64 {
    (height * 1e-3).min(1e-5)
}

/// Collects a glyph's contours in font units, flattening curves.
struct Pen {
    tol: f64,
    contours: Vec<Vec<DVec2>>,
    cur: Vec<DVec2>,
}

impl Pen {
    fn last(&self) -> DVec2 {
        self.cur.last().copied().unwrap_or(DVec2::ZERO)
    }
    fn push(&mut self, p: DVec2) {
        if self.cur.last().is_none_or(|q| q.distance(p) > 1e-9) {
            self.cur.push(p);
        }
    }
    fn finish(&mut self) {
        let mut c = std::mem::take(&mut self.cur);
        while c.len() > 1 && c[0].distance(c[c.len() - 1]) <= 1e-9 {
            c.pop();
        }
        if c.len() >= 3 {
            self.contours.push(c);
        }
    }
    fn sample(&mut self, n_for: f64, f: impl Fn(f64) -> DVec2) {
        let n = (n_for / self.tol).sqrt().ceil().clamp(1.0, 256.0) as usize;
        for i in 1..=n {
            self.push(f(i as f64 / n as f64));
        }
    }
}

impl OutlinePen for Pen {
    fn move_to(&mut self, x: f32, y: f32) {
        self.finish();
        self.push(DVec2::new(x as f64, y as f64));
    }
    fn line_to(&mut self, x: f32, y: f32) {
        self.push(DVec2::new(x as f64, y as f64));
    }
    fn quad_to(&mut self, cx: f32, cy: f32, x: f32, y: f32) {
        let (p0, p1, p2) = (
            self.last(),
            DVec2::new(cx as f64, cy as f64),
            DVec2::new(x as f64, y as f64),
        );
        // Chord error of n even steps ≤ |p0 - 2p1 + p2| / (4 n²).
        let d = (p0 - 2.0 * p1 + p2).length() / 4.0;
        self.sample(d, |t| {
            let u = 1.0 - t;
            u * u * p0 + 2.0 * u * t * p1 + t * t * p2
        });
    }
    fn curve_to(&mut self, c0x: f32, c0y: f32, c1x: f32, c1y: f32, x: f32, y: f32) {
        let (p0, p1, p2, p3) = (
            self.last(),
            DVec2::new(c0x as f64, c0y as f64),
            DVec2::new(c1x as f64, c1y as f64),
            DVec2::new(x as f64, y as f64),
        );
        // |B''| ≤ 6 max(|Δ²|); chord error ≤ |B''| / (8 n²).
        let d = 0.75
            * (p0 - 2.0 * p1 + p2)
                .length()
                .max((p1 - 2.0 * p2 + p3).length());
        self.sample(d, |t| {
            let u = 1.0 - t;
            u * u * u * p0 + 3.0 * u * u * t * p1 + 3.0 * u * t * t * p2 + t * t * t * p3
        });
    }
    fn close(&mut self) {
        self.finish();
    }
}

/// One glyph's closed contours (outer outline and counters), world space.
pub type Glyph = Vec<Vec<DVec3>>;

/// Outline of `text`: closed contours (world XY at `origin.z`), grouped per
/// glyph in string order, plus the characters the font has no glyph for.
pub fn outline_text(
    text: &str,
    origin: DVec3,
    height: f64,
) -> Result<(Vec<Glyph>, Vec<char>), String> {
    if !(height.is_finite() && height > 0.0) {
        return Err(format!("text height must be positive, got {height}"));
    }
    let font = skrifa::FontRef::new(epaint_default_fonts::HACK_REGULAR)
        .map_err(|e| format!("font: {e}"))?;
    let m = font.metrics(Size::unscaled(), LocationRef::default());
    let cap = m.cap_height.filter(|c| *c > 0.0).unwrap_or(m.ascent) as f64;
    let scale = height / cap;
    let charmap = font.charmap();
    let glyphs = font.outline_glyphs();
    let adv = font.glyph_metrics(Size::unscaled(), LocationRef::default());
    let mut pen_x = 0.0;
    let mut out = Vec::new();
    let mut missing = Vec::new();
    for ch in text.chars() {
        let Some(gid) = charmap.map(ch) else {
            if !ch.is_whitespace() {
                missing.push(ch);
            }
            pen_x += adv.advance_width(Default::default()).unwrap_or(0.0) as f64;
            continue;
        };
        if let Some(g) = glyphs.get(gid) {
            let mut pen = Pen {
                tol: flatten_tol(height) / scale,
                contours: Vec::new(),
                cur: Vec::new(),
            };
            g.draw(
                DrawSettings::unhinted(Size::unscaled(), LocationRef::default()),
                &mut pen,
            )
            .map_err(|e| format!("glyph {ch:?}: {e}"))?;
            pen.finish();
            let place = |p: DVec2| {
                DVec3::new(
                    origin.x + (pen_x + p.x) * scale,
                    origin.y + p.y * scale,
                    origin.z,
                )
            };
            let contours: Vec<Vec<DVec3>> = pen
                .contours
                .iter()
                .map(|c| c.iter().map(|&p| place(p)).collect())
                .collect();
            if !contours.is_empty() {
                out.push(contours);
            }
        }
        pen_x += adv.advance_width(gid).unwrap_or(0.0) as f64;
    }
    Ok((out, missing))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Even-odd nesting depth of each contour (0 = outermost).
    fn depths(cs: &[Vec<DVec3>]) -> Vec<usize> {
        let inside = |p: DVec3, poly: &[DVec3]| {
            let mut c = false;
            for (i, a) in poly.iter().enumerate() {
                let b = poly[(i + 1) % poly.len()];
                if (a.y > p.y) != (b.y > p.y) && p.x < a.x + (p.y - a.y) * (b.x - a.x) / (b.y - a.y)
                {
                    c = !c;
                }
            }
            c
        };
        cs.iter()
            .enumerate()
            .map(|(i, c)| {
                cs.iter()
                    .enumerate()
                    .filter(|(j, o)| *j != i && inside(c[0], o))
                    .count()
            })
            .collect()
    }

    #[test]
    fn ijc_08_glyphs_have_their_counters() {
        let h = 0.002;
        let (glyphs, missing) = outline_text("IJC 08", DVec3::ZERO, h).unwrap();
        assert!(missing.is_empty());
        // Space draws nothing: I, J, C, 0, 8.
        assert_eq!(glyphs.len(), 5);
        let counts: Vec<usize> = glyphs.iter().map(|g| g.len()).collect();
        // I/J/C are one solid outline each; 8 has two counters.
        assert_eq!(&counts[..3], &[1, 1, 1]);
        assert_eq!(counts[4], 3, "8: outer + 2 counters");
        assert!(counts[3] >= 2, "0 has a counter");
        let d8 = depths(&glyphs[4]);
        assert_eq!(d8.iter().filter(|&&d| d == 1).count(), 2, "{d8:?}");
        let d0 = depths(&glyphs[3]);
        assert_eq!(d0.iter().filter(|&&d| d == 0).count(), 1, "{d0:?}");
        // Cap height of "I" equals the text height; glyphs advance left→right.
        let ys = glyphs[0][0].iter().map(|p| p.y);
        let (lo, hi) = ys.fold((f64::MAX, f64::MIN), |(a, b), y| (a.min(y), b.max(y)));
        assert!(lo.abs() < 1e-9 && (hi - h).abs() < 1e-6, "{lo} {hi}");
        let x0 = |g: &Glyph| g[0].iter().map(|p| p.x).fold(f64::MAX, f64::min);
        assert!(glyphs.windows(2).all(|w| x0(&w[0]) < x0(&w[1])));
    }

    #[test]
    fn missing_glyphs_are_reported() {
        let (_, missing) = outline_text("A\u{10FFFD}", DVec3::ZERO, 0.01).unwrap();
        assert_eq!(missing, vec!['\u{10FFFD}']);
        assert!(outline_text("A", DVec3::ZERO, 0.0).is_err());
    }
}
