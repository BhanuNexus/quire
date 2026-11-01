// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Quire desktop: an egui adapter over `quire-app`.
//!
//! Usage: `quire [VAULT_DIR]`. Without an argument the last vault is reopened.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod dialogs;
mod editor;
mod graph_view;
mod highlight;
mod overlays;
mod panels;
mod theme;
mod view;
mod view_ui;

use std::path::PathBuf;
use std::sync::Arc;

/// Window/taskbar icon (macOS uses the bundle's .icns instead).
fn app_icon() -> Option<Arc<egui::IconData>> {
    let img = image::load_from_memory(include_bytes!("../../../packaging/icons/quire-256.png")).ok()?.into_rgba8();
    let (width, height) = img.dimensions();
    Some(Arc::new(egui::IconData { rgba: img.into_raw(), width, height }))
}

fn main() -> eframe::Result {
    if std::env::args().nth(1).is_some_and(|a| a == "--version" || a == "-V") {
        println!("Quire {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    let vault_arg: Option<PathBuf> = std::env::args_os().nth(1).map(PathBuf::from);
    let mut viewport =
        egui::ViewportBuilder::default().with_title("Quire").with_inner_size([1280.0, 820.0]).with_min_inner_size([520.0, 360.0]);
    if let Some(icon) = app_icon() {
        viewport = viewport.with_icon(icon);
    }
    let options = eframe::NativeOptions { viewport, ..Default::default() };
    eframe::run_native("Quire", options, Box::new(move |cc| Ok(Box::new(app::QuireApp::new(cc, vault_arg)))))
}
