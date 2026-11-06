// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Settings → Agents → Connections (0.11): every agent that has used this
//! vault, its activity log, its permissions, limiting it to tags or
//! folders, and revoking it in one click (`quire_app::connections`).

use std::time::{Duration, Instant};

use egui::{RichText, TextEdit};
use quire_app::Workspace;
use quire_app::agent::policy::Mode;
use quire_app::connections::{self, ActivityKind, Connection, Kind, TokenInfo};
use quire_app::workspace::NoticeLevel;

use crate::panels::PanelOut;
use crate::theme::Palette;

/// Something to do after drawing (a revoke, a restore).
type Later = Box<dyn FnOnce(&mut Workspace, &mut ConnectionsUi)>;

/// UI state, kept in the Agents tab's state.
pub(crate) struct ConnectionsUi {
    cache: Option<(Instant, Vec<Connection>)>,
    /// The connection whose activity and permissions are open.
    open: Option<String>,
    kinds: [bool; 4],
    filter: String,
    confirm_revoke: Option<String>,
    /// (key, tags, folders) being edited.
    limits: Option<(String, String, String)>,
}

impl Default for ConnectionsUi {
    fn default() -> Self {
        Self { cache: None, open: None, kinds: [true; 4], filter: String::new(), confirm_revoke: None, limits: None }
    }
}

fn tokens() -> Vec<TokenInfo> {
    if cfg!(test) {
        return Vec::new();
    }
    let now = chrono::Utc::now().timestamp_millis();
    quire_mcp::tokens::TokenStore::default_store()
        .map(|s| {
            s.summary(now)
                .into_iter()
                .map(|r| TokenInfo { actor: r.actor, label: r.name, scopes: r.scopes, created: r.created, last_used: r.last_used })
                .collect()
        })
        .unwrap_or_default()
}

fn ago(ms: Option<i64>) -> String {
    let Some(ms) = ms else { return "never used".into() };
    let secs = (chrono::Utc::now().timestamp_millis() - ms).max(0) / 1000;
    match secs {
        0..60 => "just now".into(),
        60..3600 => format!("{} min ago", secs / 60),
        3600..86_400 => format!("{} h ago", secs / 3600),
        _ => quire_app::chrono_fmt(ms),
    }
}

fn mode_word(m: Mode) -> &'static str {
    match m {
        Mode::Deny => "no access",
        Mode::Read => "read",
        Mode::Propose => "suggest",
        Mode::CreateOnly => "add new notes",
        Mode::Direct => "write",
    }
}

