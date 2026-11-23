// SPDX-License-Identifier: AGPL-3.0-only
// Copyright © 2026 Hector Tarrido-Picart

use bytemuck::{Pod, Zeroable};
use itsjustcad_doc::LineType;
use kernel_mesh::RenderMesh;

use crate::batch::{Batch, ObjectParams, ParamBuffer, Staged};
use crate::DEPTH_FORMAT;

/// A scene shader's WGSL with the shared prelude (`shaders/common.wgsl`: the
/// camera uniform, background gradient and fog, #363) in front.
macro_rules! scene_shader {
    ($file:literal) => {
        concat!(include_str!("shaders/common.wgsl"), include_str!(concat!("shaders/", $file)))
    };
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub struct CameraUniform {
    pub view_proj: [[f32; 4]; 4],
    pub inv_view_proj: [[f32; 4]; 4],
    pub eye: [f32; 4],
    /// x = mesh fill alpha multiplier (display mode).
    /// y,z,w = sun direction xyz (all-zero = no sun / headlight fallback).
    pub misc: [f32; 4],
    /// x = lighting mode (0 = Working hemispheric, 1 = Sun, 2 = Presentation).
    /// y = background gradient flag. z = 1 hides the ground grid lines, w = 1
    /// hides the world axes (#364; see [`CameraUniform::with_overlays`]).
    /// GPU-only view state; never serialized (no replay concern).
    pub light: [f32; 4],
    /// Screen-space stroke (itsjustcad-ios#174): x,y = target size in device
    /// px, z = stroke width in device px, w = device px per logical px (the
    /// dash pattern scale, #364; 0 reads as 1). Zero x,y,z (the default) keeps
    /// the 1-device-px hardware hairline; see [`DisplayMode::line_pt`].
    pub screen: [f32; 4],
    /// Edge styles (#361): x = depth-cue width of the nearest strokes in
    /// device px (0 = no depth cue), y = 1 draws the dashed back edges, z =
    /// device px per logical px of the back-edge dashes, w spare. See
    /// [`CameraUniform::with_edge_style`].
    pub edges: [f32; 4],
    /// Depth ramp of the depth cue and the fog, filled by
    /// [`SceneRenderer::write_camera`] from the scene bounds: `dot(p, xyz) +
    /// w` runs 0 at the nearest scene point to 1 at the farthest. Zero when
    /// both are off.
    pub cue: [f32; 4],
    /// Fog (#363): x = start, y = end (fractions of the `cue` ramp), z = 1
    /// on, w = 1 takes the colour from the background behind each pixel
    /// (the gradient when it is on). See [`CameraUniform::with_fog`].
    pub fog: [f32; 4],
    /// Fog colour RGBA: the custom colour, or the flat background colour.
    pub fog_color: [f32; 4],
    /// Ambient occlusion (#358): x = 1 on, y = intensity (0..1), z = the
    /// requested distance (world units, or minus a fraction of the scene
    /// diagonal), w = the sampling radius in world units, resolved by
    /// [`SceneRenderer::write_camera`]. See [`CameraUniform::with_ao`].
    pub ao: [f32; 4],
    /// The colour occluded faces darken toward (RGBA).
    pub ao_color: [f32; 4],
    /// Sun shadows (#357): x = 1 on, y = direct light (0..1), z = ambient
    /// (0..1), w = 1 the ground catches them. See
    /// [`CameraUniform::with_shadows`].
    pub shadow: [f32; 4],
    /// Unit direction toward the sun the shadows fall from (xyz), which is
    /// also the key light while they are on.
    pub shadow_dir: [f32; 4],
    /// Live section cut (#359): the active plane as `xyz` = unit normal
    /// (pointing at the removed side), `w` = minus its offset, so a world
    /// point `p` is cut away where `dot(p, xyz) + w` is positive. All zero
    /// = no cut. See [`CameraUniform::with_section`].
    pub clip: [f32; 4],
    /// Section fill colour RGB, `w` = 1 caps the cut solids.
    pub clip_fill: [f32; 4],
}

impl CameraUniform {
    /// The same camera with the ground grid lines and the world axes shown or
    /// hidden for the pane (`grid on|off`, `axes on|off`, #364).
    pub fn with_overlays(mut self, grid: bool, axes: bool) -> Self {
        self.light[2] = if grid { 0.0 } else { 1.0 };
        self.light[3] = if axes { 0.0 } else { 1.0 };
        self
    }

    /// Whether the grid lines are shown (see [`Self::with_overlays`]).
    pub fn shows_grid(&self) -> bool {
        self.light[2] <= 0.5
    }

    /// Whether the world axes are shown (see [`Self::with_overlays`]).
    pub fn shows_axes(&self) -> bool {
        self.light[3] <= 0.5
    }

    /// The same camera with the pane's GPU-side edge style (#361): the
    /// depth cue widens the nearest strokes to `style.depth_cue_px` logical
    /// px (`ppp` device px each), tapering to the mode's own width (at least
    /// one device px) at the far end of the scene; back edges flag the
    /// dashed occluded-edge pass. Call after `screen` is set: depth cue turns
    /// a hairline mode's strokes into 1-device-px fat strokes so they can
    /// widen. The default style leaves the camera as it was.
    pub fn with_edge_style(mut self, style: &crate::EdgeStyle, ppp: f32) -> Self {
        let ppp = ppp.max(1.0);
        self.edges = [0.0, 0.0, ppp, 0.0];
        if style.depth_cue {
            if self.screen[2] <= 0.0 {
                self.screen[2] = 1.0;
            }
            self.edges[0] = (style.depth_cue_px * ppp).max(self.screen[2]);
        }
        if style.back_edges {
            self.edges[1] = 1.0;
        }
        self
    }

    /// The same camera with the pane's fog (#363). `background` is the flat
    /// colour behind the scene (theme or paper, the clear colour): the fog
    /// colour in [`crate::FogColor::Background`] unless the sky/ground
    /// gradient is on, in which case each pixel takes the gradient behind
    /// it. Fog off leaves the camera as it was.
    pub fn with_fog(mut self, style: &crate::FogStyle, background: [f32; 4]) -> Self {
        if !style.on {
            self.fog = [0.0; 4];
            return self;
        }
        let (start, end) = style.range();
        let from_bg = style.color == crate::FogColor::Background;
        self.fog = [start, end, 1.0, if from_bg { 1.0 } else { 0.0 }];
        self.fog_color = if from_bg { background } else { style.custom_rgba() };
        self
    }

    /// Whether the pane draws fog.
    pub fn has_fog(&self) -> bool {
        self.fog[2] > 0.5
    }

    /// The same camera with the pane's ambient occlusion (#358), on only
    /// when `style` is on and `mode` has an opaque fill
    /// ([`DisplayMode::takes_ao`]). Off leaves the camera as it was.
    pub fn with_ao(mut self, style: &crate::AoStyle, mode: DisplayMode) -> Self {
        if !style.on || !mode.takes_ao() {
            self.ao = [0.0; 4];
            self.ao_color = [0.0; 4];
            return self;
        }
        let (request, radius) = match style.units {
            crate::AoUnits::World => (style.radius(1.0), style.radius(1.0)),
            // Resolved against the scene bounds in `write_camera`.
            crate::AoUnits::Scene => (-style.radius(1.0), 0.0),
        };
        self.ao = [1.0, style.strength(), request, radius];
        self.ao_color = style.rgba();
        self
    }

    /// Whether the pane draws ambient occlusion.
    pub fn has_ao(&self) -> bool {
        self.ao[0] > 0.5
    }

    /// The same camera with the sun shadows (#357), on only when `style` is
    /// on and `mode` has an opaque fill ([`DisplayMode::takes_ao`]). `sun`
    /// is the document's direction toward the sun, `None` for the default
    /// ([`crate::ShadowStyle::sun_dir`]). Off leaves the camera as it was.
    pub fn with_shadows(mut self, style: &crate::ShadowStyle, mode: DisplayMode, sun: Option<[f32; 3]>) -> Self {
        if !style.on || !mode.takes_ao() {
            self.shadow = [0.0; 4];
            self.shadow_dir = [0.0; 4];
            return self;
        }
        let ground = if style.ground { 1.0 } else { 0.0 };
        self.shadow = [1.0, style.light_level(), style.dark_level(), ground];
        let [x, y, z] = crate::ShadowStyle::sun_dir(sun);
        self.shadow_dir = [x, y, z, 0.0];
        self
    }

    /// Whether the pane draws sun shadows.
    pub fn has_shadows(&self) -> bool {
        self.shadow[0] > 0.5
    }

    /// The same camera cut by the section `plane` (#359): `[point, normal]`,
    /// the normal pointing at the removed side. Off (no plane, `cuts off`,
    /// or a zero normal) leaves the camera as it was.
    pub fn with_section(mut self, plane: Option<[[f32; 3]; 2]>, style: &crate::SectionStyle) -> Self {
        let n = plane.map(|[_, n]| glam::Vec3::from(n).normalize_or_zero()).unwrap_or_default();
        let Some([p, _]) = plane.filter(|_| style.cuts && n != glam::Vec3::ZERO) else {
            self.clip = [0.0; 4];
            self.clip_fill = [0.0; 4];
            return self;
        };
        self.clip = [n.x, n.y, n.z, -n.dot(glam::Vec3::from(p))];
        let [r, g, b, _] = style.fill_rgba();
        self.clip_fill = [r, g, b, if style.fill { 1.0 } else { 0.0 }];
        self
    }

    /// Whether the pane is cut by a section plane.
    pub fn has_clip(&self) -> bool {
        self.clip[..3] != [0.0; 3]
    }

    /// Whether the pane caps the cut solids in the section fill.
    pub fn draws_section_fill(&self) -> bool {
        self.has_clip() && self.clip_fill[3] > 0.5
    }

    /// Whether the camera depends on the scene bounds: the depth ramp, or an
    /// AO distance relative to the scene.
    fn uses_bounds(&self) -> bool {
        self.uses_ramp() || (self.has_ao() && self.ao[2] < 0.0)
    }

    /// Whether the depth ramp (`cue`) is in use: depth cue or fog.
    fn uses_ramp(&self) -> bool {
        self.edges[0] > 0.0 || self.has_fog()
    }

    /// Whether the pane asks for the dashed back-edge pass.
    pub fn draws_back_edges(&self) -> bool {
        self.edges[1] > 0.5
    }
}

/// Lighting model applied to mesh fills. Decoupled from [`DisplayMode`]: a pane
/// can be Shaded *and* in any lighting mode. View state — never logged.
///
/// * `Working` (default): hemispheric sky/ground ambient fill + one soft 3/4
///   directional key, matte. The ambient floor guarantees no face reads fully
///   black (accessibility min-luminance floor) so geometry stays readable while
///   orienting the model. No specular.
/// * `Sun`: the real SPA solar direction is the key light (for shadow / solar
///   studies) but a hemispheric ambient floor still lifts grazing faces off
///   black. No specular.
/// * `Presentation`: adds Blinn-Phong specular driven by the material presets
///   (glass shiny, concrete matte) on top of the Working fill.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LightMode {
    #[default]
    Working,
    Sun,
    Presentation,
}

impl LightMode {
    pub const ALL: [LightMode; 3] = [LightMode::Working, LightMode::Sun, LightMode::Presentation];

    pub fn label(self) -> &'static str {
        match self {
            LightMode::Working => "Working",
            LightMode::Sun => "Sun",
            LightMode::Presentation => "Presentation",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "working" | "work" => Some(LightMode::Working),
            "sun" | "solar" => Some(LightMode::Sun),
            "presentation" | "present" | "pbr" => Some(LightMode::Presentation),
            _ => None,
        }
    }

    /// Value written to `camera.light.x`, read by the mesh shader to branch the
    /// lighting model.
    pub fn shader_flag(self) -> f32 {
        match self {
            LightMode::Working => 0.0,
            LightMode::Sun => 1.0,
            LightMode::Presentation => 2.0,
        }
    }
}

