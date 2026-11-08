// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! "Ingest into wiki…" with a scripted model on a fixture source (design
//! §6.2 WS-D acceptance): the expected pages, citations, index and log;
//! the person's note gets a suggestion, not an edit; and "Revert run"
//! restores the vault byte for byte.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use quire_agent::{Actor, AgentStore, DeviceInfo, ProposalFilter, RevertSelector, SystemClock, Via};
use quire_app::intel::chat::{CancelFlag, ScriptedChat};
use quire_app::vault::Vault;
use quire_assistant::wiki::{Ingest, IngestEvent, estimate, ingest};
use serde_json::json;

const SOURCE: &str = "---\nsource: https://example.org/ropes\ntrust: untrusted\n---\n# Ropes: an alternative to strings\n\n## Abstract\n\nRopes are binary trees whose leaves hold short strings. Concatenation of two ropes takes constant time.\n\n## Undo\n\nBecause subtrees are shared between versions, keeping old versions for undo is cheap.\n";

/// Every file outside `.quire/` and `.trash/`, with its bytes.
#[expect(clippy::unwrap_used, reason = "a test helper: any failure should fail the test")]
fn snapshot(root: &Path) -> BTreeMap<String, Vec<u8>> {
    fn walk(dir: &Path, root: &Path, out: &mut BTreeMap<String, Vec<u8>>) {
        for e in std::fs::read_dir(dir).unwrap().flatten() {
            let p = e.path();
            let rel = p.strip_prefix(root).unwrap().to_string_lossy().replace('\\', "/");
            if rel.starts_with(".quire") || rel.starts_with(".trash") {
                continue;
            }
            if p.is_dir() {
                walk(&p, root, out);
            } else {
                out.insert(rel, std::fs::read(&p).unwrap());
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(root, root, &mut out);
    out
}

#[test]
fn ingest_writes_the_wiki_suggests_links_and_reverts_byte_for_byte() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    std::fs::create_dir_all(root.join("raw")).unwrap();
    std::fs::create_dir_all(root.join("wiki/concepts")).unwrap();
    std::fs::write(root.join("raw/Ropes paper.md"), SOURCE).unwrap();
    std::fs::write(root.join("wiki/concepts/Rope.md"), "---\ntype: concept\n---\n# Rope\n\nA tree of strings.[^1]\n\n[^1]: Something.\n")
        .unwrap();
    std::fs::write(root.join("Editors.md"), "# Editors\n\nMy editor uses structural sharing for its history.\n").unwrap();
    std::fs::write(
        root.join("wiki/index.md"),
        "# My wiki\n\nHand-written intro.\n\n<!-- quire:index:start -->\nold\n<!-- quire:index:end -->\n\nHand-written footer.\n",
    )
    .unwrap();
    let before = snapshot(root);

    let vault = Vault::open_local(root).unwrap();
    let agent = AgentStore::open(&vault, DeviceInfo { id: "d_test".into(), name: "Test".into() }, Arc::new(SystemClock));
    let model = ScriptedChat::texts([
        json!({"summary_title": "Ropes paper", "concepts": [{"name": "Rope", "why": "the data structure"}, {"name": "Structural sharing", "why": "why undo is cheap"}], "entities": []}).to_string(),
        format!(
            "Here you go: {}",
            json!({"points": [
                {"text": "Ropes are binary trees of short strings", "quote": "Ropes are binary trees whose leaves hold short strings."},
                {"text": "Concatenating ropes takes constant time", "quote": "concatenation of two ropes takes constant time"},
                {"text": "A claim the source never makes", "quote": "ropes are always faster than gap buffers"}
            ]})
        ),
        json!({"definition": "", "points": [{"text": "Ropes share subtrees between versions", "quote": "subtrees are shared between versions"}]}).to_string(),
        json!({"definition": "Reusing unchanged parts of a structure across versions.", "points": [{"text": "Sharing subtrees makes undo cheap", "quote": "keeping old versions for undo is cheap"}]}).to_string(),
    ]);
    let actor = Actor::agent("panel:anthropic", "Ingest", Via::App, "d_test");
    let job = Ingest {
        model: &model,
        agent: &agent,
        actor: &actor,
        source: "raw/Ropes paper.md".into(),
        today: "2026-11-03".into(),
        now: "2026-11-03 14:05".into(),
    };
    let mut steps = Vec::new();
    let report = ingest(&job, &CancelFlag::new(), &mut |e| {
        if let IngestEvent::Step(s) = e {
            steps.push(s);
        }
    })
    .unwrap();
    assert_eq!(steps.first().map(String::as_str), Some("Planning"));
    assert_eq!(report.dropped, 1, "the bullet whose quote isn't in the source is dropped");

    // The source summary, cited.
    let summary = std::fs::read_to_string(root.join("wiki/sources/Ropes paper.md")).unwrap();
    assert!(summary.starts_with("---\ntype: source\nsource: \"[[raw/Ropes paper]]\"\ningested: 2026-11-03\nby: Scripted\n"), "{summary}");
    assert!(summary.contains("- Ropes are binary trees of short strings.[^1]"), "{summary}");
    assert!(
        summary.contains(
            "[^1]: [[raw/Ropes paper#Abstract]] — “Ropes are binary trees whose leaves hold short strings.” <!-- quire:cite v=1 sha="
        ),
        "{summary}"
    );
    assert!(summary.contains("[^2]:") && !summary.contains("[^3]:") && !summary.contains("gap buffers"), "{summary}");
    assert!(summary.contains("## Concepts\n\n- [[Rope]]\n- [[Structural sharing]]"));
    // The existing concept page got a section, numbered after its own footnote.
    let rope = std::fs::read_to_string(root.join("wiki/concepts/Rope.md")).unwrap();
    assert!(rope.contains("## From Ropes paper\n\n- Ropes share subtrees between versions.[^2]"), "{rope}");
    assert!(rope.contains("[^2]: [[raw/Ropes paper#Undo]]"), "{rope}");
    // A new concept page under the contract's shape.
    let sharing = std::fs::read_to_string(root.join("wiki/concepts/Structural sharing.md")).unwrap();
    assert!(sharing.starts_with("---\ntype: concept\nsources:\n  - \"[[raw/Ropes paper]]\"\n"), "{sharing}");
    assert!(sharing.contains("Reusing unchanged parts of a structure across versions."));
    // The index, between the markers only.
    let index = std::fs::read_to_string(root.join("wiki/index.md")).unwrap();
    assert!(
        index.starts_with("# My wiki\n\nHand-written intro.\n\n<!-- quire:index:start -->\n")
            && index.ends_with("<!-- quire:index:end -->\n\nHand-written footer.\n"),
        "{index}"
    );
    // "Ropes paper" is also the raw note's name, so the link names its folder.
    assert!(
        index.contains("## Sources\n\n- [[wiki/sources/Ropes paper|Ropes paper]] — Ropes are binary trees of short strings.\n"),
        "{index}"
    );
    assert!(index.contains("- [[Structural sharing]] — Reusing unchanged parts of a structure across versions.\n"), "{index}");
    // The log.
    let log = std::fs::read_to_string(root.join("wiki/log.md")).unwrap();
    let run = report.run.clone().unwrap();
    assert!(log.contains(&format!("- 2026-11-03 14:05 · ingest · [[raw/Ropes paper]] → 3 pages · Scripted (run {run})")), "{log}");
    // The person's note: a suggestion, not an edit.
    assert_eq!(
        std::fs::read_to_string(root.join("Editors.md")).unwrap(),
        "# Editors\n\nMy editor uses structural sharing for its history.\n"
    );
    let proposals = agent.proposals(&ProposalFilter::default());
    assert_eq!(proposals.len(), 1);
    assert_eq!(report.proposals, vec![proposals[0].id.clone()]);
    assert_eq!(report.created.len(), 2);
    assert_eq!(report.updated.len(), 1);

    // Revert run: back to exactly what was there.
    let plan = agent.revert_plan(&RevertSelector { run: Some(run), ..RevertSelector::default() });
    let r = agent.revert_apply(&plan, "Revert the ingest");
    assert!(r.failed.is_empty() && r.proposal.is_none(), "{r:?}");
    assert_eq!(snapshot(root), before, "byte for byte");
}

#[test]
fn estimates_and_bad_replies() {
    let e = estimate(SOURCE);
    assert_eq!(e.parts, 1);
    assert_eq!(e.calls, 10);
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join("raw")).unwrap();
    std::fs::write(tmp.path().join("raw/S.md"), SOURCE).unwrap();
    let vault = Vault::open_local(tmp.path()).unwrap();
    let agent = AgentStore::open(&vault, DeviceInfo { id: "d_test".into(), name: "Test".into() }, Arc::new(SystemClock));
    let model = ScriptedChat::texts(["no json here", "still none"]);
    let actor = Actor::agent("panel:x", "Ingest", Via::App, "d_test");
    let job = Ingest {
        model: &model,
        agent: &agent,
        actor: &actor,
        source: "raw/S.md".into(),
        today: "2026-11-03".into(),
        now: "2026-11-03 14:05".into(),
    };
    let r = ingest(&job, &CancelFlag::new(), &mut |_| {});
    assert!(r.is_err_and(|e| e.contains("expected format")));
    assert_eq!(vault.notes().unwrap().len(), 1, "nothing written when the model fails");
}

/// A vault with `raw/Ropes paper.md`, `Journal/` locked and the policy
/// `policy` (approved on this device).
#[expect(clippy::unwrap_used, reason = "a test helper: any failure should fail the test")]
fn guarded_vault(root: &Path, policy: &str) -> (Vault, AgentStore) {
    std::fs::create_dir_all(root.join("raw")).unwrap();
    std::fs::create_dir_all(root.join("wiki")).unwrap();
    std::fs::create_dir_all(root.join("Journal")).unwrap();
    std::fs::create_dir_all(root.join(".quire/shared")).unwrap();
    std::fs::write(root.join("raw/Ropes paper.md"), SOURCE).unwrap();
    std::fs::write(root.join("wiki/Ropes.md"), "# Ropes\nzqx-wiki-text\n").unwrap();
    std::fs::write(root.join("Journal/Day.md"), "# Day\nzqx-diary-text\n").unwrap();
    std::fs::write(root.join(".quire/shared/agent-policy.toml"), policy).unwrap();
    let vault = Vault::open_local(root).unwrap();
    quire_app::vault::lock::lock_folder(&vault, "Journal", Some("correct horse"), quire_app::vault::lock::Kdf::TEST, 1).unwrap();
    let agent = AgentStore::open(&vault, DeviceInfo { id: "d_test".into(), name: "Test".into() }, Arc::new(SystemClock));
    agent.set_state_dir(Some(root.parent().unwrap().join("state")));
    let shown = agent.policy_state().file_hash();
    agent.approve_policy(shown.as_ref()).unwrap();
    (vault, agent)
}

/// Security review S-M1: with AI off, a revoked client, or a source the
/// client may not read, ingest stops before any model call.
#[test]
fn ingest_sends_nothing_the_client_may_not_read() {
    for (policy, why) in [
        ("format = 1\nai = \"off\"\n", "AI is off"),
        ("format = 1\n[clients.\"panel:anthropic\"]\nrevoked = true\n", "revoked"),
        ("format = 1\n[[zone]]\npath = \"raw/\"\nrole = \"private\"\nmode = \"deny\"\n", "isn't sent"),
        ("format = 1\n[clients.\"panel:anthropic\"]\nallow_folders = [\"wiki/\"]\n", "isn't sent"),
    ] {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("vault");
        let (_vault, agent) = guarded_vault(&root, policy);
        // A model with no answers: any call would fail with another error.
        let model = ScriptedChat::texts(Vec::<String>::new());
        let actor = Actor::agent("panel:anthropic", "Ingest", Via::App, "d_test");
        let job = Ingest {
            model: &model,
            agent: &agent,
            actor: &actor,
            source: "raw/Ropes paper.md".into(),
            today: "2026-11-03".into(),
            now: "2026-11-03 10:00".into(),
        };
        let e = ingest(&job, &CancelFlag::new(), &mut |_| {}).unwrap_err();
        assert!(e.contains(why), "{policy}: {e}");
    }
}

/// Security review S-H3: the agent panel's context (open note, selection,
/// mentions) goes through the policy check: revoke and AI off refuse, allow
/// lists, deny zones and locked folders leave notes out, and mentions are
/// read without keys.
#[test]
fn the_agent_panel_sends_only_what_the_client_may_read() {
    use quire_assistant::runner::{panel_actor, panel_context};
    let actor = panel_actor("anthropic", "Agent panel", Some("m"), "d_test");
    let no_open = |_: &str| None;
    for (policy, refused) in
        [("format = 1\nai = \"off\"\n", "AI is off"), ("format = 1\n[clients.\"panel:anthropic\"]\nrevoked = true\n", "revoked")]
    {
        let tmp = tempfile::tempdir().unwrap();
        let (_v, agent) = guarded_vault(&tmp.path().join("vault"), policy);
        let e = panel_context(&agent, &actor, Some("wiki/Ropes.md"), Some("sel"), &[], true, &no_open).unwrap_err();
        assert!(e.contains(refused), "{e}");
    }
    let tmp = tempfile::tempdir().unwrap();
    let (vault, agent) = guarded_vault(
        &tmp.path().join("vault"),
        "format = 1\ndefault = \"read\"\n[clients.\"panel:anthropic\"]\nallow_folders = [\"wiki/\", \"Journal/\"]\n",
    );
    // The person's vault holds the key; the panel's reads don't.
    vault.unlock("Journal", "correct horse").unwrap();
    let cx =
        panel_context(&agent, &actor, Some("raw/Ropes paper.md"), Some("Ropes are"), &["wiki/Ropes.md", "Journal/Day.md"], true, &no_open)
            .unwrap();
    assert_eq!(cx.note, None, "outside the allow list");
    assert_eq!(cx.selection, None, "the selection goes with its note");
    assert_eq!(cx.mentions, ["wiki/Ropes.md"]);
    assert!(cx.texts[0].contains("zqx-wiki-text"));
    assert_eq!(cx.left_out, ["raw/Ropes paper.md", "Journal/Day.md"]);
    let open = |p: &str| (p == "wiki/Ropes.md").then(|| "typed, unsaved".to_string());
    let cx = panel_context(&agent, &actor, Some("wiki/Ropes.md"), Some("sel"), &[], true, &open).unwrap();
    assert_eq!(
        (cx.note.as_deref(), cx.selection.as_deref(), cx.note_text.as_deref()),
        (Some("wiki/Ropes.md"), Some("sel"), Some("typed, unsaved"))
    );
}
