// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Flashcard review sessions (design §E.6).
//!
//! Cards come from notes tagged `#flashcards` or with `srs: true`. Each
//! answer is appended to this device's log in `.quire/shared/srs/`; card
//! state is replayed from every device's log. When a card's question is
//! edited (it vanishes and a similar one appears in the same note) its
//! history carries over, recorded as a rename in the log.
//!
//! Owned by WS-E; kept in `ws.features.srs`.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use quire_markdown::srs::{self, Card};
use quire_plan::srs::{CardState, Rating, ReviewEvent, SRS_DIR, merge_logs, parse_log, replay, whole_days};
use quire_vault::NotePath;

use crate::workspace::{NoticeLevel, Workspace};

/// New cards brought into one session at most.
pub const NEW_PER_SESSION: usize = 20;
/// Questions at least this alike count as the same card after an edit.
pub const RENAME_SIMILARITY: f64 = 0.8;

/// A card and the note it's in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeckCard {
    pub path: NotePath,
    pub card: Card,
}

/// A review in progress.
#[derive(Debug, Clone)]
pub struct Session {
    pub title: String,
    pub cards: Vec<DeckCard>,
    /// Indexes into `cards`, next first.
    pub queue: VecDeque<usize>,
    pub revealed: bool,
    pub reviewed: u32,
    pub again: u32,
    /// Total at the start, for "3 of 12".
    pub total: usize,
}

impl Session {
    pub fn current(&self) -> Option<&DeckCard> {
        self.queue.front().and_then(|i| self.cards.get(*i))
    }
}

type Loaded = (Vec<ReviewEvent>, HashMap<String, CardState>);

/// State for this feature.
#[derive(Debug, Default)]
pub struct SrsState {
    loaded: bool,
    loading: Option<Arc<Mutex<Option<Loaded>>>>,
    events: Vec<ReviewEvent>,
    pub states: HashMap<String, CardState>,
    cards: RefCell<Option<(u64, Rc<Vec<DeckCard>>)>>,
    pub session: Option<Session>,
    /// Cards per note at the last look, to notice edited questions.
    known: HashMap<NotePath, Vec<(String, String)>>,
    known_version: u64,
}

fn now_ms() -> i64 {
    crate::history_ops::now_ms()
}

/// Whether a note's cards are reviewed.
fn is_deck(r: &quire_index::NoteRecord) -> bool {
    r.has_tag("flashcards")
        || quire_index::views::property(r, "srs").and_then(quire_markdown::Value::as_str).is_some_and(|v| v.eq_ignore_ascii_case("true"))
}

fn log_path(ws: &Workspace) -> NotePath {
    NotePath::new(format!("{SRS_DIR}/{}.jsonl", ws.device().id))
}

/// Every device's log, merged and replayed.
fn read_logs(vault: &quire_vault::Vault, own: &NotePath) -> Loaded {
    let storage = vault.storage();
    let files = storage.list_under(SRS_DIR).unwrap_or_default();
    let mut logs: Vec<Vec<ReviewEvent>> = files
        .iter()
        .filter(|p| std::path::Path::new(p.as_str()).extension().is_some_and(|e| e.eq_ignore_ascii_case("jsonl")))
        .filter_map(|p| storage.read(p).ok())
        .map(|t| parse_log(&t))
        .collect();
    // A log the listing can't see (in-memory vaults): this device's.
    if !files.contains(own)
        && let Ok(t) = storage.read(own)
    {
        logs.push(parse_log(&t));
    }
    let events = merge_logs(logs);
    let states = replay(&events);
    (events, states)
}

/// Read every device's log and replay it (now).
pub(crate) fn load(ws: &mut Workspace) {
    let Some(vault) = ws.vault().cloned() else { return };
    let (events, states) = read_logs(&vault, &log_path(ws));
    let st = &mut ws.features.srs;
    st.states = states;
    st.events = events;
    st.loaded = true;
    st.loading = None;
}

