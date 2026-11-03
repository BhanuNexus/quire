// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The Agent activity tab: a lane per agent, runs and their events (design §A.9).
//!
//! Owned by WS-A. Built from this device's ledger: a timeline with one lane
//! per agent, then the runs, newest first, each opening to its events.
//! "Undo an agent's changes" plans a revert (a dry run listing every step)
//! before anything is written; the undo keeps edits made since, and turns
//! the ones it can't merge into a suggestion to review.

use std::collections::HashSet;

use egui::{Align2, FontId, Pos2, Rect, RichText, Sense, Stroke, Vec2};
use quire_app::Workspace;
use quire_app::agent::{ActorKind, LedgerEntry, Op, RevertPlan, RevertSelector};
use quire_app::agent_state::activity::Run;
use quire_app::agent_state::{Bg, load_activity};
use quire_app::vault::NotePath;
use quire_app::workspace::{NoticeLevel, UiRequest};

use crate::inbox_ui::review_ui::{added, short_time};
use crate::overlays::{OverlayResult, OverlayView};
use crate::panels::PanelOut;
use crate::registry::FeatureUi;
use crate::theme::Palette;

const HOUR: i64 = 3_600_000;
const DAY: i64 = 24 * HOUR;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Range {
    #[default]
    Day,
    Week,
    Month,
}

impl Range {
    const fn ms(self) -> i64 {
        match self {
            Self::Day => DAY,
            Self::Week => 7 * DAY,
            Self::Month => 30 * DAY,
        }
    }

