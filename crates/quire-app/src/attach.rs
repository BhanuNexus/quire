// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Attachments: pasted images, dropped files, and resolving embeds to files.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use quire_vault::NotePath;

use crate::workspace::{NoticeLevel, Workspace};

/// Name → path cache for non-note files.
///
/// 0.12: the vault is listed on a thread of its own, never on the UI
/// thread, and not at all before the first frame (the app opens its
/// startup vault deferred): with 10,000 notes the listing was one of the
/// two walks of the vault before the first frame. Until the first list
/// arrives, embeds don't resolve and the Files tab shows only notes.
#[derive(Default)]
pub struct FileCache(RefCell<FileList>);

/// The vault's files that aren't notes: by lowercase name and path, as
/// stored (`Journal/photo.jpg.qenc` too, which the sidebar lists); and the
/// files of locked folders by their plain names, for embeds (0.12).
#[derive(Default)]
struct FileMap {
    files: HashMap<String, NotePath>,
    locked: HashMap<String, NotePath>,
}

#[derive(Default)]
struct FileList {
    /// The list and when it was read.
    map: Option<(Instant, std::rc::Rc<FileMap>)>,
    /// A file came or went: read the list again (the old one stays until then).
    stale: bool,
    /// A listing on its way.
    pending: Option<crossbeam_channel::Receiver<FileMap>>,
}

impl FileCache {
    /// Read the list again (a file was just added or removed).
    pub(crate) fn clear(&self) {
        self.0.borrow_mut().stale = true;
    }
}

/// Where an embed's file is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttachmentSource {
    /// A plain file on disk.
    File(PathBuf),
    /// A file of a locked folder (stored encrypted, `X.qenc`): its bytes come
    /// only from memory, through [`Workspace::read_locked_attachment`],
    /// while the folder is unlocked (0.12).
    Locked(NotePath),
}

/// List the vault's files that aren't notes ([`FileMap`]).
fn list_files(vault: &quire_vault::Vault) -> FileMap {
    let mut map = FileMap::default();
    for p in vault.storage().list().unwrap_or_default() {
        if p.is_markdown() {
            continue;
        }
        if let Some(plain) = quire_vault::lock::plain_name(&p).filter(|q| !q.is_markdown() && vault.locked_folder(q).is_some()) {
            map.locked.entry(plain.file_name().to_lowercase()).or_insert_with(|| plain.clone());
            map.locked.insert(plain.as_str().to_lowercase(), plain);
        }
        map.files.entry(p.file_name().to_lowercase()).or_insert_with(|| p.clone());
        map.files.insert(p.as_str().to_lowercase(), p);
    }
    map
}

const IMAGE_EXTS: &[&str] = &["png", "jpg", "jpeg", "gif", "webp", "bmp", "svg"];

pub fn is_image(name: &str) -> bool {
    name.rsplit_once('.').is_some_and(|(_, e)| IMAGE_EXTS.contains(&e.to_ascii_lowercase().as_str()))
        || quire_canvas::excalidraw::is_drawing(name)
}

/// Encode straight RGBA8 pixels as PNG.
pub fn encode_png(width: u32, height: u32, rgba: &[u8]) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut out, width, height);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        let mut w = enc.write_header().map_err(|e| e.to_string())?;
        w.write_image_data(rgba).map_err(|e| e.to_string())?;
    }
    Ok(out)
}

/// The embed target if `line` is just an image embed (`![[x.png]]` or `![](x.png)`).
pub fn image_embed(line: &str) -> Option<&str> {
    let t = line.trim();
    let target = match t.strip_prefix("![[").and_then(|r| r.strip_suffix("]]")) {
        Some(inner) => inner.split('|').next().unwrap_or(inner),
        None => t.strip_prefix("![")?.split_once("](")?.1.strip_suffix(')')?.split_whitespace().next()?,
    };
    (is_image(target) && !target.contains("://")).then_some(target.trim())
}

