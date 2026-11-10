// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Importing, with a record that makes it undoable (0.12 switching tools).
//!
//! [`run`] writes an [`quire_import::ImportPlan`] into one folder, keeps a
//! history snapshot of every note (provenance "Imported (Logseq)"), and
//! writes an **import record** to `.quire/imports/<id>.json` listing every
//! file it created with a hash of what it wrote. [`undo`] moves exactly those
//! files to the trash, except any that were edited since (those stay, and are
//! reported). Nothing outside the record is touched, and a record can't point
//! outside the vault's visible folders. See `docs/formats/import-record.md`.

use std::sync::atomic::AtomicBool;

use quire_agent::{Actor, Op, Provenance};
use quire_import::{ImportPlan, ImportReport, Progress};
use quire_vault::{ContentHash, NotePath, Vault};
use serde::{Deserialize, Serialize};

/// Where records live, relative to the vault root.
pub const RECORD_DIR: &str = ".quire/imports";
pub const RECORD_FORMAT: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Item {
    pub path: NotePath,
    pub hash: ContentHash,
}

/// One import, as written to `.quire/imports/<id>.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ImportRecord {
    pub format: u32,
    /// `i_<unix ms>`.
    pub id: String,
    pub at_ms: i64,
    /// "Logseq", "Joplin"…
    pub source: String,
    /// The export's file or folder name (never its full path).
    pub origin: String,
    /// The folder everything went into ("" for the vault root).
    pub folder: String,
    pub notes: Vec<Item>,
    pub files: Vec<Item>,
    /// Set when the import was undone.
    pub undone_ms: Option<i64>,
    /// Written by a person's hand or a newer Quire: kept on rewrite.
    #[serde(flatten)]
    pub other: serde_json::Map<String, serde_json::Value>,
}

impl Default for ImportRecord {
    fn default() -> Self {
        Self {
            format: RECORD_FORMAT,
            id: String::new(),
            at_ms: 0,
            source: String::new(),
            origin: String::new(),
            folder: String::new(),
            notes: Vec::new(),
            files: Vec::new(),
            undone_ms: None,
            other: serde_json::Map::new(),
        }
    }
}

impl ImportRecord {
    pub fn title(&self) -> String {
        let place = if self.folder.is_empty() { "the vault root".to_string() } else { format!("{}/", self.folder) };
        format!("{} import into {place}: {} notes, {} files", self.source, self.notes.len(), self.files.len())
    }
}

fn record_path(id: &str) -> NotePath {
    NotePath::new(format!("{RECORD_DIR}/{id}.json"))
}

/// A record id is `i_` and digits: it names a file, so nothing else is accepted.
fn valid_id(id: &str) -> bool {
    id.strip_prefix("i_").is_some_and(|d| !d.is_empty() && d.len() <= 20 && d.bytes().all(|b| b.is_ascii_digit()))
}

/// The import's name as shown: a file name, not a path.
fn origin_name(source: &std::path::Path) -> String {
    source.file_name().map_or_else(|| "export".to_string(), |n| n.to_string_lossy().into_owned())
}

/// Import `plan` into `folder`. Notes get a history snapshot saying where they
/// came from; the record is written even when the import is cancelled part
/// way, so what was written can be undone.
pub fn run(
    vault: &Vault,
    plan: &ImportPlan,
    folder: &str,
    origin: &std::path::Path,
    device: &str,
    now_ms: i64,
    cancel: Option<&AtomicBool>,
    progress: &mut dyn FnMut(Progress),
) -> Result<(ImportReport, ImportRecord), String> {
    let report = quire_import::apply_with(plan, vault, folder, cancel, progress)?;
    let source = plan.source.map_or("Unknown", quire_import::Source::label);
    let mut rec = ImportRecord {
        id: format!("i_{now_ms}"),
        at_ms: now_ms,
        source: source.to_string(),
        origin: origin_name(origin),
        folder: folder.trim_matches('/').to_string(),
        ..ImportRecord::default()
    };
    let actor = Actor::import(source, device);
    for p in &report.created {
        if p.is_markdown() {
            let Ok(text) = vault.read(p) else { continue };
            let prov = Provenance::new(actor.clone(), Op::Create, &text, now_ms);
            // History is a convenience: a note that can't get a snapshot is still imported.
            let _ = vault.snapshot_with(p, &text, now_ms, true, Some(&prov.to_json()));
            rec.notes.push(Item { path: p.clone(), hash: ContentHash::of(&text) });
        } else if let Ok(bytes) = vault.storage().read_bytes(p) {
            rec.files.push(Item { path: p.clone(), hash: ContentHash::of_bytes(&bytes) });
        }
    }
    save(vault, &rec)?;
    Ok((report, rec))
}

