// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Tab bar, sidebar (files / search / tags / tasks), insights panel, status
//! bar, preview and the welcome screen.

use std::collections::BTreeMap;

use egui::text::{LayoutJob, TextFormat};
use egui::{FontId, RichText};
use egui_commonmark::{CommonMarkCache, CommonMarkViewer};
use quire_app::index::HybridHit;
use quire_app::intel::llm::Assist;
use quire_app::markdown::render::{for_preview, parse_note_url};
use quire_app::vault::NotePath;
use quire_app::{Command, Workspace};

use crate::theme::Palette;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum SidebarTab {
    #[default]
    Files,
    Search,
    Tags,
    Tasks,
}

#[derive(Default)]
pub(crate) struct SidebarState {
    pub tab: SidebarTab,
    pub query: String,
    pub focus_query: bool,
    cache: Option<(String, u64, u64, Vec<HybridHit>)>,
    open_tag: Option<String>,
}

/// What a panel asks the app to do after drawing.
#[derive(Default)]
pub(crate) struct PanelOut {
    pub run: Option<Command>,
    /// Focus the editor and scroll to the caret.
    pub jumped: bool,
    /// Scroll to the caret without taking focus.
    pub reveal_only: bool,
    /// Hide the available update until a newer one appears.
    pub skip_update: bool,
    /// Open this note in the other half of a split editor.
    pub open_side: Option<NotePath>,
}

fn section(ui: &mut egui::Ui, pal: &Palette, title: &str) {
    ui.add_space(10.0);
    ui.label(RichText::new(title.to_uppercase()).size(11.0).color(pal.muted).strong());
    ui.add_space(2.0);
}

// ---------------------------------------------------------------- merge banner

/// Shown above a note whose outside change conflicts with unsaved edits.
pub(crate) fn merge_banner(ui: &mut egui::Ui, ws: &mut Workspace, pal: &Palette) -> PanelOut {
    let mut out = PanelOut::default();
    let Some(doc) = ws.active_doc() else { return out };
    let Some(m) = &doc.pending_merge else { return out };
    let (id, n) = (doc.id, m.conflicts);
    let mut choice = None;
    ui.horizontal_wrapped(|ui| {
        ui.label(
            RichText::new(format!(
                "⚠ This note was also changed elsewhere. {n} change{} conflict{} with your unsaved edits. Autosave is paused.",
                if n == 1 { "" } else { "s" },
                if n == 1 { "s" } else { "" }
            ))
            .color(pal.text),
        );
        if ui.button("Keep mine").on_hover_text("Save your version; the other one stays in version history").clicked() {
            choice = Some(quire_app::sync::Resolution::KeepMine);
        }
        if ui.button("Take theirs").on_hover_text("Use the other version (Undo brings yours back)").clicked() {
            choice = Some(quire_app::sync::Resolution::TakeTheirs);
        }
        if ui.button("Keep both").on_hover_text("Put both versions in the note between <<<<<<< and >>>>>>> lines").clicked() {
            choice = Some(quire_app::sync::Resolution::MergeWithMarkers);
        }
    });
    if let Some(how) = choice {
        ws.resolve_merge(id, how);
        out.jumped = true;
    }
    out
}

// ---------------------------------------------------------------- note menu

#[derive(Clone, Copy)]
enum NoteAction {
    OpenSide,
    Rename,
    Move,
    History,
    CopyLink,
    Reveal,
    Trash,
}

/// Right-click menu shared by sidebar entries and tabs.
fn note_menu(resp: &egui::Response) -> Option<NoteAction> {
    let mut act = None;
    resp.context_menu(|ui| {
        for (label, a) in [
            ("Open to the side", NoteAction::OpenSide),
            ("Rename…", NoteAction::Rename),
            ("Move to folder…", NoteAction::Move),
            ("Version history…", NoteAction::History),
            ("Copy [[link]]", NoteAction::CopyLink),
            ("Show in folder", NoteAction::Reveal),
        ] {
            if ui.button(label).clicked() {
                act = Some(a);
                ui.close();
            }
        }
        ui.separator();
        if ui.button("Move to trash…").clicked() {
            act = Some(NoteAction::Trash);
            ui.close();
        }
    });
    act
}

fn apply_note_action(ctx: &egui::Context, ws: &mut Workspace, path: &NotePath, act: NoteAction, out: &mut PanelOut) {
    match act {
        NoteAction::OpenSide => out.open_side = Some(path.clone()),
        NoteAction::CopyLink => {
            ctx.copy_text(format!("[[{}]]", path.stem()));
            ws.notify(quire_app::workspace::NoticeLevel::Info, format!("Copied [[{}]]", path.stem()));
        }
        NoteAction::Reveal => {
            if let Some(dir) = ws.vault_root().map(|r| r.join(path.folder())) {
                ctx.open_url(egui::OpenUrl::new_tab(format!("file://{}", dir.display())));
            }
        }
        NoteAction::Rename | NoteAction::Move | NoteAction::History | NoteAction::Trash => {
            // These act on the open note, so open it first.
            if ws.active_doc().and_then(|d| d.path.as_ref()) != Some(path) && ws.open_note(path).is_none() {
                return;
            }
            out.run = Some(match act {
                NoteAction::Rename => Command::RenameNote,
                NoteAction::Move => Command::MoveNote,
                NoteAction::History => Command::ShowHistory,
                _ => Command::DeleteNote,
            });
        }
    }
}

