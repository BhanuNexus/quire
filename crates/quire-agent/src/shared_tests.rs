// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Review across devices (0.10): an agent writes on a headless server, the
//! person reviews on the desktop. Two in-memory vaults; `carry` does what
//! the settings lane does with one device's files.

use std::fmt::Write as _;
use std::sync::Arc;

use quire_vault::{NotePath, Vault};

use crate::shared::{DECISIONS_DIR, DEVICES_DIR, PROPOSALS_DIR, SHARED_LEDGER_DIR, TEXTS_DIR};
use crate::*;

const T0: i64 = 1_793_000_000_000;
const NOTE: &str = "# Launch\nThe venue is booked.\nWe ship on Monday.\nBring snacks.\n";
const AGENT: &str = "# Launch\nThe venue is booked for Friday.\nWe ship on Monday.\nBring snacks and water.\n";

struct Dev {
    vault: Vault,
    store: AgentStore,
    clock: Arc<FixedClock>,
}

fn dev(id: &str, name: &str, files: &[(&str, &str)]) -> Dev {
    let vault = Vault::in_memory(files);
    let clock = FixedClock::new(T0);
    let store = AgentStore::open(&vault, DeviceInfo { id: id.into(), name: name.into() }, Arc::<FixedClock>::clone(&clock));
    Dev { vault, store, clock }
}

fn server() -> Dev {
    dev("d_server", "home server", &[("Launch.md", NOTE), ("wiki/Plan.md", "# Plan\none\ntwo\nthree\nfour\n")])
}

fn desktop() -> Dev {
    dev("d_desk", "Asha's laptop", &[("Launch.md", NOTE), ("wiki/Plan.md", "# Plan\none\ntwo\nthree\nfour\n")])
}

fn agent() -> Actor {
    Actor::agent("oauth:claude.ai", "Claude", Via::McpHttp, "d_server")
}

/// The settings lane: `from`'s own files under `.quire/shared/agent/`
/// become the same on `to` (added, changed, removed).
fn carry(from: &Dev, to: &Dev) {
    let id = &from.store.device().id;
    let (a, b) = (from.vault.storage(), to.vault.storage());
    let dev_file = NotePath::new(format!("{DEVICES_DIR}/{id}.json"));
    if let Ok(bytes) = a.read_bytes(&dev_file) {
        b.write_bytes_atomic(&dev_file, &bytes).unwrap();
    }
    for dir in [PROPOSALS_DIR, DECISIONS_DIR, SHARED_LEDGER_DIR, TEXTS_DIR] {
        let folder = format!("{dir}/{id}");
        let theirs = a.list_under(&folder).unwrap_or_default();
        for f in b.list_under(&folder).unwrap_or_default() {
            if !theirs.contains(&f) {
                b.remove(&f).unwrap();
            }
        }
        for f in theirs {
            b.write_bytes_atomic(&f, &a.read_bytes(&f).unwrap()).unwrap();
        }
    }
}

/// Both devices publish, exchange and take in.
fn exchange(x: &Dev, y: &Dev) {
    x.store.export_shared();
    y.store.export_shared();
    carry(x, y);
    carry(y, x);
    x.store.import_shared();
    y.store.import_shared();
}

fn read(d: &Dev, p: &str) -> String {
    d.vault.read(&NotePath::new(p)).unwrap()
}

fn decision(h: &Hunk, action: HunkAction) -> HunkDecision {
    HunkDecision { hunk: h.id.clone(), action, at: 0, text: None, other: serde_json::Map::new() }
}

fn propose(s: &Dev, text: &str) -> ProposalId {
    let cs = ChangeSet {
        run: Some(RunId::new(T0)),
        title: "Launch details".into(),
        ..ChangeSet::one(Change::Edit { path: "Launch.md".into(), new_text: text.into() })
    };
    let SubmitOutcome::Proposed { id, .. } = s.store.submit(&agent(), cs).unwrap() else { panic!("expected a proposal") };
    id
}

