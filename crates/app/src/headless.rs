// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! Headless CLI runner: executes a script file (or stdin) against the command
//! substrate, optionally renders an offscreen PNG, and saves the document.
//!
//! Exit codes: 0 ok | 1 command error | 2 file/IO error.

use crate::app_verbs::{self, AppVerb};
use itsjustcad_commands::{Session, parse};
use itsjustcad_render::{
    DisplayMode, LightMode, OrbitCamera, PanoProjection, SketchyParams, StandardView,
};

/// Build renderer [`UnderlayData`] from a document's transient basemap (already
/// decoded + georeferenced). Shared conceptually with the GUI's `basemap_data`.
pub(crate) fn basemap_scene_data(
    doc: &itsjustcad_doc::Document,
) -> Option<itsjustcad_render::UnderlayData> {
    let b = doc.basemap.as_ref()?;
    if b.rgba.is_empty() || b.width_px == 0 || b.height_px == 0 {
        return None;
    }
    let c = b.quad_corners();
    Some(itsjustcad_render::UnderlayData {
        rgba: b.rgba.clone(),
        width_px: b.width_px,
        height_px: b.height_px,
        corners: [
            [c[0].x as f32, c[0].y as f32, 0.0],
            [c[1].x as f32, c[1].y as f32, 0.0],
            [c[2].x as f32, c[2].y as f32, 0.0],
            [c[3].x as f32, c[3].y as f32, 0.0],
        ],
        opacity: b.opacity,
    })
}

/// Apply a headless `basemap` verb: set or clear the transient basemap on the
/// document. OFFLINE by default (cache-only) so scripts/tests never hit the
/// network; set `ITSJUSTCAD_BASEMAP_NET=1` to permit live tile fetches. Needs a
/// site location (`location`/`sun`/EPW) to georeference against.
fn apply_basemap(
    session: &mut Session,
    args: &crate::app_verbs::BasemapArgs,
) -> Result<(), String> {
    use crate::basemap::{
        build_basemap, default_cache_root, provider_by_name, CachedHttpTileSource,
    };
    if args.clear {
        session.doc.basemap = None;
        return Ok(());
    }
    let loc = session
        .doc
        .location
        .ok_or("basemap needs a site location — run `location <lat> <lon>` first")?;
    let allow_net = std::env::var("ITSJUSTCAD_BASEMAP_NET").as_deref() == Ok("1");
    let provider = provider_by_name(&args.provider);
    let slug = provider.slug().to_string();
    // A small runtime just for the blocking tile GETs (only used when network
    // is allowed; a cache-only run never awaits).
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    let source = CachedHttpTileSource::new(provider, default_cache_root(), rt.handle().clone(), allow_net);
    let b = build_basemap(loc, args.span_m, args.opacity, &slug, &source)?;
    session.doc.basemap = Some(b);
    Ok(())
}

/// Seed the on-disk tile cache with a solid-colour tile for every tile in the
/// current location's basemap grid, so a subsequent `basemap` verb stitches an
/// image with NO network. Used by the offline sanity path. `spec` is an
/// optional `"span_m [r g b]"`; defaults to a warm sand tone at 500 m.
fn seed_basemap_cache(session: &Session, spec: &str) -> Result<(), String> {
    use crate::basemap::{
        default_cache_root, solid_tile_png, tile_cache_path, TileGrid,
    };
    let loc = session
        .doc
        .location
        .ok_or("basemapseed needs a location first")?;
    let mut it = spec.split_whitespace();
    let span_m: f64 = it.next().and_then(|s| s.parse().ok()).unwrap_or(500.0);
    let rgba: [u8; 4] = {
        let v: Vec<u8> = it.filter_map(|s| s.parse().ok()).collect();
        if v.len() >= 3 {
            [v[0], v[1], v[2], 255]
        } else {
            [206, 186, 150, 255]
        }
    };
    let z = crate::basemap::pick_zoom(loc.lat_deg, span_m, 1024.0);
    let half = span_m / 2.0;
    let dlat = half / 111_320.0;
    let dlon = half / (111_320.0 * loc.lat_deg.to_radians().cos()).max(1.0);
    let grid = TileGrid::covering(
        loc.lon_deg - dlon,
        loc.lat_deg - dlat,
        loc.lon_deg + dlon,
        loc.lat_deg + dlat,
        z,
        0,
    );
    let png = solid_tile_png(rgba);
    let root = default_cache_root();
    for t in grid.tiles() {
        let path = tile_cache_path(&root, "osm", t);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        std::fs::write(&path, &png).map_err(|e| e.to_string())?;
    }
    Ok(())
}

