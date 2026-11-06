// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! [`AgentStore`]: the one write path for everything that isn't the person
//! typing (design §2.5). MCP, the agent panel, ACP, the CLI and Quire's
//! jobs all call [`AgentStore::submit`], so policy is enforced in one place.
//!
//! A change set is applied when every change in it may be applied directly
//! (agent zones, new files in `raw/`); otherwise the whole set becomes one
//! proposal and no file is touched. A write by an agent to a path it already
//! has a pending proposal for amends that proposal instead of adding one.
//! Applied changes keep a history snapshot before and after (the after one
//! with provenance) and a ledger entry.
//!
//! Reviewing and deciding proposals is in `ops.rs`; undoing in `revert.rs`.

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};

use quire_markdown::frontmatter;
use quire_vault::{ContentHash, NotePath, Vault};
use unicode_normalization::UnicodeNormalization;

use crate::actor::{Actor, ActorKind, Clock, DeviceInfo, Op, Provenance, SourceRef, Trust};
use crate::approval::{APPROVED_INSTRUCTIONS_FILE, APPROVED_POLICY_FILE, DeviceState, StateDir};
use crate::change::{Change, ChangeSet, SubmitOutcome, Violation, WriteError};
use crate::contract::{Contract, parse_all};
use crate::ids::{BatchId, ProposalId};
use crate::inbox::InboxStore;
use crate::ledger::{Ledger, LedgerEntry, LedgerQuery};
use crate::policy::{Mode, OpKind, POLICY_PATH, Policy, Route, Widening, ZoneRole, hard_rule, stricter_limits, strictest, widenings};
use crate::proposal::{Proposal, ProposalChange, ProposalFilter, ProposalStatus, ProposalStore, ProposalSummary};

/// Where device approvals live: a folder, or memory (in-memory vaults, tests).
enum StateStore {
    Dir(StateDir),
    Memory(RwLock<(DeviceState, HashMap<String, String>)>),
}

impl StateStore {
    fn load(&self) -> DeviceState {
        match self {
            Self::Dir(d) => d.load(),
            Self::Memory(m) => m.read().unwrap_or_else(std::sync::PoisonError::into_inner).0.clone(),
        }
    }

    fn update(&self, f: impl FnOnce(&mut DeviceState)) -> Result<(), String> {
        match self {
            Self::Dir(d) => d.update(f).map(|_| ()),
            Self::Memory(m) => {
                f(&mut m.write().unwrap_or_else(std::sync::PoisonError::into_inner).0);
                Ok(())
            }
        }
    }

    fn read_copy(&self, name: &str) -> Option<String> {
        match self {
            Self::Dir(d) => d.read_copy(name),
            Self::Memory(m) => m.read().unwrap_or_else(std::sync::PoisonError::into_inner).1.get(name).cloned(),
        }
    }

    fn write_copy(&self, name: &str, text: &str) -> Result<(), String> {
        match self {
            Self::Dir(d) => d.write_copy(name, text),
            Self::Memory(m) => {
                m.write().unwrap_or_else(std::sync::PoisonError::into_inner).1.insert(name.into(), text.into());
                Ok(())
            }
        }
    }

    fn stamp(&self) -> Option<std::time::SystemTime> {
        match self {
            Self::Dir(d) => d.stamp(),
            Self::Memory(_) => None,
        }
    }
}

/// The vault's agent policy as this device sees it.
#[derive(Debug, Clone)]
pub struct PolicyState {
    /// The policy file's text, if there is one.
    pub file_text: Option<String>,
    /// The file parsed, or why it can't be (`None` without a file).
    pub file: Option<Result<Policy, String>>,
    /// This device approved exactly this file (or there is no file and
    /// nothing was ever approved: the defaults).
    pub approved: bool,
    /// What agents get when the file isn't approved: the last approved
    /// policy, or the defaults.
    pub baseline: Policy,
    /// The baseline as text (the approved file, or the default file).
    pub baseline_text: String,
    /// How the file gives agents more than the baseline (empty when approved).
    pub widenings: Vec<Widening>,
    /// Contracts in the file that can't be read.
    pub contract_errors: Vec<String>,
}

impl PolicyState {
    pub fn file_hash(&self) -> Option<ContentHash> {
        self.file_text.as_deref().map(ContentHash::of)
    }

    /// The file as parsed, if it can be.
    pub fn file_policy(&self) -> Option<&Policy> {
        self.file.as_ref().and_then(|r| r.as_ref().ok())
    }

    /// The policies that all limit agents (the most restrictive answer wins).
    pub fn active(&self) -> Vec<&Policy> {
        match (self.file_policy(), self.approved) {
            (Some(f), true) => vec![f],
            (Some(f), false) => vec![f, &self.baseline],
            (None, _) => vec![&self.baseline],
        }
    }

    /// The person should look at the file: it widens what was approved.
    pub fn needs_review(&self) -> bool {
        !self.approved && !self.widenings.is_empty()
    }

    /// The policy whose limits and contracts apply: the stricter limits and
    /// every contract of the active policies.
    pub fn in_force(&self) -> Policy {
        let active = self.active();
        let mut p = active[0].clone();
        for other in &active[1..] {
            p.limits = stricter_limits(&p.limits, &other.limits);
            for c in &other.contracts {
                if !p.contracts.contains(c) {
                    p.contracts.push(c.clone());
                }
            }
        }
        p
    }
}

/// Who last wrote each note, from the ledger (design §C.1 `last_actor`).
#[derive(Default)]
struct LastActors {
    stamp: Vec<(NotePath, Option<std::time::SystemTime>)>,
    map: HashMap<NotePath, (Actor, i64)>,
    imported: HashSet<NotePath>,
}

/// Tags per note, with the file's modification time when read.
type TagCache = HashMap<NotePath, (Option<std::time::SystemTime>, Vec<String>)>;

type CachedPolicy = Option<((Option<ContentHash>, Option<std::time::SystemTime>, u64), Arc<PolicyState>)>;

struct Inner {
    vault: Vault,
    device: DeviceInfo,
    clock: Arc<dyn Clock>,
    policy: RwLock<CachedPolicy>,
    /// Bumped when approvals change in this process (the cache key for memory state).
    approvals_gen: std::sync::atomic::AtomicU64,
    /// Grant caps per actor id (stdio without `--allow-write` is `Read`).
    caps: RwLock<HashMap<String, Mode>>,
    state: RwLock<Arc<StateStore>>,
    /// Held while a rate counter file is read and rewritten.
    rate: Mutex<()>,
    last: RwLock<LastActors>,
    /// Tags per note for allow lists, by modification time (0.11).
    tags: Mutex<TagCache>,
}

/// Agent state of one vault on this device. Cheap to clone.
#[derive(Clone)]
pub struct AgentStore {
    inner: Arc<Inner>,
}

/// Properties set and removed by a `SetProperties` change.
pub(crate) type PropertyOps = (Vec<(String, quire_markdown::Value)>, Vec<String>);

/// A change with its paths checked and its resulting text worked out.
pub(crate) struct Planned {
    pub change: Change,
    pub route: Route,
    /// Text before (edits, deletes, renames).
    pub before: Option<String>,
    /// Text after (creates and edits).
    pub after: Option<String>,
    /// For property changes: what was set and removed (to amend precisely).
    pub props: Option<PropertyOps>,
}

/// Where a path an agent named really leads ([`AgentStore::resolve`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    /// The path as named, in canonical form.
    pub path: NotePath,
    /// The file it reaches with links followed, as a vault path (the same
    /// as `path` when nothing on the way is a link).
    pub real: NotePath,
}

