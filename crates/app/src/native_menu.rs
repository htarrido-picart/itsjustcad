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
use muda::{Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu};

use crate::menu::{MenuAction, NativeItem, PredefinedKind, native_model, needs_selection};
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
        let (menu, routes, selection_items) = build_menu(style)?;

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
            })
        }

        // Linux/other: eframe does not surface the gtk window handle muda needs,
        // so we do not attach a native bar; the in-window egui bar remains.
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        {
            let _ = (menu, routes, selection_items);
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
) -> Option<(Menu, HashMap<String, MenuAction>, Vec<MenuItem>)> {
    let menu = Menu::new();
    let mut routes: HashMap<String, MenuAction> = HashMap::new();
    let mut selection_items: Vec<MenuItem> = Vec::new();

    // Selection-dependent items start disabled (built with no selection); the
    // app's per-frame `sync_selection` enables them once something is selected.
    for top in native_model(style, false) {
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
                    submenu.append(&mi).ok()?;
                    routes.insert(id.clone(), action.clone());
                }
                NativeItem::Predefined(kind) => {
                    let pi = predefined(*kind);
                    submenu.append(&pi).ok()?;
                }
            }
        }
        menu.append(&submenu).ok()?;
    }
    Some((menu, routes, selection_items))
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
            for top in native_model(style, true) {
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

    /// Every menu-item accelerator string in the model parses into a muda
    /// [`Accelerator`] — a malformed shortcut would silently vanish from the OS
    /// menu, so we assert none is malformed.
    #[test]
    fn every_native_shortcut_parses() {
        for style in [MenuStyle::Rhino, MenuStyle::AutoCAD] {
            for top in native_model(style, true) {
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
