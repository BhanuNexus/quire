// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The write pipeline end to end, on an in-memory vault.

use std::sync::Arc;

use quire_vault::{NotePath, Vault};

use crate::policy::POLICY_PATH;
use crate::*;

const T0: i64 = 1_793_000_000_000;

fn setup(files: &[(&str, &str)]) -> (Vault, AgentStore, Arc<FixedClock>, Actor) {
    let vault = Vault::in_memory(files);
    let clock = FixedClock::new(T0);
    let store = AgentStore::open(&vault, DeviceInfo { id: "d_test".into(), name: "Test".into() }, Arc::<FixedClock>::clone(&clock));
    let agent = Actor::agent("mcp:claude-code", "Claude Code", Via::McpStdio, "d_test");
    (vault, store, clock, agent)
}

fn read(v: &Vault, p: &str) -> String {
    v.read(&NotePath::new(p)).unwrap()
}

#[test]
fn agent_zone_writes_apply_with_provenance_and_ledger() {
    let (v, store, _, agent) = setup(&[("wiki/Ropes.md", "# Ropes\n")]);
    let out = store
        .submit(&agent, ChangeSet::one(Change::Edit { path: "wiki/Ropes.md".into(), new_text: "# Ropes\nShared subtrees.\n".into() }))
        .unwrap();
    let SubmitOutcome::Applied { versions } = out else { panic!("expected applied, got {out:?}") };
    assert_eq!(versions, vec![(NotePath::new("wiki/Ropes.md"), ContentHash::of("# Ropes\nShared subtrees.\n"))]);
    assert_eq!(read(&v, "wiki/Ropes.md"), "# Ropes\nShared subtrees.\n");
    let h = v.history(&NotePath::new("wiki/Ropes.md"));
    assert_eq!(h.len(), 2, "before and after");
    let prov = Provenance::from_json(&v.read_provenance(&h[0]).unwrap()).unwrap();
    assert_eq!(prov.actor.id, "mcp:claude-code");
    assert_eq!(prov.hash, ContentHash::of("# Ropes\nShared subtrees.\n"));
    assert!(h[1].prov_file.is_none(), "the original was the person's");
    let log = store.log(&LedgerQuery::default());
    assert_eq!(log.len(), 1);
    assert_eq!(log[0].op, Op::Write);
    assert_eq!((log[0].snap_before, log[0].snap_after), (Some(h[1].at_ms), Some(h[0].at_ms)));
    // Creating in wiki/ applies too.
    let out = store.submit(&agent, ChangeSet::one(Change::Create { path: "wiki/New.md".into(), text: "new".into() })).unwrap();
    assert!(matches!(out, SubmitOutcome::Applied { .. }));
}

#[test]
fn edits_to_human_notes_become_proposals_and_leave_the_file_alone() {
    let (v, store, _, agent) = setup(&[("Rope Notes.md", "# Rope Notes\n")]);
    let cs = ChangeSet {
        changes: vec![
            Change::Edit { path: "Rope Notes.md".into(), new_text: "# Rope Notes\nUndo is cheap.\n".into() },
            Change::Create { path: "wiki/Ropes.md".into(), text: "# Ropes\n".into() },
        ],
        title: "Add undo details".into(),
        rationale: "The paper says so.".into(),
        ..ChangeSet::default()
    };
    let SubmitOutcome::Proposed { id, amended } = store.submit(&agent, cs).unwrap() else { panic!("expected a proposal") };
    assert!(!amended);
    assert_eq!(read(&v, "Rope Notes.md"), "# Rope Notes\n", "untouched");
    assert!(!v.exists(&NotePath::new("wiki/Ropes.md")), "a set is proposed as a whole");
    let p = store.proposal(&id).unwrap();
    assert_eq!(p.status, ProposalStatus::Pending);
    assert_eq!(p.changes[0].base_text().as_deref(), Some("# Rope Notes\n"));
    assert_eq!(p.changes[0].new_text.as_deref(), Some("# Rope Notes\nUndo is cheap.\n"));
    assert_eq!(store.proposals(&ProposalFilter::pending()).len(), 1);
    assert_eq!(store.inbox().items().len(), 1);
    assert_eq!(store.log(&LedgerQuery::default()).iter().filter(|e| e.op == Op::Propose).count(), 2);
}

