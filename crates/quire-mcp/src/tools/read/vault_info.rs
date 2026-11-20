// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The read tool `vault_info`: the first call of a session.
//!
//! Its definition (name, schema, description) and its handler sit together.

use std::sync::Arc;

use super::*;

pub(super) fn register() -> Box<dyn Tool> {
    let r = Annotations::READ;
    tool(
        "vault_info",
        "About this vault",
        "Start here. Returns the vault's size, its agent instructions (AGENTS.md, if approved), the zones and what you may do in each (direct: your writes apply; propose: your writes become suggestions the person reviews; create-only: you may add new files; read; deny: hidden), write limits, the server time and how many of your suggestions are waiting. Example: {}.",
        json!({"type": "object", "properties": {}}),
        schema!({"type": "object", "properties": {
                "notes": {"type": "integer"}, "attachments": {"type": "integer"},
                "instructions": {"type": ["object", "null"], "properties": {"path": {"type": "string"}, "approved": {"type": "boolean"}}},
                "zones": {"type": "array", "items": {"type": "object", "properties": {"path": {"type": "string"}, "role": {"type": "string"}, "mode_for_you": {"type": "string"}}}},
                "truncated": {"type": "boolean"},
                "limits": {"type": "object"}, "server_time": {"type": "string"}, "pending_proposals_mine": {"type": "integer"}}}),
        r,
        Access::Read,
        vault_info,
    )
}

/// The most zones `vault_info` lists (`docs/design/0.14-agent-tools.md` §3.3).
const MAX_ZONES: usize = 50;

fn vault_info(cx: &ToolCx<'_>, ext: &CallExt<'_>, _a: &Value) -> ToolOutput {
    let ix = cx.index.read();
    let vis = Arc::clone(cx.visibility_in(&ix));
    let notes = ix.records().filter(|r| vis.allows_with_tags(&r.path, &r.tags)).count();
    drop(ix);
    // One walk of the vault per call (`vault_info` is called once a session); the listing holds
    // regular files only, so the rules decide without a disk read per file.
    let attachments = cx.vault.storage().list().map_or(0, |all| all.iter().filter(|p| !p.is_markdown() && vis.allows(p)).count());
    let policy = cx.agent.policy();
    // The same rules as the store's, with this call's folder checks: no disk read per zone.
    let probe = |prefix: &str| {
        let p = NotePath::new(format!("{}quire-probe.md", prefix.trim_start_matches("./")));
        mode_name(vis.mode(&p, OpKind::Edit), vis.mode(&p, OpKind::Create))
    };
    let mut zones: Vec<Value> = policy
        .zones
        .iter()
        .filter(|z| !z.path.contains(['*', '?', '[']))
        .map(|z| {
            let role = serde_json::to_value(z.role).unwrap_or(Value::Null);
            json!({"path": z.path, "role": role, "mode_for_you": probe(&z.path)})
        })
        .collect();
    // At most 50 zones, then what applies everywhere else.
    let truncated = zones.len() > MAX_ZONES;
    zones.truncate(MAX_ZONES);
    zones.push(json!({"path": "(everything else)", "role": "human", "mode_for_you": probe("")}));
    let instr = instructions::find(cx.vault).map(|(p, text)| {
        json!({"path": p, "approved": instructions::approved(ext.state_dir, &ContentHash::of(&text)), "uri": "quire://vault/instructions"})
    });
    let mine =
        cx.agent.proposals(&ProposalFilter { status: Some(ProposalStatus::Pending), actor: Some(cx.actor.id.clone()), path: None }).len();
    let limits = json!({
        "writes_per_minute": policy.limits.writes_per_minute,
        "max_write_bytes": policy.limits.max_write_bytes,
        "max_files_per_change": policy.limits.max_files_per_change,
        "max_pending_per_client": policy.limits.max_pending_per_client,
    });
    let st = json!({
        "notes": notes, "attachments": attachments, "instructions": instr, "zones": zones, "limits": limits,
        "truncated": truncated, "server_time": iso(cx.agent.now_ms()), "pending_proposals_mine": mine,
        "you": {"id": cx.actor.id, "label": cx.actor.label},
    });
    let mut text = format!("Vault: {notes} notes, {attachments} attachments.\nYou are {} ({}).\nZones:\n", cx.actor.label, cx.actor.id);
    for z in &zones {
        writeln!(
            text,
            "- {} ({}): you may {}",
            z["path"].as_str().unwrap_or(""),
            z["role"].as_str().unwrap_or(""),
            z["mode_for_you"].as_str().unwrap_or("")
        )
        .expect("a String takes any text");
    }
    if let Some(i) = &instr {
        writeln!(
            text,
            "Instructions: {} ({}). Read them first: resource quire://vault/instructions.",
            i["path"].as_str().unwrap_or(""),
            if i["approved"] == true { "approved" } else { "awaiting the person's review; don't rely on them" }
        )
        .expect("a String takes any text");
    }
    if truncated {
        writeln!(text, "(More zones than {MAX_ZONES}: the rest are left out.)").expect("a String takes any text");
    }
    write!(text, "Your pending suggestions: {mine}. Cite notes as [[Note#Heading]].").expect("a String takes any text");
    ToolOutput::ok(text, st)
}

/// What an agent may do in a zone, in words agents read.
fn mode_name(edit: Mode, create: Mode) -> &'static str {
    match (edit, create) {
        (Mode::Deny, _) => "deny",
        (Mode::Direct, _) => "direct",
        (Mode::Read, _) => "read",
        (_, Mode::CreateOnly) | (Mode::CreateOnly, _) => "create-only",
        (Mode::Propose, _) => "propose",
    }
}
