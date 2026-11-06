// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The Inbox tab of the right panel: suggested changes, checks, contradictions, stale notes (design §7.3).
//!
//! Owned by WS-A. Suggested changes are grouped by agent run with Review,
//! Accept all and Reject all; checks are batched per kind with their quick
//! fixes; a changed agent policy or `AGENTS.md` waits here for approval.
//! Filters by kind, agent and folder; snooze for a day or a week; resolved
//! suggestions on request. A suggestion under review has its banner above
//! the note (`review_ui::banner`).

#[path = "review_ui.rs"]
pub(crate) mod review_ui;

use std::collections::HashSet;

use egui::{RichText, Stroke};
use quire_app::Workspace;
use quire_app::agent::inbox::{InboxItem, StoredItem};
use quire_app::agent::{Actor, ProposalId, ProposalStatus};
use quire_app::agent_state::inbox::{Filters, Group, Section, item_path, proposal_fingerprint};
use quire_app::history_ops::{DiffKind, diff};
use quire_app::vault::NotePath;
use quire_app::workspace::NoticeLevel;

use crate::panels::PanelOut;
use crate::registry::FeatureUi;
use crate::theme::Palette;

/// UI state kept between frames (in [`FeatureUi`]).
#[derive(Default)]
pub(crate) struct InboxUi {
    filters: Filters,
    folder: String,
    /// Groups and check batches opened up.
    expanded: HashSet<String>,
    /// A new note shown in full (its path); the review banner's too.
    pub(crate) preview: Option<String>,
    show_policy_diff: bool,
    show_instructions_diff: bool,
}

/// Whether the tab is offered.
/// Shown while review history stays on each device (Settings → Sync).
pub(crate) const OTHER_DEVICES_OFF: &str =
    "Suggestions from other devices appear here when 'Also sync vault settings and review history' is on (Settings → Sync).";

pub(crate) const fn available(ws: &Workspace) -> bool {
    ws.agent_store().is_some()
}

/// The tab's label, with the count.
pub(crate) fn label(ws: &Workspace) -> String {
    let a = &ws.features.agent.inbox;
    let n = a.badge(&|c| a.lint_prefs.level(c) != quire_app::agent::lint::Level::On);
    if n == 0 { "Inbox".into() } else { format!("Inbox · {n}") }
}

fn heading(ui: &mut egui::Ui, pal: &Palette, title: &str, n: usize) {
    ui.add_space(10.0);
    let t = if n > 0 { format!("{} · {n}", title.to_uppercase()) } else { title.to_uppercase() };
    ui.label(RichText::new(t).size(11.0).color(pal.muted).strong());
    ui.add_space(2.0);
}

/// Something to do once drawing is over (drawing borrows the data).
type Act = crate::registry::InboxAct;

fn snooze_menu(ui: &mut egui::Ui, fingerprint: &str, act: &mut Option<Act>) {
    ui.menu_button(RichText::new("Snooze").size(12.0), |ui| {
        for (label, days) in [("For a day", 1), ("For a week", 7)] {
            if ui.button(label).clicked() {
                let fp = fingerprint.to_string();
                *act = Some(Box::new(move |ws: &mut Workspace, _| snooze(ws, &fp, days)));
                ui.close();
            }
        }
    });
}

fn snooze(ws: &mut Workspace, fingerprint: &str, days: i64) {
    let Some(store) = ws.agent_store().cloned() else { return };
    let until = store.now_ms() + days * 86_400_000;
    let r = store.update_device_state(|s| {
        s.snoozed.retain(|_, t| *t > until - 400 * 86_400_000);
        s.snoozed.insert(fingerprint.to_string(), until);
    });
    if let Err(e) = r {
        ws.notify(NoticeLevel::Error, format!("Couldn't snooze: {e}"));
    }
    ws.features.agent.reload_inbox_soon();
}

