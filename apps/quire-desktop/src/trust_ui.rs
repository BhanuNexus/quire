// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Trust tools in the desktop (design §C, §7.2, §7.3), owned by WS-C:
//!
//! * the Inbox's contradiction, staleness and citation items, with their
//!   actions (registered in `registry::INBOX_KINDS`);
//! * the bar above a note whose citations need a look (`EDITOR_BANNERS`);
//! * the note under a drifted citation's footnote (`paint_widget`);
//! * Settings → AI (its Features section draws these): ask style, citation checks, staleness, and the
//!   contradiction finder (Preview) with its estimate, cap and usage.

use std::fmt::Write as _;
use std::sync::mpsc;

use egui::{RichText, Stroke};
use quire_app::Workspace;
use quire_app::agent::inbox::{InboxItem, StaleReason, StoredItem, Verdict};
use quire_app::decor::{ActionId, WidgetId};
use quire_app::intel::anchor::{AnchorStatus, Support};
use quire_app::intel::chat::Locality;
use quire_app::trust_jobs::{action, widget_action};
use quire_app::vault::NotePath;
use quire_app::workspace::NoticeLevel;

use crate::panels::PanelOut;
use crate::registry::{FeatureUi, InboxAct};
use crate::theme::Palette;

// ------------------------------------------------------------------ Inbox

/// Items this module draws in the Inbox.
pub(crate) const fn is_trust_item(item: &InboxItem) -> bool {
    matches!(
        item,
        InboxItem::Contradiction { .. } | InboxItem::Stale { .. } | InboxItem::CitationDrift { .. } | InboxItem::VerifyFailed { .. }
    )
}

fn quote(text: &str, n: usize) -> String {
    let t: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let short: String = t.chars().take(n).collect();
    if t.chars().count() > n { format!("“{short}…”") } else { format!("“{short}”") }
}

fn line(ui: &mut egui::Ui, text: impl Into<String>, size: f32, color: egui::Color32) {
    ui.add(egui::Label::new(RichText::new(text).size(size).color(color)).wrap());
}

fn stale_reason(r: &StaleReason) -> String {
    use quire_app::intel::stale::{Reason, describe};
    let r = match r {
        StaleReason::Age { days } => Reason::Age { days: *days },
        StaleReason::LinkedChanged { path } => Reason::LinkedChanged { target: path.stem().to_string() },
        StaleReason::TimeBound { phrase, count } => Reason::TimeBound { phrase: phrase.clone(), count: *count },
        StaleReason::ValidUntil { date } => Reason::ValidUntil { date: date.clone() },
        StaleReason::Superseded { by } => Reason::Superseded { by: by.clone() },
    };
    describe(&r)
}

/// The item's text in the Inbox card.
pub(crate) fn inbox_body(ui: &mut egui::Ui, _ws: &Workspace, s: &StoredItem, pal: &Palette) {
    match &s.item {
        InboxItem::Contradiction { a, b, verdict, confidence, rationale } => {
            let what = match verdict {
                Verdict::Contradiction => "disagree",
                Verdict::Tension => "may pull against each other",
            };
            line(ui, format!("“{}” and “{}” {what}", a.path.stem(), b.path.stem()), 12.5, pal.text);
            line(ui, format!("{} {}", a.path.stem(), quote(&a.quote, 140)), 12.0, pal.muted);
            line(ui, format!("{} {}", b.path.stem(), quote(&b.quote, 140)), 12.0, pal.muted);
            if !rationale.is_empty() {
                line(ui, format!("Why: {rationale} ({:.0}% sure)", confidence * 100.0), 11.5, pal.faint);
            }
        }
        InboxItem::Stale { path, reasons, .. } => {
            line(ui, format!("“{}” may be out of date", path.stem()), 12.5, pal.text);
            let why: Vec<String> = reasons.iter().take(3).map(stale_reason).collect();
            if !why.is_empty() {
                line(ui, why.join(" · "), 12.0, pal.muted);
            }
        }
        InboxItem::CitationDrift { in_note, anchor, status } => {
            let what = match status {
                AnchorStatus::Drifted { similarity, .. } => {
                    format!("the cited text in “{}” changed ({:.0}% the same)", anchor.path.stem(), similarity * 100.0)
                }
                AnchorStatus::Lost => format!("the cited text is no longer in “{}”", anchor.path.stem()),
                AnchorStatus::Exact | AnchorStatus::Moved { .. } => format!("a citation of “{}”", anchor.path.stem()),
            };
            line(ui, format!("“{}”: {what}", in_note.stem()), 12.5, pal.text);
            line(ui, format!("Quoted: {}", quote(&anchor.quote, 160)), 12.0, pal.muted);
        }
        InboxItem::VerifyFailed { in_note, anchor, claim } => {
            line(ui, format!("“{}”: a cited passage doesn't support what it's cited for", in_note.stem()), 12.5, pal.text);
            line(ui, format!("Says: {}", quote(claim, 140)), 12.0, pal.muted);
            line(ui, format!("Cites {}: {}", anchor.path.stem(), quote(&anchor.quote, 140)), 12.0, pal.muted);
        }
        _ => {}
    }
}

