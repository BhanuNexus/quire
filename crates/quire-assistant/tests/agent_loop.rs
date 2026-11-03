// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The panel's agent loop with a scripted model (design §6.2 WS-D
//! acceptance): a three-step tool loop (search, view, edit) ends in a
//! suggestion, not a write; the small model gets no tools; one change per
//! message for the balanced model; Ask mode changes nothing.

use std::sync::Arc;

use quire_agent::{AgentStore, DeviceInfo, ProposalFilter, SystemClock};
use quire_app::intel::chat::{CancelFlag, ChatEvent, ChatMessage, Role, ScriptedChat, StopReason, Usage};
use quire_app::vault::Vault;
use quire_assistant::runner::{Capability, RunEvent, RunLimits, RunSpec, ToolHost, panel_actor, run};
use serde_json::json;

const ROPES: &str = "# Rope Notes\n\nRopes make undo cheap.\n";

fn setup() -> (Vault, AgentStore, ToolHost) {
    let vault = Vault::in_memory(&[
        ("Rope Notes.md", ROPES),
        ("Bread.md", "# Bread\nFlour and water.\n"),
        ("wiki/Ropes.md", "# Ropes\nA tree of strings.\n"),
    ]);
    let agent = AgentStore::open(&vault, DeviceInfo { id: "d_test".into(), name: "Test".into() }, Arc::new(SystemClock));
    let host = ToolHost::open(vault.clone(), None, agent.clone());
    (vault, agent, host)
}

fn tool(id: &str, name: &str, input: serde_json::Value) -> Vec<ChatEvent> {
    vec![
        ChatEvent::ToolUse { id: id.into(), name: name.into(), input },
        ChatEvent::Done { stop: StopReason::ToolUse, usage: Usage { input_tokens: 100, output_tokens: 10 } },
    ]
}

fn spec<'a>(model: &'a ScriptedChat, host: &'a ToolHost, actor: &'a quire_agent::Actor, cap: Capability, read_only: bool) -> RunSpec<'a> {
    RunSpec {
        model,
        host: Some(host),
        actor,
        capability: cap,
        core_tools: false,
        read_only,
        limits: RunLimits::default(),
        system: "sys".into(),
        max_reply_tokens: 500,
    }
}

#[test]
fn a_three_step_tool_loop_ends_in_a_suggestion() {
    let (vault, agent, host) = setup();
    let model = ScriptedChat::new(vec![
        tool("t1", "search", json!({"query": "ropes undo"})),
        tool("t2", "view", json!({"path": "Rope Notes.md"})),
        tool(
            "t3",
            "str_replace",
            json!({"path": "Rope Notes.md", "old_str": "Ropes make undo cheap.", "new_str": "Ropes make undo cheap because subtrees are shared."}),
        ),
        vec![ChatEvent::Text { text: "I suggested a clearer sentence.".into() }],
    ]);
    let actor = panel_actor("anthropic", "Agent panel", Some("claude-sonnet-5-5"), "d_test");
    let mut conv = vec![ChatMessage::user("Make the ropes note clearer")];
    let mut events = Vec::new();
    run(&spec(&model, &host, &actor, Capability::Full, false), &mut conv, &CancelFlag::new(), &mut |e| events.push(e));

    let summaries: Vec<String> =
        events.iter().filter_map(|e| if let RunEvent::ToolFinished { summary, .. } = e { Some(summary.clone()) } else { None }).collect();
    assert_eq!(summaries.len(), 3, "{events:#?}");
    assert!(summaries[0].starts_with("Searched notes for *ropes undo*"), "{summaries:?}");
    assert!(summaries[1].starts_with("Read *Rope Notes*"), "{summaries:?}");
    assert_eq!(summaries[2], "Suggested a change to *Rope Notes* · review it in the Inbox");
    let proposal = events
        .iter()
        .find_map(|e| if let RunEvent::Proposed { proposal, path } = e { Some((proposal.clone(), path.clone())) } else { None })
        .expect("a proposal card");
    assert_eq!(proposal.1, "Rope Notes.md");
    assert!(matches!(events.last(), Some(RunEvent::Done { steps: 4, .. })), "{:?}", events.last());

    assert_eq!(vault.read(&"Rope Notes.md".into()).unwrap(), ROPES, "the person's note is untouched");
    let pending = agent.proposals(&ProposalFilter::default());
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].id.to_string(), proposal.0);
    assert_eq!(pending[0].actor.id, "panel:anthropic");

    // The model saw each tool's result before its next turn.
    let reqs = model.requests();
    assert_eq!(reqs.len(), 4);
    assert!(
        reqs[0].tools.iter().any(|t| t.name == "str_replace") && reqs[0].tools.iter().all(|t| t.name != "capture_url"),
        "write tools, no network"
    );
    let last = reqs[3].messages.last().unwrap();
    assert_eq!(last.role, Role::Tool);
    assert_eq!(conv.len(), 8, "user, 3 × (call, result), answer");
    assert_eq!(conv.last().unwrap().text(), "I suggested a clearer sentence.");
}

