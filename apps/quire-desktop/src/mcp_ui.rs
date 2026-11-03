// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Settings → Connections: connect AI apps to this vault (design §7.2), the
//! MCP server over HTTP, tokens, requests from apps that want access, and
//! "Capture web page…".
//!
//! The HTTP server runs in the app while `[mcp_http] enabled` is on; this
//! file gives `quire-app` the factory that starts it (registered when the
//! UI state is first made, at start).

use std::fmt::Write as _;
use std::net::IpAddr;
use std::sync::mpsc;

use egui::{RichText, TextEdit};
use quire_app::Workspace;
use quire_app::agent::{Actor, AgentStore, SubmitOutcome, SystemClock};
use quire_app::mcp_state::{ConsentPrompt, McpHost};
use quire_app::settings::McpHttpSettings;
use quire_app::vault::NotePath;
use quire_app::workspace::NoticeLevel;
use quire_mcp::consent::ConsentStore;
use quire_mcp::http::{HttpConfig, HttpServer};
use quire_mcp::tokens::{Kind, Scopes, TokenStore, parse_expiry};
use quire_mcp::{Server, ServerConfig};

use crate::overlays::{OverlayResult, OverlayView};
use crate::panels::PanelOut;
use crate::registry::FeatureUi;
use crate::theme::Palette;

/// UI state kept between frames (in [`FeatureUi`]).
pub(crate) struct McpUi {
    token_name: String,
    scope_propose: bool,
    scope_write: bool,
    scope_ask: bool,
    expiry: &'static str,
    /// A token just made, shown once.
    new_token: Option<String>,
    message: Option<String>,
    /// The person accepted that another address reaches the vault.
    network_ok: bool,
    bind_text: String,
    consent: Option<ConsentDialog>,
}

impl Default for McpUi {
    fn default() -> Self {
        quire_app::mcp_state::register_host_factory(start_host);
        Self {
            token_name: String::new(),
            scope_propose: true,
            scope_write: false,
            scope_ask: false,
            expiry: "90d",
            new_token: None,
            message: None,
            network_ok: false,
            bind_text: String::new(),
            consent: None,
        }
    }
}

/// The HTTP server, as `quire-app` sees it.
struct Host {
    _http: HttpServer,
    url: String,
    consent: ConsentStore,
}

fn now() -> i64 {
    quire_app::history_ops::now_ms()
}

impl McpHost for Host {
    fn pending(&mut self) -> Vec<ConsentPrompt> {
        self.consent
            .pending(now())
            .into_iter()
            .map(|r| ConsentPrompt {
                wants: r.scopes().describe().iter().map(|s| (*s).to_string()).collect(),
                id: r.id,
                client_name: r.client_name,
                client_host: r.client_host,
            })
            .collect()
    }

    fn decide(&mut self, id: &str, approve: bool) -> Result<(), String> {
        self.consent.decide(id, approve, now()).map(|_| ())
    }

    fn url(&self) -> String {
        self.url.clone()
    }
}

fn start_host(s: &McpHttpSettings, root: &std::path::Path) -> Result<Box<dyn McpHost>, String> {
    let bind: IpAddr = s.bind.trim().parse().map_err(|_| format!("“{}” isn't an IP address", s.bind))?;
    let mut cfg = HttpConfig::local(s.port).ok_or("no config folder on this system")?;
    cfg.bind = bind;
    cfg.public_url.clone_from(&s.public_url);
    let consent = cfg.consent.clone();
    // Over HTTP, what a client may do depends on its token's scopes.
    let srv = Server::open_with(
        &root.to_string_lossy(),
        // WS-D: the built-in prompts and the person's AI for `ask` in model mode.
        ServerConfig {
            allow_write: true,
            prompts: quire_assistant::prompts::mcp_defs(),
            chat: quire_app::ai::chat_model(&quire_app::Settings::load()),
            ..ServerConfig::default()
        },
    )?;
    let http = HttpServer::start(srv, cfg)?;
    Ok(Box::new(Host { url: http.urls().resource.clone(), _http: http, consent }))
}

/// Whether the tab is offered.
pub(crate) fn available(ws: &Workspace) -> bool {
    ws.vault_root().is_some()
}

fn section(ui: &mut egui::Ui, pal: &Palette, title: &str) {
    ui.add_space(12.0);
    ui.label(RichText::new(title).strong().color(pal.strong));
    ui.add_space(4.0);
}

