// build.rs — embed the app icon into the Windows executable (issue #19).
//
// On non-Windows targets this script is a no-op, so Linux/macOS builds pay
// nothing beyond compiling this tiny script. For Windows targets it:
//   1. synthesizes a multi-size icon.ico in OUT_DIR from the PNG sources in
//      assets/icon/ (so new artwork flows through automatically), and
//   2. embeds it as the main icon resource via `winresource`.

use std::env;
use std::fs;
use std::path::Path;

mod icon {
    include!("build_support/icon.rs");
}

/// Icon sizes shipped in assets/icon/ that go into the .ico.
/// Explorer/taskbar pick the nearest size; 16–256 covers every Windows DPI.
const ICO_SIZES: [u32; 5] = [16, 32, 64, 128, 256];

fn main() {
    println!("cargo:rerun-if-changed=build_support/icon.rs");
    for px in ICO_SIZES {
        println!("cargo:rerun-if-changed=../../assets/icon/{px}.png");
    }

    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    let manifest_dir = env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR not set");
    let out_dir = env::var("OUT_DIR").expect("OUT_DIR not set");

    let pngs: Vec<Vec<u8>> = ICO_SIZES
        .iter()
        .map(|px| {
            // Icon PNGs live at the workspace root (shared with gen_icon.rs
            // and the macOS/Linux packaging), two levels up from this crate.
            let path = Path::new(&manifest_dir).join(format!("../../assets/icon/{px}.png"));
            fs::read(&path).unwrap_or_else(|e| panic!("failed to read {}: {e}", path.display()))
        })
        .collect();
    let png_refs: Vec<&[u8]> = pngs.iter().map(Vec::as_slice).collect();

    let ico_bytes = icon::build_ico(&png_refs).expect("failed to synthesize icon.ico");
    let ico_path = Path::new(&out_dir).join("icon.ico");
    fs::write(&ico_path, ico_bytes)
        .unwrap_or_else(|e| panic!("failed to write {}: {e}", ico_path.display()));

    let mut res = winresource::WindowsResource::new();
    res.set_icon(ico_path.to_str().expect("OUT_DIR path is not valid UTF-8"));

    if let Err(e) = res.compile() {
        if cfg!(windows) {
            // Native Windows build (CI release leg): a missing icon resource
            // is a real regression — fail loudly.
            panic!("winresource failed to embed icon.ico: {e}");
        }
        // Cross-checking from macOS/Linux without a Windows resource compiler
        // (rc.exe / llvm-rc / windres): skip gracefully so `cargo check
        // --target x86_64-pc-windows-msvc` stays usable on dev machines.
        println!("cargo:warning=skipping Windows icon resource (no resource compiler on this host): {e}");
    }
}
