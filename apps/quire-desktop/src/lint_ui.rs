// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The linter's UI (0.12): Settings → Linter, and "Tidy notes in a folder…".
//!
//! The folder dialog reads and tidies every note on a thread (nothing is
//! written), lists the notes that would change with a diff of each, and
//! writes the ones you leave ticked in small steps, so the window stays
//! alive. The result is one batch in the activity list: "Undo" puts every
//! note back (a note you edited since is merged or left alone, never lost).

use std::collections::HashSet;
use std::fmt::Write as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, channel};
use std::time::Duration;

use egui::{Button, ComboBox, RichText, TextEdit};
use quire_app::Workspace;
use quire_app::switch_state::linter::{Options, RULES};
use quire_app::switch_state::{DiffRow, FileLint, LintPlan, diff_rows, fixes_text, lint_options, lint_text};
use quire_app::vault::NotePath;

use crate::overlays::{OverlayResult, OverlayView};
use crate::panels::PanelOut;
use crate::registry::FeatureUi;
use crate::theme::Palette;

/// Text being typed in Settings → Linter, kept until the box is left.
#[derive(Default)]
pub(crate) struct SwitchUi {
    /// Home's view box (see `home_ui`).
    pub view: Option<String>,
    lint: Option<LintBuf>,
}

struct LintBuf {
    from: Options,
    exclude: String,
    order: String,
    key: String,
    format: String,
}

impl LintBuf {
    fn new(o: &Options) -> Self {
        Self {
            from: o.clone(),
            exclude: o.exclude.join(", "),
            order: o.front_matter_order.join(", "),
            key: o.date_modified_key.clone(),
            format: o.date_format.clone(),
        }
    }
}

fn list(text: &str) -> Vec<String> {
    text.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect()
}