    const fn label(self) -> &'static str {
        match self {
            Self::Day => "24 hours",
            Self::Week => "7 days",
            Self::Month => "30 days",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Since {
    Hour,
    #[default]
    Today,
    Day,
    Week,
}

impl Since {
    const ALL: [Self; 4] = [Self::Hour, Self::Today, Self::Day, Self::Week];

    const fn label(self) -> &'static str {
        match self {
            Self::Hour => "in the last hour",
            Self::Today => "today",
            Self::Day => "in the last 24 hours",
            Self::Week => "in the last 7 days",
        }
    }

    fn start(self, now: i64) -> i64 {
        match self {
            Self::Hour => now - HOUR,
            Self::Day => now - DAY,
            Self::Week => now - 7 * DAY,
            Self::Today => chrono::Local::now()
                .date_naive()
                .and_hms_opt(0, 0, 0)
                .and_then(|d| d.and_local_timezone(chrono::Local).single())
                .map_or(now - DAY, |d| d.timestamp_millis()),
        }
    }
}

/// UI state kept between frames (in [`FeatureUi`]).
#[derive(Default)]
pub(crate) struct ActivityUi {
    range: Range,
    expanded: HashSet<String>,
    agent: Option<String>,
    since: Since,
    /// A plan being worked out, and the one to show.
    plan_bg: Bg<RevertPlan>,
    plan: Option<(RevertPlan, String)>,
    /// Title of the plan being worked out.
    pending_title: String,
}

/// Whether the tab is offered.
pub(crate) const fn available(ws: &Workspace) -> bool {
    ws.agent_store().is_some()
}

fn key(r: &Run) -> String {
    format!("{}|{}", r.actor_id, r.start)
}

type Act = Box<dyn FnOnce(&mut Workspace, &mut ActivityUi, &mut PanelOut)>;

pub(crate) fn show(ui: &mut egui::Ui, ws: &mut Workspace, st: &mut FeatureUi, pal: &Palette) -> PanelOut {
    let mut out = PanelOut::default();
    let a = &mut st.activity;
    if ws.features.agent.activity_loaded.is_none() && !ws.features.agent.activity_bg.busy() {
        load_activity(ws);
    }
    if let Some(plan) = a.plan_bg.poll() {
        a.plan = Some((plan, std::mem::take(&mut a.pending_title)));
    }
    if a.plan_bg.busy() {
        ui.ctx().request_repaint_after(std::time::Duration::from_millis(100));
    }
    let mut act: Option<Act> = None;
    egui::ScrollArea::vertical().id_salt("quire-activity").auto_shrink([false, false]).show(ui, |ui| {
        ui.add_space(6.0);
        ui.horizontal_wrapped(|ui| {
            for r in [Range::Day, Range::Week, Range::Month] {
                ui.selectable_value(&mut a.range, r, r.label());
            }
            if ui.small_button("Reload").on_hover_text("Read the activity log again").clicked() {
                act = Some(Box::new(|ws: &mut Workspace, _, _| load_activity(ws)));
            }
        });
        let now = ws.agent_store().map_or(0, quire_app::agent::AgentStore::now_ms);
        let runs: Vec<&Run> = ws.features.agent.activity.iter().filter(|r| r.end >= now - a.range.ms()).collect();
        ui.add_space(4.0);
        if ui.button("Undo an agent's changes…").clicked() {
            out.run = Some(quire_app::Command::UndoAgentChanges);
        }
        ui.add_space(6.0);
        if ws.features.agent.activity_loaded.is_none() {
            ui.label(RichText::new("Loading…").color(pal.muted));
            return;
        }
        if runs.is_empty() {
            ui.add_space(12.0);
            ui.add(
                egui::Label::new(
                    RichText::new(format!(
                        "No agent activity on this device in the last {}. When an agent writes or suggests changes, it shows here.",
                        a.range.label()
                    ))
                    .color(pal.muted),
                )
                .wrap(),
            );
            return;
        }
        if let Some(k) = lanes(ui, &runs, now, a.range, pal) {
            a.expanded.insert(k);
        }
        ui.add_space(6.0);
        for r in runs {
            run_card(ui, ws, a, r, pal, &mut act);
        }
        ui.add_space(12.0);
    });
    if let Some(f) = act {
        f(ws, a, &mut out);
    }
    out
}

/// The timeline: one lane per actor over the chosen range. Returns the run clicked.
fn lanes(ui: &mut egui::Ui, runs: &[&Run], now: i64, range: Range, pal: &Palette) -> Option<String> {
    let mut actors: Vec<(&str, &str)> = runs.iter().map(|r| (r.actor_id.as_str(), r.actor_label.as_str())).collect();
    actors.sort_unstable();
    actors.dedup();
    let lane_h = 20.0;
    let label_w = 86.0;
    let w = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(Vec2::new(w, lane_h * actors.len() as f32 + 18.0), Sense::hover());
    let p = ui.painter_at(rect);
    let t0 = now - range.ms();
    let x_of = |t: i64| rect.left() + label_w + (w - label_w - 4.0) * ((t - t0) as f32 / range.ms() as f32).clamp(0.0, 1.0);
    // Ticks: every 6 hours, every day, or every week.
    let step = match range {
        Range::Day => 6 * HOUR,
        Range::Week => DAY,
        Range::Month => 7 * DAY,
    };
    let mut t = now - now.rem_euclid(step);
    while t > t0 {
        let x = x_of(t);
        p.line_segment([Pos2::new(x, rect.top()), Pos2::new(x, rect.bottom() - 14.0)], Stroke::new(1.0, pal.border));
        let label = chrono::DateTime::from_timestamp_millis(t)
            .map(|d| d.with_timezone(&chrono::Local))
            .map_or_else(String::new, |d| if range == Range::Day { d.format("%H:%M").to_string() } else { d.format("%d %b").to_string() });
        p.text(Pos2::new(x, rect.bottom() - 2.0), Align2::CENTER_BOTTOM, label, FontId::proportional(10.0), pal.faint);
        t -= step;
    }
    let pointer = ui.input(|i| i.pointer.hover_pos());
    let clicked = ui.input(|i| i.pointer.primary_clicked());
    let mut hit = None;
    for (i, (id, label)) in actors.iter().enumerate() {
        let y = rect.top() + lane_h * i as f32 + lane_h / 2.0;
        let short: String = label.chars().take(12).collect();
        p.text(Pos2::new(rect.left(), y), Align2::LEFT_CENTER, short, FontId::proportional(11.5), pal.muted);
        p.line_segment([Pos2::new(rect.left() + label_w, y), Pos2::new(rect.right() - 4.0, y)], Stroke::new(1.0, pal.border));
        for r in runs.iter().filter(|r| r.actor_id == *id) {
            let (a, b) = (x_of(r.start), x_of(r.end).max(x_of(r.start) + 5.0));
            let bar = Rect::from_min_max(Pos2::new(a, y - 5.0), Pos2::new(b, y + 5.0));
            let color = match r.kind {
                ActorKind::Human => pal.muted,
                ActorKind::Job => pal.tag,
                ActorKind::Import => pal.warn,
                ActorKind::Agent => added(pal),
            };
            let hover = pointer.is_some_and(|q| bar.expand(3.0).contains(q));
            p.rect_filled(bar, 3.0, if hover { color } else { color.gamma_multiply(0.7) });
            if hover {
                ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                if clicked {
                    hit = Some(key(r));
                }
            }
        }
    }
    hit
}

fn event_words(e: &LedgerEntry) -> String {
    let path = e.path.as_ref().map_or("", NotePath::as_str);
    match e.op {
        Op::Write => format!("changed {path}"),
        Op::Create => format!("created {path}"),
        Op::Rename => format!("renamed {path} → {}", e.to.as_ref().map_or("", NotePath::as_str)),
        Op::Delete => format!("deleted {path}"),
        Op::Propose => format!("suggested changes to {path}"),
        Op::Accept => format!("accepted a suggestion for {path}"),
        Op::Reject => format!("rejected a suggestion for {path}"),
        Op::Revert => {
            if path.is_empty() {
                "put a suggestion back".into()
            } else {
                format!("undid changes to {path}")
            }
        }
        Op::Read => format!("read {path}"),
        Op::Batch => "finished a batch".into(),
    }
}

fn summary(r: &Run) -> String {
    let count = |ops: &[Op]| r.events.iter().filter(|e| ops.contains(&e.op)).count();
    let mut parts = Vec::new();
    let w = count(&[Op::Write, Op::Create, Op::Rename, Op::Delete, Op::Revert]);
    if w > 0 {
        parts.push(format!("{w} change{}", if w == 1 { "" } else { "s" }));
    }
    let s = r.proposals().len();
    if s > 0 {
        parts.push(format!("{s} suggestion{}", if s == 1 { "" } else { "s" }));
    }
    let d = count(&[Op::Accept, Op::Reject]);
    if d > 0 {
        parts.push(format!("{d} decision{}", if d == 1 { "" } else { "s" }));
    }
    let rd = count(&[Op::Read]);
    if rd > 0 {
        parts.push(format!("{rd} read{}", if rd == 1 { "" } else { "s" }));
    }
    parts.join(" · ")
}

fn selector_for(r: &Run) -> RevertSelector {
    match (&r.id, &r.batch) {
        (Some(run), _) => RevertSelector { run: Some(run.clone()), actor: Some(r.actor_id.clone()), ..RevertSelector::default() },
        (None, Some(b)) => RevertSelector { batch: Some(b.clone()), ..RevertSelector::default() },
        (None, None) => {
            RevertSelector { actor: Some(r.actor_id.clone()), since: Some(r.start), until: Some(r.end), ..RevertSelector::default() }
        }
    }
}

fn start_plan(ws: &Workspace, a: &mut ActivityUi, sel: RevertSelector, title: String) {
    let Some(store) = ws.agent_store().cloned() else { return };
    a.plan = None;
    a.pending_title = title;
    a.plan_bg.start(move || store.revert_plan(&sel));
}

fn run_card(ui: &mut egui::Ui, ws: &Workspace, a: &mut ActivityUi, r: &Run, pal: &Palette, act: &mut Option<Act>) {
    let k = key(r);
    let open = a.expanded.contains(&k);
    egui::Frame::NONE
        .fill(pal.bg)
        .stroke(Stroke::new(1.0, pal.border))
        .corner_radius(6.0)
        .inner_margin(egui::Margin::symmetric(8, 6))
        .outer_margin(egui::Margin { bottom: 6, ..egui::Margin::ZERO })
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            let when = if r.end - r.start > 60_000 {
                format!(
                    "{}–{}",
                    short_time(r.start),
                    chrono::DateTime::from_timestamp_millis(r.end)
                        .map(|d| d.with_timezone(&chrono::Local).format("%H:%M").to_string())
                        .unwrap_or_default()
                )
            } else {
                short_time(r.start)
            };
            ui.label(RichText::new(format!("{} · {when}", r.actor_label)).color(pal.strong));
            ui.label(RichText::new(summary(r)).size(12.0).color(pal.muted));
            ui.horizontal_wrapped(|ui| {
                if ui.small_button(if open { "Less" } else { "Events" }).clicked() {
                    if open {
                        a.expanded.remove(&k);
                    } else {
                        a.expanded.insert(k.clone());
                    }
                }
                if r.kind != ActorKind::Human
                    && r.writes() > 0
                    && ui.small_button("Revert run").on_hover_text("See what undoing this run would do, then decide").clicked()
                {
                    let sel = selector_for(r);
                    let title = format!("Undo {}'s changes from {}", r.actor_label, short_time(r.start));
                    *act = Some(Box::new(move |ws: &mut Workspace, _, _| {
                        ws.features.agent.revert_preset = Some((sel, title));
                        ws.request_ui(UiRequest::Feature(quire_app::Command::UndoAgentChanges));
                    }));
                }
                let waiting: Vec<_> =
                    r.proposals().into_iter().filter(|id| ws.features.agent.inbox.pending.iter().any(|p| &p.id == id)).collect();
                if !waiting.is_empty() {
                    let w2 = waiting.clone();
                    if ui
                        .small_button("Accept the rest")
                        .on_hover_text("Every suggestion of this run still waiting, except conflicts")
                        .clicked()
                    {
                        *act = Some(Box::new(move |ws: &mut Workspace, _, _| {
                            for id in &w2 {
                                ws.resolve_whole_proposal(id, true);
                            }
                        }));
                    }
                    if ui.small_button("Reject the rest").clicked() {
                        *act = Some(Box::new(move |ws: &mut Workspace, _, _| {
                            for id in &waiting {
                                ws.resolve_whole_proposal(id, false);
                            }
                        }));
                    }
                }
            });
            if open {
                for e in r.events.iter().rev().take(200) {
                    ui.horizontal_wrapped(|ui| {
                        let t = chrono::DateTime::from_timestamp_millis(e.at)
                            .map(|d| d.with_timezone(&chrono::Local).format("%H:%M").to_string())
                            .unwrap_or_default();
                        ui.label(RichText::new(t).size(11.5).color(pal.faint).monospace());
                        ui.label(RichText::new(event_words(e)).size(12.0).color(pal.text));
                        let target = e.to.clone().or_else(|| e.path.clone()).filter(|p| ws.vault().is_some_and(|v| v.exists(p)));
                        if let Some(p) = target
                            && matches!(e.op, Op::Write | Op::Create | Op::Rename | Op::Accept | Op::Revert)
                            && ui.small_button("Open diff").on_hover_text("Version history of this note").clicked()
                        {
                            *act = Some(Box::new(move |ws: &mut Workspace, _, out: &mut PanelOut| {
                                if ws.open_note(&p).is_some() {
                                    out.run = Some(quire_app::Command::ShowHistory);
                                }
                            }));
                        }
                    });
                }
            }
        });
}

