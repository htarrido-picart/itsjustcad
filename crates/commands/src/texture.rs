// SPDX-License-Identifier: AGPL-3.0-only
// Copyright © 2026 Hector Tarrido-Picart

//! Texture import (#360): turn a PNG/JPEG file into the bytes the document's
//! image store keeps, scaled down to [`MAX_IMAGE_PX`] when larger, plus the
//! image's average colour (the flat fill with textures off).

use std::sync::Arc;

use image::{DynamicImage, ImageFormat};
use itsjustcad_doc::{StoredImage, MAX_IMAGE_PX};

/// A texture ready to store, and its average linear colour.
pub struct Prepared {
    pub image: StoredImage,
    pub average: [f32; 3],
    /// Original pixel size when it was scaled down.
    pub scaled_from: Option<(u32, u32)>,
}

/// Decode `bytes` (PNG or JPEG), scale the longest side down to
/// [`MAX_IMAGE_PX`] if larger (re-encoded in the same format), and average it.
/// Images within the limit keep their exact bytes.
pub fn prepare(bytes: &[u8]) -> Result<Prepared, String> {
    let format = image::guess_format(bytes).map_err(|_| "not a PNG or JPEG image".to_string())?;
    if !matches!(format, ImageFormat::Png | ImageFormat::Jpeg) {
        return Err(format!("{format:?} is not supported; use a PNG or JPEG image"));
    }
    let img = image::load_from_memory_with_format(bytes, format).map_err(|e| format!("cannot decode: {e}"))?;
    let (w, h) = (img.width(), img.height());
    if w == 0 || h == 0 {
        return Err("empty image".into());
    }
    let (img, stored, scaled_from) = if w.max(h) > MAX_IMAGE_PX {
        let small = img.resize(MAX_IMAGE_PX, MAX_IMAGE_PX, image::imageops::FilterType::Triangle);
        let mut out = std::io::Cursor::new(Vec::new());
        match format {
            ImageFormat::Jpeg => small
                .to_rgb8()
                .write_with_encoder(image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 90)),
            _ => small.write_to(&mut out, ImageFormat::Png),
        }
        .map_err(|e| format!("cannot re-encode: {e}"))?;
        (small, out.into_inner(), Some((w, h)))
    } else {
        (img, bytes.to_vec(), None)
    };
    Ok(Prepared {
        image: StoredImage { width: img.width(), height: img.height(), bytes: Arc::from(stored) },
        average: average_linear(&img),
        scaled_from,
    })
}

fn srgb_to_linear(c: u8) -> f32 {
    let c = f32::from(c) / 255.0;
    if c <= 0.04045 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) }
}

/// Mean linear RGB over the image, or a 64 px thumbnail of a larger one
/// (cheap, and close enough for a flat stand-in colour).
fn average_linear(img: &DynamicImage) -> [f32; 3] {
    let thumb = if img.width().max(img.height()) > 64 { img.thumbnail(64, 64) } else { img.clone() }.to_rgb8();
    let n = (thumb.width() * thumb.height()).max(1) as f32;
    let mut sum = [0.0f32; 3];
    for p in thumb.pixels() {
        for (s, c) in sum.iter_mut().zip(p.0) {
            *s += srgb_to_linear(c);
        }
    }
    sum.map(|s| s / n)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png(w: u32, h: u32, rgb: [u8; 3]) -> Vec<u8> {
        let img = image::RgbImage::from_pixel(w, h, image::Rgb(rgb));
        let mut out = std::io::Cursor::new(Vec::new());
        img.write_to(&mut out, ImageFormat::Png).unwrap();
        out.into_inner()
    }

    #[test]
    fn small_png_keeps_its_bytes_and_averages() {
        let bytes = png(4, 2, [255, 0, 0]);
        let p = prepare(&bytes).unwrap();
        assert_eq!(&*p.image.bytes, &bytes[..]);
        assert_eq!((p.image.width, p.image.height), (4, 2));
        assert!(p.scaled_from.is_none());
        assert!((p.average[0] - 1.0).abs() < 1e-4 && p.average[1] < 1e-4);
    }

    #[test]
    fn huge_image_is_scaled_to_the_cap() {
        let p = prepare(&png(MAX_IMAGE_PX * 2, 8, [10, 20, 30])).unwrap();
        assert_eq!(p.image.width, MAX_IMAGE_PX);
        assert_eq!(p.scaled_from, Some((MAX_IMAGE_PX * 2, 8)));
        let back = image::load_from_memory(&p.image.bytes).unwrap();
        assert_eq!(back.width(), MAX_IMAGE_PX);
    }

    #[test]
    fn non_images_are_refused() {
        assert!(prepare(b"not an image").is_err());
    }
}
