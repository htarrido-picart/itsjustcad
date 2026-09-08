// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! `itsjustcad-raytrace` — a pure-Rust CPU path tracer of the actual CAD model.
//!
//! Part 1 (this crate) is the renderer *core*: primary-ray generation from the
//! viewport camera, ray/triangle intersection accelerated by the shared
//! `kernel-mesh` BVH, PBR BSDFs mapped from the doc's materials, a
//! sun+sky+emissive lighting model, a Monte-Carlo path-tracing integrator with
//! next-event estimation to the sun and Russian-roulette termination, ACES/
//! Reinhard tone mapping, and a **deterministic** ordered `rayon` render loop
//! (seed per pixel+sample → byte-identical output regardless of threading).
//! A progressive API ([`render_progressive`]) accumulates sample passes and
//! reports intermediate frames through a callback with a cancel flag, ready for
//! the part-2 GUI window.
//!
//! Reuse (no reinvention):
//!   * `kernel_mesh::{Bvh, ray_triangle, Aabb}` — acceleration + exact test;
//!   * `itsjustcad_doc::{Document, ObjectMaterial, SunPosition, GeoLocation}` —
//!     geometry, per-object PBR materials, and the SPA sun position;
//!   * `itsjustcad_solar::sun_direction` — az/alt → world sun vector;
//!   * the viewport `OrbitCamera` Z-up / fov conventions (mirrored in
//!     [`camera::Camera`]).

mod build;
mod camera;
mod material;
mod rng;
mod scene;
mod tonemap;

use std::sync::atomic::{AtomicBool, Ordering};

use glam::DVec3;
use rayon::prelude::*;

pub use build::{scene_from_doc, sun_from_doc};
pub use camera::Camera;
pub use material::{Bsdf, Material};
pub use scene::{Scene, SceneBuilder, Sky, Sun};
pub use tonemap::ToneMap;

use rng::Rng;

/// Render settings.
#[derive(Clone, Copy, Debug)]
pub struct Settings {
    pub width: u32,
    pub height: u32,
    /// Total Monte-Carlo samples per pixel.
    pub samples_per_pixel: u32,
    /// Maximum path length (bounces) before termination.
    pub max_bounces: u32,
    /// Global RNG seed; combined with pixel + sample index for determinism.
    pub seed: u64,
    pub tonemap: ToneMap,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            width: 800,
            height: 500,
            samples_per_pixel: 32,
            max_bounces: 6,
            seed: 0x5CAD_1234,
            tonemap: ToneMap::Aces,
        }
    }
}

/// A rendered image: tone-mapped 8-bit RGBA, row-major, top row first.
#[derive(Clone, Debug)]
pub struct Image {
    pub width: u32,
    pub height: u32,
    /// `width * height * 4` bytes, RGBA8.
    pub pixels: Vec<u8>,
}

impl Image {
    /// Save as a PNG file.
    pub fn save_png(&self, path: &std::path::Path) -> Result<(), String> {
        image::save_buffer(
            path,
            &self.pixels,
            self.width,
            self.height,
            image::ColorType::Rgba8,
        )
        .map_err(|e| e.to_string())
    }
}

/// A tile of accumulated (pre-tonemap) HDR radiance, reported to the progressive
/// callback so a UI can show intermediate results.
pub struct PassResult<'a> {
    /// 1-based number of passes accumulated so far.
    pub passes_done: u32,
    /// Total passes planned.
    pub passes_total: u32,
    /// The current tone-mapped image after this pass.
    pub image: &'a Image,
}