pub(crate) fn show(ui: &mut egui::Ui, ws: &mut Workspace, st: &mut FeatureUi, pal: &Palette) -> PanelOut {
    let mut out = PanelOut::default();
    let ui_st = &mut st.inbox;
    ws.features.agent.loader.set_want_resolved(ui_st.filters.show_resolved);
    egui::ScrollArea::vertical().id_salt("quire-inbox").auto_shrink([false, false]).show(ui, |ui| {
        ui.add_space(6.0);
        filters(ui, ws, ui_st, pal);
        let mut act: Option<Act> = None;
        body(ui, ws, ui_st, pal, &mut act);
        if let Some(f) = act {
            f(ws, &mut out);
        }
    });
    out
}

fn filters(ui: &mut egui::Ui, ws: &Workspace, st: &mut InboxUi, pal: &Palette) {
    let data = &ws.features.agent.inbox;
    ui.horizontal_wrapped(|ui| {
        let kind = st.filters.section.map_or("Everything", Section::title);
        egui::ComboBox::from_id_salt("quire-inbox-kind").selected_text(kind).width(120.0).show_ui(ui, |ui| {
            ui.selectable_value(&mut st.filters.section, None, "Everything");
            for s in Section::ALL {
                ui.selectable_value(&mut st.filters.section, Some(s), s.title());
            }
        });
        let agents = data.agents();
        if !agents.is_empty() {
            let who =
                st.filters.agent.as_ref().and_then(|a| agents.iter().find(|(id, _)| id == a)).map_or("Any agent", |(_, l)| l.as_str());
            egui::ComboBox::from_id_salt("quire-inbox-agent").selected_text(who).width(100.0).show_ui(ui, |ui| {
                ui.selectable_value(&mut st.filters.agent, None, "Any agent");
                for (id, l) in &agents {
                    ui.selectable_value(&mut st.filters.agent, Some(id.clone()), l);
                }
            });
        }
    });
    ui.horizontal_wrapped(|ui| {
        let r = ui.add(egui::TextEdit::singleline(&mut st.folder).hint_text("Folder…").desired_width(100.0));
        if r.changed() {
            let f = st.folder.trim().trim_matches('/').to_string();
            st.filters.folder = (!f.is_empty()).then_some(f);
        }
        ui.checkbox(&mut st.filters.show_snoozed, RichText::new("Snoozed").size(12.0).color(pal.muted));
        ui.checkbox(&mut st.filters.show_resolved, RichText::new("Resolved").size(12.0).color(pal.muted));
    });
}

fn body(ui: &mut egui::Ui, ws: &Workspace, st: &mut InboxUi, pal: &Palette, act: &mut Option<Act>) {
    let data = &ws.features.agent.inbox;
    if data.loaded_at_ms == 0 {
        ui.add_space(12.0);
        ui.label(RichText::new("Loading…").color(pal.muted));
        return;
    }
    let f = st.filters.clone();
    let mut shown = 0;
    if f.section.is_none_or(|s| s == Section::Permissions) {
        shown += permissions(ui, ws, st, pal, act);
    }
    let groups = data.groups(&f);
    if !groups.is_empty() {
        let n: usize = groups.iter().map(|g| g.proposals.len()).sum();
        heading(ui, pal, "Suggested changes", n);
        for g in &groups {
            group(ui, g, st, pal, act);
        }
        shown += groups.len();
    }
    let mut digest_seen = false;
    for (section, items) in data.findings(&f) {
        heading(ui, pal, section.title(), items.len());
        digest_seen |= matches!(section, Section::Checks | Section::Stale);
        if section == Section::Checks {
            checks(ui, ws, &items, st, pal, act);
        } else {
            for s in items {
                finding(ui, ws, s, pal, act);
            }
        }
        shown += 1;
    }
    if f.show_resolved {
        resolved(ui, ws, pal, act);
    }
    // Agents on a server (quire-cli serve) keep their suggestions there
    // unless review history travels with sync.
    if ws.sync.config.is_some() && !ws.sync_shares_state() && f.section.is_none_or(|s| s == Section::Suggested) {
        ui.add_space(8.0);
        ui.add(egui::Label::new(RichText::new(OTHER_DEVICES_OFF).size(11.5).color(pal.faint)).wrap());
    }
    // Looking at checks resets the weekly digest.
    if digest_seen && data.digest_due() && act.is_none() {
        *act = Some(Box::new(|ws: &mut Workspace, _| {
            if let Some(store) = ws.agent_store().cloned() {
                let now = store.now_ms();
                let _ = store.update_device_state(|s| s.digest_seen = now);
            }
            ws.features.agent.reload_inbox_soon();
        }));
    }
    if shown == 0 {
        ui.add_space(16.0);
        let filtered = f != Filters { show_resolved: f.show_resolved, ..Filters::default() };
        let text = if filtered {
            "Nothing matches these filters."
        } else {
            "Nothing to review. Changes agents suggest, and problems the vault checks find, appear here."
        };
        ui.add(egui::Label::new(RichText::new(text).color(pal.muted)).wrap());
        if !filtered {
            ui.add_space(6.0);
            if ui.button("Check the vault now").clicked() {
                *act = Some(Box::new(|_ws: &mut Workspace, out: &mut PanelOut| out.run = Some(quire_app::Command::LintVault)));
            }
        }
    }
    ui.add_space(12.0);
}

