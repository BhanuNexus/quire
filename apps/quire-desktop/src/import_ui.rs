// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! File → Import…: a wizard in five steps (0.12 switching tools).
//!
//! 1. **Source**: what kind of export (or "find out"), and the file or folder.
//! 2. **Preview**: a dry run, read on a thread: counts, warnings, name
//!    clashes in the target folder and a few converted notes to look at.
//!    Nothing is written.
//! 3. **Folder**: where the notes go (a new folder is the default, so the
//!    whole import is one folder you can look at or trash).
//! 4. **Importing**: written on a thread, with progress and Cancel.
//! 5. **Report**: what was made, what was renamed or refused, and "Undo
//!    import", which moves what the import created (and nobody edited) to the
//!    trash (`quire_app::import_run`).
//!
//! Reading and writing never run on the UI thread; each frame only polls.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, TryRecvError, channel};
use std::time::Duration;

use egui::{ComboBox, ProgressBar, RichText, TextEdit};
use quire_app::Workspace;
use quire_app::import_run::{ImportRecord, UndoPlan};
use quire_app::workspace::NoticeLevel;
use quire_import::{ImportPlan, ImportReport, Limits, Source};

use crate::overlays::{OverlayResult, OverlayView};
use crate::theme::Palette;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Step {
    Source,
    Preview,
    Folder,
    Importing,
    Report,
}

impl Step {
    const ALL: [Self; 5] = [Self::Source, Self::Preview, Self::Folder, Self::Importing, Self::Report];

    const fn title(self) -> &'static str {
        match self {
            Self::Source => "Source",
            Self::Preview => "Preview",
            Self::Folder => "Folder",
            Self::Importing => "Import",
            Self::Report => "Report",
        }
    }
}

type PlanMsg = Result<ImportPlan, String>;
type ClashMsg = (String, Vec<(String, String)>);
type RunMsg = Result<(ImportReport, ImportRecord), String>;

/// An import being written.
struct Running {
    rx: Receiver<RunMsg>,
    cancel: Arc<AtomicBool>,
    done: Arc<AtomicUsize>,
    total: Arc<AtomicUsize>,
}

/// The five-step import wizard.
pub(crate) struct ImportWizard {
    step: Step,
    /// `None`: find out what the export is.
    kind: Option<Source>,
    typed: String,
    folder: String,
    reading: Option<Receiver<PlanMsg>>,
    plan: Option<Arc<ImportPlan>>,
    error: Option<String>,
    /// Name clashes in `clash_folder`, worked out on a thread.
    clashes: Vec<(String, String)>,
    clash_folder: Option<String>,
    clash_rx: Option<Receiver<ClashMsg>>,
    sample: usize,
    running: Option<Running>,
    result: Option<RunMsg>,
    undone: Option<UndoPlan>,
    /// Layout samples show a fixed progress.
    fake_progress: Option<(usize, usize)>,
}

impl ImportWizard {
    pub(crate) fn make(_ws: &mut Workspace) -> Box<dyn OverlayView> {
        Box::new(Self::new())
    }

    fn new() -> Self {
        Self {
            step: Step::Source,
            kind: None,
            typed: String::new(),
            folder: "Imported".into(),
            reading: None,
            plan: None,
            error: None,
            clashes: Vec::new(),
            clash_folder: None,
            clash_rx: None,
            sample: 0,
            running: None,
            result: None,
            undone: None,
            fake_progress: None,
        }
    }