/// Per-viewport display mode. View state, not model state: never logged.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DisplayMode {
    #[default]
    Shaded,
    /// Feature edges only, no fill.
    Wireframe,
    /// Nearly transparent fill without depth writes + edges show through.
    XRay,
    /// Half-transparent fill + edges.
    Ghosted,
    /// Architect's hidden-line drawing: paper-white fill with depth writes
    /// (occlusion) and black feature edges on top. Background and grid also
    /// switch to paper white regardless of the egui theme.
    Pencil,
    /// Theme-following hidden-line (itsjustcad-ios#174): faces write depth but
    /// no colour, so occluded edges vanish while the faces show the theme
    /// background (and the grid stays hidden behind them). Edges keep the
    /// theme ink, slightly thicker than the default hairline.
    HiddenLine,
    /// Hand-drawn pencil sketch (itsjustcad-ios#174): [`DisplayMode::Pencil`]'s
    /// paper and dark ink with occlusion, plus sketchy edges
    /// ([`crate::sketchify_segments`]) forced on and a pencil-weight stroke.
    Sketchy,
}

impl DisplayMode {
    pub const ALL: [DisplayMode; 7] = [
        DisplayMode::Shaded,
        DisplayMode::Wireframe,
        DisplayMode::XRay,
        DisplayMode::Ghosted,
        DisplayMode::Pencil,
        DisplayMode::HiddenLine,
        DisplayMode::Sketchy,
    ];

    pub fn label(self) -> &'static str {
        match self {
            DisplayMode::Shaded => "Shaded",
            DisplayMode::Wireframe => "Wireframe",
            DisplayMode::XRay => "X-Ray",
            DisplayMode::Ghosted => "Ghosted",
            DisplayMode::Pencil => "Pencil",
            DisplayMode::HiddenLine => "Hidden Line",
            DisplayMode::Sketchy => "Sketchy",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "shaded" => Some(DisplayMode::Shaded),
            "wireframe" | "wire" => Some(DisplayMode::Wireframe),
            "xray" | "x-ray" => Some(DisplayMode::XRay),
            "ghosted" | "ghost" => Some(DisplayMode::Ghosted),
            "pencil" => Some(DisplayMode::Pencil),
            "hiddenline" | "hidden-line" | "hidden" => Some(DisplayMode::HiddenLine),
            "sketchy" | "sketch" => Some(DisplayMode::Sketchy),
            _ => None,
        }
    }

    /// Mesh fill alpha multiplier fed to the shader via the camera UBO.
    /// Pencil uses -1.0 as a sentinel: the shader interprets negative values
    /// as "pencil mode — render paper-white, ignore object color".
    pub fn fill_alpha(self) -> f32 {
        match self {
            DisplayMode::Shaded => 1.0,
            DisplayMode::Wireframe => 0.0, // fill pass skipped entirely
            DisplayMode::XRay => 0.18,
            DisplayMode::Ghosted => 0.55,
            DisplayMode::Pencil | DisplayMode::Sketchy => -1.0, // sentinel: pencil white fill
            // Depth only: the face shows whatever the clear colour is.
            DisplayMode::HiddenLine => 0.0,
        }
    }

    pub fn draws_fill(self) -> bool {
        self != DisplayMode::Wireframe
    }

    /// Whether ambient occlusion (#358) and sun shadows (#357) darken this
    /// mode's fill: the opaque fills (Shaded, Pencil, Sketchy). Wireframe has none, X-ray and
    /// Ghosted see through, and Hidden Line shows the background.
    pub fn takes_ao(self) -> bool {
        matches!(self, DisplayMode::Shaded | DisplayMode::Pencil | DisplayMode::Sketchy)
    }

    /// Whether this mode draws mesh feature edges. Shaded now draws them too:
    /// the SketchUp/Rhino "shaded + edges" default (thin theme-aware ink over
    /// the fill). Only the feature edges (boundary + sharp creases) are emitted
    /// by the snapshot, so a box shows its 12 clean edges with no interior
    /// triangulation. Every mode draws edges; a box uses `edges_enabled` in
    /// `paint` to let the user turn the default Shaded edges off.
    pub fn draws_edges(self) -> bool {
        true
    }

    /// Transparent modes skip depth writes so geometry reads through.
    /// Pencil uses depth writes: occlusion is the whole point of hidden-line.
    pub fn depth_writes(self) -> bool {
        matches!(
            self,
            DisplayMode::Shaded
                | DisplayMode::Pencil
                | DisplayMode::HiddenLine
                | DisplayMode::Sketchy
        )
    }

    /// The clear colour this mode forces over the theme's, if any: paper white
    /// for the pencil modes. Hidden Line follows the theme.
    pub fn background_override(self) -> Option<[f32; 4]> {
        matches!(self, DisplayMode::Pencil | DisplayMode::Sketchy).then(Self::pencil_background)
    }

    /// True when this mode forces sketchy edges on.
    pub fn forces_sketchy(self) -> bool {
        self == DisplayMode::Sketchy
    }

    /// Edge and curve stroke width in logical points (itsjustcad-ios#174), or
    /// `0.0` for the default 1-device-px hairline (1/3 pt at 3x). The
    /// hidden-line modes read as a drawing, so they get a little weight;
    /// Sketchy gets a pencil stroke, kept light enough for dense site plans.
    pub fn line_pt(self) -> f32 {
        match self {
            DisplayMode::HiddenLine | DisplayMode::Pencil => 1.0,
            DisplayMode::Sketchy => 1.5,
            _ => 0.0,
        }
    }

    /// [`DisplayMode::line_pt`] in device pixels for a device-pixel ratio, so a
    /// stroke looks the same at 1x, 2x and 3x. `0.0` for hairline modes.
    pub fn line_px(self, dpr: f32) -> f32 {
        self.line_pt() * dpr.max(1.0)
    }

    /// Pencil mode forces the viewport clear colour to paper white regardless
    /// of the egui dark/light theme.
    pub fn pencil_background() -> [f32; 4] {
        [0.97, 0.97, 0.95, 1.0]
    }
}

/// How object colors are resolved in the viewport. View state — never logged.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ColorMode {
    /// Object color → layer color → theme default (current behavior).
    #[default]
    ByLayer,
    /// Per-object color override wins; falls back to layer then theme.
    ByObject,
    /// Fixed hue per geometry type: teal for meshes, white/dark for curves,
    /// amber for annotations.
    ByType,
    /// Stable hash of the object id → unique hue; great for untangling imports.
    Random,
}

impl ColorMode {
    pub const ALL: [ColorMode; 4] = [
        ColorMode::ByLayer,
        ColorMode::ByObject,
        ColorMode::ByType,
        ColorMode::Random,
    ];

    pub fn label(self) -> &'static str {
        match self {
            ColorMode::ByLayer => "By Layer",
            ColorMode::ByObject => "By Object",
            ColorMode::ByType => "By Type",
            ColorMode::Random => "Random",
        }
    }
}

/// Rhino's default on-screen curve / surface-edge thickness, in *logical*
/// pixels. Rhino draws wires exactly 1 px wide regardless of zoom, and shows
/// selection by COLOUR (amber here), never by thickening — so this is the one
/// width every ordinary scene curve uses.
pub const RHINO_CURVE_PX: f32 = 1.0;

/// Physical curve width in device pixels for a given device-pixel-ratio
/// (egui `pixels_per_point` / the wgpu surface scale factor). Rhino keeps the
/// *physical* thickness constant across DPI, so 1 logical px → 1 px at 1x and
/// 2 px at 2x Retina.
///
/// NOTE: wgpu / WebGPU render `LineStrip`/`LineList` primitives as fixed
/// 1-device-pixel hairlines and expose no per-draw line width (unlike desktop
/// GL's `glLineWidth`), so the viewport wire is drawn as that hardware hairline.
/// At 1x this already equals the Rhino default exactly; on HiDPI the hardware
/// line stays 1 device px, which is the closest wgpu can render without
/// tessellating per-frame screen-space ribbons. This helper is the single place
/// the intended width lives, so a future fat-line pass can consume it directly.
pub fn curve_px(dpr: f32) -> f32 {
    RHINO_CURVE_PX * dpr.max(1.0)
}

/// A CPU ribbon mesh (flat f32 buffers) for a fat profile edge.
struct EdgeRibbon {
    positions: Vec<[f32; 3]>,
    normals: Vec<[f32; 3]>,
    indices: Vec<u32>,
}

/// Tessellate a segment list (consecutive point pairs) into a fat-line ribbon:
/// for each segment, two perpendicular quads (an XY-plane ribbon and a Z-plane
/// ribbon) forming a `+` cross-section, so the thick outline is visible from any
/// camera angle. `half` is the ribbon half-width in world units. The points
/// come in as pairs (`edge` segment soup); odd trailing points are ignored.
fn build_edge_ribbon(points: &[[f32; 3]], half: f32) -> EdgeRibbon {
    let mut positions: Vec<[f32; 3]> = Vec::new();
    let mut normals: Vec<[f32; 3]> = Vec::new();
    let mut indices: Vec<u32> = Vec::new();
    let mut push_quad = |a: glam::Vec3, b: glam::Vec3, perp: glam::Vec3| {
        let n = perp.normalize_or_zero().to_array();
        let i = positions.len() as u32;
        for p in [a - perp, a + perp, b + perp, b - perp] {
            positions.push(p.to_array());
            normals.push(n);
        }
        // Front + back winding so the ribbon shows from either side.
        indices.extend_from_slice(&[i, i + 1, i + 2, i, i + 2, i + 3]);
        indices.extend_from_slice(&[i, i + 2, i + 1, i, i + 3, i + 2]);
    };
    let n = points.len() / 2 * 2;
    let mut k = 0;
    while k + 1 < n {
        let a = glam::Vec3::from_array(points[k]);
        let b = glam::Vec3::from_array(points[k + 1]);
        k += 2;
        let dir = (b - a).normalize_or_zero();
        if dir == glam::Vec3::ZERO {
            continue;
        }
        // XY-plane perpendicular (visible from above).
        let perp_xy = glam::Vec3::new(-dir.y, dir.x, 0.0).normalize_or_zero() * half;
        // A perpendicular that has a Z component (visible from the side).
        let perp_z = dir.cross(perp_xy).normalize_or_zero() * half;
        if perp_xy != glam::Vec3::ZERO {
            push_quad(a, b, perp_xy);
        }
        if perp_z != glam::Vec3::ZERO {
            push_quad(a, b, perp_z);
        }
    }
    EdgeRibbon { positions, normals, indices }
}

/// Stable hue for a given u64 seed (e.g. hash of object id bytes).
/// Returns a fully-saturated, medium-lightness RGBA color.
pub fn hue_from_seed(seed: u64) -> [f32; 4] {
    // Mix the bits to spread similar ids far apart on the hue wheel.
    let h = seed.wrapping_mul(0x9e3779b97f4a7c15).wrapping_add(seed >> 16);
    let hue = (h & 0xFFFF) as f32 / 65536.0; // 0..1
    hsv_to_rgb(hue, 0.75, 0.85)
}

fn hsv_to_rgb(h: f32, s: f32, v: f32) -> [f32; 4] {
    let i = (h * 6.0).floor() as u32 % 6;
    let f = h * 6.0 - (h * 6.0).floor();
    let p = v * (1.0 - s);
    let q = v * (1.0 - f * s);
    let t = v * (1.0 - (1.0 - f) * s);
    let (r, g, b) = match i {
        0 => (v, t, p),
        1 => (q, v, p),
        2 => (p, v, t),
        3 => (p, q, v),
        4 => (t, p, v),
        _ => (v, p, q),
    };
    [r, g, b, 1.0]
}

