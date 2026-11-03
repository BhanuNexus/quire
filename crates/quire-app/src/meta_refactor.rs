// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Tag and property refactors with preview (design §E.1).
//!
//! [`plan`] works out every note's new text for a [`MetaOp`] (pure, also
//! used by the CLI). The workspace previews it off the UI thread, applies
//! the ticked notes as one batch (open notes get an undoable edit, closed
//! notes an atomic write with history before and after), records the batch
//! in the ledger (actor: you), and can undo it: a note still as the batch
//! left it gets its old text back; a note edited since gets a three-way
//! merge that keeps the later edits, or is left alone if they conflict.
//!
//! Owned by WS-E; kept in `ws.features.meta`.

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::sync::{Arc, Mutex};

use quire_markdown::frontmatter::{self, ItemChange, Value};
use quire_markdown::{Inline, scan_inlines};
use quire_vault::{ContentHash, NotePath};

use crate::workspace::{NoticeLevel, Workspace};

/// One refactor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MetaOp {
    /// `#a` → `#b`, nested tags too (`#a/x` → `#b/x`).
    RenameTag { from: String, to: String },
    /// Several tags (and their nested tags) become one.
    MergeTags { from: Vec<String>, into: String },
    /// Remove `#tag` from text and front matter (`#tag/x` stays).
    DeleteTag { tag: String },
    /// A property key (dotted for nested ones). An existing target is merged.
    RenameKey { from: String, to: String },
    /// Several keys become one; their values are combined.
    MergeKeys { from: Vec<String>, into: String },
    /// One value of a key (in lists too).
    RenameValue { key: String, from: String, to: String },
    /// A property key, with its value.
    DeleteKey { key: String },
}

impl MetaOp {
    /// "Rename tag #a to #b", for the preview, the ledger and undo.
    pub fn title(&self) -> String {
        match self {
            Self::RenameTag { from, to } => format!("Rename tag #{from} to #{to}"),
            Self::MergeTags { from, into } => {
                format!("Merge {} into #{into}", from.iter().map(|t| format!("#{t}")).collect::<Vec<_>>().join(", "))
            }
            Self::DeleteTag { tag } => format!("Delete tag #{tag}"),
            Self::RenameKey { from, to } => format!("Rename property {from} to {to}"),
            Self::MergeKeys { from, into } => format!("Merge properties {} into {into}", from.join(", ")),
            Self::RenameValue { key, from, to } => format!("Change {key}: {from} to {to}"),
            Self::DeleteKey { key } => format!("Delete property {key}"),
        }
    }

    pub const fn is_tag_op(&self) -> bool {
        matches!(self, Self::RenameTag { .. } | Self::MergeTags { .. } | Self::DeleteTag { .. })
    }

    /// Why it can't run, if it can't.
    pub fn problem(&self) -> Option<String> {
        let tag_ok = |t: &str| {
            !t.is_empty()
                && t.chars().all(|c| c.is_alphanumeric() || matches!(c, '_' | '-' | '/'))
                && !t.chars().all(|c| c.is_ascii_digit())
        };
        let key_ok = |k: &str| {
            !k.trim().is_empty() && !k.contains([':', '#', '[', ']', '{', '}', ',']) && !k.split('.').any(|p| p.trim().is_empty())
        };
        match self {
            Self::RenameTag { from, to } if !tag_ok(to) || from.is_empty() => {
                Some("Tags are letters, digits, _ - and / (not only digits).".into())
            }
            Self::RenameTag { from, to } if norm_tag(from) == norm_tag(to) && from == to => Some("That's the same tag.".into()),
            Self::MergeTags { from, into } if !tag_ok(into) || from.is_empty() => {
                Some("Pick tags to merge and a tag to merge them into.".into())
            }
            Self::RenameKey { to, .. } if !key_ok(to) => Some("A property name can't be empty or contain : # [ ] { } or ,".into()),
            Self::RenameKey { from, to } if from == to => Some("That's the same name.".into()),
            Self::MergeKeys { from, into } if !key_ok(into) || from.is_empty() => {
                Some("Pick properties to merge and a name to merge them into.".into())
            }
            Self::RenameValue { from, to, .. } if from == to => Some("That's the same value.".into()),
            _ => None,
        }
    }
}

fn norm_tag(t: &str) -> String {
    t.trim().trim_start_matches('#').to_lowercase()
}

/// `tag` is `t` or nested under it.
fn tag_matches(tag: &str, t: &str) -> bool {
    let (tag, t) = (norm_tag(tag), norm_tag(t));
    tag == t || tag.strip_prefix(&t).is_some_and(|r| r.starts_with('/'))
}