/// "Undo an agent's changes" (Activity → the button, a run's "Revert run",
/// or the command): pick an agent and a time, look at the plan, apply.
pub(crate) struct RevertOverlay {
    a: ActivityUi,
    done: bool,
}

impl RevertOverlay {
    /// The dialog, on a run's selection when one was asked for.
    pub(crate) fn make(ws: &mut Workspace) -> Box<dyn OverlayView> {
        let mut a = ActivityUi::default();
        if ws.features.agent.activity_loaded.is_none() && !ws.features.agent.activity_bg.busy() {
            load_activity(ws);
        }
        if let Some((sel, title)) = ws.features.agent.revert_preset.take() {
            if let Some(id) = &sel.actor {
                a.agent = Some(id.clone());
            }
            start_plan(ws, &mut a, sel, title);
        }
        Box::new(Self { a, done: false })
    }
}

impl OverlayView for RevertOverlay {
    fn show(&mut self, ctx: &egui::Context, ws: &mut Workspace, pal: &Palette) -> OverlayResult {
        let a = &mut self.a;
        if let Some(plan) = a.plan_bg.poll() {
            a.plan = Some((plan, std::mem::take(&mut a.pending_title)));
        }
        if let Some(runs) = ws.features.agent.activity_bg.poll() {
            ws.features.agent.activity = runs;
            ws.features.agent.activity_loaded = Some(std::time::Instant::now());
        }
        if a.plan_bg.busy() || ws.features.agent.activity_bg.busy() {
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
        }
        let now = ws.agent_store().map_or(0, quire_app::agent::AgentStore::now_ms);
        let mut act: Option<Act> = None;
        let mut close = false;
        egui::Modal::new(egui::Id::unique("quire-revert")).frame(egui::Frame::popup(&ctx.global_style()).fill(pal.panel)).show(ctx, |ui| {
            let w = 560.0_f32.min(ctx.content_rect().width() - 48.0);
            ui.set_width(w);
            ui.set_max_width(w);
            ui.heading("Undo an agent's changes");
            ui.separator();
            let h = (ctx.content_rect().height() - 180.0).clamp(120.0, 420.0);
            egui::ScrollArea::vertical().id_salt("quire-revert-body").max_height(h).auto_shrink([false, true]).show(ui, |ui| {
                revert_body(ui, ws, a, now, pal);
            });
            ui.separator();
            ui.horizontal_wrapped(|ui| {
                if let Some((plan, title)) = a.plan.clone().filter(|(p, _)| !p.steps.is_empty())
                    && ui.button(RichText::new("Undo these changes").color(pal.danger)).clicked()
                {
                    act = Some(Box::new(move |ws: &mut Workspace, a: &mut ActivityUi, _| apply_revert(ws, a, &plan, &title)));
                    self.done = true;
                }
                if ui.button("Close").clicked() {
                    close = true;
                }
            });
        });
        if let Some(f) = act {
            f(ws, a, &mut PanelOut::default());
        }
        OverlayResult { close: close || self.done, ..OverlayResult::default() }
    }
}

