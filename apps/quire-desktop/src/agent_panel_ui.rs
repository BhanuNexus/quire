// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The Agent tab: the in-app agent conversation (design §7.4, §D.5, §D.6).
//!
//! A header names the model and where it runs ("claude-sonnet-5-5 · cloud",
//! "Qwen3 1.7B · this computer", or the amber "External: Codex"). The
//! conversation streams in, with tool calls as collapsible lines and
//! suggested changes as cards that open the review. The composer has
//! context chips (this note, the selection), Ask / Agent, Send and Stop.
//!
//! Runs happen on a background thread: the UI only draws what arrives, so a
//! slow or stuck model never holds up the editor, and Stop answers at once.
//!
//! Owned by WS-D. Registered in `registry.rs`.

#[path = "acp_ui.rs"]
pub(crate) mod acp_ui;
#[path = "ingest_ui.rs"]
pub(crate) mod ingest_ui;

use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use egui::{RichText, Stroke};
use egui_commonmark::{CommonMarkCache, CommonMarkViewer};
use quire_app::agent::ProposalId;
use quire_app::intel::chat::{CancelFlag, ChatMessage, Usage};
use quire_app::vault::NotePath;
use quire_app::{Command, Workspace};
use quire_assistant::acp::{AcpAgent, AcpClient, AcpEvent, PermissionAnswer, PermissionRequest, VaultHost};
use quire_assistant::conversation::{self, Conversation, Item};
use quire_assistant::runner::{self, Capability, RunEvent, RunLimits, RunSpec, ToolHost};

use crate::panels::PanelOut;
use crate::registry::FeatureUi;
use crate::theme::Palette;

/// What the run's thread sends.
enum Msg {
    Run(RunEvent),
    Acp(AcpEvent),
    Permission(PermissionRequest, Sender<PermissionAnswer>),
    /// The turns after the run (provider runs), or an error starting it.
    Finished {
        messages: Option<Vec<ChatMessage>>,
        error: Option<String>,
    },
}

struct Running {
    rx: Receiver<Msg>,
    cancel: CancelFlag,
    stop_at: Option<Instant>,
}

/// An open ACP connection, kept between messages.
struct AcpSlot {
    key: (String, Vec<String>, PathBuf),
    client: AcpClient,
}

/// A permission question waiting for the person.
pub(crate) struct Pending {
    pub(crate) req: PermissionRequest,
    reply: Option<Sender<PermissionAnswer>>,
}

/// The panel's model: the model, its provider id and, on this computer, its size.
type PanelModel = (Arc<dyn quire_app::intel::chat::ChatModel>, String, Option<quire_app::ai::Tier>);

/// The tool host for the open vault, built on the first message.
type HostSlot = Arc<Mutex<Option<(PathBuf, Arc<ToolHost>)>>>;

/// UI state kept between frames (in [`FeatureUi`]).
pub(crate) struct AgentPanelUi {
    conv: Conversation,
    input: String,
    running: Option<Running>,
    host: HostSlot,
    acp: Arc<Mutex<Option<AcpSlot>>>,
    agent_mode: bool,
    chip_note: bool,
    chip_selection: bool,
    history: Option<Vec<Conversation>>,
    pending: Option<Pending>,
    vault: Option<PathBuf>,
    usage: Usage,
    md: CommonMarkCache,
    focus_input: bool,
    /// The panel's model, rebuilt when its settings change or every few
    /// seconds (an API key saved meanwhile), not on every frame.
    model: Option<(String, Instant, Option<PanelModel>)>,
}

impl Default for AgentPanelUi {
    fn default() -> Self {
        Self {
            conv: Conversation::new(now_ms()),
            input: String::new(),
            running: None,
            host: Arc::default(),
            acp: Arc::default(),
            agent_mode: true,
            chip_note: true,
            chip_selection: true,
            history: None,
            pending: None,
            vault: None,
            usage: Usage::default(),
            md: CommonMarkCache::default(),
            focus_input: false,
            model: None,
        }
    }
}