/// Half-length of the world axis lines in meters. Large enough to read as
/// infinite at any practical zoom, small enough that f32 keeps full precision
/// on the endpoints (1e5 is exactly representable; 2e5 m total span).
pub const AXIS_EXTENT: f32 = 1.0e5;

/// SketchUp axis colours, X red, Y green, Z blue: the world axes here and
/// the app's inference tips and rubber bands (#372/#376) share them.
pub const AXIS_RGBA: [[f32; 4]; 3] = [
    [225.0 / 255.0, 85.0 / 255.0, 85.0 / 255.0, 1.0],
    [110.0 / 255.0, 200.0 / 255.0, 110.0 / 255.0, 1.0],
    [95.0 / 255.0, 145.0 / 255.0, 1.0, 1.0],
];

/// One half of a world axis: `points` run from the origin outward.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AxisLine {
    pub points: [[f32; 3]; 2],
    pub color: [f32; 4],
    /// The negative half: drawn dotted, SketchUp style (#364).
    pub dotted: bool,
}

/// Dash pattern of the dotted negative axis halves, logical px.
pub const AXIS_DOTS: [f32; 4] = [2.0, 4.0, 2.0, 4.0];

/// World-axis overlay geometry: the X (red), Y (green) and Z (blue) axes
/// through the origin, each as a solid positive half and a dotted negative
/// half. These are drawn as REAL line segments through the hairline
/// pipelines — the old analytic tint in `grid.wgsl` wandered at far zoom (f32
/// precision in the per-fragment unproject jitters the hit point by meters),
/// which rendered the axes as jagged dotted staircases.
pub fn world_axis_lines() -> [AxisLine; 6] {
    let half = |axis: usize, sign: f32| {
        let mut end = [0.0; 3];
        end[axis] = sign * AXIS_EXTENT;
        AxisLine { points: [[0.0; 3], end], color: AXIS_RGBA[axis], dotted: sign < 0.0 }
    };
    [half(0, 1.0), half(1, 1.0), half(2, 1.0), half(0, -1.0), half(1, -1.0), half(2, -1.0)]
}

/// Line-list vertices `[x, y, z, s]` for a polyline strip, `s` the world
/// distance along it from the first point, so a dash pattern runs on across
/// the strip's segments (#364).
fn dashed_strip(points: &[[f32; 3]]) -> Vec<[f32; 4]> {
    let mut out = Vec::with_capacity(points.len().saturating_sub(1) * 2);
    let mut s = 0.0;
    for w in points.windows(2) {
        let len = glam::Vec3::from(w[0]).distance(glam::Vec3::from(w[1]));
        out.push([w[0][0], w[0][1], w[0][2], s]);
        s += len;
        out.push([w[1][0], w[1][1], w[1][2], s]);
    }
    out
}

/// Line-list vertices `[x, y, z, s]` for a segment soup: every segment
/// starts its pattern afresh, as mesh edges come in no particular order.
fn dashed_soup(points: &[[f32; 3]]) -> Vec<[f32; 4]> {
    points
        .as_chunks::<2>()
        .0
        .iter()
        .flat_map(|[a, b]| {
            let len = glam::Vec3::from(*a).distance(glam::Vec3::from(*b));
            [[a[0], a[1], a[2], 0.0], [b[0], b[1], b[2], len]]
        })
        .collect()
}

/// Screen-space dash pattern of a linetype in logical px, `[on, off, on,
/// off]`; `None` for a solid line (#364).
pub fn linetype_dash(lt: LineType) -> Option<[f32; 4]> {
    match lt {
        LineType::Continuous => None,
        LineType::Dashed => Some([9.0, 5.0, 9.0, 5.0]),
        LineType::Dotted => Some([2.0, 4.0, 2.0, 4.0]),
        LineType::DashDot => Some([12.0, 4.0, 2.0, 4.0]),
    }
}

struct GpuUnderlay {
    vertex_buf: wgpu::Buffer,
    index_buf: wgpu::Buffer,
    bind_group: wgpu::BindGroup,
}

/// A decoded raster underlay ready for GPU upload: RGBA8 pixels plus the four
/// ground-plane corners (CCW from lower-left) and blend opacity.
pub struct UnderlayData {
    pub rgba: Vec<u8>,
    pub width_px: u32,
    pub height_px: u32,
    /// World-space quad corners (z included), CCW from lower-left.
    pub corners: [[f32; 3]; 4],
    pub opacity: f32,
}

/// One line entry shared by the scene lines and the gumball ghost:
/// `(points, rgba, lineweight_mm)`. Points are a segment soup (LineList pairs)
/// or a strip depending on the consuming pass.
pub type LineEntry = (Vec<[f32; 3]>, [f32; 4], f32);

/// One per-face-coloured mesh (#327): `(mesh, per-face rgb, rgba,
/// [roughness, metallic])`. See [`SceneData::face_meshes`].
pub type FaceMeshEntry = (RenderMesh, Vec<[f32; 3]>, [f32; 4], [f32; 2]);

/// CPU-side scene snapshot handed to the renderer when the document changes.
pub struct SceneData {
    /// `(mesh, rgba, [roughness, metallic])`. The material scalars default to
    /// `[0.5, 0.0]` (mid-rough dielectric) when the object has no `material2`.
    pub meshes: Vec<(RenderMesh, [f32; 4], [f32; 2])>,
    /// Meshes drawn one colour per face (#327): `(mesh, per-face rgb, rgba,
    /// [roughness, metallic])`. The rgb list holds one entry per triangle of
    /// the flat [`RenderMesh`] (three corners each); the rgba supplies alpha.
    /// Kept apart from `meshes` so ordinary meshes keep their vertex layout.
    pub face_meshes: Vec<FaceMeshEntry>,
    /// Line strips: consecutive points, `closed` handled by repeating the seam
    /// point on the CPU side. The `f32` is the effective lineweight in mm
    /// (per-object override or layer weight, whichever wins). Used to generate
    /// fat-line quads when `show_lineweights` is on.
    pub lines: Vec<(Vec<[f32; 3]>, [f32; 4], f32)>,
    /// Mesh feature edges as flat segment lists (point pairs), drawn in the
    /// wireframe/x-ray/ghosted display modes. Lineweight follows the object's
    /// effective weight.
    pub edges: Vec<(Vec<[f32; 3]>, [f32; 4], f32)>,
    /// Linetype of each `lines` entry (#364): curves on a dashed, dotted or
    /// dash-dot layer draw with that screen-space pattern. Entries past the
    /// end of the list (or an empty list) draw solid.
    pub line_types: Vec<LineType>,
    /// Linetype of each `edges` entry, as [`Self::line_types`].
    pub edge_types: Vec<LineType>,
    /// PROFILE / silhouette feature edges — the subset of `edges` that lie on
    /// the object's outline (boundary edges or sharp creases). Drawn THICKER
    /// than interior edges to get the SketchUp "objects have lineweight" look.
    /// Each entry is `(segment points, rgba, half_width_world)` where the ribbon
    /// half-width has already been baked so the renderer can build fat-line
    /// quads. Empty when profile edges are disabled for the pane.
    pub profile_edges: Vec<(Vec<[f32; 3]>, [f32; 4], f32)>,
    /// Point clouds: flat list of positions, rendered as PointList.
    pub points: Vec<(Vec<[f32; 3]>, [f32; 4])>,
    /// Optional raster reference image on the ground plane.
    pub underlay: Option<UnderlayData>,
    /// Optional georeferenced satellite/OSM basemap, drawn UNDER the underlay on
    /// the ground plane. Same textured-quad shape as [`UnderlayData`].
    pub basemap: Option<UnderlayData>,
    /// When true the renderer draws fat-line quads for lines with
    /// non-hairline weights; when false all strokes are 1-pixel hairlines.
    pub show_lineweights: bool,
}

/// The scene's render pipelines. All share one colour format and one MSAA
/// sample count, so every viewport pass (mesh, edges, grid, overlays) agrees
/// with the target it draws into (#356).
struct Pipelines {
    grid_pipeline: wgpu::RenderPipeline,
    mesh_pipeline: wgpu::RenderPipeline,
    /// Mesh fill without depth writes, for the transparent display modes.
    mesh_xray_pipeline: wgpu::RenderPipeline,
    /// The two mesh fills for per-face-coloured meshes (#327): the vertex
    /// carries its face's colour.
    face_pipeline: wgpu::RenderPipeline,
    face_xray_pipeline: wgpu::RenderPipeline,
    curve_pipeline: wgpu::RenderPipeline,
    /// LineList variant of the curve pipeline for mesh feature edges.
    edge_pipeline: wgpu::RenderPipeline,
    /// Screen-space quad strokes for edges and curves when the camera carries
    /// a stroke width (itsjustcad-ios#174).
    fat_pipeline: wgpu::RenderPipeline,
    /// World-axis variant of the edge pipeline: LineList using the `vs_axis`
    /// shader entry, which nudges depth slightly toward the camera so the
    /// coplanar grid plane (which writes an ANALYTIC frag_depth that quantizes
    /// differently from rasterized line depth) cannot stipple over the axes.
    axis_pipeline: wgpu::RenderPipeline,
    /// Dashed hairlines (#364): LineList of `[x, y, z, s]` vertices, the dash
    /// pattern measured in screen px from the distance `s` along the stroke.
    /// Carries the axis pipeline's depth nudge, as the dotted negative axes
    /// use it too.
    dash_pipeline: wgpu::RenderPipeline,
    /// Fat-stroke variant of `dash_pipeline` (one instance per segment).
    fat_dash_pipeline: wgpu::RenderPipeline,
    /// Dashed back edges (#361): fat quads over the edge / curve LineList
    /// buffers with the depth test reversed (GREATER, no depth writes), so
    /// only the occluded parts draw.
    back_pipeline: wgpu::RenderPipeline,
    /// PointList pipeline for point-cloud geometry.
    point_pipeline: wgpu::RenderPipeline,
    /// Textured-quad pipeline (underlay and basemap).
    underlay_pipeline: wgpu::RenderPipeline,
    /// Section fill (#359): the back faces seen through the live cut, flat
    /// in the fill colour (`vs_cap`/`fs_cap`), for the plain and the
    /// per-face-coloured meshes (whose colour attribute it ignores).
    cap_pipeline: wgpu::RenderPipeline,
    cap_face_pipeline: wgpu::RenderPipeline,
}

