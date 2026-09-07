// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright © 2026 Hector Tarrido-Picart
#![cfg(not(target_os = "linux"))]

//! True native OS menu bar via the `muda` crate (Tauri's menu lib).
//!
//! On macOS this becomes the global screen-top `NSMenu` bar; on Windows an
//! in-window `HMENU`; on Linux a gtk menu bar. All three render from the SAME
//! description as the in-window egui bar — [`crate::menu::native_model`] — so the
//! two never drift.
//!
//! Substrate discipline: each native item carries the stable id of a
//! [`crate::menu::NativeItem::Leaf`], and we keep a `HashMap<id, MenuAction>`.
//! When muda reports a click we look the id up and hand the [`MenuAction`] back to
//! [`crate::app::App::apply_menu_action`] — the exact same dispatch the in-window
//! bar uses, which routes through the op-log. Never a side channel.
//!
//! Headless / test / CI safety: this module is only *instantiated* from the
//! interactive windowed path ([`crate::app::App::new`] when a winit window
//! exists). Constructing [`NativeMenuBar`] is fallible and a no-op-returning
//! `None` on any platform where attaching fails, so `--headless`/`--shot` never
//! require a menu server. `poll()` is cheap and safe to call every frame.

use std::collections::HashMap;
use std::str::FromStr;

use muda::accelerator::Accelerator;
use muda::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu};

use crate::menu::{MenuAction, NativeItem, PredefinedKind, ViewState, native_model, needs_selection};
use crate::preset::MenuStyle;

/// A live native menu bar attached to the OS, plus the id→action routing table.
///
/// Held by the [`crate::app::App`] for the process lifetime. Dropping it detaches
/// the menu. Only ever created in interactive windowed mode.
pub struct NativeMenuBar {
    /// The root muda menu. Kept alive so the OS bar stays attached (muda tears
    /// the native menu down when this is dropped).
    _menu: Menu,
    /// Maps a clicked muda item id back to the substrate [`MenuAction`] to run.
    routes: HashMap<String, MenuAction>,
    /// Live handles to the selection-dependent items, so we can flip their
    /// enabled state each frame (disable-don't-hide) without rebuilding the bar.
    /// muda [`MenuItem`] is a cheap ref-counted handle.
    selection_items: Vec<MenuItem>,
    /// Last selection-presence we pushed to `set_enabled`, so we only touch the
    /// native items when it actually changes.
    last_has_selection: Option<bool>,
    /// Live handles to the checkable LLM toggles (Local Only / Allow Web Search),
    /// keyed by muda id, so the app can sync their checked/enabled state each
    /// frame from the real deck state.
    check_items: HashMap<String, CheckMenuItem>,
    /// Last `(local_only, web_search, terse)` we pushed, to skip redundant
    /// native calls.
    last_toggles: Option<(bool, bool, bool)>,
    /// Live handle to the View ▸ Panel item so its label flips
    /// "Hide Panel" ⇄ "Show Panel" as the panel toggles.
    panel_item: Option<MenuItem>,
    /// Last View state we pushed (active display / lighting mode + panel
    /// visibility), so we only touch the OS menu when it changes.
    last_view: Option<crate::menu::ViewState>,
}

impl NativeMenuBar {
    /// Build the native menu from the registry-driven model for `style` and
    /// attach it to the OS. Platform-specific attachment:
    ///
    /// - macOS: `init_for_nsapp` installs a global menu bar (needs the running
    ///   `NSApplication`, which winit has already created by the time the app's
    ///   first frame runs — hence we build lazily on first `ui`, see the caller).
    /// - Windows: `init_for_hwnd` with the raw `HWND` from the winit window.
    /// - Linux: `init_for_gtk_window` — winit's gtk window isn't exposed through
    ///   eframe, so we skip native attach there and rely on the in-window bar.
    ///
    /// Returns `None` (caller falls back to the in-window bar) if attachment is
    /// unavailable on this platform / handle.
    #[allow(unused_variables)]
    pub fn attach<W>(style: MenuStyle, window: &W) -> Option<Self>
    where
        W: raw_window_handle::HasWindowHandle,
    {
        // Build with no selection initially; `sync_selection` enables items once
        // the app has a selection.
        let (menu, routes, selection_items, check_items, panel_item) = build_menu(style)?;

        #[cfg(target_os = "macos")]
        {
            // Global screen-top NSMenu bar. winit has created the NSApp already.
            menu.init_for_nsapp();
            // Kill AppKit's auto-injected "Show Tab Bar" / "Show All Tabs" View
            // items: a single-window CAD app has no use for window tabs.
            disable_automatic_window_tabbing();
            Some(Self {
                _menu: menu,
                routes,
                selection_items,
                last_has_selection: None,
                check_items,
                last_toggles: None,
                panel_item,
                last_view: None,
            })
        }

        #[cfg(target_os = "windows")]
        {
            use raw_window_handle::RawWindowHandle;
            let handle = window.window_handle().ok()?;
            let RawWindowHandle::Win32(h) = handle.as_raw() else {
                return None;
            };
            // SAFETY: the HWND comes from the live winit window we were given.
            unsafe { menu.init_for_hwnd(h.hwnd.get()) }.ok()?;
            Some(Self {
                _menu: menu,
                routes,
                selection_items,
                last_has_selection: None,
                check_items,
                last_toggles: None,
                panel_item,
                last_view: None,
            })
        }

        // Linux/other: eframe does not surface the gtk window handle muda needs,
        // so we do not attach a native bar; the in-window egui bar remains.
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        {
            let _ = (menu, routes, selection_items, check_items, panel_item);
            None
        }
    }