/// A vault path an agent may name: NFC, relative, no `..`, no hidden
/// segments except `.quire` (which policy then denies), no control characters.
pub fn canonical_path(p: &NotePath) -> Result<NotePath, WriteError> {
    let s: String = p.as_str().nfc().collect();
    let bad = |reason: &str| Err(WriteError::InvalidPath { reason: format!("{s}: {reason}") });
    if s.is_empty() || s.ends_with('/') {
        return bad("not a file path");
    }
    if s.chars().any(char::is_control) {
        return bad("contains control characters");
    }
    if s.starts_with('/') || s.contains(':') || s.contains('\\') {
        return bad("must be relative to the vault");
    }
    for seg in s.split('/') {
        if seg.is_empty() || seg == "." || seg == ".." {
            return bad("has an empty, `.` or `..` segment");
        }
        if seg.starts_with('.') && !seg.eq_ignore_ascii_case(".quire") {
            return bad("is inside a hidden folder");
        }
    }
    Ok(NotePath::new(s))
}

/// Where `path` really leads, for any caller with a vault.
///
/// The body of [`AgentStore::resolve`]: canonical ([`canonical_path`]),
/// with links on the way followed; the file they reach must be inside the
/// vault and outside hidden folders. A link that leads nowhere is refused.
pub fn resolve_in(vault: &Vault, path: &NotePath) -> Result<Resolved, WriteError> {
    match vault.storage().absolute(&NotePath::new("")) {
        Some(root) => resolve_at(&root, path),
        None => canonical_path(path).map(|path| Resolved { real: path.clone(), path }),
    }
}

/// [`resolve_in`] for a vault folder on disk.
pub fn resolve_at(root: &std::path::Path, path: &NotePath) -> Result<Resolved, WriteError> {
    let path = canonical_path(path)?;
    let abs = root.join(path.as_str());
    let outside = |why: &str| WriteError::InvalidPath { reason: format!("{path}: {why}") };
    let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    // The deepest part that exists, with links followed; the rest as named.
    let mut probe = abs.as_path();
    let mut tail: Vec<String> = Vec::new();
    let found = loop {
        match probe.canonicalize() {
            Ok(c) => break c,
            Err(_) if probe.symlink_metadata().is_ok() => return Err(outside("is a link that leads nowhere")),
            Err(_) => {
                let name = probe.file_name().and_then(|n| n.to_str()).ok_or_else(|| outside("leads outside the vault"))?;
                tail.push(name.to_string());
                probe = probe.parent().ok_or_else(|| outside("leads outside the vault"))?;
            }
        }
    };
    let rel = found.strip_prefix(&root).map_err(|_| outside("leads outside the vault"))?;
    let mut segs: Vec<String> = Vec::new();
    for c in rel.components() {
        segs.push(c.as_os_str().to_str().ok_or_else(|| outside("leads to a file name Quire can't read"))?.to_string());
    }
    segs.extend(tail.into_iter().rev());
    let real = canonical_path(&NotePath::new(segs.join("/"))).map_err(|_| outside("leads into a hidden folder"))?;
    Ok(Resolved { path, real })
}

/// A file someone else named that Quire is about to read, show or write.
///
/// A picture a drawing embeds, a path in another device's shared review
/// file: [`resolve_in`], and neither the path as named nor the file it
/// reaches may be in any hidden folder, `.quire` included.
pub fn vault_file(vault: &Vault, path: &NotePath) -> Result<Resolved, WriteError> {
    not_hidden(resolve_in(vault, path)?)
}

/// [`vault_file`] for a vault folder on disk.
pub fn vault_file_at(root: &std::path::Path, path: &NotePath) -> Result<Resolved, WriteError> {
    not_hidden(resolve_at(root, path)?)
}

fn not_hidden(r: Resolved) -> Result<Resolved, WriteError> {
    let hidden = |p: &NotePath| p.as_str().split('/').any(|s| s.starts_with('.') || s == "node_modules");
    if hidden(&r.path) || hidden(&r.real) {
        return Err(WriteError::InvalidPath { reason: format!("{}: is inside a hidden folder", r.path) });
    }
    Ok(r)
}

impl AgentStore {
    pub fn open(vault: &Vault, device: DeviceInfo, clock: Arc<dyn Clock>) -> Self {
        let dir = if cfg!(test) {
            None
        } else {
            vault.storage().absolute(&NotePath::new("")).and_then(|root| crate::approval::default_state_dir(&root))
        };
        let state = dir.map_or_else(|| StateStore::Memory(RwLock::default()), |d| StateStore::Dir(StateDir::new(d)));
        Self {
            inner: Arc::new(Inner {
                // No keys: agents never read a locked folder, even an unlocked one (0.11).
                vault: vault.without_keys(),
                device,
                clock,
                policy: RwLock::new(None),
                approvals_gen: std::sync::atomic::AtomicU64::new(0),
                caps: RwLock::new(HashMap::new()),
                state: RwLock::new(Arc::new(state)),
                rate: Mutex::new(()),
                last: RwLock::new(LastActors::default()),
                tags: Mutex::new(HashMap::new()),
            }),
        }
    }

    pub fn vault(&self) -> &Vault {
        &self.inner.vault
    }

    pub fn device(&self) -> &DeviceInfo {
        &self.inner.device
    }

    pub fn now_ms(&self) -> i64 {
        self.inner.clock.now_ms()
    }

    pub fn clock(&self) -> Arc<dyn Clock> {
        Arc::clone(&self.inner.clock)
    }

    /// Keep this device's approvals in `dir` (`<config>/vaults/<key>/`), or
    /// in memory with `None`. `open` picks Quire's config folder for vaults
    /// on disk; tests point it elsewhere.
    pub fn set_state_dir(&self, dir: Option<PathBuf>) {
        let st = dir.map_or_else(|| StateStore::Memory(RwLock::default()), |d| StateStore::Dir(StateDir::new(d)));
        *self.inner.state.write().unwrap_or_else(std::sync::PoisonError::into_inner) = Arc::new(st);
        self.approvals_changed();
    }

    fn state(&self) -> Arc<StateStore> {
        Arc::clone(&self.inner.state.read().unwrap_or_else(std::sync::PoisonError::into_inner))
    }

    fn approvals_changed(&self) {
        self.inner.approvals_gen.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        *self.inner.policy.write().unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    }

    /// This vault's state on this device (approvals, read logging, snoozes).
    pub fn device_state(&self) -> DeviceState {
        self.state().load()
    }

    /// Change this vault's device state.
    pub fn update_device_state(&self, f: impl FnOnce(&mut DeviceState)) -> Result<(), String> {
        self.state().update(f)?;
        self.approvals_changed();
        Ok(())
    }

    /// Limit what `actor_id` may do whatever the zones say (a client's grant).
    pub fn set_cap(&self, actor_id: &str, cap: Mode) {
        self.inner.caps.write().unwrap_or_else(std::sync::PoisonError::into_inner).insert(actor_id.to_string(), cap);
    }

    // ------------------------------------------------------------ policy

    /// The policy file as it is on disk, if it can be read.
    pub fn file_policy(&self) -> Option<Policy> {
        self.policy_state().file_policy().cloned()
    }

    /// The policy in force: limits and contracts of every active policy.
    pub fn policy(&self) -> Policy {
        self.policy_state().in_force()
    }

    /// The policy file, its approval on this device and what it widens.
    pub fn policy_state(&self) -> Arc<PolicyState> {
        let text = self.inner.vault.storage().read(&NotePath::new(POLICY_PATH)).ok();
        let state = self.state();
        let key =
            (text.as_deref().map(ContentHash::of), state.stamp(), self.inner.approvals_gen.load(std::sync::atomic::Ordering::Acquire));
        if let Some((k, p)) = self.inner.policy.read().unwrap_or_else(std::sync::PoisonError::into_inner).as_ref()
            && *k == key
        {
            return Arc::clone(p);
        }
        let dev = state.load();
        let approved_copy = state
            .read_copy(APPROVED_POLICY_FILE)
            .filter(|t| dev.approved_policy.as_ref() == Some(&ContentHash::of(t)))
            .and_then(|t| Policy::parse(&t).ok().map(|p| (p, t)));
        let (baseline, baseline_text) = approved_copy.unwrap_or_else(|| (Policy::default(), crate::policy_edit::STARTER.to_string()));
        let (file, approved) = match &text {
            Some(t) => (Some(Policy::parse(t)), dev.approved_policy.as_ref() == Some(&ContentHash::of(t))),
            // No file: the defaults, approved unless something else was approved before.
            None => (dev.approved_policy.is_some().then(|| Ok(Policy::default())), dev.approved_policy.is_none()),
        };
        let wid = match file.as_ref().and_then(|f| f.as_ref().ok()) {
            Some(f) if !approved => widenings(&baseline, f),
            _ => Vec::new(),
        };
        let contract_errors = file.as_ref().and_then(|f| f.as_ref().ok()).map(|f| parse_all(&f.contracts).1).unwrap_or_default();
        let st = Arc::new(PolicyState { file_text: text, file, approved, baseline, baseline_text, widenings: wid, contract_errors });
        *self.inner.policy.write().unwrap_or_else(std::sync::PoisonError::into_inner) = Some((key, Arc::clone(&st)));
        st
    }

