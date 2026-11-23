// SPDX-License-Identifier: AGPL-3.0-only

//! Textured materials (#360): box-mapped UVs for a textured mesh, and the
//! GPU textures behind them — one per stored image, shared by every mesh
//! that uses it, mipmapped and repeating, dropped when no mesh uses it.

use std::collections::HashMap;

use itsjustcad_doc::{ImageId, StoredImage};
use kernel_mesh::RenderMesh;

/// A mesh drawn with an image texture: the flat render mesh (three corners
/// per face), one UV per corner, the fill colour (the image's average,
/// used when textures are off), `[roughness, metallic]`, and the image.
pub struct TexturedMesh {
    pub mesh: RenderMesh,
    pub uvs: Vec<[f32; 2]>,
    pub color: [f32; 4],
    pub material: [f32; 2],
    pub image: ImageId,
    pub pixels: StoredImage,
}

/// Box (tri-planar) mapping of world point `p` on a face with normal `n`:
/// project onto the plane of the normal's dominant axis, one tile every
/// `size` metres. Walls run the image upright (v down = -z); each side is
/// read unmirrored from outside. Ties favour z, then x.
pub fn box_uv(p: [f32; 3], n: [f32; 3], size: [f32; 2]) -> [f32; 2] {
    let [x, y, z] = p;
    let [ax, ay, az] = n.map(f32::abs);
    let (u, v) = if az >= ax && az >= ay {
        if n[2] >= 0.0 { (x, -y) } else { (x, y) }
    } else if ax >= ay {
        (if n[0] >= 0.0 { y } else { -y }, -z)
    } else {
        (if n[1] >= 0.0 { -x } else { x }, -z)
    };
    [u / size[0], v / size[1]]
}

/// Box-mapped UVs for every corner of a flat render mesh.
pub fn box_uvs(mesh: &RenderMesh, size: [f32; 2]) -> Vec<[f32; 2]> {
    mesh.positions.iter().zip(&mesh.normals).map(|(p, n)| box_uv(*p, *n, size)).collect()
}

/// Bind group layout of a material texture: `@group(2)` texture + sampler.
pub(crate) fn layout(device: &wgpu::Device) -> wgpu::BindGroupLayout {
    device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("material_texture_layout"),
        entries: &[
            wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Float { filterable: true },
                    view_dimension: wgpu::TextureViewDimension::D2,
                    multisampled: false,
                },
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 1,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                count: None,
            },
        ],
    })
}

/// RGBA8 mip chain of `img`, level 0 first, each level half the last.
fn mip_chain(img: image::RgbaImage) -> Vec<image::RgbaImage> {
    let mut levels = vec![img];
    loop {
        let last = levels.last().expect("level 0");
        let (w, h) = (last.width(), last.height());
        if w == 1 && h == 1 {
            return levels;
        }
        let next = image::imageops::resize(
            last,
            (w / 2).max(1),
            (h / 2).max(1),
            image::imageops::FilterType::Triangle,
        );
        levels.push(next);
    }
}

/// The GPU textures of the scene's images, keyed by image id.
pub(crate) struct TextureCache {
    layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    groups: HashMap<ImageId, wgpu::BindGroup>,
}

impl TextureCache {
    pub fn new(device: &wgpu::Device) -> Self {
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("material_sampler"),
            address_mode_u: wgpu::AddressMode::Repeat,
            address_mode_v: wgpu::AddressMode::Repeat,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::MipmapFilterMode::Linear,
            anisotropy_clamp: 8,
            ..Default::default()
        });
        Self { layout: layout(device), sampler, groups: HashMap::new() }
    }

    pub fn layout(&self) -> &wgpu::BindGroupLayout {
        &self.layout
    }

    pub fn get(&self, id: &ImageId) -> Option<&wgpu::BindGroup> {
        self.groups.get(id)
    }

    /// Number of GPU textures held.
    pub fn len(&self) -> usize {
        self.groups.len()
    }

    /// Keep exactly the textures of `meshes`: upload the new images (one
    /// texture each, however many meshes share it) and free the rest.
    /// Images that fail to decode are skipped (their meshes draw flat).
    pub fn sync(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, meshes: &[TexturedMesh]) {
        self.groups.retain(|id, _| meshes.iter().any(|m| m.image == *id));
        for m in meshes {
            if self.groups.contains_key(&m.image) {
                continue;
            }
            let Ok(img) = image::load_from_memory(&m.pixels.bytes) else {
                continue;
            };
            let group = self.upload(device, queue, img.to_rgba8());
            self.groups.insert(m.image, group);
        }
    }

    fn upload(&self, device: &wgpu::Device, queue: &wgpu::Queue, img: image::RgbaImage) -> wgpu::BindGroup {
        let levels = mip_chain(img);
        let size = wgpu::Extent3d {
            width: levels[0].width(),
            height: levels[0].height(),
            depth_or_array_layers: 1,
        };
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("material_tex"),
            size,
            mip_level_count: levels.len() as u32,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8UnormSrgb,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        for (i, level) in levels.iter().enumerate() {
            let (w, h) = (level.width(), level.height());
            queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: &texture,
                    mip_level: i as u32,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                level.as_raw(),
                wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(4 * w), rows_per_image: Some(h) },
                wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
            );
        }
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("material_tex_bg"),
            layout: &self.layout,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&view) },
                wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::Sampler(&self.sampler) },
            ],
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn box_uv_projects_on_the_dominant_axis() {
        let s = [2.0, 1.0];
        // Top face: plan x, y (image top toward +y).
        assert_eq!(box_uv([4.0, 3.0, 9.0], [0.0, 0.0, 1.0], s), [2.0, -3.0]);
        // East wall (+x): along y, upright in z.
        assert_eq!(box_uv([9.0, 4.0, 3.0], [1.0, 0.0, 0.0], s), [2.0, -3.0]);
        // West wall (-x) runs the other way, so it is not mirrored from outside.
        assert_eq!(box_uv([9.0, 4.0, 3.0], [-1.0, 0.0, 0.0], s), [-2.0, -3.0]);
        // North (+y) and south (-y) walls along x.
        assert_eq!(box_uv([4.0, 9.0, 3.0], [0.0, 1.0, 0.0], s), [-2.0, -3.0]);
        assert_eq!(box_uv([4.0, 9.0, 3.0], [0.0, -1.0, 0.0], s), [2.0, -3.0]);
        // A slanted face picks its dominant axis; ties go to z.
        assert_eq!(box_uv([4.0, 2.0, 1.0], [0.8, 0.0, 0.6], s), [1.0, -1.0]);
        assert_eq!(box_uv([4.0, 2.0, 1.0], [0.6, 0.0, 0.6], s), [2.0, -2.0]);
    }

    #[test]
    fn box_uvs_tile_once_per_size_and_follow_world_space() {
        // A unit quad on the ground, 2 m tiles: half a tile across.
        let mesh = RenderMesh {
            positions: vec![[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [1.0, 1.0, 0.0]],
            normals: vec![[0.0, 0.0, 1.0]; 3],
            indices: vec![0, 1, 2],
        };
        let uv = box_uvs(&mesh, [2.0, 2.0]);
        assert_eq!(uv, vec![[0.0, 0.0], [0.5, 0.0], [0.5, -0.5]]);
    }

    #[test]
    fn mip_chain_halves_to_one_pixel() {
        let levels = mip_chain(image::RgbaImage::new(8, 2));
        let sizes: Vec<_> = levels.iter().map(|l| (l.width(), l.height())).collect();
        assert_eq!(sizes, [(8, 2), (4, 1), (2, 1), (1, 1)]);
    }
}