impl Pipelines {
    fn new(
        device: &wgpu::Device,
        target_format: wgpu::TextureFormat,
        samples: u32,
        camera_layout: &wgpu::BindGroupLayout,
        object_layout: &wgpu::BindGroupLayout,
        underlay_layout: &wgpu::BindGroupLayout,
    ) -> Self {
        let ms = wgpu::MultisampleState { count: samples, ..Default::default() };
        let grid_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("grid_shader"),
            source: wgpu::ShaderSource::Wgsl(scene_shader!("grid.wgsl").into()),
        });
        let mesh_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("mesh_shader"),
            source: wgpu::ShaderSource::Wgsl(scene_shader!("mesh.wgsl").into()),
        });

        let grid_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("grid_pl"),
            bind_group_layouts: &[Some(camera_layout)],
            immediate_size: 0,
        });
        let grid_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("grid_pipeline"),
            layout: Some(&grid_layout),
            vertex: wgpu::VertexState {
                module: &grid_shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[],
            },
            fragment: Some(wgpu::FragmentState {
                module: &grid_shader,
                entry_point: Some("fs_main"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: target_format,
                    // Premultiplied alpha
                    blend: Some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: Some(wgpu::DepthStencilState {
                format: DEPTH_FORMAT,
                depth_write_enabled: Some(true),
                depth_compare: Some(wgpu::CompareFunction::LessEqual),
                stencil: Default::default(),
                bias: Default::default(),
            }),
            multisample: ms,
            multiview_mask: None,
            cache: None,
        });

        let mesh_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("mesh_pl"),
            bind_group_layouts: &[Some(camera_layout), Some(object_layout)],
            immediate_size: 0,
        });
        // Mesh fills. The xray variant has no depth writes and no culling:
        // transparent modes must not occlude, and back faces keep x-ray solids
        // readable. The face variant reads a per-vertex face colour (#327).
        let mesh_fill = |label: &str, xray: bool, face: bool| {
            let plain = wgpu::vertex_attr_array![0 => Float32x3, 1 => Float32x3];
            let colored = wgpu::vertex_attr_array![0 => Float32x3, 1 => Float32x3, 2 => Float32x3];
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(label),
                layout: Some(&mesh_layout),
                vertex: wgpu::VertexState {
                    module: &mesh_shader,
                    entry_point: Some(if face { "vs_face" } else { "vs_main" }),
                    compilation_options: Default::default(),
                    buffers: &[wgpu::VertexBufferLayout {
                        // 3 f32 position + 3 f32 normal (+ 3 f32 face rgb)
                        array_stride: if face { 36 } else { 24 },
                        step_mode: wgpu::VertexStepMode::Vertex,
                        attributes: if face { &colored } else { &plain },
                    }],
                },
                fragment: Some(wgpu::FragmentState {
                    module: &mesh_shader,
                    entry_point: Some(if face { "fs_face" } else { "fs_main" }),
                    compilation_options: Default::default(),
                    targets: &[Some(wgpu::ColorTargetState {
                        format: target_format,
                        blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                }),
                primitive: if xray {
                    wgpu::PrimitiveState::default()
                } else {
                    wgpu::PrimitiveState { cull_mode: Some(wgpu::Face::Back), ..Default::default() }
                },
                depth_stencil: Some(wgpu::DepthStencilState {
                    format: DEPTH_FORMAT,
                    depth_write_enabled: Some(!xray),
                    depth_compare: Some(wgpu::CompareFunction::LessEqual),
                    stencil: Default::default(),
                    bias: Default::default(),
                }),
                multisample: ms,
                multiview_mask: None,
                cache: None,
            })
        };
        let mesh_pipeline = mesh_fill("mesh_pipeline", false, false);
        let mesh_xray_pipeline = mesh_fill("mesh_xray_pipeline", true, false);
        let face_pipeline = mesh_fill("face_pipeline", false, true);
        let face_xray_pipeline = mesh_fill("face_xray_pipeline", true, true);
        // Section fill (#359): front faces culled, depth-written so the cap
        // hides what lies behind it.
        let cap_fill = |label: &str, face: bool| {
            let plain = wgpu::vertex_attr_array![0 => Float32x3, 1 => Float32x3];
            let colored = wgpu::vertex_attr_array![0 => Float32x3, 1 => Float32x3, 2 => Float32x3];
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(label),
                layout: Some(&mesh_layout),
                vertex: wgpu::VertexState {
                    module: &mesh_shader,
                    entry_point: Some("vs_cap"),
                    compilation_options: Default::default(),
                    buffers: &[wgpu::VertexBufferLayout {
                        array_stride: if face { 36 } else { 24 },
                        step_mode: wgpu::VertexStepMode::Vertex,
                        attributes: if face { &colored } else { &plain },
                    }],
                },
                fragment: Some(wgpu::FragmentState {
                    module: &mesh_shader,
                    entry_point: Some("fs_cap"),
                    compilation_options: Default::default(),
                    targets: &[Some(wgpu::ColorTargetState {
                        format: target_format,
                        blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                }),
                primitive: wgpu::PrimitiveState { cull_mode: Some(wgpu::Face::Front), ..Default::default() },
                depth_stencil: Some(wgpu::DepthStencilState {
                    format: DEPTH_FORMAT,
                    depth_write_enabled: Some(true),
                    depth_compare: Some(wgpu::CompareFunction::LessEqual),
                    stencil: Default::default(),
                    bias: Default::default(),
                }),
                multisample: ms,
                multiview_mask: None,
                cache: None,
            })
        };
        let cap_pipeline = cap_fill("cap_pipeline", false);
        let cap_face_pipeline = cap_fill("cap_face_pipeline", true);

        let curve_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("curve_shader"),
            source: wgpu::ShaderSource::Wgsl(scene_shader!("curve.wgsl").into()),
        });
        let curve_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("curve_pipeline"),
            layout: Some(&mesh_layout),
            vertex: wgpu::VertexState {
                module: &curve_shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[wgpu::VertexBufferLayout {
                    array_stride: 12,
                    step_mode: wgpu::VertexStepMode::Vertex,
                    attributes: &wgpu::vertex_attr_array![0 => Float32x3],
                }],
            },
            fragment: Some(wgpu::FragmentState {
                module: &curve_shader,
                entry_point: Some("fs_main"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: target_format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::LineStrip,
                ..Default::default()
            },
            depth_stencil: Some(wgpu::DepthStencilState {
                format: DEPTH_FORMAT,
                depth_write_enabled: Some(true),
                depth_compare: Some(wgpu::CompareFunction::LessEqual),
                stencil: Default::default(),
                bias: Default::default(),
            }),
            multisample: ms,
            multiview_mask: None,
            cache: None,
        });

        // Feature edges: same shader as curves, LineList topology (segment
        // soup instead of strips).
        let edge_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("edge_pipeline"),
            layout: Some(&mesh_layout),
            vertex: wgpu::VertexState {
                module: &curve_shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[wgpu::VertexBufferLayout {
                    array_stride: 12,
                    step_mode: wgpu::VertexStepMode::Vertex,
                    attributes: &wgpu::vertex_attr_array![0 => Float32x3],
                }],
            },
            fragment: Some(wgpu::FragmentState {
                module: &curve_shader,
                entry_point: Some("fs_main"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: target_format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::LineList,
                ..Default::default()
            },
            depth_stencil: Some(wgpu::DepthStencilState {
                format: DEPTH_FORMAT,
                depth_write_enabled: Some(true),
                depth_compare: Some(wgpu::CompareFunction::LessEqual),
                stencil: Default::default(),
                bias: Default::default(),
            }),
            multisample: ms,
            multiview_mask: None,
            cache: None,
        });

        // Fat strokes (itsjustcad-ios#174): one instance per segment of a
        // LineList soup (stride 24: start at 0, end at 12), so edges reuse
        // their hairline buffer as is. Curve strips get a LineList copy
        // (`curve_segments`), since one binding cannot read `p[i+1]` at
        // stride 12.
        let fat_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("fat_pipeline"),
            layout: Some(&mesh_layout),
            vertex: wgpu::VertexState {
                module: &curve_shader,
                entry_point: Some("vs_fat"),
                compilation_options: Default::default(),
                buffers: &[wgpu::VertexBufferLayout {
                    array_stride: 24,
                    step_mode: wgpu::VertexStepMode::Instance,
                    attributes: &wgpu::vertex_attr_array![0 => Float32x3, 1 => Float32x3],
                }],
            },
            fragment: Some(wgpu::FragmentState {
                module: &curve_shader,
                entry_point: Some("fs_fat"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: target_format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: Some(wgpu::DepthStencilState {
                format: DEPTH_FORMAT,
                depth_write_enabled: Some(true),
                depth_compare: Some(wgpu::CompareFunction::LessEqual),
                stencil: Default::default(),
                bias: Default::default(),
            }),
            multisample: ms,
            multiview_mask: None,
            cache: None,
        });

        // World axes: same shader/topology as feature edges, but with the
        // `vs_axis` entry point, which nudges depth a few ULPs toward the
        // camera. The axes are coplanar with the grid plane (z=0) but the grid
        // writes an ANALYTIC frag_depth while lines rasterize interpolated
        // depth — the two quantize differently in Depth24Plus, so without the
        // nudge the depth test flips per-pixel and the (opaque, gradient-mode)
        // grid stipples over the axes. The underlay solves the same problem
        // with a fixed-function depth bias, but wgpu rejects depth bias on
        // line topology, hence the vertex-shader nudge.
        let axis_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("axis_pipeline"),
            layout: Some(&mesh_layout),
            vertex: wgpu::VertexState {
                module: &curve_shader,
                entry_point: Some("vs_axis"),
                compilation_options: Default::default(),
                buffers: &[wgpu::VertexBufferLayout {
                    array_stride: 12,
                    step_mode: wgpu::VertexStepMode::Vertex,
                    attributes: &wgpu::vertex_attr_array![0 => Float32x3],
                }],
            },
            fragment: Some(wgpu::FragmentState {
                module: &curve_shader,
                entry_point: Some("fs_main"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: target_format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::LineList,
                ..Default::default()
            },
            depth_stencil: Some(wgpu::DepthStencilState {
                format: DEPTH_FORMAT,
                depth_write_enabled: Some(true),
                depth_compare: Some(wgpu::CompareFunction::LessEqual),
                stencil: Default::default(),
                bias: Default::default(),
            }),
            multisample: ms,
            multiview_mask: None,
            cache: None,
        });

        // Dashed strokes (#364): the curve shader's dash entry points over
        // `[x, y, z, s]` vertices. Both topologies must share the MSAA count.
        let dash = |label: &str, fat: bool| {
            let hair = wgpu::vertex_attr_array![0 => Float32x4];
            let pair = wgpu::vertex_attr_array![0 => Float32x4, 1 => Float32x4];
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(label),
                layout: Some(&mesh_layout),
                vertex: wgpu::VertexState {
                    module: &curve_shader,
                    entry_point: Some(if fat { "vs_fat_dash" } else { "vs_dash" }),
                    compilation_options: Default::default(),
                    buffers: &[wgpu::VertexBufferLayout {
                        array_stride: if fat { 32 } else { 16 },
                        step_mode: if fat {
                            wgpu::VertexStepMode::Instance
                        } else {
                            wgpu::VertexStepMode::Vertex
                        },
                        attributes: if fat { &pair } else { &hair },
                    }],
                },
                fragment: Some(wgpu::FragmentState {
                    module: &curve_shader,
                    entry_point: Some(if fat { "fs_fat_dash" } else { "fs_dash" }),
                    compilation_options: Default::default(),
                    targets: &[Some(wgpu::ColorTargetState {
                        format: target_format,
                        blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                }),
                primitive: if fat {
                    wgpu::PrimitiveState::default()
                } else {
                    wgpu::PrimitiveState {
                        topology: wgpu::PrimitiveTopology::LineList,
                        ..Default::default()
                    }
                },
                depth_stencil: Some(wgpu::DepthStencilState {
                    format: DEPTH_FORMAT,
                    depth_write_enabled: Some(true),
                    depth_compare: Some(wgpu::CompareFunction::LessEqual),
                    stencil: Default::default(),
                    bias: Default::default(),
                }),
                multisample: ms,
                multiview_mask: None,
                cache: None,
            })
        };
        let dash_pipeline = dash("dash_pipeline", false);
        let fat_dash_pipeline = dash("fat_dash_pipeline", true);

        // Back edges (#361): the fat-stroke layout over the plain LineList
        // buffers, depth-tested GREATER without writes.
        let back_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("back_pipeline"),
            layout: Some(&mesh_layout),
            vertex: wgpu::VertexState {
                module: &curve_shader,
                entry_point: Some("vs_back"),
                compilation_options: Default::default(),
                buffers: &[wgpu::VertexBufferLayout {
                    array_stride: 24,
                    step_mode: wgpu::VertexStepMode::Instance,
                    attributes: &wgpu::vertex_attr_array![0 => Float32x3, 1 => Float32x3],
                }],
            },
            fragment: Some(wgpu::FragmentState {
                module: &curve_shader,
                entry_point: Some("fs_back"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: target_format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: Some(wgpu::DepthStencilState {
                format: DEPTH_FORMAT,
                depth_write_enabled: Some(false),
                depth_compare: Some(wgpu::CompareFunction::Greater),
                stencil: Default::default(),
                bias: Default::default(),
            }),
            multisample: ms,
            multiview_mask: None,
            cache: None,
        });

        // Point cloud: same shader as curves, PointList topology.
        let point_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("point_pipeline"),
            layout: Some(&mesh_layout),
            vertex: wgpu::VertexState {
                module: &curve_shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[wgpu::VertexBufferLayout {
                    array_stride: 12,
                    step_mode: wgpu::VertexStepMode::Vertex,
                    attributes: &wgpu::vertex_attr_array![0 => Float32x3],
                }],
            },
            fragment: Some(wgpu::FragmentState {
                module: &curve_shader,
                entry_point: Some("fs_main"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: target_format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::PointList,
                ..Default::default()
            },
            depth_stencil: Some(wgpu::DepthStencilState {
                format: DEPTH_FORMAT,
                depth_write_enabled: Some(true),
                depth_compare: Some(wgpu::CompareFunction::LessEqual),
                stencil: Default::default(),
                bias: Default::default(),
            }),
            multisample: ms,
            multiview_mask: None,
            cache: None,
        });

        // Underlay: a textured quad. Bind group 1 = texture + sampler + opacity.
        let underlay_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("underlay_shader"),
            source: wgpu::ShaderSource::Wgsl(scene_shader!("underlay.wgsl").into()),
        });
        let underlay_pl = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("underlay_pl"),
            bind_group_layouts: &[Some(camera_layout), Some(underlay_layout)],
            immediate_size: 0,
        });
        let underlay_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("underlay_pipeline"),
            layout: Some(&underlay_pl),
            vertex: wgpu::VertexState {
                module: &underlay_shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[wgpu::VertexBufferLayout {
                    array_stride: 20, // 3 f32 position + 2 f32 uv
                    step_mode: wgpu::VertexStepMode::Vertex,
                    attributes: &wgpu::vertex_attr_array![0 => Float32x3, 1 => Float32x2],
                }],
            },
            fragment: Some(wgpu::FragmentState {
                module: &underlay_shader,
                entry_point: Some("fs_main"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: target_format,
                    blend: Some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: Some(wgpu::DepthStencilState {
                format: DEPTH_FORMAT,
                depth_write_enabled: Some(true),
                depth_compare: Some(wgpu::CompareFunction::LessEqual),
                stencil: Default::default(),
                // The underlay quad is coplanar with the grid plane (both z=0),
                // but the grid writes an ANALYTIC frag_depth while this quad
                // rasterizes interpolated depth — the two quantize differently
                // in Depth24Plus, so without an offset the depth test flips
                // per-pixel and the grid's axis lines stipple over the image.
                // A small constant bias pushes the quad behind the grid plane.
                bias: wgpu::DepthBiasState {
                    constant: 4,
                    slope_scale: 0.0,
                    clamp: 0.0,
                },
            }),
            multisample: ms,
            multiview_mask: None,
            cache: None,
        });
        Self {
            grid_pipeline,
            mesh_pipeline,
            mesh_xray_pipeline,
            face_pipeline,
            face_xray_pipeline,
            curve_pipeline,
            edge_pipeline,
            fat_pipeline,
            axis_pipeline,
            dash_pipeline,
            fat_dash_pipeline,
            back_pipeline,
            point_pipeline,
            underlay_pipeline,
            cap_pipeline,
            cap_face_pipeline,
        }
    }
}

