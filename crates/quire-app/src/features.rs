// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The 0.9 feature hub: where the workspace hands work to features.
//!
//! `workspace.rs` is frozen after the foundation step (design §6.3), so it
//! calls only the functions here. Each workstream has one entry module
//! (`agent_state`, `mcp_state`, `trust_jobs`, `assistant_state`,
//! `tasks_state`, `nav`, `canvas_state`) with the same set of functions, and
//! keeps its state in a field of [`Features`]. Adding a feature means
//! filling in those functions, never editing this file or the workspace.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Instant;

use quire_vault::VaultEvent;

use crate::commands::{Command, Owner};
use crate::decor::{Decoration, DecorationEvent, StatusItem};
use crate::document::DocId;
use crate::workspace::{UiRequest, Workspace};
use crate::{agent_state, assistant_state, canvas_state, mcp_state, nav, tasks_state, trust_jobs};

/// Decorations cached for (document, revision, generation).
type DecorCache = RefCell<Option<((DocId, u64, u64), Rc<Vec<Decoration>>)>>;

/// Every feature's state. One field per module; the comment names the owner.
#[derive(Default)]
pub struct Features {
    /// WS-A: inbox, review, activity (`agent_state.rs`).
    pub agent: agent_state::AgentState,
    /// WS-B: HTTP MCP, tokens, capture (`mcp_state.rs`).
    pub mcp: mcp_state::McpState,
    /// WS-C: trust jobs and timeline (`trust_jobs.rs`).
    pub trust: trust_jobs::TrustState,
    /// WS-D: agent panel and ACP (`assistant_state.rs`).
    pub assistant: assistant_state::AssistantState,
    /// WS-D: on-device AI (`ai.rs`).
    pub ai: crate::ai::LocalAiState,
    /// WS-D: first-run AI choice (`onboarding.rs`).
    pub onboarding: crate::onboarding::OnboardingState,
    /// WS-E: tasks, agenda, reminders (`tasks_state.rs`).
    pub tasks: tasks_state::TasksState,
    /// WS-E: periodic notes and calendar (`periodic.rs`).
    pub periodic: crate::periodic::PeriodicState,
    /// WS-E: flashcards (`srs_state.rs`).
    pub srs: crate::srs_state::SrsState,
    /// WS-E: tag and property refactors (`meta_refactor.rs`).
    pub meta: crate::meta_refactor::MetaState,
    /// WS-F: navigation, tab modes, Vim (`nav.rs`).
    pub nav: nav::NavState,
    /// WS-F: git (`git.rs`).
    pub git: crate::git::GitState,
    /// WS-F: themes and icons (`themes.rs`).
    pub themes: crate::themes::ThemesState,
    /// WS-G: canvas (`canvas_state.rs`).
    pub canvas: canvas_state::CanvasState,
    /// WS-G: PDF (`pdf_state.rs`).
    pub pdf: crate::pdf_state::PdfState,
    /// WS-G: citations (`citations.rs`).
    pub citations: crate::citations::CitationsState,
    /// Bump to make every document's decorations be asked for again.
    pub decor_generation: u64,
    decor_cache: DecorCache,
}

/// Per-frame work for every feature. Returns true to repaint.
pub(crate) fn tick(ws: &mut Workspace, now: Instant) -> bool {
    // Non-short-circuiting: every feature ticks every frame.
    agent_state::tick(ws, now)
        | mcp_state::tick(ws, now)
        | trust_jobs::tick(ws, now)
        | assistant_state::tick(ws, now)
        | tasks_state::tick(ws, now)
        | nav::tick(ws, now)
        | canvas_state::tick(ws, now)
}

/// The earliest instant any feature has time-driven work.
pub(crate) fn next_deadline(ws: &Workspace) -> Option<Instant> {
    [
        agent_state::next_deadline(ws),
        mcp_state::next_deadline(ws),
        trust_jobs::next_deadline(ws),
        assistant_state::next_deadline(ws),
        tasks_state::next_deadline(ws),
        nav::next_deadline(ws),
        canvas_state::next_deadline(ws),
    ]
    .into_iter()
    .flatten()
    .min()
}

/// Run a 0.9 command in the module that owns it.
pub(crate) fn run(ws: &mut Workspace, cmd: Command) -> Option<UiRequest> {
    match cmd.owner() {
        Owner::Agent => agent_state::run(ws, cmd),
        Owner::Mcp => mcp_state::run(ws, cmd),
        Owner::Trust => trust_jobs::run(ws, cmd),
        Owner::Assistant => assistant_state::run(ws, cmd),
        Owner::Plan => tasks_state::run(ws, cmd),
        Owner::Editor => nav::run(ws, cmd),
        Owner::Spatial => canvas_state::run(ws, cmd),
        Owner::Core => None,
    }
}

