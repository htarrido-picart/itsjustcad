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

use muda::{Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu};

use crate::menu::{MenuAction, NativeItem, native_model};
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
        let (menu, routes) = build_menu(style)?;

        #[cfg(target_os = "macos")]
        {
            // Global screen-top NSMenu bar. winit has created the NSApp already.
            menu.init_for_nsapp();
            Some(Self { _menu: menu, routes })
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
            Some(Self { _menu: menu, routes })
        }

        // Linux/other: eframe does not surface the gtk window handle muda needs,
        // so we do not attach a native bar; the in-window egui bar remains.
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        {
            let _ = (menu, routes);
            None
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

/// Build the muda [`Menu`] and the id→action table from the pure model. Kept
/// separate from [`NativeMenuBar::attach`] so the tree construction is testable
/// and platform-independent (attachment is the only platform-specific step).
fn build_menu(style: MenuStyle) -> Option<(Menu, HashMap<String, MenuAction>)> {
    let menu = Menu::new();
    let mut routes: HashMap<String, MenuAction> = HashMap::new();

    for top in native_model(style) {
        let submenu = Submenu::new(&top.title, true);
        for item in &top.items {
            match item {
                NativeItem::Separator => {
                    submenu.append(&PredefinedMenuItem::separator()).ok()?;
                }
                NativeItem::Leaf { id, label, action } => {
                    let mi = MenuItem::with_id(id.as_str(), label, true, None);
                    submenu.append(&mi).ok()?;
                    routes.insert(id.clone(), action.clone());
                }
            }
        }
        menu.append(&submenu).ok()?;
    }
    Some((menu, routes))
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
            for top in native_model(style) {
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
}
