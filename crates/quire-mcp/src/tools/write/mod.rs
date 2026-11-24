// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! One file per tool, each holding its definition and its handler; `shared`
//! holds the submit path they use. [`tools`] lists them in the design's order.
//!
//! The 0.9 write tools (`docs/design/0.9.md` §4.2, rows 18–30) and optimistic concurrency
//! (`docs/design/0.9.md` §B.2, §4.4).
//!
//! Every write goes through `AgentStore::submit`: in an agent zone it
//! applies, in the person's notes it becomes a proposal they review. An
//! edit names the version it was made against (`expected_version`):
//!
//! * the same version: the edit applies;
//! * an older version, and the edit finds its place by content
//!   (`str_replace`, `edit_section`, `append`, `set_properties`) and still
//!   fits the current text: it applies to the current text, and the result
//!   says `rebased` with a diff of what changed in between;
//! * otherwise `version_conflict`, with the diff since that version and, if
//!   a clean three-way merge exists, the merged text.
//!
//! When the vault lets a client approve changes itself (MRTR, `docs/design/0.9.md` §4.5), a
//! write that would be a proposal asks the client first.

use std::fmt::Write as _;

use quire_agent::policy::{Route, hard_rule};
use quire_agent::{Actor, Change, ChangeSet, Mode, OpKind, RevertSelector, RunId, SourceRef, SubmitOutcome, Via, WriteError};
use quire_app::markdown::Value as FmValue;
use quire_app::markdown::comment;
use quire_app::text::merge::merge3;
use quire_app::vault::{ContentHash, NotePath, sanitize_title};
use serde_json::{Value, json};

use super::common::{
    existing, flag, list, note_uri, opt, opt_any, outcome, outcome_schema, path_arg, req, req_any, s, tool, unified, version_arg, visible,
    write_error,
};
use super::read::own_pending;
use super::{Access, Annotations, CallExt, Tool, ToolCx, ToolOutput};
use crate::edit::{EditOp, SectionMode};
use crate::mrtr::{self, Decision, StatePayload};

mod append;
mod begin_run;
mod capture_url;
mod create;
mod delete;
mod edit_section;
mod insert;
mod rename;
mod revert;
mod save_answer;
mod set_properties;
mod shared;
mod str_replace;

pub(crate) use shared::edit;
use shared::{EXPECTED, props, run_arg, submit, title_for};

/// The write tools after the canvas ones, in the design's order.
pub fn tools() -> Vec<Box<dyn Tool>> {
    vec![
        begin_run::register(),
        create::register(),
        str_replace::register(),
        insert::register(),
        edit_section::register(),
        append::register(),
        set_properties::register(),
        rename::register(),
        delete::register(),
        save_answer::register(),
        capture_url::register(),
        revert::register(),
    ]
}