// ---------------------------------------------------------------- tab bar

pub(crate) fn tab_bar(ui: &mut egui::Ui, ws: &mut Workspace, pal: &Palette) -> PanelOut {
    let mut out = PanelOut::default();
    ui.horizontal(|ui| {
        ui.add_space(4.0);
        ui.label(RichText::new(ws.vault_name()).strong().color(pal.strong));
        ui.add_space(4.0);
        ui.separator();
        let mut close = None;
        let mut activate = None;
        let mut tab_action: Option<(usize, NotePath, NoteAction)> = None;
        egui::ScrollArea::horizontal().id_salt("tabs").max_width(ui.available_width() - 440.0).show(ui, |ui| {
            ui.horizontal(|ui| {
                for (i, d) in ws.docs().iter().enumerate() {
                    let active = ws.active_index() == Some(i);
                    let dot = if d.conflict {
                        " ⚠"
                    } else if d.is_dirty() {
                        " •"
                    } else {
                        ""
                    };
                    let r = ui.selectable_label(active, format!("{}{dot}", d.title()));
                    let r = if d.path.is_some() { r.on_hover_text("Double-click to rename · right-click for more") } else { r };
                    if r.clicked() {
                        activate = Some(i);
                    }
                    if let Some(p) = &d.path {
                        if r.double_clicked() {
                            tab_action = Some((i, p.clone(), NoteAction::Rename));
                        } else if let Some(a) = note_menu(&r) {
                            tab_action = Some((i, p.clone(), a));
                        }
                    }
                    if r.middle_clicked() {
                        close = Some(d.id);
                    }
                    if active && ui.small_button("×").on_hover_text("Close tab (Ctrl+W)").clicked() {
                        close = Some(d.id);
                    }
                }
            });
        });
        if let Some(i) = activate {
            ws.set_active(i);
            out.jumped = true;
        }
        if let Some(id) = close {
            ws.close(id);
        }
        if let Some((i, p, a)) = tab_action {
            ws.set_active(i);
            apply_note_action(ui.ctx(), ws, &p, a, &mut out);
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.selectable_label(ws.settings.show_insights, "Insights").on_hover_text("Ctrl+Shift+I").clicked() {
                out.run = Some(Command::ToggleInsights);
            }
            if ui.selectable_label(ws.settings.show_preview, "Preview").on_hover_text("Ctrl+E").clicked() {
                out.run = Some(Command::TogglePreview);
            }
            let (palette_keys, help_keys) = if cfg!(target_os = "macos") { ("⌘K", "⌘/") } else { ("Ctrl+K", "Ctrl+/") };
            let search = egui::Button::new(RichText::new(format!("Search or run a command   {palette_keys}")).color(pal.muted).size(12.5))
                .min_size(egui::vec2(220.0, 24.0));
            if ui.add(search).clicked() {
                out.run = Some(Command::CommandPalette);
            }
            if ui.button("?").on_hover_text(format!("Keyboard shortcuts ({help_keys})")).clicked() {
                out.run = Some(Command::ShowShortcuts);
            }
        });
    });
    out
}

// ---------------------------------------------------------------- sidebar

fn folder_tree(paths: &[NotePath]) -> BTreeMap<String, Vec<NotePath>> {
    let mut m: BTreeMap<String, Vec<NotePath>> = BTreeMap::new();
    for p in paths {
        m.entry(p.folder().to_string()).or_default().push(p.clone());
    }
    for v in m.values_mut() {
        v.sort_by_key(|p| p.stem().to_lowercase());
    }
    m
}

fn highlighted(text: &str, ranges: &[std::ops::Range<usize>], pal: &Palette) -> LayoutJob {
    let mut job = LayoutJob::default();
    let normal = TextFormat { font_id: FontId::proportional(12.5), color: pal.muted, ..Default::default() };
    let hl = TextFormat { color: pal.text, background: pal.mark_bg, ..normal.clone() };
    let mut pos = 0;
    for r in ranges {
        if r.start < pos || r.end > text.len() || !text.is_char_boundary(r.start) || !text.is_char_boundary(r.end) {
            continue;
        }
        job.append(&text[pos..r.start], 0.0, normal.clone());
        job.append(&text[r.clone()], 0.0, hl.clone());
        pos = r.end;
    }
    job.append(&text[pos..], 0.0, normal);
    job.wrap.max_width = f32::INFINITY;
    job
}