/// What `op` does to one tag.
enum Retag {
    Keep,
    To(String),
    Remove,
}

fn retag(tag: &str, op: &MetaOp) -> Retag {
    let bare = tag.trim_start_matches('#');
    match op {
        MetaOp::RenameTag { from, to } if tag_matches(bare, from) => {
            let rest = &bare[norm_tag(from).len().min(bare.len())..];
            Retag::To(format!("{}{rest}", to.trim_start_matches('#')))
        }
        MetaOp::MergeTags { from, into } => match from.iter().find(|f| tag_matches(bare, f)) {
            Some(f) => {
                let rest = &bare[norm_tag(f).len().min(bare.len())..];
                Retag::To(format!("{}{rest}", into.trim_start_matches('#')))
            }
            None => Retag::Keep,
        },
        MetaOp::DeleteTag { tag: t } if norm_tag(bare) == norm_tag(t) => Retag::Remove,
        _ => Retag::Keep,
    }
}

/// A note's new text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileEdit {
    pub path: NotePath,
    pub before: String,
    pub after: String,
    /// How many places changed.
    pub changes: usize,
}

impl FileEdit {
    /// Changed lines, for the preview: (removed, added) pairs per hunk.
    pub fn diff(&self, max: usize) -> Vec<(Vec<String>, Vec<String>)> {
        use similar::{ChangeTag, TextDiff};
        let d = TextDiff::from_lines(&self.before, &self.after);
        let mut out = Vec::new();
        for group in d.grouped_ops(0) {
            let (mut gone, mut new) = (Vec::new(), Vec::new());
            for op in group {
                for c in d.iter_changes(&op) {
                    let l = c.value().trim_end_matches(['\n', '\r']).to_string();
                    match c.tag() {
                        ChangeTag::Delete => gone.push(l),
                        ChangeTag::Insert => new.push(l),
                        ChangeTag::Equal => {}
                    }
                }
            }
            out.push((gone, new));
            if out.len() >= max {
                break;
            }
        }
        out
    }
}

/// Apply front-matter edits one at a time (each re-parses, so ranges stay right).
fn apply_fm(text: &mut String, edit: Option<frontmatter::Edit>, n: &mut usize) {
    if let Some(e) = edit {
        *text = e.apply(text);
        *n += 1;
    }
}

/// A tag op on one note.
fn retag_note(src: &str, op: &MetaOp) -> (String, usize) {
    let mut n = 0;
    // Inline tags, from the end so ranges stay valid.
    let mut out = src.to_string();
    let tags: Vec<(std::ops::Range<usize>, String)> = scan_inlines(src)
        .into_iter()
        .filter_map(|i| match i {
            Inline::Tag { range, name } => Some((range, name)),
            _ => None,
        })
        .collect();
    for (range, name) in tags.into_iter().rev() {
        match retag(&name, op) {
            Retag::Keep => continue,
            Retag::To(new) => out.replace_range(range, &format!("#{new}")),
            Retag::Remove => {
                // Take one neighbouring space so no double space is left.
                let (mut a, mut b) = (range.start, range.end);
                if out[..a].ends_with(' ') && (out[b..].starts_with([' ', '\n', '\r']) || b == out.len()) {
                    a -= 1;
                } else if out[b..].starts_with(' ') {
                    b += 1;
                }
                out.replace_range(a..b, "");
            }
        }
        n += 1;
    }
    // Front matter `tags:` / `tag:` lists. A tag renamed onto one already
    // listed is dropped, so the list doesn't repeat it.
    let target = norm_tag(target_tag(op));
    for key in ["tags", "tag"] {
        let mut seen: HashSet<String> = HashSet::new();
        let edit = frontmatter::edit_list_items(&out, &[key], |item| {
            let bare = item.trim_start_matches('#');
            match retag(bare, op) {
                Retag::Remove => ItemChange::Remove,
                Retag::To(new) => {
                    if seen.insert(norm_tag(&new)) {
                        ItemChange::Replace(if item.starts_with('#') { format!("#{new}") } else { new })
                    } else {
                        ItemChange::Remove
                    }
                }
                Retag::Keep => {
                    let k = norm_tag(bare);
                    if !target.is_empty() && k == target && seen.contains(&k) {
                        ItemChange::Remove
                    } else {
                        seen.insert(k);
                        ItemChange::Keep
                    }
                }
            }
        });
        apply_fm(&mut out, edit, &mut n);
    }
    (out, n)
}

fn target_tag(op: &MetaOp) -> &str {
    match op {
        MetaOp::RenameTag { to, .. } => to,
        MetaOp::MergeTags { into, .. } => into,
        _ => "",
    }
}

