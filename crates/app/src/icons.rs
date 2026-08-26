// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart

//! Lucide line-icon set for the primary chrome (menu bar, tab strip, toolbars).
//!
//! We vendor a small set of [Lucide](https://lucide.dev) SVGs (ISC-licensed,
//! FOSS-redistributable — unlike Apple's SF Symbols, which are Apple-platform-
//! only and cannot ship in an AGPL project). Each icon is pre-rasterized to a
//! 48×48 **white-on-transparent** PNG (`assets/icons/png/*.png`) and embedded
//! here with `include_bytes!`. White so we can `tint` a single raster to the
//! active theme's foreground color at draw time, one image for both skins.
//!
//! [`Icons`] owns a per-name [`egui::TextureHandle`] cache (interior-mutable, so
//! draw sites take `&self`). The pure parts — the [`Icon`] enum, its stable name
//! and byte-slice mapping — carry no egui state and are unit-tested standalone.

use std::cell::RefCell;
use std::collections::HashMap;

/// A vendored Lucide icon. Variants map 1:1 to `assets/icons/png/<name>.png`.
/// Named by *role* in our UI, not always by Lucide's filename, so call sites
/// read semantically (e.g. [`Icon::New`] → Lucide `file-plus`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Icon {
    // File
    New,
    NewSession,
    Open,
    Save,
    Import,
    Export,
    Print,
    Download,
    // Edit
    Undo,
    Redo,
    History,
    // Tools / Help / appearance
    Model,
    Help,
    About,
    Theme,
    // Category marks
    EditCat,
    View,
    Curve,
    Solid,
    Boolean,
    Transform,
    Annotate,
    Dimension,
    Analyze,
    Structure,
    ToolsCat,
    // Specific verb marks
    Line,
    Rect,
    CircleShape,
    BoxShape,
    Move,
    Copy,
    Rotate,
    Scale,
    Mirror,
    // Panel tabs
    Layers,
    Properties,
    Chat,
    // Layers panel controls
    Lightbulb,
    Lock,
    LockOpen,
    Plus,
    Minus,
    Settings,
    CircleDot,
    // Deck / chrome controls
    Sessions,
    Run,
    Stop,
    Skip,
    Close,
    Image,
    // Panel + viewport controls
    PanelOpen,
    PanelClose,
    Maximize,
    Layout1,
    Layout2,
    Layout4,
}

impl Icon {
    /// Every icon, for exhaustiveness (embedding + tests). Part of the module's
    /// public surface; exercised by the raster-validation tests.
    #[allow(dead_code)]
    pub const ALL: [Icon; 57] = [
        Icon::New,
        Icon::NewSession,
        Icon::Open,
        Icon::Save,
        Icon::Import,
        Icon::Export,
        Icon::Print,
        Icon::Download,
        Icon::Undo,
        Icon::Redo,
        Icon::History,
        Icon::Model,
        Icon::Help,
        Icon::About,
        Icon::Theme,
        Icon::EditCat,
        Icon::View,
        Icon::Curve,
        Icon::Solid,
        Icon::Boolean,
        Icon::Transform,
        Icon::Annotate,
        Icon::Dimension,
        Icon::Analyze,
        Icon::Structure,
        Icon::ToolsCat,
        Icon::Line,
        Icon::Rect,
        Icon::CircleShape,
        Icon::BoxShape,
        Icon::Move,
        Icon::Copy,
        Icon::Rotate,
        Icon::Scale,
        Icon::Mirror,
        Icon::Layers,
        Icon::Properties,
        Icon::Chat,
        Icon::Lightbulb,
        Icon::Lock,
        Icon::LockOpen,
        Icon::Plus,
        Icon::Minus,
        Icon::Settings,
        Icon::CircleDot,
        Icon::Sessions,
        Icon::Run,
        Icon::Stop,
        Icon::Skip,
        Icon::Close,
        Icon::Image,
        Icon::PanelOpen,
        Icon::PanelClose,
        Icon::Maximize,
        Icon::Layout1,
        Icon::Layout2,
        Icon::Layout4,
    ];

