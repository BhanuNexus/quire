// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! WS-D's entry point into the workspace.
//!
//! The agent panel, the external agent (ACP), on-device AI (downloads, OCR),
//! the first-run AI choice, and the wiki loop's upkeep: `wiki/index.md`
//! regenerated 10 s after wiki pages change, and a line in `wiki/log.md`
//! for each agent run and each batch of accepted suggestions in `wiki/`.
//!
//! `features.rs` calls the functions below for every Assistant-owned command,
//! every frame, every vault event and every decorated document; this file
//! (and the modules it uses) belongs to WS-D. State lives in
//! `ws.features.assistant` (and `ws.features.ai`, `ws.features.onboarding`).

pub mod wiki;
mod wiki_job;

use std::time::Instant;

use quire_vault::VaultEvent;

use crate::Command;
use crate::decor::{Decoration, DecorationEvent, StatusItem};
use crate::document::Document;
use crate::workspace::{UiRequest, Workspace};

/// WS-D's state, kept in `ws.features.assistant`.
#[derive(Default)]
pub struct AssistantState {
    wiki: Option<wiki_job::Handle>,
    /// What the Ingest dialog should start with (the open note, if it is a source).
    pub ingest_source: Option<quire_vault::NotePath>,
}

impl std::fmt::Debug for AssistantState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AssistantState").field("ingest_source", &self.ingest_source).finish_non_exhaustive()
    }
}

/// Time-driven work; return true to repaint.
pub(crate) fn tick(ws: &mut Workspace, now: Instant) -> bool {
    let _ = now;
    crate::onboarding::tick(ws);
    crate::ai::ocr::tick(ws);
    if let Some(h) = &ws.features.assistant.wiki {
        // The index waits while the person has it open with unsaved edits.
        let busy = ws.docs().iter().any(|d| d.path.as_ref().is_some_and(|p| p.as_str() == wiki::INDEX) && d.is_dirty());
        h.set_index_open_dirty(busy);
    }
    ws.ai_tick()
}

/// The earliest instant `tick` has work to do (reminders, debounces).
pub(crate) fn next_deadline(ws: &Workspace) -> Option<Instant> {
    ws.features.ai.busy().then(|| Instant::now() + std::time::Duration::from_millis(250))
}

/// Whether an Assistant-owned command is ready to offer. Commands that
/// aren't are hidden from the palette, the shortcut sheet and the keyboard.
pub(crate) const fn available(ws: &Workspace, cmd: Command) -> bool {
    match cmd {
        Command::AiSetup => true,
        Command::ToggleAgentPanel => ws.settings.agent_panel.enabled,
        // 0.11: with AI off for the vault, nothing is ingested (the source would go to a model).
        Command::IngestIntoWiki => ws.agent.is_some() && !crate::secure_state::ai_off_applied(ws),
        Command::ConnectExternalAgent => ws.agent.is_some(),
        Command::RecordVoiceNote => ws.vault.is_some() && ws.features.ai.voice.can_record,
        _ => false,
    }
}

/// An Assistant-owned command ([`Command::owner`]).
#[expect(clippy::unnecessary_wraps, reason = "the features hub's signature: other owners return `None` for some commands")]
pub(crate) fn run(ws: &mut Workspace, cmd: Command) -> Option<UiRequest> {
    use Command as C;
    match cmd {
        C::IngestIntoWiki => {
            ws.features.assistant.ingest_source = ws.active_doc().and_then(|d| d.path.clone()).filter(|p| !wiki::is_page(p));
            Some(UiRequest::Feature(cmd))
        }
        _ => Some(UiRequest::Feature(cmd)),
    }
}

/// Decorations for `doc`, appended to `out`.
pub(crate) fn decorations(ws: &Workspace, doc: &Document, out: &mut Vec<Decoration>) {
    let _ = (ws, doc, out);
}

/// A click on one of this feature's decoration actions.
pub(crate) fn decoration_event(ws: &mut Workspace, ev: &DecorationEvent) {
    let _ = (ws, ev);
}

/// A file changed in the vault (notes and attachments).
#[expect(clippy::needless_pass_by_ref_mut, reason = "the features hub's signature")]
pub(crate) fn vault_event(ws: &mut Workspace, ev: &VaultEvent) {
    if let (Some(h), VaultEvent::Changed(p) | VaultEvent::Removed(p)) = (&ws.features.assistant.wiki, ev)
        && wiki::is_page(p)
    {
        h.touch();
    }
    crate::ai::ocr::vault_event(ws, ev);
}

/// A vault was opened: state starts fresh, and the wiki job starts.
pub(crate) fn vault_opened(ws: &mut Workspace) {
    ws.features.assistant = AssistantState::default();
    crate::ai::ocr::reset(ws);
    if ws.agent.is_some() {
        let (job, handle) = wiki_job::WikiJob::new(ws.device().id.clone());
        ws.jobs.add(Box::new(job));
        ws.features.assistant.wiki = Some(handle);
    }
    // Background jobs get the configured chat model.
    ws.refresh_job_chat();
}

/// Status-bar items, appended to `out`.
pub(crate) fn status(ws: &Workspace, out: &mut Vec<StatusItem>) {
    let _ = (ws, out);
}
