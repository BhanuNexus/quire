// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Settings → Agents: zones, connected clients, the external agent, activity logging (design §7.2).
//!
//! Owned by WS-A. Edits `.quire/shared/agent-policy.toml` in place (comments
//! and anything Quire doesn't know survive) and approves the result on this
//! device. A policy changed elsewhere is shown with what it would allow and
//! waits for approval here.

use egui::{RichText, Stroke};
use quire_app::Workspace;
use quire_app::agent::policy::{Mode, ZoneRole};
use quire_app::agent::policy_edit::PolicyDoc;
use quire_app::agent::{ActorKind, Policy};
use quire_app::workspace::NoticeLevel;

use crate::panels::PanelOut;
use crate::registry::FeatureUi;
use crate::theme::Palette;

/// A change to the policy file, made after drawing.
type PolicyEdit = Box<dyn FnOnce(&mut PolicyDoc)>;
/// Something to do with the workspace after drawing.
type Later = Box<dyn FnOnce(&mut Workspace)>;

/// UI state kept between frames (in [`FeatureUi`]).
pub(crate) struct AgentsSettingsUi {
    new_folder: String,
    new_role: ZoneRole,
    error: Option<String>,
    show_file: bool,
}

impl Default for AgentsSettingsUi {
    fn default() -> Self {
        Self { new_folder: String::new(), new_role: ZoneRole::Agent, error: None, show_file: false }
    }
}

/// Whether the tab is offered.
pub(crate) const fn available(ws: &Workspace) -> bool {
    ws.agent_store().is_some()
}

fn heading(ui: &mut egui::Ui, pal: &Palette, title: &str) {
    ui.add_space(10.0);
    ui.label(RichText::new(title.to_uppercase()).size(11.0).color(pal.muted).strong());
    ui.add_space(2.0);
}

/// What a client's own `default` means, in the list's words.
const CLIENT_MODES: [(Option<Mode>, &str); 4] = [
    (None, "Follows the zones"),
    (Some(Mode::Propose), "Suggestions only"),
    (Some(Mode::Read), "Read only"),
    (Some(Mode::Deny), "No access"),
];

const DEFAULT_MODES: [(Mode, &str); 4] =
    [(Mode::Propose, "Suggest changes"), (Mode::Read, "Only read"), (Mode::Direct, "Write directly"), (Mode::Deny, "Not see them")];