/// The dialog's body: the agent and time, and the plan once worked out.
fn revert_body(ui: &mut egui::Ui, ws: &Workspace, a: &mut ActivityUi, now: i64, pal: &Palette) {
    let mut agents: Vec<(String, String)> = ws
        .features
        .agent
        .activity
        .iter()
        .filter(|r| r.kind != ActorKind::Human)
        .map(|r| (r.actor_id.clone(), r.actor_label.clone()))
        .collect();
    agents.sort();
    agents.dedup();
    if ws.features.agent.activity_loaded.is_none() {
        ui.label(RichText::new("Loading…").color(pal.muted));
    } else if agents.is_empty() {
        ui.label(RichText::new("No agent has changed notes on this device in the last 30 days.").size(12.5).color(pal.muted));
    } else {
        if a.agent.as_ref().is_none_or(|x| !agents.iter().any(|(id, _)| id == x)) {
            a.agent = Some(agents[0].0.clone());
        }
        ui.horizontal_wrapped(|ui| {
            ui.label("Everything");
            let who = agents.iter().find(|(id, _)| Some(id) == a.agent.as_ref()).map_or("", |(_, l)| l.as_str());
            egui::ComboBox::from_id_salt("quire-revert-agent").selected_text(who).width(120.0).show_ui(ui, |ui| {
                for (id, l) in &agents {
                    ui.selectable_value(&mut a.agent, Some(id.clone()), l);
                }
            });
            ui.label("did");
            egui::ComboBox::from_id_salt("quire-revert-since").selected_text(a.since.label()).width(150.0).show_ui(ui, |ui| {
                for s in Since::ALL {
                    ui.selectable_value(&mut a.since, s, s.label());
                }
            });
        });
        let busy = a.plan_bg.busy();
        ui.horizontal_wrapped(|ui| {
            if ui.add_enabled(!busy, egui::Button::new("Show what would change")).clicked()
                && let Some(id) = a.agent.clone()
            {
                let label = agents.iter().find(|(x, _)| *x == id).map_or_else(|| id.clone(), |(_, l)| l.clone());
                let since = a.since.start(now);
                let title = format!("Undo {label}'s changes {}", a.since.label());
                start_plan(ws, a, RevertSelector { actor: Some(id), since: Some(since), ..RevertSelector::default() }, title);
            }
            if busy {
                ui.spinner();
            }
        });
    }
    if let Some((plan, title)) = &a.plan {
        ui.separator();
        ui.label(RichText::new(title).color(pal.text));
        if let Some(n) = &plan.note {
            ui.label(RichText::new(n).size(12.5).color(pal.muted));
        }
        for s in &plan.steps {
            ui.add(egui::Label::new(RichText::new(format!("• {}", s.describe())).size(12.5).color(pal.text)).wrap());
        }
        for (p, why) in &plan.skipped {
            ui.add(egui::Label::new(RichText::new(format!("• Can't undo “{}”: {why}", p.stem())).size(12.5).color(pal.warn)).wrap());
        }
        if !plan.steps.is_empty() {
            ui.add(
                egui::Label::new(
                    RichText::new("Edits made since are kept. Notes edited where the agent wrote become a suggestion for you to review.")
                        .size(12.0)
                        .color(pal.muted),
                )
                .wrap(),
            );
        }
    }
}