impl Workspace {
    fn insert_at_caret(&mut self, text: &str) {
        let needs_break = self.active_doc().is_some_and(|d| {
            let s = d.selection().start();
            s > 0 && d.buffer().char_at(s - 1).is_some_and(|c| c != '\n')
        });
        let t = if needs_break && text.starts_with('!') { format!("\n{text}") } else { text.to_string() };
        self.insert_text(&t);
    }

    /// Where a picture or file for the active note goes: `Ok(None)` the
    /// vault's attachments folder; `Ok(Some(folder))` (0.12) the note is in
    /// locked folder `folder`, unlocked, so it goes into that folder's own
    /// attachments subfolder, encrypted; `Err(())` the folder is locked
    /// (refused, with the reason): it would be stored unencrypted.
    fn attachment_folder(&mut self) -> Result<Option<String>, ()> {
        let Some(path) = self.active_doc().and_then(|d| d.path.clone()) else { return Ok(None) };
        let Some(f) = self.vault.as_ref().and_then(|v| v.locked_folder(&path)) else { return Ok(None) };
        if self.in_unlocked_folder(&path) {
            return Ok(Some(f.path));
        }
        self.notify(NoticeLevel::Info, format!("{}/ is locked: unlock it to add pictures and files to its notes.", f.path));
        Err(())
    }

    /// Store an attachment for the active note ([`Self::attachment_folder`]).
    fn store_attachment(&self, folder: Option<&str>, name: &str, bytes: &[u8]) -> quire_vault::Result<NotePath> {
        let vault = self.vault.as_ref().ok_or_else(|| quire_vault::VaultError::NotFound("no vault is open".into()))?;
        match folder {
            Some(f) => vault.save_locked_attachment(f, name, bytes),
            None => vault.save_attachment(name, bytes),
        }
    }

    /// Save a pasted image (straight RGBA8) and embed it at the caret.
    pub fn paste_image(&mut self, width: u32, height: u32, rgba: &[u8]) -> Option<NotePath> {
        self.vault.as_ref()?;
        self.active_doc()?;
        let folder = self.attachment_folder().ok()?;
        let png = match encode_png(width, height, rgba) {
            Ok(b) => b,
            Err(e) => {
                self.notify(NoticeLevel::Error, format!("Couldn't encode image: {e}"));
                return None;
            }
        };
        let name = format!("Pasted image {}.png", chrono::Local::now().format("%Y%m%d %H%M%S"));
        match self.store_attachment(folder.as_deref(), &name, &png) {
            Ok(p) => {
                self.file_cache.clear();
                self.insert_at_caret(&format!("![[{}]]\n", p.file_name()));
                self.notify(NoticeLevel::Info, format!("Saved {p}"));
                Some(p)
            }
            Err(e) => {
                self.notify(NoticeLevel::Error, e.to_string());
                None
            }
        }
    }

