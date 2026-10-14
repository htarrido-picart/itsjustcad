// SPDX-License-Identifier: AGPL-3.0-only
// Copyright © 2026 Hector Tarrido-Picart

//! Generate the ItsJustCAD app icon PNGs at standard sizes.
//!
//! Run: cargo run -p itsjustcad --example gen_icon
//!
//! Source: `assets/logo/logo-appstore.png` — the white line-art smoking-cat
//! brand mark on a black square (flat, full-bleed, no alpha). This example:
//!
//! * composes the flat art onto a transparent canvas as a macOS-style
//!   squircle (rounded rect on the Apple icon grid: ~100 px margin and
//!   ~185 px corner radius at 1024), then resizes it (Lanczos) into
//!   `assets/icon/{16,32,64,128,256,512,1024}.png` and an `.iconset` dir;
//! * regenerates the README logos `assets/logo/logo-squircle.png` and
//!   `logo-squircle-dark.png` (full-bleed rounded corners; the art is
//!   white-on-black so light and dark renders are identical) and
//!   `logo-transparent.png` (black background removed).
//!
//! After running this, build the macOS `.icns` with:
//!   iconutil -c icns -o assets/icon/icon.icns <iconset>
//! (the example prints the exact command).

use image::imageops::FilterType;
use image::{Rgba, RgbaImage};

/// Apple macOS icon grid, expressed as ratios of the canvas size:
/// on a 1024 canvas the visible squircle is 824×824 (100 px margin all
/// around) with a corner radius of ~185.4 px.
const MACOS_INNER_RATIO: f32 = 824.0 / 1024.0;
const MACOS_RADIUS_RATIO: f32 = 185.4 / 824.0; // relative to the inner square

/// Full-bleed rounded-corner ratio used for the README squircle logos
/// (matches the iOS icon corner ratio, ~22.37% of the side).
const FULL_BLEED_RADIUS_RATIO: f32 = 0.2237;

/// Antialiased coverage (0..=1) of an axis-aligned rounded rectangle for the
/// pixel centered at (`x + 0.5`, `y + 0.5`). The rect spans
/// `[x0, x0 + w] × [y0, y0 + h]` with corner radius `r`, via a signed
/// distance function smoothed over one pixel.
fn rounded_rect_coverage(x: u32, y: u32, x0: f32, y0: f32, w: f32, h: f32, r: f32) -> f32 {
    let (px, py) = (x as f32 + 0.5, y as f32 + 0.5);
    let (cx, cy) = (x0 + w / 2.0, y0 + h / 2.0);
    let (hx, hy) = (w / 2.0 - r, h / 2.0 - r);
    let qx = (px - cx).abs() - hx;
    let qy = (py - cy).abs() - hy;
    let outside = (qx.max(0.0).powi(2) + qy.max(0.0).powi(2)).sqrt();
    let inside = qx.max(qy).min(0.0);
    let d = outside + inside - r; // signed distance: <0 inside, >0 outside
    (0.5 - d).clamp(0.0, 1.0)
}

/// Compose flat square art onto a transparent `canvas`×`canvas` image as a
/// macOS-style squircle: the art is resized to the Apple icon-grid inner
/// square, centered, and alpha-masked to a rounded rect with antialiased
/// edges. Panics if `src` is not square.
pub fn macos_squircle_icon(src: &RgbaImage, canvas: u32) -> RgbaImage {
    assert_eq!(src.width(), src.height(), "icon source must be square");
    let inner = (canvas as f32 * MACOS_INNER_RATIO).round() as u32;
    let margin = ((canvas - inner) / 2) as f32;
    let radius = inner as f32 * MACOS_RADIUS_RATIO;
    let art = image::imageops::resize(src, inner, inner, FilterType::Lanczos3);

    let mut out = RgbaImage::from_pixel(canvas, canvas, Rgba([0, 0, 0, 0]));
    for y in 0..canvas {
        for x in 0..canvas {
            let cov =
                rounded_rect_coverage(x, y, margin, margin, inner as f32, inner as f32, radius);
            if cov <= 0.0 {
                continue;
            }
            let ax = (x as f32 - margin).clamp(0.0, (inner - 1) as f32) as u32;
            let ay = (y as f32 - margin).clamp(0.0, (inner - 1) as f32) as u32;
            let Rgba([r, g, b, a]) = *art.get_pixel(ax, ay);
            let alpha = (a as f32 * cov).round() as u8;
            out.put_pixel(x, y, Rgba([r, g, b, alpha]));
        }
    }
    out
}