    /// Each step in a state worth checking, for `layout_check`.
    #[cfg(test)]
    pub(crate) fn sample(n: usize) -> Self {
        let mut w = Self::new();
        w.typed = "/home/me/Downloads/joplin-export.jex".into();
        if n == 6 {
            w.error = Some("This doesn't look like a Joplin export: no notes were found in it. ".repeat(3));
            return w;
        }
        if n >= 1 {
            let notes: Vec<quire_import::PlannedNote> = (0..40)
                .map(|i| {
                    quire_import::PlannedNote::new(
                        format!("Work/Project {i}/A note with a rather long title number {i}.md"),
                        format!(
                            "---\ncreated: 2026-11-0{}\n---\n# Note {i}\n\nSome converted text, with a [[link]] and a #tag.\n",
                            i % 9 + 1
                        ),
                    )
                })
                .collect();
            let warnings = (0..6)
                .map(|i| format!("{} links point to notes that aren't in the export (batch {i}, so they stay as links)", i + 2))
                .collect();
            w.plan = Some(Arc::new(ImportPlan { source: Some(Source::Joplin), notes, files: Vec::new(), warnings }));
            w.clashes = vec![("Imported/Work/Plan.md".into(), "Imported/Work/Plan 2.md".into())];
            w.clash_folder = Some(w.folder.clone());
        }
        match n {
            0 => {}
            1 => w.step = Step::Preview,
            2 => w.step = Step::Folder,
            3 => {
                w.step = Step::Importing;
                w.fake_progress = Some((17, 40));
            }
            _ => {
                w.step = Step::Report;
                let report = ImportReport {
                    notes: 40,
                    files: 3,
                    renamed: vec![("Imported/Work/Plan.md".into(), "Imported/Work/Plan 2.md".into())],
                    refused: vec!["../evil.md: a path that leaves the folder".into()],
                    times_set: 43,
                    ..ImportReport::default()
                };
                let rec = ImportRecord {
                    id: "i_1".into(),
                    source: "Joplin".into(),
                    origin: "joplin-export.jex".into(),
                    folder: "Imported".into(),
                    ..ImportRecord::default()
                };
                w.result = Some(Ok((report, rec)));
            }
        }
        w
    }

    fn start_reading(&mut self) {
        let path = expand_home(self.typed.trim());
        let kind = self.kind;
        let (tx, rx) = channel();
        std::thread::spawn(move || {
            let _ = tx.send(quire_import::plan_as(&path, kind, &Limits::default()));
        });
        self.reading = Some(rx);
        self.plan = None;
        self.error = None;
        self.clashes.clear();
        self.clash_folder = None;
        self.sample = 0;
        self.step = Step::Preview;
    }

    fn start_clashes(&mut self, ws: &Workspace) {
        let (Some(plan), Some(vault)) = (self.plan.clone(), ws.vault().cloned()) else { return };
        let folder = self.folder.trim().trim_matches('/').to_string();
        let (tx, rx) = channel();
        let asked = folder.clone();
        std::thread::spawn(move || {
            let c = plan.clashes(&vault, &folder).into_iter().map(|(a, b)| (a.as_str().to_string(), b.as_str().to_string())).collect();
            let _ = tx.send((asked, c));
        });
        self.clash_rx = Some(rx);
    }

