// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Rename, merge or delete tags and properties across the vault, with a
//! preview of every note that changes (design §E.1).
//!
//! Owned by WS-E. Opened by "Rename tag…" and "Rename property…"; shown
//! as a dialog (`registry::UiTarget::Overlay`).

use std::collections::HashSet;

use egui::{Key, Modifiers, RichText, Ui};
use quire_app::Workspace;
use quire_app::meta_refactor::MetaOp;
use quire_app::vault::NotePath;

use crate::overlays::{OverlayResult, OverlayView};
use crate::theme::Palette;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Act {
    #[default]
    Rename,
    Merge,
    Value,
    Delete,
}

/// (index version, properties?, names with counts).
type Names = (u64, bool, Vec<(String, usize)>);

/// The dialog's state, kept in `FeatureUi`.
#[derive(Default)]
pub(crate) struct RefactorUi {
    started: bool,
    props: bool,
    act: Act,
    filter: String,
    /// The tags or keys chosen (one, or several to merge).
    picked: Vec<String>,
    /// For "change a value": the value to change.
    value: String,
    target: String,
    skip: HashSet<NotePath>,
    open: HashSet<NotePath>,
    /// The batch just applied (for Undo).
    applied: Option<String>,
    focus_target: bool,
    /// Tags or keys with counts, for the index version they came from.
    names: Option<Names>,
    /// Work the preview out as it opens (`layout_check` samples).
    preview_on_open: bool,
}

impl RefactorUi {
    fn op(&self) -> Option<MetaOp> {
        let first = self.picked.first()?.clone();
        let t = self.target.trim().trim_start_matches('#').to_string();
        Some(match (self.props, self.act) {
            (false, Act::Rename) => MetaOp::RenameTag { from: first, to: t },
            (false, Act::Merge) => MetaOp::MergeTags { from: self.picked.clone(), into: t },
            (false, Act::Delete | Act::Value) => MetaOp::DeleteTag { tag: first },
            (true, Act::Rename) => MetaOp::RenameKey { from: first, to: self.target.trim().to_string() },
            (true, Act::Merge) => MetaOp::MergeKeys { from: self.picked.clone(), into: self.target.trim().to_string() },
            (true, Act::Value) => MetaOp::RenameValue { key: first, from: self.value.clone(), to: self.target.trim().to_string() },
            (true, Act::Delete) => MetaOp::DeleteKey { key: first },
        })
    }

    fn ready(&self) -> bool {
        !self.picked.is_empty()
            && (self.act == Act::Delete || !self.target.trim().is_empty())
            && (self.act != Act::Value || !self.value.is_empty())
            && (self.act != Act::Merge || self.picked.len() > 1 || !self.target.trim().is_empty())
    }

    /// Changing the question drops the preview.
    fn reset_preview(&mut self, ws: &mut Workspace) {
        ws.features.meta.preview = None;
        self.skip.clear();
        self.open.clear();
    }