/// A card around one entry.
fn card<R>(ui: &mut egui::Ui, pal: &Palette, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    egui::Frame::NONE
        .fill(pal.bg)
        .stroke(Stroke::new(1.0, pal.border))
        .corner_radius(6.0)
        .inner_margin(egui::Margin::symmetric(8, 6))
        .outer_margin(egui::Margin { bottom: 6, ..egui::Margin::ZERO })
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui)
        })
        .inner
}

fn group(ui: &mut egui::Ui, g: &Group<'_>, st: &mut InboxUi, pal: &Palette, act: &mut Option<Act>) {
    let key = format!("g:{}", g.proposals[0].id);
    let open = st.expanded.contains(&key);
    card(ui, pal, |ui| {
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new(review_ui::group_line(g)).size(12.0).color(pal.muted));
        });
        let title = if g.proposals.len() > 1 { format!("{} (and {} more)", g.title, g.proposals.len() - 1) } else { g.title.to_string() };
        ui.add(egui::Label::new(RichText::new(title).color(pal.strong)).wrap());
        let n = g.changes();
        ui.label(RichText::new(format!("{n} change{}", if n == 1 { "" } else { "s" })).size(12.0).color(pal.muted));
        for p in &g.proposals {
            review_ui::untrusted_chip(ui, p, pal);
        }
        ui.horizontal_wrapped(|ui| {
            let first = g.proposals[0].id.clone();
            if ui.button("Review").on_hover_text("Open the changes in the editor, one by one").clicked() {
                *act = Some(Box::new(move |ws: &mut Workspace, out: &mut PanelOut| {
                    if ws.start_review(&first, None) {
                        out.jumped = true;
                    }
                }));
            }
            let ids: Vec<ProposalId> = g.proposals.iter().map(|p| p.id.clone()).collect();
            let ids2 = ids.clone();
            if ui.button("Accept all").on_hover_text("Every change from this run, except ones that conflict with your edits").clicked() {
                *act = Some(Box::new(move |ws: &mut Workspace, _| {
                    for id in &ids {
                        ws.resolve_whole_proposal(id, true);
                    }
                }));
            }
            if ui.button("Reject all").clicked() {
                *act = Some(Box::new(move |ws: &mut Workspace, _| {
                    for id in &ids2 {
                        ws.resolve_whole_proposal(id, false);
                    }
                }));
            }
            snooze_menu(ui, &proposal_fingerprint(g.proposals[0]), act);
            let toggle = if open { "Less" } else { "Details" };
            if ui.small_button(toggle).clicked() {
                if open {
                    st.expanded.remove(&key);
                } else {
                    st.expanded.insert(key.clone());
                }
            }
        });
        if open {
            for p in &g.proposals {
                ui.separator();
                ui.label(RichText::new(&p.title).strong().size(12.5).color(pal.text));
                review_ui::rationale(ui, p, pal);
                for c in p.pending_changes() {
                    ui.horizontal_wrapped(|ui| {
                        ui.label(RichText::new(format!("• {}", review_ui::change_words(c))).size(12.0).color(pal.text));
                        if let Some(path) = c.subject().cloned() {
                            let id = p.id.clone();
                            if ui.small_button("Review").clicked() {
                                *act = Some(Box::new(move |ws: &mut Workspace, out: &mut PanelOut| {
                                    if ws.start_review(&id, Some(&path)) {
                                        out.jumped = true;
                                    }
                                }));
                            }
                        }
                    });
                }
            }
        }
    });
}