    /// Stable cache key / Lucide source name.
    pub fn name(self) -> &'static str {
        match self {
            Icon::New => "file-plus",
            Icon::NewSession => "copy-plus",
            Icon::Open => "folder-open",
            Icon::Save => "save",
            Icon::Import => "import",
            Icon::Export => "upload",
            Icon::Print => "printer",
            Icon::Download => "download",
            Icon::Undo => "undo-2",
            Icon::Redo => "redo-2",
            Icon::History => "clock",
            Icon::Model => "bot",
            Icon::Help => "circle-question-mark",
            Icon::About => "info",
            Icon::Theme => "sun-moon",
            Icon::EditCat => "pencil",
            Icon::View => "eye",
            Icon::Curve => "spline",
            Icon::Solid => "package",
            Icon::Boolean => "combine",
            Icon::Transform => "axis-3d",
            Icon::Annotate => "type",
            Icon::Dimension => "ruler",
            Icon::Analyze => "search",
            Icon::Structure => "building-2",
            Icon::ToolsCat => "wrench",
            Icon::Line => "pen-line",
            Icon::Rect => "square",
            Icon::CircleShape => "circle",
            Icon::BoxShape => "box",
            Icon::Move => "move",
            Icon::Copy => "copy",
            Icon::Rotate => "rotate-cw",
            Icon::Scale => "scaling",
            Icon::Mirror => "flip-horizontal-2",
            Icon::Layers => "layers",
            Icon::Properties => "sliders-horizontal",
            Icon::Chat => "message-square",
            Icon::Lightbulb => "lightbulb",
            Icon::Lock => "lock",
            Icon::LockOpen => "lock-open",
            Icon::Plus => "plus",
            Icon::Minus => "minus",
            Icon::Settings => "settings",
            Icon::CircleDot => "circle-dot",
            Icon::Sessions => "messages-square",
            Icon::Run => "play",
            Icon::Stop => "circle-stop",
            Icon::Skip => "skip-forward",
            Icon::Close => "x",
            Icon::Image => "image",
            Icon::PanelOpen => "panel-right-open",
            Icon::PanelClose => "panel-right-close",
            Icon::Maximize => "maximize-2",
            Icon::Layout1 => "square-dashed",
            Icon::Layout2 => "columns-2",
            Icon::Layout4 => "grid-2x2",
        }
    }

    /// The embedded white-on-transparent PNG bytes for this icon.
    fn png_bytes(self) -> &'static [u8] {
        macro_rules! png {
            ($n:literal) => {
                include_bytes!(concat!("../../../assets/icons/png/", $n, ".png"))
            };
        }
        match self {
            Icon::New => png!("file-plus"),
            Icon::NewSession => png!("copy-plus"),
            Icon::Open => png!("folder-open"),
            Icon::Save => png!("save"),
            Icon::Import => png!("import"),
            Icon::Export => png!("upload"),
            Icon::Print => png!("printer"),
            Icon::Download => png!("download"),
            Icon::Undo => png!("undo-2"),
            Icon::Redo => png!("redo-2"),
            Icon::History => png!("clock"),
            Icon::Model => png!("bot"),
            Icon::Help => png!("circle-question-mark"),
            Icon::About => png!("info"),
            Icon::Theme => png!("sun-moon"),
            Icon::EditCat => png!("pencil"),
            Icon::View => png!("eye"),
            Icon::Curve => png!("spline"),
            Icon::Solid => png!("package"),
            Icon::Boolean => png!("combine"),
            Icon::Transform => png!("axis-3d"),
            Icon::Annotate => png!("type"),
            Icon::Dimension => png!("ruler"),
            Icon::Analyze => png!("search"),
            Icon::Structure => png!("building-2"),
            Icon::ToolsCat => png!("wrench"),
            Icon::Line => png!("pen-line"),
            Icon::Rect => png!("square"),
            Icon::CircleShape => png!("circle"),
            Icon::BoxShape => png!("box"),
            Icon::Move => png!("move"),
            Icon::Copy => png!("copy"),
            Icon::Rotate => png!("rotate-cw"),
            Icon::Scale => png!("scaling"),
            Icon::Mirror => png!("flip-horizontal-2"),
            Icon::Layers => png!("layers"),
            Icon::Properties => png!("sliders-horizontal"),
            Icon::Chat => png!("message-square"),
            Icon::Lightbulb => png!("lightbulb"),
            Icon::Lock => png!("lock"),
            Icon::LockOpen => png!("lock-open"),
            Icon::Plus => png!("plus"),
            Icon::Minus => png!("minus"),
            Icon::Settings => png!("settings"),
            Icon::CircleDot => png!("circle-dot"),
            Icon::Sessions => png!("messages-square"),
            Icon::Run => png!("play"),
            Icon::Stop => png!("circle-stop"),
            Icon::Skip => png!("skip-forward"),
            Icon::Close => png!("x"),
            Icon::Image => png!("image"),
            Icon::PanelOpen => png!("panel-right-open"),
            Icon::PanelClose => png!("panel-right-close"),
            Icon::Maximize => png!("maximize-2"),
            Icon::Layout1 => png!("square-dashed"),
            Icon::Layout2 => png!("columns-2"),
            Icon::Layout4 => png!("grid-2x2"),
        }
    }
}