pub(crate) fn sidebar(ui: &mut egui::Ui, ws: &mut Workspace, st: &mut SidebarState, pal: &Palette) -> PanelOut {
    let mut out = PanelOut::default();
    ui.add_space(6.0);
    ui.horizontal(|ui| {
        for (tab, label) in
            [(SidebarTab::Files, "Files"), (SidebarTab::Search, "Search"), (SidebarTab::Tags, "Tags"), (SidebarTab::Tasks, "Tasks")]
        {
            if ui.selectable_value(&mut st.tab, tab, label).clicked() && tab == SidebarTab::Search {
                st.focus_query = true;
            }
        }
    });
    ui.separator();
    let active_path = ws.active_doc().and_then(|d| d.path.clone());
    let mut open: Option<(NotePath, Option<usize>)> = None;
    let mut file_action: Option<(NotePath, NoteAction)> = None;
    let mut folder_action: Option<(String, Command)> = None;

    match st.tab {
        SidebarTab::Files => {
            ui.horizontal(|ui| {
                ui.menu_button("+ New", |ui| {
                    for (label, keys, cmd) in [
                        ("Note", "Ctrl+N", Command::NewNote),
                        ("Project…", "Ctrl+Alt+N", Command::NewProject),
                        ("Note in folder…", "", Command::NewInFolder),
                        ("From template…", "Ctrl+Shift+N", Command::NewFromTemplate),
                    ] {
                        if ui.add(egui::Button::new(label).shortcut_text(keys)).clicked() {
                            out.run = Some(cmd);
                            ui.close();
                        }
                    }
                });
                if ui.button("Today").on_hover_text("Alt+D").clicked() {
                    out.run = Some(Command::DailyNote);
                }
                if ui.button("Scratch").on_hover_text("Alt+S — never saved to the vault").clicked() {
                    out.run = Some(Command::Scratchpad);
                }
            });
            ui.add_space(4.0);
            let paths = ws.index().read().paths();
            let tree = folder_tree(&paths);
            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                for (folder, notes) in &tree {
                    let mut draw = |ui: &mut egui::Ui| {
                        ui.spacing_mut().item_spacing.y = 1.0;
                        for p in notes {
                            let is_active = active_path.as_ref() == Some(p);
                            let r = crate::theme::row(ui, is_active, 26.0, pal, |painter, rect| {
                                let color = if is_active { pal.strong } else { pal.text };
                                let galley = painter.layout(p.stem().to_string(), FontId::proportional(13.5), color, rect.width() - 20.0);
                                let galley = if galley.rows.len() > 1 {
                                    // One line, ending in … when the name is long.
                                    let mut job = LayoutJob::simple_singleline(p.stem().to_string(), FontId::proportional(13.5), color);
                                    job.wrap = egui::text::TextWrapping::truncate_at_width(rect.width() - 20.0);
                                    painter.layout_job(job)
                                } else {
                                    galley
                                };
                                let y = rect.center().y - galley.size().y / 2.0;
                                painter.galley(egui::pos2(rect.left() + 10.0, y), galley, color);
                            })
                            .on_hover_text(p.as_str());
                            if r.clicked() {
                                open = Some((p.clone(), None));
                            }
                            if let Some(a) = note_menu(&r) {
                                file_action = Some((p.clone(), a));
                            }
                        }
                    };
                    if folder.is_empty() {
                        draw(ui);
                    } else {
                        let resp = egui::CollapsingHeader::new(RichText::new(folder.clone()).color(pal.muted).size(12.5))
                            .id_salt(folder)
                            .default_open(true)
                            .show(ui, draw);
                        resp.header_response.context_menu(|ui| {
                            if ui.button("New note here").clicked() {
                                folder_action = Some((folder.clone(), Command::NewInFolder));
                                ui.close();
                            }
                            if ui.button("New project…").clicked() {
                                folder_action = Some((folder.clone(), Command::NewProject));
                                ui.close();
                            }
                        });
                    }
                }
                if paths.is_empty() {
                    ui.label(RichText::new("No notes yet. Press Ctrl+N.").color(pal.muted));
                }
            });
        }
        SidebarTab::Search => {
            let r =
                ui.add(egui::TextEdit::singleline(&mut st.query).hint_text("rope \"undo tree\" #tag -draft").desired_width(f32::INFINITY));
            if std::mem::take(&mut st.focus_query) {
                r.request_focus();
            }
            let (version, sv) = (ws.index().version(), ws.index().semantic_version());
            let stale = st.cache.as_ref().is_none_or(|(q, v, s, _)| *q != st.query || *v != version || *s != sv);
            if stale {
                let hits = ws.index().hybrid_search(&st.query, 50);
                st.cache = Some((st.query.clone(), version, sv, hits));
            }
            let hits = st.cache.as_ref().map(|c| c.3.clone()).unwrap_or_default();
            let meaning = hits.iter().filter(|h| h.by_meaning).count();
            let summary = match (st.query.trim().is_empty(), meaning) {
                (true, _) => String::new(),
                (false, 0) => format!("{} results", hits.len()),
                (false, m) => format!("{} results · {m} by meaning (≈)", hits.len()),
            };
            ui.label(RichText::new(summary).color(pal.muted).size(11.5));
            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                for h in &hits {
                    let resp = ui
                        .vertical(|ui| {
                            let t = ui
                                .horizontal_wrapped(|ui| {
                                    let t = ui.add(
                                        egui::Label::new(RichText::new(&h.title).strong().color(pal.strong)).sense(egui::Sense::click()),
                                    );
                                    if h.by_meaning {
                                        ui.label(RichText::new("≈").color(pal.tag)).on_hover_text("Found by meaning, not exact words");
                                    }
                                    if let Some(hd) = h.heading.as_ref().filter(|hd| **hd != h.title) {
                                        ui.label(RichText::new(format!("› {hd}")).color(pal.muted).size(11.5));
                                    }
                                    t
                                })
                                .inner;
                            ui.add(egui::Label::new(highlighted(&h.snippet, &h.highlights, pal)).wrap());
                            ui.add_space(6.0);
                            t
                        })
                        .inner;
                    if resp.clicked() {
                        // Jump to the first occurrence of the first term.
                        let ix = ws.index().read();
                        let term = quire_app::index::Query::parse(&st.query).highlight_terms().into_iter().next();
                        let at = term.and_then(|t| ix.source(&h.path).and_then(|s| s.to_lowercase().find(&t))).or(h.offset);
                        open = Some((h.path.clone(), at));
                    }
                }
            });
        }
        SidebarTab::Tags => {
            let tags = ws.index().read().tags();
            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                for (tag, count) in &tags {
                    let depth = tag.matches('/').count();
                    let leaf = tag.rsplit('/').next().unwrap_or(tag);
                    ui.horizontal(|ui| {
                        ui.add_space(depth as f32 * 14.0);
                        let selected = st.open_tag.as_deref() == Some(tag.as_str());
                        if ui.selectable_label(selected, RichText::new(format!("#{leaf}")).color(pal.tag)).clicked() {
                            st.open_tag = if selected { None } else { Some(tag.clone()) };
                        }
                        ui.label(RichText::new(count.to_string()).color(pal.muted).size(11.0));
                    });
                    if st.open_tag.as_deref() == Some(tag.as_str()) {
                        let notes = ws.index().read().notes_with_tag(tag);
                        for p in notes {
                            ui.horizontal(|ui| {
                                ui.add_space(depth as f32 * 14.0 + 18.0);
                                if ui.link(p.stem()).clicked() {
                                    open = Some((p.clone(), None));
                                }
                            });
                        }
                    }
                }
                if tags.is_empty() {
                    ui.label(RichText::new("Tags like #idea or #area/topic show up here.").color(pal.muted));
                }
            });
        }
        SidebarTab::Tasks => {
            let tasks = ws.index().read().tasks(false);
            ui.label(RichText::new(format!("{} open", tasks.len())).color(pal.muted).size(11.5));
            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                let mut last: Option<NotePath> = None;
                for (p, t) in &tasks {
                    if last.as_ref() != Some(p) {
                        section(ui, pal, p.stem());
                        last = Some(p.clone());
                    }
                    let mut done = false;
                    ui.horizontal_wrapped(|ui| {
                        if ui.checkbox(&mut done, "").on_hover_text("Mark done").changed() && ws.open_note(p).is_some() {
                            ws.jump_to_byte(t.offset);
                            ws.run(Command::ToggleTask);
                            ws.save_all();
                        }
                        if ui.link(&t.text).clicked() {
                            open = Some((p.clone(), Some(t.offset)));
                        }
                    });
                }
            });
        }
    }

    if let Some((p, a)) = file_action {
        apply_note_action(ui.ctx(), ws, &p, a, &mut out);
    }
    if let Some((folder, cmd)) = folder_action {
        if cmd == Command::NewInFolder {
            // Straight into a new note there; the dialog is for picking.
            if let Err(e) = ws.new_note_in(&folder) {
                ws.notify(quire_app::workspace::NoticeLevel::Error, e);
            } else {
                out.jumped = true;
            }
        } else {
            out.run = Some(cmd);
        }
    }
    if let Some((p, at)) = open
        && ws.open_note(&p).is_some()
    {
        if let Some(b) = at {
            ws.jump_to_byte(b);
        }
        out.jumped = true;
    }
    out
}

