// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! 0.12 typed links in the desktop.
//!
//! The breadcrumbs trail above a note, the typed groups in Insights, the
//! Hierarchy sidebar tab and the "Add typed link…" dialog.
//!
//! Everything drawn here comes from `Workspace::note_relations` (cached per
//! index version and note) or the tree rows cached in [`HierarchyUi`]
//! (per index version and expanded set), so a frame costs the rows shown.

use std::collections::HashSet;
use std::rc::Rc;

use egui::{Key, RichText, Sense, TextEdit};
use quire_app::Workspace;
use quire_app::links_state::{self, NoteRelations, RelGroup, TreeRow};
use quire_app::markdown::fields::FieldStyle;
use quire_app::vault::NotePath;

use crate::overlays::{OverlayResult, OverlayView};
use crate::panels::PanelOut;
use crate::registry::FeatureUi;
use crate::theme::Palette;

// ------------------------------------------------------------------ trail

/// The trail shows when the vault turns it on and the note has a parent.
pub(crate) fn trail_available(ws: &Workspace) -> bool {
    ws.active_doc().is_some_and(|d| d.path.is_some()) && ws.note_relations().show_trail()
}

/// "Maths › Calculus › Limits", one line per path (at most four).
pub(crate) fn trail(ui: &mut egui::Ui, ws: &mut Workspace, _st: &mut FeatureUi, pal: &Palette) -> PanelOut {
    let mut out = PanelOut::default();
    let rel = ws.note_relations();
    let mut open = None;
    for (n, t) in rel.trails.iter().enumerate() {
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            for (i, (p, title)) in t.iter().enumerate() {
                let last = i + 1 == t.len();
                if last {
                    ui.label(RichText::new(title).size(12.5).color(pal.strong));
                } else {
                    let r = ui.add(egui::Label::new(RichText::new(title).size(12.5).color(pal.link)).sense(Sense::click()));
                    if r.on_hover_text(p.as_str()).clicked() {
                        open = Some(p.clone());
                    }
                    ui.label(RichText::new("›").size(12.5).color(pal.faint));
                }
            }
            if n == 0 && rel.trails.len() > 1 {
                ui.label(RichText::new(format!("· {} paths", rel.trails.len())).size(11.5).color(pal.faint));
            }
        });
    }
    if let Some(p) = open
        && ws.open_note(&p).is_some()
    {
        out.jumped = true;
    }
    out
}

// ------------------------------------------------------------------ insights

/// Arrow and tooltip for one related note.
fn item_hint(i: &links_state::RelItem) -> (&'static str, String) {
    if i.incoming {
        ("←", format!("{} links here as {}", i.title, i.field))
    } else if i.implied {
        ("", format!("Implied: {} links to this note", i.title))
    } else {
        ("", format!("{}:: {}", i.field, i.path.stem()))
    }
}

/// The typed groups for the Insights panel: "Parents", "Children",
/// "Supports"… Returns a note to open (and where in it), if one was clicked.
pub(crate) fn insights_groups(ui: &mut egui::Ui, rel: &NoteRelations, pal: &Palette) -> Option<NotePath> {
    let mut open = None;
    for RelGroup { label, items, .. } in &rel.groups {
        ui.add_space(6.0);
        ui.label(RichText::new(format!("{label} · {}", items.len())).size(11.5).color(pal.muted).strong());
        ui.horizontal_wrapped(|ui| {
            for i in items {
                let (arrow, tip) = item_hint(i);
                let text = if arrow.is_empty() { i.title.clone() } else { format!("{arrow} {}", i.title) };
                let color = if i.implied { pal.muted } else { pal.link };
                let r = ui.add(egui::Label::new(RichText::new(text).color(color)).sense(Sense::click()));
                if r.on_hover_text(tip).clicked() {
                    open = Some(i.path.clone());
                }
            }
        });
    }
    open
}

// ------------------------------------------------------------------ hierarchy tab

/// (index version, generation, around note).
type RowsKey = (u64, u64, Option<NotePath>);
/// (step, query, index version).
type ItemsKey = (u8, String, u64);
/// The dialog's rows: (value, label detail).
type Items = Rc<Vec<(String, String)>>;

