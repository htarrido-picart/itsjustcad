//! Icon packaging tests (issue #19): the .ico synthesis used by build.rs and
//! the Linux desktop entry shipped in the release tarball.

use std::fs;
use std::path::PathBuf;

// Same source file build.rs compiles — keeps test and build logic identical.
mod icon {
    include!("../build_support/icon.rs");
}

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// Mirrors ICO_SIZES in crates/app/build.rs.
const ICO_SIZES: [u32; 5] = [16, 32, 64, 128, 256];

#[test]
fn ico_round_trips_from_asset_pngs() {
    let pngs: Vec<Vec<u8>> = ICO_SIZES
        .iter()
        .map(|px| {
            let path = repo_root().join(format!("assets/icon/{px}.png"));
            fs::read(&path).unwrap_or_else(|e| panic!("missing {}: {e}", path.display()))
        })
        .collect();
    let png_refs: Vec<&[u8]> = pngs.iter().map(Vec::as_slice).collect();

    let bytes = icon::build_ico(&png_refs).expect("build_ico failed on real assets");

    // Round-trip: the ico crate itself must parse what we wrote.
    let dir = ico::IconDir::read(std::io::Cursor::new(&bytes)).expect("invalid ICO produced");
    assert_eq!(dir.entries().len(), ICO_SIZES.len());
    for (entry, px) in dir.entries().iter().zip(ICO_SIZES) {
        assert_eq!(entry.width(), px, "entry width mismatch");
        assert_eq!(entry.height(), px, "entry height mismatch");
        let decoded = entry.decode().expect("entry must decode back to an image");
        assert_eq!(decoded.width(), px);
    }
}

#[test]
fn ico_rejects_empty_and_garbage_input() {
    assert!(icon::build_ico(&[]).is_err(), "empty input must error");
    assert!(
        icon::build_ico(&[b"not a png".as_slice()]).is_err(),
        "non-PNG bytes must error"
    );
}

#[test]
fn linux_desktop_entry_has_required_keys() {
    let path = repo_root().join("packaging/linux/itsjustcad.desktop");
    let text = fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("missing {}: {e}", path.display()));

    assert!(text.starts_with("[Desktop Entry]"), "must start with [Desktop Entry]");
    for key in [
        "Type=Application",
        "Name=ItsJustCAD",
        "Exec=itsjustcad",
        "Icon=itsjustcad",
        "Categories=Graphics;Engineering;",
    ] {
        assert!(text.contains(key), "desktop entry missing `{key}`");
    }
}

#[test]
fn linux_install_script_stages_expected_paths() {
    let path = repo_root().join("packaging/linux/install.sh");
    let text = fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("missing {}: {e}", path.display()));

    assert!(text.contains(".local/bin"), "must install binary to ~/.local/bin");
    assert!(text.contains(".local/share/applications"), "must install .desktop");
    assert!(text.contains("icons/hicolor"), "must install hicolor icons");
    assert!(!text.contains("sudo"), "per-user install must not use sudo");
}
