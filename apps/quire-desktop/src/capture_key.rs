// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The quick-capture shortcut, heard from any app while Quire runs.
//!
//! Cmd+Shift+Space on macOS (Ctrl+Option+Space switches input sources
//! there), Ctrl+Alt+Space elsewhere. On Linux it needs X11 (or `XWayland`
//! with an X window focused); if it can't be registered, Quick capture is
//! still in the command palette.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

use global_hotkey::hotkey::{Code, HotKey, Modifiers};
use global_hotkey::{GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState};

static PRESSED: AtomicBool = AtomicBool::new(false);
/// Why the shortcut couldn't be registered, for Settings to show.
static UNAVAILABLE: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

/// Why the shortcut isn't working, if it isn't.
pub(crate) fn unavailable() -> Option<String> {
    UNAVAILABLE.lock().ok().and_then(|g| g.clone())
}
static WAKE: OnceLock<egui::Context> = OnceLock::new();

/// The shortcut as people read it.
pub(crate) const fn label() -> &'static str {
    if cfg!(target_os = "macos") { "⌘⇧Space" } else { "Ctrl+Alt+Space" }
}

fn hotkey() -> HotKey {
    let mods = if cfg!(target_os = "macos") { Modifiers::META | Modifiers::SHIFT } else { Modifiers::CONTROL | Modifiers::ALT };
    HotKey::new(Some(mods), Code::Space)
}

/// A registered shortcut; dropping it gives the keys back.
pub(crate) struct CaptureKey {
    manager: GlobalHotKeyManager,
    key: HotKey,
}

impl CaptureKey {
    pub(crate) fn register(ctx: &egui::Context) -> Result<Self, String> {
        let _ = WAKE.set(ctx.clone());
        // One handler for the process: note the press and wake the UI, which
        // may be minimized and not repainting.
        GlobalHotKeyEvent::set_event_handler(Some(|e: GlobalHotKeyEvent| {
            if e.state == HotKeyState::Pressed && e.id == hotkey().id() {
                PRESSED.store(true, Ordering::Relaxed);
                if let Some(ctx) = WAKE.get() {
                    ctx.request_repaint();
                }
            }
        }));
        let result = GlobalHotKeyManager::new().map_err(|e| e.to_string()).and_then(|manager| {
            let key = hotkey();
            manager.register(key).map_err(|e| format!("{} is taken by another app ({e})", label()))?;
            Ok(Self { manager, key })
        });
        if let Ok(mut g) = UNAVAILABLE.lock() {
            *g = result.as_ref().err().cloned();
        }
        result
    }

    /// Was the shortcut pressed since the last call? (Only while registered.)
    pub(crate) fn take(_registered: &Self) -> bool {
        PRESSED.swap(false, Ordering::Relaxed)
    }
}

impl Drop for CaptureKey {
    fn drop(&mut self) {
        let _ = self.manager.unregister(self.key);
    }
}