/// Owns all wgpu resources; lives in egui-wgpu's `CallbackResources` type map.
pub struct SceneRenderer {
    /// Every scene pipeline, built for one colour format and MSAA sample
    /// count. Rebuilt as a set by [`SceneRenderer::set_sample_count`].
    pipes: Pipelines,
    target_format: wgpu::TextureFormat,
    /// Sample count every pipeline in `pipes` was built for (1 = no MSAA).
    sample_count: u32,
    /// Per-pane multisampled targets for the viewport MSAA path (#356).
    pub(crate) msaa: crate::msaa::ViewportMsaa,
    /// Per-pane ambient-occlusion targets and passes (#358).
    pub(crate) ao: crate::ao::ViewportAo,
    /// The sun shadow map shared by every pane (#357).
    pub(crate) shadow: crate::shadow::ViewportShadow,
    /// LineList copies of `lines` (strips) for the fat-stroke pass.
    curve_segments: Batch,
    /// Per pane: whether its last camera asked for fat strokes.
    fat_lines: Vec<bool>,
    /// Per pane: whether its last camera hid the world axes (#364).
    axes_hidden: Vec<bool>,
    /// Per pane: its last camera, re-sent with a fresh depth-cue ramp when
    /// the scene bounds change (#361).
    pub(crate) last_cameras: Vec<CameraUniform>,
    /// World bounds `(min, max)` of the uploaded scene's strokes and meshes,
    /// for the depth-cue ramp (#361).
    scene_bounds: Option<(glam::Vec3, glam::Vec3)>,
    /// World bounds of the faces alone (`meshes` and `face_meshes`), which
    /// the sun shadow map is fitted to (#357).
    pub(crate) caster_bounds: Option<(glam::Vec3, glam::Vec3)>,
    pub(crate) camera_layout: wgpu::BindGroupLayout,
    /// One camera UBO + bind group per viewport pane. egui-wgpu runs every
    /// callback's `prepare` before any `paint`, so panes must not share a
    /// single camera buffer — the last write would win for all of them.
    pub(crate) cameras: Vec<(wgpu::Buffer, wgpu::BindGroup)>,
    object_layout: wgpu::BindGroupLayout,
    /// Per-object color/material for every scene batch, one bind group.
    scene_params: ParamBuffer,
    /// Parameters of the ghost and axis batches, kept apart so a drag frame
    /// or the constant axes never rewrite the scene's buffer.
    ghost_params: ParamBuffer,
    axis_params: ParamBuffer,
    /// Underlay bind layout: (texture, sampler, opacity).
    underlay_layout: wgpu::BindGroupLayout,
    pub(crate) meshes: Batch,
    /// Per-face-coloured meshes, 9 floats per vertex (#327).
    pub(crate) face_meshes: Batch,
    lines: Batch,
    edges: Batch,
    /// Profile / silhouette edges tessellated into fat-line ribbon meshes so
    /// they render visibly thicker than the 1-pixel interior edge lines.
    profile_ribbons: Batch,
    point_clouds: Batch,
    /// Transient ghost preview lines (the iOS gumball live wireframe). Rebuilt
    /// every drag frame via [`SceneRenderer::set_ghost_lines`] and cleared on
    /// commit/cancel. Drawn as hairline segments over the scene, under the grid.
    /// Independent of the scene `generation` so it updates between doc changes.
    ghost_lines: Batch,
    /// Persistent solid positive world-axis halves (see [`world_axis_lines`]).
    /// Built once in [`SceneRenderer::new`]; drawn every frame unless the
    /// pane's camera hides the axes, independent of document content.
    axis_lines: Batch,
    /// The dotted negative axis halves, `[x, y, z, s]` (dash pipeline).
    axis_dots: Batch,
    /// Curves and mesh edges on a dashed-linetype layer (#364), `[x, y, z,
    /// s]` line lists drawn by the dash pipelines instead of `lines` /
    /// `curve_segments` / `edges`.
    dash_lines: Batch,
    dash_edges: Batch,
    underlay: Option<GpuUnderlay>,
    basemap: Option<GpuUnderlay>,
    /// Document generation the GPU buffers were built from.
    pub generation: u64,
}

impl SceneRenderer {
    /// Single-sample renderer (no MSAA) for `target_format`.
    pub fn new(device: &wgpu::Device, target_format: wgpu::TextureFormat) -> Self {
        Self::with_samples(device, target_format, 1)
    }