// ---------------------------------------------------------------- insights

pub(crate) fn insights(ui: &mut egui::Ui, ws: &mut Workspace, pal: &Palette) -> PanelOut {
    let mut out = PanelOut::default();
    let Some(doc) = ws.active_doc() else {
        ui.label(RichText::new("Open a note to see its outline, backlinks and related notes.").color(pal.muted));
        return out;
    };
    let outline = doc.outline();
    let ins = ws.insights();
    let outgoing = ws.outgoing();
    let mut jump_byte = None;
    let mut open_link: Option<(String, Option<String>)> = None;
    let mut open_path: Option<(NotePath, Option<usize>)> = None;
    let mut link_one = None;
    let mut add_tag = None;

    let doc_id = doc.id;
    let has_path = doc.path.is_some();
    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        if has_path {
            let n = ws.active_properties().len();
            egui::CollapsingHeader::new(RichText::new(format!("PROPERTIES · {n}")).size(11.0).color(pal.muted).strong())
                .id_salt(("quire-props-header", doc_id.0))
                .default_open(n > 0)
                .show(ui, |ui| crate::view_ui::properties(ui, ws, pal, doc_id));
        }
        if !outline.is_empty() {
            section(ui, pal, "Outline");
            for h in &outline {
                ui.horizontal(|ui| {
                    ui.add_space(f32::from(h.level.saturating_sub(1)) * 12.0);
                    if ui.link(RichText::new(&h.text).color(pal.text)).clicked() {
                        jump_byte = Some(h.offset);
                    }
                });
            }
        }

        section(ui, pal, &format!("Backlinks · {}", ins.backlinks.len()));
        if ins.backlinks.is_empty() {
            ui.label(RichText::new("No notes link here yet.").color(pal.muted).size(12.5));
        }
        for b in &ins.backlinks {
            if ui.link(RichText::new(b.from.stem()).strong()).clicked() {
                open_path = Some((b.from.clone(), Some(b.link.range.start)));
            }
            ui.label(RichText::new(&b.context).color(pal.muted).size(12.5));
            ui.add_space(4.0);
        }

        if !outgoing.is_empty() {
            section(ui, pal, "Links");
            ui.horizontal_wrapped(|ui| {
                for (target, resolved) in &outgoing {
                    let label = RichText::new(target).color(if resolved.is_some() { pal.accent } else { pal.muted });
                    let r = ui.link(label).on_hover_text(if resolved.is_some() { "Open" } else { "Not created yet — click to create" });
                    if r.clicked() {
                        open_link = Some((target.clone(), None));
                    }
                }
            });
        }

        if !ins.link_suggestions.is_empty() {
            section(ui, pal, &format!("Suggested links · {}", ins.link_suggestions.len()));
            for (i, s) in ins.link_suggestions.iter().enumerate().take(8) {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(format!("“{}”", s.text)));
                    ui.label(RichText::new(format!("→ {}", s.target.stem())).color(pal.muted).size(12.0));
                    if ui.small_button("Link").clicked() {
                        link_one = Some(i);
                    }
                });
            }
            if ui.small_button("Link all").clicked() {
                out.run = Some(Command::LinkAllSuggestions);
            }
        }

        if !ins.unlinked.is_empty() {
            section(ui, pal, &format!("Unlinked mentions · {}", ins.unlinked.len()));
            for m in ins.unlinked.iter().take(8) {
                if ui.link(RichText::new(m.from.stem()).strong()).clicked() {
                    open_path = Some((m.from.clone(), Some(m.range.start)));
                }
                ui.label(RichText::new(&m.context).color(pal.muted).size(12.5));
            }
        }

        if !ins.related.is_empty() {
            section(ui, pal, "Related");
            for r in &ins.related {
                ui.horizontal_wrapped(|ui| {
                    if ui.link(&r.title).clicked() {
                        open_path = Some((r.path.clone(), None));
                    }
                    let why = if r.linked { format!("linked · {}", r.because.join(", ")) } else { r.because.join(", ") };
                    ui.label(RichText::new(why).color(pal.muted).size(11.5));
                });
            }
        }

        if !ins.tag_suggestions.is_empty() {
            section(ui, pal, "Suggested tags");
            ui.horizontal_wrapped(|ui| {
                for (t, _) in &ins.tag_suggestions {
                    if ui.button(RichText::new(format!("+ #{t}")).color(pal.tag).size(12.5)).clicked() {
                        add_tag = Some(t.clone());
                    }
                }
            });
        }

        if !ins.summary.is_empty() {
            section(ui, pal, "Key sentences");
            for s in &ins.summary {
                ui.label(RichText::new(format!("• {s}")).size(12.5));
            }
        }

        section(ui, pal, &format!("Assistant · {}", ws.assistant_name()));
        ui.horizontal_wrapped(|ui| {
            for task in
                [Assist::Summarize, Assist::FixGrammar, Assist::Rewrite { tone: "clearer and more concise".into() }, Assist::Continue]
            {
                if ui.add_enabled(!ws.assist_pending(), egui::Button::new(task.label()).small()).clicked() {
                    ws.start_assist(task);
                }
            }
        });
        if ws.assist_pending() {
            ui.add(egui::Spinner::new());
        }
        let mut accept = None;
        let mut dismiss = false;
        if let Some(r) = &ws.assist_result {
            egui::Frame::group(ui.style()).show(ui, |ui| match &r.output {
                Ok(t) => {
                    ui.label(RichText::new(t).size(12.5));
                    ui.horizontal(|ui| {
                        if ui.small_button("Insert below").clicked() {
                            accept = Some(false);
                        }
                        if !r.selection.is_empty() && ui.small_button("Replace selection").clicked() {
                            accept = Some(true);
                        }
                        if ui.small_button("Copy").clicked() {
                            ui.ctx().copy_text(t.clone());
                        }
                        if ui.small_button("Dismiss").clicked() {
                            dismiss = true;
                        }
                    });
                }
                Err(e) => {
                    ui.label(RichText::new(e).color(pal.warn).size(12.5));
                    ui.label(
                        RichText::new("Build with --features remote-llm and set ANTHROPIC_API_KEY for generative help.")
                            .color(pal.muted)
                            .size(11.5),
                    );
                    if ui.small_button("Dismiss").clicked() {
                        dismiss = true;
                    }
                }
            });
        }
        if let Some(replace) = accept {
            ws.accept_assist(replace);
        }
        if dismiss {
            ws.assist_result = None;
        }
        ui.add_space(16.0);
    });

    if let Some(b) = jump_byte {
        ws.jump_to_byte(b);
        out.jumped = true;
    }
    if let Some(i) = link_one {
        ws.apply_link_suggestion(&ins.link_suggestions[i]);
    }
    if let Some(t) = add_tag {
        let at_end = ws.active_doc().map_or(0, |d| d.buffer().len_chars());
        if let Some(d) = ws.active_doc_mut() {
            d.set_selection(quire_app::text::Selection::cursor(at_end));
        }
        let sep = if ws.active_doc().is_some_and(|d| d.text().ends_with('\n')) { "" } else { "\n" };
        ws.insert_text(&format!("{sep}#{t} "));
    }
    if let Some((t, h)) = open_link {
        out.jumped = ws.open_link(&t, h.as_deref()).is_some();
    }
    if let Some((p, at)) = open_path
        && ws.open_note(&p).is_some()
    {
        if let Some(b) = at {
            ws.jump_to_byte(b);
        }
        out.jumped = true;
    }
    out
}