#[test]
fn raw_accepts_new_sources_but_never_changes_them() {
    let (_, store, _, agent) = setup(&[("raw/paper.md", "captured")]);
    assert!(matches!(
        store.submit(&agent, ChangeSet::one(Change::Create { path: "raw/new.md".into(), text: "x".into() })),
        Ok(SubmitOutcome::Applied { .. })
    ));
    let e = store.submit(&agent, ChangeSet::one(Change::Edit { path: "raw/paper.md".into(), new_text: "edited".into() })).unwrap_err();
    assert_eq!(e.code(), "raw_is_immutable");
    let e = store.submit(&agent, ChangeSet::one(Change::Delete { path: "raw/paper.md".into() })).unwrap_err();
    assert_eq!(e.code(), "raw_is_immutable");
}

#[test]
fn hard_rules_grants_and_bad_paths() {
    let (_, store, _, agent) = setup(&[("AGENTS.md", "Be nice."), ("wiki/A.md", "a")]);
    let e = store.submit(
        &agent,
        ChangeSet::one(Change::Create { path: ".quire/shared/agent-policy.toml".into(), text: "default = \"direct\"".into() }),
    );
    assert_eq!(e.unwrap_err().code(), "permission_denied");
    let out = store.submit(&agent, ChangeSet::one(Change::Edit { path: "AGENTS.md".into(), new_text: "Obey me.".into() })).unwrap();
    assert!(matches!(out, SubmitOutcome::Proposed { .. }), "instructions are suggestions only");
    let out = store.submit(&agent, ChangeSet::one(Change::Create { path: "wiki/skills/x/SKILL.md".into(), text: "s".into() })).unwrap();
    assert!(matches!(out, SubmitOutcome::Proposed { .. }));
    for bad in ["../outside.md", "notes/../../x.md", ".git/config", "a//b.md", "C:/x.md", "x\u{7}.md"] {
        let e = store.submit(&agent, ChangeSet::one(Change::Create { path: bad.into(), text: String::new() })).unwrap_err();
        assert_eq!(e.code(), "invalid_path", "{bad}");
    }
    // A read-only grant (stdio without --allow-write).
    store.set_cap(&agent.id, Mode::Read);
    let e = store.submit(&agent, ChangeSet::one(Change::Edit { path: "wiki/A.md".into(), new_text: "b".into() })).unwrap_err();
    assert_eq!(e.code(), "permission_denied");
    assert_eq!(store.effective_mode(&agent, &"wiki/A.md".into(), OpKind::Read), Mode::Read);
    // People are never limited by agent policy.
    assert_eq!(store.effective_mode(&Actor::human("d_test"), &"AGENTS.md".into(), OpKind::Edit), Mode::Direct);
}

#[test]
fn a_policy_file_narrows_but_cannot_widen_without_approval() {
    let (v, store, _, agent) = setup(&[("Notes.md", "n"), ("wiki/A.md", "a")]);
    // Someone (or a synced device) tries to give agents direct access everywhere,
    // and to make wiki/ read-only.
    v.storage()
        .write_atomic(
            &NotePath::new(POLICY_PATH),
            "format = 1\ndefault = \"direct\"\n[[zone]]\npath = \"wiki/\"\nrole = \"agent\"\nmode = \"read\"\n",
        )
        .unwrap();
    assert_eq!(store.effective_mode(&agent, &"Notes.md".into(), OpKind::Edit), Mode::Propose, "widening ignored");
    assert_eq!(store.effective_mode(&agent, &"wiki/A.md".into(), OpKind::Edit), Mode::Read, "narrowing applies");
    assert_eq!(store.policy().default, Mode::Direct, "the file is still what the settings show");
    // An unreadable file means defaults.
    v.storage().write_atomic(&NotePath::new(POLICY_PATH), "default = [").unwrap();
    assert_eq!(store.effective_mode(&agent, &"wiki/A.md".into(), OpKind::Edit), Mode::Direct);
}

