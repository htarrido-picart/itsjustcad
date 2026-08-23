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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_points_bounds_all_points() {
        let aabb = Aabb::from_points([
            DVec3::new(1.0, -2.0, 3.0),
            DVec3::new(-1.0, 4.0, 0.0),
            DVec3::new(0.5, 0.0, -5.0),
        ]);
        assert_eq!(aabb.min, DVec3::new(-1.0, -2.0, -5.0));
        assert_eq!(aabb.max, DVec3::new(1.0, 4.0, 3.0));
    }

    #[test]
    fn from_single_point_is_degenerate() {
        let p = DVec3::new(2.0, 3.0, 4.0);
        let aabb = Aabb::from_points([p]);
        assert_eq!(aabb.min, p);
        assert_eq!(aabb.max, p);
        assert_eq!(aabb.size(), DVec3::ZERO);
        assert_eq!(aabb.center(), p);
    }

    #[test]
    fn from_empty_is_inverted_infinite() {
        let aabb = Aabb::from_points([]);
        assert_eq!(aabb.min, DVec3::splat(f64::INFINITY));
        assert_eq!(aabb.max, DVec3::splat(f64::NEG_INFINITY));
        // Union with an empty aabb is the identity.
        let real = Aabb::from_points([DVec3::ZERO, DVec3::ONE]);
        assert_eq!(real.union(aabb), real);
    }

    #[test]
    fn union_covers_both_boxes() {
        let a = Aabb::from_points([DVec3::ZERO, DVec3::ONE]);
        let b = Aabb::from_points([DVec3::new(5.0, -1.0, 0.5), DVec3::new(6.0, 0.0, 2.0)]);
        let u = a.union(b);
        assert_eq!(u.min, DVec3::new(0.0, -1.0, 0.0));
        assert_eq!(u.max, DVec3::new(6.0, 1.0, 2.0));
        assert_eq!(u, b.union(a), "union is commutative");
    }

    #[test]
    fn center_and_size() {
        let aabb = Aabb::from_points([DVec3::new(-1.0, 0.0, 2.0), DVec3::new(3.0, 4.0, 6.0)]);
        assert_eq!(aabb.center(), DVec3::new(1.0, 2.0, 4.0));
        assert_eq!(aabb.size(), DVec3::new(4.0, 4.0, 4.0));
    }
}