// ── Headless view state ─────────────────────────────────────────────────────────

/// View-affecting state accumulated by app-level verbs while a headless script
/// runs. Applied by [`render_headless`] to build the offscreen camera, so
/// `ze` / `view` / `camera` / `display` in a script actually change the render.
#[derive(Clone, Debug)]
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
    /// Lighting model, from `lightmode <mode>`.
    pub light: LightMode,
    /// SketchUp-style thick profile edges + gradient background, from
    /// `profileedges [on|off]` or the `sketchup` preset.
    pub profile_edges: bool,
    /// Hand-drawn "sketchy edges" NPR character, from `sketchy [on|off]` /
    /// `edgefx …`. Default (disabled) is a clean pass.
    pub sketchy: SketchyParams,
    /// Thin mesh feature edges in Shaded mode. ON by default (the SketchUp /
    /// Rhino "shaded + edges" look); toggled by `shadededges [on|off]`.
    pub shaded_edges: bool,
}

impl Default for HeadlessView {
    fn default() -> Self {
        Self {
            view: None,
            focal_mm: None,
            two_point: None,
            pano: None,
            display: DisplayMode::default(),
            light: LightMode::default(),
            profile_edges: false,
            sketchy: SketchyParams::default(),
            shaded_edges: true,
        }
    }
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
            Some(AppVerb::Light(m)) => view.light = m,
            Some(AppVerb::ProfileEdges(on)) => {
                view.profile_edges = on.unwrap_or(!view.profile_edges)
            }
            Some(AppVerb::ShadedEdges(on)) => {
                view.shaded_edges = on.unwrap_or(!view.shaded_edges)
            }
            Some(AppVerb::SketchUp) => {
                view.light = LightMode::Working;
                view.profile_edges = true;
                view.display = DisplayMode::Shaded;
            }
            Some(AppVerb::Sketchy(on)) => {
                view.sketchy.enabled = on.unwrap_or(!view.sketchy.enabled);
            }
            Some(AppVerb::EdgeFx(tokens)) => {
                // Tuning implies the effect is on.
                view.sketchy.enabled = true;
                view.sketchy =
                    view.sketchy.apply_tokens(tokens.iter().map(String::as_str));
            }
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
            Some(AppVerb::Gumball(_)) => {
                // No gizmo in headless mode — silently accepted.
            }
            Some(AppVerb::ReduceMotion(_)) => {
                // No animated UI in headless mode — silently accepted.
            }
            Some(AppVerb::GuiOnly(name)) => {
                eprintln!("warning: '{name}' is GUI-only; ignored in headless mode");
            }
            Some(AppVerb::Basemap(args)) => {
                apply_basemap(&mut session, &args).map_err(|e| (line.clone(), e))?;
            }
            None if line.starts_with("basemapseed") => {
                // Offline sanity helper: seed the local tile cache with a solid
                // colour for the current location's grid, so a following
                // `basemap` renders WITHOUT any network. Not a user verb.
                seed_basemap_cache(&session, line.strip_prefix("basemapseed").unwrap().trim())
                    .map_err(|e| (line.clone(), e))?;
            }
            None => {
                // `controlimages <prefix>` needs the GPU view; the substrate
                // exec can't render, so intercept it here and drive the wgpu
                // control-image path against the accumulated headless view.
                if let Some(prefix) = line.strip_prefix("controlimages ") {
                    let prefix = prefix.trim();
                    if prefix.is_empty() {
                        return Err((line.clone(), "controlimages needs a path prefix".into()));
                    }
                    render_control_images_headless(&session, prefix, &view)
                        .map_err(|e| (line.clone(), e))?;
                    continue;
                }
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
        // `camera phone <preset>`: named real phone lens (bare = iPhone main).
        "phone" => {
            let focal = arg2
                .and_then(itsjustcad_render::phone_preset)
                .map(|p| p.focal_mm)
                .or(if arg2.is_none() { Some(26.0) } else { None });
            if let Some(f) = focal {
                view.focal_mm = Some(f);
                view.pano = None;
                view.two_point = Some(false);
            }
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

/// Build the offscreen [`OrbitCamera`] for a headless render at `aspect`,
/// applying the accumulated `view`/`camera` state and framing to the scene
/// extents. Shared by the PNG renderer and the control-image export so both see
/// the same view.
fn build_headless_camera(session: &Session, view: &HeadlessView, aspect: f32) -> OrbitCamera {
    let mut camera = OrbitCamera::default();
    if let Some(v) = view.view {
        camera.set_view(v);
    } else {
        camera.pitch = 0.55;
        camera.yaw = -0.6;
    }
    if let Some(f) = view.focal_mm {
        camera.set_lens_mm(f, aspect);
    }
    if let Some(tp) = view.two_point {
        camera.two_point = tp;
        camera.ortho = false;
        if tp {
            // Two-point folds the pitch into a vertical frame shear of
            // `tan(pitch)/tan(fov_y/2)` NDC (see `OrbitCamera::two_point_view_proj`).
            // At the default 3/4 framing pitch (~0.55 rad) with a normal lens
            // that shift is >1 NDC, sliding the whole massing out of frame — a
            // GUI user would pan to recompose, but a headless shot cannot. Shoot
            // two-point with a modest architectural rise; the target lift below
            // then recenters the sheared frame.
            camera.pitch = 0.22;
        }
    }
    if let Some(p) = view.pano {
        camera.pano = Some(p);
        camera.ortho = false;
        camera.two_point = false;
    }
    if let Some(bb) = session.doc.scene_aabb() {
        let c = bb.center();
        camera.target = glam::Vec3::new(c.x as f32, c.y as f32, c.z as f32);
        camera.distance = (bb.size().length() as f32 * 1.2).max(5.0);
    } else {
        camera.distance = 16.0;
    }
    // Two-point recenters the sheared frame: the projection slides content down
    // by `tan(pitch)/tan(fov/2)` NDC, which at the target depth equals a world
    // height of `distance * tan(pitch)`. Raising the aim point by that amount
    // puts the scene centre back at frame centre while keeping verticals plumb.
    if camera.two_point {
        camera.target.z -= 0.5 * camera.distance * camera.pitch.tan();
    }
    camera
}

/// Render the three control images (`<prefix>_depth/edge/mask.png`) from the
/// accumulated headless view using an on-demand wgpu device.
pub fn render_control_images_headless(
    session: &Session,
    prefix: &str,
    view: &HeadlessView,
) -> Result<(), String> {
    const W: u32 = 1280;
    const H: u32 = 800;
    let aspect = W as f32 / H as f32;

    let instance = wgpu::Instance::default();
    let adapter =
        pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions::default()))
            .map_err(|e| format!("no wgpu adapter: {e:?}"))?;
    let (device, queue) =
        pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default()))
            .map_err(|e| e.to_string())?;

    let camera = build_headless_camera(session, view, aspect);
    let view_proj = camera.view_proj(aspect);
    let eye = camera.eye();
    // Depth normalization range from the scene extents around the eye.
    let (near, far) = match session.doc.scene_aabb() {
        Some(bb) => {
            let c = bb.center();
            let center = glam::Vec3::new(c.x as f32, c.y as f32, c.z as f32);
            let radius = (bb.size().length() as f32 * 0.5).max(0.5);
            let d = (eye - center).length();
            ((d - radius).max(0.01), d + radius)
        }
        None => (0.1, 100.0),
    };

    itsjustcad_render::render_control_images(
        &device, &queue, &session.doc, view_proj, eye, near, far, W, H, prefix,
    )
    .map(|_| ())
}