// ---------------------------------------------------------------- status bar

pub(crate) fn status_bar(ui: &mut egui::Ui, ws: &Workspace, pal: &Palette) -> PanelOut {
    let mut out = PanelOut::default();
    ui.horizontal(|ui| {
        let small = |s: String| RichText::new(s).size(12.0).color(pal.muted);
        if let Some(doc) = ws.active_doc() {
            let (l, c) = doc.caret_line_col();
            if doc.large {
                let b = doc.buffer();
                ui.label(small(format!("{} lines · {} chars", b.len_lines(), b.len_chars())));
            } else {
                let st = doc.extracted().stats;
                ui.label(small(format!("{} words · {} chars · {} min read", st.words, st.chars, st.reading_minutes.max(1))));
            }
            ui.separator();
            ui.label(small(format!("Ln {l}, Col {c}")));
            if doc.large {
                ui.separator();
                let classic = ws.settings.editor == "classic";
                let (label, tip) = if classic {
                    ("plain mode", "The classic editor turns off styling for files this large")
                } else {
                    ("large file", "Outline, word count and preview are off for files this large; editing and styling stay fast")
                };
                ui.label(RichText::new(label).size(12.0).color(pal.warn)).on_hover_text(tip);
            }
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            {
                use quire_app::vault_sync::SyncStatus;
                let st = &ws.sync.status;
                if *st != SyncStatus::Off {
                    let (color, hover) = match st {
                        SyncStatus::Synced(at) => (pal.muted, format!("Up to date (checked {}s ago)", at.elapsed().as_secs())),
                        SyncStatus::Syncing => (pal.accent, "Syncing".to_string()),
                        SyncStatus::Offline(e) => (pal.warn, format!("Can't sync right now; will keep trying.\n{e}")),
                        SyncStatus::Waiting(e) => (pal.warn, e.clone()),
                        SyncStatus::Error(e) => (pal.danger, e.clone()),
                        SyncStatus::Off => (pal.muted, String::new()),
                    };
                    if ui
                        .add(egui::Label::new(RichText::new(st.label()).size(12.0).color(color)).sense(egui::Sense::click()))
                        .on_hover_text(hover)
                        .on_hover_cursor(egui::CursorIcon::PointingHand)
                        .clicked()
                    {
                        out.run = Some(Command::SyncSettings);
                    }
                    ui.separator();
                }
            }
            if let Some(r) = &ws.updates.available {
                let link = ui
                    .add(
                        egui::Label::new(RichText::new(format!("↑ Quire {} available", r.version)).size(12.0).color(pal.accent))
                            .sense(egui::Sense::click()),
                    )
                    .on_hover_text("Open the release page. Right-click to skip this version.")
                    .on_hover_cursor(egui::CursorIcon::PointingHand);
                if link.clicked() {
                    ui.ctx().open_url(egui::OpenUrl::new_tab(r.url.clone()));
                }
                link.context_menu(|ui| {
                    if ui.button("Skip this version").clicked() {
                        out.skip_update = true;
                        ui.close();
                    }
                });
                ui.separator();
            }
            let conflicts = ws.conflict_copy_count();
            if conflicts > 0 {
                let label = format!("⚠ {conflicts} sync conflict{}", if conflicts == 1 { "" } else { "s" });
                if ui
                    .add(egui::Label::new(RichText::new(label).size(12.0).color(pal.warn)).sense(egui::Sense::click()))
                    .on_hover_text("A sync tool saved a conflicting copy of a note. Click to merge it back.")
                    .on_hover_cursor(egui::CursorIcon::PointingHand)
                    .clicked()
                {
                    out.run = Some(Command::ResolveSyncConflicts);
                }
                ui.separator();
            }
            let s = ws.index().status();
            let n = ws.index().read().len();
            ui.label(small(if s.busy && s.total > 0 { format!("Indexing {}/{}", s.indexed, s.total) } else { format!("{n} notes") }));
            ui.separator();
            let sem = if ws.index().semantic_busy() {
                "Learning meanings…".to_string()
            } else {
                match ws.index().semantic() {
                    Some(si) => format!("≈ {} passages", si.len()),
                    None => "≈ —".into(),
                }
            };
            ui.label(small(sem)).on_hover_text("On-device semantic index used by search, related notes and Ask");
            ui.separator();
            if let Some(doc) = ws.active_doc() {
                let state = if doc.conflict {
                    RichText::new("Changed on disk").color(pal.warn)
                } else if doc.path.is_none() {
                    small("Scratchpad".into())
                } else if doc.is_dirty() {
                    small("Editing…".into())
                } else {
                    small("Saved".into())
                };
                ui.label(state.size(12.0));
            }
        });
    });
    out
}