/// Lint findings, batched per check.
fn checks(ui: &mut egui::Ui, ws: &Workspace, items: &[&StoredItem], st: &mut InboxUi, pal: &Palette, act: &mut Option<Act>) {
    if let Ok(Some(run)) = ws.features.agent.lint.last.lock().map(|l| l.clone()) {
        let mins = run.at.elapsed().as_secs() / 60;
        let when = if mins == 0 { "just now".to_string() } else { format!("{mins} min ago") };
        ui.label(RichText::new(format!("Checked {} notes {when}", run.notes)).size(11.5).color(pal.faint));
    }
    let mut start = 0;
    while start < items.len() {
        let InboxItem::Lint { check, .. } = &items[start].item else {
            start += 1;
            continue;
        };
        let check = *check;
        let end = items[start..]
            .iter()
            .position(|s| !matches!(&s.item, InboxItem::Lint { check: c, .. } if *c == check))
            .map_or(items.len(), |i| start + i);
        let batch = &items[start..end];
        let key = format!("c:{}", check.name());
        let open = st.expanded.contains(&key) || batch.len() <= 3;
        card(ui, pal, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new(format!("{} · {}", check.title(), batch.len())).color(pal.strong));
                if batch.len() > 3 && ui.small_button(if open { "Less" } else { "Show" }).clicked() {
                    if open {
                        st.expanded.remove(&key);
                    } else {
                        st.expanded.insert(key.clone());
                    }
                }
            });
            if !open {
                return;
            }
            for s in batch {
                let InboxItem::Lint { path, detail, fix, .. } = &s.item else { continue };
                ui.separator();
                ui.add(egui::Label::new(RichText::new(path.as_str()).size(12.5).color(pal.text)).wrap());
                ui.add(egui::Label::new(RichText::new(detail).size(12.0).color(pal.muted)).wrap());
                ui.horizontal_wrapped(|ui| {
                    open_button(ui, path, act);
                    if let Some(cs) = fix.clone() {
                        let label = fix_label(check);
                        let fp = s.fingerprint.clone();
                        if ui.small_button(label).clicked() {
                            *act = Some(Box::new(move |ws: &mut Workspace, _| apply_fix(ws, cs, &fp)));
                        }
                    }
                    let fp = s.fingerprint.clone();
                    if ui.small_button("Dismiss").on_hover_text("Hide this until the note changes").clicked() {
                        *act = Some(Box::new(move |ws: &mut Workspace, _| dismiss(ws, &fp)));
                    }
                    snooze_menu(ui, &s.fingerprint, act);
                });
            }
        });
        start = end;
    }
}

const fn fix_label(check: quire_app::agent::inbox::LintCheck) -> &'static str {
    use quire_app::agent::inbox::LintCheck as L;
    match check {
        L::BrokenLink => "Fix links",
        L::MissingConcept => "Create the note",
        L::MissingProperty => "Add the properties",
        L::EmptyNote => "Move to trash",
        L::StaleIndex => "Update the index",
        L::BrokenBlock | L::Orphan | L::DuplicateTitle => "Fix",
    }
}