    /// Approve the policy file the person was shown (`shown`: its hash, or
    /// `None` for no file, which goes back to the defaults). Refused when
    /// the file changed since, so nothing is approved unseen (P7).
    pub fn approve_policy(&self, shown: Option<&ContentHash>) -> Result<(), String> {
        let text = self.inner.vault.storage().read(&NotePath::new(POLICY_PATH)).ok();
        if text.as_deref().map(ContentHash::of).as_ref() != shown {
            return Err("The agent policy changed since it was shown. Look at it again before approving it.".into());
        }
        let state = self.state();
        if let Some(t) = &text {
            if Policy::parse(t).is_err() {
                return Err("The policy file can't be read; fix it before approving it".into());
            }
            state.write_copy(APPROVED_POLICY_FILE, t)?;
        }
        state.update(|s| s.approved_policy = text.as_deref().map(ContentHash::of))?;
        self.approvals_changed();
        Ok(())
    }

    /// Write a new policy file (Settings → Agents) and approve it here.
    /// The person made it on this device, so it takes effect at once.
    pub fn write_policy(&self, text: &str) -> Result<(), String> {
        let p = Policy::parse(text).map_err(|e| format!("That policy can't be read: {e}"))?;
        if self.policy_state().file_policy().is_some_and(Policy::is_newer) || p.is_newer() {
            return Err("This vault's agent policy was made by a newer Quire; update Quire to change it".into());
        }
        self.inner.vault.storage().write_atomic(&NotePath::new(POLICY_PATH), text).map_err(|e| format!("Couldn't save the policy: {e}"))?;
        self.approve_policy(Some(&ContentHash::of(text)))
    }

    /// What `actor` may do to `path` for `op`. People are never limited.
    /// For anyone else the most restrictive of these applies: the zones of
    /// every active policy (with the client's entry), the hard rules, and
    /// the actor's grant, for the path as named and for the file it really
    /// reaches ([`Self::resolve`]). A path that can't be resolved is denied.
    pub fn effective_mode(&self, actor: &Actor, path: &NotePath, op: OpKind) -> Mode {
        self.effective(actor, path, op).0
    }

    /// The mode and the zone (folder prefix, or `default`) it came from.
    pub fn effective(&self, actor: &Actor, path: &NotePath, op: OpKind) -> (Mode, String) {
        if actor.is_human() {
            return (Mode::Direct, String::new());
        }
        let Ok(r) = self.resolve(path) else { return (Mode::Deny, path.to_string()) };
        let mut candidates = self.candidates(actor, &r.path);
        if r.real != r.path {
            candidates.extend(self.candidates(actor, &r.real));
        }
        strictest(candidates, op)
    }

    /// The modes that limit `actor` on one (resolved) path.
    fn candidates(&self, actor: &Actor, path: &NotePath) -> Vec<(Mode, String)> {
        let p = path.as_str();
        let mut candidates: Vec<(Mode, String)> = Vec::new();
        // 0.11: locked folders are invisible to everyone but the person,
        // locked or not, and so are encrypted files and key files anywhere.
        if quire_vault::lock::is_encrypted_name(p)
            || quire_vault::lock::is_key_file(p)
            || quire_vault::lock::folder_of(self.inner.vault.storage().as_ref(), p).is_some()
        {
            candidates.push((Mode::Deny, "locked folder".into()));
        }
        if actor.kind == ActorKind::Agent {
            for pol in self.policy_state().active() {
                candidates.extend(pol.modes_for(Some(actor.policy_key()), actor.unverified, p));
                // 0.11: AI off, revoked clients, allow lists.
                if pol.ai_off() {
                    candidates.push((Mode::Deny, "AI is off for this vault".into()));
                }
                if let Some(c) = pol.client(&actor.id) {
                    if c.revoked {
                        candidates.push((Mode::Deny, format!("access revoked ({})", actor.policy_key())));
                    }
                    if c.limited() && !c.allows(p, &self.tags_of(path)) {
                        candidates.push((Mode::Deny, format!("outside the tags and folders {} may see", actor.policy_key())));
                    }
                }
            }
        } else {
            candidates.push((Mode::Direct, String::new()));
        }
        if let Some(rule) = hard_rule(p, &self.inner.vault.config.templates_folder) {
            candidates.push((rule.cap(), p.to_string()));
        }
        if let Some(cap) = self.inner.caps.read().unwrap_or_else(std::sync::PoisonError::into_inner).get(&actor.id) {
            candidates.push((*cap, "grant".into()));
        }
        candidates
    }

    /// Where `path` really leads (design §A.2 step 1). The path must be
    /// canonical ([`canonical_path`]: NFC, relative, no `..`, no hidden
    /// folders); links on the way are followed, and the file they reach must
    /// be inside the vault and outside hidden folders too. A link that leads
    /// nowhere is refused.
    pub fn resolve(&self, path: &NotePath) -> Result<Resolved, WriteError> {
        resolve_in(&self.inner.vault, path)
    }

    /// The one path check for everything an agent names: MCP tools and
    /// resources, ACP file requests, captured images. The path is resolved
    /// ([`Self::resolve`]) and `actor` must not be denied `op` on it or on
    /// the file it reaches. Returns the canonical path.
    pub fn guard(&self, actor: &Actor, path: &NotePath, op: OpKind) -> Result<NotePath, WriteError> {
        let r = self.resolve(path)?;
        let (mode, zone) = self.effective(actor, &r.path, op);
        if mode.route(op) == Route::Deny {
            return Err(WriteError::PermissionDenied { path: r.path, zone, mode });
        }
        Ok(r.path)
    }

    /// The contracts that apply to an agent's write.
    pub fn contracts(&self) -> Vec<Contract> {
        parse_all(&self.policy().contracts).0
    }

    // ------------------------------------------------------------ trust

    /// Whether `path` is untrusted content (design §A.3): in a `raw` zone,
    /// brought in by an importer, or marked `trust: untrusted`. `text` is
    /// its text when the caller has it (front matter can only lower trust).
    pub fn trust_of(&self, path: &NotePath, text: Option<&str>) -> Trust {
        let st = self.policy_state();
        // The path as named and the file it reaches; an unapproved policy
        // file can't relabel a zone the defaults or the approved file call raw.
        let paths = self.resolve(path).map_or_else(|_| vec![path.clone()], |r| vec![r.path, r.real]);
        let raw =
            paths.iter().any(|p| st.active().iter().chain([&&Policy::default()]).any(|pol| pol.role_of(p.as_str()) == Some(ZoneRole::Raw)));
        if raw || paths.iter().any(|p| self.imported(p)) {
            return Trust::Untrusted;
        }
        // 0.11: a skill installed from someone else (`quire-cli skill install --from`)
        // has its source recorded beside it, and stays data.
        let storage = self.inner.vault.storage();
        if paths
            .iter()
            .any(|p| p.file_name().eq_ignore_ascii_case("skill.md") && storage.exists(&NotePath::join(p.folder(), "SKILL.source.json")))
        {
            return Trust::Untrusted;
        }
        let owned = if text.is_none() { self.inner.vault.storage().read(path).ok() } else { None };
        let text = text.or(owned.as_deref());
        let marked = text
            .and_then(frontmatter::parse)
            .and_then(|fm| fm.get("trust").and_then(|v| v.as_str().map(|s| s.trim().eq_ignore_ascii_case("untrusted"))))
            .unwrap_or(false);
        if marked { Trust::Untrusted } else { Trust::Trusted }
    }

