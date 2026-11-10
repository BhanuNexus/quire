// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Pictures in locked folders (0.12, docs/design/0.12-loose.md §3).
//!
//! A file of a locked folder is stored encrypted (`photo.jpg.qenc`), so
//! egui's file loader can't read it. While the folder is unlocked its bytes
//! are decrypted into memory and handed to egui under
//! `bytes://quire-locked/<path>`; once a frame, the pictures of folders
//! that are locked again are forgotten (egui's bytes and textures). Nothing
//! is written anywhere.

use egui::{Context, Id};
use quire_app::Workspace;
use quire_app::attach::AttachmentSource;
use quire_app::vault::NotePath;

/// Where decrypted pictures go in egui's bytes loader.
pub(crate) const PREFIX: &str = "bytes://quire-locked/";

fn key() -> Id {
    Id::unique("quire-locked-media")
}

/// The URI that shows the file an embed points to: `file://` for a plain
/// file; for a file of a locked folder that is unlocked, its decrypted
/// bytes in memory; `None` when there is nothing to show.
pub(crate) fn uri(ctx: &Context, ws: &Workspace, target: &str) -> Option<String> {
    match ws.attachment_source(target)? {
        AttachmentSource::File(p) => Some(format!("file://{}", p.display())),
        AttachmentSource::Locked(p) => {
            let uri = format!("{PREFIX}{}", p.as_str());
            let known = ctx.data(|d| d.get_temp::<Vec<String>>(key()).is_some_and(|v| v.contains(&uri)));
            if !known {
                let bytes = ws.read_locked_attachment(&p)?;
                ctx.include_bytes(uri.clone(), bytes);
                ctx.data_mut(|d| d.get_temp_mut_or_default::<Vec<String>>(key()).push(uri.clone()));
            }
            Some(uri)
        }
    }
}

/// Once a frame: forget the pictures of folders that are locked now.
pub(crate) fn sweep(ctx: &Context, ws: &Workspace) {
    let shown: Vec<String> = ctx.data(|d| d.get_temp(key())).unwrap_or_default();
    if shown.is_empty() {
        return;
    }
    let (keep, gone): (Vec<String>, Vec<String>) =
        shown.into_iter().partition(|u| u.strip_prefix(PREFIX).is_some_and(|p| ws.in_unlocked_folder(&NotePath::new(p))));
    if gone.is_empty() {
        return;
    }
    for u in &gone {
        ctx.forget_image(u);
    }
    ctx.data_mut(|d| d.insert_temp(key(), keep));
}

/// The pictures shown from memory now (tests).
#[cfg(test)]
fn shown(ctx: &Context) -> Vec<String> {
    ctx.data(|d| d.get_temp(key())).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pictures_of_locked_folders_are_forgotten_when_they_lock() {
        let dir = std::env::temp_dir().join(format!("quire-locked-media-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("attachments")).unwrap();
        std::fs::write(dir.join("attachments/plain.png"), b"png").unwrap();
        let mut ws = Workspace::new(quire_app::Settings::default());
        ws.open_vault(&dir).unwrap();
        // The vault's files are listed on a thread of their own.
        let start = std::time::Instant::now();
        while ws.attachment_source("plain.png").is_none() {
            assert!(start.elapsed().as_secs() < 20, "the file list never came");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let ctx = Context::default();
        let plain = uri(&ctx, &ws, "plain.png").unwrap();
        assert!(plain.starts_with("file://") && plain.ends_with("plain.png"), "{plain}");
        // A picture registered for a folder that isn't unlocked goes on the next sweep.
        let gone = format!("{PREFIX}Journal/photo.png");
        ctx.include_bytes(gone.clone(), b"secret".to_vec());
        ctx.data_mut(|d| d.insert_temp(key(), vec![gone.clone()]));
        sweep(&ctx, &ws);
        assert!(shown(&ctx).is_empty());
        assert!(uri(&ctx, &ws, "photo.png").is_none(), "nothing to show");
        drop(ws);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
