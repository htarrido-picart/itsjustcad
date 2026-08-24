// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! Generate ItsJustCAD app icon PNGs at standard sizes.
//!
//! Run: cargo run -p itsjustcad --example gen_icon
//!
//! Outputs: assets/icon/{16,32,128,256,512,1024}.png
//!
//! Design: dark navy rounded square (#1b1c2e), white "iJ" lettermark drawn
//! with thick strokes. Simple, scales well at every size.

use image::{ImageBuffer, Rgba, RgbaImage};

fn main() {
    let sizes: &[u32] = &[16, 32, 128, 256, 512, 1024];
    let out_dir = std::path::PathBuf::from(
        std::env::var("CARGO_MANIFEST_DIR")
            .unwrap_or_else(|_| ".".to_owned()),
    )
    .parent()
    .unwrap()
    .parent()
    .unwrap()
    .join("assets/icon");

    std::fs::create_dir_all(&out_dir).expect("create assets/icon");

    for &size in sizes {
        let img = render_icon(size);
        let path = out_dir.join(format!("{size}.png"));
        img.save(&path).unwrap_or_else(|e| panic!("save {path:?}: {e}"));
        println!("wrote {path:?}");
    }

    println!("Done. Run scripts/bundle-macos.sh to build the .app.");
}

/// RGBA background colour: dark navy #1b1c2e.
const BG: Rgba<u8> = Rgba([0x1b, 0x1c, 0x2e, 0xff]);
/// Foreground: white.
const FG: Rgba<u8> = Rgba([0xff, 0xff, 0xff, 0xff]);
/// Transparent.
const CLEAR: Rgba<u8> = Rgba([0, 0, 0, 0]);

fn render_icon(size: u32) -> RgbaImage {
    let mut img: RgbaImage = ImageBuffer::from_pixel(size, size, CLEAR);

    let s = size as f32;
    // Corner radius: ~22 % of size for a "squircle" feel.
    let r = (s * 0.22).round();
    // Stroke weight: ~10 % of size, min 1 px.
    let stroke = ((s * 0.10) as u32).max(1);

    // 1. Fill rounded-rect background.
    fill_rounded_rect(&mut img, 0, 0, size, size, r as u32, BG);

    // 2. Draw "iJ" lettermark centred on the icon.
    draw_ij(&mut img, size, stroke, FG);

    img
}

/// Fill every pixel inside a rounded rectangle with `colour`.
fn fill_rounded_rect(img: &mut RgbaImage, x0: u32, y0: u32, x1: u32, y1: u32, r: u32, colour: Rgba<u8>) {
    let (cx0, cy0) = (x0 + r, y0 + r);
    let (cx1, cy1) = (x1 - r, y1 - r);
    let r2 = (r * r) as f32;

    for py in y0..y1 {
        for px in x0..x1 {
            // Distance to the nearest corner centre.
            let in_corner = |cx: u32, cy: u32| -> bool {
                let dx = px as f32 - cx as f32;
                let dy = py as f32 - cy as f32;
                dx * dx + dy * dy <= r2
            };
            let inside = if px < cx0 && py < cy0 {
                in_corner(cx0, cy0)
            } else if px >= cx1 && py < cy0 {
                in_corner(cx1, cy0)
            } else if px < cx0 && py >= cy1 {
                in_corner(cx0, cy1)
            } else if px >= cx1 && py >= cy1 {
                in_corner(cx1, cy1)
            } else {
                true
            };
            if inside {
                img.put_pixel(px, py, colour);
            }
        }
    }
}