/// The item's own actions (the Inbox adds Dismiss and Snooze).
pub(crate) fn inbox_actions(ui: &mut egui::Ui, _ws: &Workspace, s: &StoredItem, _pal: &Palette) -> Option<InboxAct> {
    let mut act: Option<InboxAct> = None;
    match &s.item {
        InboxItem::Contradiction { a, b, .. } => {
            let (a1, b1) = (a.clone(), b.clone());
            if ui.small_button("Open both").on_hover_text("Side by side, with the two passages highlighted").clicked() {
                act = Some(Box::new(move |ws: &mut Workspace, out: &mut PanelOut| {
                    ws.open_side_by_side(&a1, &b1);
                    out.jumped = true;
                }));
            }
            let fp = s.fingerprint.clone();
            let (pa, pb) = (a.path.clone(), b.path.clone());
            ui.menu_button(RichText::new("Mark superseded").size(12.0), |ui| {
                for (old, new) in [(pa.clone(), pb.clone()), (pb, pa)] {
                    if old == new {
                        continue;
                    }
                    let label = format!("“{}” is replaced by “{}”", old.stem(), new.stem());
                    if ui
                        .button(label)
                        .on_hover_text(format!("Sets superseded_by: [[{}]] on “{}”", new.without_ext(), old.stem()))
                        .clicked()
                    {
                        let fp = fp.clone();
                        act = Some(Box::new(move |ws: &mut Workspace, _| ws.mark_superseded(&old, &new, Some(&fp))));
                        ui.close();
                    }
                }
            });
            let (a2, b2) = (a.clone(), b.clone());
            if ui
                .small_button("Ask agent to reconcile")
                .on_hover_text("The agent panel gets both passages; its changes arrive here as suggestions")
                .clicked()
            {
                act = Some(Box::new(move |ws: &mut Workspace, _| ws.reconcile(&a2, &b2)));
            }
        }
        InboxItem::Stale { path, .. } => {
            open_button(ui, path, None, &mut act);
            let p = path.clone();
            if ui.small_button("Mark reviewed").on_hover_text("Sets reviewed: today in the note's properties").clicked() {
                act = Some(Box::new(move |ws: &mut Workspace, _| ws.mark_reviewed(&p)));
            }
        }
        InboxItem::CitationDrift { in_note, anchor, .. } | InboxItem::VerifyFailed { in_note, anchor, .. } => {
            open_button(ui, in_note, None, &mut act);
            let lines = anchor.lines;
            open_button(ui, &anchor.path, Some(lines), &mut act);
        }
        _ => {}
    }
    act
}