    fn start_import(&mut self, ws: &Workspace) {
        let (Some(plan), Some(vault)) = (self.plan.clone(), ws.vault().cloned()) else { return };
        let folder = self.folder.trim().trim_matches('/').to_string();
        let origin = expand_home(self.typed.trim());
        let device = ws.device().id.clone();
        let (tx, rx) = channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let (done, total) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(plan.notes.len() + plan.files.len())));
        let (c2, d2, t2) = (Arc::clone(&cancel), Arc::clone(&done), Arc::clone(&total));
        std::thread::spawn(move || {
            let now = quire_app::history_ops::now_ms();
            let r = quire_app::import_run::run(&vault, &plan, &folder, &origin, &device, now, Some(&c2), &mut |p| {
                d2.store(p.done, Ordering::Relaxed);
                t2.store(p.total, Ordering::Relaxed);
            });
            let _ = tx.send(r);
        });
        self.running = Some(Running { rx, cancel, done, total });
        self.step = Step::Importing;
    }

    /// Take in what the threads have sent.
    fn poll(&mut self, ctx: &egui::Context, ws: &mut Workspace) {
        if let Some(rx) = &self.reading {
            match rx.try_recv() {
                Ok(Ok(plan)) => {
                    self.plan = Some(Arc::new(plan));
                    self.reading = None;
                    self.start_clashes(ws);
                }
                Ok(Err(e)) => {
                    self.error = Some(e);
                    self.reading = None;
                }
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => {
                    self.error = Some("Reading the export stopped unexpectedly".into());
                    self.reading = None;
                }
            }
        }
        if let Some(rx) = &self.clash_rx
            && let Ok((asked, c)) = rx.try_recv()
        {
            self.clashes = c;
            self.clash_folder = Some(asked);
            self.clash_rx = None;
        }
        if let Some(run) = &self.running {
            match run.rx.try_recv() {
                Ok(r) => {
                    self.running = None;
                    match &r {
                        Ok((rep, rec)) => {
                            ws.reload_index();
                            let msg = if rep.cancelled {
                                "Import cancelled"
                            } else if rep.error.is_some() {
                                "Import stopped"
                            } else {
                                "Imported"
                            };
                            ws.notify(
                                NoticeLevel::Info,
                                format!("{msg}: {} notes and {} files in {}", rep.notes, rep.files, folder_label(&rec.folder)),
                            );
                        }
                        Err(e) => ws.notify(NoticeLevel::Error, format!("Import failed: {e}")),
                    }
                    self.result = Some(r);
                    self.step = Step::Report;
                }
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => {
                    self.running = None;
                    self.result = Some(Err("The import stopped unexpectedly".into()));
                    self.step = Step::Report;
                }
            }
        }
        // The folder changed: look for clashes there (one thread at a time).
        let want = self.folder.trim().trim_matches('/').to_string();
        if self.plan.is_some()
            && self.clash_rx.is_none()
            && self.clash_folder.as_deref() != Some(want.as_str())
            && self.step != Step::Importing
            && self.step != Step::Report
        {
            self.start_clashes(ws);
        }
        if self.reading.is_some() || self.clash_rx.is_some() || self.running.is_some() {
            ctx.request_repaint_after(Duration::from_millis(100));
        }
    }

    fn steps_bar(&self, ui: &mut egui::Ui, pal: &Palette) {
        ui.horizontal_wrapped(|ui| {
            for (i, s) in Step::ALL.iter().enumerate() {
                let t = RichText::new(format!("{} {}", i + 1, s.title())).size(12.5);
                ui.label(if *s == self.step { t.strong().color(pal.strong) } else { t.color(pal.muted) });
                if i + 1 < Step::ALL.len() {
                    ui.label(RichText::new("›").color(pal.faint));
                }
            }
        });
    }

    fn source_step(&mut self, ui: &mut egui::Ui, pal: &Palette) {
        ui.label(RichText::new("Pick what you are importing from, then the file or folder. Quire reads it and shows you what would happen before anything is written.").color(pal.muted));
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            ui.label("From");
            let current = self.kind.map_or_else(|| "Find out for me".to_string(), |k| k.label().to_string());
            ComboBox::from_id_salt("quire-import-kind").selected_text(current).width(220.0).show_ui(ui, |ui| {
                ui.selectable_value(&mut self.kind, None, "Find out for me");
                for k in Source::ALL {
                    ui.selectable_value(&mut self.kind, Some(k), k.label());
                }
            });
        });
        if let Some(k) = self.kind {
            ui.label(RichText::new(format!("Pick {}.", k.hint())).size(12.5).color(pal.muted));
        } else {
            ui.label(
                RichText::new("Logseq, Notion, Bear, Evernote, Roam, Google Keep, Joplin, Day One, a folder of Markdown or HTML files.")
                    .size(12.5)
                    .color(pal.muted),
            );
        }
        ui.add_space(6.0);
        ui.horizontal_wrapped(|ui| {
            ui.add(TextEdit::singleline(&mut self.typed).hint_text("a file or folder").desired_width(300.0));
            if ui.button("Folder…").clicked()
                && let Some(p) = rfd::FileDialog::new().pick_folder()
            {
                self.typed = p.display().to_string();
            }
            if ui.button("File…").clicked()
                && let Some(p) =
                    rfd::FileDialog::new().add_filter("Exports", &["zip", "enex", "bear2bk", "jex", "json", "md", "html"]).pick_file()
            {
                self.typed = p.display().to_string();
            }
        });
        if let Some(e) = &self.error {
            ui.add(egui::Label::new(RichText::new(e).color(pal.danger)).wrap());
        }
    }

    fn preview_step(&mut self, ui: &mut egui::Ui, pal: &Palette, avail_h: f32) {
        if self.reading.is_some() {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("Reading the export…");
            });
            return;
        }
        if let Some(e) = &self.error {
            ui.add(egui::Label::new(RichText::new(e).color(pal.danger)).wrap());
            return;
        }
        let Some(plan) = self.plan.clone() else { return };
        ui.label(RichText::new(plan.summary()).strong().color(pal.strong));
        if avail_h >= 520.0 {
            ui.label(RichText::new("This is a dry run: nothing has been written.").size(12.5).color(pal.muted));
        }
        if !self.clashes.is_empty() {
            ui.add_space(4.0);
            ui.label(
                RichText::new(format!(
                    "{} names are taken in {}; those notes will be saved under another name.",
                    self.clashes.len(),
                    folder_label(&self.folder)
                ))
                .color(pal.warn),
            );
            for (want, saved) in self.clashes.iter().take(if avail_h < 600.0 { 1 } else { 3 }) {
                ui.add(egui::Label::new(RichText::new(format!("{want} → {saved}")).size(12.0).color(pal.muted)).truncate());
            }
            if self.clashes.len() > if avail_h < 600.0 { 1 } else { 3 } {
                ui.label(
                    RichText::new(format!("and {} more", self.clashes.len() - if avail_h < 600.0 { 1 } else { 3 }))
                        .size(12.0)
                        .color(pal.muted),
                );
            }
        }
        if !plan.warnings.is_empty() {
            ui.add_space(4.0);
            ui.label(RichText::new(format!("{} warnings", plan.warnings.len())).color(pal.warn));
            egui::ScrollArea::vertical()
                .id_salt("quire-import-warnings")
                .max_height(if avail_h < 520.0 {
                    20.0
                } else if avail_h < 600.0 {
                    36.0
                } else {
                    72.0
                })
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    for w in plan.warnings.iter().take(100) {
                        ui.add(egui::Label::new(RichText::new(format!("⚠ {w}")).size(12.0).color(pal.warn)).wrap());
                    }
                });
        }
        if plan.notes.is_empty() {
            return;
        }
        ui.add_space(if avail_h < 520.0 { 2.0 } else { 6.0 });
        if avail_h >= 520.0 {
            ui.label(RichText::new("A few converted notes").color(pal.muted));
        }
        // Up to six, spread over the list.
        let n = plan.notes.len();
        let picks: Vec<usize> = if n <= 6 { (0..n).collect() } else { (0..6).map(|i| i * (n - 1) / 5).collect() };
        ui.horizontal_wrapped(|ui| {
            for (slot, &i) in picks.iter().enumerate() {
                let name = plan.notes[i].path.stem().to_string();
                ui.selectable_value(&mut self.sample, slot, RichText::new(name).size(12.5));
            }
        });
        let i = picks.get(self.sample).copied().unwrap_or(0);
        let note = &plan.notes[i];
        if avail_h >= 520.0 {
            ui.add(egui::Label::new(RichText::new(note.path.as_str()).size(12.0).color(pal.faint)).truncate());
        }
        egui::ScrollArea::vertical()
            .id_salt("quire-import-sample")
            .max_height((avail_h - 470.0).clamp(30.0, 150.0))
            .auto_shrink([false, true])
            .show(ui, |ui| {
                let head: String = note.content.lines().take(40).collect::<Vec<_>>().join("\n");
                ui.add(egui::Label::new(RichText::new(head).monospace().size(12.0)).wrap());
            });
    }

    fn folder_step(&mut self, ui: &mut egui::Ui, ws: &Workspace, pal: &Palette) {
        ui.label(
            RichText::new(
                "Where should the notes go? A new folder keeps the whole import in one place you can look through, move or undo.",
            )
            .color(pal.muted),
        );
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.label("Into folder");
            ui.add(TextEdit::singleline(&mut self.folder).hint_text("the vault's top folder").desired_width(260.0));
        });
        let existing: Vec<String> =
            ws.vault().map(quire_app::vault::Vault::folders).unwrap_or_default().into_iter().filter(|f| !f.contains('/')).take(8).collect();
        if !existing.is_empty() {
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new("Folders:").size(12.0).color(pal.muted));
                for f in existing {
                    if ui.small_button(&f).clicked() {
                        self.folder = f;
                    }
                }
            });
        }
        ui.add_space(4.0);
        ui.label(
            RichText::new("Existing notes are never overwritten: a name that is taken gets another name.").size(12.5).color(pal.muted),
        );
        if self.clash_rx.is_some() {
            ui.label(RichText::new("Checking for names that are taken…").size(12.0).color(pal.muted));
        } else if self.clashes.is_empty() {
            ui.label(RichText::new(format!("No names are taken in {}.", folder_label(&self.folder))).size(12.5).color(pal.muted));
        } else {
            ui.label(
                RichText::new(format!("{} names are taken in {}.", self.clashes.len(), folder_label(&self.folder)))
                    .size(12.5)
                    .color(pal.warn),
            );
        }
    }

    fn importing_step(&self, ui: &mut egui::Ui, pal: &Palette) {
        let (done, total) = match (&self.running, self.fake_progress) {
            (_, Some(p)) => p,
            (Some(r), None) => (r.done.load(Ordering::Relaxed), r.total.load(Ordering::Relaxed)),
            (None, None) => (0, 0),
        };
        let frac = if total == 0 { 0.0 } else { done as f32 / total as f32 };
        ui.label(RichText::new(format!("Importing into {}…", folder_label(&self.folder))).color(pal.strong));
        ui.add(ProgressBar::new(frac).text(format!("{done} of {total}")));
        ui.label(
            RichText::new("Cancel stops after the file being written. What was written can still be undone.").size(12.5).color(pal.muted),
        );
    }

    fn report_step(&mut self, ui: &mut egui::Ui, ws: &mut Workspace, pal: &Palette) {
        match &self.result {
            Some(Err(e)) => {
                ui.add(egui::Label::new(RichText::new(format!("The import failed: {e}")).color(pal.danger)).wrap());
                ui.label(RichText::new("Nothing outside the folder you chose was changed.").size(12.5).color(pal.muted));
            }
            Some(Ok((rep, rec))) => {
                let head = if rep.cancelled { "Cancelled" } else { "Done" };
                ui.label(
                    RichText::new(format!("{head}: {} notes and {} files written to {}.", rep.notes, rep.files, folder_label(&rec.folder)))
                        .strong()
                        .color(pal.strong),
                );
                let mut facts = Vec::new();
                if !rep.renamed.is_empty() {
                    facts.push(format!(
                        "{} note{} saved under another name because the name was taken",
                        rep.renamed.len(),
                        if rep.renamed.len() == 1 { "" } else { "s" }
                    ));
                }
                if !rep.renamed_files.is_empty() {
                    facts.push(format!(
                        "{} attachment{} saved under another name because a different file had the name; the links follow",
                        rep.renamed_files.len(),
                        if rep.renamed_files.len() == 1 { "" } else { "s" }
                    ));
                }
                if let Some(e) = &rep.error {
                    facts.push(format!("stopped early: {e}. What was written can be undone"));
                }
                if rep.skipped_files > 0 {
                    facts.push(format!(
                        "{} attachment{} already existed and {} kept",
                        rep.skipped_files,
                        if rep.skipped_files == 1 { "" } else { "s" },
                        if rep.skipped_files == 1 { "was" } else { "were" }
                    ));
                }
                if rep.times_set > 0 {
                    facts.push(format!(
                        "created and modified dates kept on {} file{}",
                        rep.times_set,
                        if rep.times_set == 1 { "" } else { "s" }
                    ));
                }
                if !rep.refused.is_empty() {
                    facts.push(format!("{} refused as unsafe", rep.refused.len()));
                }
                for f in facts {
                    ui.label(RichText::new(format!("• {f}")).size(12.5).color(pal.muted));
                }
                for (a, b) in rep.renamed.iter().take(3) {
                    ui.add(egui::Label::new(RichText::new(format!("{a} → {b}")).size(12.0).color(pal.faint)).truncate());
                }
                for r in rep.refused.iter().take(3) {
                    ui.add(egui::Label::new(RichText::new(r).size(12.0).color(pal.warn)).truncate());
                }
                ui.add_space(6.0);
                if let Some(plan) = &self.undone {
                    ui.label(
                        RichText::new(format!(
                            "Undone: {} files moved to .trash{}",
                            plan.trash.len(),
                            match (plan.edited.len(), plan.missing.len()) {
                                (0, 0) => String::new(),
                                (e, 0) => format!("; {e} you edited since stay"),
                                (0, m) => format!("; {m} renamed, moved or deleted since were not touched, so the import stays open"),
                                (e, m) => format!("; {e} you edited since stay and {m} renamed, moved or deleted were not touched, so the import stays open"),
                            }
                        ))
                        .color(pal.added),
                    );
                } else {
                    let undo =
                        ui.button("Undo import").on_hover_text("Moves what this import created, and you haven't edited since, to .trash");
                    if undo.clicked() {
                        self.undone = ws.undo_import(&rec.id.clone());
                    }
                }
            }
            None => {}
        }
    }
}

