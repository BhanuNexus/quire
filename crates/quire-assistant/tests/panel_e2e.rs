// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The agent panel end to end (integration wave 2): an OpenAI-compatible
//! server on loopback answers with tool calls (search, view, `str_replace`),
//! the run ends in a suggestion, the Inbox lists it, the person reviews and
//! accepts it in the note, history records who made the version, Agent
//! activity shows the run, and "Revert run" restores the note.
#![expect(clippy::unwrap_used, reason = "a test: any failure should fail it")]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::mpsc;

use quire_agent::RevertSelector;
use quire_app::intel::chat::{CancelFlag, ChatMessage};
use quire_app::intel::llm::OpenAiCompatible;
use quire_app::vault::{NotePath, Vault};
use quire_app::{Settings, Workspace};
use quire_assistant::runner::{self, Capability, RunEvent, RunLimits, RunSpec, ToolHost};
use serde_json::{Value, json};

const ROPES: &str = "# Rope Notes\n\nRopes make undo cheap.\nThey are trees.\n";
const BETTER: &str = "Ropes make undo cheap because subtrees are shared.";

/// A chat completion that calls one tool.
fn call(id: &str, name: &str, args: &Value) -> String {
    json!({
        "choices": [{"index": 0, "finish_reason": "tool_calls", "message": {"role": "assistant", "content": null,
            "tool_calls": [{"id": id, "type": "function", "function": {"name": name, "arguments": args.to_string()}}]}}],
        "usage": {"prompt_tokens": 120, "completion_tokens": 12}
    })
    .to_string()
}

fn text(t: &str) -> String {
    json!({
        "choices": [{"index": 0, "finish_reason": "stop", "message": {"role": "assistant", "content": t}}],
        "usage": {"prompt_tokens": 200, "completion_tokens": 9}
    })
    .to_string()
}

/// A loopback server answering each request with the next JSON body (a
/// server that doesn't stream). Sends each request's body.
fn server(replies: Vec<String>) -> (String, mpsc::Receiver<Value>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = format!("http://{}", listener.local_addr().unwrap());
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for (stream, reply) in listener.incoming().zip(replies) {
            let mut s = stream.unwrap();
            let mut r = BufReader::new(s.try_clone().unwrap());
            let mut line = String::new();
            r.read_line(&mut line).unwrap();
            let mut len = 0;
            loop {
                let mut h = String::new();
                r.read_line(&mut h).unwrap();
                if h == "\r\n" || h.is_empty() {
                    break;
                }
                if let Some(v) = h.to_ascii_lowercase().strip_prefix("content-length:") {
                    len = v.trim().parse().unwrap();
                }
            }
            let mut body = vec![0; len];
            r.read_exact(&mut body).unwrap();
            let _ = tx.send(serde_json::from_slice(&body).unwrap_or(Value::Null));
            let _ = write!(
                s,
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{reply}",
                reply.len()
            );
        }
    });
    (addr, rx)
}

#[test]
fn the_panel_searches_reads_proposes_and_the_person_reviews_and_reverts() {
    let path = NotePath::new("Rope Notes.md");
    let mut ws = Workspace::new(Settings::default());
    ws.attach_vault(Vault::in_memory(&[(path.as_str(), ROPES), ("Bread.md", "# Bread\nFlour and water.\n")]));
    let vault = ws.vault().unwrap().clone();
    let store = ws.agent_store().unwrap().clone();

    // The model: search, view, then a precise edit, then a sentence.
    let (base, requests) = server(vec![
        call("c1", "search", &json!({"query": "ropes undo"})),
        call("c2", "view", &json!({"path": path.as_str()})),
        call("c3", "str_replace", &json!({"path": path.as_str(), "old_str": "Ropes make undo cheap.", "new_str": BETTER})),
        text("I suggested a clearer sentence; review it in the Inbox."),
    ]);
    let model = OpenAiCompatible::new(base, "mock-model", None);
    let actor = runner::panel_actor("openai", "Agent panel (mock-model)", Some("mock-model"), &ws.device().id);
    runner::grant(&store, &actor, Capability::Full);
    let host = ToolHost::open(vault.clone(), None, store.clone());
    let spec = RunSpec {
        model: &model,
        host: Some(&host),
        actor: &actor,
        capability: Capability::Full,
        core_tools: false,
        read_only: false,
        limits: RunLimits::default(),
        system: runner::system_for(&store, true, true),
        max_reply_tokens: 1000,
    };
    let mut messages = vec![ChatMessage::user("Make the rope note clearer")];
    let mut events = Vec::new();
    runner::run(&spec, &mut messages, &CancelFlag::new(), &mut |e| events.push(e));

    // Four requests; the tools went out with the first and results came back.
    let bodies: Vec<Value> = requests.try_iter().collect();
    assert_eq!(bodies.len(), 4, "{events:#?}");
    assert!(bodies[0]["tools"].as_array().is_some_and(|t| t.iter().any(|t| t["function"]["name"] == "str_replace")));
    assert!(bodies[3]["messages"].to_string().contains("Rope Notes"), "tool results reach the model");
    let proposal = events
        .iter()
        .find_map(|e| if let RunEvent::Proposed { proposal, .. } = e { quire_agent::ProposalId::parse(proposal) } else { None })
        .expect("the edit becomes a suggestion");
    assert_eq!(vault.read(&path).unwrap(), ROPES, "nothing is written to the person's note");

    // The Inbox shows it under the panel's name.
    let inbox = quire_app::agent_state::inbox::load(&store, false, false);
    assert!(inbox.pending.iter().any(|p| p.id == proposal), "{:?}", inbox.pending);
    // Activity: one run of the panel that searched, read and proposed.
    let runs = quire_app::agent_state::activity::load(&store, 1);
    let run = runs.iter().find(|r| r.actor_id == "panel:openai").expect("the panel's run");
    assert_eq!(run.proposals(), vec![proposal.clone()]);

    // Review in the note and accept every change.
    let doc = ws.open_note(&path).unwrap();
    assert!(ws.start_review(&proposal, Some(&path)));
    ws.review_all_in_file(true);
    assert!(ws.doc(doc).unwrap().text().contains(BETTER));
    assert!(store.proposal(&proposal).is_none(), "settled");
    ws.save_all();
    ws.flush_history();
    let accepted = vault.read(&path).unwrap();
    assert!(accepted.contains(BETTER));
    // History: the person's version, accepted from the panel.
    let h = vault.history(&path);
    let prov = quire_agent::Provenance::from_json(&vault.read_provenance(&h[0]).unwrap()).unwrap();
    assert!(prov.actor.is_human(), "{prov:?}");
    assert_eq!(prov.accepted_from.as_ref().map(|a| a.id.as_str()), Some("panel:openai"));
    // Activity has the acceptance too.
    let log = store.log(&quire_agent::LedgerQuery::default());
    assert!(log.iter().any(|e| e.from.as_deref() == Some("panel:openai")), "{log:#?}");

    // Revert run: the note is back to what it was before the panel's run.
    let plan = store.revert_plan(&RevertSelector { actor: Some("panel:openai".into()), ..RevertSelector::default() });
    assert!(plan.note.is_none() && !plan.steps.is_empty(), "{plan:#?}");
    let report = store.revert_apply(&plan, "Revert the agent panel's run");
    assert!(report.failed.is_empty(), "{report:?}");
    if let Some(p) = report.proposal {
        // Undos that need review are a suggestion; accept it as the person.
        assert!(ws.start_review(&p, Some(&path)));
        ws.review_all_in_file(true);
        ws.save_all();
    }
    assert_eq!(vault.read(&path).unwrap(), ROPES, "byte for byte");
}