    /// Bring dropped files into the vault and link them at the caret.
    pub fn import_files(&mut self, files: &[PathBuf]) -> usize {
        let Some(vault) = self.vault.clone() else { return 0 };
        let Ok(folder) = self.attachment_folder() else { return 0 };
        let root = self.vault_root().map(Path::to_path_buf);
        let mut links = Vec::new();
        for f in files {
            let name = f.file_name().map_or_else(|| "file".into(), |n| n.to_string_lossy().into_owned());
            let inside =
                root.as_ref().and_then(|r| f.canonicalize().ok()?.strip_prefix(r).ok().map(|p| p.to_string_lossy().replace('\\', "/")));
            let is_md = name.to_ascii_lowercase().ends_with(".md");
            let link = match inside {
                Some(rel) if is_md => format!("[[{}]]", NotePath::new(rel).without_ext()),
                Some(rel) => format!("![[{rel}]]"),
                None => match std::fs::read(f) {
                    Ok(bytes) if is_md => {
                        let title = name.trim_end_matches(".md");
                        // Into the locked folder of a locked note (encrypted there).
                        match vault.create_note(folder.as_deref().unwrap_or(""), title, &String::from_utf8_lossy(&bytes)) {
                            Ok(p) => {
                                self.index.upsert(p.clone(), String::from_utf8_lossy(&bytes).into_owned());
                                format!("[[{}]]", p.stem())
                            }
                            Err(e) => {
                                self.notify(NoticeLevel::Error, e.to_string());
                                continue;
                            }
                        }
                    }
                    Ok(bytes) => match self.store_attachment(folder.as_deref(), &name, &bytes) {
                        Ok(p) if is_image(&name) => format!("![[{}]]", p.file_name()),
                        Ok(p) => format!("[[{}]]", p.file_name()),
                        Err(e) => {
                            self.notify(NoticeLevel::Error, e.to_string());
                            continue;
                        }
                    },
                    Err(e) => {
                        self.notify(NoticeLevel::Error, format!("{}: {e}", f.display()));
                        continue;
                    }
                },
            };
            links.push(link);
        }
        if links.is_empty() || self.active_doc().is_none() {
            return 0;
        }
        self.file_cache.clear();
        let n = links.len();
        let joined = if links.iter().all(|l| l.starts_with('!')) { links.join("\n") + "\n" } else { links.join(" ") };
        self.insert_at_caret(&joined);
        self.notify(NoticeLevel::Info, format!("Added {n} file{}", if n == 1 { "" } else { "s" }));
        n
    }

    /// List the vault's files that aren't notes again (on another thread)
    /// when a file came or went, or the list is older than a minute (5 s
    /// without a watcher). Never before the deferred start of a vault.
    pub(crate) fn refresh_file_cache(&self) {
        let Some(vault) = self.vault.as_ref() else { return };
        let mut c = self.file_cache.0.borrow_mut();
        if let Some(map) = c.pending.as_ref().and_then(|rx| rx.try_recv().ok()) {
            c.pending = None;
            c.map = Some((Instant::now(), std::rc::Rc::new(map)));
        }
        if c.pending.is_some() || self.deferred_start {
            return;
        }
        let max_age = if self.watcher.is_some() { Duration::from_mins(1) } else { Duration::from_secs(5) };
        if !c.stale && c.map.as_ref().is_some_and(|(t, _)| t.elapsed() <= max_age) {
            return;
        }
        c.stale = false;
        // Unit tests read the list at once; the app never waits for it.
        if cfg!(test) {
            c.map = Some((Instant::now(), std::rc::Rc::new(list_files(vault))));
            return;
        }
        let (tx, rx) = crossbeam_channel::bounded(1);
        let vault = vault.clone();
        let spawned = std::thread::Builder::new().name("quire-file-list".into()).spawn(move || {
            quire_index::background::lower_priority();
            let _ = tx.send(list_files(&vault));
        });
        if spawned.is_ok() {
            c.pending = Some(rx);
        }
    }

    /// Whether a listing of the vault's files is on its way (the app repaints).
    pub(crate) fn file_list_pending(&self) -> bool {
        self.file_cache.0.borrow().pending.is_some()
    }

    /// The list of the vault's files that aren't notes, as last read.
    fn file_map(&self) -> Option<std::rc::Rc<FileMap>> {
        self.refresh_file_cache();
        self.file_cache.0.borrow().map.as_ref().map(|(_, m)| std::rc::Rc::clone(m))
    }

    /// The vault's files that aren't notes (attachments, `.base`, `.canvas`,
    /// `.pdf`), sorted; listed again every few seconds.
    pub fn other_files(&self) -> Vec<NotePath> {
        let mut v: Vec<NotePath> = self.file_map().map(|m| m.files.values().cloned().collect()).unwrap_or_default();
        v.sort();
        v.dedup();
        v
    }

