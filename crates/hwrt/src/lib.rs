// SPDX-License-Identifier: AGPL-3.0-only
// Copyright © 2026 Hector Tarrido-Picart

//! Hardware ray tracing for analysis shadow rays (#317, spec #261) and the
//! `raytrace` renderer (#318).
//!
//! [`ShadowCaster`] answers batches of `TriBvh::ray_occluded` queries on the
//! GPU (Metal intersection queries over a bounding-box acceleration
//! structure) and returns exactly what the CPU would: rays the GPU cannot
//! classify for certain are re-run on the CPU, and every `true` is confirmed
//! there with one exact test. The exactness argument is in [`margin`].
//!
//! [`Tracer`] answers batches of f32 closest-hit and any-hit rays for the
//! renderer over a triangle acceleration structure, with no CPU re-check:
//! the image is not logged state (#261 item 3).
//!
//! Gate: macOS / Mac Catalyst builds, an Apple9-family GPU that supports ray
//! tracing, and `ITSJUSTCAD_HWRT` not set to `0`/`off`/`cpu`. Everywhere else
//! [`ShadowCaster::new`] and [`Tracer::new`] return `None` and callers keep
//! the CPU path.
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

#[cfg(any(target_os = "macos", all(target_os = "ios", target_abi = "macabi")))]
mod tracer;
#[cfg(any(target_os = "macos", all(target_os = "ios", target_abi = "macabi")))]
pub use tracer::{Batch, Tracer};

/// Did the environment force the CPU path (`ITSJUSTCAD_HWRT=0|off|cpu`)?
pub fn forced_cpu() -> bool {
    std::env::var("ITSJUSTCAD_HWRT").is_ok_and(|v| is_off(&v))
}

/// Did the environment ask for stats (`ITSJUSTCAD_HWRT_STATS=1`)?
pub fn stats_enabled() -> bool {
    std::env::var_os("ITSJUSTCAD_HWRT_STATS").is_some_and(|v| !v.is_empty() && v != "0")
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

/// Work in flight on [`Tracer`]; never built on this target.
#[cfg(not(any(target_os = "macos", all(target_os = "ios", target_abi = "macabi"))))]
pub struct Batch {
    never: std::convert::Infallible,
}

/// No hardware ray tracing on this target: [`Tracer::new`] is always `None`.
#[cfg(not(any(target_os = "macos", all(target_os = "ios", target_abi = "macabi"))))]
pub struct Tracer {
    never: std::convert::Infallible,
}

#[cfg(not(any(target_os = "macos", all(target_os = "ios", target_abi = "macabi"))))]
impl Tracer {
    pub fn new(_count: usize, _tri: impl Fn(usize) -> [DVec3; 3] + Sync) -> Option<Self> {
        None
    }

    pub fn closest(&self, _rays: &[[DVec3; 2]], _out: &mut [Option<(u32, f32)>]) {
        match self.never {}
    }

    pub fn occluded(&self, _rays: &[[DVec3; 2]], _out: &mut [bool]) {
        match self.never {}
    }

    pub fn submit(&self, _closest: &[&[[DVec3; 2]]], _any: &[&[[DVec3; 2]]]) -> Batch {
        match self.never {}
    }

    pub fn finish(&self, b: Batch, _hits: &mut [Option<(u32, f32)>], _occ: &mut [bool]) {
        match b.never {}
    }

    pub fn report(&self) {
        match self.never {}
    }
}