/// What a proposal came to, wherever it is (waiting or archived).
fn outcome(d: &Dev, id: &ProposalId) -> (ProposalStatus, Vec<Option<ChangeState>>) {
    let p = d.store.proposal(id).or_else(|| d.store.archived_proposals().into_iter().find(|p| &p.id == id)).unwrap();
    (p.status, p.changes.iter().map(|c| c.state).collect())
}

#[test]
fn a_suggestion_made_on_the_server_is_reviewed_on_the_desktop() {
    let (s, d) = (server(), desktop());
    let id = propose(&s, AGENT);
    // Shared state off: nothing leaves either device.
    carry(&s, &d);
    assert!(d.store.proposals(&ProposalFilter::pending()).is_empty());
    // On: the desktop gets it, from the server.
    s.store.export_shared();
    carry(&s, &d);
    let r = d.store.import_shared();
    assert_eq!(r.added, 1, "{r:?}");
    let pending = d.store.proposals(&ProposalFilter::pending());
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].from_device, Some(("d_server".into(), "home server".into())));
    assert_eq!(d.store.device_names().get("d_server").map(String::as_str), Some("home server"));
    // Importing again changes nothing.
    assert!(!d.store.import_shared().changed_anything());

    // Review on the desktop: accept one hunk, reject the other.
    let path = NotePath::new("Launch.md");
    let hunks = d.store.review(&id, &path, NOTE).hunks;
    assert_eq!(hunks.len(), 2);
    d.clock.advance(1_000);
    d.store.accept_in_file(&id, &path, &[decision(&hunks[0], HunkAction::Accepted)], None).unwrap();
    let after = read(&d, "Launch.md");
    assert!(after.contains("for Friday") && !after.contains("water"), "{after}");
    let left = d.store.review(&id, &path, &after).hunks;
    assert_eq!(left.len(), 1);
    d.store.decide_checked(&id, &path, &[decision(&left[0], HunkAction::Rejected)], &after, None).unwrap();
    assert_eq!(outcome(&d, &id).0, ProposalStatus::PartiallyApplied);

    // The server learns it: resolved, and the note there untouched (it
    // arrives by note sync; the decision itself never writes a note).
    d.store.export_shared();
    carry(&d, &s);
    let r = s.store.import_shared();
    assert_eq!((r.decided, r.settled), (1, 1), "{r:?}");
    assert_eq!(outcome(&s, &id), outcome(&d, &id));
    assert_eq!(read(&s, "Launch.md"), NOTE, "never applied by the import");
    assert!(s.store.proposals(&ProposalFilter::pending()).is_empty());
    // Taking the decisions in again changes nothing.
    assert!(!s.store.import_shared().changed_anything());

    // The rejection is kept as content on the server.
    let p = s.store.archived_proposals().into_iter().find(|p| p.id == id).unwrap();
    assert!(p.changes[0].decided.iter().any(|x| x.action == HunkAction::Rejected && x.new == ["bring snacks and water."]), "{p:?}");

    // Each device's activity shows the other's events.
    let desk_log = d.store.log(&LedgerQuery::default());
    assert!(desk_log.iter().any(|e| e.op == Op::Propose && e.actor.device == "d_server"), "{desk_log:?}");
    s.store.export_shared();
    d.store.export_shared();
    carry(&d, &s);
    let srv_log = s.store.log(&LedgerQuery::default());
    assert!(srv_log.iter().any(|e| e.op == Op::Accept && e.actor.device == "d_desk"), "{srv_log:?}");
    assert!(srv_log.iter().all(|e| e.op != Op::Read));
}