// ---------------------------------------------------------------- preview

/// Obsidian's callout types as the preview's alert boxes.
fn callout_alerts() -> egui_commonmark::AlertBundle {
    egui_commonmark::AlertBundle::from_alerts(
        quire_app::markdown::callout::KINDS
            .iter()
            .map(|k| egui_commonmark::Alert {
                accent_color: egui::Color32::from_rgb(k.color[0], k.color[1], k.color[2]),
                icon: k.glyph,
                identifier: k.id.to_uppercase(),
                identifier_rendered: k.label.to_string(),
            })
            .collect(),
    )
}

pub(crate) fn preview(ui: &mut egui::Ui, ws: &mut Workspace, cache: &mut CommonMarkCache, pal: &Palette) -> PanelOut {
    let mut out = PanelOut::default();
    let Some(doc) = ws.active_doc() else {
        return out;
    };
    if doc.large {
        ui.label(RichText::new("Preview is off for very large files.").color(pal.muted));
        return out;
    }
    // Mermaid blocks become display math holding a marker; the math hook draws the diagram.
    let text = doc.text().to_string();
    let figures = quire_app::markdown::blocks::figures(&text);
    let mut diagrams: Vec<String> = Vec::new();
    let text = if figures.iter().any(|f| f.kind == quire_app::markdown::blocks::FigureKind::Diagram) {
        let lines: Vec<&str> = text.split('\n').collect();
        let mut out = Vec::with_capacity(lines.len());
        let mut i = 0;
        while i < lines.len() {
            if let Some(f) = figures.iter().find(|f| f.first_line == i && f.kind == quire_app::markdown::blocks::FigureKind::Diagram) {
                // Keep the block's indent so a diagram inside a list stays in it.
                let indent: String = lines[i].chars().take_while(|c| c.is_whitespace()).collect();
                out.push(format!("{indent}$$quire-diagram:{}$$", diagrams.len()));
                diagrams.push(f.source.clone());
                i = f.last_line + 1;
            } else {
                out.push(lines[i].to_string());
                i += 1;
            }
        }
        out.join("\n")
    } else {
        text
    };
    let md = for_preview(&text, &|t| ws.attachment_path(t).map(|p| format!("file://{}", p.display())));
    let urls: Vec<String> =
        md.match_indices("(quire://note/").filter_map(|(i, _)| md[i + 1..].find(')').map(|e| md[i + 1..i + 1 + e].to_string())).collect();
    for u in &urls {
        cache.add_link_hook(u.clone());
    }
    egui::ScrollArea::vertical().id_salt("preview").auto_shrink([false, false]).show(ui, |ui| {
        egui::Frame::NONE.inner_margin(egui::Margin::symmetric(18, 22)).show(ui, |ui| {
            let (text_color, dark, size) = (pal.text, pal.dark, ui.style().text_styles[&egui::TextStyle::Body].size);
            let math = move |ui: &mut egui::Ui, src: &str, inline: bool| {
                let job =
                    match src.trim().strip_prefix("quire-diagram:").and_then(|n| n.parse::<usize>().ok()).and_then(|n| diagrams.get(n)) {
                        Some(source) => crate::typeset_ui::Job::Diagram { source: source.clone(), dark },
                        None => crate::typeset_ui::Job::Math {
                            latex: src.trim().to_string(),
                            display: !inline,
                            size: size.round() as u32,
                            color: crate::typeset_ui::rgba(text_color),
                        },
                    };
                match crate::typeset_ui::request(ui.ctx(), &job) {
                    crate::typeset_ui::Out::Ready(p) => {
                        let scale = (ui.available_width() / p.size.x).min(1.0);
                        ui.image((p.texture.id(), p.size * scale));
                    }
                    crate::typeset_ui::Out::Pending => {
                        ui.label(RichText::new("…").weak());
                    }
                    crate::typeset_ui::Out::Failed(e) => {
                        ui.label(RichText::new(if inline { src.to_string() } else { format!("{src}\n({e})") }).monospace().weak());
                    }
                }
            };
            CommonMarkViewer::new().alerts(callout_alerts()).render_math_fn(Some(&math)).show(ui, cache, &md);
        });
    });
    let mut target = None;
    for u in &urls {
        if cache.get_link_hook(u) == Some(true) {
            target = parse_note_url(u);
        }
        cache.remove_link_hook(u);
    }
    if let Some((t, h)) = target {
        out.jumped = ws.open_link(&t, h.as_deref()).is_some();
    }
    out
}

