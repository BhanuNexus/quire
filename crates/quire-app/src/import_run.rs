// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Importing, with a record that makes it undoable (0.12 switching tools).
//!
//! [`run`] writes an [`quire_import::ImportPlan`] into one folder, keeps a
//! history snapshot of every note (provenance "Imported (Logseq)"), and
//! writes an **import record** to `.quire/imports/<id>.json` listing every
//! file it created with a hash of the bytes it wrote *as stored* (for a file
//! in a locked folder that is the encrypted file, so the record never holds a
//! hash of a locked note's text). The record is created before the first
//! write and kept up to date as the import goes, so an import that fails or
//! is killed can still be undone. [`undo`] moves exactly those files to the
//! trash, except any that were edited since (those stay, and are reported) and
//! any that aren't where the import put them (reported; the import then stays
//! open). Nothing outside the record is touched, and a record can't point
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
    /// Set while the import runs. A record that still has it was left by an
    /// import that was killed: what it lists was written, and can be undone.
    pub in_progress: bool,
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
            in_progress: false,
            other: serde_json::Map::new(),
        }
    }
}

impl ImportRecord {
    pub fn title(&self) -> String {
        let place = if self.folder.is_empty() { "the vault root".to_string() } else { format!("{}/", self.folder) };
        let unfinished = if self.in_progress { " (didn't finish)" } else { "" };
        format!("{} import into {place}: {} notes, {} files{unfinished}", self.source, self.notes.len(), self.files.len())
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

/// How many files are written between saves of the record while importing.
const SAVE_EVERY: usize = 20;

/// Import `plan` into `folder`. Notes get a history snapshot saying where they
/// came from. The record exists before the first file is written and is saved
/// every few files, at the end, and when the import fails or is cancelled, so
/// whatever was written can be undone. A failure part way comes back as
/// `report.error` next to what was written, not as an `Err`; `Err` means
/// nothing was written.
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
    let source = plan.source.map_or("Unknown", quire_import::Source::label);
    let mut rec = ImportRecord {
        id: format!("i_{now_ms}"),
        at_ms: now_ms,
        source: source.to_string(),
        origin: origin_name(origin),
        folder: folder.trim_matches('/').to_string(),
        in_progress: true,
        ..ImportRecord::default()
    };
    create_record(vault, &mut rec)?;
    let actor = Actor::import(source, device);
    let mut since_save = 0;
    let report = quire_import::apply_observed(plan, vault, folder, cancel, progress, &mut |p| {
        let Some(item) = item_for(vault, p) else { return };
        if p.is_markdown() {
            if let Ok(text) = vault.read(p) {
                let prov = Provenance::new(actor.clone(), Op::Create, &text, now_ms);
                // History is a convenience: a note that can't get a snapshot is still imported.
                let _ = vault.snapshot_with(p, &text, now_ms, true, Some(&prov.to_json()));
            }
            rec.notes.push(item);
        } else {
            rec.files.push(item);
        }
        since_save += 1;
        if since_save >= SAVE_EVERY {
            since_save = 0;
            // A failed save here is retried at the end; the files are already written.
            let _ = save(vault, &rec);
        }
    });
    rec.in_progress = false;
    if rec.notes.is_empty() && rec.files.is_empty() && report.error.is_some() {
        // Nothing was written: no record to keep.
        let _ = vault.storage().remove(&record_path(&rec.id));
        return Err(report.error.unwrap_or_default());
    }
    save(vault, &rec)?;
    Ok((report, rec))
}

/// Make the record's file, taking the next free id when another import made
/// one in the same millisecond (never replacing a record).
fn create_record(vault: &Vault, rec: &mut ImportRecord) -> Result<(), String> {
    for _ in 0..100 {
        let json = serde_json::to_string_pretty(&*rec).map_err(|e| e.to_string())?;
        let made = vault
            .storage()
            .write_bytes_if(&record_path(&rec.id), &|now| now.is_none(), json.as_bytes())
            .map_err(|e| format!("couldn't save the import record: {e}"))?;
        if made {
            return Ok(());
        }
        rec.at_ms += 1;
        rec.id = format!("i_{}", rec.at_ms);
    }
    Err("couldn't make a free import record".into())
}

/// The bytes of `p` as stored on disk: the encrypted file for a note in a
/// locked folder, otherwise the file.
fn stored_bytes(vault: &Vault, p: &NotePath) -> Option<Vec<u8>> {
    if vault.locked_folder(p).is_some()
        && let Ok(b) = vault.storage().read_bytes(&quire_vault::lock::stored(p))
    {
        return Some(b);
    }
    vault.storage().read_bytes(p).ok()
}

fn item_for(vault: &Vault, p: &NotePath) -> Option<Item> {
    Some(Item { path: p.clone(), hash: ContentHash::of_bytes(&stored_bytes(vault, p)?) })
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
    /// Not where the import put them: renamed, moved or deleted since. They
    /// aren't touched, and the import stays open (not marked undone).
    pub missing: Vec<NotePath>,
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
            plan.missing.push(item.path.clone());
            continue;
        }
        let same = stored_bytes(vault, &item.path).is_some_and(|b| ContentHash::of_bytes(&b) == item.hash);
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
    if plan.missing.is_empty() {
        rec.undone_ms = Some(now_ms);
    } else {
        // Some of it isn't where the import put it (renamed, moved or deleted):
        // keep the import open so it can be run again. What went to the trash
        // is done, so it leaves the record.
        rec.notes.retain(|i| !plan.trash.contains(&i.path));
        rec.files.retain(|i| !plan.trash.contains(&i.path));
    }
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
    fn a_record_is_written_before_the_first_file_and_kept_up_to_date() {
        let v = Vault::in_memory(&[]);
        let notes: Vec<(String, String)> = (0..45).map(|i| (format!("N{i}.md"), format!("# {i}\n"))).collect();
        let big = ImportPlan {
            source: Some(Source::Joplin),
            notes: notes.iter().map(|(p, c)| PlannedNote::new(p.as_str(), c.as_str())).collect(),
            ..ImportPlan::default()
        };
        let mut seen = Vec::new();
        let (report, rec) = run(&v, &big, "Imp", std::path::Path::new("x"), "d", 77, None, &mut |p| {
            // What a killed import leaves: read the record from disk while it runs.
            if p.done == 1 || p.done == 45 {
                seen.push(list(&v).into_iter().next().map(|r| (r.in_progress, r.notes.len())));
            }
        })
        .unwrap();
        assert_eq!(seen[0], Some((true, 0)), "the record exists before the first write");
        let (still, n) = seen[1].unwrap();
        assert!(still && n >= 40, "saved as it goes: {n} notes recorded when the 45th is written");
        assert!(!rec.in_progress && rec.notes.len() == 45 && report.error.is_none());
        assert!(!list(&v)[0].in_progress);

        // The unfinished record, as a killed import leaves it, can be undone.
        let mut half = list(&v).remove(0);
        half.in_progress = true;
        half.notes.truncate(30);
        save(&v, &half).unwrap();
        let (_, done) = undo(&v, &half.id, 99).unwrap();
        assert_eq!(done.trash.len(), 30);
        assert!(v.exists(&NotePath::new("Imp/N40.md")), "only what the record lists");
    }