/// A command line with a Copy button.
fn snippet(ui: &mut egui::Ui, pal: &Palette, label: &str, text: &str) {
    ui.label(RichText::new(label).color(pal.muted));
    ui.horizontal(|ui| {
        let mut shown = text.to_string();
        let w = (ui.available_width() - 70.0).max(120.0);
        ui.add(TextEdit::multiline(&mut shown).code_editor().desired_rows(1).desired_width(w).interactive(true));
        if ui.button("Copy").clicked() {
            ui.ctx().copy_text(text.to_string());
        }
    });
}

/// `quire-cli`, next to this app when it was installed with it.
fn cli_path() -> String {
    let name = if cfg!(windows) { "quire-cli.exe" } else { "quire-cli" };
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join(name)))
        .filter(|p| p.is_file())
        .map_or_else(|| name.to_string(), |p| p.to_string_lossy().into_owned())
}

fn quote(s: &str) -> String {
    if s.chars().any(|c| c.is_whitespace() || c == '\'' || c == '"') { format!("\"{}\"", s.replace('"', "\\\"")) } else { s.to_string() }
}

pub(crate) fn show(ui: &mut egui::Ui, ws: &mut Workspace, st: &mut FeatureUi, pal: &Palette) -> PanelOut {
    let mut out = PanelOut::default();
    let me = &mut st.mcp;
    // An app asking to connect (the status bar's item opens this tab).
    if me.consent.is_none()
        && let Some(p) = ws.features.mcp.pending.first()
    {
        me.consent = Some(ConsentDialog::new(p.clone()));
    }
    if let Some(d) = me.consent.as_mut()
        && d.show(ui.ctx(), ws, pal).close
    {
        me.consent = None;
    }
    let Some(root) = ws.vault_root().map(std::path::Path::to_path_buf) else {
        ui.label(RichText::new("Open a vault to connect apps to it.").color(pal.muted));
        return out;
    };
    body(ui, ws, me, pal, &root, &mut out.run);
    out
}

fn body(
    ui: &mut egui::Ui,
    ws: &mut Workspace,
    me: &mut McpUi,
    pal: &Palette,
    root: &std::path::Path,
    run: &mut Option<quire_app::Command>,
) {
    let vault = quote(&root.to_string_lossy());
    let cli = quote(&cli_path());

    ui.label(
        "AI apps can search, read and suggest changes to this vault. Changes outside wiki/ arrive as suggestions in the Inbox; you decide.",
    );
    section(ui, pal, "Connect an app on this computer");
    snippet(
        ui,
        pal,
        "Claude Code (in a terminal):",
        &format!("claude mcp add quire -- {cli} mcp {vault} --allow-write --client claude-code"),
    );
    let desktop = format!(
        "{{\n  \"mcpServers\": {{\n    \"quire\": {{\n      \"command\": {},\n      \"args\": [\"mcp\", {}, \"--allow-write\", \"--client\", \"claude-desktop\"]\n    }}\n  }}\n}}",
        serde_json_string(&cli_path()),
        serde_json_string(&root.to_string_lossy())
    );
    snippet(ui, pal, "Claude Desktop (claude_desktop_config.json):", &desktop);
    snippet(
        ui,
        pal,
        "Any MCP app (command to run; leave out --allow-write for read-only):",
        &format!("{cli} mcp {vault} --allow-write --client my-app"),
    );

    section(ui, pal, "Over HTTP (other apps, other computers)");
    let mut changed = false;
    changed |= ui.checkbox(&mut ws.settings.mcp_http.enabled, "Serve this vault over HTTP while Quire is open").changed();
    ui.horizontal(|ui| {
        ui.label("Port");
        changed |= ui.add(egui::DragValue::new(&mut ws.settings.mcp_http.port).range(1024..=65535)).changed();
        ui.label("Address");
        if me.bind_text.is_empty() {
            me.bind_text.clone_from(&ws.settings.mcp_http.bind);
        }
        ui.add(TextEdit::singleline(&mut me.bind_text).desired_width(120.0));
    });
    let wanted: Option<IpAddr> = me.bind_text.trim().parse().ok();
    match wanted {
        None => {
            ui.label(RichText::new("Enter an IP address, such as 127.0.0.1 (this computer only).").color(pal.warn));
        }
        Some(ip) if ip.to_string() != ws.settings.mcp_http.bind => {
            if ip.is_loopback() {
                ws.settings.mcp_http.bind = ip.to_string();
                changed = true;
            } else {
                ui.label(
                    RichText::new("Other computers on your network will be able to reach this vault at this address. Every request still needs a token, but traffic isn't encrypted: use a tunnel or HTTPS proxy for anything beyond your own network.")
                        .color(pal.warn),
                );
                ui.checkbox(&mut me.network_ok, "I understand");
                if me.network_ok && ui.button("Use this address").clicked() {
                    ws.settings.mcp_http.bind = ip.to_string();
                    changed = true;
                }
            }
        }
        Some(_) => {}
    }
    ui.horizontal(|ui| {
        ui.label("Public URL");
        changed |= ui
            .add(
                TextEdit::singleline(&mut ws.settings.mcp_http.public_url)
                    .hint_text("https://notes.example.com (behind a proxy or tunnel)")
                    .desired_width(320.0),
            )
            .changed();
    });
    match (&ws.features.mcp.url(), &ws.features.mcp.error) {
        (Some(url), _) => {
            ui.horizontal(|ui| {
                ui.label(RichText::new(format!("Running at {url}")).color(pal.accent));
                if ui.small_button("Copy").clicked() {
                    ui.ctx().copy_text(url.clone());
                }
            });
            ui.label(RichText::new("Apps sign in with OAuth and you approve them here, or use a token below.").color(pal.muted));
        }
        (None, Some(e)) if ws.settings.mcp_http.enabled => {
            ui.label(RichText::new(format!("Couldn't start: {e}")).color(pal.danger));
        }
        _ => {}
    }
    if changed {
        let _ = ws.settings.save();
    }

    section(ui, pal, "Apps and tokens");
    match TokenStore::default_store() {
        None => {
            ui.label(RichText::new("No config folder on this system.").color(pal.muted));
        }
        Some(store) => tokens(ui, pal, me, &store),
    }
    if !ws.features.mcp.pending.is_empty() {
        section(ui, pal, "Waiting for you");
        for p in ws.features.mcp.pending.clone() {
            ui.horizontal(|ui| {
                ui.label(format!("{} ({})", p.client_name, p.client_host));
                if ui.button("Review…").clicked() {
                    me.consent = Some(ConsentDialog::new(p.clone()));
                }
            });
        }
    }
    section(ui, pal, "Web pages");
    if ui.button("Capture a web page…").clicked() {
        *run = Some(quire_app::Command::CaptureWebPage);
    }
}