fn save(vault: &Vault, rec: &ImportRecord) -> Result<(), String> {
    let json = serde_json::to_string_pretty(rec).map_err(|e| e.to_string())?;
    vault.storage().write_atomic(&record_path(&rec.id), &json).map_err(|e| format!("couldn't save the import record: {e}"))
}

/// Every import record of this vault, newest first. Records that can't be
/// read, or were made by a newer Quire, are skipped.
pub fn list(vault: &Vault) -> Vec<ImportRecord> {
    let mut out: Vec<ImportRecord> = vault
        .storage()
        .list_under(RECORD_DIR)
        .unwrap_or_default()
        .iter()
        .filter(|p| std::path::Path::new(p.as_str()).extension().is_some_and(|e| e.eq_ignore_ascii_case("json")))
        .filter_map(|p| serde_json::from_str::<ImportRecord>(&vault.storage().read(p).ok()?).ok())
        .filter(|r| r.format <= RECORD_FORMAT && valid_id(&r.id))
        .collect();
    out.sort_by_key(|r| std::cmp::Reverse(r.at_ms));
    out
}

/// What undoing an import would do.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UndoPlan {
    /// Unchanged since the import: will go to the trash.
    pub trash: Vec<NotePath>,
    /// Edited since: stay.
    pub edited: Vec<NotePath>,
    /// Already gone.
    pub missing: usize,
}

/// A path from a record that is still inside the folder the import used and plain.
fn trustworthy(rec: &ImportRecord, p: &NotePath) -> bool {
    let s = p.as_str();
    let inside = rec.folder.is_empty() || s.starts_with(&format!("{}/", rec.folder));
    inside && !s.split('/').any(|seg| seg.is_empty() || seg == ".." || seg == "." || seg.starts_with('.')) && !s.contains('\\')
}

pub fn undo_plan(vault: &Vault, rec: &ImportRecord) -> UndoPlan {
    let mut plan = UndoPlan::default();
    for item in rec.notes.iter().chain(&rec.files) {
        if !trustworthy(rec, &item.path) {
            continue;
        }
        if !vault.exists(&item.path) {
            plan.missing += 1;
            continue;
        }
        let same = vault.storage().read_bytes(&item.path).is_ok_and(|b| ContentHash::of_bytes(&b) == item.hash);
        if same { plan.trash.push(item.path.clone()) } else { plan.edited.push(item.path.clone()) }
    }
    plan
}

/// Move what the import created, and nobody changed, to the trash.
pub fn undo(vault: &Vault, id: &str, now_ms: i64) -> Result<(ImportRecord, UndoPlan), String> {
    if !valid_id(id) {
        return Err("that isn't an import id".into());
    }
    let mut rec = list(vault).into_iter().find(|r| r.id == id).ok_or_else(|| format!("no import {id} in this vault"))?;
    if rec.undone_ms.is_some() {
        return Err("that import was already undone".into());
    }
    let plan = undo_plan(vault, &rec);
    for p in &plan.trash {
        vault.trash(p).map_err(|e| format!("couldn't move {p} to the trash: {e}"))?;
    }
    rec.undone_ms = Some(now_ms);
    save(vault, &rec)?;
    Ok((rec, plan))
}

#[cfg(test)]
mod tests {
    use super::*;
    use quire_import::{PlannedFile, PlannedNote, Source, Stamp};

