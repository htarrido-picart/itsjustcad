// SPDX-License-Identifier: AGPL-3.0-only
// Copyright © 2026 Hector Tarrido-Picart

//! Hardware ray tracing for analysis shadow rays (#317, spec #261).
//!
//! [`ShadowCaster`] answers batches of `TriBvh::ray_occluded` queries on the
//! GPU (Metal intersection queries over a bounding-box acceleration
//! structure) and returns exactly what the CPU would: rays the GPU cannot
//! classify for certain are re-run on the CPU, and every `true` is confirmed
//! there with one exact test. The exactness argument is in [`margin`].
//!
//! Gate: macOS / Mac Catalyst builds, an Apple9-family GPU that supports ray
//! tracing, and `ITSJUSTCAD_HWRT` not set to `0`/`off`/`cpu`. Everywhere else
//! [`ShadowCaster::new`] returns `None` and callers keep the CPU path.
//! `ITSJUSTCAD_HWRT_STATS=1` prints per-batch counts and timings to stderr.

use glam::DVec3;
#[cfg(not(any(target_os = "macos", all(target_os = "ios", target_abi = "macabi"))))]
use kernel_mesh::TriBvh;

pub mod margin;

#[cfg(any(target_os = "macos", all(target_os = "ios", target_abi = "macabi")))]
pub mod metal;

#[cfg(any(target_os = "macos", all(target_os = "ios", target_abi = "macabi")))]
mod caster;
#[cfg(any(target_os = "macos", all(target_os = "ios", target_abi = "macabi")))]
pub use caster::ShadowCaster;

/// Did the environment force the CPU path (`ITSJUSTCAD_HWRT=0|off|cpu`)?
pub fn forced_cpu() -> bool {
    std::env::var("ITSJUSTCAD_HWRT").is_ok_and(|v| is_off(&v))
}

fn is_off(v: &str) -> bool {
    matches!(v.trim().to_ascii_lowercase().as_str(), "0" | "off" | "cpu" | "false")
}

#[cfg(test)]
#[test]
fn override_values() {
    assert!(is_off("cpu") && is_off(" OFF ") && is_off("0") && is_off("false"));
    assert!(!is_off("") && !is_off("1") && !is_off("on"));
}

/// The occlusion query callers hand to per-item analysis code.
pub type Occluded<'a> = dyn FnMut(DVec3, DVec3) -> bool + 'a;

/// No hardware ray tracing on this target: [`ShadowCaster::new`] is always
/// `None`.
#[cfg(not(any(target_os = "macos", all(target_os = "ios", target_abi = "macabi"))))]
pub struct ShadowCaster {
    never: std::convert::Infallible,
}

#[cfg(not(any(target_os = "macos", all(target_os = "ios", target_abi = "macabi"))))]
impl ShadowCaster {
    pub fn new(_bvh: &TriBvh, _dirs: Option<&[DVec3]>) -> Option<Self> {
        None
    }

    pub fn map<I: Sync, T: Send>(
        &self,
        _bvh: &TriBvh,
        _items: &[I],
        _f: impl Fn(&I, &mut Occluded<'_>) -> T + Sync,
    ) -> Vec<T> {
        match self.never {}
    }
}
