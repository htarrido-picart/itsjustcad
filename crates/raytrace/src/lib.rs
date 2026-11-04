// SPDX-License-Identifier: AGPL-3.0-only
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
//! Hardware ray tracing (#318): where `itsjustcad_hwrt::Tracer` is available
//! (Apple9 GPU, not `ITSJUSTCAD_HWRT=cpu`) each pass runs as a wavefront
//! (`render_pass_batched`) with f32 closest-hit and shadow rays on the GPU.
//! The image is not logged state, so GPU answers are used directly (#261
//! item 3); it stays deterministic run to run on one machine.
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

/// One path's running state between bounces.
#[derive(Clone, Copy)]
struct PathState {
    origin: DVec3,
    dir: DVec3,
    throughput: DVec3,
    result: DVec3,
}

/// A next-event shadow ray toward the sun and the light it adds if clear.
#[derive(Clone, Copy)]
struct Shadow {
    ray: [DVec3; 2],
    light: DVec3,
}

/// Surface offset to avoid self-intersection ("shadow acne"), scaled to the
/// scene size so it works at building and millimetre scales alike.
fn surface_eps(scene: &Scene) -> f64 {
    scene
        .bounds
        .map(|b| (b.size().length() * 1e-6).max(1e-6))
        .unwrap_or(1e-6)
}

/// Shade one bounce given the ray's `hit`: add sky or emission, sample the
/// sun (the caller traces the returned shadow ray and adds its light when
/// clear), and scatter. Returns the shadow ray and whether the path goes on.
/// Shared by the per-pixel CPU integrator and the batched one, so both
/// draw the same random numbers in the same order.
fn step(
    scene: &Scene,
    s: &mut PathState,
    hit: Option<scene::Hit>,
    bounce: u32,
    eps: f64,
    rng: &mut Rng,
) -> (Option<Shadow>, bool) {
    let Some(hit) = hit else {
        // Escaped to the sky — add its radiance and stop.
        s.result += s.throughput * scene.sky.radiance(s.dir);
        return (None, false);
    };

    // Emissive surfaces radiate directly (area lights).
    if hit.material.is_emissive() {
        s.result += s.throughput * hit.material.emission;
    }

    let p = hit.point + hit.normal * eps;

    // Next-event estimation: sample the sun for direct light on diffuse
    // surfaces (specular bounces sample it implicitly via the BSDF).
    let mut shadow = None;
    if let (scene::Bsdf::Diffuse, Some(sun)) = (bsdf_kind(&hit.material), scene.sun) {
        let ldir = sun_sample_dir(sun, rng);
        let ndl = hit.normal.dot(ldir);
        if ndl > 0.0 {
            // Lambertian BRDF = albedo / π; the solid-angle weight of the
            // sun disc is folded into its radiance (treated as a bright
            // directional source), so direct = albedo * radiance * cos.
            let light = s.throughput * hit.material.albedo * sun.radiance * (ndl / std::f64::consts::PI);
            shadow = Some(Shadow { ray: [p, ldir], light });
        }
    }

    // Sample the BSDF for the next bounce.
    let Some(scat) = hit.material.scatter(s.dir, hit.normal, rng) else {
        return (shadow, false);
    };
    s.throughput *= scat.attenuation;
    s.origin = p;
    s.dir = scat.dir;

    // Russian roulette after a few bounces: kill low-throughput paths
    // unbiasedly.
    if bounce >= 3 {
        let q = s.throughput.max_element().clamp(0.05, 0.95);
        if rng.f01() > q {
            return (shadow, false);
        }
        s.throughput /= q;
    }
    (shadow, true)
}