pub(crate) fn show(ui: &mut egui::Ui, ws: &mut Workspace, st: &mut FeatureUi, pal: &Palette) -> PanelOut {
    let st = &mut st.agents_settings;
    let Some(store) = ws.agent_store().cloned() else {
        ui.label(RichText::new("Open a vault first; agent settings are per vault.").color(pal.muted));
        return PanelOut::default();
    };
    let state = store.policy_state();
    let max_h = (ui.ctx().content_rect().height() * 0.62).clamp(200.0, 520.0);
    let mut edit: Option<PolicyEdit> = None;
    let mut after: Option<Later> = None;
    egui::ScrollArea::vertical().id_salt("quire-agents-settings").max_height(max_h).auto_shrink([false, true]).show(ui, |ui| {
        ui.set_width(ui.available_width());
        if let Some(e) = &st.error {
            ui.add(egui::Label::new(RichText::new(e).color(pal.danger)).wrap());
        }
        let file_policy = state.file_policy().cloned();
        let newer = file_policy.as_ref().is_some_and(Policy::is_newer);
        if let Some(Err(e)) = &state.file {
            ui.add(
                egui::Label::new(
                    RichText::new(format!(
                        "The agent policy file can't be read ({e}). Agents get the last approved rules until it is fixed. Saving here replaces it."
                    ))
                    .color(pal.warn),
                )
                .wrap(),
            );
        }
        if newer {
            ui.add(
                egui::Label::new(RichText::new("This vault's agent policy was made by a newer Quire. It is shown here but can't be changed.").color(pal.warn)).wrap(),
            );
        }
        if state.needs_review() {
            egui::Frame::NONE.fill(pal.warn.gamma_multiply(0.15)).corner_radius(6.0).inner_margin(8.0).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.add(
                    egui::Label::new(
                        RichText::new("Review policy changes: the agent policy changed on another device or outside Quire. It isn't in effect here until you approve it.")
                            .color(pal.text),
                    )
                    .wrap(),
                );
                for w in &state.widenings {
                    ui.add(egui::Label::new(RichText::new(format!("• {}", w.what)).size(12.5).color(pal.text)).wrap());
                }
                ui.horizontal_wrapped(|ui| {
                    if ui.button("Approve on this device").clicked() {
                        after = Some(Box::new(|ws: &mut Workspace| {
                            if let Some(s) = ws.agent_store().cloned()
                                && let Err(e) = s.approve_policy()
                            {
                                ws.notify(NoticeLevel::Error, e);
                            }
                        }));
                    }
                    if ui.small_button(if st.show_file { "Hide the file" } else { "Show the file" }).clicked() {
                        st.show_file = !st.show_file;
                    }
                });
                if st.show_file {
                    crate::inbox_ui::diff_view(ui, &state.baseline_text, state.file_text.as_deref().unwrap_or_default(), pal, "settings-policy");
                }
            });
        }
        let shown = file_policy.unwrap_or_else(|| state.baseline.clone());
        let can_edit = !newer;

        heading(ui, pal, "Zones");
        ui.add(egui::Label::new(RichText::new(shown.sentence()).color(pal.text)).wrap());
        ui.add_space(4.0);
        // Path and role side by side when there's room, else one under the other.
        let narrow = ui.available_width() < 380.0;
        let combo_w = (ui.available_width() - 40.0).clamp(100.0, 210.0);
        let row = |ui: &mut egui::Ui, label: &mut dyn FnMut(&mut egui::Ui), control: &mut dyn FnMut(&mut egui::Ui)| {
            if narrow {
                ui.horizontal(|ui| label(ui));
                ui.horizontal(|ui| control(ui));
                ui.add_space(4.0);
            } else {
                ui.horizontal(|ui| {
                    ui.allocate_ui(egui::vec2(150.0, 20.0), |ui| ui.horizontal(|ui| label(ui)));
                    control(ui);
                });
            }
        };
        for z in &shown.zones {
            let mut role = z.role;
            let mut remove = false;
            row(
                ui,
                &mut |ui| {
                    ui.add(egui::Label::new(RichText::new(&z.path).monospace()).truncate());
                    if can_edit && ui.small_button("✕").on_hover_text("Remove this zone").clicked() {
                        remove = true;
                    }
                },
                &mut |ui| {
                    ui.add_enabled_ui(can_edit, |ui| {
                        egui::ComboBox::from_id_salt(("quire-zone-role", &z.path)).selected_text(role.label()).width(combo_w).show_ui(ui, |ui| {
                            for r in ZoneRole::ALL {
                                ui.selectable_value(&mut role, r, r.label());
                            }
                        });
                    });
                },
            );
            let path = z.path.clone();
            if remove {
                edit = Some(Box::new(move |d: &mut PolicyDoc| d.remove_zone(&path)));
            } else if role != z.role {
                edit = Some(Box::new(move |d: &mut PolicyDoc| d.set_zone(&path, role, role.default_mode())));
            }
        }
        let mut m = shown.default;
        row(
            ui,
            &mut |ui| {
                ui.label(RichText::new("Everything else").color(pal.muted));
            },
            &mut |ui| {
                ui.add_enabled_ui(can_edit, |ui| {
                    let current = DEFAULT_MODES.iter().find(|(x, _)| *x == m).map_or_else(|| m.describe(), |(_, l)| l);
                    egui::ComboBox::from_id_salt("quire-zone-default").selected_text(format!("Agents {}", current.to_lowercase())).width(combo_w).show_ui(ui, |ui| {
                        for (mode, l) in DEFAULT_MODES {
                            ui.selectable_value(&mut m, mode, format!("Agents {}", l.to_lowercase()));
                        }
                    });
                });
            },
        );
        if m != shown.default {
            edit = Some(Box::new(move |d: &mut PolicyDoc| d.set_default(m)));
        }
        if can_edit {
            ui.add_space(4.0);
            let folders = ws.vault().map(quire_app::vault::Vault::folders).unwrap_or_default();
            let free: Vec<&String> = folders.iter().filter(|f| !shown.zones.iter().any(|z| z.path.trim_end_matches('/').eq_ignore_ascii_case(f))).collect();
            ui.horizontal(|ui| {
                let shown_folder = if st.new_folder.is_empty() { "Add a folder…".to_string() } else { format!("{}/", st.new_folder) };
                egui::ComboBox::from_id_salt("quire-zone-new").selected_text(shown_folder).width(combo_w.min(160.0)).show_ui(ui, |ui| {
                    if free.is_empty() {
                        ui.label("Every folder has a zone");
                    }
                    for f in &free {
                        ui.selectable_value(&mut st.new_folder, (*f).clone(), format!("{f}/"));
                    }
                });
            });
            if !st.new_folder.is_empty() {
                ui.horizontal(|ui| {
                    egui::ComboBox::from_id_salt("quire-zone-new-role").selected_text(st.new_role.label()).width(combo_w.min(180.0)).show_ui(ui, |ui| {
                        for r in ZoneRole::ALL {
                            ui.selectable_value(&mut st.new_role, r, r.label());
                        }
                    });
                });
                ui.horizontal(|ui| {
                    if ui.button("Add zone").clicked() {
                        let (path, role) = (format!("{}/", st.new_folder.trim_matches('/')), st.new_role);
                        st.new_folder.clear();
                        edit = Some(Box::new(move |d: &mut PolicyDoc| d.set_zone(&path, role, role.default_mode())));
                    }
                    if ui.button("Cancel").clicked() {
                        st.new_folder.clear();
                    }
                });
            }
        }
        ui.add(
            egui::Label::new(
                RichText::new("Agent-owned: agents write directly. Sources: agents add captured pages but never change them. Your notes: agents only suggest changes, which you review. Private: agents can't see it.")
                    .size(12.0)
                    .color(pal.muted),
            )
            .wrap(),
        );

        heading(ui, pal, "Agents");
        clients(ui, ws, &shown, can_edit, pal, &mut edit);

        heading(ui, pal, "Connect an agent");
        connect(ui, ws, pal);

        heading(ui, pal, "Agent instructions");
        instructions(ui, ws, pal, &mut after);

        heading(ui, pal, "Activity log");
        let mut reads = store.device_state().log_reads;
        let changed = ui.checkbox(&mut reads, "Log which notes agents read").changed();
        ui.label(RichText::new("Kept for 30 days, on this computer only.").size(12.0).color(pal.muted));
        if changed {
            let r = store.update_device_state(|s| s.log_reads = reads);
            if let Err(e) = r {
                st.error = Some(format!("Couldn't save: {e}"));
            }
        }
        ui.add_space(6.0);
    });
    if let Some(f) = edit {
        let mut doc = match PolicyDoc::parse(state.file_text.as_deref().filter(|_| state.file_policy().is_some())) {
            Ok(d) => d,
            Err(e) => {
                st.error = Some(format!("The policy file can't be changed: {e}"));
                return PanelOut::default();
            }
        };
        f(&mut doc);
        match store.write_policy(&doc.text()) {
            Ok(()) => st.error = None,
            Err(e) => st.error = Some(e),
        }
        ws.features.agent.reload_inbox_soon();
    }
    if let Some(f) = after {
        f(ws);
        ws.features.agent.reload_inbox_soon();
    }
    PanelOut::default()
}