impl AgentPanelUi {
    fn model(&mut self, ws: &Workspace) -> Option<PanelModel> {
        let st = &ws.settings;
        let key = format!(
            "{}|{}|{}|{}|{}|{}|{}",
            st.ai_provider, st.ai_base_url, st.ai_model, st.agent_panel.model, st.ai_local.engine, st.ai_local.llm, st.ai_local.llm_model
        );
        let fresh = self.model.as_ref().is_some_and(|(k, at, _)| *k == key && at.elapsed() < Duration::from_secs(5));
        if !fresh {
            self.model = Some((key, Instant::now(), quire_app::ai::panel_chat(st)));
        }
        self.model.as_ref().and_then(|(_, _, m)| m.clone())
    }
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// Whether the tab is offered.
pub(crate) const fn available(ws: &Workspace) -> bool {
    ws.settings.agent_panel.enabled
}

/// The external agent, when one is connected (the warning was accepted).
fn external(ws: &Workspace) -> Option<AcpAgent> {
    let p = &ws.settings.agent_panel;
    (p.backend == "acp" && p.acp_risk_ack && !p.acp_command.trim().is_empty()).then(|| AcpAgent {
        command: p.acp_command.trim().to_string(),
        args: p.acp_args.clone(),
        label: quire_assistant::acp::PRESETS
            .iter()
            .find(|(_, c)| *c == p.acp_command.trim())
            .map_or_else(|| p.acp_command.trim().to_string(), |(l, _)| (*l).to_string()),
        env: Vec::new(),
    })
}

pub(crate) fn show(ui: &mut egui::Ui, ws: &mut Workspace, st: &mut FeatureUi, pal: &Palette) -> PanelOut {
    let mut out = PanelOut::default();
    let p = &mut st.agent_panel;
    // A different vault: start over.
    let root = ws.vault_root().map(PathBuf::from);
    if p.vault != root {
        *p = AgentPanelUi { vault: root, ..AgentPanelUi::default() };
    }
    p.drain(ws);
    if p.running.is_some() {
        ui.ctx().request_repaint_after(Duration::from_millis(60));
    }
    let ext = external(ws);
    let model = if ext.is_some() { None } else { p.model(ws) };
    header(ui, ws, p, pal, ext.as_ref(), model.as_ref());
    ui.separator();
    if ext.is_none() && model.is_none() {
        empty_setup(ui, pal, &mut out);
        return out;
    }
    // The composer sits at the bottom at whatever height it needs; the
    // conversation scrolls in the rest.
    egui::Panel::bottom("quire-agent-composer").frame(egui::Frame::NONE).resizable(false).show_separator_line(false).show(ui, |ui| {
        composer(ui, ws, p, pal, ext.as_ref(), model.as_ref());
    });
    egui::CentralPanel::default().frame(egui::Frame::NONE).show(ui, |ui| {
        egui::ScrollArea::vertical().id_salt("quire-agent-convo").auto_shrink([false, false]).stick_to_bottom(true).show(ui, |ui| {
            if let Some(list) = &p.history {
                history_list(ui, ws, p, pal, list.clone());
            } else {
                conversation(ui, ws, p, pal, &mut out);
            }
        });
    });
    if p.pending.is_some() {
        let close = p.pending.as_mut().is_some_and(|pending| acp_ui::permission_modal(ui.ctx(), pending, pal));
        if close {
            p.pending = None;
        }
    }
    out
}

fn header(ui: &mut egui::Ui, ws: &mut Workspace, p: &mut AgentPanelUi, pal: &Palette, ext: Option<&AcpAgent>, model: Option<&PanelModel>) {
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        if let Some(a) = ext {
            egui::Frame::NONE
                .fill(pal.warn.gamma_multiply(0.18))
                .stroke(Stroke::new(1.0, pal.warn))
                .corner_radius(10.0)
                .inner_margin(egui::Margin::symmetric(8, 2))
                .show(ui, |ui| {
                    ui.label(RichText::new(format!("External: {}", a.label)).size(12.0).strong().color(pal.warn))
                        .on_hover_text(quire_assistant::acp::WARNING);
                });
        } else if let Some((m, _, _)) = model {
            ui.label(RichText::new(m.name()).strong().color(pal.strong));
            ui.label(RichText::new(format!("· {}", quire_app::ai::locality_words(m.locality()))).color(pal.muted));
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.menu_button("···", |ui| {
                if ui.add_enabled(p.running.is_none(), egui::Button::new("New conversation")).clicked() {
                    p.new_conversation(ws);
                    ui.close();
                }
                if ui.button("Earlier conversations").clicked() {
                    p.history = Some(ws.vault_root().map(|r| conversation::list(r, now_ms())).unwrap_or_default());
                    ui.close();
                }
                if ui.add_enabled(!p.conv.items.is_empty(), egui::Button::new("Save as note")).clicked() {
                    p.save_as_note(ws);
                    ui.close();
                }
                ui.separator();
                if ext.is_some() {
                    if ui.button("Disconnect the external agent").clicked() {
                        ws.settings.agent_panel.backend = "provider".into();
                        let _ = ws.settings.save();
                        *p.acp.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = None;
                        ui.close();
                    }
                } else if ui.button("Connect an external agent…").clicked() {
                    ws.request_ui(quire_app::workspace::UiRequest::Feature(Command::ConnectExternalAgent));
                    ui.close();
                }
                if ui.button("Set up AI…").clicked() {
                    ws.request_ui(quire_app::workspace::UiRequest::Feature(Command::AiSetup));
                    ui.close();
                }
            });
            if !p.conv.title.is_empty() {
                ui.add(egui::Label::new(RichText::new(&p.conv.title).size(12.0).color(pal.faint)).truncate());
            }
        });
    });
    // What this model may do here (chat only, one change, or every tool).
    if ext.is_none()
        && let Some((m, _, tier)) = model
    {
        ui.label(RichText::new(Capability::for_model(m.as_ref(), *tier).label()).size(11.5).color(pal.faint));
    }
}

