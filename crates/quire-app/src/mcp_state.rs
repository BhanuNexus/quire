// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! WS-B's entry point into the workspace: MCP over HTTP while the app runs,
//! requests from apps that want to connect, and web capture.
//!
//! `quire-app` can't depend on `quire-mcp` (which sits above it), so the
//! desktop registers a [`HostFactory`] at start; `tick` then starts, stops
//! or restarts the HTTP server as `[mcp_http]` in settings says, and polls
//! it for connection requests. A request shows in the status bar until the
//! person approves or denies it ("Connect an app", Settings → Connections).
//!
//! State lives in `ws.features.mcp`.

use std::sync::OnceLock;
use std::time::{Duration, Instant};

use quire_vault::VaultEvent;

use crate::Command;
use crate::decor::{Decoration, DecorationEvent, StatusItem};
use crate::document::Document;
use crate::settings::McpHttpSettings;
use crate::workspace::{UiRequest, Workspace};

/// An app asking for access over HTTP, as the person sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConsentPrompt {
    pub id: String,
    /// The name the app gives itself (shown, never trusted alone).
    pub client_name: String,
    /// The host of its `client_id`, shown prominently.
    pub client_host: String,
    /// What it may do, in plain words.
    pub wants: Vec<String>,
}

/// The running HTTP server, provided by the desktop (`quire-mcp`).
pub trait McpHost: Send {
    /// Requests waiting for the person, oldest first.
    fn pending(&mut self) -> Vec<ConsentPrompt>;
    fn decide(&mut self, id: &str, approve: bool) -> Result<(), String>;
    /// Where clients connect (`http://127.0.0.1:7311/mcp`).
    fn url(&self) -> String;
}

/// Starts the HTTP server for a vault folder with these settings.
pub type HostFactory = fn(&McpHttpSettings, &std::path::Path) -> Result<Box<dyn McpHost>, String>;

static FACTORY: OnceLock<HostFactory> = OnceLock::new();

/// Called once by the desktop at start.
pub fn register_host_factory(f: HostFactory) {
    let _ = FACTORY.set(f);
}

/// How often the consent folder is checked while the server runs.
const POLL: Duration = Duration::from_millis(800);

/// WS-B's state, kept in `ws.features.mcp`.
#[derive(Default)]
pub struct McpState {
    host: Option<Box<dyn McpHost>>,
    /// What the running (or failed) server was started with.
    started_with: Option<(McpHttpSettings, std::path::PathBuf)>,
    /// Why the server isn't running, when it should be.
    pub error: Option<String>,
    pub pending: Vec<ConsentPrompt>,
    next_poll: Option<Instant>,
    /// A vault folder is open (the commands need one).
    vault_open: bool,
}

impl std::fmt::Debug for McpState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpState")
            .field("running", &self.host.is_some())
            .field("error", &self.error)
            .field("pending", &self.pending)
            .finish_non_exhaustive()
    }
}

impl McpState {
    /// Where HTTP clients connect, while the server runs.
    pub fn url(&self) -> Option<String> {
        self.host.as_ref().map(|h| h.url())
    }

    /// Approve or deny a connection request.
    pub fn decide(&mut self, id: &str, approve: bool) -> Result<(), String> {
        let host = self.host.as_mut().ok_or("The HTTP server isn't running")?;
        host.decide(id, approve)?;
        self.pending.retain(|p| p.id != id);
        Ok(())
    }

    /// Stop the server (for example to restart it with new settings).
    pub fn stop(&mut self) {
        self.host = None;
        self.started_with = None;
    }
}

/// Time-driven work; return true to repaint.
pub(crate) fn tick(ws: &mut Workspace, now: Instant) -> bool {
    let want = ws.settings.mcp_http.enabled.then(|| ws.vault_root().map(std::path::Path::to_path_buf)).flatten();
    let st = &mut ws.features.mcp;
    let mut changed = false;
    match (want, FACTORY.get()) {
        (Some(root), Some(factory)) => {
            let key = (ws.settings.mcp_http.clone(), root.clone());
            if st.started_with.as_ref() != Some(&key) {
                st.host = None;
                match factory(&ws.settings.mcp_http, &root) {
                    Ok(h) => {
                        st.host = Some(h);
                        st.error = None;
                    }
                    Err(e) => st.error = Some(e),
                }
                st.started_with = Some(key);
                changed = true;
            }
        }
        _ => {
            if st.host.is_some() || st.started_with.is_some() {
                st.stop();
                st.pending.clear();
                changed = true;
            }
        }
    }
    if let Some(h) = st.host.as_mut()
        && st.next_poll.is_none_or(|t| now >= t)
    {
        st.next_poll = Some(now + POLL);
        let pending = h.pending();
        if pending != st.pending {
            st.pending = pending;
            changed = true;
        }
    }
    changed
}