pub(crate) fn show(ui: &mut egui::Ui, ws: &mut Workspace, st: &mut ConnectionsUi, pal: &Palette) -> PanelOut {
    let out = PanelOut::default();
    let Some(store) = ws.agent_store().cloned() else { return out };
    let now = chrono::Utc::now().timestamp_millis();
    if st.cache.as_ref().is_none_or(|(at, _)| at.elapsed() > Duration::from_secs(2)) {
        st.cache = Some((Instant::now(), connections::gather(&store, &tokens(), now)));
    }
    let list = st.cache.as_ref().map(|c| c.1.clone()).unwrap_or_default();
    ui.add(
        egui::Label::new(
            RichText::new(
                "Every agent that used this vault in the last 90 days, and apps with a token. Revoking takes effect at its next request.",
            )
            .color(pal.muted),
        )
        .wrap(),
    );
    if quire_app::secure_state::ai_off(ws) {
        ui.label(RichText::new("AI is off for this vault: none of these can use it now.").color(pal.warn));
    }
    ui.add_space(6.0);
    if list.is_empty() {
        ui.label(RichText::new("No agent has used this vault yet. Connect one in Settings → Connections.").color(pal.muted));
        return out;
    }
    let mut later: Option<Later> = None;
    for c in &list {
        egui::Frame::NONE.stroke(egui::Stroke::new(1.0, pal.border)).corner_radius(6.0).inner_margin(10.0).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new(&c.label).strong().color(pal.strong));
                ui.label(RichText::new(c.key.as_str()).monospace().size(11.0).color(pal.faint));
                if c.revoked {
                    ui.label(RichText::new("Revoked").color(pal.danger).strong());
                }
                if c.unverified {
                    ui.label(RichText::new("unverified name").size(11.5).color(pal.warn))
                        .on_hover_text("It started without a --client key, so it is known by the name it reports. Policy can narrow it but never widen it.");
                }
            });
            let mut facts = vec![c.kind.label().to_string(), ago(c.last_used)];
            facts.push(format!("{} read{}", c.reads, if c.reads == 1 { "" } else { "s" }));
            facts.push(format!("{} write{}", c.writes, if c.writes == 1 { "" } else { "s" }));
            if c.pending > 0 {
                facts.push(format!("{} suggestion{} waiting", c.pending, if c.pending == 1 { "" } else { "s" }));
            }
            if c.tokens > 0 {
                facts.push(format!("{} token{} ({})", c.tokens, if c.tokens == 1 { "" } else { "s" }, c.scopes.join(", ")));
            }
            if !c.devices.is_empty() {
                facts.push(format!("also on {} other device{}", c.devices.len(), if c.devices.len() == 1 { "" } else { "s" }));
            }
            ui.add(egui::Label::new(RichText::new(facts.join(" · ")).size(12.5).color(pal.muted)).wrap());
            if c.allow_tags.is_some() || c.allow_folders.is_some() {
                let mut what = Vec::new();
                if let Some(t) = &c.allow_tags {
                    what.extend(t.iter().map(|t| format!("#{t}")));
                }
                if let Some(f) = &c.allow_folders {
                    what.extend(f.iter().cloned());
                }
                ui.label(RichText::new(format!("Only sees: {}", what.join(", "))).size(12.5).color(pal.accent));
            }
            ui.horizontal_wrapped(|ui| {
                let open = st.open.as_deref() == Some(c.key.as_str());
                if ui.selectable_label(open, "Activity and permissions").clicked() {
                    st.open = if open { None } else { Some(c.key.clone()) };
                }
                if ui.button("Limit to tags or folders…").clicked() {
                    st.limits = Some((
                        c.key.clone(),
                        c.allow_tags.clone().unwrap_or_default().join(", "),
                        c.allow_folders.clone().unwrap_or_default().join(", "),
                    ));
                }
                if c.revoked {
                    if ui.button("Restore access").on_hover_text("Other devices ask before they apply this.").clicked() {
                        let c = c.clone();
                        later = Some(Box::new(move |ws, st| {
                            let r = ws.agent_store().cloned().ok_or_else(|| "no vault".to_string()).and_then(|s| connections::restore(&s, &c));
                            match r {
                                Ok(()) => ws.notify(NoticeLevel::Info, format!("{} can use this vault again", c.label)),
                                Err(e) => ws.notify(NoticeLevel::Error, e),
                            }
                            st.cache = None;
                        }));
                    }
                } else if st.confirm_revoke.as_deref() == Some(c.key.as_str()) {
                    let what = if c.kind.uses_tokens() { "Its tokens are deleted" } else { "Its next start is refused" };
                    ui.label(RichText::new(format!("Revoke {}? {what}.", c.label)).color(pal.danger));
                    if ui.button(RichText::new("Revoke").color(pal.danger)).clicked() {
                        let c = c.clone();
                        later = Some(Box::new(move |ws, st| {
                            revoke(ws, &c);
                            st.confirm_revoke = None;
                            st.cache = None;
                        }));
                    }
                    if ui.button("Cancel").clicked() {
                        st.confirm_revoke = None;
                    }
                } else if ui.button(RichText::new("Revoke").color(pal.danger)).clicked() {
                    st.confirm_revoke = Some(c.key.clone());
                }
            });
            if st.limits.as_ref().is_some_and(|(k, _, _)| *k == c.key) {
                limits_editor(ui, ws, st, c, pal);
            }
            if st.open.as_deref() == Some(c.key.as_str()) {
                ui.separator();
                details(ui, ws, st, c, pal);
            }
        });
        ui.add_space(6.0);
    }
    if let Some(f) = later {
        f(ws, st);
    }
    out
}