    /// The first untrusted source of a change, in words ("raw/paper.md",
    /// a URL), if any.
    pub fn untrusted_source(&self, sources: &[SourceRef]) -> Option<String> {
        sources.iter().find_map(|s| {
            let uri = s.uri.trim();
            if let Some(rest) = uri.strip_prefix("quire://note/") {
                let path = NotePath::new(percent_decode(rest));
                (self.trust_of(&path, None) == Trust::Untrusted).then(|| path.to_string())
            } else if uri.starts_with("http://") || uri.starts_with("https://") {
                Some(uri.to_string())
            } else {
                None
            }
        })
    }

    // ------------------------------------------------------------ last actors

    fn refresh_last(&self) {
        let ledger = self.ledger();
        let mut stamp = ledger.stamp();
        stamp.extend(self.shared_log_stamp());
        if self.inner.last.read().unwrap_or_else(std::sync::PoisonError::into_inner).stamp == stamp && !stamp.is_empty() {
            return;
        }
        let since = self.now_ms() - 90 * 86_400_000;
        let mut map: HashMap<NotePath, (Actor, i64)> = HashMap::new();
        let mut imported = HashSet::new();
        // Every device's changes: an agent on a server wrote this note too.
        for e in self.log(&LedgerQuery { since: Some(since), ..LedgerQuery::default() }) {
            let Some(path) = e.path.clone() else { continue };
            match e.op {
                Op::Write | Op::Create | Op::Delete | Op::Revert | Op::Accept => {
                    if e.op == Op::Create && e.actor.kind == ActorKind::Import {
                        imported.insert(path.clone());
                    }
                    map.insert(path, (e.actor, e.at));
                }
                Op::Rename => {
                    if let Some(to) = e.to.clone() {
                        map.remove(&path);
                        if imported.remove(&path) {
                            imported.insert(to.clone());
                        }
                        map.insert(to, (e.actor, e.at));
                    }
                }
                Op::Propose | Op::Reject | Op::Read | Op::Batch => {}
            }
        }
        *self.inner.last.write().unwrap_or_else(std::sync::PoisonError::into_inner) = LastActors { stamp, map, imported };
    }

    /// Who last wrote `path` according to the ledger (last 90 days), and
    /// when. Edits the person typed aren't in the ledger: compare with the
    /// file's modification time.
    pub fn last_actor(&self, path: &NotePath) -> Option<(Actor, i64)> {
        self.refresh_last();
        self.inner.last.read().unwrap_or_else(std::sync::PoisonError::into_inner).map.get(path).cloned()
    }

    fn imported(&self, path: &NotePath) -> bool {
        self.refresh_last();
        self.inner.last.read().unwrap_or_else(std::sync::PoisonError::into_inner).imported.contains(path)
    }

    // ------------------------------------------------------------ submit

    /// A path an agent may create (or rename to): the canonical form, not
    /// through a link out of the vault, not differing only in case from an
    /// existing file.
    fn check_new_path(&self, path: &NotePath) -> Result<(), WriteError> {
        let storage = self.inner.vault.storage();
        if let (Some(root), Some(abs)) = (storage.absolute(&NotePath::new("")), storage.absolute(path)) {
            let root = root.canonicalize().unwrap_or(root);
            let mut probe = abs.parent().map(std::path::Path::to_path_buf);
            while let Some(p) = probe {
                if p.exists() {
                    if !p.canonicalize().is_ok_and(|c| c.starts_with(&root)) {
                        return Err(WriteError::InvalidPath { reason: format!("{path}: leads outside the vault") });
                    }
                    break;
                }
                probe = p.parent().map(std::path::Path::to_path_buf);
            }
        }
        let lower = path.as_str().to_lowercase();
        if let Some(other) = storage.list().unwrap_or_default().into_iter().find(|p| p.as_str().to_lowercase() == lower && p != path) {
            return Err(WriteError::InvalidPath { reason: format!("{path}: differs only in case from {other}") });
        }
        Ok(())
    }

    /// An existing file must not be reached through a link out of the vault.
    fn check_existing_path(&self, path: &NotePath) -> Result<(), WriteError> {
        let storage = self.inner.vault.storage();
        if let (Some(root), Some(abs)) = (storage.absolute(&NotePath::new("")), storage.absolute(path))
            && abs.exists()
        {
            let root = root.canonicalize().unwrap_or(root);
            if !abs.canonicalize().is_ok_and(|c| c.starts_with(&root)) {
                return Err(WriteError::InvalidPath { reason: format!("{path}: leads outside the vault") });
            }
        }
        Ok(())
    }

    pub(crate) fn plan_one(&self, actor: &Actor, change: Change, limit: u64) -> Result<Planned, WriteError> {
        let vault = &self.inner.vault;
        let read = |p: &NotePath| vault.storage().read(p).map_err(|_| WriteError::NotFound { path: p.clone() });
        let mut props = None;
        let (change, op, before, after) = match change {
            Change::Create { path, text } => {
                let path = canonical_path(&path)?;
                if vault.exists(&path) {
                    return Err(WriteError::AlreadyExists { path });
                }
                self.check_new_path(&path)?;
                (Change::Create { path, text: text.clone() }, OpKind::Create, None, Some(text))
            }
            Change::Edit { path, new_text } => {
                let path = canonical_path(&path)?;
                self.check_existing_path(&path)?;
                let before = read(&path)?;
                (Change::Edit { path, new_text: new_text.clone() }, OpKind::Edit, Some(before), Some(new_text))
            }
            Change::SetProperties { path, set, remove } => {
                let path = canonical_path(&path)?;
                self.check_existing_path(&path)?;
                let before = read(&path)?;
                let text = set_props(&before, &set, &remove);
                props = Some((set, remove));
                (Change::Edit { path, new_text: text.clone() }, OpKind::Edit, Some(before), Some(text))
            }
            Change::Rename { from, to, update_links } => {
                let (from, to) = (canonical_path(&from)?, canonical_path(&to)?);
                self.check_existing_path(&from)?;
                let before = read(&from)?;
                if vault.exists(&to) && !to.as_str().eq_ignore_ascii_case(from.as_str()) {
                    return Err(WriteError::AlreadyExists { path: to });
                }
                if !to.as_str().eq_ignore_ascii_case(from.as_str()) {
                    self.check_new_path(&to)?;
                }
                // Renaming needs both: leaving the old place and arriving at the new one.
                let (to_mode, to_zone) = self.effective(actor, &to, OpKind::Create);
                if to_mode.route(OpKind::Create) == Route::Deny {
                    return Err(WriteError::PermissionDenied { path: to, zone: to_zone, mode: to_mode });
                }
                (Change::Rename { from, to, update_links }, OpKind::Rename, Some(before), None)
            }
            Change::Delete { path } => {
                let path = canonical_path(&path)?;
                self.check_existing_path(&path)?;
                let before = read(&path)?;
                (Change::Delete { path }, OpKind::Delete, Some(before), None)
            }
        };
        if let Some(t) = &after
            && t.len() as u64 > limit
        {
            return Err(WriteError::TooLarge { limit });
        }
        let path = change.path().clone();
        let (mode, zone) = self.effective(actor, &path, op);
        let mut route = mode.route(op);
        if let Change::Rename { from, to, update_links } = &change {
            route = route.min(self.effective(actor, to, OpKind::Create).0.route(OpKind::Create));
            // Rewriting links in notes the agent may only suggest changes to
            // makes the rename a suggestion too.
            if *update_links && route == Route::Apply && self.links_need_review(actor, from, to) {
                route = Route::Propose;
            }
        }
        match route {
            Route::Deny => Err(WriteError::PermissionDenied { path, zone, mode }),
            Route::Immutable => Err(WriteError::RawIsImmutable { path }),
            Route::Propose | Route::Apply => Ok(Planned { change, route, before, after, props }),
        }
    }