/// The key path in a note matching `dotted` (case-insensitive), as written.
fn find_key(src: &str, dotted: &str) -> Option<Vec<String>> {
    let want: Vec<String> = dotted.split('.').map(|p| p.trim().to_lowercase()).collect();
    frontmatter::key_paths(src).into_iter().find(|p| p.len() == want.len() && p.iter().zip(&want).all(|(a, b)| a.to_lowercase() == *b))
}

fn get_value(src: &str, path: &[String]) -> Option<Value> {
    let fm = frontmatter::parse(src)?;
    let (head, rest) = path.split_first()?;
    let refs: Vec<&str> = rest.iter().map(String::as_str).collect();
    fm.get(head)?.at_path(&refs).cloned()
}

/// Values combined: equal ones once, otherwise a list.
fn combine(values: &[Value]) -> Value {
    let mut items: Vec<String> = Vec::new();
    let mut all_scalar = true;
    for v in values {
        match v {
            Value::Str(s) => {
                if !s.is_empty() && !items.iter().any(|i| i.eq_ignore_ascii_case(s)) {
                    items.push(s.clone());
                }
            }
            Value::List(l) => {
                all_scalar = false;
                for s in l {
                    if !items.iter().any(|i| i.eq_ignore_ascii_case(s)) {
                        items.push(s.clone());
                    }
                }
            }
            Value::Map(_) => return v.clone(),
        }
    }
    if all_scalar && items.len() <= 1 { Value::Str(items.pop().unwrap_or_default()) } else { Value::List(items) }
}

/// A property op on one note.
fn reprop_note(src: &str, op: &MetaOp) -> (String, usize) {
    let mut out = src.to_string();
    let mut n = 0;
    match op {
        MetaOp::RenameKey { from, to } => {
            let Some(path) = find_key(&out, from) else { return (out, 0) };
            let target: Vec<String> = to.split('.').map(|p| p.trim().to_string()).collect();
            let same_parent =
                path.len() == target.len() && path[..path.len() - 1].iter().zip(&target).all(|(a, b)| a.eq_ignore_ascii_case(b));
            if find_key(&out, to).is_some() || !same_parent {
                // Onto an existing key, or into another map: merge.
                return reprop_note(src, &MetaOp::MergeKeys { from: vec![from.clone()], into: to.clone() });
            }
            let p: Vec<&str> = path.iter().map(String::as_str).collect();
            {
                let e = frontmatter::rename_field_path(&out, &p, target.last().map_or("", String::as_str));
                apply_fm(&mut out, e, &mut n);
            }
        }
        MetaOp::MergeKeys { from, into } => {
            let target = find_key(&out, into).unwrap_or_else(|| into.split('.').map(|p| p.trim().to_string()).collect());
            let mut values: Vec<Value> = get_value(&out, &target).into_iter().collect();
            let mut found = false;
            for f in from {
                if let Some(p) = find_key(&out, f).filter(|p| *p != target) {
                    values.extend(get_value(&out, &p));
                    let pr: Vec<&str> = p.iter().map(String::as_str).collect();
                    {
                        let e = frontmatter::remove_field_path(&out, &pr);
                        apply_fm(&mut out, e, &mut n);
                    }
                    found = true;
                }
            }
            if found {
                let t: Vec<&str> = target.iter().map(String::as_str).collect();
                out = frontmatter::set_field_path(&out, &t, &combine(&values)).apply(&out);
            }
        }
        MetaOp::RenameValue { key, from, to } => {
            let Some(path) = find_key(&out, key) else { return (out, 0) };
            let p: Vec<&str> = path.iter().map(String::as_str).collect();
            match get_value(&out, &path) {
                Some(Value::Str(s)) if s.trim().eq_ignore_ascii_case(from.trim()) => {
                    out = frontmatter::set_field_path(&out, &p, &Value::Str(to.clone())).apply(&out);
                    n += 1;
                }
                Some(Value::List(_) | Value::Str(_)) => {
                    let edit = frontmatter::edit_list_items(&out, &p, |item| {
                        if item.trim().eq_ignore_ascii_case(from.trim()) { ItemChange::Replace(to.clone()) } else { ItemChange::Keep }
                    });
                    apply_fm(&mut out, edit, &mut n);
                }
                _ => {}
            }
        }
        MetaOp::DeleteKey { key } => {
            if let Some(path) = find_key(&out, key) {
                let p: Vec<&str> = path.iter().map(String::as_str).collect();
                {
                    let e = frontmatter::remove_field_path(&out, &p);
                    apply_fm(&mut out, e, &mut n);
                }
            }
        }
        _ => {}
    }
    (out, n)
}