// ---------------------------------------------------------------- welcome

pub(crate) enum WelcomeAction {
    None,
    Browse,
    Sample,
}

pub(crate) fn welcome(ui: &mut egui::Ui, pal: &Palette) -> WelcomeAction {
    let mut act = WelcomeAction::None;
    ui.vertical_centered(|ui| {
        ui.add_space((ui.available_height() * 0.22).max(24.0));
        ui.label(RichText::new("Quire").size(44.0).strong().color(pal.strong));
        ui.label(RichText::new("A fast, local-first Markdown notepad.").size(17.0).color(pal.muted));
        ui.add_space(28.0);
        if ui.add(egui::Button::new(RichText::new("Open a notes folder…").size(16.0)).min_size([260.0, 40.0].into())).clicked() {
            act = WelcomeAction::Browse;
        }
        ui.add_space(8.0);
        if ui.add(egui::Button::new(RichText::new("Explore the sample vault").size(16.0)).min_size([260.0, 40.0].into())).clicked() {
            act = WelcomeAction::Sample;
        }
        ui.add_space(24.0);
        ui.label(
            RichText::new("Notes stay plain .md files on your disk. Works with folders from Obsidian, iA Writer, Logseq…")
                .color(pal.muted)
                .size(13.0),
        );
    });
    act
}