/// Trace one primary ray through the scene and return its linear radiance.
fn radiance(scene: &Scene, origin: DVec3, dir: DVec3, max_bounces: u32, rng: &mut Rng) -> DVec3 {
    let mut s = PathState { origin, dir, throughput: DVec3::ONE, result: DVec3::ZERO };
    let eps = surface_eps(scene);
    for bounce in 0..=max_bounces {
        let hit = scene.intersect(s.origin, s.dir);
        let (shadow, more) = step(scene, &mut s, hit, bounce, eps, rng);
        if let Some(sh) = shadow
            && !scene.occluded(sh.ray[0], sh.ray[1], f64::INFINITY)
        {
            s.result += sh.light;
        }
        if !more {
            break;
        }
    }
    s.result
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

/// The primary ray and RNG stream for pixel `idx` of pass `sample`.
fn primary(cam: &Camera, s: &Settings, sample: u32, idx: usize) -> (DVec3, Rng) {
    let w = s.width as usize;
    let h = s.height as usize;
    let px = idx % w;
    let py = idx / w;
    let mut rng = Rng::new(idx as u64, sample as u64, s.seed);
    // Jitter within the pixel for anti-aliasing.
    let jx = rng.f01();
    let jy = rng.f01();
    let u = (px as f64 + jx) / w as f64;
    // Image row 0 is the top; camera t=0 is the bottom → flip.
    let v = 1.0 - (py as f64 + jy) / h as f64;
    (cam.ray(u, v), rng)
}

/// Render a single accumulation pass into `accum` (HDR sums), deterministically.
/// Each pixel seeds its own RNG stream from (pixel, sample, global seed), so the
/// ordered `rayon` map is byte-identical to a sequential run.
fn render_pass(scene: &Scene, cam: &Camera, s: &Settings, sample: u32, accum: &mut [DVec3]) {
    accum
        .par_iter_mut()
        .enumerate()
        .for_each(|(idx, cell)| {
            let (dir, mut rng) = primary(cam, s, sample, idx);
            *cell += radiance(scene, cam.origin(), dir, s.max_bounces, &mut rng);
        });
}

/// One wavefront path: its state, RNG stream and shadow ray in flight.
struct Path {
    state: PathState,
    rng: Rng,
    shadow: Option<Shadow>,
}

/// Pixels per wavefront lane. A pass splits into at least two lanes so the
/// CPU shades one while the GPU traces the other; this caps a lane's GPU
/// buffers at 12 MB of rays.
const LANE: usize = 1 << 19;

/// Pixels per parallel task (a [`Piece`] at the first bounce).
const TASK: usize = 4096;

/// The rays one task hands to the next bounce: path indices (ascending)
/// and their rays. Pieces of a lane cover disjoint, ascending path ranges,
/// so each can be shaded by its own task with no shared mutable state.
#[derive(Default)]
struct Piece {
    /// Paths whose next ray is in flight, and those rays.
    live: Vec<u32>,
    rays: Vec<[DVec3; 2]>,
    /// Paths whose shadow ray is in flight, and those rays.
    lit: Vec<u32>,
    shadows: Vec<[DVec3; 2]>,
}

impl Piece {
    fn clear(&mut self) {
        self.live.clear();
        self.rays.clear();
        self.lit.clear();
        self.shadows.clear();
    }

    /// The path range this piece touches.
    fn span(&self) -> Option<std::ops::Range<usize>> {
        let lo = self.live.first().into_iter().chain(self.lit.first()).min()?;
        let hi = self.live.last().into_iter().chain(self.lit.last()).max()?;
        Some(*lo as usize..*hi as usize + 1)
    }
}

/// One lane's paths (one per pixel, in pixel order) and its reusable
/// buffers, so a pass allocates (and page-faults) almost nothing after the
/// first.
#[derive(Default)]
struct Lane {
    paths: Vec<Path>,
    /// The pieces in flight, and spares for the next bounce.
    cur: Vec<Piece>,
    spare: Vec<Piece>,
    hits: Vec<Option<(u32, f32)>>,
    occ: Vec<bool>,
}

/// Buffers reused by every [`render_pass_batched`] of one render.
#[derive(Default)]
struct Wavefront {
    lanes: Vec<Lane>,
}

/// [`render_pass`] as a wavefront: every live path's next ray is traced in
/// one batch per lane, with the previous bounce's shadow rays riding along,
/// so the GPU gets whole-lane batches (one round trip per bounce) instead
/// of per-ray round trips, and lanes alternate so the CPU shades one while
/// the GPU traces another. Per path it runs the same [`step`]s with the
/// same RNG stream, adding each sun sample before the next bounce's light,
/// so with `hw = None` it is byte-identical to [`render_pass`].
fn render_pass_batched(
    scene: &Scene,
    hw: Option<&itsjustcad_hwrt::Tracer>,
    cam: &Camera,
    s: &Settings,
    sample: u32,
    accum: &mut [DVec3],
    wf: &mut Wavefront,
) {
    let eps = surface_eps(scene);
    let origin = cam.origin();
    let n = accum.len();
    let lanes = n.div_ceil(LANE).max(2).min(n.max(1));
    let per = n.div_ceil(lanes);
    wf.lanes.resize_with(lanes, Lane::default);
    // Start every lane: primary rays, no shadows yet.
    let mut pending: Vec<Option<scene::Pending>> = Vec::with_capacity(lanes);
    for (l, lane) in wf.lanes.iter_mut().enumerate() {
        let start = (l * per).min(n);
        let len = ((l + 1) * per).min(n) - start;
        (start..start + len)
            .into_par_iter()
            .map(|idx| {
                let (dir, rng) = primary(cam, s, sample, idx);
                let state = PathState { origin, dir, throughput: DVec3::ONE, result: DVec3::ZERO };
                Path { state, rng, shadow: None }
            })
            .collect_into_vec(&mut lane.paths);
        lane.cur.resize_with(len.div_ceil(TASK), Piece::default);
        let paths = &lane.paths;
        lane.cur.par_iter_mut().enumerate().for_each(|(c, pc)| {
            pc.clear();
            let r = c * TASK..((c + 1) * TASK).min(len);
            pc.live.extend(r.clone().map(|i| i as u32));
            pc.rays.extend(paths[r].iter().map(|p| [p.state.origin, p.state.dir]));
        });
        pending.push(Some(submit(scene, hw, &lane.cur)));
    }
    let mut bounce = vec![0u32; lanes];
    while pending.iter().any(Option::is_some) {
        for (l, lane) in wf.lanes.iter_mut().enumerate() {
            let Some(p) = pending[l].take() else { continue };
            let Lane { paths, cur, spare, hits, occ } = lane;
            hits.resize(cur.iter().map(|pc| pc.rays.len()).sum(), None);
            occ.resize(cur.iter().map(|pc| pc.shadows.len()).sum(), false);
            let rays: Vec<&[[DVec3; 2]]> = cur.iter().map(|pc| &pc.rays[..]).collect();
            let shadows: Vec<&[[DVec3; 2]]> = cur.iter().map(|pc| &pc.shadows[..]).collect();
            scene.finish(hw, p, &rays, &shadows, hits, occ);
            let b = bounce[l];
            bounce[l] += 1;
            let last = b >= s.max_bounces;
            // Pair each piece with its disjoint slice of paths and answers.
            let mut tasks = Vec::with_capacity(cur.len());
            let (mut rest, mut base) = (&mut paths[..], 0usize);
            let (mut hs, mut os) = (&hits[..], &occ[..]);
            for pc in cur.iter() {
                let (h, ht) = hs.split_at(pc.rays.len());
                let (o, ot) = os.split_at(pc.shadows.len());
                (hs, os) = (ht, ot);
                let Some(span) = pc.span() else { continue };
                let (_, tail) = std::mem::take(&mut rest).split_at_mut(span.start - base);
                let (mine, tail) = tail.split_at_mut(span.len());
                tasks.push((mine, span.start, pc, h, o));
                rest = tail;
                base = span.end;
            }
            spare.resize_with(tasks.len(), Piece::default);
            tasks.into_par_iter().zip(spare.par_iter_mut()).for_each(|((ps, base, pc, h, o), next)| {
                next.clear();
                // The previous bounce's sun light, before this bounce's.
                for (&i, &occluded) in pc.lit.iter().zip(o) {
                    let p = &mut ps[i as usize - base];
                    if let Some(sh) = p.shadow.take()
                        && !occluded
                    {
                        p.state.result += sh.light;
                    }
                }
                for (&i, h) in pc.live.iter().zip(h) {
                    let p = &mut ps[i as usize - base];
                    let hit = h.map(|(tri, t)| scene.hit_at(p.state.origin, p.state.dir, tri, t));
                    let (shadow, more) = step(scene, &mut p.state, hit, b, eps, &mut p.rng);
                    if let Some(sh) = shadow {
                        p.shadow = shadow;
                        next.lit.push(i);
                        next.shadows.push(sh.ray);
                    }
                    if more && !last {
                        next.live.push(i);
                        next.rays.push([p.state.origin, p.state.dir]);
                    }
                }
            });
            std::mem::swap(cur, spare);
            if cur.iter().any(|pc| !pc.live.is_empty() || !pc.lit.is_empty()) {
                pending[l] = Some(submit(scene, hw, cur));
            }
        }
    }
    let mut start = 0;
    for lane in &wf.lanes[..lanes] {
        let len = lane.paths.len();
        accum[start..start + len]
            .par_iter_mut()
            .zip(lane.paths.par_iter())
            .for_each(|(c, p)| *c += p.state.result);
        start += len;
    }
}

/// [`Scene::submit`] for every piece's rays and shadow rays.
fn submit(scene: &Scene, hw: Option<&itsjustcad_hwrt::Tracer>, pieces: &[Piece]) -> scene::Pending {
    let rays: Vec<&[[DVec3; 2]]> = pieces.iter().map(|pc| &pc.rays[..]).collect();
    let shadows: Vec<&[[DVec3; 2]]> = pieces.iter().map(|pc| &pc.shadows[..]).collect();
    scene.submit(hw, &rays, &shadows)
}

/// Tone-map an HDR accumulation buffer (summed over `samples` passes) to an RGBA
/// image.
fn resolve(accum: &[DVec3], samples: u32, s: &Settings) -> Image {
    // Each pixel is a pure function of its own accumulator, with no
    // cross-pixel reduction, so the parallel split is byte-identical to the
    // serial loop (#321).
    let inv = 1.0 / samples.max(1) as f64;
    let mut pixels = vec![0u8; accum.len() * 4];
    pixels.par_chunks_mut(4).zip(accum.par_iter()).with_min_len(4096).for_each(|(p, c)| {
        let [r, g, b] = tonemap::to_srgb8(*c * inv, s.tonemap);
        p.copy_from_slice(&[r, g, b, 255]);
    });
    Image { width: s.width, height: s.height, pixels }
}

/// Render the scene to a finished image (all samples, no intermediate callback).
///
/// Only the final image is tone-mapped: no one sees the intermediate ones
/// (#321).
pub fn render(scene: &Scene, cam: &Camera, settings: &Settings) -> Image {
    let dummy = AtomicBool::new(false);
    accumulate(scene, cam, settings, None, &dummy)
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
    accumulate(scene, cam, settings, Some(&mut on_pass), cancel)
}

/// The pass loop behind [`render`] and [`render_progressive`]. Intermediate
/// images are resolved only when `on_pass` is given.
fn accumulate(
    scene: &Scene,
    cam: &Camera,
    settings: &Settings,
    mut on_pass: Option<&mut dyn FnMut(PassResult<'_>)>,
    cancel: &AtomicBool,
) -> Image {
    let n = (settings.width as usize) * (settings.height as usize);
    let mut accum = vec![DVec3::ZERO; n];
    let total = settings.samples_per_pixel.max(1);
    let mut done = 0u32;
    // Hardware ray tracing when available (#318); the CPU path otherwise.
    let hw = scene.hw();
    let mut wf = Wavefront::default();
    for sample in 0..total {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        match hw {
            Some(_) => render_pass_batched(scene, hw, cam, settings, sample, &mut accum, &mut wf),
            None => render_pass(scene, cam, settings, sample, &mut accum),
        }
        done = sample + 1;
        if let Some(on_pass) = on_pass.as_mut() {
            let image = resolve(&accum, done, settings);
            on_pass(PassResult { passes_done: done, passes_total: total, image: &image });
        }
    }
    if let Some(hw) = hw {
        hw.report();
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

    /// The wavefront pass on the CPU backend is the per-pixel pass, bit for
    /// bit: same steps, same RNG streams, same accumulation order.
    #[test]
    fn batched_cpu_pass_is_byte_identical() {
        let (scene, cam) = box_scene();
        let s = Settings { width: 128, height: 80, samples_per_pixel: 1, max_bounces: 6, ..Default::default() };
        let n = (s.width * s.height) as usize;
        for sample in 0..3 {
            let mut a = vec![DVec3::ZERO; n];
            let mut b = vec![DVec3::ZERO; n];
            render_pass(&scene, &cam, &s, sample, &mut a);
            render_pass_batched(&scene, None, &cam, &s, sample, &mut b, &mut Wavefront::default());
            assert!(a.iter().zip(&b).all(|(x, y)| x.to_array().map(f64::to_bits) == y.to_array().map(f64::to_bits)));
        }
    }

    /// On hardware ray tracing the image matches the CPU's closely.
    #[test]
    fn hw_render_matches_cpu() {
        let (scene, cam) = box_scene();
        let Some(hw) = scene.hw() else { return };
        let s = Settings { width: 80, height: 50, samples_per_pixel: 4, max_bounces: 4, ..Default::default() };
        let n = (s.width * s.height) as usize;
        let (mut a, mut b) = (vec![DVec3::ZERO; n], vec![DVec3::ZERO; n]);
        let mut wf = Wavefront::default();
        for sample in 0..s.samples_per_pixel {
            render_pass(&scene, &cam, &s, sample, &mut a);
            render_pass_batched(&scene, Some(hw), &cam, &s, sample, &mut b, &mut wf);
        }
        let (a, b) = (resolve(&a, 4, &s), resolve(&b, 4, &s));
        let diff = a.pixels.chunks(4).zip(b.pixels.chunks(4)).filter(|(x, y)| x != y).count();
        assert!(diff <= n / 100, "{diff} of {n} pixels differ");
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

    /// The parallel resolve is the serial per-pixel tone map, byte for byte,
    /// on both operators and across zero, HDR and odd sample counts.
    #[test]
    fn parallel_resolve_matches_serial() {
        let s = Settings { width: 37, height: 23, ..Default::default() };
        let n = (s.width * s.height) as usize;
        let accum: Vec<DVec3> = (0..n)
            .map(|i| {
                let t = i as f64;
                DVec3::new((t * 0.37).sin().abs() * 9.0, (t * 0.011).powi(2), if i % 7 == 0 { 0.0 } else { t * 1e-3 })
            })
            .collect();
        for tonemap in [ToneMap::Reinhard, ToneMap::Aces] {
            let s = Settings { tonemap, ..s };
            for samples in [0, 1, 3, 48] {
                let inv = 1.0 / samples.max(1) as f64;
                let mut want = Vec::with_capacity(n * 4);
                for c in &accum {
                    want.extend(tonemap::to_srgb8(*c * inv, tonemap));
                    want.push(255);
                }
                let got = resolve(&accum, samples, &s);
                assert_eq!((got.width, got.height), (s.width, s.height));
                assert_eq!(got.pixels, want);
            }
        }
    }

    /// Cancelling `render_progressive` before the first pass still returns a
    /// full-size black frame.
    #[test]
    fn cancel_before_first_pass_returns_black_frame() {
        let (scene, cam) = box_scene();
        let s = Settings { width: 12, height: 8, samples_per_pixel: 4, max_bounces: 2, ..Default::default() };
        let cancel = AtomicBool::new(true);
        let mut called = false;
        let img = render_progressive(&scene, &cam, &s, |_| called = true, &cancel);
        assert!(!called);
        assert!(img.pixels.chunks(4).all(|p| p == [0, 0, 0, 255]));
    }
}
