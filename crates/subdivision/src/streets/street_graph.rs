// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! `StreetGraph` — the output of a road-network generator (plan §7.3): a set of
//! street **centerlines**, each with a ROW **width** and a **hierarchy** tier
//! (spine / connector / stub / alley), plus adjacency (which streets meet at a
//! shared endpoint). Block extraction (`block_extractor`) offsets each
//! centerline by `width/2`, boolean-subtracts the union from the site, and tags
//! the resulting block edges back to the streets here.
//!
//! Pure geometry: no doc/egui. Deterministic — generators seed all randomness
//! from the site + `settings.seed`, so the same input yields byte-identical
//! graphs (the replay/undo invariant).

use glam::DVec2;

/// Where a street sits in the road hierarchy. Determines default ROW width and
/// draw order; the alley tier is inserted last (plan § alley tier).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreetTier {
    /// Primary through-road (the site spine).
    Spine,
    /// Secondary road connecting spines / splitting blocks.
    Connector,
    /// A cul-de-sac stub terminating in a bulb.
    Stub,
    /// A narrow rear lane (alley), inserted when `loading == AlleyLoaded`.
    Alley,
}

/// A single street: an ordered centerline polyline with a ROW width + tier.
#[derive(Debug, Clone)]
pub struct Street {
    /// Stable id (index into [`StreetGraph::streets`] at build time). Carried
    /// onto every block edge coincident with this street's ROW.
    pub id: u32,
    /// Centerline as an ordered open polyline (≥ 2 points).
    pub centerline: Vec<DVec2>,
    /// Right-of-way width (full, curb to curb). Half of this is the offset.
    pub width: f64,
    pub tier: StreetTier,
}

impl Street {
    /// Arc length of the centerline.
    pub fn length(&self) -> f64 {
        self.centerline
            .windows(2)
            .map(|w| w[0].distance(w[1]))
            .sum()
    }

    /// True for the alley tier.
    pub fn is_alley(&self) -> bool {
        self.tier == StreetTier::Alley
    }
}

/// The generated road network. `streets` is ordered; ids equal their index.
#[derive(Debug, Clone, Default)]
pub struct StreetGraph {
    pub streets: Vec<Street>,
}

impl StreetGraph {
    pub fn new() -> StreetGraph {
        StreetGraph { streets: Vec::new() }
    }

    /// Push a street, assigning it the next id (== its index). Returns the id.
    pub fn add(&mut self, centerline: Vec<DVec2>, width: f64, tier: StreetTier) -> u32 {
        let id = self.streets.len() as u32;
        self.streets.push(Street {
            id,
            centerline,
            width,
            tier,
        });
        id
    }

    pub fn is_empty(&self) -> bool {
        self.streets.is_empty()
    }

    pub fn len(&self) -> usize {
        self.streets.len()
    }

    /// Adjacency: pairs of street ids whose endpoints coincide within `tol`
    /// (they meet at an intersection). O(n²) — road networks are small.
    pub fn adjacency(&self, tol: f64) -> Vec<(u32, u32)> {
        let mut out = Vec::new();
        let ends = |s: &Street| {
            [
                *s.centerline.first().unwrap(),
                *s.centerline.last().unwrap(),
            ]
        };
        for i in 0..self.streets.len() {
            for j in (i + 1)..self.streets.len() {
                let ei = ends(&self.streets[i]);
                let ej = ends(&self.streets[j]);
                let touches = ei.iter().any(|p| ej.iter().any(|q| p.distance(*q) < tol));
                if touches {
                    out.push((self.streets[i].id, self.streets[j].id));
                }
            }
        }
        out
    }

    /// All non-alley streets (the base rectilinear tier).
    pub fn roads(&self) -> impl Iterator<Item = &Street> {
        self.streets.iter().filter(|s| !s.is_alley())
    }

    /// Just the alley tier.
    pub fn alleys(&self) -> impl Iterator<Item = &Street> {
        self.streets.iter().filter(|s| s.is_alley())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_assigns_sequential_ids() {
        let mut g = StreetGraph::new();
        let a = g.add(vec![DVec2::ZERO, DVec2::new(10.0, 0.0)], 12.0, StreetTier::Spine);
        let b = g.add(vec![DVec2::new(0.0, 5.0), DVec2::new(10.0, 5.0)], 8.0, StreetTier::Connector);
        assert_eq!(a, 0);
        assert_eq!(b, 1);
        assert_eq!(g.len(), 2);
    }

    #[test]
    fn street_length_is_arc_length() {
        let s = Street {
            id: 0,
            centerline: vec![DVec2::ZERO, DVec2::new(3.0, 0.0), DVec2::new(3.0, 4.0)],
            width: 12.0,
            tier: StreetTier::Spine,
        };
        assert!((s.length() - 7.0).abs() < 1e-12);
    }

    #[test]
    fn adjacency_detects_shared_endpoints() {
        let mut g = StreetGraph::new();
        g.add(vec![DVec2::ZERO, DVec2::new(10.0, 0.0)], 12.0, StreetTier::Spine);
        // Second street starts where the first ends → adjacent.
        g.add(vec![DVec2::new(10.0, 0.0), DVec2::new(10.0, 10.0)], 8.0, StreetTier::Connector);
        // Third is disjoint.
        g.add(vec![DVec2::new(50.0, 50.0), DVec2::new(60.0, 50.0)], 8.0, StreetTier::Connector);
        let adj = g.adjacency(1e-6);
        assert_eq!(adj, vec![(0, 1)]);
    }
}
