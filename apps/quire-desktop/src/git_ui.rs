// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The Git tab of the right panel (design §F.5): the working tree, commit,
//! pull and push, the vault's git settings, and the git history of the
//! open note next to Quire's own version history.

use egui::{RichText, Ui};
use quire_app::git::{self, GitPrefs, LogEntry};
use quire_app::vault::NotePath;
use quire_app::{Command, Workspace};

use crate::panels::PanelOut;
use crate::registry::FeatureUi;
use crate::theme::Palette;

/// UI state of the Git tab.
#[derive(Default)]
pub(crate) struct GitUi {
    /// The note whose history was asked for.
    log_for: Option<NotePath>,
    /// The commit whose change is shown.
    open: Option<String>,
    show_files: bool,
    /// Settings being edited (saved when they change).
    prefs: Option<GitPrefs>,
}

pub(crate) const fn available(ws: &Workspace) -> bool {
    ws.features.git.repo
}

pub(crate) fn label(ws: &Workspace) -> String {
    match ws.features.git.status.as_ref().map(|s| s.changed.len()) {
        Some(n) if n > 0 => format!("Git · {n}"),
        _ => "Git".into(),
    }
}

fn section(ui: &mut Ui, pal: &Palette, title: &str) {
    ui.add_space(10.0);
    ui.label(RichText::new(title.to_uppercase()).size(11.0).color(pal.muted).strong());
    ui.add_space(2.0);
}

pub(crate) fn show(ui: &mut Ui, ws: &mut Workspace, st: &mut FeatureUi, pal: &Palette) -> PanelOut {
    let st = &mut st.git;
    let mut out = PanelOut::default();
    let g = &ws.features.git;
    if !g.repo {
        ui.label(RichText::new(g.unavailable.clone().unwrap_or_else(|| "This vault isn't in a git repository.".into())).color(pal.muted));
        return out;
    }
    egui::ScrollArea::vertical().id_salt("quire-git").auto_shrink([false, false]).show(ui, |ui| {
        let g = &ws.features.git;
        if let Some(s) = &g.status {
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new(&s.branch).strong().color(pal.strong));
                if let Some(u) = &s.upstream {
                    ui.label(RichText::new(format!("→ {u}")).size(12.0).color(pal.muted));
                }
                if s.ahead > 0 {
                    ui.label(RichText::new(format!("↑{}", s.ahead)).size(12.0).color(pal.muted)).on_hover_text("Commits to push");
                }
                if s.behind > 0 {
                    ui.label(RichText::new(format!("↓{}", s.behind)).size(12.0).color(pal.muted)).on_hover_text("Commits to pull");
                }
            });
            let n = s.changed.len();
            let summary =
                if n == 0 { "Nothing to commit".to_string() } else { format!("{n} changed file{}", if n == 1 { "" } else { "s" }) };
            if n == 0 {
                ui.label(RichText::new(summary).size(12.5).color(pal.muted));
            } else if ui.link(RichText::new(summary).size(12.5)).on_hover_text("Show the files").clicked() {
                st.show_files = !st.show_files;
            }
            if st.show_files {
                for f in s.changed.iter().take(200) {
                    ui.label(RichText::new(f).size(11.5).color(pal.muted));
                }
            }
        }
        for b in &g.busy {
            ui.horizontal(|ui| {
                ui.add(egui::Spinner::new().size(12.0));
                ui.label(RichText::new(*b).size(12.0).color(pal.muted));
            });
        }
        if let Some((msg, err)) = &g.last {
            ui.add(egui::Label::new(RichText::new(msg).size(12.0).color(if *err { pal.danger } else { pal.muted })).wrap());
        }
        let can_remote = git::can_pull_push(ws);
        ui.horizontal_wrapped(|ui| {
            let changed = ws.features.git.status.as_ref().is_some_and(|s| !s.changed.is_empty());
            if ui.add_enabled(changed, egui::Button::new("Commit now")).clicked() {
                out.run = Some(Command::GitCommitNow);
            }
            let why = if can_remote { "" } else { git::SYNC_ON };
            if ui.add_enabled(can_remote, egui::Button::new("Pull")).on_disabled_hover_text(why).clicked() {
                out.run = Some(Command::GitPull);
            }
            if ui.add_enabled(can_remote, egui::Button::new("Push")).on_disabled_hover_text(why).clicked() {
                out.run = Some(Command::GitPush);
            }
        });
        if !can_remote {
            ui.label(RichText::new("Pull and push are off while Quire Sync is on.").size(11.5).color(pal.muted));
        }

        settings(ui, ws, st, pal);
        history(ui, ws, st, pal);
    });
    out
}