/// Start reading the logs off the UI thread, once.
pub(crate) fn ensure_loaded(ws: &mut Workspace) {
    if ws.features.srs.loaded || ws.features.srs.loading.is_some() {
        return;
    }
    let Some(vault) = ws.vault().cloned() else { return };
    let own = log_path(ws);
    let slot: Arc<Mutex<Option<Loaded>>> = Arc::default();
    let out = Arc::clone(&slot);
    let spawned = std::thread::Builder::new().name("quire-srs-load".into()).spawn(move || {
        let loaded = read_logs(&vault, &own);
        if let Ok(mut s) = out.lock() {
            *s = Some(loaded);
        }
    });
    if spawned.is_ok() {
        ws.features.srs.loading = Some(slot);
    } else {
        load(ws);
    }
}

/// The logs have been read: take them. True when they just arrived.
pub(crate) fn poll_load(ws: &mut Workspace) -> bool {
    let Some(slot) = &ws.features.srs.loading else { return false };
    let Some((events, states)) = slot.lock().ok().and_then(|mut s| s.take()) else { return false };
    let st = &mut ws.features.srs;
    st.events = events;
    st.states = states;
    st.loaded = true;
    st.loading = None;
    ws.features.decor_generation += 1;
    true
}

impl SrsState {
    pub const fn is_loaded(&self) -> bool {
        self.loaded
    }
}

impl Workspace {
    /// Every card in deck notes, cached until the index changes.
    pub fn srs_cards(&self) -> Rc<Vec<DeckCard>> {
        let version = self.index.version();
        if let Some((v, c)) = self.features.srs.cards.borrow().as_ref()
            && *v == version
        {
            return Rc::clone(c);
        }
        let ix = self.index.read();
        let mut out = Vec::new();
        for r in ix.records().filter(|r| is_deck(r)) {
            if let Some(src) = ix.source(&r.path) {
                out.extend(srs::cards(src).into_iter().map(|card| DeckCard { path: r.path.clone(), card }));
            }
        }
        drop(ix);
        out.sort_by(|a, b| a.path.cmp(&b.path).then(a.card.lines.start.cmp(&b.card.lines.start)));
        let out = Rc::new(out);
        *self.features.srs.cards.borrow_mut() = Some((version, Rc::clone(&out)));
        out
    }

    /// (due reviews, new cards) now.
    pub fn srs_due_counts(&self) -> (usize, usize) {
        let end = end_of_today_ms();
        let cards = self.srs_cards();
        let new = cards.iter().filter(|c| !self.features.srs.states.contains_key(&c.card.id)).count();
        let due = cards.iter().filter(|c| self.features.srs.states.get(&c.card.id).is_some_and(|s| s.due <= end)).count();
        (due, new)
    }

    /// Start reviewing: every due card, or one note's. False when nothing is due.
    pub fn srs_start(&mut self, note: Option<&NotePath>) -> bool {
        if !self.features.srs.loaded {
            load(self);
        }
        let end = end_of_today_ms();
        let all = self.srs_cards();
        let cards: Vec<DeckCard> = all.iter().filter(|c| note.is_none_or(|n| &c.path == n)).cloned().collect();
        let (mut due, mut new): (Vec<usize>, Vec<usize>) = (Vec::new(), Vec::new());
        for (i, c) in cards.iter().enumerate() {
            match self.features.srs.states.get(&c.card.id) {
                None => new.push(i),
                Some(s) if s.due <= end => due.push(i),
                Some(_) => {}
            }
        }
        // Most overdue first, then new cards in note order.
        due.sort_by_key(|i| self.features.srs.states.get(&cards[*i].card.id).map_or(0, |s| s.due));
        new.truncate(NEW_PER_SESSION);
        let queue: VecDeque<usize> = due.into_iter().chain(new).collect();
        if queue.is_empty() {
            return false;
        }
        let title = note.map_or_else(|| "Flashcards".to_string(), |n| n.stem().to_string());
        let total = queue.len();
        self.features.srs.session = Some(Session { title, cards, queue, revealed: false, reviewed: 0, again: 0, total });
        true
    }