/// Trace one primary ray through the scene and return its linear radiance.
fn radiance(scene: &Scene, mut origin: DVec3, mut dir: DVec3, max_bounces: u32, rng: &mut Rng) -> DVec3 {
    let mut throughput = DVec3::ONE;
    let mut result = DVec3::ZERO;
    // Surface offset to avoid self-intersection ("shadow acne"), scaled to the
    // scene size so it works at building and millimetre scales alike.
    let eps = scene
        .bounds
        .map(|b| (b.size().length() * 1e-6).max(1e-6))
        .unwrap_or(1e-6);

    for bounce in 0..=max_bounces {
        let Some(hit) = scene.intersect(origin, dir) else {
            // Escaped to the sky — add its radiance and stop.
            result += throughput * scene.sky.radiance(dir);
            break;
        };

        // Emissive surfaces radiate directly (area lights).
        if hit.material.is_emissive() {
            result += throughput * hit.material.emission;
        }

        let p = hit.point + hit.normal * eps;

        // Next-event estimation: sample the sun for direct light on diffuse
        // surfaces (specular bounces sample it implicitly via the BSDF).
        if let (scene::Bsdf::Diffuse, Some(sun)) = (bsdf_kind(&hit.material), scene.sun) {
            let ldir = sun_sample_dir(sun, rng);
            let ndl = hit.normal.dot(ldir);
            if ndl > 0.0 && !scene.occluded(p, ldir, f64::INFINITY) {
                // Lambertian BRDF = albedo / π; the solid-angle weight of the
                // sun disc is folded into its radiance (treated as a bright
                // directional source), so direct = albedo * radiance * cos.
                result += throughput * hit.material.albedo * sun.radiance
                    * (ndl / std::f64::consts::PI);
            }
        }

        // Sample the BSDF for the next bounce.
        let Some(scat) = hit.material.scatter(dir, hit.normal, rng) else {
            break;
        };
        throughput *= scat.attenuation;
        origin = p;
        dir = scat.dir;

        // Russian roulette after a few bounces: kill low-throughput paths
        // unbiasedly.
        if bounce >= 3 {
            let q = throughput.max_element().clamp(0.05, 0.95);
            if rng.f01() > q {
                break;
            }
            throughput /= q;
        }
    }
    result
}

// Local mirror of the material's lobe so the integrator can special-case
// diffuse NEE without exposing the enum internals.
fn bsdf_kind(m: &Material) -> scene::Bsdf {
    match m.bsdf {
        Bsdf::Diffuse => scene::Bsdf::Diffuse,
        Bsdf::Metal { .. } => scene::Bsdf::Metal,
        Bsdf::Glass { .. } => scene::Bsdf::Glass,
    }
}

/// Pick a shadow-ray direction toward the sun disc (soft shadows via the sun's
/// small angular radius). A cone sample around the sun direction.
fn sun_sample_dir(sun: Sun, rng: &mut Rng) -> DVec3 {
    if sun.angular_radius <= 0.0 {
        return sun.dir;
    }
    // Uniform cone sample around sun.dir with half-angle = angular_radius.
    let cos_max = sun.angular_radius.cos();
    let u1 = rng.f01();
    let u2 = rng.f01();
    let cos_t = 1.0 - u1 * (1.0 - cos_max);
    let sin_t = (1.0 - cos_t * cos_t).max(0.0).sqrt();
    let phi = std::f64::consts::TAU * u2;
    // Orthonormal basis around the sun direction.
    let w = sun.dir;
    let a = if w.x.abs() > 0.9 { DVec3::Y } else { DVec3::X };
    let u = w.cross(a).normalize();
    let v = w.cross(u);
    (u * (sin_t * phi.cos()) + v * (sin_t * phi.sin()) + w * cos_t).normalize()
}

/// Render a single accumulation pass into `accum` (HDR sums), deterministically.
/// Each pixel seeds its own RNG stream from (pixel, sample, global seed), so the
/// ordered `rayon` map is byte-identical to a sequential run.
fn render_pass(scene: &Scene, cam: &Camera, s: &Settings, sample: u32, accum: &mut [DVec3]) {
    let w = s.width as usize;
    let h = s.height as usize;
    accum
        .par_iter_mut()
        .enumerate()
        .for_each(|(idx, cell)| {
            let px = idx % w;
            let py = idx / w;
            let mut rng = Rng::new(idx as u64, sample as u64, s.seed);
            // Jitter within the pixel for anti-aliasing.
            let jx = rng.f01();
            let jy = rng.f01();
            let u = (px as f64 + jx) / w as f64;
            // Image row 0 is the top; camera t=0 is the bottom → flip.
            let v = 1.0 - (py as f64 + jy) / h as f64;
            let dir = cam.ray(u, v);
            *cell += radiance(scene, cam.origin(), dir, s.max_bounces, &mut rng);
        });
}

