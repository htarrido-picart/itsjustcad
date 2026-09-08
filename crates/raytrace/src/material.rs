// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! PBR materials and their BSDFs. The document's per-object [`ObjectMaterial`]
//! (colour + roughness + metallic, or a named preset) maps here via
//! [`Material::from_pbr`]; untagged objects fall back to a matte default.
//!
//! Four appearance classes are supported, chosen from the (roughness, metallic)
//! scalars the doc already carries plus an explicit glass/emissive flag:
//!   * **Diffuse** (Lambertian) — the default for matte dielectrics.
//!   * **Metal** (glossy specular reflection, roughness-perturbed).
//!   * **Glass** (smooth dielectric with Fresnel reflect/refract).
//!   * **Emissive** — a surface that radiates light (also acts as an area light).

use glam::DVec3;

use crate::rng::Rng;

/// Which BSDF family a surface uses.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Bsdf {
    /// Lambertian diffuse reflector.
    Diffuse,
    /// Specular metal; `roughness` blurs the reflection lobe (0 = mirror).
    Metal { roughness: f64 },
    /// Dielectric (glass); `ior` is the index of refraction.
    Glass { ior: f64 },
}

/// A resolved surface material in linear colour space.
#[derive(Clone, Copy, Debug)]
pub struct Material {
    /// Base/albedo colour (also the reflectance tint for metals, and the
    /// transmission tint for glass), linear 0..1.
    pub albedo: DVec3,
    /// Which lobe governs scattering.
    pub bsdf: Bsdf,
    /// Radiance this surface emits (0 = non-emissive). Emitters double as area
    /// lights hit by path rays.
    pub emission: DVec3,
}

impl Default for Material {
    fn default() -> Self {
        // Untagged geometry: a neutral matte grey (architectural default).
        Self {
            albedo: DVec3::splat(0.72),
            bsdf: Bsdf::Diffuse,
            emission: DVec3::ZERO,
        }
    }
}

impl Material {
    /// Map the doc's PBR triple `(albedo, roughness, metallic)` — exactly what
    /// [`itsjustcad_doc::ObjectMaterial::pbr`] returns — to a ray-tracer
    /// material. Heuristics matching the viewport shader's intent:
    ///   * very low roughness + dielectric → **glass** (the `glass` preset is
    ///     roughness 0.05, metallic 0);
    ///   * metallic ≥ 0.5 → **metal** (roughness feeds the gloss lobe);
    ///   * otherwise **diffuse**.
    pub fn from_pbr(albedo: [f32; 3], roughness: f32, metallic: f32) -> Self {
        let albedo = srgb_to_linear(DVec3::new(
            albedo[0] as f64,
            albedo[1] as f64,
            albedo[2] as f64,
        ));
        let bsdf = if metallic >= 0.5 {
            Bsdf::Metal { roughness: roughness as f64 }
        } else if roughness <= 0.12 {
            // Smooth dielectric → treat as glass. Architectural glass IOR ≈ 1.5.
            Bsdf::Glass { ior: 1.5 }
        } else {
            Bsdf::Diffuse
        };
        Self { albedo, bsdf, emission: DVec3::ZERO }
    }

    /// A pure Lambertian material of the given linear colour.
    pub fn diffuse(albedo: DVec3) -> Self {
        Self { albedo, bsdf: Bsdf::Diffuse, emission: DVec3::ZERO }
    }

    /// An emitter radiating `radiance` (linear, may exceed 1 for bright lights).
    pub fn emissive(radiance: DVec3) -> Self {
        Self { albedo: DVec3::ZERO, bsdf: Bsdf::Diffuse, emission: radiance }
    }

    pub fn is_emissive(&self) -> bool {
        self.emission.max_element() > 0.0
    }
}

/// Outcome of scattering an incoming ray off a surface.
pub struct Scatter {
    /// New ray direction (unit).
    pub dir: DVec3,
    /// Throughput multiplier (BSDF·cos / pdf, already folded).
    pub attenuation: DVec3,
    /// True when the bounce is a perfect/near-perfect specular event (so the
    /// integrator should not also do next-event estimation for it).
    pub specular: bool,
}