/// Round the corners of `src` in place (full bleed, no margin) with an
/// antialiased rounded-rect alpha mask of radius `radius` pixels.
pub fn round_corners(src: &mut RgbaImage, radius: f32) {
    let (w, h) = (src.width() as f32, src.height() as f32);
    for y in 0..src.height() {
        for x in 0..src.width() {
            let cov = rounded_rect_coverage(x, y, 0.0, 0.0, w, h, radius);
            let p = src.get_pixel_mut(x, y);
            p.0[3] = (p.0[3] as f32 * cov).round() as u8;
        }
    }
}

/// Knock out the black background of white-on-black line art: alpha becomes
/// the pixel luminance and the color becomes pure white, so the mark reads
/// on any background that isn't white itself.
pub fn black_to_transparent(src: &RgbaImage) -> RgbaImage {
    let mut out = src.clone();
    for p in out.pixels_mut() {
        let Rgba([r, g, b, a]) = *p;
        let lum = (0.2126 * r as f32 + 0.7152 * g as f32 + 0.0722 * b as f32).round() as u8;
        let alpha = ((lum as u32 * a as u32 + 127) / 255) as u8;
        *p = Rgba([255, 255, 255, alpha]);
    }
    out
}

fn main() {
    let sizes: &[u32] = &[16, 32, 64, 128, 256, 512, 1024];

    let repo_root = std::path::PathBuf::from(
        std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".to_owned()),
    )
    .parent()
    .unwrap()
    .parent()
    .unwrap()
    .to_path_buf();

    let src_path = repo_root.join("assets/logo/logo-appstore.png");
    let logo_dir = repo_root.join("assets/logo");
    let out_dir = repo_root.join("assets/icon");
    std::fs::create_dir_all(&out_dir).expect("create assets/icon");

    let src = image::open(&src_path)
        .unwrap_or_else(|e| panic!("open {src_path:?}: {e}"))
        .to_rgba8();
    let (w, h) = (src.width(), src.height());
    assert_eq!(w, h, "source logo must be square (got {w}×{h})");

    // macOS app icon: squircle on transparent canvas, masked once at 1024
    // then Lanczos-resized down (keeps the edge antialiasing consistent).
    let master = macos_squircle_icon(&src, 1024);
    for &size in sizes {
        let resized = image::imageops::resize(&master, size, size, FilterType::Lanczos3);
        let path = out_dir.join(format!("{size}.png"));
        resized
            .save(&path)
            .unwrap_or_else(|e| panic!("save {path:?}: {e}"));
        println!("wrote {path:?}");
    }

    // Build a macOS .iconset directory so `iconutil` can produce icon.icns.
    // iconutil expects icon_<px>x<px>[@2x].png names.
    let iconset = out_dir.join("ItsJustCAD.iconset");
    std::fs::create_dir_all(&iconset).expect("create iconset");
    // (base_px, is_2x)
    let iconset_entries: &[(u32, bool)] = &[
        (16, false),
        (16, true),
        (32, false),
        (32, true),
        (128, false),
        (128, true),
        (256, false),
        (256, true),
        (512, false),
        (512, true),
    ];
    for &(base, is_2x) in iconset_entries {
        let px = if is_2x { base * 2 } else { base };
        let resized = image::imageops::resize(&master, px, px, FilterType::Lanczos3);
        let name = if is_2x {
            format!("icon_{base}x{base}@2x.png")
        } else {
            format!("icon_{base}x{base}.png")
        };
        let path = iconset.join(name);
        resized
            .save(&path)
            .unwrap_or_else(|e| panic!("save {path:?}: {e}"));
    }
    println!("wrote iconset dir {iconset:?}");

    // README logos: full-bleed rounded corners. The art is white-on-black,
    // so the light and dark variants are the same render.
    let mut squircle = src.clone();
    round_corners(&mut squircle, w as f32 * FULL_BLEED_RADIUS_RATIO);
    for name in ["logo-squircle.png", "logo-squircle-dark.png"] {
        let path = logo_dir.join(name);
        squircle
            .save(&path)
            .unwrap_or_else(|e| panic!("save {path:?}: {e}"));
        println!("wrote {path:?}");
    }

    // White-on-transparent mark (black background knocked out).
    let transparent = black_to_transparent(&src);
    let path = logo_dir.join("logo-transparent.png");
    transparent
        .save(&path)
        .unwrap_or_else(|e| panic!("save {path:?}: {e}"));
    println!("wrote {path:?}");

    println!();
    println!("Now build the .icns:");
    println!(
        "  iconutil -c icns -o {:?} {:?}",
        out_dir.join("icon.icns"),
        iconset
    );
    println!("Then: scripts/bundle-macos.sh to build the .app.");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flat_black(size: u32) -> RgbaImage {
        RgbaImage::from_pixel(size, size, Rgba([0, 0, 0, 255]))
    }

    #[test]
    fn macos_squircle_is_square_and_sized() {
        let out = macos_squircle_icon(&flat_black(512), 1024);
        assert_eq!(out.width(), 1024);
        assert_eq!(out.height(), 1024);
    }

    #[test]
    fn macos_squircle_corners_transparent_center_opaque() {
        let out = macos_squircle_icon(&flat_black(1024), 1024);
        // Canvas corners and the margin band are fully transparent.
        for &(x, y) in &[(0, 0), (1023, 0), (0, 1023), (1023, 1023), (50, 512)] {
            assert_eq!(out.get_pixel(x, y).0[3], 0, "expected transparent at ({x},{y})");
        }
        // Center and points just inside the inner square are fully opaque.
        for &(x, y) in &[(512, 512), (512, 110), (110, 512)] {
            assert_eq!(out.get_pixel(x, y).0[3], 255, "expected opaque at ({x},{y})");
        }
        // Inner-square corner (inside margin, outside the rounded corner)
        // is clipped by the radius.
        assert_eq!(out.get_pixel(105, 105).0[3], 0);
    }

    #[test]
    fn macos_squircle_edge_is_antialiased() {
        let out = macos_squircle_icon(&flat_black(1024), 1024);
        // The straight edges can land exactly on pixel boundaries, but the
        // rounded corner arc must produce partially-covered pixels.
        let partial = (100..300)
            .flat_map(|y| (100..300).map(move |x| (x, y)))
            .any(|(x, y)| {
                let a = out.get_pixel(x, y).0[3];
                a > 0 && a < 255
            });
        assert!(partial, "expected antialiased pixels on the corner arc");
    }

    #[test]
    fn round_corners_full_bleed() {
        let mut img = flat_black(256);
        round_corners(&mut img, 256.0 * FULL_BLEED_RADIUS_RATIO);
        assert_eq!(img.get_pixel(0, 0).0[3], 0, "corner must be transparent");
        assert_eq!(img.get_pixel(128, 128).0[3], 255, "center must be opaque");
        assert_eq!(img.get_pixel(128, 0).0[3], 255, "edge midpoint must be opaque");
    }

    #[test]
    fn black_to_transparent_knocks_out_background() {
        let mut img = flat_black(4);
        img.put_pixel(1, 1, Rgba([255, 255, 255, 255]));
        img.put_pixel(2, 2, Rgba([128, 128, 128, 255]));
        let out = black_to_transparent(&img);
        assert_eq!(out.get_pixel(0, 0).0[3], 0, "black becomes transparent");
        assert_eq!(*out.get_pixel(1, 1), Rgba([255, 255, 255, 255]));
        let mid = out.get_pixel(2, 2);
        assert_eq!(&mid.0[..3], &[255, 255, 255], "color forced to white");
        assert!(mid.0[3] > 0 && mid.0[3] < 255, "gray becomes partial alpha");
    }

    #[test]
    fn coverage_inside_outside() {
        // 100×100 rect at origin, radius 20.
        assert_eq!(rounded_rect_coverage(50, 50, 0.0, 0.0, 100.0, 100.0, 20.0), 1.0);
        assert_eq!(rounded_rect_coverage(200, 200, 0.0, 0.0, 100.0, 100.0, 20.0), 0.0);
        // Square corner of the bounding box is outside the rounded corner.
        assert_eq!(rounded_rect_coverage(0, 0, 0.0, 0.0, 100.0, 100.0, 20.0), 0.0);
    }
}