fn empty_setup(ui: &mut egui::Ui, pal: &Palette, out: &mut PanelOut) {
    ui.add_space(30.0);
    ui.vertical_centered(|ui| {
        ui.label(RichText::new("The agent needs an AI model").size(16.0).color(pal.strong));
        ui.add_space(4.0);
        ui.add(
            egui::Label::new(
                RichText::new("Use a provider (Claude, OpenAI, Ollama…) or download a model that runs on this computer.").color(pal.muted),
            )
            .wrap(),
        );
        ui.add_space(10.0);
        if ui.button(RichText::new("Set up AI…").strong()).clicked() {
            out.run = Some(Command::AiSetup);
        }
    });
}

fn bubble(ui: &mut egui::Ui, pal: &Palette, text: &str) {
    ui.with_layout(egui::Layout::right_to_left(egui::Align::Min), |ui| {
        egui::Frame::NONE.fill(pal.active_bg).corner_radius(10.0).inner_margin(egui::Margin::symmetric(10, 7)).show(ui, |ui| {
            ui.set_max_width(ui.available_width() * 0.85);
            ui.add(egui::Label::new(RichText::new(text).color(pal.strong)).wrap());
        });
    });
}

fn conversation(ui: &mut egui::Ui, ws: &mut Workspace, p: &mut AgentPanelUi, pal: &Palette, out: &mut PanelOut) {
    ui.add_space(6.0);
    if p.conv.items.is_empty() {
        ui.add_space(20.0);
        ui.vertical_centered(|ui| {
            ui.label(RichText::new("Ask about your notes, or ask for changes.").color(pal.muted));
            ui.label(RichText::new("“Summarize what I wrote about ropes”, “Link this note to related ones”").size(12.0).color(pal.faint));
        });
        return;
    }
    let mut open: Option<NotePath> = None;
    let mut review: Option<String> = None;
    let running = p.running.is_some();
    let n = p.conv.items.len();
    for (i, item) in p.conv.items.iter().enumerate() {
        match item {
            Item::User { text } => {
                ui.add_space(6.0);
                bubble(ui, pal, text);
                ui.add_space(4.0);
            }
            Item::Assistant { text } => {
                CommonMarkViewer::new().show(ui, &mut p.md, text);
                if running && i + 1 == n {
                    ui.label(RichText::new("▍").color(pal.accent));
                }
            }
            Item::Tool { id, summary, detail, is_error, running: busy } => {
                let color = if *is_error { pal.warn } else { pal.muted };
                let icon = if *busy {
                    "⟳"
                } else if *is_error {
                    "⚠"
                } else {
                    "✓"
                };
                egui::CollapsingHeader::new(RichText::new(format!("{icon} {}", strip_md(summary))).size(12.5).color(color))
                    .id_salt(("quire-agent-tool", id, i))
                    .default_open(false)
                    .show(ui, |ui| {
                        if detail.is_empty() {
                            ui.label(RichText::new("No details.").size(12.0).color(pal.faint));
                        } else {
                            egui::ScrollArea::vertical().id_salt(("quire-agent-tool-detail", i)).max_height(160.0).show(ui, |ui| {
                                ui.add(egui::Label::new(RichText::new(detail).monospace().size(11.5).color(pal.text)).wrap());
                            });
                        }
                    });
            }
            Item::Proposal { proposal, path } => {
                ui.add_space(4.0);
                egui::Frame::NONE.fill(pal.panel).stroke(Stroke::new(1.0, pal.added)).corner_radius(8.0).inner_margin(10.0).show(
                    ui,
                    |ui| {
                        ui.set_width(ui.available_width());
                        ui.horizontal(|ui| {
                            ui.label(RichText::new("Suggested change").size(11.0).strong().color(pal.added));
                            ui.label(RichText::new(NotePath::from(path.as_str()).stem()).strong().color(pal.strong));
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                if ui.button("Review").clicked() {
                                    review = Some(proposal.clone());
                                }
                            });
                        });
                        ui.label(
                            RichText::new("Nothing changes until you accept it. It also waits in the Inbox.").size(11.5).color(pal.muted),
                        );
                    },
                );
                ui.add_space(4.0);
            }
            Item::Applied { path } => {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(format!("✎ Changed {}", NotePath::from(path.as_str()).stem())).size(12.5).color(pal.muted));
                    if ui.small_button("Open").clicked() {
                        open = Some(NotePath::from(path.as_str()));
                    }
                });
            }
            Item::Notice { text, error } => {
                ui.add(
                    egui::Label::new(RichText::new(text).size(12.0).italics().color(if *error { pal.danger } else { pal.muted })).wrap(),
                );
            }
        }
    }
    if let Some(id) = review.and_then(|s| ProposalId::parse(&s)) {
        if ws.start_review(&id, None) {
            out.jumped = true;
        } else {
            ws.request_ui(quire_app::workspace::UiRequest::Feature(Command::OpenInbox));
        }
    }
    if let Some(path) = open
        && ws.open_note(&path).is_some()
    {
        out.jumped = true;
    }
}