#[test]
fn both_devices_deciding_at_once_converge_and_apply_text_once() {
    let (s, d) = (server(), desktop());
    let id = propose(&s, AGENT);
    exchange(&s, &d);
    let path = NotePath::new("Launch.md");
    let (hs, hd) = (s.store.review(&id, &path, NOTE).hunks, d.store.review(&id, &path, NOTE).hunks);
    assert_eq!(hs.iter().map(|h| &h.id).collect::<Vec<_>>(), hd.iter().map(|h| &h.id).collect::<Vec<_>>(), "same ids on both");
    // At the same moment: the desktop accepts the first and rejects the
    // second; the server (the CLI there) rejects the first and accepts the second.
    s.clock.advance(5_000);
    d.clock.advance(5_000);
    d.store.accept_in_file(&id, &path, &[decision(&hd[0], HunkAction::Accepted)], None).unwrap();
    let dt = read(&d, "Launch.md");
    d.store.decide_checked(&id, &path, &[decision(&d.store.review(&id, &path, &dt).hunks[0], HunkAction::Rejected)], &dt, None).unwrap();
    s.store.decide_checked(&id, &path, &[decision(&hs[0], HunkAction::Rejected)], NOTE, None).unwrap();
    s.store.accept_in_file(&id, &path, &[decision(&s.store.review(&id, &path, NOTE).hunks[0], HunkAction::Accepted)], None).unwrap();
    exchange(&s, &d);
    exchange(&s, &d);
    // Same outcome on both: each hunk was accepted somewhere, so its text is in the note.
    assert_eq!(outcome(&s, &id), outcome(&d, &id));
    assert_eq!(outcome(&s, &id), (ProposalStatus::Applied, vec![Some(ChangeState::Applied)]));
    // The notes, merged by sync: each accepted line once.
    let merged = quire_text::merge::merge3(NOTE, &read(&d, "Launch.md"), &read(&s, "Launch.md"));
    assert!(merged.is_clean());
    assert_eq!(merged.text, AGENT);

    // Both accept the same hunk at once: the same edit on both, taken once.
    let (s, d) = (server(), desktop());
    let id = propose(&s, AGENT);
    exchange(&s, &d);
    for x in [&s, &d] {
        let h = x.store.review(&id, &path, NOTE).hunks;
        x.store.accept_in_file(&id, &path, &[decision(&h[1], HunkAction::Accepted)], None).unwrap();
    }
    exchange(&s, &d);
    assert_eq!(outcome(&s, &id), outcome(&d, &id));
    let merged = quire_text::merge::merge3(NOTE, &read(&d, "Launch.md"), &read(&s, "Launch.md"));
    assert_eq!(merged.text.matches("and water").count(), 1, "{}", merged.text);
    // And once the note has synced, nothing is left to review anywhere.
    for x in [&s, &d] {
        x.vault.write(&path, &merged.text).unwrap();
        assert!(x.store.review(&id, &path, &merged.text).hunks.iter().all(|h| !h.new_lines.concat().contains("water")));
    }
}

#[test]
fn a_rejection_on_the_desktop_holds_when_the_server_agent_rewrites_around_it() {
    let (s, d) = (server(), desktop());
    let id = propose(&s, AGENT);
    exchange(&s, &d);
    let path = NotePath::new("Launch.md");
    let h = d.store.review(&id, &path, NOTE).hunks;
    d.clock.advance(1_000);
    d.store.decide_checked(&id, &path, &[decision(&h[1], HunkAction::Rejected)], NOTE, None).unwrap();
    exchange(&s, &d);
    let shown = |x: &Dev| -> Vec<String> { x.store.review(&id, &path, NOTE).hunks.iter().map(|h| h.new_lines.concat()).collect() };
    assert!(shown(&s).iter().all(|l| !l.contains("water")), "hidden on the server too: {:?}", shown(&s));
    // The agent on the server, unaware, writes again: the rejected line
    // reworded (case, spacing, a full stop), the line next to it rewritten
    // (so both are one hunk), and more lines around.
    s.clock.advance(1_000);
    let again = "# Launch\nThe venue is booked for Friday evening.\nWe ship on Tuesday.\nbring  snacks and Water\nSee the checklist.\n";
    assert_eq!(propose(&s, again), id, "amends its proposal");
    let srv = shown(&s);
    assert!(srv.iter().all(|l| !l.to_lowercase().contains("water")), "{srv:?}");
    assert!(srv.iter().any(|l| l.contains("evening")), "{srv:?}");
    assert!(srv.iter().any(|l| l.contains("Tuesday")) && srv.iter().any(|l| l.contains("checklist")), "the rest is kept: {srv:?}");
    exchange(&s, &d);
    let desk = shown(&d);
    assert!(desk.iter().all(|l| !l.to_lowercase().contains("water")), "{desk:?}");
    assert!(desk.iter().any(|l| l.contains("evening")), "the new suggestion reaches the desktop: {desk:?}");
    // Something different at the same place is asked about.
    s.clock.advance(1_000);
    propose(&s, "# Launch\nThe venue is booked for Friday evening.\nWe ship on Monday.\nBring a first-aid kit.\nSee the checklist.\n");
    assert!(shown(&s).iter().any(|l| l.contains("first-aid")));
}