    #[test]
    fn a_refused_import_leaves_no_record() {
        let v = Vault::in_memory(&[]);
        let err = run(&v, &plan(), ".quire", std::path::Path::new("x"), "d", 5, None, &mut |_| {}).unwrap_err();
        assert!(err.contains("isn't allowed"), "{err}");
        assert!(list(&v).is_empty() && v.storage().list_under(RECORD_DIR).unwrap_or_default().is_empty());
        // Two imports in the same millisecond get two records.
        let a = run(&v, &plan(), "A", std::path::Path::new("x"), "d", 9, None, &mut |_| {}).unwrap().1;
        let b = run(&v, &plan(), "B", std::path::Path::new("x"), "d", 9, None, &mut |_| {}).unwrap().1;
        assert_ne!(a.id, b.id);
        assert_eq!(list(&v).len(), 2);
    }

    #[test]
    fn a_locked_note_leaves_no_hash_of_its_text_and_is_still_undone() {
        use quire_vault::lock::{Kdf, lock_folder};
        let v = Vault::in_memory(&[("Journal/Old.md", "old")]);
        lock_folder(&v, "Journal", Some("a passphrase for the test"), Kdf::TEST, 1).unwrap();
        let text = "# Diary\nMARKER-secret\n";
        let p = ImportPlan {
            source: Some(Source::Markdown),
            notes: vec![PlannedNote::new("Day.md", text)],
            files: vec![PlannedFile { path: NotePath::new("attachments/p.png"), bytes: b"MARKER-pic".to_vec(), times: Stamp::default() }],
            warnings: vec![],
        };
        let (_, rec) = run(&v, &p, "Journal", std::path::Path::new("x"), "d", 1, None, &mut |_| {}).unwrap();
        let raw = v.storage().read(&record_path(&rec.id)).unwrap();
        assert!(!raw.contains(ContentHash::of(text).to_string().as_str()), "{raw}");
        assert!(!raw.contains(ContentHash::of_bytes(b"MARKER-pic").to_string().as_str()), "{raw}");
        assert_eq!(v.read(&NotePath::new("Journal/Day.md")).unwrap(), text);
        // Undo still matches them (by the stored bytes), and trashes the encrypted files.
        let before = undo_plan(&v, &rec);
        assert_eq!((before.trash.len(), before.edited.len()), (2, 0), "{before:?}");
        // An edit re-seals the note, so it is kept.
        v.write(&NotePath::new("Journal/Day.md"), "edited").unwrap();
        let after = undo_plan(&v, &rec);
        assert_eq!((after.trash.len(), after.edited.len()), (1, 1));
        let (_, done) = undo(&v, &rec.id, 2).unwrap();
        assert_eq!(done.trash.len(), 1);
        assert_eq!(v.read(&NotePath::new("Journal/Day.md")).unwrap(), "edited");
        assert!(!v.storage().list().unwrap().iter().any(|p| p.as_str().contains("attachments/p.png")));
    }

    #[test]
    fn a_renamed_note_is_reported_and_the_import_stays_open() {
        let v = Vault::in_memory(&[]);
        let (_, rec) = run(&v, &plan(), "Imported", std::path::Path::new("x"), "d", 1000, None, &mut |_| {}).unwrap();
        v.storage().rename(&NotePath::new("Imported/A.md"), &NotePath::new("Imported/Renamed.md")).unwrap();
        let (rec2, done) = undo(&v, &rec.id, 2000).unwrap();
        assert_eq!(done.missing, vec![NotePath::new("Imported/A.md")]);
        assert_eq!(done.trash.len(), 2);
        assert!(rec2.undone_ms.is_none(), "not marked undone");
        assert!(v.exists(&NotePath::new("Imported/Renamed.md")), "the renamed note is not touched");
        // Put it back and run again: only what is left is done, and now it closes.
        v.storage().rename(&NotePath::new("Imported/Renamed.md"), &NotePath::new("Imported/A.md")).unwrap();
        let (rec3, again) = undo(&v, &rec.id, 3000).unwrap();
        assert_eq!((again.trash.len(), again.missing.len()), (1, 0));
        assert!(rec3.undone_ms.is_some());
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