/// Draw "iJ" lettermark with thick strokes.
///
/// Each letter is a set of thick line segments (x0,y0)→(x1,y1) scaled to the
/// icon. `i` is a dot + stem on the left; `J` is a stem with a hook on the
/// right.
fn draw_ij(img: &mut RgbaImage, size: u32, stroke: u32, colour: Rgba<u8>) {
    let s = size as f32;
    // Letter block occupies the middle ~64 % of the icon, centred.
    let margin = s * 0.18;
    let block_w = s - margin * 2.0;
    let block_h = s - margin * 2.0;

    let letter_gap = block_w * 0.12; // gap between i and J
    let i_w = block_w * 0.30;
    let j_w = block_w * 0.58 - letter_gap;
    let i_x = margin;
    let j_x = margin + i_w + letter_gap;
    let top_y = margin;
    let bot_y = margin + block_h;

    // The dot of the i sits above the stem; the stem starts below it.
    let dot_y = top_y;
    let stem_top = top_y + block_h * 0.28;

    let draw_seg = |img: &mut RgbaImage, x0: f32, y0: f32, x1: f32, y1: f32| {
        thick_line(img, x0, y0, x1, y1, stroke, colour);
    };

    // --- i ---
    let i_cx = i_x + i_w * 0.5;
    // dot: a very short thick segment reads as a square dot at every size.
    draw_seg(img, i_cx, dot_y, i_cx, dot_y + stroke as f32 * 0.6);
    // stem
    draw_seg(img, i_cx, stem_top, i_cx, bot_y);

    // --- J ---
    let j_cx = j_x + j_w * 0.62;
    // top serif (short horizontal cap)
    draw_seg(img, j_cx - j_w * 0.28, top_y, j_cx + j_w * 0.28, top_y);
    // vertical stem
    draw_seg(img, j_cx, top_y, j_cx, bot_y - block_h * 0.14);
    // hook: two segments sweeping down-left to a foot
    let hook_y = bot_y - block_h * 0.14;
    let foot_x = j_x + j_w * 0.06;
    draw_seg(img, j_cx, hook_y, (j_cx + foot_x) / 2.0, bot_y);
    draw_seg(img, (j_cx + foot_x) / 2.0, bot_y, foot_x, bot_y - block_h * 0.06);
}

/// Rasterise a thick line segment from (x0,y0) to (x1,y1) with given pixel `width`.
fn thick_line(img: &mut RgbaImage, x0: f32, y0: f32, x1: f32, y1: f32, width: u32, colour: Rgba<u8>) {
    let (iw, ih) = img.dimensions();
    let dx = x1 - x0;
    let dy = y1 - y0;
    let len = (dx * dx + dy * dy).sqrt().max(1e-6);
    // Unit perpendicular.
    let px = -dy / len;
    let py = dx / len;
    let hw = width as f32 / 2.0;

    // Bounding box.
    let xs = [x0, x1];
    let ys = [y0, y1];
    let xmin = xs.iter().cloned().fold(f32::INFINITY, f32::min) - hw - 1.0;
    let xmax = xs.iter().cloned().fold(f32::NEG_INFINITY, f32::max) + hw + 1.0;
    let ymin = ys.iter().cloned().fold(f32::INFINITY, f32::min) - hw - 1.0;
    let ymax = ys.iter().cloned().fold(f32::NEG_INFINITY, f32::max) + hw + 1.0;

    let x0i = (xmin as i32).max(0) as u32;
    let x1i = (xmax as i32 + 1).min(iw as i32) as u32;
    let y0i = (ymin as i32).max(0) as u32;
    let y1i = (ymax as i32 + 1).min(ih as i32) as u32;

    for py_px in y0i..y1i {
        for px_px in x0i..x1i {
            let cx = px_px as f32 + 0.5;
            let cy = py_px as f32 + 0.5;
            // Project pixel centre onto the segment.
            let along = (cx - x0) * dx / len + (cy - y0) * dy / len;
            let t = along.clamp(0.0, len);
            let qx = x0 + dx / len * t;
            let qy = y0 + dy / len * t;
            let dist = ((cx - qx) * (cx - qx) + (cy - qy) * (cy - qy)).sqrt();
            if dist <= hw {
                img.put_pixel(px_px, py_px, colour);
            }
        }
    }
    let _ = (px, py);
}