fn choice<T: Copy + PartialEq>(
    ui: &mut egui::Ui,
    label: &str,
    value: &mut T,
    all: &[(T, &'static str)],
    names: &dyn Fn(&str) -> &'static str,
) {
    ui.horizontal_wrapped(|ui| {
        ui.label(label);
        let current = all.iter().find(|(v, _)| v == value).map_or("", |(_, t)| *t);
        ComboBox::from_id_salt(("quire-lint-choice", label)).selected_text(names(current)).show_ui(ui, |ui| {
            for (v, t) in all {
                ui.selectable_value(value, *v, names(t));
            }
        });
    });
}

pub(crate) fn available(_ws: &Workspace) -> bool {
    true
}

const EXAMPLE: &str = "---\ntags: [Work,  Ideas]\ntitle:   Plan\n---\nPlan\n====\n\n* one  \n* two\n\n\n\n#### Deep heading\nSee [[ Other note | alias ]] and __bold__.   \n";

pub(crate) fn settings_tab(ui: &mut egui::Ui, ws: &mut Workspace, fui: &mut FeatureUi, pal: &Palette) -> PanelOut {
    let mut out = PanelOut::default();
    if ws.vault().is_none() {
        ui.label(RichText::new("Open a vault to set up the linter.").color(pal.muted));
        return out;
    }
    let before = ws.switch_prefs().lint.clone();
    let mut o = before.clone();
    let st = &mut fui.switch;
    if st.lint.as_ref().is_none_or(|b| b.from != before) {
        st.lint = Some(LintBuf::new(&before));
    }
    let buf = st.lint.as_mut().expect("just set");
    ui.label(RichText::new("Tidies the Markdown of a note: spacing, headings, lists, front matter. It never touches code, math or HTML, and it changes only whitespace and markup. Saved in this vault (vault.toml, [lint.format]).").color(pal.muted));
    ui.add_space(6.0);
    ui.horizontal_wrapped(|ui| {
        if ui.button("Tidy this note").clicked() {
            out.run = Some(quire_app::Command::LintNote);
        }
        if ui.button("Tidy notes in a folder…").clicked() {
            out.run = Some(quire_app::Command::LintFolder);
        }
    });
    ui.add_space(4.0);
    ui.checkbox(&mut o.on_save, "Tidy a note when I save it (Save, not autosave)");
    let mut left = false;
    ui.horizontal_wrapped(|ui| {
        ui.label("Leave alone these folders");
        left |= ui
            .add(
                TextEdit::singleline(&mut buf.exclude)
                    .hint_text("Archive, Templates")
                    .desired_width(ui.available_width().clamp(60.0, 220.0)),
            )
            .lost_focus();
    });
    ui.label(RichText::new("A note with `lint: false` in its front matter is always left alone.").size(12.0).color(pal.muted));
    ui.add_space(8.0);
    ui.label(RichText::new("Rules").strong());
    let rule_help = |id: &str| RULES.iter().find(|r| r.id == id).map_or("", |r| r.help);
    ui.checkbox(&mut o.heading_increment, "Headings go down one level at a time").on_hover_text(rule_help("headings"));
    ui.horizontal_wrapped(|ui| {
        ui.label("The first heading is at most level");
        ui.add(egui::DragValue::new(&mut o.heading_start).range(0..=6));
        ui.label(RichText::new("0 for any").size(12.0).color(pal.muted));
    });
    choice(ui, "Underlined headings", &mut o.heading_style, quire_markdown_choices::HEADING_STYLE, &|t| {
        if t == "atx" { "become # headings" } else { "stay as they are" }
    });
    ui.checkbox(&mut o.trailing_spaces, "No spaces at the end of a line").on_hover_text(rule_help("trailing-spaces"));
    choice(ui, "Two spaces for a line break", &mut o.two_space_linebreak, quire_markdown_choices::TWO_SPACE, &|t| {
        if t == "strip" { "are removed" } else { "stay" }
    });
    choice(ui, "Bullet", &mut o.list_marker, quire_markdown_choices::LIST_MARKER, &|t| match t {
        "-" => "- dash",
        "*" => "* star",
        "+" => "+ plus",
        _ => "keep each list's own",
    });
    choice(ui, "List indent", &mut o.list_indent, quire_markdown_choices::LIST_INDENT, &|t| match t {
        "2" => "2 spaces",
        "4" => "4 spaces",
        "tab" => "a tab",
        _ => "keep",
    });
    ui.checkbox(&mut o.blank_lines, "A blank line around headings, lists and code").on_hover_text(rule_help("blank-lines"));
    ui.horizontal_wrapped(|ui| {
        ui.label("At most this many blank lines in a row");
        ui.add(egui::DragValue::new(&mut o.max_blank_lines).range(1..=5));
    });
    ui.checkbox(&mut o.final_newline, "End the file with one line ending");
    choice(ui, "Emphasis", &mut o.emphasis, quire_markdown_choices::EMPHASIS, &|t| match t {
        "asterisk" => "*italic*",
        "underscore" => "_italic_",
        _ => "keep",
    });
    choice(ui, "Strong", &mut o.strong, quire_markdown_choices::EMPHASIS, &|t| match t {
        "asterisk" => "**bold**",
        "underscore" => "__bold__",
        _ => "keep",
    });
    ui.checkbox(&mut o.link_spacing, "No spaces inside [[ links | aliases ]]").on_hover_text(rule_help("link-spacing"));
    choice(ui, "Line endings", &mut o.line_endings, quire_markdown_choices::LINE_ENDINGS, &|t| match t {
        "lf" => "all lines LF",
        "crlf" => "all lines CRLF",
        _ => "keep the file's own",
    });
    ui.add_space(8.0);
    ui.label(RichText::new("Front matter").strong());
    ui.checkbox(&mut o.front_matter, "Tidy `key: value` spacing");
    ui.horizontal_wrapped(|ui| {
        ui.label("Keys in this order first");
        left |= ui
            .add(
                TextEdit::singleline(&mut buf.order)
                    .hint_text("title, tags, created")
                    .desired_width(ui.available_width().clamp(60.0, 220.0)),
            )
            .lost_focus();
    });
    choice(ui, "The other keys", &mut o.front_matter_sort, quire_markdown_choices::SORT, &|t| {
        if t == "alpha" { "A to Z" } else { "stay as they are" }
    });
    choice(ui, "Tags", &mut o.tags_format, quire_markdown_choices::TAGS_FORMAT, &|t| match t {
        "list" => "a list, one per line",
        "flow" => "[one, line]",
        _ => "keep",
    });
    choice(ui, "Tag letters", &mut o.tags_case, quire_markdown_choices::TAGS_CASE, &|t| if t == "lower" { "lower case" } else { "keep" });
    ui.checkbox(&mut o.date_modified, "Set a “date modified” property when a note is tidied by hand");
    if o.date_modified {
        ui.horizontal_wrapped(|ui| {
            ui.label("Property");
            left |= ui.add(TextEdit::singleline(&mut buf.key).desired_width(ui.available_width().clamp(60.0, 100.0))).lost_focus();
            ui.label("format");
            left |= ui.add(TextEdit::singleline(&mut buf.format).desired_width(ui.available_width().clamp(60.0, 140.0))).lost_focus();
        });
    }
    if left {
        o.exclude = list(&buf.exclude);
        o.front_matter_order = list(&buf.order);
        if !buf.key.trim().is_empty() {
            o.date_modified_key = buf.key.trim().to_string();
        }
        if !buf.format.trim().is_empty() {
            o.date_format = buf.format.trim().to_string();
        }
    }
    // What the rules do, on a small note.
    ui.add_space(10.0);
    ui.label(RichText::new("Example").strong());
    let path: NotePath = "Example.md".into();
    let after = lint_text(&o, &path, EXAMPLE, true).text;
    for (title, text) in [("Before", EXAMPLE), ("After", after.as_str())] {
        ui.label(RichText::new(title).size(11.5).color(pal.muted));
        egui::Frame::NONE.fill(pal.panel).inner_margin(6.0).corner_radius(4.0).show(ui, |ui| {
            egui::ScrollArea::horizontal().id_salt(("quire-lint-example", title)).auto_shrink([false, true]).show(ui, |ui| {
                ui.add(egui::Label::new(RichText::new(text.replace(' ', "·")).monospace().size(11.0)).extend());
            });
        });
    }
    if o != before {
        ws.set_lint_options(o);
    }
    out
}

/// The enums' choice lists, under short names.
mod quire_markdown_choices {
    use quire_app::switch_state::linter::{
        Emphasis, HeadingStyle, LineEndings, ListIndent, ListMarker, Sort, TagsCase, TagsFormat, TwoSpace,
    };
    pub(super) const HEADING_STYLE: &[(HeadingStyle, &str)] = HeadingStyle::ALL;
    pub(super) const TWO_SPACE: &[(TwoSpace, &str)] = TwoSpace::ALL;
    pub(super) const LIST_MARKER: &[(ListMarker, &str)] = ListMarker::ALL;
    pub(super) const LIST_INDENT: &[(ListIndent, &str)] = ListIndent::ALL;
    pub(super) const EMPHASIS: &[(Emphasis, &str)] = Emphasis::ALL;
    pub(super) const LINE_ENDINGS: &[(LineEndings, &str)] = LineEndings::ALL;
    pub(super) const SORT: &[(Sort, &str)] = Sort::ALL;
    pub(super) const TAGS_FORMAT: &[(TagsFormat, &str)] = TagsFormat::ALL;
    pub(super) const TAGS_CASE: &[(TagsCase, &str)] = TagsCase::ALL;
}

// ------------------------------------------------------------------ the folder dialog

#[derive(Clone, Copy, PartialEq, Eq)]
enum Stage {
    Choose,
    Reading,
    Review,
    Writing,
    Done,
}

/// "Tidy notes in a folder…".
pub(crate) struct LintFolderDialog {
    stage: Stage,
    folder: String,
    rx: Option<Receiver<LintPlan>>,
    cancel: Arc<AtomicBool>,
    plan: Option<LintPlan>,
    ticked: Vec<bool>,
    shown: usize,
    diff: Option<(usize, Vec<DiffRow>)>,
    to_write: Vec<FileLint>,
    undone: Option<usize>,
    batch: Option<String>,
}

impl LintFolderDialog {
    pub(crate) fn make(ws: &mut Workspace) -> Box<dyn OverlayView> {
        // The open note's folder is the likely one.
        let folder = ws
            .active_doc()
            .and_then(|d| d.path.as_ref())
            .and_then(|p| p.as_str().rsplit_once('/').map(|(f, _)| f.to_string()))
            .unwrap_or_default();
        Box::new(Self::new(folder))
    }

    fn new(folder: String) -> Self {
        Self {
            stage: Stage::Choose,
            folder,
            rx: None,
            cancel: Arc::new(AtomicBool::new(false)),
            plan: None,
            ticked: Vec::new(),
            shown: 0,
            diff: None,
            to_write: Vec::new(),
            undone: None,
            batch: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn sample(n: usize) -> Self {
        let mut d = Self::new("Projects".into());
        if n == 0 {
            return d;
        }
        let files: Vec<FileLint> = (0..30)
            .map(|i| FileLint {
                path: format!("Projects/Some folder with a long name/A note with a long title {i}.md").as_str().into(),
                before: "Title\n=====\n\n* one  \n* two\n".into(),
                after: "# Title\n\n- one\n- two\n".into(),
                rules: vec!["headings", "list-marker", "trailing-spaces"],
            })
            .collect();
        d.ticked = vec![true; files.len()];
        d.plan = Some(LintPlan { files, checked: 120, skipped: 4, ..LintPlan::default() });
        d.stage = if n == 1 { Stage::Review } else { Stage::Done };
        d.batch = Some("b_1".into());
        d
    }

    fn read(&mut self, ws: &Workspace) {
        let Some(vault) = ws.vault().cloned() else { return };
        let opts = lint_options(&vault);
        let skip: HashSet<NotePath> = ws.unsaved_notes();
        let folder = self.folder.trim().trim_matches('/').to_string();
        let cancel = Arc::new(AtomicBool::new(false));
        self.cancel = Arc::clone(&cancel);
        let (tx, rx) = channel();
        std::thread::spawn(move || {
            let _ = tx.send(quire_app::switch_state::plan_folder(&vault, &folder, &opts, &skip, &cancel));
        });
        self.rx = Some(rx);
        self.stage = Stage::Reading;
    }

    fn explain(plan: &LintPlan) -> String {
        let mut s = format!("{} notes checked, {} would change", plan.checked, plan.files.len());
        if plan.skipped > 0 {
            let _ = write!(s, ", {} left alone (excluded, opted out or being edited)", plan.skipped);
        }
        s
    }
}

impl OverlayView for LintFolderDialog {
    fn show(&mut self, ctx: &egui::Context, ws: &mut Workspace, pal: &Palette) -> OverlayResult {
        let mut res = OverlayResult::default();
        if let Some(rx) = &self.rx
            && let Ok(plan) = rx.try_recv()
        {
            self.ticked = vec![true; plan.files.len()];
            self.plan = Some(plan);
            self.rx = None;
            self.shown = 0;
            self.diff = None;
            self.stage = Stage::Review;
        }
        if self.stage == Stage::Writing {
            // A few notes a frame, so the window keeps drawing.
            if ws.lint_apply_step(&self.to_write, 20) {
                self.batch = ws.last_lint_batch().map(|(id, _)| id);
                self.stage = Stage::Done;
            }
            ctx.request_repaint();
        }
        if self.rx.is_some() {
            ctx.request_repaint_after(Duration::from_millis(100));
        }
        let mut go_read = false;
        let mut go_write = false;
        let mut undo = false;
        egui::Modal::new(egui::Id::unique("quire-lint-folder")).show(ctx, |ui| {
            let w = 680.0_f32.min(ctx.content_rect().width() - 48.0);
            ui.set_width(w);
            ui.set_max_width(w);
            ui.heading("Tidy notes in a folder");
            match self.stage {
                Stage::Choose => {
                    ui.label(RichText::new("Tidies every note in the folder with the rules in Settings → Linter. You see what would change first; nothing is written until you say so.").color(pal.muted));
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        ui.label("Folder");
                        ui.add(TextEdit::singleline(&mut self.folder).hint_text("the whole vault").desired_width(280.0));
                    });
                    let top: Vec<String> = ws.vault().map(quire_app::vault::Vault::folders).unwrap_or_default().into_iter().filter(|f| !f.contains('/')).take(8).collect();
                    if !top.is_empty() {
                        ui.horizontal_wrapped(|ui| {
                            for f in top {
                                if ui.small_button(&f).clicked() {
                                    self.folder = f;
                                }
                            }
                        });
                    }
                }
                Stage::Reading => {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label("Reading the notes…");
                    });
                }
                Stage::Review => self.review(ui, pal, (ctx.content_rect().height() - 300.0).clamp(70.0, 220.0)),
                Stage::Writing => {
                    let n = self.to_write.len();
                    let done = ws.features.switch.apply.as_ref().map_or(n, |a| a.next);
                    ui.add(egui::ProgressBar::new(if n == 0 { 1.0 } else { done as f32 / n as f32 }).text(format!("{done} of {n}")));
                }
                Stage::Done => {
                    if let Some(k) = self.undone {
                        ui.label(RichText::new(format!("Undone: {k} notes put back.")).color(pal.added));
                    } else {
                        let n = self.to_write.len().max(self.plan.as_ref().map_or(0, |p| p.files.len()));
                        ui.label(RichText::new(format!("Done: {n} notes tidied. Each has a version in its history.")).strong().color(pal.strong));
                    }
                }
            }
            ui.add_space(8.0);
            ui.separator();
            ui.horizontal(|ui| match self.stage {
                Stage::Choose => {
                    if ui.button("Preview").clicked() {
                        go_read = true;
                    }
                    if ui.button("Cancel").clicked() {
                        res.close = true;
                    }
                }
                Stage::Reading => {
                    if ui.button("Cancel").clicked() {
                        self.cancel.store(true, Ordering::Relaxed);
                        res.close = true;
                    }
                }
                Stage::Review => {
                    let n = self.ticked.iter().filter(|t| **t).count();
                    if ui.add_enabled(n > 0, Button::new(format!("Tidy {n} notes"))).clicked() {
                        go_write = true;
                    }
                    if ui.button("Back").clicked() {
                        self.stage = Stage::Choose;
                    }
                    if ui.button("Cancel").clicked() {
                        res.close = true;
                    }
                }
                Stage::Writing => {}
                Stage::Done => {
                    if self.undone.is_none() && self.batch.is_some() && ui.button("Undo").clicked() {
                        undo = true;
                    }
                    if ui.button("Close").clicked() {
                        res.close = true;
                    }
                }
            });
        });
        if go_read {
            self.read(ws);
        }
        if go_write && let Some(plan) = &self.plan {
            self.to_write = plan.files.iter().zip(&self.ticked).filter(|(_, t)| **t).map(|(f, _)| f.clone()).collect();
            ws.lint_apply_begin(self.folder.trim().trim_matches('/'));
            self.stage = Stage::Writing;
        }
        if undo && let Some(id) = self.batch.clone() {
            let r = ws.meta_undo(&id);
            self.undone = Some(r.restored);
            if !r.conflicts.is_empty() {
                ws.notify(
                    quire_app::workspace::NoticeLevel::Info,
                    format!("{} notes were edited since and were left as they are", r.conflicts.len()),
                );
            }
        }
        res
    }

    fn closed(&mut self, _ws: &mut Workspace) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

impl LintFolderDialog {
    fn review(&mut self, ui: &mut egui::Ui, pal: &Palette, body_h: f32) {
        let Some(plan) = &self.plan else { return };
        ui.label(RichText::new(Self::explain(plan)).strong().color(pal.strong));
        if plan.cancelled {
            ui.label(RichText::new("Reading stopped early.").color(pal.warn));
        }
        for (p, rule) in plan.reverted.iter().take(3) {
            ui.label(
                RichText::new(format!("{}: the “{rule}” rule would have changed more than formatting, so it was skipped", p.as_str()))
                    .size(12.0)
                    .color(pal.warn),
            );
        }
        if plan.files.is_empty() {
            ui.label(RichText::new("Everything is already tidy.").color(pal.muted));
            return;
        }
        ui.horizontal(|ui| {
            if ui.small_button("Select all").clicked() {
                self.ticked.iter_mut().for_each(|t| *t = true);
            }
            if ui.small_button("Select none").clicked() {
                self.ticked.iter_mut().for_each(|t| *t = false);
            }
        });
        let n = plan.files.len();
        let row_h = ui.text_style_height(&egui::TextStyle::Body) + 6.0;
        // The list above, the diff of the chosen note below: side by side
        // the list's scroll area measured wider than its column.
        let list_h = (body_h * 0.4).max(56.0);
        let diff_h = (body_h - list_h - 22.0).max(40.0);
        egui::ScrollArea::vertical().id_salt("quire-lint-files").max_height(list_h).auto_shrink([false, false]).show_rows(
            ui,
            row_h,
            n,
            |ui, range| {
                for i in range {
                    ui.horizontal(|ui| {
                        ui.checkbox(&mut self.ticked[i], "");
                        let p = &plan.files[i].path;
                        let w = ui.available_width().max(20.0);
                        if ui
                            .add_sized(
                                [w, row_h - 4.0],
                                Button::selectable(self.shown == i, RichText::new(p.as_str()).size(12.5)).truncate(),
                            )
                            .on_hover_text(p.as_str())
                            .clicked()
                        {
                            self.shown = i;
                            self.diff = None;
                        }
                    });
                }
            },
        );
        let i = self.shown.min(n - 1);
        let f = &plan.files[i];
        ui.add(egui::Label::new(RichText::new(fixes_text(&f.rules)).size(12.0).color(pal.muted)).truncate());
        if self.diff.as_ref().is_none_or(|(k, _)| *k != i) {
            self.diff = Some((i, diff_rows(&f.before, &f.after, 400)));
        }
        egui::ScrollArea::vertical().id_salt("quire-lint-diff").max_height(diff_h).auto_shrink([false, false]).show(ui, |ui| {
            if let Some((_, rows)) = &self.diff {
                for r in rows {
                    let (color, sign) = match r.tag {
                        '+' => (pal.added, "+"),
                        '-' => (pal.danger, "-"),
                        _ => (pal.muted, " "),
                    };
                    ui.add(
                        egui::Label::new(RichText::new(format!("{sign} {}", r.text.replace(' ', "·"))).monospace().size(11.5).color(color))
                            .truncate(),
                    );
                }
            }
        });
    }
}
