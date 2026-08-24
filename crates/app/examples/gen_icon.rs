//! Generate mydrafter app icon PNGs at standard sizes.
//!
//! Run: cargo run -p mydrafter --example gen_icon
//!
//! Outputs: assets/icon/{16,32,128,256,512,1024}.png
//!
//! Design: dark navy rounded square (#1b1c2e), white "mD" lettermark drawn
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

    // 2. Draw "mD" lettermark centred on the icon.
    draw_md(&mut img, size, stroke, FG);

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

/// Draw "mD" lettermark with thick strokes.
///
/// Each letter is defined as a set of thick line segments (x0,y0)→(x1,y1)
/// in normalised [0,1] coordinates, then scaled to the icon.
fn draw_md(img: &mut RgbaImage, size: u32, stroke: u32, colour: Rgba<u8>) {
    let s = size as f32;
    // Letter block occupies the middle 60 % of the icon, centred.
    let margin = s * 0.18;
    let block_w = s - margin * 2.0;
    let block_h = s - margin * 2.0;

    // ── m (left half of block) ───────────────────────────────────────────
    // Normalised within the m-cell ([0,1]×[0,1]):
    //   left stem:  (0,0)→(0,1)
    //   arch-left:  (0,0)→(0.5,0.5)
    //   arch-right: (0.5,0.5)→(1,0)  (but only left half, so stop at 0.5)
    //   right stem: (0.5,0.5)→(0.5,1)
    // We simplify to avoid complexity at small sizes: just two arches as V strokes.

    let letter_gap = block_w * 0.05; // gap between m and D
    let m_w = block_w * 0.50 - letter_gap / 2.0;
    let d_w = block_w * 0.50 - letter_gap / 2.0;
    let m_x = margin;
    let d_x = margin + m_w + letter_gap;
    let top_y = margin;
    let bot_y = margin + block_h;

    // Helper: draw a thick line segment on the image.
    let draw_seg = |img: &mut RgbaImage, x0: f32, y0: f32, x1: f32, y1: f32| {
        thick_line(img, x0, y0, x1, y1, stroke, colour);
    };

    // --- m ---
    // left stem
    draw_seg(img, m_x, top_y, m_x, bot_y);
    // first arch peak → down to mid
    let mid_y = top_y + block_h * 0.45;
    let m_mid = m_x + m_w * 0.5;
    let m_r = m_x + m_w;
    draw_seg(img, m_x, top_y + block_h * 0.05, m_mid, mid_y);
    // second V: right side of arch
    draw_seg(img, m_mid, mid_y, m_r, top_y + block_h * 0.05);
    // right stem of m
    draw_seg(img, m_r, top_y + block_h * 0.05, m_r, bot_y);

    // --- D ---
    // vertical bar (left edge of D)
    draw_seg(img, d_x, top_y, d_x, bot_y);
    // top horizontal serif
    draw_seg(img, d_x, top_y, d_x + d_w * 0.6, top_y);
    // bottom horizontal serif
    draw_seg(img, d_x, bot_y, d_x + d_w * 0.6, bot_y);
    // curve: approximate the D arc with 3 line segments
    let d_cx = d_x + d_w * 0.55;
    let d_mid_y = (top_y + bot_y) / 2.0;
    let d_right = d_x + d_w;
    draw_seg(img, d_x + d_w * 0.6, top_y, d_right, top_y + block_h * 0.25);
    draw_seg(img, d_right, top_y + block_h * 0.25, d_right, bot_y - block_h * 0.25);
    draw_seg(img, d_right, bot_y - block_h * 0.25, d_x + d_w * 0.6, bot_y);
    let _ = d_cx;
    let _ = d_mid_y;
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