/// Runtime icon cache: decodes + uploads each PNG once, keyed by [`Icon::name`].
/// Interior-mutable so widget code can draw through a shared `&Icons`.
#[derive(Default)]
pub struct Icons {
    cache: RefCell<HashMap<&'static str, egui::TextureHandle>>,
}

impl Icons {
    pub fn new() -> Self {
        Self::default()
    }

    /// Fetch (decoding + uploading on first use) the texture for `icon`.
    fn texture(&self, ctx: &egui::Context, icon: Icon) -> egui::TextureHandle {
        let key = icon.name();
        if let Some(h) = self.cache.borrow().get(key) {
            return h.clone();
        }
        let img = decode_png(icon.png_bytes());
        let handle = ctx.load_texture(format!("lucide:{key}"), img, egui::TextureOptions::LINEAR);
        self.cache.borrow_mut().insert(key, handle.clone());
        handle
    }

    /// The raw [`egui::TextureId`] for `icon` (decoding/uploading on first use).
    /// For call sites that must paint into a `'static` closure (e.g. a
    /// [`egui::CollapsingHeader`] disclosure-icon slot) where the `&Icons` borrow
    /// cannot be captured.
    pub fn texture_id(&self, ctx: &egui::Context, icon: Icon) -> egui::TextureId {
        self.texture(ctx, icon).id()
    }

    /// An [`egui::Image`] for `icon`, sized to `size` logical px and tinted to
    /// `color` (the raster is white, so tint recolors it wholesale).
    pub fn image(
        &self,
        ctx: &egui::Context,
        icon: Icon,
        size: f32,
        color: egui::Color32,
    ) -> egui::Image<'static> {
        let tex = self.texture(ctx, icon);
        egui::Image::new(&tex)
            .fit_to_exact_size(egui::vec2(size, size))
            .tint(color)
    }

    /// An icon-only clickable button, tinted to the current foreground and sized
    /// to the adjacent Body text height (so the mark tracks the text-style height
    /// everywhere). `hover` is the mandatory tooltip / accessibility label — every
    /// icon-only control must name its action for screen readers and mouse users.
    pub fn icon_button(
        &self,
        ui: &mut egui::Ui,
        icon: Icon,
        hover: &str,
    ) -> egui::Response {
        let size = ui.text_style_height(&egui::TextStyle::Body);
        let color = ui.visuals().text_color();
        let img = self.image(ui.ctx(), icon, size, color);
        ui.add(egui::Button::image(img).frame(true))
            .on_hover_text(hover)
    }

    /// Draw a menu row `<icon>  <label>`, tinting the icon to the current
    /// foreground so it tracks light/dark. Returns the row's click response.
    /// Mirrors the plain-text `item` layout the menus used before.
    #[allow(dead_code)] // convenience wrapper kept for callers/tests
    pub fn menu_item(&self, ui: &mut egui::Ui, icon: Icon, label: &str) -> egui::Response {
        self.menu_item_ex(ui, icon, label, None, true)
    }

    /// A menu row with an optional right-aligned keyboard `shortcut` (e.g.
    /// `"Cmd+S"`) and an `enabled` flag. When disabled the whole row is dimmed
    /// and does not respond to clicks — disable-don't-hide, so a selection-
    /// dependent verb still teaches its capability. The shortcut is drawn weak so
    /// it reads as an affordance hint, not a second label.
    pub fn menu_item_ex(
        &self,
        ui: &mut egui::Ui,
        icon: Icon,
        label: &str,
        shortcut: Option<&str>,
        enabled: bool,
    ) -> egui::Response {
        let size = ui.text_style_height(&egui::TextStyle::Body);
        let color = if enabled {
            ui.visuals().text_color()
        } else {
            ui.visuals().weak_text_color()
        };
        let img = self.image(ui.ctx(), icon, size, color);
        ui.add_enabled_ui(enabled, |ui| {
            ui.scope(|ui| {
                ui.spacing_mut().item_spacing.x = ui.spacing().item_spacing.x.max(8.0);
                ui.horizontal(|ui| {
                    ui.add(img);
                    let resp = ui.label(label);
                    if let Some(sc) = shortcut {
                        // Push the shortcut to the right edge, drawn weak/small.
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.label(egui::RichText::new(sc).weak().small());
                        });
                    }
                    resp
                })
                .inner
            })
            .inner
        })
        .inner
    }
}