/// The earliest instant `tick` has work to do.
pub(crate) fn next_deadline(ws: &Workspace) -> Option<Instant> {
    ws.features.mcp.host.as_ref().and(ws.features.mcp.next_poll)
}

/// Whether a Mcp-owned command is ready to offer.
pub(crate) const fn available(ws: &Workspace, cmd: Command) -> bool {
    matches!(cmd, Command::CaptureWebPage | Command::ConnectAgent) && ws.features.mcp.vault_open
}

/// A Mcp-owned command ([`Command::owner`]).
#[expect(clippy::unnecessary_wraps, reason = "the features hub's signature: other owners return `None` for some commands")]
pub(crate) fn run(ws: &Workspace, cmd: Command) -> Option<UiRequest> {
    let _ = ws;
    Some(UiRequest::Feature(cmd))
}

/// Decorations for `doc`, appended to `out`.
pub(crate) fn decorations(ws: &Workspace, doc: &Document, out: &mut Vec<Decoration>) {
    let _ = (ws, doc, out);
}

/// A click on one of this feature's decoration actions.
pub(crate) fn decoration_event(ws: &mut Workspace, ev: &DecorationEvent) {
    let _ = (ws, ev);
}

/// A file changed in the vault (notes and attachments).
pub(crate) fn vault_event(ws: &mut Workspace, ev: &VaultEvent) {
    let _ = (ws, ev);
}

/// A vault was opened: the server (if on) restarts for it on the next tick.
pub(crate) fn vault_opened(ws: &mut Workspace) {
    ws.features.mcp.vault_open = ws.vault_root().is_some();
    ws.features.mcp.stop();
    ws.features.mcp.pending.clear();
    ws.features.mcp.error = None;
}

/// Status-bar items, appended to `out`.
pub(crate) fn status(ws: &Workspace, out: &mut Vec<StatusItem>) {
    let st = &ws.features.mcp;
    if let Some(p) = st.pending.first() {
        out.push(StatusItem {
            owner: crate::Owner::Mcp,
            text: format!("{} wants access", p.client_name),
            tooltip: format!("{} ({}) asks to use your notes. Click to approve or deny.", p.client_name, p.client_host),
            command: Some(Command::ConnectAgent),
            warn: true,
        });
    } else if let Some(e) = &st.error {
        out.push(StatusItem {
            owner: crate::Owner::Mcp,
            text: "MCP server off".into(),
            tooltip: format!("The MCP server over HTTP couldn't start: {e}"),
            command: Some(Command::ConnectAgent),
            warn: true,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fake(Vec<ConsentPrompt>);

    impl McpHost for Fake {
        fn pending(&mut self) -> Vec<ConsentPrompt> {
            self.0.clone()
        }
        fn decide(&mut self, id: &str, _approve: bool) -> Result<(), String> {
            self.0.retain(|p| p.id != id);
            Ok(())
        }
        fn url(&self) -> String {
            "http://127.0.0.1:7311/mcp".into()
        }
    }

    #[expect(clippy::unnecessary_wraps, reason = "matches the HostFactory signature")]
    fn fake(_s: &McpHttpSettings, _root: &std::path::Path) -> Result<Box<dyn McpHost>, String> {
        Ok(Box::new(Fake(vec![ConsentPrompt {
            id: "r1".into(),
            client_name: "App".into(),
            client_host: "a.example".into(),
            wants: vec![],
        }])))
    }

    #[test]
    fn the_server_follows_the_settings_and_shows_requests() {
        register_host_factory(fake);
        let dir = tempfile::tempdir().unwrap();
        let mut ws = Workspace::new(crate::Settings::default());
        ws.open_vault(dir.path()).unwrap();
        let t = Instant::now();
        assert!(!tick(&mut ws, t), "off by default");
        assert!(ws.features.mcp.url().is_none());
        ws.settings.mcp_http.enabled = true;
        assert!(tick(&mut ws, t));
        assert_eq!(ws.features.mcp.url().as_deref(), Some("http://127.0.0.1:7311/mcp"));
        assert_eq!(ws.features.mcp.pending.len(), 1);
        let mut items = Vec::new();
        status(&ws, &mut items);
        assert_eq!(items[0].text, "App wants access");
        assert_eq!(items[0].command, Some(Command::ConnectAgent));
        ws.features.mcp.decide("r1", true).unwrap();
        assert!(ws.features.mcp.pending.is_empty());
        ws.settings.mcp_http.enabled = false;
        assert!(tick(&mut ws, t + POLL));
        assert!(ws.features.mcp.url().is_none());
        assert!(available(&ws, Command::CaptureWebPage));
        assert!(matches!(run(&ws, Command::CaptureWebPage), Some(UiRequest::Feature(Command::CaptureWebPage))));
    }
}