    /// Flip the enabled state of the selection-dependent native items to match
    /// the app's current selection. Cheap and idempotent — only touches the OS
    /// menu when the presence of a selection actually changes. Call each frame.
    pub fn sync_selection(&mut self, has_selection: bool) {
        if self.last_has_selection == Some(has_selection) {
            return;
        }
        self.last_has_selection = Some(has_selection);
        for it in &self.selection_items {
            it.set_enabled(has_selection);
        }
    }

    /// Push the live LLM-toggle state onto the native checkable items. Web-search
    /// is forced unchecked + disabled while local-only is on (offline/sealed).
    /// Idempotent — only touches the OS menu when a value actually changes.
    pub fn sync_toggles(&mut self, local_only: bool, web_search: bool, terse: bool) {
        if self.last_toggles == Some((local_only, web_search, terse)) {
            return;
        }
        self.last_toggles = Some((local_only, web_search, terse));
        if let Some(it) = self.check_items.get("LLM/local_only") {
            it.set_checked(local_only);
        }
        if let Some(it) = self.check_items.get("LLM/web_search") {
            it.set_checked(web_search && !local_only);
            it.set_enabled(!local_only);
        }
        if let Some(it) = self.check_items.get("LLM/terse") {
            it.set_checked(terse);
        }
    }

    /// Push the live View state onto the native View menu: the active display /
    /// lighting radio carries the check, and the Panel item's label flips
    /// "Hide Panel" ⇄ "Show Panel". Idempotent — only touches the OS menu when the
    /// state actually changes.
    pub fn sync_view_state(&mut self, view: crate::menu::ViewState) {
        use crate::menu::{CameraTag, DisplayModeTag, LightModeTag};
        if self.last_view == Some(view) {
            return;
        }
        self.last_view = Some(view);
        // Display radios: exactly the active one checked.
        for (id, tag) in [
            ("View/disp_shaded", DisplayModeTag::Shaded),
            ("View/disp_wire", DisplayModeTag::Wireframe),
            ("View/disp_xray", DisplayModeTag::XRay),
            ("View/disp_pencil", DisplayModeTag::Pencil),
        ] {
            if let Some(it) = self.check_items.get(id) {
                it.set_checked(view.display == Some(tag));
            }
        }
        // Lighting radios: exactly the active one checked.
        for (id, tag) in [
            ("View/light_working", LightModeTag::Working),
            ("View/light_sun", LightModeTag::Sun),
            ("View/light_present", LightModeTag::Presentation),
        ] {
            if let Some(it) = self.check_items.get(id) {
                it.set_checked(view.lighting == Some(tag));
            }
        }
        // Camera-projection radios: exactly the active one checked (none for
        // ortho standard views, where projection is implied by the view).
        for (id, tag) in [
            ("View/cam_persp", CameraTag::Perspective),
            ("View/cam_2point", CameraTag::TwoPoint),
            ("View/cam_pano", CameraTag::Panorama),
            ("View/cam_fisheye", CameraTag::Fisheye),
        ] {
            if let Some(it) = self.check_items.get(id) {
                it.set_checked(view.camera == Some(tag));
            }
        }
        // Skin radios (Theme ▸ Skin): exactly the active skin checked.
        for (id, origin) in crate::menu::skin_radio_items() {
            if let Some(it) = self.check_items.get(&format!("Theme/{id}")) {
                it.set_checked(view.skin == origin);
            }
        }
        // Language radios (Theme ▸ Language): exactly the active language checked.
        for (id, lang) in crate::menu::lang_radio_items() {
            if let Some(it) = self.check_items.get(&format!("Theme/{id}")) {
                it.set_checked(view.lang == lang);
            }
        }
        // Panel item label flip.
        if let Some(it) = &self.panel_item {
            it.set_text(if view.panel_visible { "Hide Panel" } else { "Show Panel" });
        }
    }