    /// Renderer whose pipelines draw into `samples`-sample targets (#356). The
    /// caller must have checked the count with [`crate::supported_sample_counts`].
    pub fn with_samples(
        device: &wgpu::Device,
        target_format: wgpu::TextureFormat,
        samples: u32,
    ) -> Self {
        let camera_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("camera_layout"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                // The pane's ambient occlusion (#358), read by the fills.
                crate::ao::occlusion_entry(1),
                // The sun shadow map (#357): its light uniform, depth and
                // comparison sampler, shared by every pane.
                crate::shadow::uniform_entry(5),
                crate::shadow::map_entry(6),
                crate::shadow::sampler_entry(7),
            ],
        });
        let object_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("object_layout"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: true,
                    min_binding_size: Some(ParamBuffer::binding_size()),
                },
                count: None,
            }],
        });
        let underlay_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("underlay_layout"),
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
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
            ],
        });
        let pipes = Pipelines::new(
            device,
            target_format,
            samples,
            &camera_layout,
            &object_layout,
            &underlay_layout,
        );

        let mut axis_lines = Batch::new("axis");
        let mut axis_dots = Batch::new("axis_dots");
        let mut axis = Staged::default();
        let mut dots = Staged::default();
        // material.w = 1: the section cut (#359) never removes the axes.
        let uncut = |mut p: ObjectParams| {
            p.material[3] = 1.0;
            p
        };
        for a in world_axis_lines() {
            if a.dotted {
                dots.push_dashed(&dashed_strip(&a.points), uncut(ObjectParams::dashed(a.color, AXIS_DOTS)));
            } else {
                axis.push_segments(&a.points, uncut(ObjectParams::stroke(a.color)));
            }
        }
        let mut axis_params = ParamBuffer::new(device);
        axis_params.init(device, &object_layout, axis.params.iter().chain(&dots.params));
        axis_dots.slot_base = axis.params.len() as u32;
        axis_lines.init(device, axis);
        axis_dots.init(device, dots);

        Self {
            pipes,
            target_format,
            sample_count: samples,
            msaa: Default::default(),
            ao: crate::ao::ViewportAo::new(device),
            shadow: crate::shadow::ViewportShadow::new(device),
            curve_segments: Batch::new("curve_segments"),
            fat_lines: Vec::new(),
            axes_hidden: Vec::new(),
            last_cameras: Vec::new(),
            scene_bounds: None,
            caster_bounds: None,
            camera_layout,
            cameras: Vec::new(),
            scene_params: ParamBuffer::new(device),
            ghost_params: ParamBuffer::new(device),
            axis_params,
            object_layout,
            underlay_layout,
            meshes: Batch::new("meshes"),
            face_meshes: Batch::new("face_meshes"),
            lines: Batch::new("curves"),
            edges: Batch::new("edges"),
            profile_ribbons: Batch::new("profile_ribbons"),
            point_clouds: Batch::new("point_clouds"),
            ghost_lines: Batch::new("ghost_lines"),
            axis_lines,
            axis_dots,
            dash_lines: Batch::new("dash_lines"),
            dash_edges: Batch::new("dash_edges"),
            underlay: None,
            basemap: None,
            generation: u64::MAX,
        }
    }

    /// MSAA sample count the pipelines were built for (1 = off).
    pub fn sample_count(&self) -> u32 {
        self.sample_count
    }

    /// Colour format the pipelines target.
    pub fn target_format(&self) -> wgpu::TextureFormat {
        self.target_format
    }

    /// Rebuild every pipeline for a new MSAA sample count, keeping the
    /// uploaded scene. No-op when the count is unchanged, so a per-frame call
    /// costs nothing; pane targets of the old count are dropped.
    pub fn set_sample_count(&mut self, device: &wgpu::Device, samples: u32) {
        if samples == self.sample_count {
            return;
        }
        self.pipes = Pipelines::new(
            device,
            self.target_format,
            samples,
            &self.camera_layout,
            &self.object_layout,
            &self.underlay_layout,
        );
        self.sample_count = samples;
        self.msaa.clear();
    }

    /// Upload the camera for one viewport pane, growing the slot list on demand.
    pub fn write_camera(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        viewport: usize,
        cam: &CameraUniform,
    ) {
        while self.cameras.len() <= viewport {
            let buf = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("camera_ubo"),
                size: std::mem::size_of::<CameraUniform>() as u64,
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            let bind_group = self.camera_bind_group(device, &buf, self.cameras.len());
            self.cameras.push((buf, bind_group));
        }
        let mut ao = cam.ao;
        if cam.has_ao() && ao[2] < 0.0 {
            let diagonal = self.scene_bounds.map_or(1.0, |(lo, hi)| (hi - lo).length());
            ao[3] = -ao[2] * diagonal.max(1e-3);
        }
        let cam = &CameraUniform { cue: depth_cue_ramp(cam, self.scene_bounds), ao, ..*cam };
        queue.write_buffer(&self.cameras[viewport].0, 0, bytemuck::bytes_of(cam));
        if self.fat_lines.len() <= viewport {
            self.fat_lines.resize(viewport + 1, false);
            self.axes_hidden.resize(viewport + 1, false);
            self.last_cameras.resize(viewport + 1, *cam);
        }
        self.last_cameras[viewport] = *cam;
        self.fat_lines[viewport] =
            cam.screen[0] > 0.0 && cam.screen[1] > 0.0 && cam.screen[2] > 0.0;
        self.axes_hidden[viewport] = !cam.shows_axes();
    }

    /// Pane `viewport`'s camera bind group over `buf`: the camera and the
    /// pane's occlusion (its AO target, else the empty one).
    pub(crate) fn camera_bind_group(
        &self,
        device: &wgpu::Device,
        buf: &wgpu::Buffer,
        viewport: usize,
    ) -> wgpu::BindGroup {
        device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("camera_bind_group"),
            layout: &self.camera_layout,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: buf.as_entire_binding() },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(self.ao.occlusion_view(viewport)),
                },
                wgpu::BindGroupEntry { binding: 5, resource: self.shadow.uniform().as_entire_binding() },
                wgpu::BindGroupEntry {
                    binding: 6,
                    resource: wgpu::BindingResource::TextureView(self.shadow.map_view()),
                },
                wgpu::BindGroupEntry { binding: 7, resource: wgpu::BindingResource::Sampler(self.shadow.sampler()) },
            ],
        })
    }

    /// Replace the mesh fill only, leaving the other scene batches as they
    /// are. Geometry identical to what the GPU already holds is not re-sent.
    pub fn set_meshes(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        meshes: &[(RenderMesh, [f32; 4], [f32; 2])],
        generation: u64,
    ) {
        self.meshes.set(device, queue, stage_meshes(meshes));
        self.write_scene_params(device, queue);
        self.generation = generation;
        self.caster_bounds = bounds_of(meshes.iter().flat_map(|m| m.0.positions.iter()));
    }

    /// Upload a scene snapshot into the shared batches (#269). Unchanged
    /// geometry is skipped, so a color-only change (selection, theme, color
    /// mode) costs one in-place write of the parameter buffer. The queue also
    /// uploads texture pixels for the underlay.
    pub fn set_scene(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        scene: &SceneData,
        generation: u64,
    ) {
        self.meshes.set(device, queue, stage_meshes(&scene.meshes));
        let mut faces = Staged::default();
        for (mesh, rgb, color, rm) in &scene.face_meshes {
            let params = ObjectParams::surface(*color, [rm[0], rm[1], 0.0, 0.0]);
            faces.push_face_mesh(&mesh.positions, &mesh.normals, rgb, &mesh.indices, params);
        }
        self.face_meshes.set(device, queue, faces);

        // Curves: strips for the hairline pass, plus LineList copies of the
        // same strips for the fat-stroke pass.
        // Dashed-linetype curves and edges go to their own `[x, y, z, s]`
        // batches instead (#364).
        let mut lines = Staged::default();
        let mut segments = Staged::default();
        let mut dash_lines = Staged::default();
        for (i, (points, color, _lw_mm)) in scene.lines.iter().enumerate() {
            if points.len() < 2 {
                continue; // a strip needs at least one segment
            }
            let lt = scene.line_types.get(i).copied().unwrap_or_default();
            if let Some(dash) = linetype_dash(lt) {
                dash_lines.push_dashed(&dashed_strip(points), ObjectParams::dashed(*color, dash));
                continue;
            }
            let pairs: Vec<[f32; 3]> = points.windows(2).flatten().copied().collect();
            segments.push_segments(&pairs, ObjectParams::stroke(*color));
            lines.push_points(points, ObjectParams::stroke(*color));
        }
        self.lines.set(device, queue, lines);
        self.curve_segments.set(device, queue, segments);
        self.dash_lines.set(device, queue, dash_lines);

        let mut edges = Staged::default();
        let mut dash_edges = Staged::default();
        for (i, (points, color, _lw_mm)) in scene.edges.iter().enumerate() {
            if points.len() < 2 {
                continue;
            }
            let lt = scene.edge_types.get(i).copied().unwrap_or_default();
            match linetype_dash(lt) {
                Some(dash) => dash_edges
                    .push_dashed(&dashed_soup(points), ObjectParams::dashed(*color, dash)),
                None => edges.push_segments(points, ObjectParams::stroke(*color)),
            }
        }
        self.edges.set(device, queue, edges);
        self.dash_edges.set(device, queue, dash_edges);

        // Profile edges: tessellate into ribbon meshes, drawn with the solid
        // mesh pipeline. Fully matte so they read as flat ink.
        let mut ribbons = Staged::default();
        for (points, color, half) in &scene.profile_edges {
            if points.len() < 2 || *half <= 0.0 {
                continue;
            }
            let ribbon = build_edge_ribbon(points, *half);
            let params = ObjectParams::surface(*color, [1.0, 0.0, 0.0, 0.0]);
            ribbons.push_mesh(&ribbon.positions, &ribbon.normals, &ribbon.indices, params);
        }
        self.profile_ribbons.set(device, queue, ribbons);

        let mut clouds = Staged::default();
        for (points, color) in &scene.points {
            if !points.is_empty() {
                clouds.push_points(points, ObjectParams::stroke(*color));
            }
        }
        self.point_clouds.set(device, queue, clouds);

        self.write_scene_params(device, queue);
        self.generation = generation;

        // Depth cue (#361) and fog (#363): new bounds re-send the panes that
        // use the ramp.
        self.caster_bounds = bounds_of(
            scene
                .meshes
                .iter()
                .flat_map(|m| m.0.positions.iter())
                .chain(scene.face_meshes.iter().flat_map(|m| m.0.positions.iter())),
        );
        let bounds = scene_bounds(scene);
        if bounds != self.scene_bounds {
            self.scene_bounds = bounds;
            for i in 0..self.last_cameras.len() {
                let cam = self.last_cameras[i];
                if cam.uses_bounds() {
                    self.write_camera(device, queue, i, &cam);
                }
            }
        }

        self.underlay = scene
            .underlay
            .as_ref()
            .map(|u| self.build_underlay(device, queue, u));
        self.basemap = scene
            .basemap
            .as_ref()
            .map(|u| self.build_underlay(device, queue, u));
    }

    /// Lay the scene batches' parameters out back to back in the shared
    /// buffer (in paint order) and write it; a no-op when nothing changed.
    fn write_scene_params(&mut self, device: &wgpu::Device, queue: &wgpu::Queue) {
        let mut base = 0;
        for batch in self.scene_batches_mut() {
            batch.slot_base = base;
            base += batch.params.len() as u32;
        }
        let Self {
            meshes,
            face_meshes,
            edges,
            profile_ribbons,
            curve_segments,
            lines,
            point_clouds,
            dash_lines,
            dash_edges,
            scene_params,
            object_layout,
            ..
        } = self;
        let params = [
            &*meshes,
            &*edges,
            &*profile_ribbons,
            &*curve_segments,
            &*lines,
            &*point_clouds,
            &*face_meshes,
            &*dash_lines,
            &*dash_edges,
        ]
        .into_iter()
            .flat_map(|b| b.params.iter());
        scene_params.write(device, queue, object_layout, params);
    }

    fn scene_batches_mut(&mut self) -> [&mut Batch; 9] {
        [
            &mut self.meshes,
            &mut self.edges,
            &mut self.profile_ribbons,
            &mut self.curve_segments,
            &mut self.lines,
            &mut self.point_clouds,
            &mut self.face_meshes,
            &mut self.dash_lines,
            &mut self.dash_edges,
        ]
    }

    /// Replace the transient ghost preview lines (iOS gumball live wireframe).
    /// Each entry is `(segment-soup points, rgba, lineweight_mm)` where the
    /// points are flat LineList pairs in world space — the same shape the edge
    /// pass consumes. Lineweight is ignored (ghost is always a 1-pixel hairline).
    /// Rewrites one shared vertex buffer in place; call every drag frame.
    /// Additive — does not touch the scene.
    pub fn set_ghost_lines(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, lines: &[LineEntry]) {
        let mut ghost = Staged::default();
        for (points, color, _lw_mm) in lines {
            if points.len() >= 2 {
                ghost.push_segments(points, ObjectParams::stroke(*color));
            }
        }
        self.ghost_params.write(device, queue, &self.object_layout, ghost.params.iter());
        self.ghost_lines.set(device, queue, ghost);
    }

    /// Drop all ghost preview lines (gumball commit / cancel). Cheap no-op when
    /// already empty.
    pub fn clear_ghost_lines(&mut self) {
        self.ghost_lines.draws.clear();
    }

    /// Upload one underlay: an RGBA8 texture, a quad (two triangles) with uvs,
    /// and an opacity UBO, wired into one bind group.
    fn build_underlay(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        u: &UnderlayData,
    ) -> GpuUnderlay {
        use wgpu::util::DeviceExt as _;
        let size = wgpu::Extent3d {
            width: u.width_px.max(1),
            height: u.height_px.max(1),
            depth_or_array_layers: 1,
        };
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("underlay_tex"),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8UnormSrgb,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &u.rgba,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(4 * size.width),
                rows_per_image: Some(size.height),
            },
            size,
        );
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("underlay_sampler"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        let opacity_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("underlay_opacity"),
            contents: bytemuck::bytes_of(&[u.opacity, 0.0, 0.0, 0.0]),
            usage: wgpu::BufferUsages::UNIFORM,
        });
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("underlay_bg"),
            layout: &self.underlay_layout,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&view) },
                wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::Sampler(&sampler) },
                wgpu::BindGroupEntry { binding: 2, resource: opacity_buf.as_entire_binding() },
            ],
        });
        // Two triangles. uv v flipped so the image top maps to +y (north-up).
        let [c0, c1, c2, c3] = u.corners; // ll, lr, ur, ul
        let verts: [[f32; 5]; 4] = [
            [c0[0], c0[1], c0[2], 0.0, 1.0],
            [c1[0], c1[1], c1[2], 1.0, 1.0],
            [c2[0], c2[1], c2[2], 1.0, 0.0],
            [c3[0], c3[1], c3[2], 0.0, 0.0],
        ];
        let vertex_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("underlay_vb"),
            contents: bytemuck::cast_slice(&verts),
            usage: wgpu::BufferUsages::VERTEX,
        });
        let indices: [u16; 6] = [0, 1, 2, 0, 2, 3];
        let index_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("underlay_ib"),
            contents: bytemuck::cast_slice(&indices),
            usage: wgpu::BufferUsages::INDEX,
        });
        GpuUnderlay { vertex_buf, index_buf, bind_group }
    }

    /// `edges_enabled` lets the caller suppress the feature-edge pass (the
    /// Shaded "+ edges" default is on but user-toggleable). It has no effect on
    /// modes that require edges to be legible (wireframe/x-ray/ghosted/pencil).
    pub fn paint(
        &self,
        render_pass: &mut wgpu::RenderPass<'static>,
        viewport: usize,
        mode: DisplayMode,
        edges_enabled: bool,
    ) {
        let Some((_, camera_bind_group)) = self.cameras.get(viewport) else {
            return; // paint before any prepare for this pane — nothing to draw yet
        };
        render_pass.set_bind_group(0, camera_bind_group, &[]);

        // Basemap first (bottom of the ground-plane stack): satellite/OSM tiles
        // under any tracing underlay. Same textured-quad pipeline.
        if let Some(b) = &self.basemap {
            render_pass.set_pipeline(&self.pipes.underlay_pipeline);
            render_pass.set_bind_group(1, &b.bind_group, &[]);
            render_pass.set_vertex_buffer(0, b.vertex_buf.slice(..));
            render_pass.set_index_buffer(b.index_buf.slice(..), wgpu::IndexFormat::Uint16);
            render_pass.draw_indexed(0..6, 0, 0..1);
        }

        // Underlay next: on the ground plane, depth-written so meshes occlude
        // it and the grid blends over it.
        if let Some(u) = &self.underlay {
            render_pass.set_pipeline(&self.pipes.underlay_pipeline);
            render_pass.set_bind_group(1, &u.bind_group, &[]);
            render_pass.set_vertex_buffer(0, u.vertex_buf.slice(..));
            render_pass.set_index_buffer(u.index_buf.slice(..), wgpu::IndexFormat::Uint16);
            render_pass.draw_indexed(0..6, 0, 0..1);
        }

        let scene = &self.scene_params;
        if mode.draws_fill() {
            render_pass.set_pipeline(if mode.depth_writes() {
                &self.pipes.mesh_pipeline
            } else {
                &self.pipes.mesh_xray_pipeline
            });
            draw_indexed(render_pass, &self.meshes, scene);
            if !self.face_meshes.draws.is_empty() {
                render_pass.set_pipeline(if mode.depth_writes() {
                    &self.pipes.face_pipeline
                } else {
                    &self.pipes.face_xray_pipeline
                });
                draw_indexed(render_pass, &self.face_meshes, scene);
            }
        }

        // Shaded draws edges by default but can be turned off; other modes
        // always need them. Wireframe additionally has no fill, so edges must
        // never be suppressed there.
        let draw_edges = mode.draws_edges()
            && (edges_enabled || mode == DisplayMode::Wireframe);
        let fat = self.fat_lines.get(viewport).copied().unwrap_or(false);
        if draw_edges && fat {
            render_pass.set_pipeline(&self.pipes.fat_pipeline);
            draw_fat(render_pass, &self.edges, scene);
        } else if draw_edges {
            render_pass.set_pipeline(&self.pipes.edge_pipeline);
            draw_ranges(render_pass, &self.edges, scene);
        }
        if draw_edges {
            self.draw_dashed(render_pass, &self.dash_edges, fat);
        }

        // Profile / silhouette ribbons: thick outline meshes. Drawn with the
        // solid mesh pipeline (depth-writing) so they read as fat ink edges on
        // top of the fill. Present only when the snapshot populated them.
        if !self.profile_ribbons.draws.is_empty() {
            render_pass.set_pipeline(&self.pipes.mesh_pipeline);
            draw_indexed(render_pass, &self.profile_ribbons, scene);
        }

        // Default curves: the crisp 1-device-pixel hardware hairline is the
        // Rhino wire (constant on-screen width, selection shown by colour).
        // Fat-stroke modes widen them like the edges above.
        if fat {
            render_pass.set_pipeline(&self.pipes.fat_pipeline);
            draw_fat(render_pass, &self.curve_segments, scene);
        } else {
            render_pass.set_pipeline(&self.pipes.curve_pipeline);
            draw_ranges(render_pass, &self.lines, scene);
        }
        self.draw_dashed(render_pass, &self.dash_lines, fat);

        // Point clouds: rendered as PointList after lines so they appear on top
        // of surfaces but below the grid.
        render_pass.set_pipeline(&self.pipes.point_pipeline);
        draw_ranges(render_pass, &self.point_clouds, scene);

        // Ghost preview lines (iOS gumball live wireframe): hairline segments
        // drawn with the LineList edge pipeline (the ghost is a flat segment
        // soup, not a strip). Over the scene, under the grid. Present only while
        // a gumball drag is active; empty otherwise.
        if !self.ghost_lines.draws.is_empty() {
            render_pass.set_pipeline(&self.pipes.edge_pipeline);
            draw_ranges(render_pass, &self.ghost_lines, &self.ghost_params);
        }

        // Section fill (#359): after every stroke, which the caps (pulled
        // toward the eye past them) cover where the cut opens a solid. Only
        // the opaque modes, whose faces write depth.
        let caps = mode.draws_fill()
            && mode.depth_writes()
            && self.last_cameras.get(viewport).is_some_and(CameraUniform::draws_section_fill);
        if caps {
            render_pass.set_pipeline(&self.pipes.cap_pipeline);
            draw_indexed(render_pass, &self.meshes, scene);
            if !self.face_meshes.draws.is_empty() {
                render_pass.set_pipeline(&self.pipes.cap_face_pipeline);
                draw_indexed(render_pass, &self.face_meshes, scene);
            }
        }

        // Back edges (#361): after every occluder, before the grid, only in
        // the opaque modes (never X-ray, Ghosted or Wireframe, whose faces
        // write no depth) and only for the panes that ask.
        let back = draw_edges
            && mode.depth_writes()
            && self.last_cameras.get(viewport).is_some_and(|c| {
                c.draws_back_edges() && c.screen[0] > 0.0 && c.screen[1] > 0.0
            });
        if back {
            render_pass.set_pipeline(&self.pipes.back_pipeline);
            draw_fat(render_pass, &self.edges, scene);
            draw_fat(render_pass, &self.curve_segments, scene);
        }

        // Grid last: blends over background, depth-tested against meshes.
        // A hidden grid (`grid off`, camera `light.z`) is dropped in the
        // shader, which still paints the gradient background (#364).
        render_pass.set_pipeline(&self.pipes.grid_pipeline);
        render_pass.draw(0..3, 0..1);

        // World axes: X/Y are the z=0 datum lines (incl. the horizontal datum
        // in Front/Back elevations), Z the vertical through the origin; real
        // line geometry so they stay straight at any zoom. Drawn after the
        // grid (whose gradient mode is opaque) with the depth-nudged axis and
        // dash pipelines so the coplanar grid plane does not stipple over
        // them; still depth-tested, so meshes occlude them. `axes off`
        // (camera `light.w`) hides them for the pane (#364).
        if !self.axes_hidden.get(viewport).copied().unwrap_or(false) {
            render_pass.set_pipeline(&self.pipes.axis_pipeline);
            draw_ranges(render_pass, &self.axis_lines, &self.axis_params);
            render_pass.set_pipeline(&self.pipes.dash_pipeline);
            draw_ranges(render_pass, &self.axis_dots, &self.axis_params);
        }
    }

    /// Draw a dashed batch as hairlines, or as fat strokes when the pane's
    /// camera asks for them (#364).
    fn draw_dashed(&self, pass: &mut wgpu::RenderPass<'static>, batch: &Batch, fat: bool) {
        if batch.draws.is_empty() {
            return;
        }
        if fat {
            pass.set_pipeline(&self.pipes.fat_dash_pipeline);
            draw_fat(pass, batch, &self.scene_params);
        } else {
            pass.set_pipeline(&self.pipes.dash_pipeline);
            draw_ranges(pass, batch, &self.scene_params);
        }
    }
}