    /// Whether renaming `from` to `to` would rewrite links in a note that
    /// `actor` may not edit directly.
    fn links_need_review(&self, actor: &Actor, from: &NotePath, to: &NotePath) -> bool {
        let vault = &self.inner.vault;
        let by_name = quire_vault::LinkRewrite {
            old_stem: from.stem(),
            old_path: from.without_ext(),
            new_stem: to.stem(),
            resolves: None,
            new_path: None,
        };
        let by_path = quire_vault::LinkRewrite { new_path: Some(to.without_ext()), ..by_name.clone() };
        vault.notes().unwrap_or_default().iter().filter(|p| *p != from).any(|p| {
            let Ok(src) = vault.read(p) else { return false };
            let touched = quire_vault::rewrite_links(&src, &by_name).is_some() || quire_vault::rewrite_links(&src, &by_path).is_some();
            touched && self.effective(actor, p, OpKind::Edit).0.route(OpKind::Edit) != Route::Apply
        })
    }

    /// Check the agent's resulting text against the contracts in force.
    fn check_contracts(&self, actor: &Actor, planned: &[Planned]) -> Result<(), WriteError> {
        if actor.kind != ActorKind::Agent {
            return Ok(());
        }
        let contracts = self.contracts();
        if contracts.is_empty() {
            return Ok(());
        }
        let notes = self.inner.vault.notes().unwrap_or_default();
        // Notes created in the same change set count as existing.
        let created: Vec<NotePath> =
            planned.iter().filter(|p| matches!(p.change, Change::Create { .. })).map(|p| p.change.path().clone()).collect();
        for p in planned {
            let (path, text) = match &p.change {
                Change::Create { path, text } => (path, text.as_str()),
                Change::Edit { path, .. } => (path, p.after.as_deref().unwrap_or_default()),
                Change::Rename { to, .. } => (to, p.before.as_deref().unwrap_or_default()),
                Change::Delete { .. } | Change::SetProperties { .. } => continue,
            };
            let kind = frontmatter::parse(text).and_then(|fm| fm.get("type").and_then(|v| v.as_str().map(str::to_string)));
            for c in contracts.iter().filter(|c| c.applies_to(path.as_str(), kind.as_deref())) {
                let resolves = |t: &str| {
                    Vault::resolve_among(&notes, t, Some(path)).is_some() || Vault::resolve_among(&created, t, Some(path)).is_some()
                };
                let violations: Vec<Violation> = c.check(text, &resolves);
                if !violations.is_empty() {
                    return Err(WriteError::ContractViolation { contract: c.name.clone(), violations });
                }
            }
        }
        Ok(())
    }

    fn rate_file(actor: &Actor) -> NotePath {
        NotePath::new(format!(".quire/agent/rate/{}.txt", &ContentHash::of(&actor.id).hex()[..16]))
    }

    /// The times of `actor`'s writes in the minute before `now`, oldest first.
    fn recent_writes(&self, actor: &Actor, now: i64) -> Vec<i64> {
        let storage = self.inner.vault.storage();
        let mut times: Vec<i64> = storage
            .read(&Self::rate_file(actor))
            .map(|t| t.lines().filter_map(|l| l.split_whitespace().next()?.parse().ok()).collect())
            .unwrap_or_default();
        times.retain(|t| now - t < 60_000 && *t <= now);
        times.sort_unstable();
        times
    }

    /// How many more writes `actor` may make this minute (`u32::MAX`: no limit).
    pub fn writes_left(&self, actor: &Actor) -> u32 {
        let per_minute = self.policy().limits.writes_per_minute;
        if actor.kind != ActorKind::Agent || per_minute == 0 {
            return u32::MAX;
        }
        let used = u32::try_from(self.recent_writes(actor, self.now_ms()).len()).unwrap_or(u32::MAX);
        per_minute.saturating_sub(used)
    }

    /// Writes by `actor` in the last minute, across processes: a small file
    /// per client under `.quire/agent/rate/` plus this process's own count.
    fn rate_check(&self, actor: &Actor, per_minute: u32, record: bool) -> Result<(), WriteError> {
        if actor.kind != ActorKind::Agent || per_minute == 0 {
            return Ok(());
        }
        let now = self.now_ms();
        let file = Self::rate_file(actor);
        let storage = self.inner.vault.storage();
        // One process at a time in this process; across processes the file is
        // rewritten atomically, so at worst a racing write goes uncounted.
        let _guard = self.inner.rate.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut times = self.recent_writes(actor, now);
        if times.len() >= per_minute as usize {
            let oldest = times.first().copied().unwrap_or(now);
            let wait = u64::try_from((oldest + 60_000 - now).max(1)).unwrap_or(1);
            return Err(WriteError::RateLimited { retry_after_ms: wait });
        }
        if record {
            times.push(now);
            let text = times.iter().fold(String::new(), |mut s, t| {
                let _ = writeln!(s, "{t}");
                s
            });
            let _ = storage.write_atomic(&file, &text);
        }
        Ok(())
    }

    /// An edit of a file that doesn't exist yet, but that `actor` proposed
    /// creating: the edit amends the proposed create.
    fn edits_to_own_creates(&self, actor: &Actor, changes: Vec<Change>) -> Vec<Change> {
        let vault = &self.inner.vault;
        let cell = std::cell::OnceCell::new();
        let pending = || cell.get_or_init(|| self.own_pending(actor));
        changes
            .into_iter()
            .map(|c| match c {
                Change::Edit { path, new_text } if !vault.exists(&path) => {
                    let proposed = pending().iter().any(|p| p.changes.iter().any(|ch| ch.is_pending_create_of(&path)));
                    if proposed { Change::Create { path, text: new_text } } else { Change::Edit { path, new_text } }
                }
                Change::SetProperties { path, set, remove } if !vault.exists(&path) => {
                    let text = pending()
                        .iter()
                        .find_map(|p| p.changes.iter().find(|ch| ch.is_pending_create_of(&path)).and_then(|ch| ch.text.clone()));
                    match text {
                        Some(t) => Change::Create { text: set_props(&t, &set, &remove), path },
                        None => Change::SetProperties { path, set, remove },
                    }
                }
                other => other,
            })
            .collect()
    }

    /// Apply or propose `cs` as `actor`. Nothing is written when an error is returned.
    pub fn submit(&self, actor: &Actor, mut cs: ChangeSet) -> Result<SubmitOutcome, WriteError> {
        let policy = self.policy();
        let changes = std::mem::take(&mut cs.changes);
        if changes.len() as u64 > u64::from(policy.limits.max_files_per_change) {
            return Err(WriteError::TooLarge { limit: u64::from(policy.limits.max_files_per_change) });
        }
        self.rate_check(actor, policy.limits.writes_per_minute, false)?;
        let changes = if actor.kind == ActorKind::Agent { self.edits_to_own_creates(actor, changes) } else { changes };
        let mut planned = Vec::with_capacity(changes.len());
        for change in changes {
            planned.push(self.plan_one(actor, change, policy.limits.max_write_bytes)?);
        }
        if actor.kind == ActorKind::Agent {
            // Creating again what it proposed to create amends that proposal.
            let own = self.own_pending(actor);
            for p in &mut planned {
                if let Change::Create { path, .. } = &p.change
                    && own.iter().any(|o| o.changes.iter().any(|c| c.is_pending_create_of(path)))
                {
                    p.route = Route::Propose;
                }
            }
        }
        for (path, expected) in &cs.expected {
            let path = canonical_path(path)?;
            // A version of the agent's own pending text is what it saw with `include_pending`.
            if self.pending_text(actor, &path).is_some_and(|(_, h)| h == *expected) {
                continue;
            }
            if let Ok(text) = self.inner.vault.storage().read(&path) {
                let current = ContentHash::of(&text);
                if current != *expected {
                    // The served-version cache (WS-B) fills in the diff and a merged preview.
                    return Err(WriteError::VersionConflict {
                        path,
                        expected: expected.clone(),
                        current,
                        diff_since: String::new(),
                        merged_preview: None,
                    });
                }
            }
        }
        self.check_contracts(actor, &planned)?;
        let out = if planned.iter().all(|p| p.route == Route::Apply) {
            self.apply(actor, &cs, planned)
        } else {
            self.propose(actor, &cs, &planned, policy.limits.max_pending_per_client)
        }?;
        let _ = self.rate_check(actor, policy.limits.writes_per_minute, true);
        Ok(out)
    }

