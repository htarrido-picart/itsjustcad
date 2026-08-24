//! Headless CLI runner: executes a script file (or stdin) against the command
//! substrate, optionally renders an offscreen PNG, and saves the document.
//!
//! Exit codes: 0 ok | 1 command error | 2 file/IO error.

use crate::app_verbs::{self, AppVerb};
use itsjustcad_commands::{Session, parse};
use itsjustcad_render::{DisplayMode, OrbitCamera, PanoProjection, StandardView};

// ── Headless view state ─────────────────────────────────────────────────────────

/// View-affecting state accumulated by app-level verbs while a headless script
/// runs. Applied by [`render_headless`] to build the offscreen camera, so
/// `ze` / `view` / `camera` / `display` in a script actually change the render.
#[derive(Clone, Debug, Default)]
pub struct HeadlessView {
    /// Standard view direction, if a `top`/`front`/`persp`/… verb was run.
    pub view: Option<StandardView>,
    /// Lens focal length in mm, from `camera <n>mm|phone|…`.
    pub focal_mm: Option<f32>,
    /// Two-point perspective toggle, from `camera 2point|persp`.
    pub two_point: Option<bool>,
    /// Non-pinhole projection, from `camera pano|fisheye [fov]`. Cleared by
    /// `camera persp`. Rendered via the cubemap remap path.
    pub pano: Option<PanoProjection>,
    /// Display mode, from `display <mode>`.
    pub display: DisplayMode,
}

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
/// App-level verbs (`ze`, `view`, `camera`, `display`, `save`, `help`) are
/// handled here via the shared [`app_verbs::classify`] table so the headless
/// runner accepts the same vocabulary as the GUI:
///   * view-state verbs accumulate into the returned [`HeadlessView`], which
///     [`render_headless`] applies to the offscreen camera;
///   * `save [path]` writes the document immediately;
///   * `help [verb]` prints the reference to stdout;
///   * GUI-only verbs (`template`, `critique`) are warned-and-ignored, not
///     errors — they have no headless meaning.
///
/// Everything else falls through to the substrate parser. Only genuinely
/// unknown commands produce `Err`, so exit code 1 still means "bad command".
///
/// Returns `Ok((session, view))` on success, or `Err((line, message))` for the
/// first failing command.
pub fn run_script_lines(
    mut session: Session,
    lines: &[String],
) -> Result<(Session, HeadlessView), (String, String)> {
    let mut view = HeadlessView::default();
    for line in lines {
        match app_verbs::classify(line) {
            Some(AppVerb::ZoomExtents) => {
                // Framing always tracks the scene extents in headless render;
                // `ze` is a no-op accumulator (kept for script parity).
            }
            Some(AppVerb::View(v)) => view.view = Some(v),
            Some(AppVerb::Display(mode)) => view.display = mode,
            Some(AppVerb::Camera(arg, arg2)) => {
                apply_camera(&mut view, arg.as_deref(), arg2.as_deref())
            }
            Some(AppVerb::Save(path)) => {
                let out = path.as_deref().unwrap_or("out.itsjustcad.json");
                itsjustcad_commands::io::save_file(&session, std::path::Path::new(out))
                    .map_err(|e| (line.clone(), e.to_string()))?;
                println!("saved {out}");
            }
            Some(AppVerb::Help(verb)) => {
                for l in crate::app::help_lines(verb.as_deref()) {
                    println!("{l}");
                }
            }
            Some(AppVerb::GuiOnly(name)) => {
                eprintln!("warning: '{name}' is GUI-only; ignored in headless mode");
            }
            None => {
                let cmd = parse(line).map_err(|e| (line.clone(), e.to_string()))?;
                session.run(cmd).map_err(|e| (line.clone(), e.to_string()))?;
            }
        }
    }
    Ok((session, view))
}