fn open_button(ui: &mut egui::Ui, path: &NotePath, lines: Option<(u32, u32)>, act: &mut Option<InboxAct>) {
    let label = if lines.is_some() { format!("Open “{}”", path.stem()) } else { "Open".to_string() };
    if ui.small_button(label).clicked() {
        let p = path.clone();
        *act = Some(Box::new(move |ws: &mut Workspace, out: &mut PanelOut| {
            if ws.vault().is_some_and(|v| v.exists(&p)) {
                ws.open_note(&p);
                if let Some((l, _)) = lines {
                    let at = ws
                        .active_doc()
                        .map_or(0, |d| d.text().split_inclusive('\n').take(l.saturating_sub(1) as usize).map(str::len).sum());
                    ws.jump_to_byte(at);
                }
                out.jumped = true;
            } else {
                ws.notify(NoticeLevel::Info, format!("“{}” doesn't exist", p.stem()));
            }
        }));
    }
}

// ------------------------------------------------------------------ banner

pub(crate) fn banner_available(ws: &Workspace) -> bool {
    ws.active_doc().and_then(|d| d.path.as_ref()).is_some_and(|p| ws.features.trust.flagged(p) > 0)
}

/// "2 citations in this note need a look · Check again".
pub(crate) fn banner(ui: &mut egui::Ui, ws: &mut Workspace, _st: &mut FeatureUi, pal: &Palette) -> PanelOut {
    let mut out = PanelOut::default();
    let Some(path) = ws.active_doc().and_then(|d| d.path.clone()) else { return out };
    let checks = ws.citation_checks(&path);
    let changed = checks.iter().filter(|c| !c.status.holds()).count();
    let unsupported =
        checks.iter().filter(|c| c.status.holds() && (c.weak || matches!(c.support, Some((Support::No | Support::Partly, _))))).count();
    let mut words = Vec::new();
    if changed > 0 {
        words.push(format!(
            "{changed} citation{} no longer match{} the cited text",
            if changed == 1 { "" } else { "s" },
            if changed == 1 { "es" } else { "" }
        ));
    }
    if unsupported > 0 {
        words.push(format!("{unsupported} may not support the sentence citing {}", if unsupported == 1 { "it" } else { "them" }));
    }
    ui.horizontal_wrapped(|ui| {
        ui.label(RichText::new("⚠").color(pal.warn));
        ui.label(RichText::new(words.join("; ")).color(pal.text));
        ui.label(RichText::new("· see the notes under the footnotes").color(pal.muted));
        let model = ws.settings.trust.verify_citations && ws.trust_chat().is_some();
        let tip = if model {
            format!("Look again, and ask {} whether each passage supports its sentence", ws.assistant_name())
        } else {
            "Look again for the quoted text".into()
        };
        if ui.button("Check again").on_hover_text(tip).clicked() {
            out.run = Some(quire_app::Command::CheckCitations);
        }
    });
    out
}

// ------------------------------------------------------------------ widget