/// World bounds of a scene's meshes, strokes and points, `None` when empty.
fn scene_bounds(scene: &SceneData) -> Option<(glam::Vec3, glam::Vec3)> {
    bounds_of(
        scene
            .meshes
            .iter()
            .flat_map(|m| m.0.positions.iter())
            .chain(scene.face_meshes.iter().flat_map(|m| m.0.positions.iter()))
            .chain(scene.edges.iter().flat_map(|e| e.0.iter()))
            .chain(scene.lines.iter().flat_map(|l| l.0.iter()))
            .chain(scene.points.iter().flat_map(|p| p.0.iter())),
    )
}

/// World bounds of `points`, `None` when empty.
fn bounds_of<'a>(points: impl Iterator<Item = &'a [f32; 3]>) -> Option<(glam::Vec3, glam::Vec3)> {
    points.map(|p| glam::Vec3::from(*p)).fold(None, |acc, p| match acc {
        None => Some((p, p)),
        Some((lo, hi)) => Some((lo.min(p), hi.max(p))),
    })
}

/// The depth ramp of a camera over the scene bounds (depth cue #361, fog
/// #363): a plane
/// whose `dot(p, xyz) + w` is 0 at the bounds' nearest corner (or the eye,
/// if nearer) and 1 at the farthest, measured along the view direction so
/// parallel views ramp too. Zero when the camera has neither.
fn depth_cue_ramp(cam: &CameraUniform, bounds: Option<(glam::Vec3, glam::Vec3)>) -> [f32; 4] {
    let Some((lo, hi)) = bounds.filter(|_| cam.uses_ramp()) else {
        return [0.0; 4];
    };
    // View direction from two unprojected points on the screen centre.
    let inv = glam::Mat4::from_cols_array_2d(&cam.inv_view_proj);
    let fwd = (inv.project_point3(glam::Vec3::new(0.0, 0.0, 0.5))
        - inv.project_point3(glam::Vec3::ZERO))
    .normalize_or_zero();
    if fwd == glam::Vec3::ZERO || !fwd.is_finite() {
        return [0.0; 4];
    }
    let corners = (0..8).map(|i| {
        glam::Vec3::new(
            if i & 1 == 0 { lo.x } else { hi.x },
            if i & 2 == 0 { lo.y } else { hi.y },
            if i & 4 == 0 { lo.z } else { hi.z },
        )
        .dot(fwd)
    });
    let (near, far) = corners.fold((f32::MAX, f32::MIN), |(a, b), d| (a.min(d), b.max(d)));
    let eye = glam::Vec3::new(cam.eye[0], cam.eye[1], cam.eye[2]).dot(fwd);
    let near = near.max(eye.min(far));
    let range = (far - near).max(1e-3);
    [fwd.x / range, fwd.y / range, fwd.z / range, -near / range]
}

/// Pack scene meshes into one indexed batch.
fn stage_meshes(meshes: &[(RenderMesh, [f32; 4], [f32; 2])]) -> Staged {
    let mut s = Staged::default();
    for (mesh, color, rm) in meshes {
        let params = ObjectParams::surface(*color, [rm[0], rm[1], 0.0, 0.0]);
        s.push_mesh(&mesh.positions, &mesh.normals, &mesh.indices, params);
    }
    s
}

/// Draw every object of an indexed batch, one draw per object (paint order is
/// object order, as before batching, so blending is unchanged).
fn draw_indexed(pass: &mut wgpu::RenderPass<'static>, batch: &Batch, params: &ParamBuffer) {
    let (Some(vb), Some(ib), Some(bg)) = (batch.vertex_slice(), batch.index_slice(), params.bind_group())
    else {
        return;
    };
    pass.set_vertex_buffer(0, vb);
    pass.set_index_buffer(ib, wgpu::IndexFormat::Uint32);
    for d in &batch.draws {
        pass.set_bind_group(1, bg, &[params.offset(batch.slot_base + d.slot)]);
        pass.draw_indexed(d.range(), d.base_vertex, 0..1);
    }
}

/// Draw every object of a line or point batch as a vertex range.
fn draw_ranges(pass: &mut wgpu::RenderPass<'static>, batch: &Batch, params: &ParamBuffer) {
    let (Some(vb), Some(bg)) = (batch.vertex_slice(), params.bind_group()) else {
        return;
    };
    pass.set_vertex_buffer(0, vb);
    for d in &batch.draws {
        pass.set_bind_group(1, bg, &[params.offset(batch.slot_base + d.slot)]);
        pass.draw(d.range(), 0..1);
    }
}

/// Draw a line-list batch through the fat-stroke pipeline: six quad vertices
/// per segment, segments read as instances.
fn draw_fat(pass: &mut wgpu::RenderPass<'static>, batch: &Batch, params: &ParamBuffer) {
    let (Some(vb), Some(bg)) = (batch.vertex_slice(), params.bind_group()) else {
        return;
    };
    pass.set_vertex_buffer(0, vb);
    for d in &batch.draws {
        pass.set_bind_group(1, bg, &[params.offset(batch.slot_base + d.slot)]);
        pass.draw(0..6, d.segments());
    }
}

