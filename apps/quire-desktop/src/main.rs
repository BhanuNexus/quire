// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Quire desktop: an egui adapter over `quire-app`.
//!
//! Usage: `quire [VAULT_DIR]`. Without an argument the last vault is reopened.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod activity_ui;
mod agenda_ui;
mod agent_panel_ui;
mod agents_settings;
mod ai_settings_ui;
mod app;
mod ask_ui;
mod audio_ui;
mod base_surface;
#[cfg(feature = "bench")]
mod bench;
mod board_ui;
mod calendar_ui;
mod canvas_ui;
mod capture_key;
mod cite_ui;
mod commands_ui;
mod connections_ui;
mod decor;
mod diagram_ui;
mod dialogs;
mod drawing_ui;
mod editor;
mod embed_edit;
mod git_ui;
mod graph_view;
mod highlight;
mod home_ui;
mod import_ui;
mod inbox_ui;
#[cfg(test)]
mod layout_check;
// 0.12: typed links, trail, hierarchy.
mod links_ui;
mod lint_ui;
mod lock_ui;
mod locked_media;
mod map_ui;
mod mcp_ui;
mod meta_refactor_ui;
mod onboarding_ui;
mod overlays;
mod panels;
mod pdf_ui;
mod registry;
mod remind_ui;
mod spell_ui;
mod srs_ui;
mod surfaces;
mod task_ui;
mod template_prompt_ui;
mod theme;
mod theme_ui;
mod timeline_ui;
mod toolbar_ui;
mod trust_ui;
mod typeset_ui;
mod view;
mod view_ui;
mod voice_ui;

use std::path::PathBuf;
use std::sync::Arc;

/// Window/taskbar icon (macOS uses the bundle's .icns instead).
fn app_icon() -> Option<Arc<egui::IconData>> {
    let img = image::load_from_memory(include_bytes!("../../../packaging/icons/quire-256.png")).ok()?.into_rgba8();
    let (width, height) = img.dimensions();
    Some(Arc::new(egui::IconData { rgba: img.into_raw(), width, height }))
}

fn main() -> eframe::Result {
    #[cfg(feature = "bench")]
    {
        bench::mark_start();
        if bench::headless() {
            return Ok(());
        }
    }
    if std::env::args().nth(1).is_some_and(|a| a == "--version" || a == "-V") {
        println!("Quire {}{}", env!("CARGO_PKG_VERSION"), if cfg!(feature = "typeset") { "" } else { " (Lite)" });
        return Ok(());
    }
    let vault_arg: Option<PathBuf> = std::env::args_os().nth(1).map(PathBuf::from);
    let mut viewport =
        egui::ViewportBuilder::default().with_title("Quire").with_inner_size([1280.0, 820.0]).with_min_inner_size([520.0, 360.0]);
    if let Some(icon) = app_icon() {
        viewport = viewport.with_icon(icon);
    }
    // Copy, cut and paste keys arrive as keys and the app turns them into
    // clipboard events itself (`app::clipboard_keys`): egui-winit would make
    // Ctrl+V a paste and drop the key, so Vim's Ctrl+V (Visual block) never
    // arrived, and with an empty clipboard nothing arrived at all.
    let options = eframe::NativeOptions { viewport, clipboard_shortcuts: false, ..Default::default() };
    eframe::run_native("Quire", options, Box::new(move |cc| Ok(Box::new(app::QuireApp::new(cc, vault_arg)))))
}
