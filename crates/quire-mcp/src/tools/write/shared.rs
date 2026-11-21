// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! What several write tools share: the schema pieces, the run id, and the
//! path that submits a change (asking the client first when it may approve).

use super::{
    Actor, CallExt, Change, ChangeSet, ContentHash, Decision, EditOp, NotePath, OpKind, Route, RunId, StatePayload, SubmitOutcome, ToolCx,
    ToolOutput, Value, Via, WriteError, existing, hard_rule, json, merge3, mrtr, note_uri, opt, outcome, own_pending, unified, version_arg,
    write_error,
};

pub(super) const RUN: &str = "Optional run id from begin_run, to group the changes of one task.";

pub(super) const EXPECTED: &str = "The version you read (from view). If the note changed since, the edit is re-applied when it still fits, or you get version_conflict with a diff.";

pub(super) fn props(extra: Value) -> Value {
    let mut p = extra;
    p["run"] = json!({"type": "string", "description": RUN});
    p
}

pub(super) fn run_arg(a: &Value) -> Result<Option<RunId>, ToolOutput> {
    match opt(a, "run") {
        None => Ok(None),
        Some(r) => RunId::parse(&r)
            .map(Some)
            .ok_or_else(|| ToolOutput::error("invalid_args", format!("`run` must be an id from begin_run (r_…), got {r}"))),
    }
}

/// Whether `cs` would become a proposal for this caller (not applied, not refused).
pub(super) fn would_propose(cx: &ToolCx<'_>, cs: &ChangeSet) -> bool {
    let route = |p: &NotePath, op| cx.agent.effective_mode(cx.actor, p, op).route(op);
    let routes: Vec<Route> = cs
        .changes
        .iter()
        .flat_map(|c| match c {
            Change::Create { path, .. } => vec![route(path, OpKind::Create)],
            Change::Edit { path, .. } | Change::SetProperties { path, .. } => vec![route(path, OpKind::Edit)],
            Change::Delete { path } => vec![route(path, OpKind::Delete)],
            Change::Rename { from, to, .. } => vec![route(from, OpKind::Rename), route(to, OpKind::Create)],
        })
        .collect();
    routes.iter().all(|r| matches!(r, Route::Propose | Route::Apply)) && routes.contains(&Route::Propose)
}

/// Paths a change set touches.
pub(super) fn paths(cs: &ChangeSet) -> Vec<&NotePath> {
    cs.changes
        .iter()
        .flat_map(|c| match c {
            Change::Rename { from, to, .. } => vec![from, to],
            other => vec![other.path()],
        })
        .collect()
}

/// Submit `cs`, asking the client first when it may approve changes itself.
#[expect(clippy::too_many_arguments, reason = "the change, how to describe it, and the call it came from")]
/// `before`/`after` describe the change for the approval form.
pub(super) fn submit(
    cx: &ToolCx<'_>,
    ext: &CallExt<'_>,
    a: &Value,
    tool: &str,
    path: &NotePath,
    cs: ChangeSet,
    diff: &str,
    base: &ContentHash,
    extra: Value,
) -> ToolOutput {
    let templates = cx.vault.config.templates_folder.clone();
    let approvable = ext.approve_in_client
        && ext.sealer.is_some()
        && paths(&cs).iter().all(|p| hard_rule(p.as_str(), &templates).is_none())
        && would_propose(cx, &cs);
    if approvable && let Some(sealer) = ext.sealer {
        let digest = mrtr::args_digest(tool, a);
        let now = cx.agent.now_ms();
        let ask_again = || {
            let sealed = sealer.seal(&StatePayload {
                principal: cx.actor.id.clone(),
                exp: now + mrtr::STATE_TTL_MS,
                method: ext.method.to_string(),
                args_digest: digest.clone(),
                base_version: base.to_string(),
            });
            match sealed {
                Ok(state) => ToolOutput::InputRequired { requests: mrtr::approval_request(path.as_str(), diff), state },
                // D2: no state under a zero nonce; nothing is written.
                Err(e) => ToolOutput::Err {
                    code: "no_randomness",
                    message: format!("Quire can't ask for approval right now ({e}); nothing was written."),
                    structured: json!({"error": "no_randomness"}),
                },
            }
        };
        match (ext.request_state, ext.input_responses) {
            (Some(state), Some(responses)) => {
                let payload = match sealer.open(state, &cx.actor.id, ext.method, &digest, now) {
                    Ok(p) => p,
                    Err(e) => {
                        return ToolOutput::Err {
                            code: "invalid_request_state",
                            message: format!("The approval state was refused ({e:?}); call the tool again without requestState."),
                            structured: json!({"error": "invalid_request_state", "reason": format!("{e:?}")}),
                        };
                    }
                };
                if payload.base_version != base.as_str() {
                    // The note changed while the person decided: ask about the new change.
                    return ask_again();
                }
                match mrtr::decision(responses) {
                    None => return ask_again(),
                    Some(Decision::Cancel) => {
                        return ToolOutput::Err {
                            code: "cancelled",
                            message: "The person cancelled this change in their app; nothing was written.".into(),
                            structured: json!({"error": "cancelled"}),
                        };
                    }
                    Some(Decision::Propose) => {}
                    Some(Decision::Apply) => return apply_approved(cx, path, cs, extra),
                }
            }
            _ => return ask_again(),
        }
    }
    match cx.agent.submit(cx.actor, cs) {
        Ok(out) => {
            if let SubmitOutcome::Applied { versions } = &out {
                for (p, _) in versions {
                    match cx.vault.read_regular(p) {
                        Ok(Some(t)) => {
                            ext.served.store(&t);
                            cx.index.upsert(p.clone(), t);
                        }
                        // A link is not a note: never read through it.
                        Ok(None) => cx.index.remove(p.clone()),
                        Err(_) => {}
                    }
                }
                cx.index.flush();
            }
            outcome(path, &out, extra)
        }
        Err(e) => write_error(&e),
    }
}