/// The Hierarchy sidebar tab's state.
#[derive(Default)]
pub(crate) struct HierarchyUi {
    open: HashSet<NotePath>,
    /// Bumped when `open` changes.
    generation: u64,
    /// Rows for (index version, generation, around note).
    rows: Option<(RowsKey, Rc<Vec<TreeRow>>)>,
    /// Show only the tree holding the open note.
    this_note: bool,
}

pub(crate) fn hierarchy_available(ws: &Workspace) -> bool {
    ws.vault().is_some()
}

impl HierarchyUi {
    fn rows(&mut self, ws: &Workspace) -> Rc<Vec<TreeRow>> {
        let focus = if self.this_note { ws.active_doc().and_then(|d| d.path.clone()) } else { None };
        let key = (ws.index().version(), self.generation, focus.clone());
        if let Some((k, r)) = &self.rows
            && *k == key
        {
            return Rc::clone(r);
        }
        // Never wait for the index worker on a frame: show the rows we have.
        let Some(ix) = ws.index().try_read() else {
            return self.rows.as_ref().map(|(_, r)| Rc::clone(r)).unwrap_or_default();
        };
        let roots: Vec<NotePath> = match &focus {
            // The top of the open note's first trail (or the note itself).
            Some(p) => {
                let rel = ix.relations();
                let top = rel.trails(p, 1, 64).first().and_then(|t| t.first().cloned()).unwrap_or_else(|| p.clone());
                // Open the way down to the note, once.
                if let Some(t) = rel.trails(p, 1, 64).first() {
                    for q in &t[..t.len() - 1] {
                        self.open.insert(q.clone());
                    }
                }
                vec![top]
            }
            None => links_state::tree_roots(&ix).into_iter().map(|(p, _)| p).collect(),
        };
        let rows = Rc::new(links_state::tree_rows(&ix, &roots, &self.open));
        drop(ix);
        self.rows = Some((key, Rc::clone(&rows)));
        rows
    }
}