fn settings(ui: &mut Ui, ws: &mut Workspace, st: &mut GitUi, pal: &Palette) {
    section(ui, pal, "Settings for this vault");
    let current = ws.features.git.prefs.clone();
    let p = st.prefs.get_or_insert_with(|| current.clone());
    let mut changed = false;
    changed |= ui
        .checkbox(&mut p.auto_commit, "Commit by itself")
        .on_hover_text("When notes have changed and you've stopped typing for a while")
        .changed();
    ui.add_enabled_ui(p.auto_commit, |ui| {
        ui.horizontal(|ui| {
            ui.label(RichText::new("after").size(12.5));
            let mut mins = (p.commit_after_idle_s / 60).max(1);
            if ui.add(egui::DragValue::new(&mut mins).range(1..=1440).suffix(" min")).changed() {
                p.commit_after_idle_s = mins * 60;
                changed = true;
            }
            ui.label(RichText::new("without edits").size(12.5));
        });
    });
    ui.horizontal(|ui| {
        ui.label(RichText::new("Message").size(12.5));
        let r = ui.add(egui::TextEdit::singleline(&mut p.message).desired_width(160.0)).on_hover_text("{{count}} is the number of files");
        changed |= r.lost_focus() && p.message != current.message;
    });
    let sync = ws.sync.config.is_some();
    ui.add_enabled_ui(!sync, |ui| {
        changed |= ui.checkbox(&mut p.pull_on_start, "Pull when the vault opens").changed();
        changed |= ui.checkbox(&mut p.push_after_commit, "Push after each commit").changed();
    });
    if changed && *p != current {
        let p = p.clone();
        if let Err(e) = git::set_prefs(ws, &p) {
            ws.notify(quire_app::NoticeLevel::Error, format!("Couldn't save the git settings: {e}"));
        }
        st.prefs = None;
    }
    if !git::gitignore_ok(ws) {
        ui.add_space(4.0);
        ui.label(
            RichText::new("Quire's caches and history (.quire/) don't belong in git, but its vault settings (.quire/shared/) do.")
                .size(11.5)
                .color(pal.muted),
        );
        if ui.small_button("Add that to .gitignore").clicked()
            && let Err(e) = git::add_gitignore(ws)
        {
            ws.notify(quire_app::NoticeLevel::Error, format!("Couldn't write .gitignore: {e}"));
        }
    }
}

/// The open note's git history (the Git tab and Version history → Git).
pub(crate) fn history(ui: &mut Ui, ws: &mut Workspace, st: &mut GitUi, pal: &Palette) {
    let Some(path) = ws.active_doc().and_then(|d| d.path.clone()) else { return };
    section(ui, pal, &format!("Git history of {}", path.stem()));
    if st.log_for.as_ref() != Some(&path) {
        st.log_for = Some(path.clone());
        st.open = None;
        git::request_log(ws, &path);
    }
    let log = ws.features.git.log.clone();
    let entries: Vec<LogEntry> = match log {
        Some((p, Ok(e))) if p == path => e,
        Some((p, Err(e))) if p == path => {
            ui.label(RichText::new(e).size(12.0).color(pal.danger));
            return;
        }
        _ => {
            ui.label(RichText::new("Reading…").size(12.0).color(pal.muted));
            return;
        }
    };
    if entries.is_empty() {
        ui.label(RichText::new("Not committed yet.").size(12.0).color(pal.muted));
    }
    if ui.small_button("Refresh").clicked() {
        git::request_log(ws, &path);
    }
    for e in &entries {
        let open = st.open.as_deref() == Some(e.hash.as_str());
        let when = quire_app::chrono_fmt(e.at * 1000);
        let r = ui
            .horizontal_wrapped(|ui| {
                let r = ui.selectable_label(open, RichText::new(&e.subject).size(12.5));
                ui.label(RichText::new(format!("{} · {} · {}", e.short, when, e.author)).size(11.0).color(pal.muted));
                r
            })
            .inner;
        if r.clicked() {
            if open {
                st.open = None;
            } else {
                st.open = Some(e.hash.clone());
                git::request_show(ws, e);
            }
        }
        if open {
            diff_view(ui, ws, &e.hash, pal);
        }
    }
}

fn diff_view(ui: &mut Ui, ws: &Workspace, hash: &str, pal: &Palette) {
    match &ws.features.git.shown {
        Some((h, Ok(diff))) if h == hash => {
            egui::Frame::NONE.fill(pal.code_bg).corner_radius(6.0).inner_margin(8.0).show(ui, |ui| {
                egui::ScrollArea::both().id_salt(("quire-git-diff", hash)).max_height(320.0).show(ui, |ui| {
                    for line in diff.lines().skip_while(|l| !l.starts_with("@@")).take(2000) {
                        let color = if line.starts_with('+') {
                            pal.added
                        } else if line.starts_with('-') {
                            pal.danger
                        } else if line.starts_with("@@") {
                            pal.muted
                        } else {
                            pal.text
                        };
                        ui.label(RichText::new(line).monospace().size(11.5).color(color));
                    }
                });
            });
        }
        Some((h, Err(e))) if h == hash => {
            ui.label(RichText::new(e).size(12.0).color(pal.danger));
        }
        _ => {
            ui.label(RichText::new("Reading…").size(12.0).color(pal.muted));
        }
    }
}