    /// Drain muda's event queue and return the substrate [`MenuAction`] for the
    /// most recent click, if any. Cheap; call once per frame from `ui`.
    pub fn poll(&self) -> Option<MenuAction> {
        let mut chosen = None;
        // Drain everything queued since last frame; last click wins.
        while let Ok(ev) = MenuEvent::receiver().try_recv() {
            if let Some(action) = self.routes.get(ev.id.as_ref()) {
                chosen = Some(action.clone());
            }
        }
        chosen
    }
}

/// macOS: turn OFF `NSWindow.allowsAutomaticWindowTabbing`. AppKit otherwise
/// auto-injects "Show Tab Bar" / "Show All Tabs" into the View menu for any
/// document-style window; a single-window CAD app has no tabs, so those items are
/// dead weight. Must run on the main thread (it does — we're called from the
/// interactive `ui` frame, which winit drives on the main thread).
#[cfg(target_os = "macos")]
fn disable_automatic_window_tabbing() {
    use objc2::MainThreadMarker;
    use objc2_app_kit::NSWindow;
    // Safe: the interactive menu attach runs on winit's main thread.
    if let Some(mtm) = MainThreadMarker::new() {
        NSWindow::setAllowsAutomaticWindowTabbing(false, mtm);
    }
}

/// Build the muda [`Menu`] and the id→action table from the pure model. Kept
/// separate from [`NativeMenuBar::attach`] so the tree construction is testable
/// and platform-independent (attachment is the only platform-specific step).
#[allow(clippy::type_complexity)]
fn build_menu(
    style: MenuStyle,
) -> Option<(
    Menu,
    HashMap<String, MenuAction>,
    Vec<MenuItem>,
    HashMap<String, CheckMenuItem>,
    Option<MenuItem>,
)> {
    let menu = Menu::new();
    let mut routes: HashMap<String, MenuAction> = HashMap::new();
    let mut selection_items: Vec<MenuItem> = Vec::new();
    let mut check_items: HashMap<String, CheckMenuItem> = HashMap::new();
    // Live handle to the View ▸ Panel item, so its label can flip each frame.
    let mut panel_item: Option<MenuItem> = None;

    // Selection-dependent items start disabled (built with no selection); the
    // app's per-frame `sync_selection` enables them once something is selected.
    for top in native_model(style, false, ViewState::default()) {
        let submenu = Submenu::new(&top.title, true);
        for item in &top.items {
            match item {
                NativeItem::Separator => {
                    submenu.append(&PredefinedMenuItem::separator()).ok()?;
                }
                NativeItem::Leaf {
                    id,
                    label,
                    action,
                    shortcut,
                    enabled,
                } => {
                    // Parse the accelerator string (kept in sync with the keymap).
                    // A malformed string is dropped rather than failing the build.
                    let accel = shortcut
                        .as_deref()
                        .and_then(|s| Accelerator::from_str(s).ok());
                    let mi = MenuItem::with_id(id.as_str(), label, *enabled, accel);
                    // Track selection-dependent items so we can toggle them live.
                    if selection_dependent(id) {
                        selection_items.push(mi.clone());
                    }
                    // Track the Panel item so its label flips Hide ⇄ Show live.
                    if id == "View/panel" {
                        panel_item = Some(mi.clone());
                    }
                    submenu.append(&mi).ok()?;
                    routes.insert(id.clone(), action.clone());
                }
                NativeItem::Predefined(kind) => {
                    let pi = predefined(*kind);
                    submenu.append(&pi).ok()?;
                }
                NativeItem::Check { id, label, action, checked, enabled } => {
                    let ci = CheckMenuItem::with_id(id.as_str(), label, *enabled, *checked, None);
                    submenu.append(&ci).ok()?;
                    routes.insert(id.clone(), action.clone());
                    check_items.insert(id.clone(), ci);
                }
            }
        }
        menu.append(&submenu).ok()?;
    }
    Some((menu, routes, selection_items, check_items, panel_item))
}

/// Whether a native leaf id (`"<Menu>/<verb>"`) is a selection-dependent verb,
/// so its enabled state tracks the selection. Mirrors [`needs_selection`] on the
/// verb suffix of the id.
fn selection_dependent(id: &str) -> bool {
    id.rsplit('/').next().map(needs_selection).unwrap_or(false)
}