fn serde_json_string(s: &str) -> String {
    let mut out = String::from('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if c.is_control() => {
                let _ = write!(out, "\\u{:04x}", u32::from(c));
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn tokens(ui: &mut egui::Ui, pal: &Palette, me: &mut McpUi, store: &TokenStore) {
    let rows = store.summary(now());
    if rows.is_empty() {
        ui.label(RichText::new("No apps or tokens yet.").color(pal.muted));
    }
    for r in rows {
        ui.horizontal(|ui| {
            let who = if r.kind == Kind::Personal { format!("Token “{}”", r.name) } else { format!("{} ({})", r.name, r.client) };
            ui.label(who);
            ui.label(RichText::new(r.scopes.join(", ")).color(pal.muted));
            let used = r.last_used.map_or_else(|| "never used".to_string(), |t| format!("used {}", quire_app::chrono_fmt(t)));
            ui.label(RichText::new(used).color(pal.faint));
            if ui.small_button("Revoke").clicked() {
                let key = if r.kind == Kind::Personal { r.name.clone() } else { r.client.clone() };
                me.message = Some(match store.revoke(&key) {
                    Ok(_) => "Revoked: that app can't use this vault any more.".into(),
                    Err(e) => e,
                });
            }
        });
    }
    ui.add_space(6.0);
    ui.label(RichText::new("A token for an app that can't sign in:").color(pal.muted));
    ui.horizontal(|ui| {
        ui.add(TextEdit::singleline(&mut me.token_name).hint_text("name, e.g. laptop-script").desired_width(160.0));
        ui.checkbox(&mut me.scope_propose, "suggest changes");
        ui.checkbox(&mut me.scope_write, "write in wiki/");
        ui.checkbox(&mut me.scope_ask, "use my AI provider");
    });
    ui.horizontal(|ui| {
        egui::ComboBox::from_id_salt("mcp-token-expiry")
            .selected_text(match me.expiry {
                "30d" => "30 days",
                "90d" => "90 days",
                "365d" => "1 year",
                _ => "never expires",
            })
            .show_ui(ui, |ui| {
                for (v, l) in [("30d", "30 days"), ("90d", "90 days"), ("365d", "1 year"), ("never", "never expires")] {
                    ui.selectable_value(&mut me.expiry, v, l);
                }
            });
        if ui.button("Create token").clicked() {
            let mut scopes = vec!["notes:read"];
            if me.scope_propose {
                scopes.push("notes:propose");
            }
            if me.scope_write {
                scopes.push("notes:write");
            }
            if me.scope_ask {
                scopes.push("notes:ask");
            }
            let made = Scopes::parse(&scopes.join(","))
                .and_then(|s| Ok((s, parse_expiry(me.expiry, now())?)))
                .and_then(|(s, exp)| store.create_personal(&me.token_name, s, exp, now()));
            match made {
                Ok(t) => {
                    me.new_token = Some(t);
                    me.message = None;
                    me.token_name.clear();
                }
                Err(e) => me.message = Some(e),
            }
        }
    });
    if let Some(t) = me.new_token.clone() {
        ui.label(RichText::new("Copy it now: Quire shows it only once.").color(pal.warn));
        ui.horizontal(|ui| {
            let mut shown = t.clone();
            ui.add(TextEdit::singleline(&mut shown).code_editor().desired_width(420.0));
            if ui.button("Copy").clicked() {
                ui.ctx().copy_text(t);
            }
            if ui.button("Done").clicked() {
                me.new_token = None;
            }
        });
    }
    if let Some(m) = &me.message {
        ui.label(RichText::new(m).color(pal.muted));
    }
}

/// "`<app>` wants to use your notes": approve or deny (design §4.6).
pub(crate) struct ConsentDialog {
    prompt: ConsentPrompt,
    error: Option<String>,
}

impl ConsentDialog {
    pub(crate) const fn new(prompt: ConsentPrompt) -> Self {
        Self { prompt, error: None }
    }
}

/// One sentence about where agents may write in this vault.
fn zones_sentence(ws: &Workspace) -> String {
    let policy = ws.agent_store().map(AgentStore::policy).unwrap_or_default();
    let direct: Vec<String> = policy.zones.iter().filter(|z| z.mode == quire_app::agent::Mode::Direct).map(|z| z.path.clone()).collect();
    let hidden: Vec<String> = policy.zones.iter().filter(|z| z.mode == quire_app::agent::Mode::Deny).map(|z| z.path.clone()).collect();
    let mut s = if direct.is_empty() {
        "Apps can only suggest changes; you review every one.".to_string()
    } else {
        format!("Apps with write access can write directly in {}; elsewhere they can only suggest changes.", direct.join(", "))
    };
    if !hidden.is_empty() {
        let _ = write!(s, " {} stays hidden from them.", hidden.join(", "));
    }
    s
}

impl OverlayView for ConsentDialog {
    fn show(&mut self, ctx: &egui::Context, ws: &mut Workspace, pal: &Palette) -> OverlayResult {
        let mut res = OverlayResult::default();
        egui::Modal::new(egui::Id::unique("quire-mcp-consent")).show(ctx, |ui| {
            let w = 460.0_f32.min(ctx.content_rect().width() - 48.0);
            ui.set_width(w);
            ui.heading("An app wants to use your notes");
            ui.add_space(6.0);
            ui.label(RichText::new(&self.prompt.client_host).size(20.0).strong().color(pal.strong));
            ui.label(RichText::new(format!("calls itself “{}”", self.prompt.client_name)).color(pal.muted));
            ui.add_space(8.0);
            ui.label("If you approve, it can:");
            for w in &self.prompt.wants {
                ui.label(format!("• {w}"));
            }
            ui.add_space(4.0);
            ui.label(RichText::new(zones_sentence(ws)).color(pal.muted));
            ui.label(
                RichText::new(
                    "Only approve if you just connected this app yourself. You can revoke it any time in Settings → Connections.",
                )
                .color(pal.muted),
            );
            if let Some(e) = &self.error {
                ui.label(RichText::new(e).color(pal.danger));
            }
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui.button("Approve").clicked() {
                    match ws.features.mcp.decide(&self.prompt.id, true) {
                        Ok(()) => res.close = true,
                        Err(e) => self.error = Some(e),
                    }
                }
                if ui.button("Deny").clicked() {
                    let _ = ws.features.mcp.decide(&self.prompt.id, false);
                    res.close = true;
                }
                if ui.button("Later").clicked() {
                    res.close = true;
                }
            });
        });
        res
    }
}