    /// Draw as a modal; true when it should close.
    pub(crate) fn show_modal(&mut self, ctx: &egui::Context, ws: &mut Workspace, pal: &Palette) -> bool {
        if self.preview_on_open {
            self.preview_on_open = false;
            if let Some(op) = self.op() {
                ws.meta_preview(op);
                ws.meta_preview_wait();
            }
        }
        if !self.started {
            self.started = true;
            if let Some((tag, name)) = ws.features.tasks.refactor_start.take() {
                self.props = !tag;
                if !name.is_empty() {
                    self.picked = vec![name];
                    self.focus_target = true;
                }
            }
        }
        let mut close = ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape));
        egui::Modal::new(egui::Id::unique("quire-refactor")).show(ctx, |ui| {
            let w = 660.0_f32.min(ctx.content_rect().width() - 48.0);
            ui.set_width(w);
            ui.set_max_width(w);
            close |= self.body(ui, ws, pal, ctx.content_rect().height());
        });
        close
    }

    fn body(&mut self, ui: &mut Ui, ws: &mut Workspace, pal: &Palette, screen_h: f32) -> bool {
        let mut close = false;
        ui.horizontal(|ui| {
            ui.heading(if self.props { "Rename properties" } else { "Rename tags" });
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                close |= ui.button("Close").clicked();
            });
        });
        ui.label(
            RichText::new("Changes every note that uses it, in one step you can undo. You see each change first.")
                .size(12.0)
                .color(pal.muted),
        );
        ui.add_space(6.0);
        let mut changed = false;
        ui.horizontal(|ui| {
            changed |= ui.selectable_value(&mut self.props, false, "Tags").changed();
            changed |= ui.selectable_value(&mut self.props, true, "Properties").changed();
            ui.separator();
            changed |= ui.selectable_value(&mut self.act, Act::Rename, "Rename").changed();
            changed |= ui.selectable_value(&mut self.act, Act::Merge, "Merge").changed();
            if self.props {
                changed |= ui.selectable_value(&mut self.act, Act::Value, "Change a value").changed();
            }
            changed |= ui.selectable_value(&mut self.act, Act::Delete, "Delete").changed();
        });
        if changed {
            if self.names.as_ref().is_some_and(|n| n.1 != self.props) {
                self.picked.clear();
            }
            if !self.props && self.act == Act::Value {
                self.act = Act::Rename;
            }
            if self.act != Act::Merge {
                self.picked.truncate(1);
            }
            self.reset_preview(ws);
        }
        ui.add_space(4.0);
        // The names to pick from.
        let version = ws.index().version();
        if self.names.as_ref().is_none_or(|(v, p, _)| *v != version || *p != self.props) {
            let list = if self.props { ws.all_property_keys() } else { ws.all_tags() };
            self.names = Some((version, self.props, list));
        }
        let what = if self.props { "property" } else { "tag" };
        let multi = self.act == Act::Merge;
        ui.horizontal(|ui| {
            ui.label(RichText::new(if multi { format!("{what}s to merge") } else { format!("The {what}") }).color(pal.text));
            ui.add(egui::TextEdit::singleline(&mut self.filter).hint_text("Filter…").desired_width(180.0));
            if !self.picked.is_empty() {
                let shown: Vec<String> = self.picked.iter().map(|p| if self.props { p.clone() } else { format!("#{p}") }).collect();
                ui.label(RichText::new(shown.join(", ")).color(pal.accent).strong());
            }
        });
        let q = self.filter.trim().trim_start_matches('#').to_lowercase();
        let mut pick: Option<String> = None;
        // Heights scale with the window so the dialog always fits.
        let spare = (screen_h - 330.0).max(0.0);
        let names_h = (spare * 0.35).clamp(48.0, 130.0);
        egui::ScrollArea::vertical()
            .id_salt("quire-refactor-names")
            .max_height(names_h)
            .min_scrolled_height(names_h)
            .auto_shrink([false, false])
            .show(ui, |ui| {
                let names = self.names.as_ref().map(|n| n.2.as_slice()).unwrap_or_default();
                ui.horizontal_wrapped(|ui| {
                    let mut shown = 0;
                    for (name, n) in names.iter().filter(|(name, _)| q.is_empty() || name.to_lowercase().contains(&q)).take(400) {
                        shown += 1;
                        let on = self.picked.iter().any(|p| p.eq_ignore_ascii_case(name));
                        let label = chip(ui, pal, &if self.props { name.clone() } else { format!("#{name}") }, *n);
                        if ui.selectable_label(on, label).clicked() {
                            pick = Some(name.clone());
                        }
                    }
                    if shown == 0 {
                        ui.label(
                            RichText::new(if names.is_empty() {
                                format!("No {what}s in this vault yet.")
                            } else {
                                "Nothing matches.".into()
                            })
                            .color(pal.muted),
                        );
                    }
                });
            });
        if let Some(name) = pick {
            if multi {
                if let Some(i) = self.picked.iter().position(|p| p.eq_ignore_ascii_case(&name)) {
                    self.picked.remove(i);
                } else {
                    self.picked.push(name);
                }
            } else {
                self.picked = vec![name];
            }
            if self.act != Act::Delete {
                self.focus_target = true;
            }
            self.reset_preview(ws);
        }
        // The new name or value.
        if self.act == Act::Value
            && let Some(key) = self.picked.first().cloned()
        {
            ui.horizontal(|ui| {
                ui.label("Value");

                egui::ComboBox::from_id_salt("quire-refactor-value")
                    .width(200.0)
                    .selected_text(if self.value.is_empty() { "Pick a value".to_string() } else { self.value.clone() })
                    .show_ui(ui, |ui| {
                        // Worked out only while the menu is open.
                        let values = ws.property_values(&key);
                        for (v, n) in values.iter().take(200) {
                            if ui.selectable_label(self.value == *v, format!("{v}  ({n})")).clicked() {
                                self.value.clone_from(v);
                                self.focus_target = true;
                                ws.features.meta.preview = None;
                            }
                        }
                    });
            });
        }
        let mut go = false;
        if self.act != Act::Delete {
            ui.horizontal(|ui| {
                ui.label(match self.act {
                    Act::Merge => "Into",
                    Act::Value => "New value",
                    _ => "New name",
                });
                let hint = if self.props { "e.g. status or author.name" } else { "e.g. project/active" };
                let r = ui.add(egui::TextEdit::singleline(&mut self.target).hint_text(hint).desired_width(260.0));
                if self.focus_target {
                    r.request_focus();
                    self.focus_target = false;
                }
                if r.changed() {
                    ws.features.meta.preview = None;
                }
                go |= r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
            });
        } else if !self.props && !self.picked.is_empty() {
            ui.label(RichText::new("Nested tags (#tag/child) are kept.").size(12.0).color(pal.muted));
        }
        let op = self.op();
        let problem = op.as_ref().and_then(MetaOp::problem).filter(|_| self.ready());
        ui.horizontal(|ui| {
            go |= ui
                .add_enabled(self.ready() && problem.is_none() && !ws.features.meta.busy(), egui::Button::new("Preview changes"))
                .clicked();
            if let Some(p) = &problem {
                ui.label(RichText::new(p).size(12.0).color(pal.warn));
            } else if let Some(op) = &op
                && self.ready()
            {
                ui.label(RichText::new(op.title()).size(12.0).color(pal.muted));
            }
        });
        if go
            && self.ready()
            && problem.is_none()
            && let Some(op) = op
        {
            self.applied = None;
            self.skip.clear();
            ws.meta_preview(op);
        }
        ui.separator();
        // The preview.
        // The preview and the undo list share a fixed height, so the dialog doesn't jump.
        let batches: Vec<(String, String, usize, i64)> =
            ws.features.meta.batches.iter().rev().take(3).map(|b| (b.id.clone(), b.title.clone(), b.files.len(), b.at_ms)).collect();
        let undo_h = if batches.is_empty() { 0.0 } else { 30.0 + 26.0 * batches.len() as f32 };
        let full_h = (spare * 0.62).clamp(72.0, 320.0);
        let list_h = (full_h - undo_h).max(60.0);
        let top = ui.cursor().top();
        let preview = ws.features.meta.preview.clone();
        match preview.as_ref().map(|p| p.edits.clone()) {
            Some(None) => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(RichText::new("Finding every note it changes…").color(pal.muted));
                });
            }
            Some(Some(edits)) => {
                let n = edits.len();
                let ticked = edits.iter().filter(|e| !self.skip.contains(&e.path)).count();
                let changes: usize = edits.iter().filter(|e| !self.skip.contains(&e.path)).map(|e| e.changes).sum();
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new(format!(
                            "{n} note{} would change · {changes} place{}",
                            if n == 1 { "" } else { "s" },
                            if changes == 1 { "" } else { "s" }
                        ))
                        .strong()
                        .color(pal.text),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.small_button("None").clicked() {
                            self.skip = edits.iter().map(|e| e.path.clone()).collect();
                        }
                        if ui.small_button("All").clicked() {
                            self.skip.clear();
                        }
                    });
                });
                egui::ScrollArea::vertical()
                    .id_salt("quire-refactor-preview")
                    .max_height(list_h)
                    .min_scrolled_height(list_h)
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        if edits.is_empty() {
                            ui.label(RichText::new("No note uses it.").color(pal.muted));
                        }
                        for e in edits.iter().take(1000) {
                            ui.horizontal(|ui| {
                                let mut on = !self.skip.contains(&e.path);
                                if ui.checkbox(&mut on, "").changed() {
                                    if on {
                                        self.skip.remove(&e.path);
                                    } else {
                                        self.skip.insert(e.path.clone());
                                    }
                                }
                                let open = self.open.contains(&e.path);
                                crate::agenda_ui::disclosure(ui, open, pal.faint);
                                let label = RichText::new(e.path.as_str().trim_end_matches(".md")).color(pal.text);
                                if ui.add(egui::Label::new(label).sense(egui::Sense::click())).on_hover_text("Show the changes").clicked() {
                                    if open {
                                        self.open.remove(&e.path);
                                    } else {
                                        self.open.insert(e.path.clone());
                                    }
                                }
                                ui.label(RichText::new(format!("{}", e.changes)).size(11.0).color(pal.faint));
                            });
                            if self.open.contains(&e.path) {
                                diff(ui, pal, e);
                            }
                        }
                        if edits.len() > 1000 {
                            ui.label(RichText::new(format!("…and {} more", edits.len() - 1000)).color(pal.muted));
                        }
                    });
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    if preview.as_ref().is_some_and(|p| p.ms > 300) {
                        ui.label(
                            RichText::new(format!("worked out in {} ms", preview.as_ref().map_or(0, |p| p.ms))).size(11.0).color(pal.faint),
                        );
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let label = format!("Change {ticked} note{}", if ticked == 1 { "" } else { "s" });
                        if ui.add_enabled(ticked > 0, egui::Button::new(RichText::new(label).strong())).clicked()
                            && let Some(b) = ws.meta_apply(&self.skip)
                        {
                            self.applied = (!b.files.is_empty()).then_some(b.id);
                            self.skip.clear();
                            self.open.clear();
                            self.picked.clear();
                            self.target.clear();
                            self.value.clear();
                        }
                    });
                });
            }
            None => {
                let hint = match self.act {
                    Act::Delete => format!("Pick the {what} to delete, then preview. Nothing changes until you confirm."),
                    Act::Merge => {
                        format!("Pick the {what}s to merge and the name to keep, then preview. Nothing changes until you confirm.")
                    }
                    _ => format!("Pick a {what} and type the new name, then preview. Nothing changes until you confirm."),
                };
                ui.label(RichText::new(hint).size(12.0).color(pal.faint));
            }
        }
        let used = ui.cursor().top() - top;
        let want = (full_h + 64.0) - undo_h;
        if used < want {
            ui.add_space(want - used);
        }
        // Undo.
        if !batches.is_empty() {
            ui.separator();
            ui.label(RichText::new("DONE THIS SESSION").size(11.0).color(pal.muted).strong());
            let mut undo = None;
            for (id, title, n, at) in &batches {
                ui.horizontal(|ui| {
                    let just = self.applied.as_deref() == Some(id.as_str());
                    ui.label(
                        RichText::new(format!("{title} · {n} note{} · {}", if *n == 1 { "" } else { "s" }, quire_app::chrono_fmt(*at)))
                            .size(12.0)
                            .color(if just { pal.text } else { pal.muted }),
                    );
                    if ui.small_button("Undo").on_hover_text("Put those notes back; later edits are kept").clicked() {
                        undo = Some(id.clone());
                    }
                });
            }
            if let Some(id) = undo {
                ws.meta_undo(&id);
                self.applied = None;
            }
        }
        close
    }
}