/// Whether a command is offered (0.8 commands always are).
pub(crate) const fn available(ws: &Workspace, cmd: Command) -> bool {
    match cmd.owner() {
        Owner::Core => true,
        Owner::Agent => agent_state::available(ws, cmd),
        Owner::Mcp => mcp_state::available(ws, cmd),
        Owner::Trust => trust_jobs::available(ws, cmd),
        Owner::Assistant => assistant_state::available(ws, cmd),
        Owner::Plan => tasks_state::available(ws, cmd),
        Owner::Editor => nav::available(ws, cmd),
        Owner::Spatial => canvas_state::available(ws, cmd),
    }
}

/// Every vault event goes to every feature, after the workspace's own handling.
pub(crate) fn vault_event(ws: &mut Workspace, ev: &VaultEvent) {
    agent_state::vault_event(ws, ev);
    mcp_state::vault_event(ws, ev);
    trust_jobs::vault_event(ws, ev);
    assistant_state::vault_event(ws, ev);
    tasks_state::vault_event(ws, ev);
    nav::vault_event(ws, ev);
    canvas_state::vault_event(ws, ev);
}

/// A vault was opened (or attached): features start their jobs.
pub(crate) fn vault_opened(ws: &mut Workspace) {
    agent_state::vault_opened(ws);
    mcp_state::vault_opened(ws);
    trust_jobs::vault_opened(ws);
    assistant_state::vault_opened(ws);
    tasks_state::vault_opened(ws);
    nav::vault_opened(ws);
    canvas_state::vault_opened(ws);
}

impl Workspace {
    /// Whether to offer `cmd` in menus, the palette and the keyboard.
    pub const fn command_available(&self, cmd: Command) -> bool {
        available(self, cmd)
    }

    /// What features draw on `doc`, cached per revision. Cheap to call every frame.
    pub fn decorations(&self, doc: DocId) -> Rc<Vec<Decoration>> {
        let Some(d) = self.doc(doc) else { return Rc::default() };
        let key = (doc, d.revision(), self.features.decor_generation);
        if let Some((k, v)) = self.features.decor_cache.borrow().as_ref()
            && *k == key
        {
            return Rc::clone(v);
        }
        let mut out = Vec::new();
        agent_state::decorations(self, d, &mut out);
        mcp_state::decorations(self, d, &mut out);
        trust_jobs::decorations(self, d, &mut out);
        assistant_state::decorations(self, d, &mut out);
        tasks_state::decorations(self, d, &mut out);
        nav::decorations(self, d, &mut out);
        canvas_state::decorations(self, d, &mut out);
        let out = Rc::new(out);
        *self.features.decor_cache.borrow_mut() = Some((key, Rc::clone(&out)));
        out
    }

    /// A decoration's action was clicked: the owner handles it.
    pub fn decoration_event(&mut self, ev: &DecorationEvent) {
        match ev.action.owner {
            Owner::Agent => agent_state::decoration_event(self, ev),
            Owner::Mcp => mcp_state::decoration_event(self, ev),
            Owner::Trust => trust_jobs::decoration_event(self, ev),
            Owner::Assistant => assistant_state::decoration_event(self, ev),
            Owner::Plan => tasks_state::decoration_event(self, ev),
            Owner::Editor => nav::decoration_event(self, ev),
            Owner::Spatial => canvas_state::decoration_event(self, ev),
            Owner::Core => {}
        }
        self.features.decor_generation += 1;
    }

    /// Status-bar items from every feature, in workstream order.
    pub fn status_items(&self) -> Vec<StatusItem> {
        let mut out = Vec::new();
        agent_state::status(self, &mut out);
        mcp_state::status(self, &mut out);
        trust_jobs::status(self, &mut out);
        assistant_state::status(self, &mut out);
        tasks_state::status(self, &mut out);
        nav::status(self, &mut out);
        canvas_state::status(self, &mut out);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Settings;
    use crate::vault::Vault;

    #[test]
    fn every_0_9_command_reaches_its_owner_without_panicking() {
        let mut ws = Workspace::new(Settings::default());
        ws.attach_vault(Vault::in_memory(&[("A.md", "# A\n")]));
        ws.open_note(&"A.md".into());
        for c in crate::COMMANDS.iter().filter(|c| c.command.owner() != Owner::Core) {
            let notices = ws.notices.len();
            match ws.run(c.command) {
                Some(UiRequest::Feature(cmd)) => assert_eq!(cmd, c.command),
                Some(other) => panic!("{:?} asked for {other:?}", c.command),
                None => assert!(ws.notices.len() > notices || ws.notices.len() == 20, "{:?} said nothing", c.command),
            }
        }
    }

    #[test]
    fn decorations_are_cached_per_revision() {
        let mut ws = Workspace::new(Settings::default());
        ws.attach_vault(Vault::in_memory(&[("A.md", "# A\n")]));
        let id = ws.open_note(&"A.md".into()).unwrap();
        let a = ws.decorations(id);
        assert!(Rc::ptr_eq(&a, &ws.decorations(id)));
        assert!(a.is_empty());
        assert!(ws.status_items().is_empty());
        ws.decoration_event(&DecorationEvent { doc: id, action: crate::decor::ActionId { owner: Owner::Agent, id: 1 }, line: 0 });
        assert!(!Rc::ptr_eq(&a, &ws.decorations(id)), "an event refreshes decorations");
    }
}
