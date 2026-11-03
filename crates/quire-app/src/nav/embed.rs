// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Notes edited inside another note's `![[embed]]` (design §F.2).
//!
//! An embed edits the target note's own [`Document`]: the open tab when the
//! note is open, otherwise a hidden document kept here (the workspace has no
//! hidden documents of its own). Hidden documents autosave like tabs, keep
//! their own undo history, take in changes made elsewhere, and hand over to a
//! tab when the note is opened.

use std::time::{Duration, Instant};

use quire_text::merge;
use quire_vault::{NotePath, VaultEvent};

use crate::document::{DocId, Document};
use crate::workspace::{NoticeLevel, Workspace};

/// Hidden documents' ids have this bit set, so they never meet a tab's id.
pub const EMBED_DOC_BIT: u64 = 1 << 62;
/// A clean hidden document no embed has shown for this long is dropped.
const FORGET_AFTER: Duration = Duration::from_mins(1);

/// The document to edit `path` through: its tab if open, else a hidden one.
pub fn embed_doc(ws: &mut Workspace, path: &NotePath) -> Option<DocId> {
    if let Some(d) = ws.docs.iter().find(|d| d.path.as_ref() == Some(path)) {
        return Some(d.id);
    }
    let now = Instant::now();
    if let Some(d) = ws.features.nav.hidden.iter().find(|d| d.path.as_ref() == Some(path)) {
        let id = d.id;
        touch(ws, id, now);
        return Some(id);
    }
    let text = match ws.vault()?.read(path) {
        Ok(t) => t,
        Err(e) => {
            ws.notify(NoticeLevel::Error, e.to_string());
            return None;
        }
    };
    let st = &mut ws.features.nav;
    st.next_hidden += 1;
    let id = DocId(EMBED_DOC_BIT | st.next_hidden);
    let large = ws.settings.large_file_kb * 1024;
    ws.features.nav.hidden.push(Document::new(id, Some(path.clone()), &text, large));
    touch(ws, id, now);
    Some(id)
}

fn touch(ws: &mut Workspace, id: DocId, now: Instant) {
    let st = &mut ws.features.nav;
    if let Some(e) = st.hidden_seen.iter_mut().find(|(i, _)| *i == id) {
        e.1 = now;
    } else {
        st.hidden_seen.push((id, now));
    }
}

/// The document behind an embed, to read.
pub fn embed_doc_ref(ws: &Workspace, id: DocId) -> Option<&Document> {
    ws.docs.iter().find(|d| d.id == id).or_else(|| ws.features.nav.hidden.iter().find(|d| d.id == id))
}

/// The document behind an embed, to edit.
pub fn embed_doc_mut(ws: &mut Workspace, id: DocId) -> Option<&mut Document> {
    if ws.docs.iter().any(|d| d.id == id) {
        return ws.docs.iter_mut().find(|d| d.id == id);
    }
    ws.features.nav.hidden.iter_mut().find(|d| d.id == id)
}

/// Save the document behind an embed now.
pub fn save_embedded(ws: &mut Workspace, id: DocId) -> bool {
    if ws.docs.iter().any(|d| d.id == id) {
        return ws.save(id);
    }
    save_hidden(ws, id)
}

fn save_hidden(ws: &mut Workspace, id: DocId) -> bool {
    let Some(vault) = ws.vault().cloned() else { return false };
    let Some(doc) = ws.features.nav.hidden.iter().find(|d| d.id == id) else { return false };
    let Some(path) = doc.path.clone() else { return false };
    let text = doc.text();
    let now = crate::history_ops::now_ms();
    if let Some(orig) = doc.disk_text()
        && *orig != *text
    {
        ws.history.keep_first(&vault, &path, &orig, now - 1);
    }
    match vault.write(&path, &text) {
        Ok(()) => {
            ws.history.keep_with(&vault, &path, &text, now, false, None);
            if let Some(d) = ws.features.nav.hidden.iter_mut().find(|d| d.id == id) {
                d.mark_saved(&text);
            }
            ws.index.upsert(path, text.to_string());
            ws.sync_nudge();
            true
        }
        Err(e) => {
            ws.notify(NoticeLevel::Error, format!("Couldn't save {}: {e}", path.stem()));
            false
        }
    }
}