#[cfg(test)]
mod tests {
    use super::{
        curve_px, dashed_soup, dashed_strip, linetype_dash, world_axis_lines, DisplayMode,
        LightMode, LineType, AXIS_EXTENT, AXIS_RGBA, RHINO_CURVE_PX,
    };

    #[test]
    fn world_axis_lines_geometry() {
        let axes = world_axis_lines();
        assert_eq!(axes.len(), 6);
        assert!(AXIS_EXTENT.is_finite() && AXIS_EXTENT > 0.0);
        for (axis, color) in AXIS_RGBA.iter().enumerate() {
            // Red X, green Y, blue Z: that channel dominates, fully opaque.
            assert!((0..3).all(|c| c == axis || color[axis] > color[c]), "{axis}: {color:?}");
            assert_eq!(color[3], 1.0);
            let halves: Vec<_> = axes.iter().filter(|a| a.color == *color).collect();
            assert_eq!(halves.len(), 2, "axis {axis}: a positive and a negative half");
            for a in halves {
                // From the origin outward along this axis only, f32-exact.
                assert_eq!(a.points[0], [0.0; 3]);
                let end = a.points[1][axis];
                assert_eq!(end.abs(), AXIS_EXTENT);
                assert!((0..3).all(|c| c == axis || a.points[1][c] == 0.0));
                // SketchUp: solid positive half, dotted negative half.
                assert_eq!(a.dotted, end < 0.0);
            }
        }
        // The blue Z axis rises from the origin (#364).
        assert!(axes.iter().any(|a| a.points[1] == [0.0, 0.0, AXIS_EXTENT] && !a.dotted));
    }

    #[test]
    fn camera_overlays_toggle_grid_and_axes() {
        let cam = crate::camera_uniform(glam::Mat4::IDENTITY, glam::Vec3::ZERO);
        assert!(cam.shows_grid() && cam.shows_axes(), "shown by default");
        let off = cam.with_overlays(false, true);
        assert!(!off.shows_grid() && off.shows_axes());
        assert_eq!(off.light[2], 1.0, "grid.wgsl reads light.z");
        let off = cam.with_overlays(true, false);
        assert!(off.shows_grid() && !off.shows_axes());
        assert_eq!(off.light[3], 1.0);
        assert_eq!(off.light[..2], cam.light[..2], "lighting flags untouched");
    }

    #[test]
    fn dashed_strokes_carry_distance_along() {
        // A strip runs its distance on across segments.
        let strip = dashed_strip(&[[0.0; 3], [3.0, 4.0, 0.0], [3.0, 4.0, 2.0]]);
        let s: Vec<f32> = strip.iter().map(|v| v[3]).collect();
        assert_eq!(s, [0.0, 5.0, 5.0, 7.0]);
        assert_eq!(strip[2][..3], [3.0, 4.0, 0.0]);
        // A soup restarts every segment; an odd trailing point is dropped.
        let soup = dashed_soup(&[[0.0; 3], [2.0, 0.0, 0.0], [5.0, 0.0, 0.0], [5.0, 1.0, 0.0], [9.0; 3]]);
        let s: Vec<f32> = soup.iter().map(|v| v[3]).collect();
        assert_eq!(s, [0.0, 2.0, 0.0, 1.0]);
    }

    #[test]
    fn linetypes_map_to_dash_patterns() {
        assert_eq!(linetype_dash(LineType::Continuous), None);
        let mut seen = Vec::new();
        for lt in [LineType::Dashed, LineType::Dotted, LineType::DashDot] {
            let d = linetype_dash(lt).expect("a pattern");
            assert!(d.iter().all(|v| *v > 0.0), "{lt:?}: every dash and gap visible");
            assert!(!seen.contains(&d), "{lt:?}: distinct pattern");
            seen.push(d);
        }
        // Dash-dot: a long dash then a short dot.
        let dd = linetype_dash(LineType::DashDot).unwrap();
        assert!(dd[0] > dd[2]);
    }

    #[test]
    fn curve_px_scales_with_dpr() {
        // Rhino default is 1 logical px; physical thickness stays constant
        // across DPI, so 1x → 1 px and 2x → 2 px.
        assert_eq!(RHINO_CURVE_PX, 1.0);
        assert_eq!(curve_px(1.0), 1.0);
        assert_eq!(curve_px(2.0), 2.0);
        assert_eq!(curve_px(1.5), 1.5);
        // Never sub-pixel: a bogus <1 dpr floors to a visible hairline.
        assert_eq!(curve_px(0.0), 1.0);
        assert_eq!(curve_px(0.5), 1.0);
    }

    #[test]
    fn light_mode_default_is_working() {
        assert_eq!(LightMode::default(), LightMode::Working);
    }

    #[test]
    fn light_mode_parse_round_trip() {
        for (s, expected) in [
            ("working", LightMode::Working),
            ("work", LightMode::Working),
            ("sun", LightMode::Sun),
            ("solar", LightMode::Sun),
            ("presentation", LightMode::Presentation),
            ("present", LightMode::Presentation),
            ("pbr", LightMode::Presentation),
        ] {
            assert_eq!(LightMode::parse(s), Some(expected), "parse({s:?})");
        }
        assert_eq!(LightMode::parse("nope"), None);
    }

    #[test]
    fn light_mode_labels_parse_back() {
        for m in LightMode::ALL {
            // label lowercased must parse back to the same mode.
            assert_eq!(LightMode::parse(&m.label().to_lowercase()), Some(m));
        }
    }

    #[test]
    fn light_mode_shader_flags_distinct() {
        let flags: Vec<f32> = LightMode::ALL.iter().map(|m| m.shader_flag()).collect();
        assert_eq!(flags, vec![0.0, 1.0, 2.0]);
    }

    // Mirror of the mesh.wgsl hemispheric fill so the ambient-floor invariant is
    // checked in pure Rust on every `cargo test` (the shader itself only runs on
    // a GPU). If you change the constants in mesh.wgsl, change them here too.
    fn hemispheric_ambient(nz: f32) -> f32 {
        let up = (nz * 0.5 + 0.5).clamp(0.0, 1.0);
        0.30 + (0.65 - 0.30) * up // mix(0.30, 0.65, up)
    }
    fn diffuse_floor(nz: f32, key: f32) -> f32 {
        // diffuse = ambient + 0.55 * max(dot(n, key_dir), 0)
        hemispheric_ambient(nz) + 0.55 * key.max(0.0)
    }

    #[test]
    fn ambient_floor_keeps_faces_off_black() {
        // A face pointing straight DOWN (nz=-1) with ZERO key contribution still
        // gets the ground-bounce ambient — luminance must be strictly positive.
        let lum = diffuse_floor(-1.0, 0.0);
        assert!(lum > 0.0, "downward face fully unlit must not be black: {lum}");
        assert!((lum - 0.30).abs() < 1e-6, "ground floor is 0.30, got {lum}");

        // A face turned fully AWAY from the key light (negative dot) still reads
        // by its hemispheric ambient — no crush to black at any orientation.
        for nz in [-1.0_f32, -0.5, 0.0, 0.5, 1.0] {
            assert!(diffuse_floor(nz, -1.0) > 0.0, "nz={nz} away-from-key went black");
        }
    }

    #[test]
    fn pencil_parse_round_trip() {
        assert_eq!(DisplayMode::parse("pencil"), Some(DisplayMode::Pencil));
        assert_eq!(DisplayMode::Pencil.label(), "Pencil");
    }

    #[test]
    fn pencil_in_all() {
        assert!(DisplayMode::ALL.contains(&DisplayMode::Pencil));
    }

    #[test]
    fn pencil_fill_alpha_is_negative_sentinel() {
        assert!(DisplayMode::Pencil.fill_alpha() < 0.0);
        // Only the paper modes use the sentinel; the rest are non-negative.
        for mode in DisplayMode::ALL {
            if mode.fill_alpha() < 0.0 {
                assert!(
                    mode.background_override().is_some(),
                    "{mode:?} is a paper mode"
                );
            } else {
                assert!(
                    mode.fill_alpha() >= 0.0,
                    "{mode:?} should have non-negative alpha"
                );
            }
        }
    }

    #[test]
    fn pencil_draws_fill_and_edges_with_depth_writes() {
        assert!(DisplayMode::Pencil.draws_fill());
        assert!(DisplayMode::Pencil.draws_edges());
        assert!(DisplayMode::Pencil.depth_writes());
    }

    #[test]
    fn shaded_draws_feature_edges_by_default() {
        // Shaded now draws mesh feature edges (the SketchUp/Rhino "shaded +
        // edges" look), matching every other mode.
        assert!(DisplayMode::Shaded.draws_edges());
        assert!(DisplayMode::Shaded.draws_fill());
        for mode in DisplayMode::ALL {
            assert!(mode.draws_edges(), "{mode:?} should draw feature edges");
        }
    }

    #[test]
    fn mode_background_ink_and_width_mapping() {
        // Hidden Line follows the theme: no paper, depth-only fill (alpha 0,
        // not the pencil ink sentinel), occlusion on, a little stroke weight.
        let hl = DisplayMode::HiddenLine;
        assert_eq!(hl.background_override(), None);
        assert_eq!(hl.fill_alpha(), 0.0);
        assert!(hl.draws_fill() && hl.depth_writes() && !hl.forces_sketchy());
        // Sketchy: paper, dark ink (negative sentinel), occlusion, sketchy on.
        let sk = DisplayMode::Sketchy;
        assert_eq!(
            sk.background_override(),
            Some(DisplayMode::pencil_background())
        );
        assert!(sk.fill_alpha() < 0.0);
        assert!(sk.depth_writes() && sk.forces_sketchy());
        assert_eq!(
            DisplayMode::Pencil.background_override(),
            Some(DisplayMode::pencil_background())
        );
        // Widths: hairline elsewhere, Hidden Line < Sketchy, scaled by dpr.
        assert_eq!(DisplayMode::Shaded.line_px(3.0), 0.0);
        // Hairline = 1 device px; Hidden Line beats it on any retina screen.
        assert!(hl.line_px(2.0) > 1.0 && sk.line_pt() > hl.line_pt());
        assert_eq!(hl.line_px(2.0), hl.line_pt() * 2.0);
        assert_eq!(sk.line_px(3.0), sk.line_pt() * 3.0);
        assert_eq!(sk.line_px(0.0), sk.line_pt());
        for mode in DisplayMode::ALL {
            assert_eq!(
                DisplayMode::parse(&mode.label().to_lowercase().replace(' ', "")),
                Some(mode)
            );
        }
    }

    #[test]
    fn pencil_background_is_near_white() {
        let bg = DisplayMode::pencil_background();
        // All channels > 0.9 (near white)
        assert!(bg[0] > 0.9 && bg[1] > 0.9 && bg[2] > 0.9);
        assert_eq!(bg[3], 1.0);
    }

    #[test]
    fn all_modes_parse() {
        for (s, expected) in [
            ("shaded", DisplayMode::Shaded),
            ("wireframe", DisplayMode::Wireframe),
            ("wire", DisplayMode::Wireframe),
            ("xray", DisplayMode::XRay),
            ("x-ray", DisplayMode::XRay),
            ("ghosted", DisplayMode::Ghosted),
            ("ghost", DisplayMode::Ghosted),
            ("pencil", DisplayMode::Pencil),
            ("hiddenline", DisplayMode::HiddenLine),
            ("hidden-line", DisplayMode::HiddenLine),
            ("sketchy", DisplayMode::Sketchy),
        ] {
            assert_eq!(DisplayMode::parse(s), Some(expected), "parse({s:?}) failed");
        }
        assert_eq!(DisplayMode::parse("unknown"), None);
    }
}