/// "Capture web page…": a URL into `raw/` (design §B.3).
#[derive(Default)]
pub(crate) struct CaptureDialog {
    url: String,
    focused: bool,
    busy: Option<mpsc::Receiver<Result<(NotePath, String), String>>>,
    error: Option<String>,
}

impl CaptureDialog {
    /// The dialog for `Command::CaptureWebPage` (`registry::UiTarget::Overlay`).
    pub(crate) fn make(_ws: &mut Workspace) -> Box<dyn OverlayView> {
        Box::<Self>::default()
    }

    #[cfg(test)]
    pub(crate) fn sample(url: &str, error: Option<&str>) -> Self {
        Self { url: url.into(), focused: true, busy: None, error: error.map(str::to_string) }
    }

    fn start(&mut self, ws: &Workspace) {
        let Some(vault) = ws.vault().cloned() else { return };
        let url = self.url.trim().to_string();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let store = AgentStore::open(&vault, quire_app::device::this_device(), std::sync::Arc::new(SystemClock));
            let actor = Actor::import("url", &store.device().id);
            let res = quire_mcp::capture::capture(&vault, &store, &actor, &url, &quire_mcp::capture::Options::default())
                .map(|d| {
                    let note = match d.outcome {
                        SubmitOutcome::Applied { .. } => format!("Saved {} ({} images).", d.path, d.images),
                        SubmitOutcome::Proposed { .. } => format!("Suggested {} for review.", d.path),
                    };
                    (d.path, note)
                })
                .map_err(|e| e.to_string());
            let _ = tx.send(res);
        });
        self.busy = Some(rx);
        self.error = None;
    }
}