#[test]
fn a_run_on_the_server_is_undone_on_the_desktop_keeping_human_edits() {
    let (s, d) = (server(), desktop());
    let run = RunId::new(T0);
    let step = |text: &str| ChangeSet {
        run: Some(run.clone()),
        ..ChangeSet::one(Change::Edit { path: "wiki/Plan.md".into(), new_text: text.into() })
    };
    // Two writes in one run between syncs: the desktop only ever sees the second.
    s.store.submit(&agent(), step("# Plan\nONE\ntwo\nthree\nfour\n")).unwrap();
    s.clock.advance(1_000);
    s.store.submit(&agent(), step("# Plan\nONE\ntwo\nTHREE\nfour\n")).unwrap();
    let synced = read(&s, "wiki/Plan.md");
    d.vault.write(&NotePath::new("wiki/Plan.md"), &synced).unwrap();
    // The person then edits another line on the desktop.
    d.clock.advance(60_000);
    d.vault.write(&NotePath::new("wiki/Plan.md"), "# Plan\nONE\ntwo\nTHREE\nfour, mine\n").unwrap();
    assert!(d.vault.history(&NotePath::new("wiki/Plan.md")).is_empty(), "no local versions to undo from");
    // Without the shared history the desktop can't undo it.
    let sel = RevertSelector { run: Some(run), ..RevertSelector::default() };
    assert!(d.store.revert_plan(&sel).steps.is_empty());
    exchange(&s, &d);
    let plan = d.store.revert_plan(&sel);
    assert_eq!(plan.events.len(), 2, "{plan:?}");
    assert_eq!(plan.steps.len(), 1, "{plan:?}");
    let report = d.store.revert_apply(&plan, "Undo Claude's run");
    assert!(report.failed.is_empty() && report.proposal.is_none(), "{report:?}");
    assert_eq!(read(&d, "wiki/Plan.md"), "# Plan\none\ntwo\nthree\nfour, mine\n", "the agent's lines undone, the person's kept");

    // Overlapping edits: the undo becomes a suggestion to review, the note untouched.
    let (s, d) = (server(), desktop());
    let run = RunId::new(T0 + 1);
    s.store
        .submit(
            &agent(),
            ChangeSet {
                run: Some(run.clone()),
                ..ChangeSet::one(Change::Edit { path: "wiki/Plan.md".into(), new_text: "# Plan\nONE\ntwo\nthree\nfour\n".into() })
            },
        )
        .unwrap();
    d.vault.write(&NotePath::new("wiki/Plan.md"), "# Plan\nONE, then mine\ntwo\nthree\nfour\n").unwrap();
    exchange(&s, &d);
    let plan = d.store.revert_plan(&RevertSelector { run: Some(run), ..RevertSelector::default() });
    let report = d.store.revert_apply(&plan, "Undo");
    assert!(report.proposal.is_some(), "{report:?}");
    assert_eq!(read(&d, "wiki/Plan.md"), "# Plan\nONE, then mine\ntwo\nthree\nfour\n");
}