fn open_button(ui: &mut egui::Ui, path: &NotePath, act: &mut Option<Act>) {
    let p = path.clone();
    if ui.small_button("Open").clicked() {
        *act = Some(Box::new(move |ws: &mut Workspace, out: &mut PanelOut| {
            if ws.vault().is_some_and(|v| v.exists(&p)) {
                ws.open_note(&p);
                out.jumped = true;
            } else {
                ws.notify(NoticeLevel::Info, format!("“{}” doesn't exist", p.stem()));
            }
        }));
    }
}

/// A quick fix is the person's own edit (direct, with history and a ledger entry).
fn apply_fix(ws: &mut Workspace, cs: quire_app::agent::ChangeSet, fingerprint: &str) {
    let Some(store) = ws.agent_store().cloned() else { return };
    ws.save_all();
    match store.submit(&Actor::human(&store.device().id), cs) {
        Ok(_) => {
            let _ = store.inbox().resolve(fingerprint);
            // The watcher brings the changed notes into the index; check again after.
            ws.features.agent.lint.now.store(true, std::sync::atomic::Ordering::Release);
        }
        Err(e) => ws.notify(NoticeLevel::Error, format!("Couldn't fix it: {e}")),
    }
    ws.features.agent.reload_inbox_soon();
}

fn dismiss(ws: &mut Workspace, fingerprint: &str) {
    if let Some(store) = ws.agent_store()
        && let Err(e) = store.inbox().dismiss(fingerprint)
    {
        ws.notify(NoticeLevel::Error, format!("Couldn't dismiss: {e}"));
    }
    ws.features.agent.reload_inbox_soon();
}

/// Findings of other features: drawn by their owner when it registered
/// their kind (`registry::INBOX_KINDS`), else plainly with Open; then
/// Dismiss and Snooze.
fn finding(ui: &mut egui::Ui, ws: &Workspace, s: &StoredItem, pal: &Palette, act: &mut Option<Act>) {
    if let Some(kind) = crate::registry::INBOX_KINDS.iter().find(|k| (k.handles)(&s.item)) {
        card(ui, pal, |ui| {
            (kind.body)(ui, ws, s, pal);
            ui.horizontal_wrapped(|ui| {
                if let Some(a) = (kind.actions)(ui, ws, s, pal) {
                    *act = Some(a);
                }
                let fp = s.fingerprint.clone();
                if ui.small_button("Dismiss").on_hover_text("Hide this until it changes").clicked() {
                    *act = Some(Box::new(move |ws: &mut Workspace, _| dismiss(ws, &fp)));
                }
                snooze_menu(ui, &s.fingerprint, act);
            });
        });
        return;
    }
    let text = match &s.item {
        InboxItem::Contradiction { a, b, rationale, confidence, .. } => {
            format!("“{}” and “{}” may disagree ({:.0}% sure). {rationale}", a.path.stem(), b.path.stem(), confidence * 100.0)
        }
        InboxItem::Stale { path, reasons, .. } => format!("“{}” may be out of date ({} reasons)", path.stem(), reasons.len()),
        InboxItem::CitationDrift { in_note, anchor, .. } => {
            format!("A citation in “{}” to “{}” no longer matches", in_note.stem(), anchor.path.stem())
        }
        InboxItem::VerifyFailed { in_note, claim, .. } => format!("“{}”: a cited note doesn't support “{claim}”", in_note.stem()),
        InboxItem::JobFailed { job, message } => format!("{job} stopped: {message}"),
        InboxItem::Lint { detail, .. } => detail.clone(),
        InboxItem::Proposal(_) | InboxItem::PolicyChanged { .. } | InboxItem::InstructionsChanged { .. } => String::new(),
    };
    card(ui, pal, |ui| {
        ui.add(egui::Label::new(RichText::new(text).size(12.5).color(pal.text)).wrap());
        ui.horizontal_wrapped(|ui| {
            if let Some(p) = item_path(&s.item) {
                open_button(ui, &NotePath::new(p), act);
            }
            let fp = s.fingerprint.clone();
            if ui.small_button("Dismiss").clicked() {
                *act = Some(Box::new(move |ws: &mut Workspace, _| dismiss(ws, &fp)));
            }
            snooze_menu(ui, &s.fingerprint, act);
        });
    });
}

