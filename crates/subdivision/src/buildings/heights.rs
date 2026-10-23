// SPDX-License-Identifier: AGPL-3.0-only
// Copyright © 2026 Hector Tarrido-Picart

//! Per-lot floor counts — CityEngine-style height variation.
//!
//! With every variation option off (the default) each lot gets
//! `floor_count`, exactly as before. Otherwise each lot draws:
//!
//! 1. a floor range: its `height_mix` class (picked by weight), else
//!    `floors_min..=floors_max`, else `1..=floor_count` (falloff alone);
//! 2. a place `p ∈ [0, 1)` in that range: a deterministic hash of the height
//!    seed and the lot's centroid (to the decimetre, so it is stable across
//!    regeneration), blended with the falloff closeness by
//!    `falloff_strength`.

use glam::DVec2;

use crate::blocks::Block;
use crate::settings::{HeightFalloff, SubdivisionSettings};

/// splitmix64 finaliser.
fn mix64(mut z: u64) -> u64 {
    z = z.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// A uniform `[0, 1)` draw for `(seed, lot centroid, stream)`.
fn draw(seed: u64, c: DVec2, stream: u64) -> f64 {
    let q = |v: f64| (v * 10.0).round() as i64 as u64;
    let h =
        mix64(mix64(mix64(seed ^ stream.wrapping_mul(0xA24B_AED4_963E_E407)) ^ q(c.x)) ^ q(c.y));
    (h >> 11) as f64 / (1u64 << 53) as f64
}

fn seg_distance(p: DVec2, a: DVec2, b: DVec2) -> f64 {
    let ab = b - a;
    let t = if ab.length_squared() > 0.0 {
        ((p - a).dot(ab) / ab.length_squared()).clamp(0.0, 1.0)
    } else {
        0.0
    };
    p.distance(a + ab * t)
}

/// The seed the height draw uses: `height_seed`, or `seed` when it is 0.
pub fn height_seed(s: &SubdivisionSettings) -> u64 {
    if s.height_seed != 0 {
        s.height_seed
    } else {
        s.seed
    }
}

/// True when any height-variation option is on.
pub fn varies_heights(s: &SubdivisionSettings) -> bool {
    s.height_mix.is_some() || s.floors_max > 0 || s.height_falloff != HeightFalloff::None
}

/// Per-lot closeness to the falloff focus in `[0, 1]` (1 = nearest), min-max
/// normalised over `lots`. `None` when falloff is off or has no focus (street
/// falloff on lots without street tags).
fn closeness(lots: &[&Block], centroids: &[DVec2], s: &SubdivisionSettings) -> Option<Vec<f64>> {
    let dist: Vec<f64> = match s.height_falloff {
        HeightFalloff::None => return None,
        HeightFalloff::Center | HeightFalloff::Point => {
            let focus = if s.height_falloff == HeightFalloff::Point {
                DVec2::from_array(s.falloff_point)
            } else {
                let (sum, area) = lots
                    .iter()
                    .zip(centroids)
                    .fold((DVec2::ZERO, 0.0), |(sum, area), (l, c)| {
                        (sum + *c * l.area(), area + l.area())
                    });
                if area > 0.0 { sum / area } else { DVec2::ZERO }
            };
            centroids.iter().map(|c| c.distance(focus)).collect()
        }
        HeightFalloff::Street => {
            let segs: Vec<(DVec2, DVec2)> = lots
                .iter()
                .flat_map(|l| l.edges.iter().filter(|e| e.is_street).map(|e| (e.a, e.b)))
                .collect();
            if segs.is_empty() {
                return None;
            }
            centroids
                .iter()
                .map(|c| {
                    segs.iter()
                        .map(|(a, b)| seg_distance(*c, *a, *b))
                        .fold(f64::INFINITY, f64::min)
                })
                .collect()
        }
    };
    let lo = dist.iter().copied().fold(f64::INFINITY, f64::min);
    let hi = dist.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    let span = hi - lo;
    Some(
        dist.iter()
            .map(|d| {
                if span > 1e-9 {
                    1.0 - (d - lo) / span
                } else {
                    1.0
                }
            })
            .collect(),
    )
}

/// The floor count of each lot (parallel to `lots`). Deterministic; with no
/// variation option on, every lot gets `floor_count`.
pub fn lot_floor_counts(lots: &[&Block], s: &SubdivisionSettings) -> Vec<usize> {
    if !varies_heights(s) {
        return vec![s.floor_count; lots.len()];
    }
    let seed = height_seed(s);
    let centroids: Vec<DVec2> = lots.iter().map(|l| l.polygon.centroid()).collect();
    let near = closeness(lots, &centroids, s);
    let strength = s.falloff_strength.clamp(0.0, 1.0);
    let mix = s
        .height_mix
        .as_ref()
        .filter(|m| m.classes.iter().map(|c| c.weight.max(0.0)).sum::<f64>() > 0.0);
    centroids
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let (lo, hi) = if let Some(m) = mix {
                let total: f64 = m.classes.iter().map(|c| c.weight.max(0.0)).sum();
                let mut pick = draw(seed, *c, 1) * total;
                let class = m
                    .classes
                    .iter()
                    .filter(|c| c.weight > 0.0)
                    .find(|c| {
                        pick -= c.weight;
                        pick < 0.0
                    })
                    .or_else(|| m.classes.iter().rfind(|c| c.weight > 0.0))
                    .expect("a positive weight");
                (class.floors_min, class.floors_max)
            } else if s.floors_max > 0 {
                (s.floors_min.max(1).min(s.floors_max), s.floors_max)
            } else {
                (1, s.floor_count.max(1))
            };
            let u = draw(seed, *c, 2);
            let p = match &near {
                Some(near) => (1.0 - strength) * u + strength * near[i],
                None => u,
            };
            let n = hi - lo + 1;
            lo + ((p * n as f64) as usize).min(n - 1)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::polygon2d::Polygon2d;
    use crate::settings::HeightMix;

    /// A row of `n` 10 × 20 m lots along x.
    fn lots(n: usize) -> Vec<Block> {
        (0..n)
            .map(|i| {
                let x = i as f64 * 10.0;
                Block::untagged(
                    Polygon2d::from_pairs([(x, 0.0), (x + 10.0, 0.0), (x + 10.0, 20.0), (x, 20.0)])
                        .unwrap(),
                )
            })
            .collect()
    }

    fn counts(b: &[Block], s: &SubdivisionSettings) -> Vec<usize> {
        lot_floor_counts(&b.iter().collect::<Vec<_>>(), s)
    }

    #[test]
    fn defaults_give_floor_count() {
        let s = SubdivisionSettings {
            floor_count: 4,
            ..Default::default()
        };
        assert_eq!(counts(&lots(5), &s), vec![4; 5]);
    }

    #[test]
    fn random_range_is_deterministic_and_reseedable() {
        let b = lots(60);
        let s = SubdivisionSettings {
            floors_min: 2,
            floors_max: 8,
            ..Default::default()
        };
        let a = counts(&b, &s);
        assert_eq!(a, counts(&b, &s));
        assert!(a.iter().all(|n| (2..=8).contains(n)));
        assert!(a.iter().min() != a.iter().max(), "heights vary");
        let r = counts(
            &b,
            &SubdivisionSettings {
                height_seed: 99,
                ..s.clone()
            },
        );
        assert_ne!(a, r, "a new height seed reshuffles");
        // Seed 0 follows the plan seed.
        let p = counts(&b, &SubdivisionSettings { seed: 99, ..s });
        assert_eq!(p, r);
    }

    #[test]
    fn mix_proportions_roughly_honoured() {
        let b: Vec<Block> = (0..40)
            .flat_map(|row| {
                lots(50).into_iter().map(move |l| {
                    let v: Vec<(f64, f64)> = l
                        .polygon
                        .verts()
                        .iter()
                        .map(|p| (p.x, p.y + row as f64 * 20.0))
                        .collect();
                    Block::untagged(Polygon2d::from_pairs(v).unwrap())
                })
            })
            .collect();
        let s = SubdivisionSettings {
            height_mix: Some(HeightMix::parse("low:50,mid:35,tower:15").unwrap()),
            ..Default::default()
        };
        let c = counts(&b, &s);
        let n = c.len() as f64;
        let frac = |r: std::ops::RangeInclusive<usize>| {
            c.iter().filter(|f| r.contains(f)).count() as f64 / n
        };
        assert!((frac(1..=3) - 0.50).abs() < 0.05, "{}", frac(1..=3));
        assert!((frac(4..=8) - 0.35).abs() < 0.05, "{}", frac(4..=8));
        assert!((frac(9..=20) - 0.15).abs() < 0.05, "{}", frac(9..=20));
    }

    #[test]
    fn point_falloff_is_monotone() {
        let b = lots(30);
        let s = SubdivisionSettings {
            floors_min: 2,
            floors_max: 12,
            height_falloff: HeightFalloff::Point,
            falloff_point: [0.0, 10.0],
            falloff_strength: 1.0,
            ..Default::default()
        };
        let c = counts(&b, &s);
        assert!(c.windows(2).all(|w| w[0] >= w[1]), "{c:?}");
        assert_eq!((c[0], c[29]), (12, 2));
    }

    #[test]
    fn center_falloff_peaks_in_the_middle() {
        let b = lots(31);
        let s = SubdivisionSettings {
            floors_min: 2,
            floors_max: 8,
            height_falloff: HeightFalloff::Center,
            ..Default::default()
        };
        let c = counts(&b, &s);
        let mid: usize = c[12..19].iter().sum();
        let ends: usize = c[..7].iter().sum::<usize>().max(c[24..].iter().sum());
        assert!(mid > ends, "{c:?}");
    }

    #[test]
    fn mix_parse_and_display() {
        let m = HeightMix::parse("low:50, tower:15, podium:10:2-4").unwrap();
        assert_eq!(m.to_string(), "low:50:1-3,tower:15:9-20,podium:10:2-4");
        assert_eq!(HeightMix::parse(&m.to_string()).unwrap(), m);
        assert!(HeightMix::parse("podium:10").is_err());
        assert!(HeightMix::parse("low:0").is_err());
    }
}