// ---------------------------------------------------------------- find bar

fn toggle(ui: &mut egui::Ui, on: &mut bool, label: &str, tip: &str, pal: &Palette) -> bool {
    let text = RichText::new(label).monospace().color(if *on { pal.accent } else { pal.muted });
    let r = ui.selectable_label(*on, text).on_hover_text(tip);
    if r.clicked() {
        *on = !*on;
    }
    r.clicked()
}

/// In-note find & replace. Enter = next, Shift+Enter = previous, Esc closes.
pub(crate) fn find_bar(ui: &mut egui::Ui, ws: &mut Workspace, focus: &mut bool, pal: &Palette) -> PanelOut {
    let mut out = PanelOut::default();
    let mut step: Option<bool> = None;
    ui.horizontal_wrapped(|ui| {
        let q = ui.add(
            egui::TextEdit::singleline(&mut ws.find.query).hint_text("Find").desired_width(220.0).id(egui::Id::unique("quire-find-query")),
        );
        if std::mem::take(focus) {
            q.request_focus();
        }
        if q.changed() {
            // Incremental search: re-select from where the current match starts.
            if let Some(d) = ws.active_doc_mut() {
                let s = d.selection().start();
                d.set_selection(quire_app::text::Selection::cursor(s));
            }
            step = Some(false);
        }
        if q.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
            step = Some(ui.input(|i| i.modifiers.shift));
            q.request_focus();
        }
        let mut o = ws.find.opts;
        toggle(ui, &mut o.case_sensitive, "Aa", "Match case", pal);
        toggle(ui, &mut o.whole_word, "W", "Whole word", pal);
        toggle(ui, &mut o.regex, ".*", "Regular expression ($1 in replacement)", pal);
        ws.find.opts = o;
        let (m, err) = ws.find_matches();
        let label = match (&err, m.len(), ws.find_current()) {
            (Some(e), _, _) => RichText::new(e.lines().last().unwrap_or("invalid pattern")).color(pal.danger),
            (None, 0, _) if !ws.find.query.is_empty() => RichText::new("No results").color(pal.warn),
            (None, 0, _) => RichText::new(""),
            (None, n, Some(i)) => RichText::new(format!("{} of {n}", i + 1)).color(pal.muted),
            (None, n, None) => RichText::new(format!("{n} found")).color(pal.muted),
        };
        ui.label(label.size(12.5));
        if ui.small_button("↑").on_hover_text("Previous (Shift+Enter, Shift+F3)").clicked() {
            step = Some(true);
        }
        if ui.small_button("↓").on_hover_text("Next (Enter, F3)").clicked() {
            step = Some(false);
        }
        ui.separator();
        if ws.find.replace_mode {
            ui.add(egui::TextEdit::singleline(&mut ws.find.replacement).hint_text("Replace with").desired_width(180.0));
            if ui.small_button("Replace").clicked() {
                ws.replace_current();
                out.jumped = true;
            }
            if ui.small_button("All").on_hover_text("Replace all in this note (one undo step)").clicked() {
                ws.replace_all_in_note();
            }
        } else if ui.small_button("Replace…").on_hover_text("Ctrl+H").clicked() {
            ws.find.replace_mode = true;
        }
        if ui.small_button("In all notes…").on_hover_text("Ctrl+Shift+R").clicked() {
            out.run = Some(Command::VaultReplace);
        }
        if ui.small_button("×").on_hover_text("Close (Esc)").clicked() {
            ws.find.open = false;
            out.jumped = true;
        }
    });
    if let Some(back) = step
        && ws.find_step(back)
    {
        // Scroll to the match but keep typing in the find field.
        out.reveal_only = true;
    }
    out
}
