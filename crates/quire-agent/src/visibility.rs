// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! [`Visibility`]: which indexed notes one caller may read, answered from
//! path rules with no file system call per note (`docs/design/0.14-agent-tools.md`
//! §3.5, `docs/design/0.14-architecture.md` §4).
//!
//! A listing asks about every note of the vault. [`crate::AgentStore::effective`]
//! answers one path at a time and pays for links, locked folders and the
//! policy file on every answer; a `Visibility` takes those once:
//!
//! - The policy, the approvals and the caller's grant: the cached part
//!   ([`crate::AgentStore`] keeps it until the policy state or a grant changes).
//! - Locked folders: checked on the disk when the value is made, for each
//!   folder it is given, and for any other folder the first time a path in it
//!   is asked about; never cached across calls, so a folder another process
//!   locks is hidden from the next call. At most two `exists` per folder.
//! - Symlinked folders: a path below a folder that is a symlink is hidden
//!   (one `symlink_metadata` per folder, on a vault on disk). The vault
//!   listing never yields such a path; this refuses one that reached the
//!   index some other way.
//! - Tags, for a client with a tag allow list: the index record's tags, not
//!   the file's (the one place this is looser than `effective`, by the
//!   watcher's lag; `view` still checks the file).
//!
//! What it does not do: follow a symlinked *file*. Paths an agent names go
//! through [`crate::AgentStore::guard`]; a `Visibility` is for paths that came
//! out of the index or the vault listing, which hold regular files only.
//!
//! The rules themselves are [`crate::store::decide`], shared with `effective`.

use std::collections::{BTreeSet, HashMap};
use std::fmt::Write as _;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};

use quire_vault::{NotePath, Storage};

use crate::actor::{Actor, ActorKind};
use crate::policy::{Mode, OpKind, Policy, strictest};
use crate::store::{Rules, canonical_path, decide};

/// What a folder check found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Folder {
    Plain,
    /// It, or a folder above it, holds a key file.
    Locked,
    /// It, or a folder above it, is a symlink.
    Linked,
}

/// Which notes one caller may read, for the length of one call (or one
/// view). Cheap to ask: string work per path, a lookup per folder.
pub struct Visibility {
    actor: Actor,
    /// `None` for the person, who sees everything.
    rules: Option<Arc<Rules>>,
    storage: Arc<dyn Storage>,
    /// The vault folder on disk, for the symlink check (`None` in memory).
    root: Option<PathBuf>,
    /// Folder (no trailing `/`) to what its check found, for this value's life.
    folders: Mutex<HashMap<String, Folder>>,
    /// The record tags of a tag-limited client's notes, by path.
    tags: Option<HashMap<NotePath, Vec<String>>>,
    fingerprint: [u8; 32],
}

impl std::fmt::Debug for Visibility {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Visibility")
            .field("actor", &self.actor.id)
            .field("fingerprint", &quire_base::enc::hex(&self.fingerprint))
            .finish_non_exhaustive()
    }
}

/// A note's path and its record's tags to "may this caller read it" (what
/// [`Visibility::predicate`] gives `quire-index`).
pub type Predicate = Arc<dyn Fn(&NotePath, &[String]) -> bool + Send + Sync>;

/// Bumped when the meaning of a fingerprint changes (a new rule).
const RULES_VERSION: &str = "quire-visibility 1";