/// `*x*` → `x` for one-line summaries.
fn strip_md(s: &str) -> String {
    s.replace('*', "")
}

fn history_list(ui: &mut egui::Ui, ws: &Workspace, p: &mut AgentPanelUi, pal: &Palette, list: Vec<Conversation>) {
    ui.horizontal(|ui| {
        ui.label(RichText::new("EARLIER CONVERSATIONS").size(11.0).strong().color(pal.muted));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.small_button("Back").clicked() {
                p.history = None;
            }
        });
    });
    ui.label(RichText::new("Kept on this computer for 30 days after the last message.").size(11.5).color(pal.faint));
    if list.is_empty() {
        ui.label(RichText::new("None yet.").color(pal.muted));
    }
    for c in list {
        let r = crate::theme::row(ui, c.id == p.conv.id, 40.0, pal, |painter, rect| {
            painter.text(
                rect.left_top() + egui::vec2(8.0, 4.0),
                egui::Align2::LEFT_TOP,
                &c.title,
                egui::FontId::proportional(13.5),
                pal.text,
            );
            painter.text(
                rect.left_top() + egui::vec2(8.0, 22.0),
                egui::Align2::LEFT_TOP,
                format!("{} · {}", quire_app::chrono_fmt(c.updated), c.model),
                egui::FontId::proportional(11.0),
                pal.faint,
            );
        });
        if r.clicked() && p.running.is_none() {
            p.conv = c.clone();
            p.history = None;
        }
        r.context_menu(|ui| {
            if ui.button("Delete").clicked() {
                if let Some(root) = ws.vault_root() {
                    conversation::delete(root, &c.id);
                    p.history = Some(conversation::list(root, now_ms()));
                }
                ui.close();
            }
        });
    }
}

