// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Attachments: pasted images, dropped files, and resolving embeds to files.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use quire_vault::NotePath;

use crate::workspace::{NoticeLevel, Workspace};

/// Name → path cache for non-note files, refreshed every few seconds.
#[derive(Default)]
pub struct FileCache(RefCell<Option<(Instant, HashMap<String, NotePath>)>>);

impl FileCache {
    /// Forget the cache (a file was just added).
    pub(crate) fn clear(&self) {
        self.0.borrow_mut().take();
    }
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

    /// Save a pasted image (straight RGBA8) and embed it at the caret.
    pub fn paste_image(&mut self, width: u32, height: u32, rgba: &[u8]) -> Option<NotePath> {
        let vault = self.vault.clone()?;
        self.active_doc()?;
        let png = match encode_png(width, height, rgba) {
            Ok(b) => b,
            Err(e) => {
                self.notify(NoticeLevel::Error, format!("Couldn't encode image: {e}"));
                return None;
            }
        };
        let name = format!("Pasted image {}.png", chrono::Local::now().format("%Y%m%d %H%M%S"));
        match vault.save_attachment(&name, &png) {
            Ok(p) => {
                self.file_cache.0.borrow_mut().take();
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
                        match vault.create_note("", title, &String::from_utf8_lossy(&bytes)) {
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
                    Ok(bytes) => match vault.save_attachment(&name, &bytes) {
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
        self.file_cache.0.borrow_mut().take();
        let n = links.len();
        let joined = if links.iter().all(|l| l.starts_with('!')) { links.join("\n") + "\n" } else { links.join(" ") };
        self.insert_at_caret(&joined);
        self.notify(NoticeLevel::Info, format!("Added {n} file{}", if n == 1 { "" } else { "s" }));
        n
    }

    /// List the vault's files that aren't notes again if the list is older
    /// than a few seconds.
    fn refresh_file_cache(&self) {
        let Some(vault) = self.vault.as_ref() else { return };
        let mut cache = self.file_cache.0.borrow_mut();
        // The watcher's events clear the cache when a file comes or goes
        // (`Workspace::tick`), so with a watcher the list is only read again
        // now and then, in case it missed one: listing a 10,000-note vault
        // every 5 s was a hitch on the UI thread (0.10 perf).
        let max_age = if self.watcher.is_some() { Duration::from_mins(1) } else { Duration::from_secs(5) };
        let stale = cache.as_ref().is_none_or(|(t, _)| t.elapsed() > max_age);
        if stale {
            let mut map = HashMap::new();
            for p in vault.storage().list().unwrap_or_default().into_iter().filter(|p| !p.is_markdown()) {
                map.entry(p.file_name().to_lowercase()).or_insert_with(|| p.clone());
                map.insert(p.as_str().to_lowercase(), p);
            }
            *cache = Some((Instant::now(), map));
        }
    }

    /// The vault's files that aren't notes (attachments, `.base`, `.canvas`,
    /// `.pdf`), sorted; listed again every few seconds.
    pub fn other_files(&self) -> Vec<NotePath> {
        self.refresh_file_cache();
        let cache = self.file_cache.0.borrow();
        let mut v: Vec<NotePath> = cache.as_ref().map(|(_, m)| m.values().cloned().collect()).unwrap_or_default();
        v.sort();
        v.dedup();
        v
    }

    /// Absolute path of the file an embed points to, for image display.
    pub fn attachment_path(&self, target: &str) -> Option<PathBuf> {
        let vault = self.vault.as_ref()?;
        self.refresh_file_cache();
        let cache = self.file_cache.0.borrow();
        let key = quire_markdown::extract::internal_target(target).0.to_lowercase();
        let key = if key.is_empty() { target.to_lowercase() } else { key };
        let found = cache.as_ref()?.1.get(&key).or_else(|| cache.as_ref()?.1.get(&target.to_lowercase())).cloned();
        // `![[Plan.excalidraw]]` is the note `Plan.excalidraw.md`.
        let found = match found {
            Some(f) => f,
            None if quire_canvas::excalidraw::is_drawing(target) => self.index.read().resolve(target.trim_end_matches(".md"), None)?,
            None => return None,
        };
        vault.storage().absolute(&found)
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