/// Save every hidden document with unsaved edits (quitting, switching
/// vaults, "save all").
pub(super) fn save_all_hidden(ws: &mut Workspace) {
    let dirty: Vec<DocId> = ws.features.nav.hidden.iter().filter(|d| d.is_dirty() && d.path.is_some()).map(|d| d.id).collect();
    for id in dirty {
        save_hidden(ws, id);
    }
}

/// Autosave hidden documents, hand them over to tabs, forget idle ones.
pub(super) fn tick(ws: &mut Workspace, now: Instant) -> bool {
    if ws.features.nav.hidden.is_empty() {
        return false;
    }
    let delay = Duration::from_millis(ws.settings.autosave_ms);
    let due: Vec<DocId> = ws
        .features
        .nav
        .hidden
        .iter()
        .filter(|d| d.is_dirty() && !d.conflict && d.last_edit().is_some_and(|t| now.duration_since(t) >= delay))
        .map(|d| d.id)
        .collect();
    let mut changed = false;
    for id in due {
        changed |= save_hidden(ws, id);
    }
    // The note was opened in a tab: the tab takes over.
    let opened: Vec<(DocId, NotePath)> = ws
        .features
        .nav
        .hidden
        .iter()
        .filter_map(|d| d.path.clone().filter(|p| ws.docs.iter().any(|t| t.path.as_ref() == Some(p))).map(|p| (d.id, p)))
        .collect();
    for (id, path) in opened {
        if ws.features.nav.hidden.iter().any(|d| d.id == id && d.is_dirty()) {
            save_hidden(ws, id);
        }
        let text = ws.features.nav.hidden.iter().find(|d| d.id == id).map(Document::text);
        if let (Some(text), Some(tab)) = (text, ws.docs.iter_mut().find(|t| t.path.as_ref() == Some(&path)))
            && !tab.is_dirty()
            && *tab.text() != *text
        {
            tab.reload(&text);
        }
        drop_hidden(ws, id);
        changed = true;
    }
    let idle: Vec<DocId> = ws
        .features
        .nav
        .hidden_seen
        .iter()
        .filter(|(_, at)| now.duration_since(*at) > FORGET_AFTER)
        .map(|(id, _)| *id)
        .filter(|id| ws.features.nav.hidden.iter().any(|d| d.id == *id && !d.is_dirty()))
        .collect();
    for id in idle {
        drop_hidden(ws, id);
    }
    changed
}

fn drop_hidden(ws: &mut Workspace, id: DocId) {
    ws.features.nav.hidden.retain(|d| d.id != id);
    ws.features.nav.hidden_seen.retain(|(i, _)| *i != id);
}

pub(super) fn next_deadline(ws: &Workspace) -> Option<Instant> {
    let delay = Duration::from_millis(ws.settings.autosave_ms);
    ws.features.nav.hidden.iter().filter(|d| d.is_dirty()).filter_map(Document::last_edit).map(|t| t + delay).min()
}

/// A note behind an embed changed on disk: take the change in.
pub(super) fn vault_event(ws: &mut Workspace, ev: &VaultEvent) {
    match ev {
        VaultEvent::Changed(p) => {
            let Some(i) = ws.features.nav.hidden.iter().position(|d| d.path.as_ref() == Some(p)) else { return };
            let Some(content) = ws.vault().and_then(|v| v.read(p).ok()) else { return };
            let doc = &mut ws.features.nav.hidden[i];
            if doc.matches_disk(&content) {
                return;
            }
            if !doc.is_dirty() {
                doc.reload(&content);
                return;
            }
            let mine = doc.text().to_string();
            let base = doc.disk_text();
            let r = merge::merge(base.as_deref(), &mine, &content);
            if r.is_clean() {
                doc.replace_minimally(&r.text);
                doc.set_disk_base(&content);
            } else {
                // Keep what was typed here; the other version is in version history.
                doc.set_disk_base(&content);
                let name = p.stem().to_string();
                ws.notify(NoticeLevel::Warn, format!("“{name}” also changed elsewhere; your edits in the embed were kept"));
            }
        }
        VaultEvent::Removed(p) => {
            let gone: Vec<DocId> = ws.features.nav.hidden.iter().filter(|d| d.path.as_ref() == Some(p)).map(|d| d.id).collect();
            for id in gone {
                drop_hidden(ws, id);
            }
        }
        VaultEvent::AttachmentChanged(_) | VaultEvent::AttachmentRemoved(_) => {}
    }
}