    /// Add a binary file (a captured image) to the attachments folder as
    /// `actor`. The same path guard, policy and limits as [`Self::submit`]
    /// apply, except that a binary file can't be a proposal: it is written
    /// only where `actor` may create files directly, and refused otherwise.
    /// An agent's file counts as one write and must fit `max_write_bytes`.
    pub fn add_attachment(&self, actor: &Actor, file_name: &str, bytes: &[u8], source: Option<&str>) -> Result<NotePath, WriteError> {
        let policy = self.policy();
        if actor.kind == ActorKind::Agent && bytes.len() as u64 > policy.limits.max_write_bytes {
            return Err(WriteError::TooLarge { limit: policy.limits.max_write_bytes });
        }
        self.rate_check(actor, policy.limits.writes_per_minute, false)?;
        let vault = &self.inner.vault;
        let path = canonical_path(&vault.attachment_path(file_name))?;
        if vault.exists(&path) {
            return Err(WriteError::AlreadyExists { path });
        }
        self.check_new_path(&path)?;
        let (mode, zone) = self.effective(actor, &path, OpKind::Create);
        if mode.route(OpKind::Create) != Route::Apply {
            return Err(WriteError::PermissionDenied { path, zone, mode });
        }
        vault.storage().write_bytes_atomic(&path, bytes)?;
        let mut e = LedgerEntry::new(actor.clone(), Op::Create, self.now_ms()).path(&path);
        e.bytes = Some(bytes.len() as u64);
        if let Some(src) = source {
            e.other.insert("source".into(), serde_json::Value::from(src));
        }
        let _ = self.ledger().append(&e);
        let _ = self.rate_check(actor, policy.limits.writes_per_minute, true);
        Ok(path)
    }

    pub(crate) fn ledger(&self) -> Ledger {
        Ledger::new(self.inner.vault.clone(), &self.inner.device.id)
    }

    pub(crate) fn provenance(&self, actor: &Actor, cs: &ChangeSet, op: Op, text: &str) -> serde_json::Value {
        let op = if cs.accepted_from.is_some() && matches!(op, Op::Write | Op::Create) { Op::Accept } else { op };
        let mut p = Provenance::new(actor.clone(), op, text, self.now_ms());
        p.accepted_from.clone_from(&cs.accepted_from);
        p.run.clone_from(&cs.run);
        p.sources.clone_from(&cs.sources);
        if self.untrusted_source(&cs.sources).is_some() {
            p.trust = Trust::Untrusted;
        }
        p.to_json()
    }

    /// Newest snapshot time of `path`, after a write.
    pub(crate) fn newest_snapshot(&self, path: &NotePath) -> Option<i64> {
        self.inner.vault.history(path).first().map(|s| s.at_ms)
    }

    fn apply(&self, actor: &Actor, cs: &ChangeSet, planned: Vec<Planned>) -> Result<SubmitOutcome, WriteError> {
        let vault = &self.inner.vault;
        let now = self.now_ms();
        let batch = (planned.len() > 1).then(|| BatchId::new(now).to_string());
        let mut versions = Vec::new();
        for p in planned {
            let mut entry = LedgerEntry::new(actor.clone(), Op::Write, now);
            entry.run.clone_from(&cs.run);
            entry.batch.clone_from(&batch);
            match p.change {
                Change::Create { path, text } => {
                    vault.storage().write_atomic(&path, &text)?;
                    let _ = vault.snapshot_with(&path, &text, now, true, Some(&self.provenance(actor, cs, Op::Create, &text)));
                    entry.op = Op::Create;
                    entry.after = Some(ContentHash::of(&text));
                    entry.snap_after = self.newest_snapshot(&path);
                    entry.bytes = Some(text.len() as u64);
                    versions.push((path.clone(), ContentHash::of(&text)));
                    entry.path = Some(path);
                }
                Change::Edit { path, new_text } => {
                    let before = p.before.unwrap_or_default();
                    if before != new_text {
                        // Keep what was there (if history doesn't have it yet), then the new version.
                        let _ = vault.snapshot(&path, &before, now - 1, true);
                        entry.snap_before = self.newest_snapshot(&path);
                        vault.storage().write_atomic(&path, &new_text)?;
                        let _ = vault.snapshot_with(&path, &new_text, now, true, Some(&self.provenance(actor, cs, Op::Write, &new_text)));
                        entry.snap_after = self.newest_snapshot(&path);
                    }
                    entry.before = Some(ContentHash::of(&before));
                    entry.after = Some(ContentHash::of(&new_text));
                    entry.bytes = Some(new_text.len() as u64);
                    versions.push((path.clone(), ContentHash::of(&new_text)));
                    entry.path = Some(path);
                }
                Change::Rename { from, to, update_links } => {
                    let text = p.before.unwrap_or_default();
                    let updated = self.rename_file(&from, &to, update_links)?;
                    // Links rewritten in other notes are part of the same batch.
                    let batch = batch.clone().or_else(|| (!updated.is_empty()).then(|| BatchId::new(now).to_string()));
                    entry.batch.clone_from(&batch);
                    for (other, old_text, new_text) in updated {
                        let mut e = LedgerEntry::new(actor.clone(), Op::Write, now).path(&other);
                        e.run.clone_from(&cs.run);
                        e.batch.clone_from(&batch);
                        let _ = vault.snapshot(&other, &old_text, now - 1, true);
                        e.snap_before = self.newest_snapshot(&other);
                        let _ = vault.snapshot_with(&other, &new_text, now, true, Some(&self.provenance(actor, cs, Op::Write, &new_text)));
                        e.snap_after = self.newest_snapshot(&other);
                        e.before = Some(ContentHash::of(&old_text));
                        e.after = Some(ContentHash::of(&new_text));
                        let _ = self.ledger().append(&e);
                    }
                    self.rename_hook(&from, &to);
                    entry.op = Op::Rename;
                    entry.before = Some(ContentHash::of(&text));
                    entry.after = Some(ContentHash::of(&text));
                    entry.to = Some(to.clone());
                    versions.push((to, ContentHash::of(&text)));
                    entry.path = Some(from);
                }
                Change::Delete { path } => {
                    let text = p.before.unwrap_or_default();
                    let _ = vault.snapshot(&path, &text, now - 1, true);
                    entry.snap_before = self.newest_snapshot(&path);
                    vault.trash(&path)?;
                    entry.op = Op::Delete;
                    entry.before = Some(ContentHash::of(&text));
                    entry.path = Some(path);
                }
                Change::SetProperties { .. } => {} // planned as an edit
            }
            if let Some(from) = &cs.accepted_from {
                // Approved in the agent's app: the person accepted the agent's change.
                if matches!(entry.op, Op::Write | Op::Create) {
                    entry.op = Op::Accept;
                }
                entry.from = Some(from.id.clone());
            }
            // The ledger is a record, not the write: a failure to log never undoes the change.
            let _ = self.ledger().append(&entry);
        }
        Ok(SubmitOutcome::Applied { versions })
    }