#[test]
fn small_models_chat_and_balanced_ones_make_one_change() {
    let (vault, _agent, host) = setup();
    let actor = panel_actor("device", "Agent panel", None, "d_test");
    let model = ScriptedChat::texts(["Bread needs flour."]);
    let mut conv = vec![ChatMessage::user("hi")];
    run(&spec(&model, &host, &actor, Capability::ChatOnly, false), &mut conv, &CancelFlag::new(), &mut |_| {});
    assert!(model.requests()[0].tools.is_empty(), "no tools for the small model");

    let model = ScriptedChat::new(vec![
        vec![
            ChatEvent::ToolUse { id: "a".into(), name: "append".into(), input: json!({"path": "wiki/Ropes.md", "text": "One."}) },
            ChatEvent::ToolUse { id: "b".into(), name: "append".into(), input: json!({"path": "wiki/Ropes.md", "text": "Two."}) },
        ],
        vec![ChatEvent::Text { text: "Done.".into() }],
    ]);
    let mut events = Vec::new();
    run(
        &spec(&model, &host, &actor, Capability::ReadOneWrite, false),
        &mut vec![ChatMessage::user("add two lines")],
        &CancelFlag::new(),
        &mut |e| events.push(e),
    );
    let errors: Vec<bool> =
        events.iter().filter_map(|e| if let RunEvent::ToolFinished { is_error, .. } = e { Some(*is_error) } else { None }).collect();
    assert_eq!(errors, [false, true], "the second change is refused");
    let text = vault.read(&"wiki/Ropes.md".into()).unwrap();
    assert!(text.contains("One.") && !text.contains("Two."), "{text}");

    // Ask mode: no write tools offered, and a write that slips through is refused.
    let model = ScriptedChat::new(vec![
        tool("w", "append", json!({"path": "wiki/Ropes.md", "text": "Three."})),
        vec![ChatEvent::Text { text: "ok".into() }],
    ]);
    run(&spec(&model, &host, &actor, Capability::Full, true), &mut vec![ChatMessage::user("x")], &CancelFlag::new(), &mut |_| {});
    assert!(model.requests()[0].tools.iter().all(|t| t.name != "append" && t.name != "create"));
    assert!(!vault.read(&"wiki/Ropes.md".into()).unwrap().contains("Three."));
}

#[test]
fn the_step_cap_ends_a_run_that_keeps_calling_tools() {
    let (_vault, _agent, host) = setup();
    let actor = panel_actor("openai", "Agent panel", None, "d_test");
    let model = ScriptedChat::new((0..5).map(|i| tool(&format!("t{i}"), "list", json!({}))).collect());
    let mut events = Vec::new();
    let mut s = spec(&model, &host, &actor, Capability::Full, false);
    s.limits = RunLimits { max_steps: 3, max_tokens: 0 };
    run(&s, &mut vec![ChatMessage::user("loop")], &CancelFlag::new(), &mut |e| events.push(e));
    assert_eq!(model.requests().len(), 3);
    assert!(events.iter().any(|e| matches!(e, RunEvent::Text(t) if t.contains("Stopped after 3 steps"))));
}