/// Agents this vault knows: from the policy, the activity log and the inbox.
fn clients(ui: &mut egui::Ui, ws: &Workspace, shown: &Policy, can_edit: bool, pal: &Palette, edit: &mut Option<PolicyEdit>) {
    let mut known: Vec<(String, String)> = shown.clients.keys().map(|k| (k.clone(), k.clone())).collect();
    for r in ws.features.agent.activity.iter().filter(|r| r.kind == ActorKind::Agent) {
        known.push((r.actor_id.clone(), r.actor_label.clone()));
    }
    for p in ws.features.agent.inbox.pending.iter().filter(|p| p.actor.kind == ActorKind::Agent) {
        known.push((p.actor.id.clone(), p.actor.label.clone()));
    }
    known.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| b.1.len().cmp(&a.1.len())));
    known.dedup_by(|a, b| a.0 == b.0);
    if known.is_empty() {
        ui.add(
            egui::Label::new(
                RichText::new("No agent has connected yet. When one does, you can limit it here.").size(12.5).color(pal.muted),
            )
            .wrap(),
        );
        return;
    }
    for (key, label) in known {
        let entry = shown.clients.get(&key);
        ui.add_space(2.0);
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new(&label).color(pal.text));
            if label != key {
                ui.label(RichText::new(&key).size(11.0).color(pal.faint).monospace());
            }
        });
        ui.horizontal_wrapped(|ui| {
            let mut mode = entry.and_then(|c| c.default);
            ui.add_enabled_ui(can_edit, |ui| {
                let text = CLIENT_MODES.iter().find(|(m, _)| *m == mode).map_or("Custom", |(_, l)| l);
                egui::ComboBox::from_id_salt(("quire-client-mode", &key)).selected_text(text).width(140.0).show_ui(ui, |ui| {
                    for (m, l) in CLIENT_MODES {
                        ui.selectable_value(&mut mode, m, l);
                    }
                });
            });
            if mode != entry.and_then(|c| c.default) {
                let k = key.clone();
                *edit = Some(Box::new(move |d: &mut PolicyDoc| d.set_client_default(&k, mode)));
            }
            let mut in_app = entry.is_some_and(|c| c.approve_in_client);
            let r = ui.add_enabled(can_edit, egui::Checkbox::new(&mut in_app, "Approve in the agent's app")).on_hover_text(
                "Let this agent ask you to apply a change in its own app instead of making a suggestion here (whole changes only)",
            );
            if r.changed() {
                let k = key.clone();
                *edit = Some(Box::new(move |d: &mut PolicyDoc| d.set_approve_in_client(&k, in_app)));
            }
        });
    }
    ui.add(
        egui::Label::new(
            RichText::new("An agent connected without a Quire key (it only reported its name) can be limited here, never given more.")
                .size(12.0)
                .color(pal.muted),
        )
        .wrap(),
    );
}