/// One note under `op`; `None` when nothing changes.
pub fn apply_to(path: &NotePath, src: &str, op: &MetaOp) -> Option<FileEdit> {
    let (after, changes) = if op.is_tag_op() { retag_note(src, op) } else { reprop_note(src, op) };
    (after != src).then(|| FileEdit { path: path.clone(), before: src.to_string(), after, changes: changes.max(1) })
}

/// Every note's edit under `op`, in path order.
pub fn plan<'a>(notes: impl IntoIterator<Item = (&'a NotePath, &'a str)>, op: &MetaOp) -> Vec<FileEdit> {
    let mut v: Vec<FileEdit> = notes.into_iter().filter_map(|(p, src)| apply_to(p, src, op)).collect();
    v.sort_by(|a, b| a.path.cmp(&b.path));
    v
}

// ------------------------------------------------------------------ app state

/// A preview being worked out, or ready.
#[derive(Debug, Clone)]
pub struct Preview {
    pub op: MetaOp,
    /// `None` while it's being worked out.
    pub edits: Option<Arc<Vec<FileEdit>>>,
    /// Milliseconds the plan took (shown when slow).
    pub ms: u128,
}

/// A batch that was applied, for undo.
#[derive(Debug, Clone)]
pub struct Batch {
    pub id: String,
    pub title: String,
    pub at_ms: i64,
    /// (path, text before, text after).
    pub files: Vec<(NotePath, String, String)>,
}

/// What undo did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UndoReport {
    pub restored: usize,
    /// Notes edited since in ways that can't be merged: left as they are.
    pub conflicts: Vec<NotePath>,
}

type PlanSlot = Arc<Mutex<Option<(Vec<FileEdit>, u128)>>>;

/// State for this feature.
#[derive(Debug, Default)]
pub struct MetaState {
    pub preview: Option<Preview>,
    slot: Option<PlanSlot>,
    /// Batches applied this session, newest last.
    pub batches: Vec<Batch>,
}

impl MetaState {
    /// Work the plan out yet? Called every tick; true when it just arrived.
    pub(crate) fn poll(&mut self) -> bool {
        let Some(slot) = &self.slot else { return false };
        let Some((edits, ms)) = slot.lock().ok().and_then(|mut s| s.take()) else { return false };
        if let Some(p) = &mut self.preview {
            p.edits = Some(Arc::new(edits));
            p.ms = ms;
        }
        self.slot = None;
        true
    }

    pub const fn busy(&self) -> bool {
        self.slot.is_some()
    }
}

impl Workspace {
    /// Every tag with how many notes use it, nested tags included, by name.
    pub fn all_tags(&self) -> Vec<(String, usize)> {
        let mut v = self.index.read().tags();
        v.sort_by_key(|a| a.0.to_lowercase());
        v
    }

    /// Every front-matter key (dotted for nested ones) with how many notes have it.
    pub fn all_property_keys(&self) -> Vec<(String, usize)> {
        let ix = self.index.read();
        let mut counts: HashMap<String, (String, usize)> = HashMap::new();
        for r in ix.records() {
            let mut seen = HashSet::new();
            for (k, v) in &r.properties {
                for (sub, _) in v.leaves() {
                    let name = if sub.is_empty() { k.clone() } else { format!("{k}.{sub}") };
                    for prefix in prefixes(&name) {
                        if seen.insert(prefix.to_lowercase()) {
                            counts.entry(prefix.to_lowercase()).or_insert_with(|| (prefix.clone(), 0)).1 += 1;
                        }
                    }
                }
            }
        }
        let mut v: Vec<(String, usize)> = counts.into_values().collect();
        v.sort_by_key(|a| a.0.to_lowercase());
        v
    }

    /// Values of a key across notes, with counts (for "rename value").
    pub fn property_values(&self, key: &str) -> Vec<(String, usize)> {
        let ix = self.index.read();
        let mut counts: HashMap<String, (String, usize)> = HashMap::new();
        for r in ix.records() {
            if let Some(v) = quire_index::views::property(r, key) {
                let items = match v {
                    Value::List(l) => l.clone(),
                    Value::Str(s) => vec![s.clone()],
                    Value::Map(_) => Vec::new(),
                };
                for i in items.into_iter().filter(|i| !i.trim().is_empty()) {
                    counts.entry(i.to_lowercase()).or_insert_with(|| (i.clone(), 0)).1 += 1;
                }
            }
        }
        let mut v: Vec<(String, usize)> = counts.into_values().collect();
        v.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        v
    }