/// Apply a `camera <arg> [arg2]` token to the headless view: two-point /
/// perspective toggles, panorama / fisheye projections, or a numeric/preset
/// focal length (mirrors `App::set_camera`).
fn apply_camera(view: &mut HeadlessView, arg: Option<&str>, arg2: Option<&str>) {
    let Some(arg) = arg else { return };
    match arg {
        "2point" | "twopoint" | "2pt" => {
            view.two_point = Some(true);
            view.pano = None;
        }
        "persp" | "perspective" | "1point" | "normal" => {
            view.two_point = Some(false);
            view.pano = None;
        }
        "pano" | "panorama" | "equirect" | "360" => {
            view.pano = Some(PanoProjection::Equirect);
        }
        "fisheye" | "fish" => {
            view.pano = Some(parse_fisheye(arg2));
        }
        _ => {
            let focal = itsjustcad_render::preset_focal_mm(arg)
                .or_else(|| arg.strip_suffix("mm").unwrap_or(arg).parse::<f32>().ok());
            if let Some(f) = focal.filter(|f| *f > 0.0) {
                view.focal_mm = Some(f);
            }
        }
    }
}

/// Parse the optional fisheye field of view (in degrees) into a
/// [`PanoProjection::Fisheye`]; defaults to a 180° hemisphere. Clamped to a
/// sane 1°..=360° so a stray value can't produce a degenerate lens.
pub(crate) fn parse_fisheye(arg2: Option<&str>) -> PanoProjection {
    match arg2.and_then(|s| s.parse::<f32>().ok()) {
        Some(deg) if deg > 0.0 => {
            PanoProjection::Fisheye { fov: deg.clamp(1.0, 360.0).to_radians() }
        }
        _ => PanoProjection::default_fisheye(),
    }
}

// ── Offscreen PNG renderer ────────────────────────────────────────────────────