impl Visibility {
    /// Made by [`crate::AgentStore::visibility`].
    pub(crate) fn new<'a>(
        actor: &Actor,
        rules: Option<Arc<Rules>>,
        storage: Arc<dyn Storage>,
        folders: impl IntoIterator<Item = &'a str>,
    ) -> Self {
        let root = storage.absolute(&NotePath::new(""));
        let mut v =
            Self { actor: actor.clone(), rules, storage, root, folders: Mutex::new(HashMap::new()), tags: None, fingerprint: [0; 32] };
        let mut found: BTreeSet<String> = BTreeSet::new();
        if v.rules.is_some() {
            for f in folders {
                let f = f.trim_matches('/');
                if !f.is_empty() {
                    v.folder(f);
                }
            }
            // The outermost folder of each kind: what the answers depend on.
            let map = v.folders.lock().unwrap_or_else(PoisonError::into_inner);
            // Into a sorted set, so the order of the map does not matter.
            #[expect(clippy::iter_over_hash_type, reason = "the result is a sorted set")]
            for (f, state) in map.iter() {
                let parent = f.rsplit_once('/').map(|(p, _)| p);
                if *state != Folder::Plain && parent.and_then(|p| map.get(p)).is_none_or(|s| *s == Folder::Plain) {
                    found.insert(format!("{state:?} {f}"));
                }
            }
        }
        v.fingerprint = v.compute_fingerprint(&found);
        v
    }

    /// Whether answers need each note's tags: the caller is a client with a
    /// tag allow list.
    pub fn needs_tags(&self) -> bool {
        self.rules
            .as_ref()
            .is_some_and(|r| r.policy.active().iter().any(|p| p.client(&self.actor.id).is_some_and(|c| c.allow_tags.is_some())))
    }

    /// The tags [`Self::allows`] uses for a tag-limited client: the index
    /// records' tags, by path. Without them such a client sees only what its
    /// folder list allows.
    #[must_use]
    pub fn with_tags(mut self, tags: HashMap<NotePath, Vec<String>>) -> Self {
        self.tags = Some(tags);
        self
    }

    /// Whether the caller may read `path`, a path from the index or the
    /// vault listing. Equal to `effective_mode(.., Read) != Deny` for such a
    /// path, with the tags given to [`Self::with_tags`].
    pub fn allows(&self, path: &NotePath) -> bool {
        let tags = self.tags.as_ref().and_then(|t| t.get(path)).map_or(&[][..], Vec::as_slice);
        self.allows_with_tags(path, tags)
    }

    /// [`Self::allows`] with the note's tags given (its index record's); a
    /// caller without a tag allow list ignores them.
    pub fn allows_with_tags(&self, path: &NotePath, tags: &[String]) -> bool {
        let Some(rules) = &self.rules else { return true };
        let Ok(path) = canonical_path(path) else { return false };
        let p = path.as_str();
        let hidden = p.rsplit_once('/').is_some_and(|(f, _)| self.folder(f) != Folder::Plain);
        strictest(decide(rules, &self.actor, p, hidden, &|| tags.to_vec()), OpKind::Read).0 != Mode::Deny
    }

    /// 32 bytes that are equal for two callers only if every answer of
    /// [`Self::allows_with_tags`] is equal: the read rules (which zones deny
    /// reading, not their other modes), "AI off", a revoked client, the
    /// client's allow lists (the rule, not the tags), the grant if it denies,
    /// and the locked and symlinked folders found when this was made.
    pub const fn fingerprint(&self) -> [u8; 32] {
        self.fingerprint
    }

    /// [`Self::allows_with_tags`] as a closure that does not name this type
    /// (what `quire-index` takes for a view).
    pub fn predicate(self: &Arc<Self>) -> Predicate {
        let me = Arc::clone(self);
        Arc::new(move |p: &NotePath, tags: &[String]| me.allows_with_tags(p, tags))
    }

    /// What the check of `folder` (no trailing `/`) finds, the folders above
    /// it first; each folder is checked once.
    fn folder(&self, folder: &str) -> Folder {
        if let Some(s) = self.folders.lock().unwrap_or_else(PoisonError::into_inner).get(folder) {
            return *s;
        }
        let name = folder.rsplit('/').next().unwrap_or(folder);
        let state = match folder.rsplit_once('/').map(|(p, _)| self.folder(p)) {
            Some(above) if above != Folder::Plain => above,
            // A folder inside a hidden one is never a locked folder (`lock::folder_of_with`); its
            // paths fail `canonical_path` anyway.
            _ if name.is_empty() || name.starts_with('.') => Folder::Plain,
            _ => self.check(folder),
        };
        self.folders.lock().unwrap_or_else(PoisonError::into_inner).insert(folder.to_string(), state);
        state
    }

    /// One folder on the disk: a symlink, a key file, an unlocking file.
    fn check(&self, folder: &str) -> Folder {
        use quire_vault::lock::{KEY_FILE, UNLOCKING_FILE};
        if let Some(root) = &self.root
            && std::fs::symlink_metadata(root.join(folder)).is_ok_and(|m| m.file_type().is_symlink())
        {
            return Folder::Linked;
        }
        let exists = |name: &str| self.storage.exists(&NotePath::new(format!("{folder}/{name}")));
        if exists(KEY_FILE) || exists(UNLOCKING_FILE) { Folder::Locked } else { Folder::Plain }
    }

    fn compute_fingerprint(&self, folders: &BTreeSet<String>) -> [u8; 32] {
        let mut text = format!("{RULES_VERSION}\n");
        let push = |text: &mut String, line: std::fmt::Arguments<'_>| {
            text.write_fmt(line).expect("a String takes any text");
            text.push('\n');
        };
        match &self.rules {
            None => push(&mut text, format_args!("person: everything")),
            Some(rules) => {
                let a = &self.actor;
                push(&mut text, format_args!("kind {:?} unverified {}", a.kind, a.unverified));
                push(&mut text, format_args!("grant denies {}", rules.cap == Some(Mode::Deny)));
                if a.kind == ActorKind::Agent {
                    for pol in rules.policy.active() {
                        policy_lines(&mut text, pol, a);
                    }
                }
                for f in folders {
                    push(&mut text, format_args!("folder {f}"));
                }
            }
        }
        let hex = quire_base::enc::sha256_hex(text.as_bytes());
        let mut out = [0; 32];
        if let Some(bytes) = quire_base::enc::unhex(&hex) {
            for (o, b) in out.iter_mut().zip(bytes) {
                *o = b;
            }
        }
        out
    }
}

/// Whether a mode hides a note from reading: the only part of a mode that
/// changes an answer of [`Visibility::allows`].
const fn hides(m: Mode) -> &'static str {
    if matches!(m, Mode::Deny) { "deny" } else { "read" }
}

/// The parts of one active policy that [`decide`] reads for `actor`'s reads.
fn policy_lines(text: &mut String, pol: &Policy, actor: &Actor) {
    let mut line = |args: std::fmt::Arguments<'_>| {
        text.write_fmt(args).expect("a String takes any text");
        text.push('\n');
    };
    line(format_args!("policy default {} ai_off {}", hides(pol.default), pol.ai_off()));
    // In order: at equal specificity the later zone wins (`Policy::zone_for`).
    for z in &pol.zones {
        line(format_args!("zone {:?} {}", z.path, hides(z.mode)));
    }
    let key = actor.policy_key();
    if let Some(c) = pol.clients.get(key) {
        line(format_args!("entry {key:?} default {:?}", c.default.map(hides)));
        for (p, m) in &c.zones {
            line(format_args!("entry zone {p:?} {}", hides(*m)));
        }
    }
    if let Some(c) = pol.client(&actor.id) {
        line(format_args!("client revoked {} folders {:?} tags {:?}", c.revoked, c.allow_folders, c.allow_tags));
    }
}