// ── Offscreen PNG renderer ────────────────────────────────────────────────────

/// Resolve the render theme from `ITSJUSTCAD_THEME` env var.
///
/// Accepted values: `"light"` → [`itsjustcad_render::Theme::Light`]; anything
/// else (including unset) → [`itsjustcad_render::Theme::Dark`].
pub(crate) fn theme_from_env() -> itsjustcad_render::Theme {
    match std::env::var("ITSJUSTCAD_THEME").ok().as_deref() {
        Some("light") => itsjustcad_render::Theme::Light,
        _ => itsjustcad_render::Theme::Dark,
    }
}

/// Render the document to `path` using the headless wgpu path (no window).
///
/// `view` carries the accumulated `view`/`camera`/`display` state from the
/// script; framing (target + distance) always tracks the scene extents.
pub fn render_headless(
    session: &Session,
    path: &std::path::Path,
    view: &HeadlessView,
) -> Result<(), String> {
    use glam::DVec3;
    use itsjustcad_render::{
        camera_uniform_ex, snapshot_with_mode, ColorModeSnapshot, SceneRenderer, DEPTH_FORMAT,
    };

    const W: u32 = 1280;
    const H: u32 = 800;
    const FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8UnormSrgb;

    // Honour ITSJUSTCAD_THEME=dark|light (default: dark).
    let theme = theme_from_env();
    let mode = view.display;

    let instance = wgpu::Instance::default();
    let adapter =
        pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions::default()))
            .map_err(|e| format!("no wgpu adapter: {e:?}"))?;
    let (device, queue) =
        pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default()))
            .map_err(|e| e.to_string())?;

    let mut renderer = SceneRenderer::new(&device, FORMAT);
    // Depth cue needs the camera eye + scene radius; build them up front so the
    // sketchy transform can bias foreground edges. Aspect only affects the lens,
    // not the eye/radius used here, so the pre-pano camera is fine.
    let aspect = W as f32 / H as f32;
    let sketchy = view.sketchy;
    let (sketchy_eye, sketchy_radius) = if sketchy.active() {
        let cam = build_headless_camera(session, view, aspect);
        let r = session
            .doc
            .scene_aabb()
            .map(|bb| (bb.size().length() as f32 * 0.5).max(0.5))
            .unwrap_or(8.0);
        (Some(cam.eye()), r)
    } else {
        (None, 0.0)
    };
    let mut scene = snapshot_with_mode(
        &session.doc,
        theme,
        ColorModeSnapshot {
            profile_edges: view.profile_edges,
            sketchy,
            sketchy_eye,
            sketchy_radius,
            ..Default::default()
        },
    );

    // Attach the transient basemap (satellite/OSM ground image) if one was set
    // by a `basemap` verb earlier in the script. Pixels are already decoded and
    // georeferenced in local meters.
    scene.basemap = basemap_scene_data(&session.doc);

    // When show_lineweights is on, add thick-line quad meshes for each line
    // with a non-hairline lineweight. In headless mode the egui painter overlay
    // is unavailable, so we tessellate fat lines into mesh geometry instead.
    if session.doc.show_lineweights {
        // Scale: 1 mm lineweight → 0.05 m world-space half-width. At typical
        // building scale (lines spanning metres) a 2 mm pen produces a 10 cm
        // wide ribbon, clearly distinct from a 0.18 mm hairline.
        const HALF_WIDTH_PER_MM: f64 = 0.05;
        let hairline_mm = 0.18_f32;
        for (pts, color, lw_mm) in scene.lines.iter().filter(|(_, _, lw)| *lw > hairline_mm) {
            let half = (*lw_mm as f64) * HALF_WIDTH_PER_MM;
            if pts.len() < 2 {
                continue;
            }
            // Build ribbon quads per segment: two perpendicular ribbons form a
            // cross-section so the fat line is visible from any camera angle.
            let mut positions = Vec::new();
            let mut faces: Vec<[u32; 3]> = Vec::new();
            // Two-sided quad: push both winding orders so the ribbon is visible
            // from either side regardless of camera angle (no culling needed).
            let push_quad = |positions: &mut Vec<DVec3>, faces: &mut Vec<[u32; 3]>,
                              a: DVec3, b: DVec3, perp: DVec3| {
                let i = positions.len() as u32;
                positions.push(a - perp); // 0
                positions.push(a + perp); // 1
                positions.push(b + perp); // 2
                positions.push(b - perp); // 3
                // Front face (counter-clockwise when viewed from perp direction).
                faces.push([i, i + 1, i + 2]);
                faces.push([i, i + 2, i + 3]);
                // Back face (reverse winding) — makes ribbon visible from both sides.
                faces.push([i, i + 2, i + 1]);
                faces.push([i, i + 3, i + 2]);
            };
            for pair in pts.windows(2) {
                let a = DVec3::new(pair[0][0] as f64, pair[0][1] as f64, pair[0][2] as f64);
                let b = DVec3::new(pair[1][0] as f64, pair[1][1] as f64, pair[1][2] as f64);
                let dir = (b - a).normalize_or_zero();
                // XY-plane perpendicular ribbon (visible from above).
                let perp_xy = DVec3::new(-dir.y, dir.x, 0.0) * half;
                push_quad(&mut positions, &mut faces, a, b, perp_xy);
                // Z-direction ribbon (visible from the side).
                let perp_z = DVec3::new(0.0, 0.0, 1.0) * half;
                push_quad(&mut positions, &mut faces, a, b, perp_z);
            }
            if !faces.is_empty() {
                let mesh = kernel_mesh::Mesh::new(positions, faces);
                scene.meshes.push((mesh.to_render(), *color, [0.0_f32, 0.0_f32]));
            }
        }
    }

    renderer.set_scene(&device, &queue, &scene, 0);

    // Orientation, lens, projection and framing (shared with control-image
    // export so both paths see the same view).
    let mut camera = build_headless_camera(session, view, aspect);

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
    let sun_dir = session
        .doc
        .sun
        .map(|s| itsjustcad_solar::sun_direction(s.azimuth_deg, s.altitude_deg));
    let cam = camera_uniform_ex(
        view_proj,
        eye,
        mode,
        view.light,
        sun_dir,
        view.profile_edges, // SketchUp preset → gradient background
    );
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
        renderer.paint(&mut pass, 0, mode, view.shaded_edges);
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
    fn lightmode_and_profile_verbs_accumulate() {
        let session = Session::default();
        let lines = vec![
            "lightmode sun".to_owned(),
            "profileedges on".to_owned(),
        ];
        let (_out, view) = run_script_lines(session, &lines).expect("light verbs run");
        assert_eq!(view.light, LightMode::Sun);
        assert!(view.profile_edges);
    }

    #[test]
    fn sketchup_preset_sets_working_light_and_profiles() {
        let session = Session::default();
        // Start from a non-default state to prove the preset overrides it.
        let lines = vec![
            "lightmode presentation".to_owned(),
            "display wireframe".to_owned(),
            "sketchup".to_owned(),
        ];
        let (_out, view) = run_script_lines(session, &lines).expect("preset runs");
        assert_eq!(view.light, LightMode::Working);
        assert_eq!(view.display, DisplayMode::Shaded);
        assert!(view.profile_edges);
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
    fn camera_phone_presets_set_focal_length() {
        // Named lens resolves to its 35mm-equivalent focal length.
        let (_o, view) = run_script_lines(
            Session::default(),
            &["camera phone iphone-ultrawide".to_owned()],
        )
        .unwrap();
        assert_eq!(view.focal_mm, Some(13.0));

        let (_o, view) =
            run_script_lines(Session::default(), &["camera phone iphone-tele".to_owned()]).unwrap();
        assert_eq!(view.focal_mm, Some(77.0));

        // Bare `camera phone` = iPhone main wide (26mm equiv).
        let (_o, view) =
            run_script_lines(Session::default(), &["camera phone".to_owned()]).unwrap();
        assert_eq!(view.focal_mm, Some(26.0));

        // Unknown lens name leaves focal unset (no silent wrong lens).
        let (_o, view) =
            run_script_lines(Session::default(), &["camera phone boguslens".to_owned()]).unwrap();
        assert_eq!(view.focal_mm, None);
    }

    #[test]
    fn two_point_headless_keeps_massing_in_frame() {
        // Regression: two-point folds pitch into a vertical NDC shear; at the
        // default 3/4 framing pitch with a normal lens that shift slid the whole
        // massing off the bottom of a headless shot. `build_headless_camera` must
        // shoot two-point level enough that a tall tower stays inside the frame.
        let session = Session::default();
        // A compact massing (roughly as wide as it is tall) — the framing keeps a
        // scene of this proportion inside the frame.
        let lines = vec![
            "box -6,-6,0 12,12,10".to_owned(),
            "box 2,2,0 4,4,8".to_owned(),
            "camera 2point".to_owned(),
        ];
        let (out, view) = run_script_lines(session, &lines).expect("two-point script runs");
        assert_eq!(view.two_point, Some(true));

        let aspect = 1280.0 / 800.0;
        let cam = build_headless_camera(&out, &view, aspect);
        assert!(cam.two_point && !cam.ortho);
        let m = cam.view_proj(aspect);

        // Every corner of the massing bounding box must project inside the NDC
        // frame (with a small margin), i.e. it is actually visible in the shot —
        // guards the shear-slides-scene-off-frame regression.
        for &x in &[-6.0f32, 6.0] {
            for &y in &[-6.0f32, 6.0] {
                for &z in &[0.0f32, 10.0] {
                    let c = m * glam::Vec3::new(x, y, z).extend(1.0);
                    let (nx, ny) = (c.x / c.w, c.y / c.w);
                    assert!(
                        nx.abs() <= 1.05 && ny.abs() <= 1.05,
                        "corner ({x},{y},{z}) projects out of frame: ndc=({nx},{ny})"
                    );
                }
            }
        }

        // And verticals must stay plumb (defining property of two-point): the two
        // z-endpoints of a vertical edge share the same screen x.
        let base = m * glam::Vec3::new(6.0, 6.0, 0.0).extend(1.0);
        let top = m * glam::Vec3::new(6.0, 6.0, 10.0).extend(1.0);
        assert!(
            (base.x / base.w - top.x / top.w).abs() < 1e-4,
            "two-point vertical must not converge"
        );
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

    /// Serialise env-var mutation tests so they don't race each other.
    ///
    /// `std::env::set_var` / `remove_var` are process-global; running them
    /// concurrently across threads is UB in Rust ≥ 1.80 and produces flaky
    /// results even on older toolchains.  A single `Mutex` serialises the
    /// three `theme_from_env` tests inside one test binary.
    fn with_theme_env<F: FnOnce()>(value: Option<&str>, f: F) {
        use std::sync::Mutex;
        static ENV_LOCK: Mutex<()> = Mutex::new(());
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let saved = std::env::var("ITSJUSTCAD_THEME").ok();
        match value {
            Some(v) => unsafe { std::env::set_var("ITSJUSTCAD_THEME", v) },
            None => unsafe { std::env::remove_var("ITSJUSTCAD_THEME") },
        }
        f();
        match saved {
            Some(v) => unsafe { std::env::set_var("ITSJUSTCAD_THEME", &v) },
            None => unsafe { std::env::remove_var("ITSJUSTCAD_THEME") },
        }
    }

    #[test]
    fn theme_env_dark_is_default() {
        use itsjustcad_render::Theme;
        with_theme_env(None, || assert_eq!(theme_from_env(), Theme::Dark));
    }

    #[test]
    fn theme_env_light_when_set() {
        use itsjustcad_render::Theme;
        with_theme_env(Some("light"), || assert_eq!(theme_from_env(), Theme::Light));
    }

    #[test]
    fn theme_env_unknown_value_falls_back_to_dark() {
        use itsjustcad_render::Theme;
        with_theme_env(Some("solarized"), || {
            assert_eq!(theme_from_env(), Theme::Dark);
        });
    }
}