fn composer(ui: &mut egui::Ui, ws: &Workspace, p: &mut AgentPanelUi, pal: &Palette, ext: Option<&AcpAgent>, model: Option<&PanelModel>) {
    ui.separator();
    let note = ws.active_doc().and_then(|d| d.path.clone());
    let selection = ws.active_doc().map(|d| d.buffer().slice(d.selection().range())).filter(|s| !s.trim().is_empty());
    ui.horizontal_wrapped(|ui| {
        if let Some(n) = &note {
            chip(ui, pal, &mut p.chip_note, &format!("📄 {}", n.stem()));
        }
        if selection.is_some() {
            chip(ui, pal, &mut p.chip_selection, "✂ Selection");
        }
        if ext.is_none() {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.selectable_value(&mut p.agent_mode, true, "Agent").on_hover_text("Reads your notes and suggests changes");
                ui.selectable_value(&mut p.agent_mode, false, "Ask").on_hover_text("Reads your notes; changes nothing");
            });
        }
    });
    let edit = egui::TextEdit::multiline(&mut p.input)
        .desired_rows(2)
        .desired_width(f32::INFINITY)
        .hint_text("Message the agent…  (Enter sends, Shift+Enter for a new line)");
    let resp = ui.add(edit);
    if p.focus_input {
        resp.request_focus();
        p.focus_input = false;
    }
    let enter = resp.has_focus() && ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Enter));
    if enter && p.input.ends_with('\n') {
        p.input.pop();
    }
    ui.horizontal(|ui| {
        if p.running.is_some() {
            let stopping = p.running.as_ref().is_some_and(|r| r.stop_at.is_some());
            if ui.add_enabled(!stopping, egui::Button::new(if stopping { "Stopping…" } else { "■ Stop" })).clicked() {
                p.stop();
            }
            ui.add(egui::Spinner::new().size(14.0));
        } else {
            let send = ui.add_enabled(!p.input.trim().is_empty(), egui::Button::new(RichText::new("Send").strong()));
            if (send.clicked() || enter) && !p.input.trim().is_empty() {
                let text = std::mem::take(&mut p.input);
                let note_ctx = if p.chip_note { note.as_ref() } else { None };
                let sel_ctx = if p.chip_selection { selection.as_deref() } else { None };
                p.send(ws, text.trim(), note_ctx, sel_ctx, ext.cloned(), model.cloned());
                p.focus_input = true;
            }
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if p.usage.input_tokens + p.usage.output_tokens > 0
                && model.as_ref().is_some_and(|(m, _, _)| m.locality() == quire_app::intel::chat::Locality::Cloud)
            {
                ui.label(
                    RichText::new(format!("{} in · {} out tokens", thousands(p.usage.input_tokens), thousands(p.usage.output_tokens)))
                        .size(11.0)
                        .color(pal.faint),
                );
            }
        });
    });
    let note_line = if ext.is_some() {
        "Its edits through Quire arrive as suggestions in the Inbox. It can still use its own tools."
    } else {
        "Edits to your notes arrive as suggestions in the Inbox."
    };
    ui.label(RichText::new(note_line).size(11.0).color(pal.faint));
}

fn thousands(n: u32) -> String {
    if n >= 10_000 {
        format!("{}k", n / 1000)
    } else if n >= 1000 {
        format!("{:.1}k", f64::from(n) / 1000.0)
    } else {
        n.to_string()
    }
}

fn chip(ui: &mut egui::Ui, pal: &Palette, on: &mut bool, label: &str) {
    let (fill, text) = if *on { (pal.active_bg, pal.strong) } else { (pal.bg, pal.faint) };
    let r = egui::Frame::NONE
        .fill(fill)
        .stroke(Stroke::new(1.0, pal.border))
        .corner_radius(10.0)
        .inner_margin(egui::Margin::symmetric(8, 2))
        .show(ui, |ui| {
            ui.label(RichText::new(label).size(12.0).color(text));
            ui.label(RichText::new(if *on { "×" } else { "+" }).size(11.0).color(pal.muted));
        });
    let r = ui.interact(r.response.rect, ui.scope_id().with(("quire-agent-chip", label)), egui::Sense::click());
    if r.clicked() {
        *on = !*on;
    }
    r.on_hover_text(if *on { "Sent with your message; click to leave it out" } else { "Click to send it with your message" });
}

impl AgentPanelUi {
    fn new_conversation(&mut self, ws: &Workspace) {
        *self = Self {
            vault: ws.vault_root().map(PathBuf::from),
            host: Arc::clone(&self.host),
            acp: Arc::clone(&self.acp),
            agent_mode: self.agent_mode,
            ..Self::default()
        };
    }

