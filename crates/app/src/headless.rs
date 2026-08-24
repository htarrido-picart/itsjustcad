//! Headless CLI runner: executes a script file (or stdin) against the command
//! substrate, optionally renders an offscreen PNG, and saves the document.
//!
//! Exit codes: 0 ok | 1 command error | 2 file/IO error.

use mydrafter_commands::{Session, parse};

// ── Script parsing ────────────────────────────────────────────────────────────

/// Strip `#` comments and blank lines; return trimmed command strings.
pub fn parse_script(src: &str) -> Vec<String> {
    src.lines()
        .filter_map(|l| {
            let t = l.trim();
            // strip inline comments: everything from '#' onward
            let t = if let Some(idx) = t.find('#') { t[..idx].trim() } else { t };
            if t.is_empty() { None } else { Some(t.to_owned()) }
        })
        .collect()
}

// ── Command runner ────────────────────────────────────────────────────────────

/// Run parsed command lines against a session.
///
/// Returns `Ok(session)` on success, or `Err((line, message))` for the first
/// failing command (exit-code 1 callers print line + message to stderr).
pub fn run_script_lines(
    mut session: Session,
    lines: &[String],
) -> Result<Session, (String, String)> {
    for line in lines {
        let cmd = parse(line).map_err(|e| (line.clone(), e.to_string()))?;
        session.run(cmd).map_err(|e| (line.clone(), e.to_string()))?;
    }
    Ok(session)
}

// ── Offscreen PNG renderer ────────────────────────────────────────────────────

/// Render the document to `path` using the headless wgpu path (no window).
pub fn render_headless(session: &Session, path: &std::path::Path) -> Result<(), String> {
    use mydrafter_render::{
        DisplayMode, OrbitCamera, SceneRenderer, Theme, camera_uniform_with_mode, snapshot,
        DEPTH_FORMAT,
    };

    const W: u32 = 1280;
    const H: u32 = 800;
    const FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8UnormSrgb;

    let theme = Theme::Dark;
    let mode = DisplayMode::default();

    let instance = wgpu::Instance::default();
    let adapter =
        pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions::default()))
            .map_err(|e| format!("no wgpu adapter: {e:?}"))?;
    let (device, queue) =
        pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default()))
            .map_err(|e| e.to_string())?;

    let mut renderer = SceneRenderer::new(&device, FORMAT);
    let scene = snapshot(&session.doc, theme);
    renderer.set_scene(&device, &queue, &scene, 0);

    let mut camera = OrbitCamera::default();
    if let Some(bb) = session.doc.scene_aabb() {
        let c = bb.center();
        camera.target = glam::Vec3::new(c.x as f32, c.y as f32, c.z as f32);
        camera.distance = (bb.size().length() as f32 * 1.2).max(5.0);
    } else {
        camera.distance = 16.0;
        camera.pitch = 0.55;
        camera.yaw = -0.6;
    }

    let view_proj = camera.view_proj(W as f32 / H as f32);
    let eye = camera.eye();
    let cam = camera_uniform_with_mode(view_proj, eye, mode);
    renderer.write_camera(&device, &queue, 0, &cam);

    let color = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("hl_color"),
        size: wgpu::Extent3d { width: W, height: H, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: FORMAT,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let depth = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("hl_depth"),
        size: wgpu::Extent3d { width: W, height: H, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: DEPTH_FORMAT,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
        view_formats: &[],
    });
    let color_view = color.create_view(&Default::default());
    let depth_view = depth.create_view(&Default::default());

    let [br, bg, bb, ba] = theme.background();
    let mut encoder = device.create_command_encoder(&Default::default());
    {
        let pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("hl_pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &color_view,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color {
                        r: br as f64,
                        g: bg as f64,
                        b: bb as f64,
                        a: ba as f64,
                    }),
                    store: wgpu::StoreOp::Store,
                },
                depth_slice: None,
            })],
            depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                view: &depth_view,
                depth_ops: Some(wgpu::Operations {
                    load: wgpu::LoadOp::Clear(1.0),
                    store: wgpu::StoreOp::Discard,
                }),
                stencil_ops: None,
            }),
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        let mut pass = pass.forget_lifetime();
        renderer.paint(&mut pass, 0, mode);
    }

    let bytes_per_row = (W * 4).next_multiple_of(256);
    let readback = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("hl_readback"),
        size: (bytes_per_row * H) as u64,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    encoder.copy_texture_to_buffer(
        color.as_image_copy(),
        wgpu::TexelCopyBufferInfo {
            buffer: &readback,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(bytes_per_row),
                rows_per_image: None,
            },
        },
        wgpu::Extent3d { width: W, height: H, depth_or_array_layers: 1 },
    );
    queue.submit([encoder.finish()]);

    let slice = readback.slice(..);
    slice.map_async(wgpu::MapMode::Read, |r| r.expect("map"));
    device
        .poll(wgpu::PollType::Wait { submission_index: None, timeout: None })
        .map_err(|e| format!("poll: {e:?}"))?;
    let data = slice.get_mapped_range();

    let mut img = image::RgbaImage::new(W, H);
    for y in 0..H {
        let row = &data[(y * bytes_per_row) as usize..][..(W * 4) as usize];
        for x in 0..W {
            let px = &row[(x * 4) as usize..][..4];
            img.put_pixel(x, y, image::Rgba([px[0], px[1], px[2], 255]));
        }
    }
    drop(data);
    img.save(path).map_err(|e| format!("write png: {e}"))?;
    Ok(())
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_script_strips_comments_and_blanks() {
        let src = "# full-line comment\n\nline one # inline\n  line two  \n";
        let lines = parse_script(src);
        assert_eq!(lines, vec!["line one", "line two"]);
    }

    #[test]
    fn parse_script_empty_input() {
        assert!(parse_script("").is_empty());
        assert!(parse_script("# nothing\n\n  \n").is_empty());
    }

    #[test]
    fn run_script_lines_ok() {
        let session = Session::default();
        // `layer <name>` creates and switches to the named layer
        let lines = vec!["layer walls".to_owned()];
        let out = run_script_lines(session, &lines).expect("should succeed");
        // BTreeMap<String, LayerStyle> — key is layer name
        assert!(out.doc.layers.contains_key("walls"));
    }

    #[test]
    fn run_script_lines_bad_command_returns_err() {
        let session = Session::default();
        let lines = vec!["this_is_not_a_command xyz".to_owned()];
        match run_script_lines(session, &lines) {
            Ok(_) => panic!("expected error"),
            Err((line, msg)) => {
                assert_eq!(line, "this_is_not_a_command xyz");
                assert!(!msg.is_empty());
            }
        }
    }

    #[test]
    fn run_script_stops_at_first_error() {
        let session = Session::default();
        let lines = vec![
            "layer first".to_owned(),
            "bad_verb".to_owned(),
            "layer third".to_owned(), // should never run
        ];
        match run_script_lines(session, &lines) {
            Ok(_) => panic!("expected error"),
            Err((line, _)) => assert_eq!(line, "bad_verb"),
        }
    }
}
