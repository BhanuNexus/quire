// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The 0.9 read tools (design §4.2, rows 1–16).
//!
//! One file per tool, each holding its definition and its handler. `shared`
//! holds what several of them use. [`tools_before_canvas`] lists them in the
//! design's order, which the tool-list order test and the recorded MCP sessions
//! rely on.
//!
//! Bounded by tokens, with cursors, versions, `quire://` links and trust
//! labels. Notes in `deny` zones don't exist for agents: they are never
//! listed, counted or read.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::fmt::Write as _;

use quire_agent::{LedgerQuery, Mode, Op, OpKind, ProposalFilter, ProposalStatus};
use quire_app::index::{SearchFilters, SearchKind, views};
use quire_app::intel::chat::{ChatRequest, chat_text};
use quire_app::intel::{extractive_answer, retrieve};
use quire_app::markdown::{Value as FmValue, frontmatter, outline};
use quire_app::vault::{ContentHash, NotePath};
use serde_json::{Value, json};

use super::common::{
    existing, fence_untrusted, flag, int, iso, last_actor, list, modified, note_link, note_uri, opt, parse_time, req, s, tool, trust,
    unified, version_arg, visible,
};
use super::{Access, Annotations, CallExt, Tool, ToolCx, ToolOutput};
use crate::bounds::{self, Cursor, Start};
use crate::edit::{line_count, section_lines};
use crate::instructions;

mod ask;
mod diff;
mod history;
mod list;
mod neighbors;
mod outline_tool;
mod properties;
mod proposals;
mod query_view;
mod read_version;
mod recent_activity;
mod search;
mod shared;
mod tags;
mod tasks;
mod vault_info;
mod view;

pub use shared::own_pending;
use shared::*;

/// The read tools, in the design's order (before the canvas tools).
pub fn tools_before_canvas() -> Vec<Box<dyn Tool>> {
    vec![
        vault_info::register(),
        search::register(),
        view::register(),
        outline_tool::register(),
        list::register(),
        neighbors::register(),
        recent_activity::register(),
        ask::register(),
        history::register(),
        read_version::register(),
        diff::register(),
        tags::register(),
        properties::register(),
        query_view::register(),
        tasks::register(),
        proposals::register(),
    ]
}