/// Reflect `v` about unit normal `n`.
fn reflect(v: DVec3, n: DVec3) -> DVec3 {
    v - 2.0 * v.dot(n) * n
}

/// Refract `v` (unit, incoming) through a surface of relative index `eta`
/// (n_from / n_to). Returns `None` on total internal reflection.
fn refract(v: DVec3, n: DVec3, eta: f64) -> Option<DVec3> {
    let cos_i = (-v).dot(n).clamp(-1.0, 1.0);
    let sin2_t = eta * eta * (1.0 - cos_i * cos_i);
    if sin2_t > 1.0 {
        return None; // total internal reflection
    }
    let cos_t = (1.0 - sin2_t).sqrt();
    Some(eta * v + (eta * cos_i - cos_t) * n)
}

/// Schlick's Fresnel reflectance approximation for a dielectric.
fn fresnel_schlick(cos_i: f64, ior: f64) -> f64 {
    let r0 = ((1.0 - ior) / (1.0 + ior)).powi(2);
    r0 + (1.0 - r0) * (1.0 - cos_i).clamp(0.0, 1.0).powi(5)
}

/// Cosine-weighted hemisphere sample around unit normal `n`.
pub fn cosine_hemisphere(n: DVec3, rng: &mut Rng) -> DVec3 {
    let u1 = rng.f01();
    let u2 = rng.f01();
    let r = u1.sqrt();
    let theta = std::f64::consts::TAU * u2;
    let x = r * theta.cos();
    let y = r * theta.sin();
    let z = (1.0 - u1).max(0.0).sqrt();
    let (t, b) = onb(n);
    (t * x + b * y + n * z).normalize()
}

/// Build an orthonormal basis `(tangent, bitangent)` for unit normal `n`.
fn onb(n: DVec3) -> (DVec3, DVec3) {
    let a = if n.x.abs() > 0.9 { DVec3::Y } else { DVec3::X };
    let t = n.cross(a).normalize();
    let b = n.cross(t);
    (t, b)
}

impl Material {
    /// Sample a scattered ray. `wo` is the incoming ray direction (unit), `n`
    /// the shading normal oriented against `wo` (front-facing).
    pub fn scatter(&self, wo: DVec3, n: DVec3, rng: &mut Rng) -> Option<Scatter> {
        match self.bsdf {
            Bsdf::Diffuse => {
                let dir = cosine_hemisphere(n, rng);
                // For cosine-weighted sampling the cos/pdf terms cancel to 1, so
                // throughput is just the albedo.
                Some(Scatter { dir, attenuation: self.albedo, specular: false })
            }
            Bsdf::Metal { roughness } => {
                let mut dir = reflect(wo, n);
                if roughness > 0.0 {
                    // Perturb the mirror direction within a fuzz sphere.
                    dir = (dir + roughness * random_in_unit_sphere(rng)).normalize();
                }
                if dir.dot(n) <= 0.0 {
                    return None; // scattered below the surface — absorb
                }
                Some(Scatter {
                    dir,
                    attenuation: self.albedo,
                    specular: roughness < 0.08,
                })
            }
            Bsdf::Glass { ior } => {
                // `wo` points along the incoming ray; `n` is front-facing.
                let entering = wo.dot(n) < 0.0;
                let (nl, eta) = if entering {
                    (n, 1.0 / ior)
                } else {
                    (-n, ior)
                };
                let cos_i = (-wo).dot(nl).clamp(0.0, 1.0);
                let reflect_prob = match refract(wo, nl, eta) {
                    None => 1.0, // TIR
                    Some(_) => fresnel_schlick(cos_i, ior),
                };
                let dir = if rng.f01() < reflect_prob {
                    reflect(wo, nl)
                } else {
                    refract(wo, nl, eta).unwrap()
                };
                // Glass tint applies on transmission; clear glass ≈ white.
                let att = if self.albedo.max_element() > 0.0 {
                    self.albedo
                } else {
                    DVec3::ONE
                };
                Some(Scatter { dir: dir.normalize(), attenuation: att, specular: true })
            }
        }
    }
}