/// Revoke: tokens deleted (HTTP), and the policy denies the key everywhere.
fn revoke(ws: &mut Workspace, c: &Connection) {
    let Some(store) = ws.agent_store().cloned() else { return };
    let mut msg = Vec::new();
    if c.kind == Kind::OAuth || c.kind == Kind::Token {
        match quire_mcp::tokens::TokenStore::default_store().map(|s| s.revoke_actor(&c.key)) {
            Some(Ok(n)) => msg.push(format!("{n} token{} deleted", if n == 1 { "" } else { "s" })),
            Some(Err(e)) => {
                ws.notify(NoticeLevel::Error, format!("Couldn't delete its tokens: {e}"));
                return;
            }
            None => {}
        }
    }
    match connections::revoke(&store, c) {
        Ok(()) => {
            msg.insert(0, format!("Revoked {}", c.label));
            if !c.kind.uses_tokens() {
                msg.push("its next start is refused and its reads are logged".into());
            }
            ws.notify(NoticeLevel::Info, msg.join("; "));
        }
        Err(e) => ws.notify(NoticeLevel::Error, e),
    }
}

fn limits_editor(ui: &mut egui::Ui, ws: &mut Workspace, st: &mut ConnectionsUi, c: &Connection, pal: &Palette) {
    let Some((_, tags, folders)) = st.limits.as_mut() else { return };
    ui.separator();
    ui.label(
        RichText::new("It sees only notes with one of these tags, or in one of these folders. Leave both empty for no limit.")
            .color(pal.muted),
    );
    ui.horizontal_wrapped(|ui| {
        ui.label("Tags");
        ui.add(TextEdit::singleline(tags).hint_text("work, project/quire").desired_width(ui.available_width().min(240.0)));
    });
    ui.horizontal_wrapped(|ui| {
        ui.label("Folders");
        ui.add(TextEdit::singleline(folders).hint_text("Projects/, wiki/").desired_width(ui.available_width().min(240.0)));
    });
    let (mut save, mut cancel) = (false, false);
    ui.horizontal(|ui| {
        save = ui.button("Save").clicked();
        cancel = ui.button("Cancel").clicked();
    });
    if save {
        let (t, f) = (connections::parse_list(tags, false), connections::parse_list(folders, true));
        let r = ws
            .agent_store()
            .cloned()
            .ok_or_else(|| "no vault".to_string())
            .and_then(|s| connections::set_allow(&s, c, t.as_deref(), f.as_deref()));
        match r {
            Ok(()) => ws.notify(NoticeLevel::Info, format!("Saved what {} may see", c.label)),
            Err(e) => ws.notify(NoticeLevel::Error, e),
        }
        st.cache = None;
    }
    if save || cancel {
        st.limits = None;
    }
}

