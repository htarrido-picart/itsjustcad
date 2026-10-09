// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! Every WGSL shader must parse and validate with naga (the same front end
//! wgpu uses), so a bad shader edit fails in `cargo test` instead of at app
//! startup on the GPU.

use naga::valid::{Capabilities, ValidationFlags, Validator};

fn validate(name: &str, source: &str) {
    let module = naga::front::wgsl::parse_str(source)
        .unwrap_or_else(|e| panic!("{name}: WGSL parse error: {e}"));
    Validator::new(ValidationFlags::all(), Capabilities::all())
        .validate(&module)
        .unwrap_or_else(|e| panic!("{name}: WGSL validation error: {e:?}"));
}

#[test]
fn all_shaders_parse_and_validate() {
    for (name, source) in [
        ("grid.wgsl", include_str!("../src/shaders/grid.wgsl")),
        ("mesh.wgsl", include_str!("../src/shaders/mesh.wgsl")),
        ("curve.wgsl", include_str!("../src/shaders/curve.wgsl")),
        ("underlay.wgsl", include_str!("../src/shaders/underlay.wgsl")),
        ("control.wgsl", include_str!("../src/shaders/control.wgsl")),
        ("pano.wgsl", include_str!("../src/shaders/pano.wgsl")),
    ] {
        validate(name, source);
    }
}

#[test]
fn grid_shader_guards_fwidth_divisions() {
    // Regression guard for the dotted-axis bug: every division by a screen
    // derivative in grid.wgsl must clamp the denominator, otherwise a zero
    // derivative yields NaN and the axis/grid pixel is discarded (renders as
    // dotted lines).
    let src = include_str!("../src/shaders/grid.wgsl");
    for line in src.lines() {
        if line.contains("/ fwidth(") {
            panic!("unguarded fwidth division in grid.wgsl: `{}`", line.trim());
        }
    }
    assert!(
        src.contains("max(fwidth("),
        "grid.wgsl should clamp fwidth denominators"
    );
}