/// Decode a white-on-transparent PNG into an [`egui::ColorImage`].
fn decode_png(bytes: &[u8]) -> egui::ColorImage {
    let dyn_img = image::load_from_memory(bytes).expect("embedded icon PNG decodes");
    let rgba = dyn_img.to_rgba8();
    let (w, h) = rgba.dimensions();
    egui::ColorImage::from_rgba_unmultiplied([w as usize, h as usize], rgba.as_raw())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_icon_has_a_nonempty_name() {
        for ic in Icon::ALL {
            assert!(!ic.name().is_empty(), "{ic:?} has empty name");
        }
    }

    #[test]
    fn every_icon_embeds_decodable_png() {
        // Each embedded raster must decode, be square, and be non-degenerate.
        for ic in Icon::ALL {
            let img = decode_png(ic.png_bytes());
            assert_eq!(img.width(), img.height(), "{ic:?} not square");
            assert!(img.width() >= 24, "{ic:?} too small: {}", img.width());
        }
    }

    #[test]
    fn rasters_are_white_on_transparent() {
        // Fully-opaque pixels are pure white (so `tint` fully recolors the mark);
        // anti-aliased edge pixels carry lower alpha and may premultiply toward
        // gray, which is fine — tint scales them too. At least one solid-white
        // pixel exists (the icon isn't blank).
        for ic in Icon::ALL {
            let img = decode_png(ic.png_bytes());
            let mut solid_white = 0usize;
            for px in img.pixels.iter() {
                if px.a() == 255 {
                    assert_eq!(
                        (px.r(), px.g(), px.b()),
                        (255, 255, 255),
                        "{ic:?} has a non-white fully-opaque pixel"
                    );
                    solid_white += 1;
                }
            }
            assert!(solid_white > 0, "{ic:?} raster has no solid stroke");
        }
    }

    #[test]
    fn no_two_actions_share_a_glyph() {
        // Design invariant (Batch B): every distinct action/role maps to a
        // *distinct* Lucide glyph, so nothing reads ambiguously in the chrome.
        // Regression guard against the old overloads (Copy/NewSession=copy-plus,
        // Solid/BoxShape=box, Transform/Move=move).
        let mut by_glyph: std::collections::HashMap<&str, Icon> = std::collections::HashMap::new();
        for ic in Icon::ALL {
            if let Some(prev) = by_glyph.insert(ic.name(), ic) {
                panic!(
                    "glyph {:?} shared by {:?} and {:?}",
                    ic.name(),
                    prev,
                    ic
                );
            }
        }
        assert_eq!(by_glyph.len(), Icon::ALL.len());
    }

    #[test]
    fn deduped_overloads_are_now_distinct() {
        // The specific pairs the report called out no longer collide.
        assert_ne!(Icon::Copy.name(), Icon::NewSession.name());
        assert_ne!(Icon::Solid.name(), Icon::BoxShape.name());
        assert_ne!(Icon::Transform.name(), Icon::Move.name());
        assert_ne!(Icon::Import.name(), Icon::Download.name());
        // Line must not be a slash (reads as "cancel").
        assert_eq!(Icon::Line.name(), "pen-line");
        assert_ne!(Icon::Line.name(), "slash");
    }

    #[test]
    fn all_array_covers_every_variant_uniquely() {
        // ALL has no duplicates and its length matches the declared size.
        let mut seen = std::collections::HashSet::new();
        for ic in Icon::ALL {
            assert!(seen.insert(ic), "{ic:?} duplicated in ALL");
        }
        assert_eq!(seen.len(), Icon::ALL.len());
    }
}
