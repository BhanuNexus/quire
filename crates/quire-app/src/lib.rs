// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Application layer. Frontends render [`Workspace`] state and send it
//! [`Command`]s; nothing here knows which UI toolkit is in use.

pub mod agent_state;
pub mod ai;
pub mod ask_state;
pub mod assistant_state;
pub mod atomic;
pub mod attach;
pub mod bases;
pub mod canvas_state;
pub mod capture;
pub mod citations;
pub mod commands;
pub mod connections;
pub mod copy_html;
pub mod decor;
pub mod device;
pub mod document;
pub mod drawings;
#[cfg(feature = "typeset")]
pub mod export_pdf;
pub mod features;
pub mod find;
mod find_state;
pub mod geo;
pub mod git;
pub mod graph;
pub mod history_ops;
pub mod jobs;
pub mod mcp_state;
pub mod meta_refactor;
pub mod nav;
pub mod onboarding;
pub mod pdf_state;
pub mod periodic;
pub mod projects;
pub mod properties;
pub mod publish;
pub mod remind_bg;
pub mod reminders;
pub mod sanitize;
pub mod secrets;
pub mod secure_state;
pub mod session;
pub mod settings;
pub mod srs_state;
pub mod suggest;
pub mod sync;
pub mod task_timer;
pub mod tasks_state;
pub mod templates_ui_state;
pub mod themes;
pub mod trust_jobs;
pub mod updates;
pub mod vault_sync;
pub mod voice;
pub mod word_goal;
pub mod workspace;

pub use ask_state::AskState;
pub use commands::{COMMANDS, Command, CommandInfo, KeyCombo, Owner};
pub use document::{DocId, Document, PendingMerge};
pub use find_state::FindState;
pub use session::Session;
pub use settings::{Settings, Theme};
pub use workspace::{Notice, NoticeLevel, Workspace};

// Re-export the layers below so frontends need one dependency.
pub use quire_agent as agent;
pub use quire_index as index;
pub use quire_intel as intel;
pub use quire_markdown as markdown;
pub use quire_plan as plan;
pub use quire_text as text;
pub use quire_vault as vault;

/// Local, human-readable time for a Unix-millisecond timestamp.
pub fn chrono_fmt(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms)
        .map(|d| d.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_default()
}
