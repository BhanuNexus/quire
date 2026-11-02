// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The Inbox tab of the right panel: suggested changes, checks, contradictions, stale notes (design §7.3).
//!
//! Owned by WS-A. Registered in `registry.rs`; hidden until [`available`]
//! says otherwise, so 0.9 builds look like 0.8 until the feature is ready.

use egui::RichText;
use quire_app::Workspace;

use crate::panels::PanelOut;
use crate::registry::FeatureUi;
use crate::theme::Palette;

/// UI state kept between frames (in [`FeatureUi`]).
#[derive(Default)]
pub(crate) struct InboxUi {}

/// Whether the tab is offered.
pub(crate) const fn available(_ws: &Workspace) -> bool {
    false
}

pub(crate) fn show(ui: &mut egui::Ui, _ws: &mut Workspace, st: &mut FeatureUi, pal: &Palette) -> PanelOut {
    let _ = &mut st.inbox;
    ui.label(RichText::new("Not available yet.").color(pal.muted));
    PanelOut::default()
}