    /// Rename (or move) a file; with `update_links`, rewrite links to it in
    /// other notes. Returns the other notes rewritten, with their text
    /// before and after.
    pub(crate) fn rename_file(
        &self,
        from: &NotePath,
        to: &NotePath,
        update_links: bool,
    ) -> Result<Vec<(NotePath, String, String)>, WriteError> {
        let vault = &self.inner.vault;
        if !update_links {
            vault.storage().rename(from, to)?;
            vault.move_history(from, to)?;
            return Ok(Vec::new());
        }
        let notes = vault.notes().unwrap_or_default();
        let before: HashMap<NotePath, String> =
            notes.iter().filter(|p| *p != from).filter_map(|p| Some((p.clone(), vault.read(p).ok()?))).collect();
        let mut updated = Vec::new();
        let mut report = if from.folder() == to.folder() {
            vault.rename_note(from, to.stem())?
        } else {
            let moved = vault.move_note(from, to.folder())?;
            if moved.new_path == *to {
                moved
            } else {
                let mut r = vault.rename_note(&moved.new_path, to.stem())?;
                r.updated.extend(moved.updated);
                r
            }
        };
        report.updated.sort();
        report.updated.dedup();
        for p in report.updated.iter().filter(|p| *p != to) {
            if let (Some(old), Ok(new)) = (before.get(p), vault.read(p)) {
                updated.push((p.clone(), old.clone(), new));
            }
        }
        Ok(updated)
    }

    /// Proposals of `actor` still waiting, newest first.
    pub(crate) fn own_pending(&self, actor: &Actor) -> Vec<Proposal> {
        let mut v: Vec<Proposal> = ProposalStore::new(self.inner.vault.clone())
            .list()
            .into_iter()
            // Only proposals made here: another device's (shared review
            // history) are amended there, by the agent that made them.
            .filter(|p| {
                p.status == ProposalStatus::Pending && p.actor.id == actor.id && p.origin().is_none_or(|(d, _)| d == self.device().id)
            })
            .collect();
        v.sort_by_key(|p| std::cmp::Reverse((p.updated, p.created)));
        v
    }

    /// The text of `path` with `actor`'s own pending proposal applied, and
    /// its version: what `view` with `include_pending` shows the agent.
    pub fn pending_text(&self, actor: &Actor, path: &NotePath) -> Option<(String, ContentHash)> {
        self.own_pending(actor).into_iter().find_map(|p| {
            p.changes.iter().find_map(|c| {
                if c.state.is_some() || c.path.as_ref() != Some(path) {
                    return None;
                }
                let t = c.new_text.clone().or_else(|| c.text.clone())?;
                let h = ContentHash::of(&t);
                Some((t, h))
            })
        })
    }

    fn proposal_change(p: &Planned) -> ProposalChange {
        match &p.change {
            Change::Create { path, text } => ProposalChange::create(path, text),
            Change::Edit { path, new_text } => ProposalChange::edit(path, p.before.as_deref().unwrap_or_default(), new_text),
            Change::Rename { from, to, update_links } => ProposalChange::rename(from, to, *update_links),
            Change::Delete { path } => ProposalChange::delete(path),
            Change::SetProperties { path, .. } => {
                ProposalChange::edit(path, p.before.as_deref().unwrap_or_default(), p.after.as_deref().unwrap_or_default())
            }
        }
    }

    /// Fold `planned` into `prop` (the same agent's pending proposal).
    fn amend(prop: &mut Proposal, planned: &[Planned], expected: &[(NotePath, ContentHash)]) {
        for p in planned {
            let path = p.change.path().clone();
            let existing = prop.changes.iter_mut().find(|c| c.state.is_none() && c.paths().contains(&path));
            match (existing, &p.change) {
                (Some(c), Change::Edit { new_text, .. }) if c.op == crate::proposal::ChangeOp::Edit => {
                    let pending = c.new_text.clone().unwrap_or_default();
                    let merged = if let Some((set, remove)) = &p.props {
                        // A property change applies to the pending text as an operation.
                        set_props(&pending, set, remove)
                    } else if expected.iter().any(|(ep, h)| *ep == path && *h == ContentHash::of(&pending)) {
                        // Written on top of what it proposed (read with `include_pending`).
                        new_text.clone()
                    } else {
                        // Written on top of the file: keep both its changes when they don't overlap.
                        let disk = p.before.as_deref().unwrap_or_default();
                        let r = quire_text::merge::merge3(disk, &pending, new_text);
                        if r.is_clean() { r.text } else { new_text.clone() }
                    };
                    c.new_text = Some(merged);
                    c.review = None;
                    // What the person rejected stays rejected, also reworded
                    // or with the lines around it rewritten.
                    c.drop_rejected();
                }
                (Some(c), Change::Create { text, .. }) if c.op == crate::proposal::ChangeOp::Create => {
                    c.text = Some(text.clone());
                }
                (Some(c), Change::Delete { .. }) => {
                    // Deleting what it proposed to change: the delete replaces the edit.
                    *c = Self::proposal_change(p);
                }
                _ => prop.changes.push(Self::proposal_change(p)),
            }
        }
    }

    /// A new proposal of `actor` keeps out what the person rejected from
    /// that actor's suggestions for the same notes in the last 30 days (on
    /// this device or, with shared review history, another): the same
    /// suggestion in a new run isn't asked about again. A change left with
    /// nothing is settled as rejected at once.
    fn carry_rejections(&self, actor: &Actor, prop: &mut Proposal) {
        use crate::proposal::{ChangeOp, ChangeState, HunkAction};
        let now = self.now_ms();
        let past: Vec<Proposal> = ProposalStore::new(self.inner.vault.clone())
            .archived(now)
            .into_iter()
            .filter(|p| p.actor.id == actor.id && now - p.updated <= crate::ops::EXPIRE_AFTER_MS)
            .collect();
        if past.is_empty() {
            return;
        }
        for c in prop.changes.iter_mut().filter(|c| c.op == ChangeOp::Edit) {
            let Some(path) = c.path.clone() else { continue };
            for old in past.iter().flat_map(|p| &p.changes).filter(|o| o.subject() == Some(&path)) {
                for d in old.decided.iter().filter(|d| d.action == HunkAction::Rejected) {
                    if !c.decided.contains(d) {
                        c.decided.push(d.clone());
                    }
                }
            }
            if c.drop_rejected() && c.new_text == c.base_text() {
                c.set_state(ChangeState::Rejected, &self.inner.device.id, now);
            }
        }
        if let Some(s) = prop.settled_status() {
            prop.status = s;
        }
    }

    fn propose(&self, actor: &Actor, cs: &ChangeSet, planned: &[Planned], max_pending: u32) -> Result<SubmitOutcome, WriteError> {
        let now = self.now_ms();
        let store = ProposalStore::new(self.inner.vault.clone());
        let paths: Vec<NotePath> = planned.iter().map(|p| p.change.path().clone()).collect();
        let own = self.own_pending(actor);
        let target = own
            .into_iter()
            .find(|p| p.changes.iter().any(|c| c.state.is_none() && c.paths().iter().any(|x| paths.contains(x))))
            .map(|p| p.id);
        let untrusted = self.untrusted_source(&cs.sources);
        // Amend the latest version of the proposal, and only if nobody
        // decided on it meanwhile (the desktop reviews it in another
        // process): otherwise read it again and amend that.
        let mut amended_prop = None;
        for _ in 0..8 {
            let Some(mut prop) = target.as_ref().and_then(|id| store.load(id)).filter(|p| p.status == ProposalStatus::Pending) else {
                break;
            };
            let seen = prop.updated;
            Self::amend(&mut prop, planned, &cs.expected);
            prop.updated = now.max(seen + 1);
            if !cs.rationale.trim().is_empty() && !prop.rationale.contains(cs.rationale.trim()) {
                if !prop.rationale.is_empty() {
                    prop.rationale.push_str("\n\n");
                }
                prop.rationale.push_str(cs.rationale.trim());
            }
            for s in &cs.sources {
                if !prop.sources.contains(s) {
                    prop.sources.push(s.clone());
                }
            }
            if untrusted.is_some() {
                prop.trust = Trust::Untrusted;
                prop.untrusted_source = prop.untrusted_source.clone().or_else(|| untrusted.clone());
            }
            if store.save_if(&prop, seen)? {
                amended_prop = Some(prop);
                break;
            }
        }
        let (proposal, amended) = if let Some(prop) = amended_prop {
            (prop, true)
        } else {
            let pending =
                self.proposals(&ProposalFilter { status: Some(ProposalStatus::Pending), actor: Some(actor.id.clone()), path: None }).len();
            if actor.kind == ActorKind::Agent && max_pending > 0 && pending >= max_pending as usize {
                return Err(WriteError::InboxFull);
            }
            let title = if cs.title.trim().is_empty() {
                match planned {
                    [one] => format!("Changes to {}", one.change.path().stem()),
                    many => format!("Changes to {} notes", many.len()),
                }
            } else {
                cs.title.trim().to_string()
            };
            let prop = Proposal {
                format: crate::proposal::PROPOSAL_FORMAT,
                id: ProposalId::new(now),
                created: now,
                updated: now,
                status: ProposalStatus::Pending,
                actor: actor.clone(),
                run: cs.run.clone(),
                title,
                rationale: cs.rationale.clone(),
                sources: cs.sources.clone(),
                trust: if untrusted.is_some() { Trust::Untrusted } else { Trust::Trusted },
                untrusted_source: untrusted,
                changes: planned.iter().map(Self::proposal_change).collect(),
                other: crate::actor::Other::new(),
            };
            let mut prop = prop;
            self.carry_rejections(actor, &mut prop);
            store.save(&prop)?;
            if prop.status != ProposalStatus::Pending {
                let _ = store.archive(&prop.id);
            }
            (prop, false)
        };
        for p in planned {
            let mut e = LedgerEntry::new(actor.clone(), Op::Propose, now).path(p.change.path());
            e.proposal = Some(proposal.id.clone());
            e.run.clone_from(&cs.run);
            e.before = p.before.as_deref().map(ContentHash::of);
            e.after = p.after.as_deref().map(ContentHash::of);
            let _ = self.ledger().append(&e);
        }
        Ok(SubmitOutcome::Proposed { id: proposal.id, amended })
    }