fn folder_label(f: &str) -> String {
    let f = f.trim().trim_matches('/');
    if f.is_empty() { "the vault's top folder".into() } else { format!("{f}/") }
}

fn expand_home(path: &str) -> PathBuf {
    match path.strip_prefix("~/") {
        Some(rest) => std::env::var_os("HOME").map_or_else(|| PathBuf::from(path), |h| PathBuf::from(h).join(rest)),
        None => PathBuf::from(path),
    }
}

impl OverlayView for ImportWizard {
    fn show(&mut self, ctx: &egui::Context, ws: &mut Workspace, pal: &Palette) -> OverlayResult {
        let mut res = OverlayResult::default();
        self.poll(ctx, ws);
        let mut go: Option<Step> = None;
        let mut close = false;
        let mut cancel_run = false;
        let mut start_read = false;
        let mut start_run = false;
        egui::Modal::new(egui::Id::unique("quire-import-wizard")).frame(egui::Frame::popup(&ctx.global_style()).fill(pal.panel)).show(
            ctx,
            |ui| {
                let w = 620.0_f32.min(ctx.content_rect().width() - 48.0);
                ui.set_width(w);
                ui.set_max_width(w);
                ui.heading("Import notes");
                self.steps_bar(ui, pal);
                ui.separator();
                // One step's content in a box of its own, so the buttons stay put.
                match self.step {
                    Step::Source => self.source_step(ui, pal),
                    Step::Preview => self.preview_step(ui, pal, ctx.content_rect().height()),
                    Step::Folder => self.folder_step(ui, ws, pal),
                    Step::Importing => self.importing_step(ui, pal),
                    Step::Report => self.report_step(ui, ws, pal),
                }
                ui.add_space(8.0);
                ui.separator();
                ui.horizontal(|ui| match self.step {
                    Step::Source => {
                        if ui.add_enabled(!self.typed.trim().is_empty(), egui::Button::new("Read it")).clicked() {
                            start_read = true;
                        }
                        if ui.button("Cancel").clicked() {
                            close = true;
                        }
                    }
                    Step::Preview => {
                        if ui.button("Back").clicked() {
                            go = Some(Step::Source);
                        }
                        let ready = self.plan.as_ref().is_some_and(|p| !p.notes.is_empty());
                        if ui.add_enabled(ready, egui::Button::new("Choose folder")).clicked() {
                            go = Some(Step::Folder);
                        }
                        if self.plan.as_ref().is_some_and(|p| p.notes.is_empty()) {
                            ui.label(RichText::new("Nothing to import was found.").color(pal.warn));
                        }
                        if ui.button("Cancel").clicked() {
                            close = true;
                        }
                    }
                    Step::Folder => {
                        if ui.button("Back").clicked() {
                            go = Some(Step::Preview);
                        }
                        if ui.add_enabled(self.clash_rx.is_none(), egui::Button::new("Import")).clicked() {
                            start_run = true;
                        }
                        if ui.button("Cancel").clicked() {
                            close = true;
                        }
                    }
                    Step::Importing => {
                        if ui.button("Cancel import").clicked() {
                            cancel_run = true;
                        }
                    }
                    Step::Report => {
                        if ui.button("Close").clicked() {
                            close = true;
                        }
                        if self.result.as_ref().is_some_and(Result::is_err) && ui.button("Back to the start").clicked() {
                            go = Some(Step::Source);
                        }
                    }
                });
            },
        );
        if start_read {
            self.start_reading();
        }
        if start_run {
            self.start_import(ws);
        }
        if cancel_run && let Some(r) = &self.running {
            r.cancel.store(true, Ordering::Relaxed);
        }
        if let Some(s) = go {
            self.step = s;
            if s == Step::Source {
                self.result = None;
            }
        }
        res.close = close && self.running.is_none();
        if close && let Some(r) = &self.running {
            // Closing mid-import stops it (what was written can be undone).
            r.cancel.store(true, Ordering::Relaxed);
        }
        res
    }

    fn closed(&mut self, _ws: &mut Workspace) {
        if let Some(r) = &self.running {
            r.cancel.store(true, Ordering::Relaxed);
        }
    }
}