/// The note under a flagged citation's footnote.
pub(crate) fn paint_widget(ui: &mut egui::Ui, ws: &Workspace, id: WidgetId, pal: &Palette) -> (f32, Option<ActionId>) {
    let Some((_, c)) = ws.citation_for_widget(id.id) else { return (0.0, None) };
    let mut clicked = None;
    let size = (ws.settings.font_size * 0.88).clamp(11.0, 20.0);
    let target = c.target.as_ref().map_or_else(|| c.citation.note().to_string(), |t| t.stem().to_string());
    let r = egui::Frame::NONE
        .fill(pal.warn.gamma_multiply(0.08))
        .stroke(Stroke::new(1.0, pal.warn.gamma_multiply(0.5)))
        .corner_radius(4.0)
        .inner_margin(egui::Margin { left: 10, right: 8, top: 5, bottom: 5 })
        .outer_margin(egui::Margin { top: 2, bottom: 6, ..egui::Margin::ZERO })
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            let headline = match (&c.target, c.status) {
                (None, _) => format!("⚠ “{target}” doesn't exist any more."),
                (_, AnchorStatus::Lost) => format!("⚠ The quoted text is no longer in “{target}”."),
                (_, AnchorStatus::Drifted { similarity, .. }) => {
                    format!("⚠ The cited text in “{target}” changed ({:.0}% the same).", similarity * 100.0)
                }
                _ => match &c.support {
                    Some((Support::No, why)) => format!("✕ The cited passage doesn't support this. {why}"),
                    Some((Support::Partly, why)) => format!("~ Only partly supported. {why}"),
                    _ => "The cited passage shares few words with the sentence citing it; check that it supports it.".into(),
                },
            };
            ui.label(RichText::new(headline).size(size).color(pal.text));
            if let (AnchorStatus::Drifted { .. }, Some(now)) = (c.status, &c.now) {
                ui.label(RichText::new(format!("Now: {}", quote(now, 220))).size(size * 0.95).color(pal.muted));
            }
            ui.horizontal_wrapped(|ui| {
                if matches!(c.status, AnchorStatus::Drifted { .. })
                    && ui
                        .small_button("Update citation")
                        .on_hover_text("Quote what the note says now (check it still supports the sentence)")
                        .clicked()
                {
                    clicked = Some(widget_action(id.id, action::UPDATE));
                }
                if c.target.is_some() && ui.small_button(format!("Open “{target}”")).clicked() {
                    clicked = Some(widget_action(id.id, action::OPEN));
                }
                if c.status.holds()
                    && c.support.is_none()
                    && ws.trust_chat().is_some()
                    && ui
                        .small_button("Check with AI")
                        .on_hover_text(format!("Ask {} whether the passage supports the sentence", ws.assistant_name()))
                        .clicked()
                {
                    clicked = Some(widget_action(id.id, action::CHECK));
                }
            });
        })
        .response;
    (r.rect.height() + 8.0, clicked)
}

// ------------------------------------------------------------------ settings

/// `text` (the note at `path`) for the Markdown preview: " ⚠ …" at the end
/// of each flagged citation footnote, and the footnotes' `quire:cite`
/// comments left out (the preview would show them as text).
pub(crate) fn mark_flagged_citations(ws: &Workspace, path: &NotePath, text: String) -> String {
    let text = flag_citations(ws, path, text);
    if !text.contains("<!-- quire:cite") {
        return text;
    }
    let mut out = String::with_capacity(text.len());
    for line in text.split_inclusive('\n') {
        match (line.starts_with("[^"), line.find("<!-- quire:cite")) {
            (true, Some(i)) => {
                let end = line[i..].find("-->").map_or(line.len(), |j| i + j + 3);
                out.push_str(line[..i].trim_end());
                out.push_str(&line[end..]);
            }
            _ => out.push_str(line),
        }
    }
    out
}

fn flag_citations(ws: &Workspace, path: &NotePath, mut text: String) -> String {
    let mut checks: Vec<_> = ws.citation_checks(path).into_iter().filter(quire_app::trust_jobs::CiteCheck::flagged).collect();
    checks.sort_by_key(|c| std::cmp::Reverse(c.citation.range.start));
    for c in checks {
        let r = c.citation.range.clone();
        let Some(line) = text.get(r.clone()).filter(|l| l.starts_with(&format!("[^{}]:", c.citation.label))) else { continue };
        let why = match c.status {
            AnchorStatus::Lost => "the quoted text is gone",
            AnchorStatus::Drifted { .. } => "the quoted text changed",
            _ => "check this citation",
        };
        // Before the `quire:cite` comment, which the preview hides.
        let visible = line.find("<!--").map_or(line, |i| &line[..i]);
        let at = r.start + visible.trim_end().len();
        text.insert_str(at, &format!(" ⚠ *{why}*"));
    }
    text
}

/// The trust part of Settings → AI: state.
#[derive(Default)]
pub(crate) struct TrustSettingsUi {
    /// Working out the first run's size: (passages, not yet read).
    estimate: Option<mpsc::Receiver<(usize, usize)>>,
    /// The estimate, waiting for the person to confirm.
    confirm: Option<(usize, usize)>,
}

pub(crate) const fn settings_available(ws: &Workspace) -> bool {
    ws.agent_store().is_some()
}

