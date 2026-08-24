// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! Generate the ItsJustCAD app icon PNGs at standard sizes.
//!
//! Run: cargo run -p itsjustcad --example gen_icon
//!
//! Source: `assets/logo/logo-appstore.png` — the white tuxedo-cat brand mark
//! on a black square (1254×1254). We resize it (square, high-quality Lanczos)
//! into `assets/icon/{16,32,64,128,256,512,1024}.png`.
//!
//! After running this, build the macOS `.icns` with:
//!   cargo run -p itsjustcad --example gen_icon && \
//!   iconutil -c icns -o assets/icon/icon.icns <iconset>
//! (the example prints the exact commands, and writes an `.iconset` dir ready
//! for `iconutil`).

use image::imageops::FilterType;

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
    let out_dir = repo_root.join("assets/icon");
    std::fs::create_dir_all(&out_dir).expect("create assets/icon");

    let src = image::open(&src_path)
        .unwrap_or_else(|e| panic!("open {src_path:?}: {e}"))
        .to_rgba8();
    let (w, h) = (src.width(), src.height());
    assert_eq!(w, h, "source logo must be square (got {w}×{h})");

    for &size in sizes {
        // Lanczos3 gives clean downscales of the crisp brand mark.
        let resized = image::imageops::resize(&src, size, size, FilterType::Lanczos3);
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
        let resized = image::imageops::resize(&src, px, px, FilterType::Lanczos3);
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
    println!();
    println!("Now build the .icns:");
    println!(
        "  iconutil -c icns -o {:?} {:?}",
        out_dir.join("icon.icns"),
        iconset
    );
    println!("Then: scripts/bundle-macos.sh to build the .app.");
}