fn details(ui: &mut egui::Ui, ws: &Workspace, st: &mut ConnectionsUi, c: &Connection, pal: &Palette) {
    let Some(store) = ws.agent_store().cloned() else { return };
    ui.label(RichText::new("PERMISSIONS").size(11.0).color(pal.muted).strong());
    for (place, read, write) in connections::permissions(&store, c) {
        let what =
            if read == Mode::Deny { "no access".to_string() } else { format!("{} · changes: {}", mode_word(read), mode_word(write)) };
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new(place).strong().size(12.5).color(pal.text));
            ui.label(RichText::new(what).size(12.5).color(pal.muted));
        });
    }
    ui.label(RichText::new("Locked folders, .quire/ and anything AI-off are never visible to agents.").size(11.5).color(pal.faint));
    ui.add_space(8.0);
    ui.label(RichText::new("ACTIVITY").size(11.0).color(pal.muted).strong());
    ui.horizontal_wrapped(|ui| {
        for (i, k) in ActivityKind::ALL.iter().enumerate() {
            ui.checkbox(&mut st.kinds[i], k.label());
        }
        ui.add(TextEdit::singleline(&mut st.filter).hint_text("filter by path").desired_width(ui.available_width().min(160.0)));
    });
    if !store.logs_reads_of(&c.actor()) {
        ui.label(RichText::new("Reads aren't logged on this device (Rules → Log what agents read).").size(11.5).color(pal.faint));
    }
    let kinds: Vec<ActivityKind> = ActivityKind::ALL.iter().zip(st.kinds).filter(|(_, on)| *on).map(|(k, _)| *k).collect();
    let now = chrono::Utc::now().timestamp_millis();
    let entries = connections::activity(&store, &c.key, &kinds, &st.filter, now, 200);
    if entries.is_empty() {
        ui.label(RichText::new("Nothing to show.").color(pal.muted));
    }
    egui::ScrollArea::vertical().id_salt(("quire-conn-log", &c.key)).max_height(220.0).show(ui, |ui| {
        for e in &entries {
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new(quire_app::chrono_fmt(e.at)).monospace().size(11.0).color(pal.faint));
                ui.add(egui::Label::new(RichText::new(connections::describe(e)).size(12.5).color(pal.text)).wrap());
            });
        }
    });
}

#[cfg(test)]
mod tests {
    use egui::{Pos2, Rect, Vec2};
    use quire_app::agent::{Actor, Via};
    use quire_app::vault::{NotePath, Vault};

    use super::*;

    /// The Connections view with a few connections (one revoked, one
    /// limited, one open on its activity) keeps its size, narrow and wide,
    /// light and dark.
    #[test]
    fn connections_view_keeps_its_size() {
        let mut ws = Workspace::new(quire_app::Settings::default());
        ws.attach_vault(Vault::in_memory(&[("Plan.md", "# Plan\n#work\n"), ("wiki/Ropes.md", "# Ropes\n")]));
        let store = ws.agent_store().cloned().unwrap();
        store.update_device_state(|d| d.log_reads = true).unwrap();
        let claude = Actor::agent("mcp:claude-code", "Claude Code", Via::McpStdio, "d");
        for _ in 0..30 {
            store.record_read(&claude, &NotePath::new("wiki/Ropes.md"));
        }
        let cursor = Actor::unverified_agent("mcp:cursor", "Cursor with a long name that wraps", Via::McpStdio, "d");
        store.record_read(&cursor, &NotePath::new("Plan.md"));
        let list = connections::gather(&store, &[], chrono::Utc::now().timestamp_millis());
        let c = list.iter().find(|c| c.key == "mcp:cursor?").unwrap().clone();
        connections::revoke(&store, &c).unwrap();
        let k = list.iter().find(|c| c.key == "mcp:claude-code").unwrap().clone();
        connections::set_allow(&store, &k, Some(&["work".to_string()]), Some(&["wiki/".to_string()])).unwrap();
        for (pal, width) in [(&crate::theme::LIGHT, 220.0_f32), (&crate::theme::DARK, 480.0)] {
            let mut st = ConnectionsUi {
                open: Some("mcp:claude-code".into()),
                confirm_revoke: Some("mcp:claude-code".into()),
                ..ConnectionsUi::default()
            };
            let ctx = egui::Context::default();
            crate::theme::install_fonts(&ctx);
            let mut sizes = Vec::new();
            for _ in 0..20 {
                let mut out = ctx.run_ui(
                    egui::RawInput { screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(1280.0, 820.0))), ..Default::default() },
                    |ui| {
                        let r = ui.allocate_ui(Vec2::new(width, 700.0), |ui| {
                            let _ = show(ui, &mut ws, &mut st, pal);
                        });
                        sizes.push(r.response.rect.size());
                    },
                );
                out.textures_delta.clear();
            }
            assert!(sizes[10..].iter().all(|s| (s.y - sizes[10].y).abs() <= 0.5 && s.x <= width + 0.5), "{width}: {:?}", &sizes[10..]);
        }
    }
}