    /// Current text of a note: the open document's, else the file's (not
    /// the index's copy, which can lag behind a change made elsewhere).
    fn note_text(&self, p: &NotePath) -> Option<String> {
        if let Some(d) = self.docs.iter().find(|d| d.path.as_ref() == Some(p)) {
            return Some(d.text().to_string());
        }
        self.vault.as_ref()?.read(p).ok()
    }

    /// Start working out what `op` would change (off the UI thread).
    pub fn meta_preview(&mut self, op: MetaOp) {
        if let Some(why) = op.problem() {
            self.notify(NoticeLevel::Warn, why);
            return;
        }
        // Copy only the notes that can be affected, under a short lock.
        let texts: Vec<(NotePath, String)> = {
            let ix = self.index.read();
            let wanted: Vec<(NotePath, String)> = ix
                .records()
                .filter(|r| match &op {
                    MetaOp::RenameTag { from, .. } => r.tags.iter().any(|t| tag_matches(t, from)),
                    MetaOp::MergeTags { from, .. } => r.tags.iter().any(|t| from.iter().any(|f| tag_matches(t, f))),
                    MetaOp::DeleteTag { tag } => r.tags.iter().any(|t| norm_tag(t) == norm_tag(tag)),
                    MetaOp::RenameKey { from, .. } => quire_index::views::property(r, from).is_some(),
                    MetaOp::MergeKeys { from, .. } => from.iter().any(|f| quire_index::views::property(r, f).is_some()),
                    MetaOp::RenameValue { key, .. } | MetaOp::DeleteKey { key } => quire_index::views::property(r, key).is_some(),
                })
                .filter_map(|r| ix.source(&r.path).map(|s| (r.path.clone(), s.to_string())))
                .collect();
            wanted
        };
        // Open notes may have unsaved edits: use those.
        let texts: Vec<(NotePath, String)> = texts
            .into_iter()
            .map(|(p, t)| (p.clone(), self.docs.iter().find(|d| d.path.as_ref() == Some(&p)).map_or(t, |d| d.text().to_string())))
            .collect();
        let slot: PlanSlot = Arc::default();
        let out = Arc::clone(&slot);
        let op2 = op.clone();
        let started = std::time::Instant::now();
        let spawned = std::thread::Builder::new().name("quire-meta-plan".into()).spawn(move || {
            let edits = plan(texts.iter().map(|(p, t)| (p, t.as_str())), &op2);
            if let Ok(mut s) = out.lock() {
                *s = Some((edits, started.elapsed().as_millis()));
            }
        });
        if spawned.is_err() {
            self.notify(NoticeLevel::Error, "Couldn't start working out the changes");
            return;
        }
        self.features.meta.preview = Some(Preview { op, edits: None, ms: 0 });
        self.features.meta.slot = Some(slot);
    }