/// Render the document to `path` using the headless wgpu path (no window).
///
/// `view` carries the accumulated `view`/`camera`/`display` state from the
/// script; framing (target + distance) always tracks the scene extents.
pub fn render_headless(
    session: &Session,
    path: &std::path::Path,
    view: &HeadlessView,
) -> Result<(), String> {
    use itsjustcad_render::{
        SceneRenderer, Theme, camera_uniform_with_mode, snapshot, DEPTH_FORMAT,
    };

    const W: u32 = 1280;
    const H: u32 = 800;
    const FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8UnormSrgb;

    let theme = Theme::Dark;
    let mode = view.display;

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
    // Orientation: an explicit `view` verb wins; otherwise keep a pleasant
    // default 3/4 perspective.
    if let Some(v) = view.view {
        camera.set_view(v);
    } else {
        camera.pitch = 0.55;
        camera.yaw = -0.6;
    }
    let aspect = W as f32 / H as f32;
    // Lens / projection from `camera` verbs (only meaningful in perspective).
    if let Some(f) = view.focal_mm {
        camera.set_lens_mm(f, aspect);
    }
    if let Some(tp) = view.two_point {
        camera.two_point = tp;
        camera.ortho = false;
    }
    if let Some(p) = view.pano {
        camera.pano = Some(p);
        camera.ortho = false;
        camera.two_point = false;
    }
    // Framing always tracks the scene extents (the `ze` behavior).
    if let Some(bb) = session.doc.scene_aabb() {
        let c = bb.center();
        camera.target = glam::Vec3::new(c.x as f32, c.y as f32, c.z as f32);
        camera.distance = (bb.size().length() as f32 * 1.2).max(5.0);
    } else {
        camera.distance = 16.0;
    }

    // Panorama / fisheye render through the cubemap remap path: the eye sits
    // *inside* the scene (at the framed target) so it is surrounded, and the
    // six faces are captured + remapped instead of a single pinhole pass.
    if view.pano.is_some() {
        // Put the eye at the scene centre by collapsing the orbit distance to
        // a hair; look direction (yaw/pitch) is preserved for the remap basis.
        camera.distance = camera.distance.clamp(1e-4, 0.01);
        let img = itsjustcad_render::render_pano_image(
            &device, &queue, &mut renderer, &camera, theme, mode, W, H, 1024,
        );
        let rgba = image::RgbaImage::from_raw(img.width, img.height, img.rgba)
            .ok_or_else(|| "pano image buffer size mismatch".to_string())?;
        rgba.save(path).map_err(|e| format!("write png: {e}"))?;
        return Ok(());
    }

    let view_proj = camera.view_proj(aspect);
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
        let (out, _view) = run_script_lines(session, &lines).expect("should succeed");
        // BTreeMap<String, LayerStyle> — key is layer name
        assert!(out.doc.layers.contains_key("walls"));
    }

    #[test]
    fn app_verbs_accepted_and_affect_view() {
        let session = Session::default();
        let lines = vec![
            "box 0,0,0 5,5,3".to_owned(),
            "ze".to_owned(),
            "front".to_owned(),
            "camera 35mm".to_owned(),
            "display pencil".to_owned(),
        ];
        let (_out, view) = run_script_lines(session, &lines).expect("app verbs should run");
        assert_eq!(view.view, Some(StandardView::Front));
        assert_eq!(view.focal_mm, Some(35.0));
        assert_eq!(view.display, DisplayMode::Pencil);
    }

    #[test]
    fn camera_pano_and_fisheye_set_projection() {
        let session = Session::default();
        let lines = vec!["camera pano".to_owned()];
        let (_o, view) = run_script_lines(session, &lines).expect("pano verb");
        assert_eq!(view.pano, Some(PanoProjection::Equirect));

        // Fisheye with explicit fov (degrees) -> radians on the projection.
        let (_o, view) =
            run_script_lines(Session::default(), &["camera fisheye 120".to_owned()]).unwrap();
        match view.pano {
            Some(PanoProjection::Fisheye { fov }) => {
                assert!((fov - 120f32.to_radians()).abs() < 1e-4, "fov={fov}")
            }
            other => panic!("expected fisheye, got {other:?}"),
        }

        // Bare `camera fisheye` defaults to a 180° hemisphere.
        let (_o, view) =
            run_script_lines(Session::default(), &["camera fisheye".to_owned()]).unwrap();
        assert_eq!(view.pano, Some(PanoProjection::default_fisheye()));

        // `camera persp` clears any panorama projection.
        let (_o, view) = run_script_lines(
            Session::default(),
            &["camera pano".to_owned(), "camera persp".to_owned()],
        )
        .unwrap();
        assert_eq!(view.pano, None);
    }

    #[test]
    fn fisheye_fov_is_clamped_to_sane_range() {
        // A wild value cannot produce a degenerate (<=0 or absurd) lens.
        assert_eq!(parse_fisheye(Some("0")), PanoProjection::default_fisheye());
        assert_eq!(parse_fisheye(Some("-40")), PanoProjection::default_fisheye());
        match parse_fisheye(Some("99999")) {
            PanoProjection::Fisheye { fov } => {
                assert!((fov - 360f32.to_radians()).abs() < 1e-4, "clamped to 360°")
            }
            _ => panic!("expected fisheye"),
        }
        assert_eq!(parse_fisheye(Some("junk")), PanoProjection::default_fisheye());
    }

    #[test]
    fn gui_only_verbs_are_ignored_not_errors() {
        let session = Session::default();
        let lines = vec!["template".to_owned(), "critique looks off".to_owned()];
        // No error: GUI-only verbs are warned-and-ignored.
        assert!(run_script_lines(session, &lines).is_ok());
    }

    #[test]
    fn save_verb_writes_document() {
        let dir = std::env::temp_dir().join(format!("itsjustcad_hl_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.join("saved.itsjustcad.json");
        let session = Session::default();
        let lines = vec![
            "box 0,0,0 1,1,1".to_owned(),
            format!("save {}", out.display()),
        ];
        assert!(run_script_lines(session, &lines).is_ok());
        assert!(out.exists(), "save verb should have written the document");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unknown_verb_still_errors() {
        let session = Session::default();
        let lines = vec!["definitely_not_a_verb".to_owned()];
        assert!(run_script_lines(session, &lines).is_err());
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