fn random_in_unit_sphere(rng: &mut Rng) -> DVec3 {
    loop {
        let p = DVec3::new(
            rng.f01() * 2.0 - 1.0,
            rng.f01() * 2.0 - 1.0,
            rng.f01() * 2.0 - 1.0,
        );
        if p.length_squared() < 1.0 {
            return p;
        }
    }
}

/// Convert an sRGB colour (as authored in the doc / palette) to linear light.
pub fn srgb_to_linear(c: DVec3) -> DVec3 {
    DVec3::new(srgb_ch(c.x), srgb_ch(c.y), srgb_ch(c.z))
}

fn srgb_ch(u: f64) -> f64 {
    if u <= 0.04045 {
        u / 12.92
    } else {
        ((u + 0.055) / 1.055).powf(2.4)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pbr_glass_preset_maps_to_glass() {
        // The doc `glass` preset: low roughness dielectric.
        let m = Material::from_pbr([0.55, 0.72, 0.80], 0.05, 0.0);
        assert!(matches!(m.bsdf, Bsdf::Glass { .. }));
    }

    #[test]
    fn pbr_metal_preset_maps_to_metal() {
        let m = Material::from_pbr([0.8, 0.81, 0.83], 0.25, 1.0);
        assert!(matches!(m.bsdf, Bsdf::Metal { .. }));
    }

    #[test]
    fn pbr_concrete_maps_to_diffuse() {
        let m = Material::from_pbr([0.62, 0.62, 0.60], 0.90, 0.0);
        assert!(matches!(m.bsdf, Bsdf::Diffuse));
    }

    #[test]
    fn diffuse_scatters_into_hemisphere() {
        let mat = Material::diffuse(DVec3::splat(0.8));
        let mut rng = Rng::new(1, 2, 3);
        let n = DVec3::Z;
        for _ in 0..1000 {
            let s = mat.scatter(DVec3::new(0.0, 0.0, -1.0), n, &mut rng).unwrap();
            assert!(s.dir.dot(n) > -1e-9, "scatter stays in the hemisphere");
            assert!((s.dir.length() - 1.0).abs() < 1e-9);
        }
    }

    #[test]
    fn metal_reflects_mirror_when_smooth() {
        let mat = Material { albedo: DVec3::ONE, bsdf: Bsdf::Metal { roughness: 0.0 }, emission: DVec3::ZERO };
        let mut rng = Rng::new(1, 1, 1);
        // Ray going -Z at a +Z-facing surface reflects to +Z.
        let s = mat.scatter(DVec3::new(0.0, 0.0, -1.0), DVec3::Z, &mut rng).unwrap();
        assert!((s.dir - DVec3::Z).length() < 1e-9, "smooth metal is a mirror: {}", s.dir);
        assert!(s.specular);
    }

    #[test]
    fn glass_bends_ray_on_entry() {
        let mat = Material { albedo: DVec3::ZERO, bsdf: Bsdf::Glass { ior: 1.5 }, emission: DVec3::ZERO };
        let mut rng = Rng::new(7, 7, 7);
        // Oblique incoming ray so refraction (when chosen) bends it.
        let wo = DVec3::new(0.6, 0.0, -0.8).normalize();
        let mut refracted = 0;
        let mut reflected = 0;
        for _ in 0..2000 {
            let s = mat.scatter(wo, DVec3::Z, &mut rng).unwrap();
            if s.dir.z < 0.0 {
                refracted += 1; // continued into the surface
            } else {
                reflected += 1; // bounced back off
            }
            assert!(s.specular);
        }
        // Most energy transmits at this angle; both channels must occur.
        assert!(refracted > 0 && reflected > 0, "refr={refracted} refl={reflected}");
        assert!(refracted > reflected, "glass mostly transmits at near-normal incidence");
    }

    #[test]
    fn srgb_linear_roundtrip_endpoints() {
        assert!(srgb_to_linear(DVec3::ZERO).length() < 1e-12);
        assert!((srgb_to_linear(DVec3::ONE) - DVec3::ONE).length() < 1e-9);
        // Mid-grey linearizes darker (sRGB 0.5 → ~0.214).
        let mid = srgb_to_linear(DVec3::splat(0.5)).x;
        assert!((mid - 0.214).abs() < 0.01, "mid={mid}");
    }
}