    /// Wait for the preview (tests and the CLI).
    pub fn meta_preview_wait(&mut self) -> Option<Arc<Vec<FileEdit>>> {
        for _ in 0..2000 {
            self.features.meta.poll();
            if let Some(e) = self.features.meta.preview.as_ref().and_then(|p| p.edits.clone()) {
                return Some(e);
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        None
    }

    /// Write `text` to a note: an undoable edit if it's open, else an
    /// atomic write with history before and after. Returns the snapshot
    /// times (before, after) for closed notes.
    pub(crate) fn write_note_with_history(
        &mut self,
        path: &NotePath,
        before: &str,
        text: &str,
        now: i64,
    ) -> Result<Option<(i64, i64)>, String> {
        let vault = self.vault.clone().ok_or("No vault is open")?;
        // Both versions go to history, the new one with provenance (a batch
        // the person ran), so the batch can be undone later from the ledger.
        let device = self.device().id.clone();
        let prov = |text: &str| quire_agent::Provenance::new(quire_agent::Actor::human(&device), quire_agent::Op::Write, text, now);
        self.history.keep(&vault, path, before, now - 1, true);
        if let Some(i) = self.docs.iter().position(|d| d.path.as_ref() == Some(path)) {
            let p = prov(text);
            self.docs[i].replace_minimally(text);
            self.docs[i].next_save_provenance = Some(p);
            let id = self.docs[i].id;
            if !self.save(id) {
                return Err(format!("Couldn't save {}", path.stem()));
            }
            return Ok(None);
        }
        vault.write(path, text).map_err(|e| format!("Couldn't write {}: {e}", path.stem()))?;
        self.history.keep_with(&vault, path, text, now, true, Some(prov(text).to_json()));
        self.index.upsert(path.clone(), text.to_string());
        Ok(Some((now - 1, now)))
    }

    /// Apply the previewed refactor to every note not in `skip`. Notes that
    /// changed since the preview are worked out again from their current text.
    pub fn meta_apply(&mut self, skip: &HashSet<NotePath>) -> Option<Batch> {
        let preview = self.features.meta.preview.take()?;
        let edits = preview.edits?;
        let now = crate::history_ops::now_ms();
        let id = quire_agent::BatchId::new(now).as_str().to_string();
        let title = preview.op.title();
        let mut files = Vec::new();
        let mut failed = 0;
        for e in edits.iter().filter(|e| !skip.contains(&e.path)) {
            let Some(current) = self.note_text(&e.path) else { continue };
            let after = if current == e.before {
                e.after.clone()
            } else {
                apply_to(&e.path, &current, &preview.op).map_or_else(|| current.clone(), |x| x.after)
            };
            if after == current {
                continue;
            }
            match self.write_note_with_history(&e.path, &current, &after, now) {
                Ok(snaps) => {
                    self.record_batch_entry(&id, &title, &e.path, &current, &after, snaps, now);
                    files.push((e.path.clone(), current, after));
                }
                Err(msg) => {
                    failed += 1;
                    self.notify(NoticeLevel::Error, msg);
                }
            }
        }
        self.index.flush();
        let n = files.len();
        let batch = Batch { id, title: title.clone(), at_ms: now, files };
        if n > 0 {
            self.features.meta.batches.push(batch.clone());
        }
        let extra = if failed > 0 { format!(" ({failed} couldn't be changed)") } else { String::new() };
        self.notify(NoticeLevel::Info, format!("{title}: {n} note{} changed{extra}", if n == 1 { "" } else { "s" }));
        Some(batch)
    }

    fn record_batch_entry(
        &self,
        batch: &str,
        title: &str,
        path: &NotePath,
        before: &str,
        after: &str,
        snaps: Option<(i64, i64)>,
        now: i64,
    ) {
        let Some(store) = self.agent_store() else { return };
        let mut e =
            quire_agent::ledger::LedgerEntry::new(quire_agent::Actor::human(&self.device().id), quire_agent::Op::Write, now).path(path);
        e.before = Some(ContentHash::of(before));
        e.after = Some(ContentHash::of(after));
        e.snap_before = snaps.map(|s| s.0);
        e.snap_after = snaps.map(|s| s.1);
        e.batch = Some(batch.to_string());
        e.bytes = Some(after.len() as u64);
        e.other.insert("title".into(), serde_json::Value::String(title.to_string()));
        store.record(&e);
    }

    /// Undo a batch applied this session.
    pub fn meta_undo(&mut self, id: &str) -> UndoReport {
        let Some(pos) = self.features.meta.batches.iter().position(|b| b.id == id) else { return UndoReport::default() };
        let batch = self.features.meta.batches.remove(pos);
        let now = crate::history_ops::now_ms();
        let undo_id = quire_agent::BatchId::new(now).as_str().to_string();
        let title = format!("Undo: {}", batch.title);
        let mut report = UndoReport::default();
        for (path, before, after) in &batch.files {
            let Some(current) = self.note_text(path) else { continue };
            let target = if current == *after {
                before.clone()
            } else {
                // Keep what was written since: undo only the batch's change.
                let m = quire_text::merge::merge3(after, &current, before);
                if m.conflicts > 0 {
                    report.conflicts.push(path.clone());
                    continue;
                }
                m.text
            };
            if target == current {
                continue;
            }
            match self.write_note_with_history(path, &current, &target, now) {
                Ok(snaps) => {
                    self.record_batch_entry(&undo_id, &title, path, &current, &target, snaps, now);
                    report.restored += 1;
                }
                Err(_) => report.conflicts.push(path.clone()),
            }
        }
        self.index.flush();
        let mut msg = format!("{title}: {} note{} restored", report.restored, if report.restored == 1 { "" } else { "s" });
        if !report.conflicts.is_empty() {
            let _ = write!(msg, "; {} left as they are (edited since)", report.conflicts.len());
        }
        self.notify(if report.conflicts.is_empty() { NoticeLevel::Info } else { NoticeLevel::Warn }, msg);
        report
    }
}

/// `a`, `a.b`, `a.b.c` for `a.b.c`.
fn prefixes(dotted: &str) -> Vec<String> {
    let parts: Vec<&str> = dotted.split('.').collect();
    (1..=parts.len()).map(|n| parts[..n].join(".")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edit(src: &str, op: &MetaOp) -> String {
        apply_to(&"N.md".into(), src, op).map_or_else(|| src.to_string(), |e| e.after)
    }

    #[test]
    fn tag_rename_merge_delete() {
        let src = "---\ntags: [project, project/alpha, misc]\n---\n# Plan #project\nSee #project/alpha and `#project` and #projects.\nhttp://x.com/#project\n";
        let out = edit(src, &MetaOp::RenameTag { from: "project".into(), to: "work".into() });
        assert_eq!(
            out,
            "---\ntags: [work, work/alpha, misc]\n---\n# Plan #work\nSee #work/alpha and `#project` and #projects.\nhttp://x.com/#project\n"
        );
        let out = edit(src, &MetaOp::MergeTags { from: vec!["misc".into(), "project/alpha".into()], into: "project".into() });
        assert_eq!(out, "---\ntags: [project]\n---\n# Plan #project\nSee #project and `#project` and #projects.\nhttp://x.com/#project\n");
        let out = edit(src, &MetaOp::DeleteTag { tag: "project".into() });
        assert_eq!(
            out,
            "---\ntags: [project/alpha, misc]\n---\n# Plan\nSee #project/alpha and `#project` and #projects.\nhttp://x.com/#project\n"
        );
        let block = "---\ntags:\n  - Project\n  - x\n---\n";
        assert_eq!(edit(block, &MetaOp::RenameTag { from: "project".into(), to: "work".into() }), "---\ntags:\n  - work\n  - x\n---\n");
    }

    #[test]
    fn property_ops() {
        let src = "---\nstatus: Active\nstate: done\nauthor:\n  name: Ada\nkind: [a, b]\n---\nbody\n";
        assert_eq!(
            edit(src, &MetaOp::RenameKey { from: "status".into(), to: "stage".into() }),
            "---\nstage: Active\nstate: done\nauthor:\n  name: Ada\nkind: [a, b]\n---\nbody\n"
        );
        assert_eq!(
            edit(src, &MetaOp::RenameKey { from: "author.name".into(), to: "author.full".into() }),
            "---\nstatus: Active\nstate: done\nauthor:\n  full: Ada\nkind: [a, b]\n---\nbody\n"
        );
        let merged = edit(src, &MetaOp::MergeKeys { from: vec!["state".into()], into: "status".into() });
        assert_eq!(merged, "---\nstatus: [Active, done]\nauthor:\n  name: Ada\nkind: [a, b]\n---\nbody\n");
        let renamed_onto = edit(src, &MetaOp::RenameKey { from: "state".into(), to: "status".into() });
        assert_eq!(renamed_onto, merged, "renaming onto an existing key merges");
        assert_eq!(
            edit(src, &MetaOp::RenameValue { key: "status".into(), from: "active".into(), to: "doing".into() }),
            src.replace("status: Active", "status: doing")
        );
        assert_eq!(
            edit(src, &MetaOp::RenameValue { key: "kind".into(), from: "b".into(), to: "c".into() }),
            src.replace("[a, b]", "[a, c]")
        );
        assert_eq!(edit(src, &MetaOp::DeleteKey { key: "Author".into() }), "---\nstatus: Active\nstate: done\nkind: [a, b]\n---\nbody\n");
        assert_eq!(edit(src, &MetaOp::DeleteKey { key: "nope".into() }), src);
        assert!(MetaOp::RenameTag { from: "a".into(), to: "a b".into() }.problem().is_some());
        assert!(MetaOp::RenameKey { from: "a".into(), to: "x:y".into() }.problem().is_some());
    }

    fn ws(files: &[(&str, &str)]) -> Workspace {
        let mut w = Workspace::new(crate::Settings::default());
        w.attach_vault(quire_vault::Vault::in_memory(files));
        w.index().flush();
        w
    }

    #[test]
    fn apply_previews_skips_and_undoes() {
        let mut w = ws(&[("A.md", "# A #old\n"), ("B.md", "---\ntags: [old]\n---\nB\n"), ("C.md", "#old/x here\n"), ("D.md", "nothing\n")]);
        w.open_note(&"A.md".into()).unwrap();
        w.meta_preview(MetaOp::RenameTag { from: "old".into(), to: "new".into() });
        let edits = w.meta_preview_wait().unwrap();
        assert_eq!(edits.iter().map(|e| e.path.as_str()).collect::<Vec<_>>(), ["A.md", "B.md", "C.md"]);
        assert_eq!(edits[0].diff(5), vec![(vec!["# A #old".to_string()], vec!["# A #new".to_string()])]);
        let skip = HashSet::from([NotePath::new("C.md")]);
        let batch = w.meta_apply(&skip).unwrap();
        assert_eq!(batch.files.len(), 2);
        assert_eq!(&*w.active_doc().unwrap().text(), "# A #new\n");
        let v = w.vault().unwrap().clone();
        assert_eq!(v.read(&"B.md".into()).unwrap(), "---\ntags: [new]\n---\nB\n");
        assert_eq!(v.read(&"C.md".into()).unwrap(), "#old/x here\n", "unticked");
        // The open note is still undoable in the editor.
        assert!(w.active_doc().unwrap().can_undo());
        // An edit after the batch survives undo.
        v.write(&"B.md".into(), "---\ntags: [new]\n---\nB\nmore\n").unwrap();
        w.index().upsert("B.md".into(), "---\ntags: [new]\n---\nB\nmore\n".into());
        w.index().flush();
        let r = w.meta_undo(&batch.id);
        assert_eq!(r, UndoReport { restored: 2, conflicts: vec![] });
        assert_eq!(v.read(&"B.md".into()).unwrap(), "---\ntags: [old]\n---\nB\nmore\n");
        assert_eq!(&*w.active_doc().unwrap().text(), "# A #old\n");
        // The ledger has both batches.
        let log = w.agent_store().unwrap().log(&quire_agent::ledger::LedgerQuery::default());
        assert_eq!(log.iter().filter(|e| e.batch.is_some()).count(), 4);
    }

    #[test]
    fn review_apply_reads_closed_notes_from_disk() {
        let mut w = ws(&[("B.md", "---\ntags: [old]\n---\nB\n")]);
        w.meta_preview(MetaOp::RenameTag { from: "old".into(), to: "new".into() });
        w.meta_preview_wait().unwrap();
        // Changed on disk (a sync, an editor elsewhere); not indexed yet.
        let v = w.vault().unwrap().clone();
        v.write(&"B.md".into(), "---\ntags: [old]\n---\nB\nadded elsewhere\n").unwrap();
        w.meta_apply(&HashSet::new()).unwrap();
        assert_eq!(v.read(&"B.md".into()).unwrap(), "---\ntags: [new]\n---\nB\nadded elsewhere\n");
    }

    #[test]
    fn review_a_batch_is_undone_after_a_restart() {
        let mut w = ws(&[("A.md", "# A #old\n"), ("B.md", "#old b\n")]);
        w.open_note(&"A.md".into()).unwrap();
        w.meta_preview(MetaOp::RenameTag { from: "old".into(), to: "new".into() });
        w.meta_preview_wait().unwrap();
        let batch = w.meta_apply(&HashSet::new()).unwrap();
        w.history.flush();
        // A restart: the session's batches are gone; the ledger has it.
        w.features.meta.batches.clear();
        w.save_all();
        let store = w.agent_store().unwrap().clone();
        let plan = store.revert_plan(&quire_agent::RevertSelector { batch: Some(batch.id), ..Default::default() });
        assert_eq!(plan.steps.len(), 2, "{plan:?}");
        let r = store.revert_apply(&plan, "Undo");
        assert!(r.failed.is_empty() && r.proposal.is_none(), "{r:?}");
        let v = w.vault().unwrap().clone();
        assert_eq!(v.read(&"A.md".into()).unwrap(), "# A #old\n");
        assert_eq!(v.read(&"B.md".into()).unwrap(), "#old b\n");
    }

    #[test]
    fn a_thousand_notes_preview_fast() {
        let files: Vec<(String, String)> = (0..1000)
            .map(|i| (format!("N{i}.md"), format!("---\ntags: [old, t{i}]\n---\n# Note {i}\nText #old and #old/sub {i}.\n")))
            .collect();
        let refs: Vec<(&str, &str)> = files.iter().map(|(a, b)| (a.as_str(), b.as_str())).collect();
        let mut w = ws(&refs);
        let t = std::time::Instant::now();
        w.meta_preview(MetaOp::RenameTag { from: "old".into(), to: "new".into() });
        let edits = w.meta_preview_wait().unwrap();
        assert_eq!(edits.len(), 1000);
        assert!(t.elapsed().as_millis() < 5000, "{:?} (debug build; the budget is 1 s in release)", t.elapsed());
        let batch = w.meta_apply(&HashSet::new()).unwrap();
        assert_eq!(batch.files.len(), 1000);
        let r = w.meta_undo(&batch.id);
        assert_eq!(r.restored, 1000);
        let v = w.vault().unwrap();
        assert!((0..1000).all(|i| v.read(&NotePath::new(format!("N{i}.md"))).unwrap() == files[i].1), "undo restores every byte");
    }
}