/// A small line diff (old → new), for policy and instructions changes.
pub(crate) fn diff_view(ui: &mut egui::Ui, old: &str, new: &str, pal: &Palette, scroll_id: &str) {
    let lines = diff(old, new, 2);
    egui::Frame::NONE.fill(pal.bg).stroke(Stroke::new(1.0, pal.border)).corner_radius(6.0).inner_margin(6.0).show(ui, |ui| {
        egui::ScrollArea::vertical().id_salt(("quire-diff", scroll_id)).max_height(200.0).show(ui, |ui| {
            ui.set_width(ui.available_width());
            if lines.is_empty() {
                ui.label(RichText::new("No difference in the text.").size(12.0).color(pal.muted));
            }
            for l in lines {
                let (mark, color) = match l.kind {
                    DiffKind::Added => ("+ ", review_ui::added(pal)),
                    DiffKind::Removed => ("− ", pal.danger),
                    DiffKind::Same => ("  ", pal.muted),
                    DiffKind::Gap => ("", pal.faint),
                };
                ui.add(egui::Label::new(RichText::new(format!("{mark}{}", l.text.trim_end())).monospace().size(11.5).color(color)).wrap());
            }
        });
    });
}

/// Policy and instructions changed elsewhere: waiting for approval here.
fn permissions(ui: &mut egui::Ui, ws: &Workspace, st: &mut InboxUi, pal: &Palette, act: &mut Option<Act>) -> usize {
    let data = &ws.features.agent.inbox;
    let mut n = 0;
    let policy = data.policy.as_ref().filter(|p| p.needs_review());
    let instructions = data.instructions.as_ref().filter(|i| !i.approved);
    if policy.is_none() && instructions.is_none() {
        return 0;
    }
    heading(ui, pal, "Permissions", data.permission_items());
    if let Some(p) = policy {
        n += 1;
        card(ui, pal, |ui| {
            ui.add(
                egui::Label::new(
                    RichText::new("Agent permissions for this vault changed on another device (or outside Quire). Until you approve them here, agents keep the rules they had.")
                        .size(12.5)
                        .color(pal.warn),
                )
                .wrap(),
            );
            for w in &p.widenings {
                ui.add(egui::Label::new(RichText::new(format!("• {}", w.what)).size(12.0).color(pal.text)).wrap());
            }
            ui.horizontal_wrapped(|ui| {
                if ui.small_button(if st.show_policy_diff { "Hide the file" } else { "Show the file" }).clicked() {
                    st.show_policy_diff = !st.show_policy_diff;
                }
                if ui.button("Approve on this device").clicked() {
                    // Exactly the file shown here: refused if it changed since.
                    let shown = p.file_hash();
                    *act = Some(Box::new(move |ws: &mut Workspace, _| {
                        if let Some(store) = ws.agent_store().cloned() {
                            match store.approve_policy(shown.as_ref()) {
                                Ok(()) => ws.notify(NoticeLevel::Info, "Approved the new agent permissions on this device"),
                                Err(e) => ws.notify(NoticeLevel::Error, e),
                            }
                        }
                        ws.features.agent.reload_inbox_soon();
                    }));
                }
            });
            if st.show_policy_diff {
                diff_view(ui, &p.baseline_text, p.file_text.as_deref().unwrap_or_default(), pal, "policy");
            }
        });
    }
    if let Some(i) = instructions {
        n += 1;
        card(ui, pal, |ui| {
            let what =
                if i.approved_text.is_some() { "changed outside Quire on this device" } else { "haven't been approved on this device" };
            ui.add(
                egui::Label::new(
                    RichText::new(format!(
                        "The vault's agent instructions ({}) {what}. Agents are told they await review until you approve them.",
                        i.path
                    ))
                    .size(12.5)
                    .color(pal.warn),
                )
                .wrap(),
            );
            ui.horizontal_wrapped(|ui| {
                if ui.small_button(if st.show_instructions_diff { "Hide changes" } else { "Show changes" }).clicked() {
                    st.show_instructions_diff = !st.show_instructions_diff;
                }
                open_button(ui, &i.path, act);
                if ui.button("Approve on this device").clicked() {
                    let shown = i.hash.clone();
                    *act = Some(Box::new(move |ws: &mut Workspace, _| {
                        if let Some(store) = ws.agent_store().cloned()
                            && let Err(e) = store.approve_instructions(&shown)
                        {
                            ws.notify(NoticeLevel::Error, e);
                        }
                        ws.features.agent.reload_inbox_soon();
                    }));
                }
            });
            if st.show_instructions_diff {
                diff_view(ui, i.approved_text.as_deref().unwrap_or_default(), &i.text, pal, "instructions");
            }
        });
    }
    n
}

