// SPDX-License-Identifier: AGPL-3.0-only
// Copyright © 2026 Hector Tarrido-Picart

//! Content-addressed image store (#360): the encoded PNG/JPEG bytes of every
//! material texture, keyed by a hash of those bytes, so a document carries
//! its textures and replays offline. The bytes travel in the op-log (the
//! logged `imageadd` op, base64 in the JSON) and land here on replay.

use std::sync::Arc;

use base64::Engine as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Longest side of a stored texture, in pixels. Larger images are scaled
/// down on import so a texture adds at most a few MB to the file.
pub const MAX_IMAGE_PX: u32 = 2048;

/// Content hash of an image's stored bytes (FNV-1a 64), written as 16 hex
/// digits. The same picture imported twice is stored once.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ImageId(pub u64);

impl ImageId {
    /// The id of `bytes`.
    pub fn of(bytes: &[u8]) -> Self {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for &b in bytes {
            h = (h ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3);
        }
        ImageId(h)
    }

    pub fn parse(s: &str) -> Option<Self> {
        (s.len() == 16).then(|| u64::from_str_radix(s, 16).ok()).flatten().map(ImageId)
    }
}

impl std::fmt::Display for ImageId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:016x}", self.0)
    }
}

impl Serialize for ImageId {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for ImageId {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        ImageId::parse(&s).ok_or_else(|| serde::de::Error::custom(format!("bad image id '{s}'")))
    }
}

/// One stored image: its encoded bytes (PNG or JPEG, as imported) and pixel
/// size. The bytes are shared, so cloning a document does not copy them.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StoredImage {
    pub width: u32,
    pub height: u32,
    /// Encoded file bytes, base64 in JSON.
    #[serde(serialize_with = "ser_b64", deserialize_with = "de_b64")]
    pub bytes: Arc<[u8]>,
}

impl StoredImage {
    pub fn id(&self) -> ImageId {
        ImageId::of(&self.bytes)
    }
}

fn ser_b64<S: Serializer>(bytes: &Arc<[u8]>, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(&base64::engine::general_purpose::STANDARD.encode(bytes))
}

fn de_b64<'de, D: Deserializer<'de>>(d: D) -> Result<Arc<[u8]>, D::Error> {
    let s = String::deserialize(d)?;
    base64::engine::general_purpose::STANDARD
        .decode(s.as_bytes())
        .map(Arc::from)
        .map_err(serde::de::Error::custom)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_is_stable_hex_and_parses_back() {
        let id = ImageId::of(b"abc");
        assert_eq!(id, ImageId::of(b"abc"));
        assert_ne!(id, ImageId::of(b"abd"));
        let s = id.to_string();
        assert_eq!(s.len(), 16);
        assert_eq!(ImageId::parse(&s), Some(id));
        assert_eq!(ImageId::parse("xyz"), None);
    }

    #[test]
    fn stored_image_round_trips_as_base64() {
        let img = StoredImage { width: 2, height: 1, bytes: Arc::from(&b"\x89PNG\0\xff"[..]) };
        let json = serde_json::to_string(&img).unwrap();
        assert!(json.contains("\"bytes\":\"iVBORwD/\""), "{json}");
        let back: StoredImage = serde_json::from_str(&json).unwrap();
        assert_eq!(back, img);
        assert_eq!(back.id(), img.id());
    }
}