fn copy_row(ui: &mut egui::Ui, text: &str, pal: &Palette) {
    egui::Frame::NONE.fill(pal.code_bg).stroke(Stroke::new(1.0, pal.border)).corner_radius(4.0).inner_margin(6.0).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.add(egui::Label::new(RichText::new(text).monospace().size(11.5).color(pal.text)).wrap().selectable(true));
    });
    if ui.small_button("Copy").clicked() {
        ui.ctx().copy_text(text.to_string());
    }
}

fn connect(ui: &mut egui::Ui, ws: &Workspace, pal: &Palette) {
    let vault = ws.vault_root().map_or_else(|| "<vault>".to_string(), |p| p.display().to_string());
    ui.label(RichText::new("Claude Code (in a terminal):").size(12.5).color(pal.text));
    copy_row(ui, &format!("claude mcp add quire -- quire-cli mcp \"{vault}\" --allow-write --client claude-code"), pal);
    ui.label(RichText::new("Claude Desktop and other MCP apps (in their config file):").size(12.5).color(pal.text));
    let quoted = vault.replace('\\', "\\\\").replace('"', "\\\"");
    let json = format!(
        "{{\n  \"mcpServers\": {{\n    \"quire\": {{\n      \"command\": \"quire-cli\",\n      \"args\": [\"mcp\", \"{quoted}\", \"--allow-write\", \"--client\", \"claude-desktop\"]\n    }}\n  }}\n}}"
    );
    copy_row(ui, &json, pal);
    ui.add(
        egui::Label::new(
            RichText::new("Without --allow-write an agent can only read. With it, changes outside agent-owned zones arrive as suggestions in the Inbox.")
                .size(12.0)
                .color(pal.muted),
        )
        .wrap(),
    );
}

fn instructions(ui: &mut egui::Ui, ws: &Workspace, pal: &Palette, after: &mut Option<Later>) {
    match &ws.features.agent.inbox.instructions {
        None => {
            ui.add(
                egui::Label::new(
                    RichText::new("AGENTS.md tells agents how this vault works: where to write, how to cite, what to treat as data.")
                        .size(12.5)
                        .color(pal.text),
                )
                .wrap(),
            );
            if ui.button("Create AGENTS.md").clicked() {
                *after = Some(Box::new(|ws: &mut Workspace| {
                    let _ = ws.run(quire_app::Command::CreateAgentInstructions);
                }));
            }
        }
        Some(i) => {
            let status = if i.approved { "approved on this device" } else { "changed; waiting for your approval" };
            ui.label(RichText::new(format!("{} · {status}", i.path)).color(if i.approved { pal.text } else { pal.warn }));
            ui.horizontal_wrapped(|ui| {
                let p = i.path.clone();
                if ui.button("Open").clicked() {
                    *after = Some(Box::new(move |ws: &mut Workspace| {
                        ws.open_note(&p);
                    }));
                }
                if !i.approved && ui.button("Approve on this device").clicked() {
                    *after = Some(Box::new(|ws: &mut Workspace| {
                        if let Some(s) = ws.agent_store().cloned()
                            && let Err(e) = s.approve_instructions()
                        {
                            ws.notify(NoticeLevel::Error, e);
                        }
                    }));
                }
            });
        }
    }
}