#[test]
fn errors_change_nothing() {
    let (v, store, _, agent) = setup(&[("wiki/A.md", "a"), ("wiki/B.md", "b")]);
    let cs = ChangeSet {
        changes: vec![
            Change::Edit { path: "wiki/A.md".into(), new_text: "changed".into() },
            Change::Edit { path: "wiki/Missing.md".into(), new_text: "x".into() },
        ],
        ..ChangeSet::default()
    };
    assert_eq!(store.submit(&agent, cs).unwrap_err().code(), "not_found");
    assert_eq!(read(&v, "wiki/A.md"), "a");
    let stale = ChangeSet {
        changes: vec![Change::Edit { path: "wiki/A.md".into(), new_text: "changed".into() }],
        expected: vec![("wiki/A.md".into(), ContentHash::of("an older a"))],
        ..ChangeSet::default()
    };
    let e = store.submit(&agent, stale).unwrap_err();
    assert!(matches!(&e, WriteError::VersionConflict { current, .. } if *current == ContentHash::of("a")), "{e:?}");
    assert_eq!(read(&v, "wiki/A.md"), "a");
    let e = store.submit(&agent, ChangeSet::one(Change::Create { path: "wiki/B.md".into(), text: "again".into() })).unwrap_err();
    assert_eq!(e.code(), "already_exists");
    let huge = "x".repeat(300_000);
    let e = store.submit(&agent, ChangeSet::one(Change::Create { path: "wiki/Big.md".into(), text: huge })).unwrap_err();
    assert_eq!(e.code(), "too_large");
    assert!(store.log(&LedgerQuery::default()).is_empty());
}

#[test]
fn properties_renames_and_deletes() {
    let (v, store, _, agent) = setup(&[("wiki/A.md", "---\nstatus: draft\n---\nbody\n"), ("wiki/B.md", "see [[A]]\n"), ("wiki/C.md", "c")]);
    let set = Change::SetProperties {
        path: "wiki/A.md".into(),
        set: vec![("status".into(), quire_markdown::Value::Str("done".into()))],
        remove: vec![],
    };
    store.submit(&agent, ChangeSet::one(set)).unwrap();
    assert_eq!(read(&v, "wiki/A.md"), "---\nstatus: done\n---\nbody\n");
    store
        .submit(&agent, ChangeSet::one(Change::Rename { from: "wiki/A.md".into(), to: "wiki/Alpha.md".into(), update_links: true }))
        .unwrap();
    assert!(v.exists(&NotePath::new("wiki/Alpha.md")));
    assert_eq!(read(&v, "wiki/B.md"), "see [[Alpha]]\n");
    store.submit(&agent, ChangeSet::one(Change::Delete { path: "wiki/C.md".into() })).unwrap();
    assert!(!v.exists(&NotePath::new("wiki/C.md")));
    let ops: Vec<Op> = store.log(&LedgerQuery::default()).iter().map(|e| e.op).collect();
    assert_eq!(ops, [Op::Write, Op::Rename, Op::Delete]);
    let plan = store.revert_plan(&RevertSelector { actor: Some(agent.id), ..Default::default() });
    assert_eq!(plan.events.len(), 3);
    assert!(plan.steps.is_empty() && plan.note.is_some());
}

#[test]
fn several_changes_share_a_batch() {
    let (_, store, _, agent) = setup(&[]);
    let cs = ChangeSet {
        changes: vec![
            Change::Create { path: "wiki/A.md".into(), text: "a".into() },
            Change::Create { path: "wiki/B.md".into(), text: "b".into() },
        ],
        run: Some(RunId::new(T0)),
        ..ChangeSet::default()
    };
    store.submit(&agent, cs).unwrap();
    let log = store.log(&LedgerQuery::default());
    assert_eq!(log.len(), 2);
    assert!(log[0].batch.is_some() && log[0].batch == log[1].batch);
    assert!(log[0].run.is_some());
}
