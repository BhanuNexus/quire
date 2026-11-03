// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Reviewing an agent's suggestion on a canvas. The review opens the
//! `.canvas` file as text; because Quire writes one card or connection per
//! line, every suggested change is one card. This bar above it says what
//! each change does in words ("Add text node “Undo is cheap”") and opens the
//! canvas itself.

use std::time::{Duration, Instant};

use egui::RichText;
use quire_app::Workspace;
use quire_app::agent::{ProposalFilter, ProposalStatus};
use quire_app::vault::NotePath;
use quire_app::workspace::UiRequest;
use quire_canvas::ops;

use crate::panels::PanelOut;
use crate::registry::FeatureUi;
use crate::theme::Palette;

/// What the bar shows, worked out at most every two seconds.
/// Each suggestion's title and its changes in words.
type Suggestions = Vec<(String, Vec<String>)>;

#[derive(Default)]
pub(crate) struct CanvasUi {
    /// For which file and revision, when, and what.
    review: Option<(NotePath, u64, Instant, Suggestions)>,
}

fn is_canvas(p: &NotePath) -> bool {
    p.as_str().to_ascii_lowercase().ends_with(".canvas")
}

/// The bar is shown while a canvas is open as text.
pub(crate) fn banner_available(ws: &Workspace) -> bool {
    ws.active_doc().and_then(|d| d.path.as_ref()).is_some_and(is_canvas)
}

/// The agent suggestions for `path`, each as its title and node-level changes.
fn suggestions(ws: &Workspace, path: &NotePath, current: &str) -> Suggestions {
    let Some(store) = ws.agent_store() else { return Vec::new() };
    let filter = ProposalFilter { status: Some(ProposalStatus::Pending), path: Some(path.clone()), ..ProposalFilter::default() };
    let before = quire_canvas::parse(current).unwrap_or_default();
    store
        .proposals(&filter)
        .into_iter()
        .filter_map(|s| {
            let p = store.proposal(&s.id)?;
            let new_text = p
                .changes
                .iter()
                .find(|c| c.path.as_ref() == Some(path) && c.state.is_none())
                .and_then(|c| c.new_text.clone().or_else(|| c.text.clone()))?;
            let after = quire_canvas::parse(&new_text).ok()?;
            let words: Vec<String> = ops::diff(&before, &after).iter().map(ops::Change::describe).collect();
            Some((format!("{} · {}", p.actor.label, p.title), words))
        })
        .collect()
}

pub(crate) fn banner(ui: &mut egui::Ui, ws: &mut Workspace, st: &mut FeatureUi, pal: &Palette) -> PanelOut {
    let out = PanelOut::default();
    let Some(doc) = ws.active_doc() else { return out };
    let Some(path) = doc.path.clone() else { return out };
    let rev = doc.revision();
    let fresh = st.canvas.review.as_ref().is_some_and(|(p, r, t, _)| *p == path && *r == rev && t.elapsed() < Duration::from_secs(2));
    if !fresh {
        let list = suggestions(ws, &path, &doc.text());
        st.canvas.review = Some((path.clone(), rev, Instant::now(), list));
    }
    let list = st.canvas.review.as_ref().map(|(_, _, _, l)| l.clone()).unwrap_or_default();
    ui.horizontal_wrapped(|ui| {
        ui.label(RichText::new("Canvas shown as text").color(pal.strong));
        ui.label(RichText::new("· each line is one card or connection, so suggestions are reviewed card by card.").color(pal.muted));
        if ui.button("Open the canvas").clicked() {
            ws.request_ui(UiRequest::OpenFile(path.clone()));
        }
    });
    for (title, words) in list.iter().take(3) {
        ui.label(RichText::new(title).size(12.5).color(pal.text));
        for w in words.iter().take(8) {
            ui.label(RichText::new(format!("   • {w}")).size(12.5).color(pal.muted));
        }
        if words.len() > 8 {
            ui.label(RichText::new(format!("   and {} more", words.len() - 8)).size(12.0).color(pal.faint));
        }
    }
    out
}
