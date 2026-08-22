use glam::DVec3;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
pub struct Aabb {
    pub min: DVec3,
    pub max: DVec3,
}

impl Aabb {
    pub fn from_points(points: impl IntoIterator<Item = DVec3>) -> Self {
        let mut min = DVec3::splat(f64::INFINITY);
        let mut max = DVec3::splat(f64::NEG_INFINITY);
        for p in points {
            min = min.min(p);
            max = max.max(p);
        }
        Self { min, max }
    }

    pub fn union(self, other: Self) -> Self {
        Self {
            min: self.min.min(other.min),
            max: self.max.max(other.max),
        }
    }

    pub fn center(&self) -> DVec3 {
        (self.min + self.max) * 0.5
    }

    pub fn size(&self) -> DVec3 {
        self.max - self.min
    }
}