fn heading(ui: &mut egui::Ui, pal: &Palette, title: &str) {
    ui.add_space(10.0);
    ui.label(RichText::new(title).size(11.0).color(pal.muted).strong());
}

fn note(ui: &mut egui::Ui, pal: &Palette, text: impl Into<String>) {
    ui.add(egui::Label::new(RichText::new(text).size(12.0).color(pal.muted)).wrap());
}

pub(crate) fn settings_tab(ui: &mut egui::Ui, ws: &mut Workspace, st: &mut FeatureUi, pal: &Palette) -> PanelOut {
    let out = PanelOut::default();
    let st = &mut st.trust;
    let mut changed = false;
    ui.scope(|ui| {
        ui.set_max_width(ui.available_width());
        heading(ui, pal, "ASK");
        changed |= ui.checkbox(&mut ws.settings.trust.ask_before_tell, "Say what I expect before seeing an answer").changed();
        note(ui, pal, "Ask first asks what you expect, then shows the passages grouped by topic, then the answer. You can switch it per question.");

        heading(ui, pal, "CITATIONS");
        changed |= ui.checkbox(&mut ws.settings.trust.verify_citations, "Check citations with the AI model").changed();
        note(
            ui,
            pal,
            "Saved answers keep their sources as footnotes. Quire notices when a cited passage changes or disappears, offline. With this on, “Check citations in this note” and answers a model wrote also ask the model whether each passage supports its sentence (the passages are sent to it).",
        );

        heading(ui, pal, "GETTING STALE");
        changed |= ui.checkbox(&mut ws.settings.trust.staleness, "Weekly digest of notes that may be out of date").changed();
        note(ui, pal, "Looks at age (or reviewed: and review_every: in a note), linked notes that changed a lot since, and words like “currently”. Offline. Folders and the threshold are in .quire/shared/vault.toml under [staleness].");
        let run = ws.features.trust.shared.stale.lock().map(|r| r.clone()).unwrap_or_default();
        ui.horizontal_wrapped(|ui| {
            let text = match run.at {
                Some(t) => {
                    let mins = t.elapsed().as_secs() / 60;
                    let when = if mins == 0 { "just now".to_string() } else if mins < 120 { format!("{mins} min ago") } else { format!("{} h ago", mins / 60) };
                    format!("Checked {} notes {when}: {} may be out of date.", run.notes, run.flagged)
                }
                None => "Not checked yet on this computer.".into(),
            };
            ui.label(RichText::new(text).size(12.0).color(pal.text));
            if ui.add_enabled(ws.settings.trust.staleness, egui::Button::new("Check now").small()).clicked() {
                ws.features.trust.shared.stale_now.store(true, std::sync::atomic::Ordering::Release);
            }
        });

        ui.add_space(10.0);
        ui.horizontal(|ui| {
            ui.label(RichText::new("FIND CONTRADICTIONS").size(11.0).color(pal.muted).strong());
            egui::Frame::NONE.stroke(Stroke::new(1.0, pal.warn)).corner_radius(8.0).inner_margin(egui::Margin::symmetric(6, 1)).show(ui, |ui| {
                ui.label(RichText::new("Preview").size(10.5).color(pal.warn));
            });
        });
        let model = ws.trust_chat();
        note(ui, pal, "An AI model lists the claims in your notes and compares claims that seem to be about the same thing. Pairs that disagree appear in the Inbox under Contradictions, to open side by side, mark one as superseded, or hand to an agent. It runs only while you're not typing.");
        match &model {
            None => {
                let mut off = false;
                ui.add_enabled(false, egui::Checkbox::new(&mut off, "Find contradictions"));
                note(
                    ui,
                    pal,
                    if ws.settings.ai_local.on_device() {
                        "Needs an AI model: choose a provider or a model on this computer above."
                    } else {
                        "Needs an AI model: choose a provider above."
                    },
                );
            }
            Some(m) => {
                let cloud = m.locality() == Locality::Cloud;
                let mut on = ws.settings.trust.contradictions;
                if ui.checkbox(&mut on, format!("Find contradictions with {}", m.name())).changed() {
                    if on {
                        // Show what the first run costs before starting it.
                        let vault = ws.vault().cloned();
                        let notes: Vec<(NotePath, String)> = {
                            let ix = ws.index().read();
                            ix.paths().into_iter().filter_map(|p| ix.source(&p).map(|s| (p, s.to_string()))).collect()
                        };
                        let name = m.name().to_string();
                        let (tx, rx) = mpsc::channel();
                        if let Some(vault) = vault {
                            std::thread::spawn(move || {
                                let _ = tx.send(quire_app::trust_jobs::claims::estimate(&vault, &notes, &name));
                            });
                            st.estimate = Some(rx);
                        }
                    } else {
                        ws.settings.trust.contradictions = false;
                        st.confirm = None;
                        changed = true;
                    }
                }
                if let Some(rx) = &st.estimate {
                    match rx.try_recv() {
                        Ok(e) => {
                            st.confirm = Some(e);
                            st.estimate = None;
                        }
                        Err(mpsc::TryRecvError::Empty) => {
                            ui.horizontal(|ui| {
                                ui.add(egui::Spinner::new());
                                ui.label(RichText::new("Counting passages…").size(12.0));
                            });
                            ui.ctx().request_repaint_after(std::time::Duration::from_millis(150));
                        }
                        Err(mpsc::TryRecvError::Disconnected) => st.estimate = None,
                    }
                }
                if let Some((total, missing)) = st.confirm {
                    egui::Frame::NONE.stroke(Stroke::new(1.0, pal.border)).corner_radius(6.0).inner_margin(8.0).show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        let cap = ws.settings.trust.claims_daily_cap;
                        let mut text = format!("About {missing} passage{} → {missing} request{} to {} to list claims", if missing == 1 { "" } else { "s" }, if missing == 1 { "" } else { "s" }, m.name());
                        if total > missing {
                            let _ = write!(text, " ({} already read)", total - missing);
                        }
                        text.push_str(", then one request per pair of claims to compare.");
                        ui.label(RichText::new(text).size(12.5).color(pal.text));
                        if cloud {
                            let days = missing.div_ceil(cap.max(1) as usize).max(1);
                            note(ui, pal, format!("Your notes' passages are sent to {}. At {cap} requests a day this takes about {days} day{}.", m.name(), if days == 1 { "" } else { "s" }));
                        } else {
                            note(ui, pal, "The model runs on this computer or your own server: nothing leaves it.");
                        }
                        ui.horizontal(|ui| {
                            if ui.button("Turn on").clicked() {
                                ws.settings.trust.contradictions = true;
                                st.confirm = None;
                                changed = true;
                            }
                            if ui.button("Cancel").clicked() {
                                st.confirm = None;
                            }
                        });
                    });
                }
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Requests per day (cloud models)").size(12.5));
                    changed |= ui.add(egui::DragValue::new(&mut ws.settings.trust.claims_daily_cap).range(10..=5_000).speed(5)).changed();
                });
                if ws.settings.trust.contradictions {
                    let s = ws.features.trust.shared.claims.lock().map(|s| s.clone()).unwrap_or_default();
                    let mut text = format!("{} claims · {} contradiction{} found", s.claims, s.contradictions, if s.contradictions == 1 { "" } else { "s" });
                    if cloud {
                        text = format!("Today: {} of {} requests · {text}", s.requests_today, ws.settings.trust.claims_daily_cap);
                    }
                    if s.waiting > 0 {
                        let _ = write!(text, " · {} passages to go", s.waiting);
                    }
                    ui.label(RichText::new(text).size(12.0).color(pal.text));
                    if let Some(n) = &s.note {
                        ui.label(RichText::new(n).size(12.0).color(pal.warn));
                    }
                }
            }
        }
    });
    if changed {
        let _ = ws.settings.save();
    }
    out
}