    fn save(&mut self, ws: &Workspace) {
        self.conv.updated = now_ms();
        if let Some(root) = ws.vault_root() {
            let _ = conversation::save(root, &self.conv);
        }
    }

    fn save_as_note(&self, ws: &mut Workspace) {
        let date = chrono::Local::now().format("%Y-%m-%d").to_string();
        let text = self.conv.to_markdown(&date);
        let folder = if ws.vault().is_some_and(|v| v.notes().is_ok_and(|n| n.iter().any(|p| p.as_str().starts_with("wiki/")))) {
            "wiki/conversations"
        } else {
            "Conversations"
        };
        let title = quire_assistant::wiki::file_name(&self.conv.title);
        let Some(vault) = ws.vault().cloned() else { return };
        match vault.create_note(folder, &title, &text) {
            Ok(p) => {
                ws.open_note(&p);
                ws.notify(quire_app::NoticeLevel::Info, format!("Saved as {}", p.stem()));
            }
            Err(e) => ws.notify(quire_app::NoticeLevel::Error, format!("Couldn't save the conversation: {e}")),
        }
    }

    fn stop(&mut self) {
        if let Some(r) = &mut self.running {
            r.cancel.cancel();
            r.stop_at = Some(Instant::now());
        }
        if let Some(pending) = &mut self.pending
            && let Some(tx) = pending.reply.take()
        {
            let _ = tx.send(None);
        }
        self.pending = None;
    }

    /// Take in what the run's thread sent.
    fn drain(&mut self, ws: &Workspace) {
        let Some(r) = &self.running else { return };
        let mut finished = None;
        let mut events = Vec::new();
        while let Ok(m) = r.rx.try_recv() {
            match m {
                Msg::Finished { messages, error } => finished = Some((messages, error)),
                other => events.push(other),
            }
        }
        // Stopped but the thread is stuck (a server that stopped answering): let it go.
        let abandoned = finished.is_none() && r.stop_at.is_some_and(|t| t.elapsed() > Duration::from_secs(3));
        for e in events {
            self.apply(e);
        }
        if abandoned {
            if let Some(Item::Assistant { text }) = self.conv.items.last() {
                let t = text.clone();
                self.conv.messages.push(ChatMessage::assistant(t));
            }
            self.conv.items.push(Item::Notice { text: "Stopped.".into(), error: false });
            self.running = None;
            self.save(ws);
            return;
        }
        if let Some((messages, error)) = finished {
            if let Some(m) = messages {
                self.conv.messages = m;
            }
            if let Some(e) = error {
                self.conv.items.push(Item::Notice { text: e, error: true });
            }
            for it in &mut self.conv.items {
                if let Item::Tool { running, .. } = it {
                    *running = false;
                }
            }
            self.running = None;
            self.save(ws);
        }
    }