pub(crate) fn hierarchy(ui: &mut egui::Ui, ws: &mut Workspace, st: &mut FeatureUi, pal: &Palette) -> PanelOut {
    let mut out = PanelOut::default();
    let st = &mut st.hierarchy;
    ui.add_space(6.0);
    ui.horizontal(|ui| {
        let has_note = ws.active_doc().and_then(|d| d.path.as_ref()).is_some();
        if ui.selectable_label(!st.this_note, "All").clicked() {
            st.this_note = false;
        }
        ui.add_enabled_ui(has_note, |ui| {
            if ui.selectable_label(st.this_note, "This note").on_hover_text("The hierarchy the open note is in").clicked() {
                st.this_note = true;
                st.generation += 1;
            }
        });
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.small_button("Collapse").on_hover_text("Collapse every branch").clicked() {
                st.open.clear();
                st.generation += 1;
            }
        });
    });
    ui.add_space(4.0);
    let rows = st.rows(ws);
    if rows.is_empty() {
        ui.label(
            RichText::new("No hierarchy yet. Give a note a parent with up:: [[Note]] or a property up: \"[[Note]]\" (Add typed link… does it for you).")
                .color(pal.muted)
                .size(12.5),
        );
        return out;
    }
    let active = ws.active_doc().and_then(|d| d.path.clone());
    let row_h = ui.text_style_height(&egui::TextStyle::Body) + 6.0;
    let mut toggle = None;
    let mut open = None;
    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        let (rect, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), row_h * rows.len() as f32), Sense::hover());
        // Only the rows in view are drawn.
        let clip = ui.clip_rect();
        let first = ((clip.top() - rect.top()) / row_h).floor().max(0.0) as usize;
        let last = (((clip.bottom() - rect.top()) / row_h).ceil().max(0.0) as usize).min(rows.len());
        for i in first.min(last)..last {
            let row = &rows[i];
            let top = rect.top() + row_h * i as f32;
            let indent = rect.left() + 14.0 * row.depth as f32;
            let arrow = egui::Rect::from_min_size(egui::pos2(indent, top), egui::vec2(16.0, row_h));
            if row.has_children {
                let expanded = st.open.contains(&row.path);
                let r = ui.interact(arrow, egui::Id::unique(("quire-tree-arrow", i)), Sense::click());
                let color = if r.hovered() { pal.strong } else { pal.muted };
                // A small triangle (the UI font has no ▸): right when closed, down when open.
                let c = arrow.center();
                let pts = if expanded {
                    vec![c + egui::vec2(-4.0, -2.0), c + egui::vec2(4.0, -2.0), c + egui::vec2(0.0, 3.0)]
                } else {
                    vec![c + egui::vec2(-2.0, -4.0), c + egui::vec2(3.0, 0.0), c + egui::vec2(-2.0, 4.0)]
                };
                ui.painter().add(egui::Shape::convex_polygon(pts, color, egui::Stroke::NONE));
                if r.on_hover_text(if expanded { "Collapse" } else { "Expand" }).clicked() {
                    toggle = Some(row.path.clone());
                }
            }
            let text_rect = egui::Rect::from_min_max(egui::pos2(arrow.right() + 2.0, top), egui::pos2(rect.right(), top + row_h));
            let r = ui.interact(text_rect, egui::Id::unique(("quire-tree-row", i)), Sense::click());
            let here = active.as_ref() == Some(&row.path);
            if here || r.hovered() {
                ui.painter().rect_filled(text_rect, 4.0, if here { pal.active_bg } else { pal.hover });
            }
            let label = if row.repeat { format!("{} (again)", row.title) } else { row.title.clone() };
            let color = if here { pal.strong } else { pal.text };
            let g = egui::WidgetText::from(RichText::new(label).color(color)).into_galley(
                ui,
                Some(egui::TextWrapMode::Truncate),
                text_rect.width() - 4.0,
                egui::TextStyle::Body,
            );
            ui.painter().galley(egui::pos2(text_rect.left() + 2.0, text_rect.center().y - g.size().y / 2.0), g, color);
            let tip = if row.repeat { "Already above in this branch (a cycle)".to_string() } else { row.path.to_string() };
            if r.on_hover_text(tip).clicked() {
                open = Some(row.path.clone());
            }
        }
    });
    if let Some(p) = toggle {
        if !st.open.remove(&p) {
            st.open.insert(p);
        }
        st.generation += 1;
    }
    if let Some(p) = open
        && ws.open_note(&p).is_some()
    {
        out.jumped = true;
    }
    out
}

// ------------------------------------------------------------------ add typed link

/// "Add typed link…": choose a type, then a note.
pub(crate) struct AddLinkDialog {
    field: String,
    query: String,
    pick: usize,
    /// 0: choosing the type; 1: choosing the note.
    step: u8,
    focused: bool,
    names: Vec<String>,
    style: Option<FieldStyle>,
    /// The list shown, for (step, query, index version): typing, not each frame, searches.
    items: Option<(ItemsKey, Items)>,
}

impl AddLinkDialog {
    pub(crate) fn make(ws: &mut Workspace) -> Box<dyn OverlayView> {
        let names = links_state::field_names(&ws.index().read());
        Box::new(Self {
            field: String::new(),
            query: String::new(),
            pick: 0,
            step: 0,
            focused: false,
            names,
            style: links_state::style_of(ws),
            items: None,
        })
    }

    #[cfg(test)]
    pub(crate) fn sample(step: u8) -> Self {
        let names = ["up", "down", "same", "next", "prev", "supports", "contradicts", "source"].map(String::from).to_vec();
        Self {
            field: if step == 1 { "supports".into() } else { String::new() },
            query: if step == 1 { "rope".into() } else { String::new() },
            pick: 0,
            step,
            focused: true,
            names,
            style: Some(FieldStyle::Line),
            items: None,
        }
    }

    fn types(&self) -> Vec<String> {
        let q = quire_app::markdown::fields::normalize_key(&self.query);
        let mut v: Vec<String> = self.names.iter().filter(|n| q.is_empty() || n.contains(&q)).cloned().collect();
        if !q.is_empty() && !v.contains(&q) && quire_app::markdown::fields::is_field_name(&self.query) {
            v.push(q);
        }
        v
    }
}