/// Map a [`PredefinedKind`] to its muda [`PredefinedMenuItem`]. `None` labels use
/// the OS-standard localized text (and shortcut) for each.
fn predefined(kind: PredefinedKind) -> PredefinedMenuItem {
    match kind {
        PredefinedKind::Minimize => PredefinedMenuItem::minimize(None),
        PredefinedKind::Zoom => PredefinedMenuItem::maximize(None),
        PredefinedKind::BringAllToFront => PredefinedMenuItem::bring_all_to_front(None),
        PredefinedKind::Fullscreen => PredefinedMenuItem::fullscreen(None),
        PredefinedKind::Quit => PredefinedMenuItem::quit(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The routing table built from the model covers every leaf id, and each id
    /// resolves to the leaf's own [`MenuAction`]. This is the substrate contract:
    /// a native click id must map to the exact action the in-window bar would run.
    /// Building the muda `Menu` itself is skipped on headless CI (no menu server),
    /// so we assert on the pure model instead — no native handle required.
    #[test]
    fn every_native_leaf_id_routes_to_its_action() {
        for style in [MenuStyle::Rhino, MenuStyle::AutoCAD] {
            let mut expected: HashMap<String, MenuAction> = HashMap::new();
            for top in native_model(style, true, ViewState::default()) {
                for item in &top.items {
                    if let NativeItem::Leaf { id, action, .. } = item {
                        assert!(
                            expected.insert(id.clone(), action.clone()).is_none(),
                            "duplicate id {id} for {style:?}"
                        );
                    }
                }
            }
            // Every id is unique and maps to a concrete action; the muda layer's
            // `routes` map is exactly this set.
            assert!(!expected.is_empty());
        }
    }

    /// M-guitest usability audit: no two menu items with DIFFERENT actions may
    /// claim the same keyboard shortcut within one menu style — a duplicate
    /// accelerator makes one of the actions unreachable from the keyboard.
    /// The same action may legitimately appear under two menus with one
    /// shortcut (e.g. Settings… in File and LLM ▸ Model Setup… both open the
    /// settings dialog on ⌘,). Normalizes case so "Cmd+S" and "cmd+s" collide
    /// as they would in the OS.
    #[test]
    fn native_shortcuts_are_unique_per_style() {
        for style in [MenuStyle::Rhino, MenuStyle::AutoCAD] {
            let mut owner: HashMap<String, MenuAction> = HashMap::new(); // accel → action
            for top in native_model(style, true, ViewState::default()) {
                for item in &top.items {
                    if let NativeItem::Leaf { shortcut: Some(s), action, .. } = item {
                        let key = s.to_ascii_lowercase();
                        if let Some(prev) = owner.insert(key, action.clone()) {
                            assert_eq!(
                                &prev, action,
                                "shortcut {s:?} bound to TWO different actions ({style:?})"
                            );
                        }
                    }
                }
            }
            assert!(!owner.is_empty(), "menus carry shortcuts to audit");
        }
    }

    /// Every menu-item accelerator string in the model parses into a muda
    /// [`Accelerator`] — a malformed shortcut would silently vanish from the OS
    /// menu, so we assert none is malformed.
    #[test]
    fn every_native_shortcut_parses() {
        for style in [MenuStyle::Rhino, MenuStyle::AutoCAD] {
            for top in native_model(style, true, ViewState::default()) {
                for item in &top.items {
                    if let NativeItem::Leaf { shortcut: Some(s), label, .. } = item {
                        assert!(
                            Accelerator::from_str(s).is_ok(),
                            "shortcut {s:?} on {label:?} does not parse"
                        );
                    }
                }
            }
        }
    }

    /// Native + in-window parity for the new Theme ▸ Skin / Language radios: the
    /// model exposes each as a `Check` carrying a stable `Theme/*` id and the
    /// SetSkin/SetLanguage action, so the muda `routes`/`check_items` maps (built
    /// from this same model) cover them and the sync path can flip their marks.
    #[test]
    fn theme_skin_language_checks_are_present_and_routed() {
        use crate::i18n::Lang;
        use crate::menu::{lang_radio_items, skin_radio_items};
        for style in [MenuStyle::Rhino, MenuStyle::AutoCAD] {
            let mut checks: std::collections::HashMap<String, MenuAction> =
                std::collections::HashMap::new();
            for top in native_model(style, true, ViewState::default()) {
                for item in &top.items {
                    if let NativeItem::Check { id, action, .. } = item {
                        checks.insert(id.clone(), action.clone());
                    }
                }
            }
            for (id, origin) in skin_radio_items() {
                assert_eq!(
                    checks.get(&format!("Theme/{id}")),
                    Some(&MenuAction::SetSkin(origin)),
                    "skin radio {id} not routed for {style:?}"
                );
            }
            for (id, lang) in lang_radio_items() {
                assert_eq!(
                    checks.get(&format!("Theme/{id}")),
                    Some(&MenuAction::SetLanguage(lang)),
                    "language radio {id} not routed for {style:?}"
                );
            }
            let _ = Lang::En;
        }
    }

    /// `selection_dependent` classifies leaf ids by their verb suffix, matching
    /// [`needs_selection`]: Move/Delete track the selection; Line/Save do not.
    #[test]
    fn selection_dependent_matches_needs_selection() {
        assert!(selection_dependent("Transform/move"));
        assert!(selection_dependent("Modify/delete"));
        assert!(!selection_dependent("Curve/line"));
        assert!(!selection_dependent("File/save"));
    }
}
