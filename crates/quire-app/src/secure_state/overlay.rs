// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Notes of unlocked folders in the app's own features (0.12,
//! docs/design/0.12-loose.md §2).
//!
//! While a locked folder is unlocked its notes are in a second, in-memory
//! index (`SecureState::locked_index`, dropped on lock). The vault index
//! never holds them, so agents, MCP, `serve`, background jobs and every
//! cache keep never seeing them. Tasks, the agenda, views and Bases, the
//! graph, backlinks and reminders read [`Workspace::with_notes`]: the vault
//! index joined with that overlay ([`quire_index::Joined`]).

use std::cell::RefCell;
use std::rc::Rc;

use quire_index::{Backlink, CrossLinks, Joined};
use quire_vault::NotePath;

use crate::workspace::Workspace;

/// Links between the two sides, by (vault index version, overlay version).
pub(super) type CrossCache = RefCell<Option<((u64, u64), Rc<CrossLinks>)>>;

impl Workspace {
    /// What the notes the app's features read last changed: the vault
    /// index's version and the overlay's. Caches built from
    /// [`Self::with_notes`] key on it.
    pub fn notes_version(&self) -> (u64, u64) {
        (self.index.version(), self.features.secure.locked_version)
    }

    /// Whether notes of an unlocked folder are in the overlay.
    pub fn has_locked_overlay(&self) -> bool {
        self.features.secure.locked_index.as_ref().is_some_and(|ix| !ix.is_empty())
    }

    /// Read the notes the desktop's own features show: the vault index and,
    /// while a folder is unlocked, its notes. Never hand this to an agent,
    /// MCP, a background job or anything that writes to disk.
    pub fn with_notes<R>(&self, f: impl FnOnce(&Joined<'_>) -> R) -> R {
        let main = self.index.read();
        self.joined_over(&main, f)
    }

    /// [`Self::with_notes`] for a frame: `None` while the index worker holds
    /// the write lock (it never waits; keep what was shown).
    pub fn try_with_notes<R>(&self, f: impl FnOnce(&Joined<'_>) -> R) -> Option<R> {
        let main = self.index.try_read()?;
        Some(self.joined_over(&main, f))
    }

    fn joined_over<R>(&self, main: &quire_index::Index, f: impl FnOnce(&Joined<'_>) -> R) -> R {
        let st = &self.features.secure;
        let Some(extra) = st.locked_index.as_ref().filter(|ix| !ix.is_empty()) else {
            return f(&Joined::main_only(main));
        };
        let key = (self.index.version(), st.locked_version);
        let cached = st.cross.borrow().as_ref().filter(|(k, _)| *k == key).map(|(_, c)| Rc::clone(c));
        let cross = cached.unwrap_or_else(|| {
            let c = Rc::new(CrossLinks::build(main, extra));
            *st.cross.borrow_mut() = Some((key, Rc::clone(&c)));
            c
        });
        f(&Joined::new(main, extra, &cross))
    }

    /// Backlinks of `p` from the notes of unlocked folders, and to a note
    /// of one from plain notes: what the vault index can't give.
    pub fn overlay_backlinks(&self, p: &NotePath) -> Vec<Backlink> {
        if !self.has_locked_overlay() {
            return Vec::new();
        }
        self.with_notes(|n| {
            let main = n.main().backlinks(p).len();
            let mut v = n.backlinks(p);
            v.drain(..main);
            v
        })
    }

    /// Whether `p` is a note of a folder that is unlocked now.
    pub fn in_unlocked_folder(&self, p: &NotePath) -> bool {
        self.vault().and_then(|v| v.locked_folder(p).map(|f| v.keys().is_unlocked(&f.path))).unwrap_or(false)
    }

    /// Drop what was derived from the overlay (a folder locked).
    pub(super) fn forget_overlay_links(&self) {
        self.features.secure.cross.borrow_mut().take();
    }
}