#[test]
fn files_from_newer_versions_and_bad_names_are_left_alone() {
    let (s, d) = (server(), desktop());
    let id = propose(&s, AGENT);
    s.store.export_shared();
    carry(&s, &d);
    // A newer format, a proposal under the wrong device, a device id that isn't one.
    let file = NotePath::new(format!("{PROPOSALS_DIR}/d_server/{id}.json"));
    let mut v: serde_json::Value = serde_json::from_slice(&d.vault.storage().read_bytes(&file).unwrap()).unwrap();
    v["format"] = 2.into();
    d.vault.storage().write_bytes_atomic(&file, &serde_json::to_vec(&v).unwrap()).unwrap();
    d.vault.storage().write_atomic(&NotePath::new(format!("{DEVICES_DIR}/..%2F.json")), "{\"format\":1,\"id\":\"..\"}").unwrap();
    assert!(!d.store.import_shared().changed_anything());
    assert!(d.store.proposals(&ProposalFilter::default()).is_empty());
    let mut v2 = v.clone();
    v2["format"] = 1.into();
    d.vault
        .storage()
        .write_bytes_atomic(&NotePath::new(format!("{PROPOSALS_DIR}/d_other/{id}.json")), &serde_json::to_vec(&v2).unwrap())
        .unwrap();
    d.vault
        .storage()
        .write_atomic(&NotePath::new(format!("{DEVICES_DIR}/d_other.json")), "{\"format\":1,\"id\":\"d_other\",\"name\":\"x\"}")
        .unwrap();
    assert!(!d.store.import_shared().changed_anything(), "only the device that made a proposal publishes it");
}

#[test]
fn big_proposals_are_packed_and_old_history_leaves() {
    let big_note = "line of the note\n".repeat(15_000);
    let s = dev("d_server", "home server", &[("Big.md", &big_note)]);
    let d = dev("d_desk", "desk", &[("Big.md", &big_note)]);
    let changed = big_note.replacen("line of the note\n", "the first line, changed\n", 1);
    let cs = ChangeSet::one(Change::Edit { path: "Big.md".into(), new_text: changed });
    let SubmitOutcome::Proposed { id, .. } = s.store.submit(&agent(), cs).unwrap() else { panic!() };
    s.store.export_shared();
    let file = NotePath::new(format!("{PROPOSALS_DIR}/d_server/{id}.json"));
    let bytes = s.vault.storage().read_bytes(&file).unwrap();
    assert!(bytes.len() < 64 * 1024, "packed: {} bytes", bytes.len());
    assert!(String::from_utf8_lossy(&bytes).contains("\"packed\""));
    carry(&s, &d);
    assert_eq!(d.store.import_shared().added, 1);
    assert_eq!(d.store.review(&id, &"Big.md".into(), &big_note).hunks.len(), 1);

    // A proposal too big to share (made by a revert of big notes, say;
    // agents' writes are capped well below) stays on its device.
    let mut x: u64 = 0x2545_F491_4F6C_DD1D;
    let mut huge = String::with_capacity(400_000 * 17);
    for _ in 0..400_000 {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        let _ = writeln!(huge, "{x:016x}");
    }
    let mut big = s.store.proposal(&id).unwrap();
    big.id = ProposalId::new(T0 + 5);
    big.changes = vec![proposal::ProposalChange::create(&"Huge.md".into(), &huge)];
    proposal::ProposalStore::new(s.vault.clone()).save(&big).unwrap();
    let huge_id = big.id;
    s.store.export_shared();
    assert!(!s.vault.storage().exists(&NotePath::new(format!("{PROPOSALS_DIR}/d_server/{huge_id}.json"))));
    for f in s.vault.storage().list_under(&format!("{PROPOSALS_DIR}/d_server")).unwrap() {
        assert!(s.vault.storage().read_bytes(&f).unwrap().len() <= shared::MAX_FILE);
    }

    // Resolved more than 30 days ago: no longer shared.
    s.store.resolve_proposal(&id, false, &|_| false);
    s.store.export_shared();
    assert!(s.vault.storage().exists(&file), "resolved recently: still shared");
    s.clock.advance(31 * 86_400_000);
    s.store.export_shared();
    assert!(!s.vault.storage().exists(&file));
    // The ledger keeps three months.
    s.clock.advance(80 * 86_400_000);
    s.store.record(&LedgerEntry::new(agent(), Op::Read, s.store.now_ms()).path(&"Big.md".into()));
    s.store.record(&LedgerEntry::new(agent(), Op::Propose, s.store.now_ms()).path(&"Big.md".into()));
    s.store.export_shared();
    let months = s.vault.storage().list_under(&format!("{SHARED_LEDGER_DIR}/d_server")).unwrap();
    assert_eq!(months.len(), 1, "{months:?}");
    let text = s.vault.storage().read(&months[0]).unwrap();
    assert!(!text.contains("\"op\":\"read\""), "reads stay on their device: {text}");
}