/// Tone-map an HDR accumulation buffer (summed over `samples` passes) to an RGBA
/// image.
fn resolve(accum: &[DVec3], samples: u32, s: &Settings) -> Image {
    let inv = 1.0 / samples.max(1) as f64;
    let mut pixels = vec![0u8; accum.len() * 4];
    for (i, c) in accum.iter().enumerate() {
        let [r, g, b] = tonemap::to_srgb8(*c * inv, s.tonemap);
        pixels[i * 4] = r;
        pixels[i * 4 + 1] = g;
        pixels[i * 4 + 2] = b;
        pixels[i * 4 + 3] = 255;
    }
    Image { width: s.width, height: s.height, pixels }
}

/// Render the scene to a finished image (all samples, no intermediate callback).
pub fn render(scene: &Scene, cam: &Camera, settings: &Settings) -> Image {
    let dummy = AtomicBool::new(false);
    render_progressive(scene, cam, settings, |_| {}, &dummy)
}

/// Progressive render: accumulate `samples_per_pixel` passes, invoking
/// `on_pass` after each with the current tone-mapped image. Stops early (and
/// returns the partial image) when `cancel` is set. Deterministic: the same
/// scene + settings + seed produce a byte-identical final image every run.
pub fn render_progressive(
    scene: &Scene,
    cam: &Camera,
    settings: &Settings,
    mut on_pass: impl FnMut(PassResult<'_>),
    cancel: &AtomicBool,
) -> Image {
    let n = (settings.width as usize) * (settings.height as usize);
    let mut accum = vec![DVec3::ZERO; n];
    let total = settings.samples_per_pixel.max(1);
    let mut done = 0u32;
    for sample in 0..total {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        render_pass(scene, cam, settings, sample, &mut accum);
        done = sample + 1;
        let image = resolve(&accum, done, settings);
        on_pass(PassResult { passes_done: done, passes_total: total, image: &image });
    }
    // Ensure the returned image reflects however many passes actually ran
    // (even zero, if cancelled before the first pass → a black frame).
    resolve(&accum, done.max(1), settings)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A box scene lit by a sun straight overhead, viewed from an angle.
    fn box_scene() -> (Scene, Camera) {
        let mut b = SceneBuilder::new();
        let m = b.add_material(Material::diffuse(DVec3::splat(0.7)));
        let mesh = kernel_mesh::make_box(DVec3::new(-1.0, -1.0, 0.0), DVec3::new(1.0, 1.0, 2.0));
        b.add_mesh(mesh.positions(), mesh.faces(), m);
        let sun = Sun {
            dir: DVec3::new(0.0, 0.0, 1.0),
            radiance: DVec3::splat(5.0),
            angular_radius: 0.02,
        };
        let scene = b.build(Some(sun), Sky::default());
        let cam = Camera::look_at(
            DVec3::new(6.0, -6.0, 5.0),
            DVec3::new(0.0, 0.0, 1.0),
            DVec3::Z,
            45f64.to_radians(),
            800.0 / 500.0,
        );
        (scene, cam)
    }

    #[test]
    fn deterministic_same_seed_byte_identical() {
        let (scene, cam) = box_scene();
        let s = Settings { width: 32, height: 20, samples_per_pixel: 4, max_bounces: 3, ..Default::default() };
        let a = render(&scene, &cam, &s);
        let b = render(&scene, &cam, &s);
        assert_eq!(a.pixels, b.pixels, "same seed+scene must be byte-identical");
    }

    #[test]
    fn different_seed_changes_image() {
        let (scene, cam) = box_scene();
        let s1 = Settings { width: 32, height: 20, samples_per_pixel: 4, max_bounces: 3, seed: 1, ..Default::default() };
        let s2 = Settings { seed: 2, ..s1 };
        let a = render(&scene, &cam, &s1);
        let b = render(&scene, &cam, &s2);
        assert_ne!(a.pixels, b.pixels, "different seeds should differ (AA jitter)");
    }

    #[test]
    fn box_render_has_lit_object_and_sky_background() {
        let (scene, cam) = box_scene();
        let s = Settings { width: 80, height: 50, samples_per_pixel: 8, max_bounces: 4, ..Default::default() };
        let img = render(&scene, &cam, &s);
        assert_eq!(img.pixels.len(), 80 * 50 * 4);

        // Center pixel should hit the box (some brightness, not pure sky-blue).
        let cx = 40;
        let cy = 25;
        let ci = (cy * 80 + cx) * 4;
        let center = [img.pixels[ci], img.pixels[ci + 1], img.pixels[ci + 2]];
        // A grey lit box: roughly neutral (R≈G≈B) and non-black.
        assert!(center.iter().any(|&c| c > 20), "center is lit: {center:?}");

        // A top corner pixel is background sky — should be bluish (B > R).
        let corner = 0usize;
        let sky = [img.pixels[corner], img.pixels[corner + 1], img.pixels[corner + 2]];
        assert!(sky[2] >= sky[0], "sky corner is bluish: {sky:?}");

        // Not a blank image: at least some pixels differ from the top-left.
        let distinct = img
            .pixels
            .chunks(4)
            .any(|px| [px[0], px[1], px[2]] != sky);
        assert!(distinct, "render must not be a flat colour");
    }

    #[test]
    fn sun_facing_side_brighter_than_shadowed() {
        // A single up-facing floor triangle under an overhead sun: shading is
        // brightest when the surface faces the light.
        let mut b = SceneBuilder::new();
        let m = b.add_material(Material::diffuse(DVec3::splat(0.8)));
        b.add_triangle(
            DVec3::new(-5.0, -5.0, 0.0),
            DVec3::new(5.0, -5.0, 0.0),
            DVec3::new(0.0, 5.0, 0.0),
            m,
        );
        // Overhead sun vs. grazing sun.
        let overhead = Sun { dir: DVec3::Z, radiance: DVec3::splat(5.0), angular_radius: 0.0 };
        let grazing = Sun { dir: DVec3::new(0.98, 0.0, 0.2).normalize(), radiance: DVec3::splat(5.0), angular_radius: 0.0 };
        let cam = Camera::look_at(DVec3::new(0.0, 0.0, 10.0), DVec3::ZERO, DVec3::Y, 45f64.to_radians(), 1.0);
        let s = Settings { width: 16, height: 16, samples_per_pixel: 8, max_bounces: 1, ..Default::default() };

        let bright = render(&b.clone().build(Some(overhead), Sky::default()), &cam, &s);
        let dim = render(&b.clone().build(Some(grazing), Sky::default()), &cam, &s);
        let sum = |img: &Image| img.pixels.iter().map(|&x| x as u64).sum::<u64>();
        assert!(sum(&bright) > sum(&dim), "overhead sun brighter than grazing: {} vs {}", sum(&bright), sum(&dim));
    }

    #[test]
    fn progressive_reports_each_pass_and_matches_render() {
        let (scene, cam) = box_scene();
        let s = Settings { width: 24, height: 16, samples_per_pixel: 5, max_bounces: 2, ..Default::default() };
        let mut passes = Vec::new();
        let cancel = AtomicBool::new(false);
        let final_img = render_progressive(
            &scene,
            &cam,
            &s,
            |pr| passes.push((pr.passes_done, pr.passes_total)),
            &cancel,
        );
        assert_eq!(passes, vec![(1, 5), (2, 5), (3, 5), (4, 5), (5, 5)]);
        // Progressive to completion == one-shot render (both accumulate all
        // samples deterministically).
        let one_shot = render(&scene, &cam, &s);
        assert_eq!(final_img.pixels, one_shot.pixels);
    }

    #[test]
    fn cancel_stops_early_with_partial_image() {
        let (scene, cam) = box_scene();
        let s = Settings { width: 24, height: 16, samples_per_pixel: 100, max_bounces: 2, ..Default::default() };
        let cancel = AtomicBool::new(false);
        let mut count = 0;
        let img = render_progressive(
            &scene,
            &cam,
            &s,
            |_| {
                count += 1;
                if count == 2 {
                    cancel.store(true, Ordering::Relaxed);
                }
            },
            &cancel,
        );
        assert!(count <= 3, "cancel should stop the loop quickly: {count} passes");
        assert_eq!(img.pixels.len(), 24 * 16 * 4);
    }
}