#[cfg(test)]
mod tests {
    use quire_text::{EditKind, Transaction};

    use super::super::tests::ws_with;
    use super::*;

    #[test]
    fn editing_a_hidden_note_saves_it_with_its_own_undo() {
        let mut ws = ws_with(&[("Host.md", "![[Target]]\n"), ("Target.md", "# T\n\nbody\n")]);
        let host = ws.open_note(&"Host.md".into()).unwrap();
        let t = embed_doc(&mut ws, &NotePath::new("Target.md")).unwrap();
        assert!(t.0 & EMBED_DOC_BIT != 0);
        assert_eq!(embed_doc(&mut ws, &NotePath::new("Target.md")), Some(t), "one document per note");
        let d = embed_doc_mut(&mut ws, t).unwrap();
        d.apply(Transaction::insert(9, "more "), EditKind::Other).unwrap();
        assert!(save_embedded(&mut ws, t));
        assert_eq!(ws.vault().unwrap().read(&"Target.md".into()).unwrap(), "# T\n\nbodymore \n");
        // Undo in the embed undoes the embedded note, not the host.
        assert!(embed_doc_mut(&mut ws, t).unwrap().undo());
        assert_eq!(&*embed_doc_ref(&ws, t).unwrap().text(), "# T\n\nbody\n");
        assert!(!ws.doc(host).unwrap().is_dirty());
        // Autosave.
        ws.tick(Instant::now() + Duration::from_secs(5));
        assert_eq!(ws.vault().unwrap().read(&"Target.md".into()).unwrap(), "# T\n\nbody\n");
    }

    #[test]
    fn review_embed_edits_survive_shutdown() {
        let mut ws = ws_with(&[("Host.md", "![[Target]]\n"), ("Target.md", "one\n")]);
        ws.open_note(&"Host.md".into()).unwrap();
        let t = embed_doc(&mut ws, &NotePath::new("Target.md")).unwrap();
        embed_doc_mut(&mut ws, t).unwrap().apply(Transaction::insert(3, " two"), EditKind::Other).unwrap();
        ws.shutdown();
        assert_eq!(ws.vault().unwrap().read(&"Target.md".into()).unwrap(), "one two\n");
        // "Save all" (and so a vault switch, which saves all first) too.
        embed_doc_mut(&mut ws, t).unwrap().apply(Transaction::insert(7, " three"), EditKind::Other).unwrap();
        ws.save_all();
        assert_eq!(ws.vault().unwrap().read(&"Target.md".into()).unwrap(), "one two three\n");
    }

    #[test]
    fn opening_the_note_hands_over_to_the_tab() {
        let mut ws = ws_with(&[("Target.md", "one\n")]);
        let t = embed_doc(&mut ws, &NotePath::new("Target.md")).unwrap();
        embed_doc_mut(&mut ws, t).unwrap().apply(Transaction::insert(3, " two"), EditKind::Other).unwrap();
        let tab = ws.open_note(&"Target.md".into()).unwrap();
        ws.tick(Instant::now());
        assert_eq!(&*ws.doc(tab).unwrap().text(), "one two\n");
        assert!(ws.features.nav.hidden.is_empty());
        assert_eq!(embed_doc(&mut ws, &NotePath::new("Target.md")), Some(tab), "an open note is edited in its tab");
    }
}