    /// When the current card would come back for each answer (ms from now).
    pub fn srs_preview(&self) -> Option<[i64; 4]> {
        let c = self.features.srs.session.as_ref()?.current()?;
        Some(quire_plan::srs::preview(&c.card.id, self.features.srs.states.get(&c.card.id), now_ms()))
    }

    pub fn srs_reveal(&mut self) {
        if let Some(s) = &mut self.features.srs.session {
            s.revealed = true;
        }
    }

    /// Answer the current card: log it, update its state, move on.
    pub fn srs_answer(&mut self, rating: Rating) {
        let Some(card) = self.features.srs.session.as_ref().and_then(Session::current).cloned() else { return };
        let at = now_ms();
        let prev = self.features.srs.states.get(&card.card.id).copied();
        let elapsed = prev.map_or(0, |p| whole_days(p.last_review, at).max(0)) as f64;
        let ev = ReviewEvent::review(&card.card.id, card.path.as_str(), at, rating, elapsed, &self.device().id);
        if let Some(vault) = self.vault()
            && let Err(e) = vault.storage().append_line(&log_path(self), &ev.to_line())
        {
            self.notify(NoticeLevel::Error, format!("Couldn't save the review: {e}"));
            return;
        }
        let next = quire_plan::srs::review(&card.card.id, prev.as_ref(), at, rating);
        self.features.srs.states.insert(card.card.id, next);
        self.features.srs.events.push(ev);
        if let Some(s) = &mut self.features.srs.session {
            let i = s.queue.pop_front();
            s.revealed = false;
            s.reviewed += 1;
            if rating == Rating::Again {
                s.again += 1;
                // Back at the end of this session.
                if let Some(i) = i {
                    s.queue.push_back(i);
                }
            }
        }
    }

    /// Stop reviewing.
    pub fn srs_end(&mut self) {
        if let Some(s) = self.features.srs.session.take()
            && s.reviewed > 0
        {
            self.notify(NoticeLevel::Info, format!("Reviewed {} card{}", s.reviewed, if s.reviewed == 1 { "" } else { "s" }));
        }
        self.features.tasks.dialog = None;
    }

    /// Give the active note's cards `^id`s, keeping their history.
    pub fn srs_add_ids(&mut self) -> usize {
        let Some(doc) = self.active_doc() else { return 0 };
        let (text, path) = (doc.text().to_string(), doc.path.clone());
        let add = srs::add_ids(&text);
        if add.inserts.is_empty() {
            return 0;
        }
        let at = now_ms();
        let device = self.device().id.clone();
        let note = path.as_ref().map(|p| p.as_str().to_string()).unwrap_or_default();
        for (old, new) in &add.renames {
            if self.features.srs.states.contains_key(old) {
                let ev = ReviewEvent::rename(old, new, &note, at, &device);
                if let Some(vault) = self.vault() {
                    let _ = vault.storage().append_line(&log_path(self), &ev.to_line());
                }
                if let Some(s) = self.features.srs.states.remove(old) {
                    self.features.srs.states.insert(new.clone(), s);
                }
            }
        }
        let n = add.inserts.len();
        if let Some(d) = self.active_doc_mut() {
            d.replace_minimally(&add.apply(&text));
        }
        n
    }
}

/// Unix ms at the end of today, local time.
pub fn end_of_today_ms() -> i64 {
    let today = chrono::Local::now().date_naive();
    today.and_hms_opt(23, 59, 59).and_then(|t| t.and_local_timezone(chrono::Local).single()).map_or_else(now_ms, |t| t.timestamp_millis())
}