impl OverlayView for AddLinkDialog {
    fn show(&mut self, ctx: &egui::Context, ws: &mut Workspace, pal: &Palette) -> OverlayResult {
        let mut res = OverlayResult::default();
        egui::Modal::new(egui::Id::unique("quire-add-typed-link")).show(ctx, |ui| {
            let w = 460.0_f32.min(ctx.content_rect().width() - 48.0);
            ui.set_width(w);
            ui.heading("Add typed link");
            let how = match self.style {
                Some(FieldStyle::Line) => "This note writes typed links as inline fields: field:: [[Note]].",
                _ => "This note writes typed links as properties: field: \"[[Note]]\".",
            };
            ui.label(RichText::new(how).size(12.0).color(pal.muted));
            ui.add_space(6.0);
            let hint = if self.step == 0 { "Type: up, down, supports… or a new one" } else { "Note to link to" };
            if self.step == 1 {
                ui.label(RichText::new(format!("Type: {}", self.field)).color(pal.text));
            }
            let r = ui.add(TextEdit::singleline(&mut self.query).hint_text(hint).desired_width(w));
            if !self.focused {
                r.request_focus();
                self.focused = true;
            }
            if r.changed() {
                self.pick = 0;
            }
            let (down, up, enter) = ui.input(|i| (i.key_pressed(Key::ArrowDown), i.key_pressed(Key::ArrowUp), i.key_pressed(Key::Enter)));
            let key = (self.step, self.query.clone(), ws.index().version());
            let items: Rc<Vec<(String, String)>> = match &self.items {
                Some((k, v)) if *k == key => Rc::clone(v),
                _ => {
                    let v: Vec<(String, String)> = if self.step == 0 {
                        self.types().into_iter().map(|t| (t, String::new())).collect()
                    } else {
                        let here = ws.active_doc().and_then(|d| d.path.clone());
                        ws.index()
                            .read()
                            .quick_open(&self.query, 12)
                            .into_iter()
                            .filter(|h| Some(&h.path) != here.as_ref())
                            .map(|h| (h.path.to_string(), h.title))
                            .collect()
                    };
                    let v = Rc::new(v);
                    self.items = Some((key, Rc::clone(&v)));
                    v
                }
            };
            if down {
                self.pick = (self.pick + 1).min(items.len().saturating_sub(1));
            }
            if up {
                self.pick = self.pick.saturating_sub(1);
            }
            let mut chosen = None;
            egui::ScrollArea::vertical().max_height(260.0).auto_shrink([false, true]).show(ui, |ui| {
                if items.is_empty() {
                    ui.label(
                        RichText::new(if self.step == 0 { "Type a name: letters, digits, - and _" } else { "No note matches" })
                            .color(pal.muted),
                    );
                }
                for (i, (key, title)) in items.iter().enumerate() {
                    let label = if self.step == 0 { key.clone() } else { format!("{title}  ·  {key}") };
                    let resp = ui.selectable_label(i == self.pick, label);
                    if resp.clicked() {
                        self.pick = i;
                        chosen = Some(i);
                    }
                    if i == self.pick && (down || up) {
                        resp.scroll_to_me(None);
                    }
                }
            });
            if enter && !items.is_empty() {
                chosen = Some(self.pick.min(items.len() - 1));
            }
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if self.step == 1 && ui.button("Back").clicked() {
                    self.step = 0;
                    self.query.clone_from(&self.field);
                    self.focused = false;
                }
                if ui.button("Cancel").clicked() {
                    res.close = true;
                }
            });
            if let Some(i) = chosen {
                if self.step == 0 {
                    self.field.clone_from(&items[i].0);
                    self.step = 1;
                    self.query.clear();
                    self.pick = 0;
                    self.focused = false;
                } else {
                    let target = NotePath::new(items[i].0.as_str());
                    if ws.add_typed_link(&self.field, &target) {
                        res.close = true;
                        res.jumped = true;
                    }
                }
            }
        });
        res
    }
}