    fn plan() -> ImportPlan {
        ImportPlan {
            source: Some(Source::Joplin),
            notes: vec![
                PlannedNote::new("A.md", "# A\n"),
                PlannedNote::new("sub/B.md", "b"),
                PlannedNote::new("../evil.md", "x"),
                PlannedNote::new(".quire/shared/vault.toml", "x"),
            ],
            files: vec![PlannedFile { path: NotePath::new("attachments/x.png"), bytes: vec![1, 2], times: Stamp::default() }],
            warnings: vec![],
        }
    }

    #[test]
    fn an_import_is_recorded_and_undone_without_touching_edited_notes() {
        let v = Vault::in_memory(&[("Mine.md", "mine")]);
        let (report, rec) =
            run(&v, &plan(), "Imported", std::path::Path::new("/home/x/export.jex"), "d1", 1000, None, &mut |_| {}).unwrap();
        assert_eq!((report.notes, report.files), (2, 1));
        assert_eq!(report.refused.len(), 2, "{:?}", report.refused);
        assert_eq!(rec.origin, "export.jex");
        assert_eq!(rec.id, "i_1000");
        assert!(v.exists(&NotePath::new("Imported/sub/B.md")));
        assert!(!v.exists(&NotePath::new("evil.md")));
        // The history says where the note came from.
        let hist = v.history(&NotePath::new("Imported/A.md"));
        assert_eq!(hist.len(), 1);
        assert_eq!(v.read_provenance(&hist[0]).unwrap()["actor"]["kind"], "import");
        assert_eq!(list(&v).len(), 1);

        v.write(&NotePath::new("Imported/sub/B.md"), "b, then edited").unwrap();
        let before = undo_plan(&v, &rec);
        assert_eq!((before.trash.len(), before.edited.len()), (2, 1));
        let (rec2, done) = undo(&v, "i_1000", 2000).unwrap();
        assert_eq!(done.trash.len(), 2);
        assert!(rec2.undone_ms.is_some());
        assert!(!v.exists(&NotePath::new("Imported/A.md")));
        assert!(!v.exists(&NotePath::new("Imported/attachments/x.png")));
        assert_eq!(v.read(&NotePath::new("Imported/sub/B.md")).unwrap(), "b, then edited");
        assert_eq!(v.read(&NotePath::new("Mine.md")).unwrap(), "mine");
        assert!(undo(&v, "i_1000", 3000).is_err(), "only once");
    }

    #[test]
    fn a_record_cannot_trash_outside_its_folder() {
        let v = Vault::in_memory(&[("Keep.md", "k"), ("Imported/Ok.md", "ok")]);
        let rec = ImportRecord {
            id: "i_5".into(),
            folder: "Imported".into(),
            notes: vec![
                Item { path: NotePath::new("Keep.md"), hash: ContentHash::of("k") },
                Item { path: NotePath::new("Imported/../Keep.md"), hash: ContentHash::of("k") },
                Item { path: NotePath::new("Imported/Ok.md"), hash: ContentHash::of("ok") },
            ],
            ..ImportRecord::default()
        };
        let plan = undo_plan(&v, &rec);
        assert_eq!(plan.trash, vec![NotePath::new("Imported/Ok.md")]);
        assert!(!valid_id("i_../x") && !valid_id("../x") && valid_id("i_17"));
    }

    #[test]
    fn a_cancelled_import_can_still_be_undone() {
        let v = Vault::in_memory(&[]);
        let stop = AtomicBool::new(false);
        let mut seen = 0;
        let (report, rec) = run(&v, &plan(), "Imported", std::path::Path::new("x"), "d", 5, Some(&stop), &mut |p| {
            seen += 1;
            if p.done == 2 {
                stop.store(true, std::sync::atomic::Ordering::Relaxed);
            }
        })
        .unwrap();
        assert!(report.cancelled);
        assert!(seen >= 2);
        assert_eq!(rec.notes.len() + rec.files.len(), report.created.len());
        let (_, done) = undo(&v, &rec.id, 6).unwrap();
        assert_eq!(done.trash.len(), report.created.len());
    }
}