/// Write a change the person approved in the agent's client:
/// as the person, with the agent recorded as the source.
pub(super) fn apply_approved(cx: &ToolCx<'_>, path: &NotePath, mut cs: ChangeSet, extra: Value) -> ToolOutput {
    let mut approver = Actor::human(&cx.agent.device().id);
    approver.via = Via::ClientApproval;
    approver.label = format!("You, approving {} in its app", cx.actor.label);
    cs.accepted_from = Some(cx.actor.clone());
    match cx.agent.submit(&approver, cs) {
        Ok(out) => {
            if let SubmitOutcome::Applied { versions } = &out {
                for (p, _) in versions {
                    match cx.vault.read_regular(p) {
                        Ok(Some(t)) => cx.index.upsert(p.clone(), t),
                        // A link is not a note: never read through it.
                        Ok(None) => cx.index.remove(p.clone()),
                        Err(_) => {}
                    }
                }
                cx.index.flush();
            }
            let mut extra = extra;
            extra["approved_in_client"] = true.into();
            outcome(path, &out, extra)
        }
        Err(e) => write_error(&e),
    }
}

/// Apply `op` to `path` with optimistic concurrency, then submit.
/// The one path every edit tool takes: read, apply `op`, check the
/// version, then submit (applied, or a proposal under review).
pub(crate) fn edit(cx: &ToolCx<'_>, ext: &CallExt<'_>, a: &Value, tool: &str, op: &EditOp) -> ToolOutput {
    let path = match existing(cx, a, "path") {
        Ok(p) => p,
        Err(e) => return e,
    };
    let (expected, run) = match (version_arg(a, "expected_version"), run_arg(a)) {
        (Ok(e), Ok(r)) => (e, r),
        (Err(e), _) | (_, Err(e)) => return e,
    };
    let Ok(current) = cx.vault.read(&path) else { return ToolOutput::error("not_found", format!("Couldn't read {path}.")) };
    let current_v = ContentHash::of(&current);
    // With a pending suggestion of its own, an agent edits that (it is amended).
    let pending = own_pending(cx, &path).map(|(_, t)| t);
    let on_pending = pending.is_some();
    let working = pending.unwrap_or_else(|| current.clone());
    let working_v = ContentHash::of(&working);
    let mut extra = json!({});
    if let EditOp::ReplaceAll { old, .. } = op {
        extra["replacements"] = working.matches(old.as_str()).count().into();
    }
    let new_text = match &expected {
        Some(v) if *v != working_v && *v != current_v => {
            let base = ext.served.get(&path, v);
            match op.apply(&working) {
                Ok(t) if op.content_addressed() => {
                    extra["rebased"] = true.into();
                    extra["diff_since"] = base.as_deref().map_or_else(String::new, |b| unified(b, &working, path.as_str())).into();
                    t
                }
                _ => {
                    let diff_since = base.as_deref().map_or_else(String::new, |b| unified(b, &working, path.as_str()));
                    let merged_preview = base.as_deref().and_then(|b| {
                        let ours = op.apply(b).ok()?;
                        let m = merge3(b, &working, &ours);
                        m.is_clean().then_some(m.text)
                    });
                    return write_error(&WriteError::VersionConflict {
                        path,
                        expected: v.clone(),
                        current: working_v,
                        diff_since,
                        merged_preview,
                    });
                }
            }
        }
        _ => match op.apply(&working) {
            Ok(t) => t,
            Err(e) => {
                let code = e.code();
                return ToolOutput::Err { code, message: e.message(path.as_str()), structured: json!({"error": code, "path": path}) };
            }
        },
    };
    if new_text == working {
        let mut st = json!({"outcome": "applied", "versions": [{"path": path, "version": working_v}], "uri": note_uri(&path), "message": "No change: the note already says this."});
        if let Some(o) = st.as_object_mut()
            && let Value::Object(e) = extra
        {
            o.extend(e);
        }
        return ToolOutput::ok("No change: the note already says this.", st);
    }
    let diff = unified(&working, &new_text, path.as_str());
    let cs = ChangeSet {
        changes: vec![Change::Edit { path: path.clone(), new_text }],
        run,
        title: title_for(tool, &path),
        // Written on top of its pending text: the store replaces that text
        // instead of merging the edit into it a second time.
        expected: vec![(path.clone(), if on_pending { working_v } else { current_v.clone() })],
        ..ChangeSet::default()
    };
    submit(cx, ext, a, tool, &path, cs, &diff, &current_v, extra)
}

pub(super) fn title_for(tool: &str, path: &NotePath) -> String {
    match tool {
        "create" => format!("Create {}", path.stem()),
        "rename" => format!("Rename {}", path.stem()),
        "delete" => format!("Delete {}", path.stem()),
        "set_properties" => format!("Set properties of {}", path.stem()),
        "add_link" => format!("Add a typed link to {}", path.stem()),
        _ => format!("Edit {}", path.stem()),
    }
}
