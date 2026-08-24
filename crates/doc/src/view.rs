use serde::{Deserialize, Serialize};

/// Saved viewport camera: orbit parameters mirroring the render camera (f32,
/// like the GPU path). Embedded in the logged `view save` op, so saved files
/// carry their named views through replay.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct NamedView {
    pub target: [f32; 3],
    pub distance: f32,
    /// Radians around +Z, 0 = looking from +X.
    pub yaw: f32,
    /// Radians above the XY plane.
    pub pitch: f32,
    #[serde(default = "default_fov_y")]
    pub fov_y: f32,
    #[serde(default)]
    pub ortho: bool,
    /// Two-point (architectural) perspective; see `OrbitCamera::two_point`.
    #[serde(default)]
    pub two_point: bool,
}

fn default_fov_y() -> f32 {
    45f32.to_radians()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_round_trip_is_exact() {
        let v = NamedView {
            target: [1.5, -2.25, 0.125],
            distance: 42.0,
            yaw: 0.75,
            pitch: -0.5,
            fov_y: 45f32.to_radians(),
            ortho: true,
            two_point: false,
        };
        let json = serde_json::to_string(&v).unwrap();
        let back: NamedView = serde_json::from_str(&json).unwrap();
        assert_eq!(back, v);
    }

    #[test]
    fn missing_optional_fields_default() {
        // Minimal JSON (pre-fov/ortho writers) still loads.
        let v: NamedView = serde_json::from_str(
            r#"{"target": [0.0, 0.0, 1.0], "distance": 30.0, "yaw": 0.0, "pitch": 0.5}"#,
        )
        .unwrap();
        assert_eq!(v.fov_y, 45f32.to_radians());
        assert!(!v.ortho);
        assert!(!v.two_point);
    }
}