impl OverlayView for CaptureDialog {
    fn show(&mut self, ctx: &egui::Context, ws: &mut Workspace, pal: &Palette) -> OverlayResult {
        let mut res = OverlayResult::default();
        if let Some(rx) = &self.busy {
            match rx.try_recv() {
                Ok(Ok((path, note))) => {
                    self.busy = None;
                    ws.notify(NoticeLevel::Info, note);
                    ws.open_note(&path);
                    res.close = true;
                    return res;
                }
                Ok(Err(e)) => {
                    self.busy = None;
                    self.error = Some(e);
                }
                Err(mpsc::TryRecvError::Empty) => ctx.request_repaint_after(std::time::Duration::from_millis(100)),
                Err(mpsc::TryRecvError::Disconnected) => self.busy = None,
            }
        }
        egui::Modal::new(egui::Id::unique("quire-capture-web")).show(ctx, |ui| {
            let w = 460.0_f32.min(ctx.content_rect().width() - 48.0);
            ui.set_width(w);
            ui.heading("Capture web page");
            ui.label(
                RichText::new("Saves the page's article and images as a note in raw/, marked as untrusted source material.")
                    .color(pal.muted),
            );
            ui.add_space(6.0);
            let r = ui.add(TextEdit::singleline(&mut self.url).hint_text("https://…").desired_width(w));
            if !self.focused {
                r.request_focus();
                self.focused = true;
            }
            let go = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            if let Some(e) = &self.error {
                ui.label(RichText::new(e).color(pal.danger));
            }
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                let busy = self.busy.is_some();
                if busy {
                    ui.spinner();
                    ui.label("Fetching…");
                } else if (ui.button("Capture").clicked() || go) && !self.url.trim().is_empty() {
                    self.start(ws);
                }
                if ui.button("Cancel").clicked() {
                    res.close = true;
                }
            });
        });
        res
    }
}

/// Dialog `i` for `layout_check`, in its interesting states.
#[cfg(test)]
pub(crate) fn layout_sample(i: usize) -> crate::overlays::Overlay {
    use crate::overlays::Overlay;
    let prompt = ConsentPrompt {
        id: "abc".into(),
        client_name: "A very long client name that someone typed into their metadata document".into(),
        client_host: "very-long-subdomain.example-client-application.com".into(),
        wants: Scopes::parse("notes:propose,notes:ask").unwrap_or_default().describe().iter().map(|s| (*s).to_string()).collect(),
    };
    match i {
        0 => Overlay::Feature(Box::new(ConsentDialog::new(prompt))),
        1 => Overlay::Feature(Box::new(ConsentDialog { prompt, error: Some("The HTTP server isn't running. ".repeat(4)) })),
        2 => Overlay::Feature(Box::new(CaptureDialog::sample("", None))),
        _ => Overlay::Feature(Box::new(CaptureDialog::sample(
            "http://127.0.0.1/x",
            Some(&"127.0.0.1 is a private or local address. ".repeat(4)),
        ))),
    }
}