#[test]
fn a_proposal_amended_after_the_desktop_resolved_it_comes_back() {
    let (s, d) = (server(), desktop());
    let id = propose(&s, AGENT);
    exchange(&s, &d);
    let path = NotePath::new("Launch.md");
    // The desktop rejects everything; before the server hears of it, the
    // agent there adds something new to the same suggestion.
    d.store.resolve_proposal(&id, false, &|_| false);
    assert_eq!(outcome(&d, &id).0, ProposalStatus::Rejected);
    s.clock.advance(10_000);
    propose(&s, &format!("{AGENT}And a new line.\n"));
    s.store.export_shared();
    carry(&s, &d);
    d.store.import_shared();
    let back = d.store.review(&id, &path, NOTE).hunks;
    let lines: Vec<String> = back.iter().map(|h| h.new_lines.concat()).collect();
    assert!(lines.iter().any(|l| l.contains("And a new line")), "{lines:?}");
    assert!(lines.iter().all(|l| !l.contains("water") && !l.contains("for Friday")), "rejected ones stay rejected: {lines:?}");
}

#[test]
fn a_rejection_holds_when_the_agent_suggests_it_again_in_a_new_run() {
    let (s, d) = (server(), desktop());
    let id = propose(&s, AGENT);
    exchange(&s, &d);
    // The desktop accepts the venue line and rejects the water: resolved.
    let path = NotePath::new("Launch.md");
    let h = d.store.review(&id, &path, NOTE).hunks;
    d.store.accept_in_file(&id, &path, &[decision(&h[0], HunkAction::Accepted)], None).unwrap();
    let now = read(&d, "Launch.md");
    let left = d.store.review(&id, &path, &now).hunks;
    d.store.decide_checked(&id, &path, &[decision(&left[0], HunkAction::Rejected)], &now, None).unwrap();
    exchange(&s, &d);
    s.vault.write(&path, &now).unwrap();
    assert_eq!(outcome(&s, &id).0, ProposalStatus::PartiallyApplied);
    // A new run: the rejected line again (reworded), and something new.
    s.clock.advance(60_000);
    let again = now.replace("Bring snacks.", "bring snacks and Water!\nAsk about parking.");
    let second = propose(&s, &again);
    assert_ne!(second, id, "a new suggestion");
    let shown: Vec<String> = s.store.review(&second, &path, &now).hunks.iter().map(|h| h.new_lines.concat()).collect();
    assert_eq!(shown, ["Ask about parking.\n"], "only what is new");
    // Only the rejected line again: settled at once, nothing to review.
    s.clock.advance(60_000);
    let third = propose(&s, &now.replace("Bring snacks.", "Bring snacks and water."));
    let _ = third;
    let waiting: Vec<_> = s.store.proposals(&ProposalFilter::pending()).into_iter().filter(|p| p.files.contains(&path)).collect();
    assert_eq!(waiting.len(), 1, "{waiting:?}");
}