/// Suggestions already decided ("Resolved").
fn resolved(ui: &mut egui::Ui, ws: &Workspace, pal: &Palette, act: &mut Option<Act>) {
    let list = &ws.features.agent.inbox.resolved;
    heading(ui, pal, "Resolved", list.len());
    if list.is_empty() {
        ui.label(RichText::new("None in the last 90 days.").size(12.0).color(pal.muted));
    }
    for p in list.iter().take(100) {
        ui.horizontal_wrapped(|ui| {
            let status = match p.status {
                ProposalStatus::Applied => "accepted",
                ProposalStatus::PartiallyApplied => "partly accepted",
                ProposalStatus::Rejected => "rejected",
                ProposalStatus::Expired => "expired",
                ProposalStatus::Superseded => "no longer applies",
                ProposalStatus::Pending => "waiting",
            };
            let who = match &p.from_device {
                Some((id, name)) => format!("{} on {}", p.actor.label, quire_app::agent::device_label(id, name)),
                None => p.actor.label.clone(),
            };
            ui.label(RichText::new(format!("{} · {who} · {status}", review_ui::short_time(p.updated))).size(11.5).color(pal.muted));
            ui.label(RichText::new(&p.title).size(12.0).color(pal.text));
            // Settled here by decisions made on another device: say so,
            // and let the person reopen it here (0.10.1 security review #11).
            if let Some(dev) = &p.settled_by {
                let on = ws.agent_store().map_or_else(|| quire_app::agent::device_label(dev, ""), |s| s.device_label(dev));
                ui.label(RichText::new(format!("decided on {on}")).size(11.5).color(pal.warn));
                let id = p.id.clone();
                if ui
                    .small_button("Reopen")
                    .on_hover_text("Ask about it again here; what was decided elsewhere is set aside on this device")
                    .clicked()
                {
                    *act = Some(Box::new(move |ws: &mut Workspace, _| {
                        if let Some(store) = ws.agent_store().cloned()
                            && let Err(e) = store.reopen_proposal(&id)
                        {
                            ws.notify(NoticeLevel::Error, format!("Couldn't reopen it: {e}"));
                        }
                        ws.features.agent.reload_inbox_soon();
                    }));
                }
            }
            if p.status == ProposalStatus::Expired {
                let id = p.id.clone();
                if ui.small_button("Restore").clicked() {
                    *act = Some(Box::new(move |ws: &mut Workspace, _| {
                        if let Some(store) = ws.agent_store().cloned()
                            && let Err(e) = store.restore_proposal(&id)
                        {
                            ws.notify(NoticeLevel::Error, format!("Couldn't restore it: {e}"));
                        }
                        ws.features.agent.reload_inbox_soon();
                    }));
                }
            }
        });
    }
}