/// After the index changes: carry history over to edited questions.
pub(crate) fn track_edits(ws: &mut Workspace) {
    let version = ws.index.version();
    if !ws.features.srs.loaded || ws.features.srs.known_version == version {
        return;
    }
    ws.features.srs.known_version = version;
    let cards = ws.srs_cards();
    let mut now: HashMap<NotePath, Vec<(String, String)>> = HashMap::new();
    for c in cards.iter() {
        now.entry(c.path.clone()).or_default().push((c.card.id.clone(), c.card.question.clone()));
    }
    let mut renames = Vec::new();
    for (path, cur) in &now {
        let Some(old) = ws.features.srs.known.get(path) else { continue };
        let cur_ids: HashSet<&str> = cur.iter().map(|c| c.0.as_str()).collect();
        let old_ids: HashSet<&str> = old.iter().map(|c| c.0.as_str()).collect();
        let gone: Vec<&(String, String)> =
            old.iter().filter(|(id, _)| !cur_ids.contains(id.as_str()) && ws.features.srs.states.contains_key(id)).collect();
        let mut fresh: Vec<&(String, String)> =
            cur.iter().filter(|(id, _)| !old_ids.contains(id.as_str()) && !ws.features.srs.states.contains_key(id)).collect();
        for (gid, gq) in gone {
            let best = fresh.iter().enumerate().map(|(i, (_, q))| (i, srs::similarity(gq, q))).max_by(|a, b| a.1.total_cmp(&b.1));
            if let Some((i, sim)) = best.filter(|b| b.1 >= RENAME_SIMILARITY) {
                let _ = sim;
                renames.push((path.clone(), gid.clone(), fresh.remove(i).0.clone()));
            }
        }
    }
    ws.features.srs.known = now;
    if renames.is_empty() {
        return;
    }
    let at = now_ms();
    let device = ws.device().id.clone();
    for (path, from, to) in renames {
        let ev = ReviewEvent::rename(&from, &to, path.as_str(), at, &device);
        if let Some(vault) = ws.vault() {
            let _ = vault.storage().append_line(&log_path(ws), &ev.to_line());
        }
        if let Some(s) = ws.features.srs.states.remove(&from) {
            ws.features.srs.states.insert(to, s);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ws() -> Workspace {
        let mut w = Workspace::new(crate::Settings::default());
        w.attach_vault(quire_vault::Vault::in_memory(&[
            ("Bio.md", "---\nsrs: true\n---\nWhat is ATP? :: Energy currency\nThe ==nucleus== holds DNA.\n"),
            ("Other.md", "Not :: a deck\n"),
        ]));
        w.index().flush();
        load(&mut w);
        w
    }

    #[test]
    fn review_logs_and_replays() {
        let mut w = ws();
        assert_eq!(w.srs_due_counts(), (0, 2));
        assert!(w.srs_start(None));
        assert_eq!(w.features.srs.session.as_ref().unwrap().total, 2);
        assert!(w.srs_preview().unwrap()[0] < w.srs_preview().unwrap()[3]);
        w.srs_reveal();
        w.srs_answer(Rating::Good);
        w.srs_answer(Rating::Again);
        let s = w.features.srs.session.as_ref().unwrap();
        assert_eq!((s.reviewed, s.queue.len()), (2, 1), "Again comes back in the session");
        w.srs_answer(Rating::Good);
        assert!(w.features.srs.session.as_ref().unwrap().queue.is_empty());
        w.srs_end();
        let log = w.vault().unwrap().read(&log_path(&w)).unwrap();
        assert_eq!(log.lines().count(), 3);
        // Replaying the log gives the same states.
        let before = w.features.srs.states.clone();
        load(&mut w);
        assert_eq!(w.features.srs.states, before);
        assert!(!w.srs_start(None), "nothing due right after reviewing");
    }

    #[test]
    fn edited_questions_keep_their_history() {
        let mut w = ws();
        track_edits(&mut w);
        w.srs_start(None);
        w.srs_answer(Rating::Easy);
        let id = w.srs_cards()[0].card.id.clone();
        w.index().upsert("Bio.md".into(), "---\nsrs: true\n---\nWhat is ATP?? :: Energy currency\nThe ==nucleus== holds DNA.\n".into());
        w.index().flush();
        track_edits(&mut w);
        let new_id = w.srs_cards()[0].card.id.clone();
        assert_ne!(id, new_id);
        assert!(w.features.srs.states.contains_key(&new_id) && !w.features.srs.states.contains_key(&id));
        load(&mut w);
        assert!(w.features.srs.states.contains_key(&new_id), "the rename is in the log");
    }
}