/// A sample for `layout_check`: a plan with many steps.
#[cfg(test)]
impl RevertOverlay {
    pub(crate) fn sample() -> Self {
        use quire_app::agent::revert::{RevertAction, RevertStep};
        let steps = (0..30)
            .map(|i| RevertStep {
                path: NotePath::new(format!("Notes/Note {i}.md")),
                action: RevertAction::Restore { text: String::new() },
            })
            .collect();
        let plan = RevertPlan { steps, ..RevertPlan::default() };
        Self { a: ActivityUi { plan: Some((plan, "Undo Claude Code's changes today".into())), ..ActivityUi::default() }, done: false }
    }
}

fn apply_revert(ws: &mut Workspace, a: &mut ActivityUi, plan: &RevertPlan, title: &str) {
    let Some(store) = ws.agent_store().cloned() else { return };
    // The files must hold the person's latest edits first.
    ws.save_all();
    let report = store.revert_apply(plan, title);
    let mut msg = format!("Undid {} change{}", report.applied, if report.applied == 1 { "" } else { "s" });
    if report.proposal.is_some() {
        msg.push_str("; the rest is in the Inbox to review");
    }
    ws.notify(NoticeLevel::Info, msg);
    for (p, e) in &report.failed {
        ws.notify(NoticeLevel::Error, format!("Couldn't undo “{}”: {e}", p.stem()));
    }
    a.plan = None;
    load_activity(ws);
    ws.features.agent.reload_inbox_soon();
}