/// A name with its count, the count fainter.
fn chip(ui: &Ui, pal: &Palette, name: &str, n: usize) -> egui::text::LayoutJob {
    let font = egui::TextStyle::Body.resolve(ui.style());
    let mut job = egui::text::LayoutJob::default();
    job.append(name, 0.0, egui::TextFormat::simple(font.clone(), pal.text));
    job.append(&n.to_string(), 5.0, egui::TextFormat::simple(egui::FontId::new(font.size * 0.85, font.family), pal.faint));
    job
}

/// A note's changed lines.
fn diff(ui: &mut Ui, pal: &Palette, e: &quire_app::meta_refactor::FileEdit) {
    let frame = egui::Frame::NONE.fill(pal.code_bg).corner_radius(4.0).inner_margin(egui::Margin::symmetric(8, 4));
    frame.show(ui, |ui| {
        ui.set_width(ui.available_width());
        for (gone, new) in e.diff(8) {
            for l in gone {
                ui.add(egui::Label::new(RichText::new(format!("− {l}")).monospace().size(11.5).color(pal.danger)).truncate());
            }
            for l in new {
                ui.add(egui::Label::new(RichText::new(format!("+ {l}")).monospace().size(11.5).color(pal.accent)).truncate());
            }
        }
    });
}

/// The dialog (`Command::RenameTag`, `Command::RenameProperty`).
pub(crate) struct RefactorOverlay(pub RefactorUi);

impl OverlayView for RefactorOverlay {
    fn show(&mut self, ctx: &egui::Context, ws: &mut Workspace, pal: &Palette) -> OverlayResult {
        OverlayResult { close: self.0.show_modal(ctx, ws, pal), ..Default::default() }
    }

    fn closed(&mut self, ws: &mut Workspace) {
        ws.features.meta.preview = None;
    }
}

impl RefactorOverlay {
    /// For `registry::UiTarget::Overlay`: it starts on what the command
    /// found at the caret (`features.tasks.refactor_start`).
    pub(crate) fn make(_ws: &mut Workspace) -> Box<dyn OverlayView> {
        Box::new(Self(RefactorUi::default()))
    }

    /// The dialog on a tag or a property value, with or without its preview.
    #[cfg(test)]
    pub(crate) fn sample(props: bool, preview: bool) -> Self {
        let mut st = RefactorUi { started: true, props, preview_on_open: preview, ..RefactorUi::default() };
        if props {
            st.picked = vec!["status".into()];
            st.act = Act::Value;
            st.value = "doing".into();
            st.target = "active".into();
        } else {
            st.picked = vec!["project".into()];
            st.target = "work".into();
        }
        Self(st)
    }
}
