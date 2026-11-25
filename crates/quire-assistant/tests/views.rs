// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The panel's tools rank through the agent's view, as MCP's do
//! (`docs/design/0.14-agent-index.md`): with meaning on, a vault with notes
//! the agent may not see answers `search` and `ask` exactly as the plain
//! vault with meaning off (Fable's review, B1).
#![allow(
    clippy::unwrap_used,
    clippy::panic,
    clippy::tests_outside_test_module,
    reason = "an integration test: its tests sit at the top level, and its helpers unwrap or panic to fail the test"
)]

use std::sync::Arc;

use quire_agent::{AgentStore, DeviceInfo, SystemClock};
use quire_app::intel::chat::CancelFlag;
use quire_app::vault::Vault;
use quire_assistant::runner::{ToolHost, panel_actor};
use serde_json::{Value, json};

const POLICY: &str = "format = 1\ndefault = \"read\"\n[[zone]]\npath = \"Private/\"\nrole = \"private\"\nmode = \"deny\"\n";

const GARDEN: &str = "# Garden\n\nTomatoes grow best in the sunny corner of the garden by the fence.\n\nThe zucchini plants need water every morning before the heat of the day.\n";

/// The panel's tools over a vault, its policy approved; with `hidden`, the
/// notes in `Private/` teach the meaning model that "zucchini" goes with
/// "tomatoes", which no visible note says.
fn host(hidden: bool) -> ToolHost {
    let mut files: Vec<(String, String)> = vec![
        (".quire/shared/agent-policy.toml".into(), POLICY.into()),
        ("Garden.md".into(), GARDEN.into()),
        ("Bread.md".into(), "# Bread\n\nSourdough bread needs flour, water and a slow rise in a warm oven overnight.\n".into()),
        ("Cars.md".into(), "# Cars\n\nThe car engine needs oil and the tyres need air before a long road trip.\n".into()),
        ("Music.md".into(), "# Music\n\nThe piano sounds best after tuning, and the violin needs new strings soon.\n".into()),
        ("Work.md".into(), "# Work\n\nThe project meeting moved to Thursday and the report is due next week.\n".into()),
    ];
    if hidden {
        for i in 0..16 {
            files.push((
                format!("Private/harvest {i}.md"),
                format!("# Harvest {i}\n\nTomatoes and zucchini together, zucchini beside tomatoes, tomatoes zucchini harvest {i}.\n"),
            ));
        }
    }
    let refs: Vec<(&str, &str)> = files.iter().map(|(p, t)| (p.as_str(), t.as_str())).collect();
    let vault = Vault::in_memory(&refs);
    let agent = AgentStore::open(&vault, DeviceInfo { id: "d_test".into(), name: "Test".into() }, Arc::new(SystemClock));
    agent.approve_policy(agent.policy_state().file_hash().as_ref()).unwrap();
    let host = ToolHost::open(vault, None, agent);
    if hidden {
        host.index().flush_semantic();
        assert!(host.index().semantic().is_some());
    } else {
        host.index().set_meaning(false);
    }
    host
}

fn reply(host: &ToolHost, tool: &str, args: &Value) -> String {
    let actor = panel_actor("test", "Test", None, "d_test");
    match host.call(&actor, tool, args, &CancelFlag::new()) {
        quire_mcp::ToolOutput::Ok { text, structured, .. } => format!("{text}\n{structured}"),
        other => panic!("{tool} {args}: {other:?}"),
    }
}

#[test]
fn the_panel_with_meaning_on_answers_as_the_plain_vault_without_meaning() {
    let (plain, hidden) = (host(false), host(true));
    for q in ["tomatoes", "Where do the tomatoes grow?", "what about zucchini", "harvest", "bread oven"] {
        for (tool, args) in [("search", json!({"query": q})), ("ask", json!({"question": q})), ("ask", json!({"question": q, "limit": 2}))]
        {
            let want = reply(&plain, tool, &args);
            assert!(!want.contains("Private/"), "{want}");
            assert_eq!(reply(&hidden, tool, &args), want, "{tool} {args}");
        }
    }
}