    fn apply(&mut self, m: Msg) {
        match m {
            Msg::Run(ev) => match ev {
                RunEvent::Text(t) => self.conv.push_text(&t),
                RunEvent::ToolStarted { id, summary, .. } => {
                    self.conv.items.push(Item::Tool { id, summary, detail: String::new(), is_error: false, running: true });
                }
                RunEvent::ToolFinished { id, summary, detail, is_error } => {
                    if let Some(Item::Tool { summary: s, detail: d, is_error: e, running, .. }) =
                        self.conv.items.iter_mut().rev().find(|it| matches!(it, Item::Tool { id: i, .. } if *i == id))
                    {
                        *s = summary;
                        *d = detail;
                        *e = is_error;
                        *running = false;
                    }
                }
                RunEvent::Proposed { proposal, path } => {
                    // One card per proposal (amended writes reuse it).
                    if !self.conv.items.iter().any(|it| matches!(it, Item::Proposal { proposal: p, .. } if *p == proposal)) {
                        self.conv.items.push(Item::Proposal { proposal, path });
                    }
                }
                RunEvent::Applied { path } => self.conv.items.push(Item::Applied { path }),
                RunEvent::Done { usage, .. } => {
                    self.usage.input_tokens += usage.input_tokens;
                    self.usage.output_tokens += usage.output_tokens;
                }
                RunEvent::Stopped => self.conv.items.push(Item::Notice { text: "Stopped.".into(), error: false }),
                RunEvent::Failed(e) => self.conv.items.push(Item::Notice { text: e, error: true }),
            },
            Msg::Acp(ev) => match ev {
                AcpEvent::Text(t) => self.conv.push_text(&t),
                AcpEvent::Thought(_) => {}
                AcpEvent::ToolCall { id, title, kind, status } => {
                    let summary = if title.is_empty() { kind } else { title };
                    self.conv.items.push(Item::Tool {
                        id,
                        summary,
                        detail: String::new(),
                        is_error: status == "failed",
                        running: status != "completed" && status != "failed",
                    });
                }
                AcpEvent::ToolUpdate { id, status, detail } => {
                    if let Some(Item::Tool { detail: d, is_error, running, .. }) =
                        self.conv.items.iter_mut().rev().find(|it| matches!(it, Item::Tool { id: i, .. } if *i == id))
                    {
                        if !detail.is_empty() {
                            *d = detail;
                        }
                        if !status.is_empty() {
                            *running = status != "completed" && status != "failed";
                            *is_error = status == "failed";
                        }
                    }
                }
                AcpEvent::Plan(entries) => {
                    let text = entries
                        .iter()
                        .map(|(c, s)| format!("{} {c}", if s == "completed" { "☑" } else { "☐" }))
                        .collect::<Vec<_>>()
                        .join("\n");
                    self.conv.items.push(Item::Notice { text: format!("Plan:\n{text}"), error: false });
                }
                AcpEvent::Write { path, proposal } => match proposal {
                    Some(p) => {
                        if !self.conv.items.iter().any(|it| matches!(it, Item::Proposal { proposal: q, .. } if *q == p)) {
                            self.conv.items.push(Item::Proposal { proposal: p, path });
                        }
                    }
                    None => self.conv.items.push(Item::Applied { path }),
                },
                AcpEvent::Refused { path, why } => {
                    self.conv.items.push(Item::Notice { text: format!("Refused {path}: {why}"), error: true });
                }
                AcpEvent::Permission { title, allowed, .. } => {
                    self.conv.items.push(Item::Notice {
                        text: format!("{} “{title}”", if allowed { "You allowed" } else { "Denied:" }),
                        error: !allowed,
                    });
                }
            },
            Msg::Permission(req, reply) => {
                self.pending = Some(Pending { req, reply: Some(reply) });
            }
            Msg::Finished { .. } => {}
        }
    }

    fn send(
        &mut self,
        ws: &Workspace,
        text: &str,
        note: Option<&NotePath>,
        selection: Option<&str>,
        ext: Option<AcpAgent>,
        model: Option<PanelModel>,
    ) {
        let (Some(vault), Some(agent)) = (ws.vault().cloned(), ws.agent_store().cloned()) else { return };
        let root = ws.vault_root().map(PathBuf::from);
        self.history = None;
        self.conv.set_title_from(text);
        self.conv.items.push(Item::User { text: text.to_string() });
        let (tx, rx) = channel();
        let cancel = CancelFlag::new();
        self.running = Some(Running { rx, cancel: cancel.clone(), stop_at: None });
        let device = ws.device().id.clone();
        if let Some(a) = ext {
            self.conv.model = format!("External: {}", a.label);
            let Some(root) = root else {
                let _ = tx.send(Msg::Finished { messages: None, error: Some("An external agent needs a vault folder on disk.".into()) });
                return;
            };
            let mut prompt = runner::context_message(note.map(NotePath::as_str), selection, None);
            prompt.push_str(text);
            let slot = Arc::clone(&self.acp);
            std::thread::spawn(move || run_acp(&a, &root, &prompt, &agent, &device, &slot, &cancel, &tx));
            return;
        }
        let Some((model, provider, tier)) = model else { return };
        self.conv.model = model.name().to_string();
        let cap = Capability::for_model(model.as_ref(), tier);
        let note_text = if cap == Capability::ChatOnly {
            note.and_then(|n| {
                ws.docs().iter().find(|d| d.path.as_ref() == Some(n)).map(|d| d.text().to_string()).or_else(|| vault.read(n).ok())
            })
        } else {
            None
        };
        let mut content = runner::context_message(note.map(NotePath::as_str), selection, note_text.as_deref());
        if !content.is_empty() {
            content.push('\n');
        }
        content.push_str(text);
        self.conv.messages.push(ChatMessage::user(content));
        let mut messages = self.conv.messages.clone();
        let host_slot = Arc::clone(&self.host);
        let max_steps = ws.settings.agent_panel.max_steps.max(1);
        let read_only = !self.agent_mode;
        let label = format!("Agent panel ({})", model.name());
        std::thread::spawn(move || {
            let actor = runner::panel_actor(&provider, &label, Some(model.name()), &device);
            runner::grant(&agent, &actor, cap);
            let host = if cap == Capability::ChatOnly {
                None
            } else {
                let mut slot = host_slot.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                let key = root.clone().unwrap_or_default();
                match slot.as_ref() {
                    Some((k, h)) if *k == key => Some(Arc::clone(h)),
                    _ => {
                        let h = Arc::new(ToolHost::open(vault, root.as_deref(), agent.clone()));
                        *slot = Some((key, Arc::clone(&h)));
                        Some(h)
                    }
                }
            };
            let core = model.locality() == quire_app::intel::chat::Locality::Device;
            let spec = RunSpec {
                model: model.as_ref(),
                host: host.as_deref(),
                actor: &actor,
                capability: cap,
                core_tools: core,
                read_only,
                limits: RunLimits { max_steps, max_tokens: 0 },
                system: runner::system_for(&agent, !read_only && cap != Capability::ChatOnly, cap != Capability::ChatOnly),
                max_reply_tokens: if core { 1024 } else { 2048 },
            };
            runner::run(&spec, &mut messages, &cancel, &mut |ev| {
                let _ = tx.send(Msg::Run(ev));
            });
            let _ = tx.send(Msg::Finished { messages: Some(messages), error: None });
        });
    }
}

