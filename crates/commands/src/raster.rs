// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! Raster (JPG) export. The GPU viewport lives in the render/app layer and the
//! commands crate has no wgpu, so headless raster export can't screenshot the
//! live view. Instead we rasterize the SAME projected 2D drawing the SVG writer
//! produces (`svg::project_scene`) onto a CPU RGB buffer and encode it as JPEG
//! via the `image` crate. Top-down orthographic, white paper, layer-coloured
//! strokes — a flat plan render, not a shaded 3D screenshot.
//!
//! A true GPU screenshot (shaded, live camera) belongs in the app's `--shot`
//! path; this is the tractable headless route and needs no new dependency.

use crate::svg::{project_scene, rgb_color};
use image::{codecs::jpeg::JpegEncoder, ExtendedColorType};
use itsjustcad_doc::Document;

/// Longest side of the output image in pixels. Fixed resolution keeps the
/// headless export deterministic; aspect ratio follows the scene bounds.
const MAX_DIM: u32 = 1600;

/// Render the document to a JPEG. Mirrors `svg::export_svg`'s signature:
/// returns `(jpeg_bytes, summary)`. Errors only if JPEG encoding itself fails.
pub fn export_jpg(doc: &Document) -> Result<(Vec<u8>, String), String> {
    let scene = project_scene(doc);
    let (vx, vy, vw, vh) = scene.bounds;

    // Fit the padded scene bounds into an image whose longest side is MAX_DIM,
    // preserving aspect ratio. Guard against degenerate spans (already clamped
    // to a unit box by project_scene, but stay defensive).
    let (vw, vh) = (vw.max(1e-9), vh.max(1e-9));
    let (w, h) = if vw >= vh {
        (MAX_DIM, ((MAX_DIM as f64) * vh / vw).round().max(1.0) as u32)
    } else {
        (((MAX_DIM as f64) * vw / vh).round().max(1.0) as u32, MAX_DIM)
    };

    // White paper background, RGB.
    let mut buf = vec![255u8; (w as usize) * (h as usize) * 3];

    // World (y-up) → pixel (y-down). One uniform scale keeps circles round.
    let scale = ((w as f64) / vw).min((h as f64) / vh);
    let to_px = |px: f64, py: f64| -> (f64, f64) {
        let sx = (px - vx) * scale;
        let sy = (h as f64) - (py - vy) * scale; // flip vertical
        (sx, sy)
    };

    let mut drawn = 0usize;
    for layer in &scene.layers {
        let color = rgb_color(layer.style.color);
        for (a, b, eff_lw) in &layer.segs {
            let (x0, y0) = to_px(a.x, a.y);
            let (x1, y1) = to_px(b.x, b.y);
            // Lineweight (mm) → pixels: mm are meter/1000, scale is px/meter.
            let half = ((eff_lw / 1000.0) * scale * 0.5).max(0.5);
            draw_line(&mut buf, w, h, x0, y0, x1, y1, color, half);
            drawn += 1;
        }
    }

    let mut out = Vec::new();
    JpegEncoder::new_with_quality(&mut out, 90)
        .encode(&buf, w, h, ExtendedColorType::Rgb8)
        .map_err(|e| format!("JPEG encode failed: {e}"))?;

    Ok((
        out,
        format!("JPG {w}x{h}, {drawn} segments across {} layer(s)", scene.layers.iter().filter(|l| !l.segs.is_empty()).count()),
    ))
}

/// Draw a solid-colour line of pixel half-width `half` by stamping a filled
/// square-capped disc along the segment. Simple and dependency-free; adequate
/// for a flat plan export.
#[allow(clippy::too_many_arguments)]
fn draw_line(
    buf: &mut [u8],
    w: u32,
    h: u32,
    x0: f64,
    y0: f64,
    x1: f64,
    y1: f64,
    color: [u8; 3],
    half: f64,
) {
    let dx = x1 - x0;
    let dy = y1 - y0;
    let len = (dx * dx + dy * dy).sqrt();
    // Step ~1px along the segment; stamp a disc at each step so any lineweight
    // renders as a solid stroke.
    let steps = len.ceil().max(1.0) as usize;
    let r = half.ceil() as i32;
    for i in 0..=steps {
        let t = i as f64 / steps as f64;
        let cx = x0 + dx * t;
        let cy = y0 + dy * t;
        let icx = cx.round() as i32;
        let icy = cy.round() as i32;
        for oy in -r..=r {
            for ox in -r..=r {
                if (ox * ox + oy * oy) as f64 > half * half + 0.5 {
                    continue;
                }
                let px = icx + ox;
                let py = icy + oy;
                if px < 0 || py < 0 || px >= w as i32 || py >= h as i32 {
                    continue;
                }
                let idx = ((py as usize) * (w as usize) + px as usize) * 3;
                buf[idx] = color[0];
                buf[idx + 1] = color[1];
                buf[idx + 2] = color[2];
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{parse, Session};

    #[test]
    fn jpg_has_jpeg_magic_and_content() {
        let mut s = Session::default();
        s.run(parse("line 0,0,0 5,0,0").unwrap()).unwrap();
        s.run(parse("box 0,0,0 2,2,2").unwrap()).unwrap();

        let (bytes, summary) = export_jpg(&s.doc).unwrap();
        // JPEG SOI + marker: FF D8 FF.
        assert_eq!(&bytes[0..3], &[0xFF, 0xD8, 0xFF], "JPEG magic bytes");
        assert!(bytes.len() > 100, "non-trivial JPEG: {}", bytes.len());
        assert!(summary.contains("JPG"), "summary mentions JPG: {summary}");
    }

    #[test]
    fn jpg_empty_document_still_encodes() {
        // Empty doc: project_scene clamps to a unit box, so we still emit a
        // valid (blank) JPEG rather than erroring.
        let s = Session::default();
        let (bytes, _) = export_jpg(&s.doc).unwrap();
        assert_eq!(&bytes[0..3], &[0xFF, 0xD8, 0xFF]);
    }
}
