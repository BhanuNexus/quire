// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! WS-E's entry point into the workspace.
//!
//! Planning and metadata: tasks, agenda, reminders and the commands of periodic notes, flashcards and metadata refactors.
//!
//! `features.rs` calls the functions below for every Plan-owned command,
//! every frame, every vault event and every decorated document; this file
//! (and the modules it uses) belongs to WS-E. State lives in
//! `ws.features.tasks`.

use std::time::Instant;

use quire_vault::VaultEvent;

use crate::Command;
use crate::decor::{Decoration, DecorationEvent, StatusItem};
use crate::document::Document;
use crate::workspace::{NoticeLevel, UiRequest, Workspace};

/// WS-E's state, kept in `ws.features.tasks`.
#[derive(Debug, Default)]
pub struct TasksState {}

/// Time-driven work; return true to repaint.
pub(crate) fn tick(ws: &mut Workspace, now: Instant) -> bool {
    let _ = (ws, now);
    false
}

/// The earliest instant `tick` has work to do (reminders, debounces).
pub(crate) fn next_deadline(ws: &Workspace) -> Option<Instant> {
    let _ = ws;
    None
}

/// Whether a Plan-owned command is ready to offer. Commands that aren't
/// are hidden from the palette, the shortcut sheet and the keyboard, so a
/// build looks like 0.8 until a feature lands.
pub(crate) const fn available(ws: &Workspace, cmd: Command) -> bool {
    let _ = (ws, cmd);
    false
}

/// A Plan-owned command ([`Command::owner`]).
pub(crate) fn run(ws: &mut Workspace, cmd: Command) -> Option<UiRequest> {
    use Command as C;
    match cmd {
        C::ShowAgenda | C::ShowCalendar => Some(UiRequest::Feature(cmd)),
        _ => {
            ws.notify(NoticeLevel::Info, format!("{} isn't available yet", cmd.info().title.trim_end_matches('…')));
            None
        }
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
pub(crate) fn vault_event(ws: &mut Workspace, ev: &VaultEvent) {
    let _ = (ws, ev);
}

/// A vault was opened: state starts fresh (start jobs here: `ws.jobs.add(...)`).
pub(crate) fn vault_opened(ws: &mut Workspace) {
    ws.features.tasks = TasksState::default();
}

/// Status-bar items, appended to `out`.
pub(crate) fn status(ws: &Workspace, out: &mut Vec<StatusItem>) {
    let _ = (ws, out);
}
