// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The write tool `revert`, which undoes the changes of an actor or a run.
//!
//! Its definition (name, schema, description) and its handler sit together.

use super::{
    Access, Annotations, CallExt, NotePath, RevertSelector, RunId, SubmitOutcome, Tool, ToolCx, ToolOutput, Value, json, list, opt,
    outcome, outcome_schema, tool, visible, write_error,
};

pub(super) fn register() -> Box<dyn Tool> {
    let o = outcome_schema;
    tool(
        "revert",
        "Undo agent changes",
        "Suggest undoing changes made by an agent (you, by default): those of one run, since a time, or to some notes. The undo is always a suggestion the person reviews; it keeps edits they made since. Example: {\"run\": \"r_…\"} or {\"since\": \"2h\"}.",
        schema!({"type": "object", "properties": {
                "actor": {"type": "string", "description": "Whose changes; defaults to yours."},
                "run": {"type": "string"}, "since": {"type": "string", "description": "RFC 3339, YYYY-MM-DD, or a time back from now: 30m, 24h, 7d, 2w"},
                "paths": {"type": "array", "items": {"type": "string"}}}}),
        o(),
        Annotations::WRITE,
        Access::Write,
        revert,
    )
}

fn revert(cx: &ToolCx<'_>, _ext: &CallExt<'_>, a: &Value) -> ToolOutput {
    let now = cx.agent.now_ms();
    let since = match super::super::common::time_any(a, &["since"], now) {
        Ok(t) => t,
        Err(e) => return e,
    };
    let run = match opt(a, "run") {
        Some(r) => match RunId::parse(&r) {
            Some(r) => Some(r),
            None => return ToolOutput::error("invalid_args", format!("`run` must be a run id (r_…), got {r}")),
        },
        None => None,
    };
    let paths = list(a, "paths");
    let mut sel = RevertSelector {
        actor: Some(opt(a, "actor").unwrap_or_else(|| cx.actor.id.clone())),
        run,
        since,
        paths: (!paths.is_empty()).then(|| {
            paths
                .iter()
                .map(|p| {
                    let is_md = std::path::Path::new(p).extension().is_some_and(|e| e.eq_ignore_ascii_case("md"));
                    NotePath::new(if is_md { p.clone() } else { format!("{p}.md") })
                })
                .collect()
        }),
        ..RevertSelector::default()
    };
    let plan = cx.agent.revert_plan(&sel);
    // Only the notes this client may see; the undo itself is built by the
    // store (three-way, against what is on disk now).
    let visible_paths: Vec<NotePath> = plan.steps.iter().filter(|st| visible(cx, &st.path)).map(|st| st.path.clone()).collect();
    let Some(first) = visible_paths.first().cloned() else {
        let why = plan.note.unwrap_or_else(|| format!("No changes match ({} events found).", plan.events.len()));
        return ToolOutput::Err {
            code: "nothing_to_revert",
            message: why.clone(),
            structured: json!({"error": "nothing_to_revert", "events": plan.events.len(), "message": why}),
        };
    };
    let n = visible_paths.len();
    sel.paths = Some(visible_paths);
    let title = format!("Undo {n} change{} by {}", if n == 1 { "" } else { "s" }, sel.actor.as_deref().unwrap_or(""));
    // An undo by an agent is always a suggestion, even in agent zones.
    match cx.agent.propose_revert(&sel, cx.actor, &title) {
        Ok(Some(id)) => outcome(&first, &SubmitOutcome::Proposed { id, amended: false }, json!({"changes": n})),
        Ok(None) => ToolOutput::Err {
            code: "nothing_to_revert",
            message: "Nothing left to undo.".into(),
            structured: json!({"error": "nothing_to_revert", "events": plan.events.len(), "message": "Nothing left to undo."}),
        },
        Err(e) => write_error(&e),
    }
}