    pub fn proposals(&self, filter: &ProposalFilter) -> Vec<ProposalSummary> {
        ProposalStore::new(self.inner.vault.clone()).list().iter().map(Proposal::summary).filter(|s| filter.matches(s)).collect()
    }

    pub fn proposal(&self, id: &ProposalId) -> Option<Proposal> {
        ProposalStore::new(self.inner.vault.clone()).load(id)
    }

    /// Append to this device's ledger.
    pub fn record(&self, entry: &LedgerEntry) {
        let _ = self.ledger().append(entry);
    }

    /// Ledger entries of this device and, with shared review history, of
    /// the other devices (read-only copies), oldest first.
    pub fn log(&self, q: &LedgerQuery) -> Vec<LedgerEntry> {
        let mut v = self.ledger().read(q);
        let others = self.shared_log(q);
        if !others.is_empty() {
            v.extend(others);
            v.sort_by(|a, b| a.at.cmp(&b.at).then_with(|| a.actor.device.cmp(&b.actor.device)));
        }
        v
    }

    /// Ledger entries of this device only.
    pub fn log_here(&self, q: &LedgerQuery) -> Vec<LedgerEntry> {
        self.ledger().read(q)
    }

    /// The tags of the note at `path` (front matter and inline), read from
    /// the file itself and kept while its modification time stays.
    fn tags_of(&self, path: &NotePath) -> Vec<String> {
        if !path.is_markdown() {
            return Vec::new();
        }
        let storage = self.inner.vault.storage();
        let stamp = storage.modified(path).ok();
        let mut cache = self.inner.tags.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some((t, tags)) = cache.get(path)
            && *t == stamp
            && stamp.is_some()
        {
            return tags.clone();
        }
        let tags = storage.read(path).map(|text| quire_markdown::extract::extract(&text).tags).unwrap_or_default();
        if cache.len() > 50_000 {
            cache.clear();
        }
        cache.insert(path.clone(), (stamp, tags.clone()));
        tags
    }

    /// Whether `actor`'s reads are logged: the device logs reads, or its
    /// policy entry asks for it (a revoked client).
    pub fn logs_reads_of(&self, actor: &Actor) -> bool {
        actor.kind == ActorKind::Agent
            && (self.device_state().log_reads
                || self.policy_state().active().iter().any(|p| p.client(&actor.id).is_some_and(|c| c.log_reads)))
    }

    /// Delete this device's agent suggestions (pending and archived) that
    /// touch a note under `folder`: they hold its text, and the folder was
    /// just locked (0.11). Returns how many went.
    pub fn forget_suggestions_under(&self, folder: &str) -> usize {
        let prefix = format!("{}/", folder.trim_matches('/').to_lowercase());
        let touches = |p: &Proposal| p.changes.iter().any(|c| c.paths().iter().any(|x| x.as_str().to_lowercase().starts_with(&prefix)));
        let store = ProposalStore::new(self.inner.vault.clone());
        let mut n = 0;
        for p in store.list().iter().filter(|p| touches(p)) {
            // Archived, then the archived copy deleted: nothing of it is left.
            if store.archive(&p.id).is_ok() {
                store.remove_archived(p);
                n += 1;
            }
        }
        for p in store.archived(self.now_ms()).iter().filter(|p| touches(p)) {
            store.remove_archived(p);
            n += 1;
        }
        n
    }

    /// Why `client` (a Quire-assigned key) may not connect at all: it was
    /// revoked, or AI is off for the vault. Checked when a stdio server starts.
    pub fn refused(&self, client: &str) -> Option<String> {
        let st = self.policy_state();
        if st.active().iter().any(|p| p.ai_off()) {
            return Some("AI is off for this vault in Quire (Settings → Agents). No agent can use it until it is turned back on.".into());
        }
        if st.active().iter().any(|p| p.revoked(client)) {
            let key = client.strip_suffix('?').unwrap_or(client);
            return Some(format!(
                "Access for {key} was revoked in Quire (Settings → Agents → Connections). Restore it there to connect again."
            ));
        }
        None
    }

    /// Record that `actor` read `path`, if this device logs reads.
    pub fn record_read(&self, actor: &Actor, path: &NotePath) {
        if self.logs_reads_of(actor) {
            self.record(&LedgerEntry::new(actor.clone(), Op::Read, self.now_ms()).path(path));
        }
    }

    pub fn inbox(&self) -> InboxStore {
        InboxStore::new(self.inner.vault.clone(), Arc::clone(&self.inner.clock))
    }

    // ------------------------------------------------------------ instructions

    /// Approve the vault's agent instructions the person was shown
    /// (`shown`: their hash). Refused when they changed since (P7).
    pub fn approve_instructions(&self, shown: &ContentHash) -> Result<(), String> {
        let Some(i) = crate::instructions::find(&self.inner.vault) else { return Ok(()) };
        let text = self.inner.vault.read(&i).map_err(|e| e.to_string())?;
        if ContentHash::of(&text) != *shown {
            return Err(format!("{i} changed since it was shown. Look at it again before approving it."));
        }
        let state = self.state();
        state.write_copy(APPROVED_INSTRUCTIONS_FILE, &text)?;
        state.update(|s| s.approved_instructions = Some(ContentHash::of(&text)))?;
        self.approvals_changed();
        Ok(())
    }

    pub(crate) fn approved_instructions_text(&self) -> Option<String> {
        let st = self.state();
        let h = st.load().approved_instructions?;
        st.read_copy(APPROVED_INSTRUCTIONS_FILE).filter(|t| ContentHash::of(t) == h)
    }

    /// A `.quire/agent/` folder's notes changed outside this process: forget caches.
    pub fn refresh(&self) {
        *self.inner.policy.write().unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    }
}

/// Set and remove front-matter properties.
pub(crate) fn set_props(text: &str, set: &[(String, quire_markdown::Value)], remove: &[String]) -> String {
    let mut text = text.to_string();
    for (k, v) in set {
        text = frontmatter::set_field(&text, k, v).apply(&text);
    }
    for k in remove {
        if let Some(e) = frontmatter::remove_field(&text, k) {
            text = e.apply(&text);
        }
    }
    text
}

/// `%20` → space, per byte; invalid sequences are kept as written.
pub(crate) fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 2 < b.len()
            && let Some(v) = std::str::from_utf8(&b[i + 1..i + 3]).ok().and_then(|h| u8::from_str_radix(h, 16).ok())
        {
            out.push(v);
            i += 3;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}
