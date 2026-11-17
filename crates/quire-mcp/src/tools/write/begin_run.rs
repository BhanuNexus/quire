// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The write tool `begin_run`, which starts a task (a run id).
//!
//! Its definition (name, schema, description) and its handler sit together.

use super::*;

pub(super) fn register() -> Box<dyn Tool> {
    tool(
        "begin_run",
        "Start a task",
        "Get a run id to pass as `run` to every write of one task, so the person can review, accept or undo the task's changes together. Quire keeps no state for it. Example: {\"title\": \"Ingest paper X\"}.",
        schema!({"type": "object", "properties": {"title": {"type": "string"}, "purpose": {"type": "string"}}, "required": ["title"]}),
        schema!({"type": "object", "properties": {"run_id": {"type": "string"}, "title": {"type": "string"}}, "required": ["run_id"]}),
        Annotations::CREATE,
        Access::Write,
        begin_run,
    )
}

fn begin_run(cx: &ToolCx<'_>, _ext: &CallExt<'_>, a: &Value) -> ToolOutput {
    let title = s(a, "title");
    let id = RunId::new(cx.agent.now_ms());
    ToolOutput::ok(format!("Run {id} started: pass \"run\": \"{id}\" to each write of this task."), json!({"run_id": id, "title": title}))
}