/// One message to the external agent, starting it (and its session) if needed.
fn run_acp(
    a: &AcpAgent,
    root: &std::path::Path,
    prompt: &str,
    agent: &quire_app::agent::AgentStore,
    device: &str,
    slot: &Mutex<Option<AcpSlot>>,
    cancel: &CancelFlag,
    tx: &Sender<Msg>,
) {
    let key = (a.command.clone(), a.args.clone(), root.to_path_buf());
    let taken = slot.lock().unwrap_or_else(std::sync::PoisonError::into_inner).take().filter(|s| s.key == key);
    let tx_ev = tx.clone();
    let tx_ask = tx.clone();
    let mut events = move |ev: AcpEvent| {
        let _ = tx_ev.send(Msg::Acp(ev));
    };
    let mut ask = move |req: &PermissionRequest, cancel: &CancelFlag| -> PermissionAnswer {
        let (rtx, rrx) = channel();
        if tx_ask.send(Msg::Permission(req.clone(), rtx)).is_err() {
            return None;
        }
        loop {
            match rrx.recv_timeout(Duration::from_millis(100)) {
                Ok(a) => return a,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) if !cancel.is_cancelled() => {}
                _ => return None,
            }
        }
    };
    let mut host = VaultHost { agent, actor: a.actor(device), root: root.to_path_buf(), ask: &mut ask, events: &mut events };
    let mut s = if let Some(s) = taken {
        s
    } else {
        let mut client = match AcpClient::spawn(a, root) {
            Ok(c) => c,
            Err(e) => {
                let _ = tx.send(Msg::Finished { messages: None, error: Some(e) });
                return;
            }
        };
        let cli = std::env::current_exe()
            .ok()
            .and_then(|e| e.parent().map(|d| d.join(if cfg!(windows) { "quire-cli.exe" } else { "quire-cli" })))
            .filter(|p| p.is_file());
        let key_id = a.actor_id();
        if let Err(e) = client.start_session(root, cli.as_deref().map(|c| (c, key_id.as_str())), &mut host) {
            let _ = tx.send(Msg::Finished { messages: None, error: Some(format!("Couldn't start {}: {e}", a.label)) });
            return;
        }
        AcpSlot { key, client }
    };
    let r = s.client.prompt(prompt, &mut host, cancel);
    let error = match r {
        Ok(stop) if stop == "cancelled" => Some("Stopped.".to_string()),
        Ok(_) => None,
        Err(e) => Some(e),
    };
    // Keep the session for the next message unless it broke.
    if error.as_deref().is_none_or(|e| e == "Stopped.") {
        *slot.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(s);
    }
    let _ = tx.send(Msg::Finished { messages: None, error });
}