    /// The file an embed points to (`photo.jpg`, `attachments/photo.jpg`).
    fn embed_target(&self, target: &str) -> Option<NotePath> {
        let map = self.file_map();
        let key = quire_markdown::extract::internal_target(target).0.to_lowercase();
        let key = if key.is_empty() { target.to_lowercase() } else { key };
        let get = |m: &HashMap<String, NotePath>| m.get(&key).or_else(|| m.get(&target.to_lowercase())).cloned();
        let found = map.as_ref().and_then(|m| get(&m.files).or_else(|| get(&m.locked)));
        // `![[Plan.excalidraw]]` is the note `Plan.excalidraw.md`.
        match found {
            Some(f) => Some(f),
            None if quire_canvas::excalidraw::is_drawing(target) => self.index.read().resolve(target.trim_end_matches(".md"), None),
            None => None,
        }
    }

    /// Absolute path of the file an embed points to, for image display.
    /// `None` for a file of a locked folder: it is never read from disk as
    /// it is ([`Self::attachment_source`]).
    pub fn attachment_path(&self, target: &str) -> Option<PathBuf> {
        match self.attachment_source(target)? {
            AttachmentSource::File(p) => Some(p),
            AttachmentSource::Locked(_) => None,
        }
    }

    /// Where the file an embed points to is: on disk, or encrypted in a
    /// locked folder (0.12).
    pub fn attachment_source(&self, target: &str) -> Option<AttachmentSource> {
        let vault = self.vault.as_ref()?;
        let found = self.embed_target(target)?;
        if vault.locked_folder(&found).is_some() {
            return Some(AttachmentSource::Locked(found));
        }
        vault.storage().absolute(&found).map(AttachmentSource::File)
    }

    /// The decrypted bytes of a file of a locked folder, while the folder is
    /// unlocked (memory only; `None` while it is locked).
    pub fn read_locked_attachment(&self, p: &NotePath) -> Option<Vec<u8>> {
        if !self.in_unlocked_folder(p) {
            return None;
        }
        self.vault.as_ref()?.read_file(p).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Settings;

    #[test]
    fn recognises_image_embeds() {
        assert_eq!(image_embed("  ![[Pasted image 1.png]] "), Some("Pasted image 1.png"));
        assert_eq!(image_embed("![[diagram.svg|300]]"), Some("diagram.svg"));
        assert_eq!(image_embed("![alt](attachments/x.JPG \"t\")"), Some("attachments/x.JPG"));
        assert_eq!(image_embed("![[Note]]"), None);
        assert_eq!(image_embed("text ![[a.png]]"), None);
        assert_eq!(image_embed("![](https://x.dev/a.png)"), None);
    }

    #[test]
    fn paste_and_import() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = Workspace::new(Settings::default());
        w.open_vault(dir.path()).unwrap();
        let id = w.new_note(Some("Doc")).unwrap();
        w.insert_text("Look:");
        let p = w.paste_image(2, 1, &[255, 0, 0, 255, 0, 0, 255, 255]).unwrap();
        let text = w.doc(id).unwrap().text().to_string();
        assert!(text.starts_with("Look:\n![[Pasted image "), "{text}");
        let bytes = w.vault().unwrap().storage().read_bytes(&p).unwrap();
        assert_eq!(&bytes[1..4], b"PNG");
        let abs = w.attachment_path(p.file_name()).unwrap();
        assert!(abs.exists());

        let outside = tempfile::tempdir().unwrap();
        let img = outside.path().join("photo.jpg");
        std::fs::write(&img, b"jpg").unwrap();
        let md = outside.path().join("Imported.md");
        std::fs::write(&md, "# Imported\n").unwrap();
        assert_eq!(w.import_files(&[img, md]), 2);
        let text = w.doc(id).unwrap().text().to_string();
        assert!(text.contains("![[photo.jpg]] [[Imported]]"), "{text}");
        assert!(w.vault().unwrap().exists(&NotePath::new("attachments/photo.jpg")));
        assert!(w.vault().unwrap().exists(&NotePath::new("Imported.md")));
    }
}
