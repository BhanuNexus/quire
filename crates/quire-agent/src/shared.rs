// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Review history shared between devices (0.10; design
//! `docs/design/0.10-agentsync.md`, format `docs/formats/shared-review.md`).
//!
//! Proposals and the ledger live in `.quire/agent/`, which never syncs. An
//! agent that writes through `quire-cli serve` on a server leaves its
//! suggestions and its run history there. With "Also sync vault settings
//! and review history" on, each device publishes copies under
//! `.quire/shared/agent/`, which travels in the settings lane:
//!
//! ```text
//! .quire/shared/agent/devices/<device>.json              its id and name
//! .quire/shared/agent/proposals/<device>/<id>.json       proposals its agents made
//! .quire/shared/agent/decisions/<device>/<id>.json       what was decided on it, per proposal
//! .quire/shared/agent/ledger/<device>/<yyyy-mm>.jsonl    its ledger, without reads
//! .quire/shared/agent/texts/<device>/<yyyy-mm>.jsonl     texts its changes need to be undone
//! ```
//!
//! Every file has one writer, the device in its path, so two devices never
//! write the same file and sync never has to merge one.
//!
//! **Other devices' files are untrusted input** (0.10.1 security review
//! #2 and #11). Every device holds the vault key, so any of them could
//! write any file here. Until per-device signatures exist (future work,
//! `docs/design/0.10-agentsync.md`), this device checks what it can:
//!
//! * a device is the folder its files are in: a device file must be named
//!   after the id inside it, a proposal's origin and actor must be that
//!   device, a ledger line's actor too, and a decision is recorded with the
//!   device whose folder it came from (never with this device);
//! * every path in another device's proposal or ledger goes through the
//!   same check as a path an agent names ([`crate::vault_file`]: canonical,
//!   no hidden folder or `.quire`, inside the vault through symlinks); a
//!   proposal with one bad path is not taken in, a ledger line is dropped;
//! * a mirrored proposal's trust can only be lowered here, never raised;
//! * what this device exports is built from its own local state, never
//!   from the shared copy of its folder (which another device could edit);
//! * a waiting proposal settled by another device's decisions keeps who
//!   decided ("decided on …" in the Inbox), and the person can reopen it
//!   ([`AgentStore::reopen_proposal`]); decisions made elsewhere before the
//!   reopening are then ignored here.
//!
//! * [`AgentStore::export_shared`] (before each sync pass) writes this
//!   device's files from its local state.
//! * [`AgentStore::import_shared`] (after each pass) brings other devices'
//!   proposals into the local store as **mirrors** (`origin` names the
//!   device), and merges every device's decisions into the local copies.
//!
//! **Convergence.** Decisions are events, kept per device and only ever
//! added. Each device merges them the same way: per hunk, one that wrote
//! text (accepted or edited) wins over a rejection, then the earliest
//! (time, device); per change, the state of highest rank (applied, partly
//! applied, rejected, superseded). So every device ends with the same
//! outcome whatever order the files arrived in. A decided hunk stays
//! decided: it is hidden by id and by content.
//!
//! **Text is applied once.** Only the device where the person decides
//! writes the note; importing a decision never writes a note. The note
//! then syncs like any edit; on the other devices the agent's lines are
//! already there (the rebase shows nothing) or the hunk is hidden as
//! decided. Two devices accepting the same hunk at once make the same
//! edit, which sync's three-way merge takes once.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Write};

use base64::Engine as _;
use quire_vault::{ContentHash, NotePath};
use serde::{Deserialize, Serialize};

use crate::actor::{Op, Other};
use crate::ids::ProposalId;
use crate::ledger::{LEDGER_DIR, LedgerEntry, LedgerQuery};
use crate::proposal::{ChangeOp, ChangeState, HunkAction, HunkDecision, PROPOSAL_FORMAT, Proposal, ProposalStatus, ProposalStore};
use crate::review::DecidedContent;
use crate::store::AgentStore;

/// Where shared review history lives (inside the settings lane).
pub const SHARED_AGENT_DIR: &str = ".quire/shared/agent";
pub(crate) const DEVICES_DIR: &str = ".quire/shared/agent/devices";
pub(crate) const PROPOSALS_DIR: &str = ".quire/shared/agent/proposals";
pub(crate) const DECISIONS_DIR: &str = ".quire/shared/agent/decisions";
/// Other devices' ledgers, read-only here.
pub const SHARED_LEDGER_DIR: &str = ".quire/shared/agent/ledger";
pub(crate) const TEXTS_DIR: &str = ".quire/shared/agent/texts";
/// What this device last exported as its decisions, per proposal. Local
/// (never synced): exports add to this, not to the shared copy, which
/// another device could have edited.
pub(crate) const SENT_DECISIONS_DIR: &str = ".quire/agent/sent-decisions";
/// The format of every JSON file here; a reader skips higher ones.
pub const SHARED_FORMAT: u32 = 1;

/// A proposal file bigger than this is stored gzipped (`packed`).
pub const PACK_ABOVE: usize = 256 * 1024;
/// No shared file grows past this.
///
/// Sync skips files over 10 MB, and a file that grew past that limit would
/// look deleted to every device, so the limit here is well below it. A
/// proposal that doesn't fit stays on its device (reviewed there).
pub const MAX_FILE: usize = 4 * 1024 * 1024;
/// Texts carried for undoing changes: each at most this big.
pub const MAX_TEXT: usize = 256 * 1024;
/// Resolved proposals (and their decisions) stay shared this long.
pub const KEEP_RESOLVED_MS: i64 = 30 * 86_400_000;
/// Ledger months shared: this one and the two before it.
const LEDGER_MONTHS: usize = 3;
/// Texts are carried for changes this recent.
const TEXT_DAYS: i64 = 35;

/// A device id as it may appear in a path: `d_` and letters, digits, `_`, `-`.
pub fn valid_device(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

pub(crate) fn month_of(at_ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(at_ms).map_or_else(|| "1970-01".into(), |d| d.format("%Y-%m").to_string())
}

/// `now`'s month and the `n - 1` before it, newest first.
fn recent_months(now_ms: i64, n: usize) -> Vec<String> {
    let now = chrono::DateTime::from_timestamp_millis(now_ms).unwrap_or_default();
    let (mut y, mut m) = (chrono::Datelike::year(&now), chrono::Datelike::month(&now));
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        out.push(format!("{y:04}-{m:02}"));
        if m == 1 {
            (y, m) = (y - 1, 12);
        } else {
            m -= 1;
        }
    }
    out
}

pub(crate) fn gzip_b64(bytes: &[u8]) -> String {
    let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    let _ = e.write_all(bytes);
    base64::engine::general_purpose::STANDARD.encode(e.finish().unwrap_or_default())
}

fn gunzip_b64(s: &str) -> Option<Vec<u8>> {
    let bytes = base64::engine::general_purpose::STANDARD.decode(s).ok()?;
    let mut out = Vec::new();
    flate2::read::GzDecoder::new(bytes.as_slice()).take(64 * 1024 * 1024).read_to_end(&mut out).ok()?;
    Some(out)
}

/// `devices/<id>.json`: who a device id is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct DeviceFile {
    format: u32,
    id: String,
    #[serde(default)]
    name: String,
    #[serde(flatten)]
    other: Other,
}

/// One hunk decision in a decisions file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct SharedHunk {
    path: NotePath,
    hunk: String,
    action: HunkAction,
    at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    text: Option<String>,
}

/// What a decided hunk held, in a decisions file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct SharedDecided {
    path: NotePath,
    #[serde(flatten)]
    content: DecidedContent,
}

/// A whole change settled on a device (created, renamed, deleted, or every
/// hunk decided).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct SharedState {
    /// The change's subject (the old path of a rename).
    path: NotePath,
    op: ChangeOp,
    state: ChangeState,
    at: i64,
}

/// `decisions/<device>/<proposal>.json`: everything decided on one device
/// about one proposal. Events are only ever added.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct DecisionsFile {
    format: u32,
    proposal: ProposalId,
    device: String,
    /// Unix ms of the newest event.
    updated: i64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    hunks: Vec<SharedHunk>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    decided: Vec<SharedDecided>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    changes: Vec<SharedState>,
    #[serde(flatten)]
    other: Other,
}

impl DecisionsFile {
    fn is_empty(&self) -> bool {
        self.hunks.is_empty() && self.decided.is_empty() && self.changes.is_empty()
    }

    /// Add `other`'s events that aren't here yet. True when something was added.
    fn absorb(&mut self, other: Self) -> bool {
        let n = self.hunks.len() + self.decided.len() + self.changes.len();
        for h in other.hunks {
            if !self.hunks.contains(&h) {
                self.hunks.push(h);
            }
        }
        for d in other.decided {
            if !self.decided.contains(&d) {
                self.decided.push(d);
            }
        }
        for c in other.changes {
            if !self.changes.contains(&c) {
                self.changes.push(c);
            }
        }
        self.updated = self.updated.max(other.updated);
        n != self.hunks.len() + self.decided.len() + self.changes.len()
    }
}

/// A text a change needs to be undone, in `texts/<device>/<month>.jsonl`.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct TextLine {
    v: u32,
    hash: ContentHash,
    gz: String,
}

/// What [`AgentStore::import_shared`] did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ImportReport {
    /// Proposals from other devices new here.
    pub added: usize,
    /// Proposals whose text changed on their device (the agent amended them).
    pub updated: usize,
    /// Proposals that got decisions made on another device.
    pub decided: usize,
    /// Proposals resolved here by those decisions.
    pub settled: usize,
}

impl ImportReport {
    pub const fn changed_anything(&self) -> bool {
        self.added + self.updated + self.decided + self.settled > 0
    }
}

/// The ids of devices that published review history, except `me`.
pub(crate) fn other_devices(vault: &quire_vault::Vault, me: &str) -> Vec<String> {
    vault
        .storage()
        .list_under(DEVICES_DIR)
        .unwrap_or_default()
        .iter()
        .filter_map(|f| f.file_name().strip_suffix(".json").map(str::to_string))
        .filter(|id| valid_device(id) && id != me)
        .collect()
}

/// The fields of a proposal another device takes over when it changed on
/// its own device (not the decisions, which travel separately).
fn body_hash(p: &Proposal) -> ContentHash {
    let changes: Vec<serde_json::Value> = p
        .changes
        .iter()
        .map(|c| serde_json::json!([c.op, c.path, c.from, c.to, c.update_links, c.base, c.base_text, c.base_text_gz, c.new_text, c.text]))
        .collect();
    let v = serde_json::json!([p.status, p.title, p.rationale, p.sources, p.trust, changes]);
    ContentHash::of(&v.to_string())
}

/// The change of `p` that `subject`/`op` names.
fn change_mut<'a>(p: &'a mut Proposal, path: &NotePath, op: Option<ChangeOp>) -> Option<&'a mut crate::proposal::ProposalChange> {
    p.changes
        .iter_mut()
        .find(|c| c.subject() == Some(path) && op.is_none_or(|o| o == c.op || (o == ChangeOp::Create && c.op == ChangeOp::Edit)))
}

/// Same decision (whichever device stored it).
fn same_decision(a: &HunkDecision, b: &HunkDecision) -> bool {
    a.hunk == b.hunk && a.action == b.action && a.at == b.at && a.device() == b.device()
}

impl AgentStore {
    fn put(&self, path: &NotePath, bytes: &[u8]) -> bool {
        let st = self.vault().storage();
        if st.read_bytes(path).ok().as_deref() == Some(bytes) {
            return false;
        }
        st.write_bytes_atomic(path, bytes).is_ok()
    }

    /// Remove files under `folder` not in `keep` (by file name).
    fn prune(&self, folder: &str, keep: &BTreeSet<String>) {
        let st = self.vault().storage();
        for f in st.list_under(folder).unwrap_or_default() {
            if !keep.contains(f.file_name()) {
                let _ = st.remove(&f);
            }
        }
    }

    /// Proposals this device shares or merges into: waiting, and resolved
    /// in the last [`KEEP_RESOLVED_MS`].
    fn recent_proposals(&self) -> Vec<(Proposal, bool)> {
        let store = ProposalStore::new(self.vault().clone());
        let now = self.now_ms();
        let mut v: Vec<(Proposal, bool)> = store.list().into_iter().map(|p| (p, false)).collect();
        v.extend(store.archived(now).into_iter().filter(|p| now - p.updated <= KEEP_RESOLVED_MS).map(|p| (p, true)));
        v
    }

    /// Whether `p` was made on this device (not a mirror of another's).
    fn made_here(&self, p: &Proposal) -> bool {
        p.origin().is_none_or(|(id, _)| id == self.device().id)
    }

    /// Write this device's review history into `.quire/shared/agent/` for
    /// other devices (before a sync pass, when shared state is on).
    /// Returns how many files changed.
    pub fn export_shared(&self) -> usize {
        let me = self.device().id.clone();
        if !valid_device(&me) {
            return 0;
        }
        let mut n = 0;
        let st = self.vault().storage();
        // Never for git: the vault's own git repository keeps `.quire/shared/`.
        let ignore = NotePath::new(format!("{SHARED_AGENT_DIR}/.gitignore"));
        if !st.exists(&ignore) {
            let _ = st.write_atomic(&ignore, "# Quire: review history shared through Quire Sync. Not for git.\n*\n");
        }
        let dev = DeviceFile { format: SHARED_FORMAT, id: me.clone(), name: self.device().name.clone(), other: Other::new() };
        if let Ok(bytes) = serde_json::to_vec_pretty(&dev) {
            n += usize::from(self.put(&NotePath::new(format!("{DEVICES_DIR}/{me}.json")), &bytes));
        }
        let recent = self.recent_proposals();
        n += self.export_proposals(&me, &recent);
        n += self.export_decisions(&me, &recent);
        n += self.export_ledger(&me);
        n += self.export_texts(&me);
        n
    }

    fn export_proposals(&self, me: &str, recent: &[(Proposal, bool)]) -> usize {
        let folder = format!("{PROPOSALS_DIR}/{me}");
        let mut keep = BTreeSet::new();
        let mut n = 0;
        for (p, _) in recent.iter().filter(|(p, _)| self.made_here(p)) {
            let mut out = p.clone();
            for c in &mut out.changes {
                c.review = None;
            }
            out.other.insert("origin".into(), serde_json::json!({"id": me, "name": self.device().name}));
            let Ok(mut bytes) = serde_json::to_vec(&out) else { continue };
            if bytes.len() > PACK_ABOVE {
                let packed = serde_json::json!({
                    "format": PROPOSAL_FORMAT, "id": out.id, "updated": out.updated, "status": out.status,
                    "origin": {"id": me}, "packed": gzip_b64(&bytes),
                });
                bytes = packed.to_string().into_bytes();
            }
            if bytes.len() > MAX_FILE {
                // Too big to share: reviewed on its own device.
                continue;
            }
            let name = format!("{}.json", p.id);
            n += usize::from(self.put(&NotePath::new(format!("{folder}/{name}")), &bytes));
            keep.insert(name);
        }
        self.prune(&folder, &keep);
        n
    }

    fn export_decisions(&self, me: &str, recent: &[(Proposal, bool)]) -> usize {
        let folder = format!("{DECISIONS_DIR}/{me}");
        let st = self.vault().storage();
        let now = self.now_ms();
        let mut keep = BTreeSet::new();
        let mut n = 0;
        for (p, _) in recent {
            let here = self.made_here(p);
            let mine = |dev: Option<&str>| dev.map_or(here, |d| d == me);
            let mut f = DecisionsFile {
                format: SHARED_FORMAT,
                proposal: p.id.clone(),
                device: me.to_string(),
                updated: 0,
                hunks: Vec::new(),
                decided: Vec::new(),
                changes: Vec::new(),
                other: Other::new(),
            };
            for c in &p.changes {
                let Some(path) = c.subject().cloned() else { continue };
                for d in c.decisions.iter().filter(|d| mine(d.device())) {
                    f.hunks.push(SharedHunk { path: path.clone(), hunk: d.hunk.clone(), action: d.action, at: d.at, text: d.text.clone() });
                    f.updated = f.updated.max(d.at);
                }
                for d in c.decided.iter().filter(|d| mine(d.device.as_deref())) {
                    let mut content = d.clone();
                    content.device = Some(me.to_string());
                    f.decided.push(SharedDecided { path: path.clone(), content });
                }
                if let Some(state) = c.state
                    && mine(c.state_by().map(|(d, _)| d))
                {
                    let at = c.state_by().map_or(p.updated, |(_, at)| at);
                    f.changes.push(SharedState { path, op: c.op, state, at });
                    f.updated = f.updated.max(at);
                }
            }
            let name = format!("{}.json", p.id);
            let file = NotePath::new(format!("{folder}/{name}"));
            let sent = NotePath::new(format!("{SENT_DECISIONS_DIR}/{name}"));
            // Events are only added: keep what an earlier export wrote (an
            // undone acceptance stays decided on the other devices). From
            // this device's own record; the shared copy only before there
            // was one (an older Quire), and then only events of this device.
            let earlier = st.read_bytes(&sent).ok().or_else(|| st.read_bytes(&file).ok());
            if let Some(old) =
                earlier.and_then(|b| serde_json::from_slice::<DecisionsFile>(&b).ok()).filter(|o| o.device == me && o.proposal == p.id)
            {
                f.absorb(old);
            }
            if f.is_empty() {
                continue;
            }
            keep.insert(name);
            if let Ok(bytes) = serde_json::to_vec(&f)
                && bytes.len() <= MAX_FILE
            {
                n += usize::from(self.put(&file, &bytes));
                self.put(&sent, &bytes);
            }
        }
        // Older decisions stay while recent; then they go with their proposal.
        for f in st.list_under(&folder).unwrap_or_default() {
            if keep.contains(f.file_name()) {
                continue;
            }
            let recent_file = st
                .read_bytes(&f)
                .ok()
                .and_then(|b| serde_json::from_slice::<DecisionsFile>(&b).ok())
                .is_some_and(|d| now - d.updated <= KEEP_RESOLVED_MS);
            if !recent_file {
                let _ = st.remove(&f);
                let _ = st.remove(&NotePath::new(format!("{SENT_DECISIONS_DIR}/{}", f.file_name())));
            }
        }
        n
    }

    /// The ledger of this device without reads, for the last three months,
    /// each month at most [`MAX_FILE`] (the newest lines kept).
    fn export_ledger(&self, me: &str) -> usize {
        let st = self.vault().storage();
        let folder = format!("{SHARED_LEDGER_DIR}/{me}");
        let mut keep = BTreeSet::new();
        let mut n = 0;
        for month in recent_months(self.now_ms(), LEDGER_MONTHS) {
            let Ok(text) = st.read(&NotePath::new(format!("{LEDGER_DIR}/{me}/{month}.jsonl"))) else { continue };
            let mut lines: Vec<&str> = text
                .lines()
                .filter(|l| {
                    serde_json::from_str::<serde_json::Value>(l).is_ok_and(|v| v.get("op").and_then(|o| o.as_str()) != Some("read"))
                })
                .collect();
            let mut size: usize = lines.iter().map(|l| l.len() + 1).sum();
            let mut skip = 0;
            while size > MAX_FILE && skip < lines.len() {
                size -= lines[skip].len() + 1;
                skip += 1;
            }
            lines.drain(..skip);
            if lines.is_empty() {
                continue;
            }
            let mut out = lines.join("\n");
            out.push('\n');
            let name = format!("{month}.jsonl");
            n += usize::from(self.put(&NotePath::new(format!("{folder}/{name}")), out.as_bytes()));
            keep.insert(name);
        }
        self.prune(&folder, &keep);
        n
    }

    /// Texts that recent changes on this device need to be undone on
    /// another device (whose history may not have every version: two agent
    /// writes between syncs, or a pruned history). Each text at most
    /// [`MAX_TEXT`], each month's file at most [`MAX_FILE`].
    fn export_texts(&self, me: &str) -> usize {
        let st = self.vault().storage();
        let folder = format!("{TEXTS_DIR}/{me}");
        let now = self.now_ms();
        let months = recent_months(now, 2);
        let mut files: BTreeMap<String, String> = BTreeMap::new();
        let mut have: BTreeSet<ContentHash> = BTreeSet::new();
        for m in &months {
            let text = st.read(&NotePath::new(format!("{folder}/{m}.jsonl"))).unwrap_or_default();
            for l in text.lines() {
                if let Ok(t) = serde_json::from_str::<TextLine>(l) {
                    have.insert(t.hash);
                }
            }
            files.insert(m.clone(), text);
        }
        let mut changed: BTreeSet<String> = BTreeSet::new();
        let since = now - TEXT_DAYS * 86_400_000;
        for e in self.ledger().read(&LedgerQuery { since: Some(since), ..LedgerQuery::default() }) {
            if !matches!(e.op, Op::Write | Op::Create | Op::Delete | Op::Revert | Op::Accept) {
                continue;
            }
            let Some(path) = e.path.clone() else { continue };
            let month = month_of(e.at);
            let Some(file) = files.get_mut(&month) else { continue };
            for (snap, hash) in [(e.snap_before, e.before.clone()), (e.snap_after, e.after.clone())] {
                let (Some(snap), Some(hash)) = (snap, hash) else { continue };
                if have.contains(&hash) {
                    continue;
                }
                let Some(s) = self.vault().history(&path).into_iter().find(|s| s.at_ms == snap) else { continue };
                let Ok(text) = self.vault().read_snapshot(&s) else { continue };
                if text.len() > MAX_TEXT || ContentHash::of(&text) != hash {
                    continue;
                }
                let line = serde_json::to_string(&TextLine { v: 1, hash: hash.clone(), gz: gzip_b64(text.as_bytes()) }).unwrap_or_default();
                if file.len() + line.len() + 1 > MAX_FILE {
                    continue;
                }
                file.push_str(&line);
                file.push('\n');
                have.insert(hash);
                changed.insert(month.clone());
            }
        }
        let mut n = 0;
        let mut keep = BTreeSet::new();
        for (m, text) in &files {
            if text.is_empty() {
                continue;
            }
            let name = format!("{m}.jsonl");
            if changed.contains(m) {
                n += usize::from(self.put(&NotePath::new(format!("{folder}/{name}")), text.as_bytes()));
            }
            keep.insert(name);
        }
        self.prune(&folder, &keep);
        n
    }

    /// The names of devices that shared review history, by id (this device included).
    pub fn device_names(&self) -> BTreeMap<String, String> {
        let st = self.vault().storage();
        let mut out: BTreeMap<String, String> = st
            .list_under(DEVICES_DIR)
            .unwrap_or_default()
            .iter()
            .filter_map(|f| {
                let d = serde_json::from_slice::<DeviceFile>(&st.read_bytes(f).ok()?).ok()?;
                // A device is the file it publishes: `devices/<id>.json`
                // names `<id>` only, so no device can claim another's id.
                (f.file_name().strip_suffix(".json") == Some(d.id.as_str())).then_some(d)
            })
            .filter(|d| d.format <= SHARED_FORMAT && valid_device(&d.id) && d.id != self.device().id)
            .map(|d| (d.id, clean_name(&d.name)))
            .filter(|(_, name)| !name.is_empty())
            .collect();
        out.insert(self.device().id.clone(), self.device().name.clone());
        out
    }

    /// Another device as people should read it: its self-reported name,
    /// marked as such, and its id ("“home server” (self-reported name,
    /// device `d_8f2c…`)"). This device is "this device".
    pub fn device_label(&self, id: &str) -> String {
        if id == self.device().id {
            return "this device".into();
        }
        device_label(id, self.device_names().get(id).map_or("", String::as_str))
    }

    /// A text with this hash that another device carried for undoing its
    /// changes (see [`Self::export_shared`]).
    pub(crate) fn shared_text(&self, hash: &ContentHash) -> Option<String> {
        let st = self.vault().storage();
        let mut devices = other_devices(self.vault(), &self.device().id);
        devices.push(self.device().id.clone());
        let needle = format!("\"hash\":\"{hash}\"");
        for dev in devices {
            for f in st.list_under(&format!("{TEXTS_DIR}/{dev}")).unwrap_or_default() {
                let Ok(text) = st.read(&f) else { continue };
                for l in text.lines().filter(|l| l.contains(&needle)) {
                    let Ok(t) = serde_json::from_str::<TextLine>(l) else { continue };
                    if t.v > 1 || t.hash != *hash {
                        continue;
                    }
                    let Some(s) = gunzip_b64(&t.gz).and_then(|b| String::from_utf8(b).ok()) else { continue };
                    if ContentHash::of(&s) == *hash {
                        return Some(s);
                    }
                }
            }
        }
        None
    }

    /// Other devices' ledgers (read-only copies), matching `q`.
    ///
    /// Untrusted input: a line counts only when its actor is the device
    /// whose folder it is in, and every path it names passes
    /// [`crate::vault_file`] (no hidden folders, `.quire`, symlinks out of
    /// the vault, absolute or drive paths). Others are dropped.
    pub(crate) fn shared_log(&self, q: &LedgerQuery) -> Vec<LedgerEntry> {
        other_devices(self.vault(), &self.device().id)
            .iter()
            .flat_map(|dev| {
                crate::ledger::Ledger::read_folder(self.vault(), &format!("{SHARED_LEDGER_DIR}/{dev}"), q)
                    .into_iter()
                    .filter(move |e| e.actor.device == *dev)
                    .map(move |mut e| {
                        // Where it came from, for revert (in memory only).
                        e.other.insert(FROM_SHARED.into(), dev.clone().into());
                        e
                    })
            })
            .filter(|e| [&e.path, &e.to].into_iter().flatten().all(|p| crate::store::vault_file(self.vault(), p).is_ok()))
            .collect()
    }

    /// When other devices' ledgers last changed (to notice new entries).
    pub(crate) fn shared_log_stamp(&self) -> Vec<(NotePath, Option<std::time::SystemTime>)> {
        let st = self.vault().storage();
        let mut out = Vec::new();
        for dev in other_devices(self.vault(), &self.device().id) {
            for f in st.list_under(&format!("{SHARED_LEDGER_DIR}/{dev}")).unwrap_or_default() {
                let m = st.modified(&f).ok();
                out.push((f, m));
            }
        }
        out.sort();
        out
    }

    /// Bring other devices' review history into this device's store (after
    /// a sync pass, when shared state is on). Never writes a note.
    pub fn import_shared(&self) -> ImportReport {
        let me = self.device().id.clone();
        let st = self.vault().storage();
        let mut report = ImportReport::default();
        let devices = other_devices(self.vault(), &me);
        if devices.is_empty() {
            return report;
        }
        // Read once per import (decisions below update it as they go).
        let mut archived = ProposalStore::new(self.vault().clone()).archived(self.now_ms());
        for dev in &devices {
            for f in st.list_under(&format!("{PROPOSALS_DIR}/{dev}")).unwrap_or_default() {
                let Some(p) = st.read_bytes(&f).ok().and_then(|b| read_shared_proposal(&b)) else { continue };
                if f.file_name().strip_suffix(".json") != Some(p.id.as_str()) || p.origin().is_none_or(|(id, _)| id != dev) {
                    continue;
                }
                let Some(p) = self.checked_mirror(p, dev) else { continue };
                self.import_proposal(p, &mut archived, &mut report);
            }
        }
        for dev in &devices {
            for f in st.list_under(&format!("{DECISIONS_DIR}/{dev}")).unwrap_or_default() {
                let Some(d) = st.read_bytes(&f).ok().and_then(|b| serde_json::from_slice::<DecisionsFile>(&b).ok()) else { continue };
                if d.format > SHARED_FORMAT || d.device != *dev || f.file_name().strip_suffix(".json") != Some(d.proposal.as_str()) {
                    continue;
                }
                self.merge_decisions(&d, &mut archived, &mut report);
            }
        }
        report
    }

    /// Another device's proposal, if it may be taken in (untrusted input,
    /// see the module docs): its actor is an agent, job or importer of
    /// that device (never a person, never another device), every path in it
    /// passes [`crate::vault_file`], and its trust is lowered when a source
    /// is untrusted here.
    fn checked_mirror(&self, mut p: Proposal, dev: &str) -> Option<Proposal> {
        if p.actor.is_human() || p.actor.device != dev {
            return None;
        }
        for c in &p.changes {
            if c.subject().is_none() || c.paths().iter().any(|x| crate::store::vault_file(self.vault(), x).is_err()) {
                return None;
            }
        }
        if let Some(src) = self.untrusted_source(&p.sources) {
            p.trust = crate::actor::Trust::Untrusted;
            p.untrusted_source.get_or_insert(src);
        }
        Some(p)
    }

    /// Settle the waiting edits of `p` that have decisions and nothing left
    /// to show for the note as it is here (every hunk decided somewhere, or
    /// its text already synced in). True when one was settled.
    fn settle_quiet(&self, p: &mut Proposal) -> bool {
        let mut any = false;
        for c in p.changes.iter_mut().filter(|c| c.state.is_none() && !c.decisions.is_empty()) {
            let Some(path) = c.subject().cloned() else { continue };
            let Ok(current) = self.vault().read(&path) else { continue };
            let texts = match c.op {
                ChangeOp::Edit => c.base_text().zip(c.new_text.clone()),
                ChangeOp::Create => c.text.clone().map(|t| (current.clone(), t)),
                ChangeOp::Rename | ChangeOp::Delete => None,
            };
            let Some((base, new)) = texts else { continue };
            let all = crate::review::review_hunks(&path, &base, &current, &new, c.review.as_ref());
            if crate::review::visible(&current, all, &c.decisions, &c.decided, &self.device().id).is_empty() {
                let at = c.decisions.iter().map(|x| x.at).max().unwrap_or_default();
                let by =
                    crate::ops::effective(&c.decisions).iter().filter_map(|x| x.device()).min().unwrap_or(&self.device().id).to_string();
                c.set_state(crate::ops::final_state(&c.decisions), &by, at);
                c.review = None;
                any = true;
            }
        }
        any
    }

    /// Take in another device's proposal: new here, changed there (the
    /// agent amended it, or it was resolved there), or unchanged.
    fn import_proposal(&self, ext: Proposal, archived: &mut Vec<Proposal>, report: &mut ImportReport) {
        let store = ProposalStore::new(self.vault().clone());
        let now = self.now_ms();
        let body = body_hash(&ext).to_string();
        let seen_body = |p: &Proposal| p.other.get("origin_body").and_then(|v| v.as_str()) == Some(body.as_str());
        if let Some(local) = store.load(&ext.id) {
            if seen_body(&local) {
                return;
            }
            let loaded = local.updated;
            let mut merged = merge_body(&local, ext, &body, now);
            self.settle_quiet(&mut merged);
            if let Some(s) = merged.settled_status() {
                merged.status = s;
            }
            if store.save_if(&merged, loaded).unwrap_or(false) {
                report.updated += 1;
                if merged.status != ProposalStatus::Pending {
                    let _ = store.archive(&merged.id);
                }
            }
            return;
        }
        let found = archived.iter().position(|p| p.id == ext.id);
        match found.map(|i| archived[i].clone()) {
            None if ext.status == ProposalStatus::Pending => {
                let mut mirror = ext;
                for c in &mut mirror.changes {
                    c.review = None;
                }
                mirror.other.insert("origin_body".into(), body.into());
                if store.save(&mirror).is_ok() {
                    report.added += 1;
                }
            }
            // Resolved here, then amended there with something new: back as
            // waiting, with everything decided here kept decided.
            Some(a) if ext.status == ProposalStatus::Pending && !seen_body(&a) && ext.updated > a.updated => {
                let old = a.clone();
                let mut merged = merge_body(&a, ext, &body, now);
                self.settle_quiet(&mut merged);
                merged.status = merged.settled_status().unwrap_or(ProposalStatus::Pending);
                if merged.status == ProposalStatus::Pending && store.save(&merged).is_ok() {
                    store.remove_archived(&old);
                    archived.retain(|p| p.id != old.id);
                    report.updated += 1;
                }
            }
            _ => {}
        }
    }

    /// Merge one device's decisions on one proposal into the local copy
    /// (waiting, or resolved recently: then only its outcome is updated).
    fn merge_decisions(&self, d: &DecisionsFile, resolved: &mut [Proposal], report: &mut ImportReport) {
        let store = ProposalStore::new(self.vault().clone());
        let now = self.now_ms();
        let (mut p, archived) = match store.load(&d.proposal) {
            Some(p) => (p, false),
            None => match resolved.iter().find(|p| p.id == d.proposal && now - p.updated <= KEEP_RESOLVED_MS) {
                Some(p) => (p.clone(), true),
                None => return,
            },
        };
        let loaded = p.updated;
        let was_pending = !archived && p.status == ProposalStatus::Pending;
        // Reopened here: what was decided elsewhere before that is overruled.
        let reopened = p.other.get(REOPENED_AT).and_then(serde_json::Value::as_i64).unwrap_or(i64::MIN);
        let mut touched: BTreeSet<NotePath> = BTreeSet::new();
        for h in d.hunks.iter().filter(|h| h.at > reopened) {
            let Some(c) = change_mut(&mut p, &h.path, None) else { continue };
            let mut dec = HunkDecision { hunk: h.hunk.clone(), action: h.action, at: h.at, text: h.text.clone(), other: Other::new() };
            dec = dec.on_device(&d.device);
            if !c.decisions.iter().any(|x| same_decision(x, &dec)) {
                c.decisions.push(dec);
                touched.insert(h.path.clone());
            }
        }
        for x in d.decided.iter().filter(|x| x.content.at > reopened) {
            let Some(c) = change_mut(&mut p, &x.path, None) else { continue };
            let mut content = x.content.clone();
            content.device = Some(d.device.clone());
            if !c.decided.contains(&content) {
                c.decided.push(content);
                touched.insert(x.path.clone());
            }
        }
        let mut states = false;
        for s in d.changes.iter().filter(|s| s.at > reopened) {
            let Some(c) = change_mut(&mut p, &s.path, Some(s.op)) else { continue };
            let theirs = (s.state.rank(), std::cmp::Reverse((s.at, d.device.clone())));
            let ours = c.state.map(|st| {
                let (dev, at) = c.state_by().map_or((String::new(), 0), |(dv, at)| (dv.to_string(), at));
                (st.rank(), std::cmp::Reverse((at, dev)))
            });
            if ours.is_none_or(|o| theirs > o) {
                c.set_state(s.state, &d.device, s.at);
                c.review = None;
                states = true;
            }
        }
        // Every device ends with the same outcome for a decided edit: the
        // one all the hunk decisions give, unless a device ranked higher.
        for c in p.changes.iter_mut().filter(|c| c.state.is_some() && !c.decisions.is_empty()) {
            let Some(path) = c.subject() else { continue };
            if !touched.contains(path) {
                continue;
            }
            let from_hunks = crate::ops::final_state(&c.decisions);
            if c.state.is_some_and(|s| from_hunks.rank() > s.rank()) {
                let at = c.decisions.iter().map(|x| x.at).max().unwrap_or_default();
                let by = crate::ops::effective(&c.decisions).iter().filter_map(|x| x.device()).min().unwrap_or(&d.device).to_string();
                c.set_state(from_hunks, &by, at);
                states = true;
            }
        }
        if archived {
            if touched.is_empty() && !states {
                return;
            }
            if let Some(s) = p.settled_status() {
                p.status = s;
            }
            if store.save_archived(&p).is_ok() {
                report.decided += 1;
                if let Some(slot) = resolved.iter_mut().find(|x| x.id == p.id) {
                    *slot = p;
                }
            }
            return;
        }
        // Rejected elsewhere: out of this copy's text too (the agent amends
        // on its own device, which takes them out the same way).
        for c in p.changes.iter_mut().filter(|c| c.subject().is_some_and(|s| touched.contains(s))) {
            c.drop_rejected();
        }
        states |= self.settle_quiet(&mut p);
        if touched.is_empty() && !states {
            return;
        }
        p.updated = now.max(loaded + 1);
        if let Some(s) = p.settled_status() {
            p.status = s;
        }
        if p.status != ProposalStatus::Pending && was_pending {
            // Settled by another device: never silently. The Inbox shows
            // it under "Resolved" as decided there, and the activity log
            // has an entry for this device too.
            p.other.insert(SETTLED_BY.into(), serde_json::json!({"device": d.device, "at": now}));
        }
        if store.save_if(&p, loaded).unwrap_or(false) {
            report.decided += 1;
            if p.status != ProposalStatus::Pending {
                report.settled += 1;
                let _ = store.archive(&p.id);
                if was_pending {
                    let op = if p.status == ProposalStatus::Rejected { Op::Reject } else { Op::Accept };
                    let mut e = LedgerEntry::new(crate::actor::Actor::human(&d.device), op, now);
                    e.proposal = Some(p.id.clone());
                    e.from = Some(p.actor.id.clone());
                    e.path = p.changes.iter().find_map(|c| c.subject().cloned());
                    e.other.insert("settled_from".into(), d.device.clone().into());
                    self.record(&e);
                }
            }
        }
    }

    /// Put a proposal that another device's decisions settled back to
    /// waiting here (the person's "Reopen" in the Inbox): the states and
    /// hunk decisions other devices made are dropped from this copy, and
    /// decisions they made before now are ignored when they arrive again.
    /// What was decided on this device stays. The note is not touched.
    pub fn reopen_proposal(&self, id: &ProposalId) -> Result<(), crate::change::WriteError> {
        let store = ProposalStore::new(self.vault().clone());
        let now = self.now_ms();
        let me = self.device().id.clone();
        let (mut p, archived) = match store.load(id) {
            Some(p) => (p, None),
            None => match store.archived(now).into_iter().find(|p| &p.id == id) {
                Some(p) => (p.clone(), Some(p)),
                None => return Err(crate::change::WriteError::NotFound { path: NotePath::new(id.to_string()) }),
            },
        };
        for c in &mut p.changes {
            c.decisions.retain(|d| d.device().is_none_or(|dev| dev == me));
            c.decided.retain(|d| d.device.as_deref().is_none_or(|dev| dev == me));
            if c.state_by().is_some_and(|(dev, _)| dev != me) {
                c.state = None;
                c.other.remove("state_device");
                c.other.remove("state_at");
            }
            if c.state.is_none() && c.decisions.is_empty() {
                c.review = None;
            }
        }
        p.other.insert(REOPENED_AT.into(), now.into());
        p.other.remove(SETTLED_BY);
        p.updated = now.max(p.updated + 1);
        p.status = p.settled_status().unwrap_or(ProposalStatus::Pending);
        store.save(&p)?;
        if let Some(old) = archived {
            store.remove_archived(&old);
        }
        if p.status != ProposalStatus::Pending {
            let _ = store.archive(&p.id);
        }
        let mut e = LedgerEntry::new(crate::actor::Actor::human(&me), Op::Revert, now);
        e.proposal = Some(p.id.clone());
        e.from = Some(p.actor.id.clone());
        self.record(&e);
        Ok(())
    }
}

/// On a ledger entry read from another device's shared ledger (in memory,
/// never written): that device.
pub(crate) const FROM_SHARED: &str = "_from_shared";

/// In a proposal: the device whose decisions settled it here, and when.
pub const SETTLED_BY: &str = "settled_by";
/// In a proposal: when the person reopened it here (decisions made
/// elsewhere before then are ignored).
pub const REOPENED_AT: &str = "reopened_at";

/// Another device by id and the name it reported about itself, for
/// people: "“home server” (self-reported name, device `d_8f2c4a…`)".
pub fn device_label(id: &str, name: &str) -> String {
    let short: String = id.chars().take(10).collect();
    let more = if id.chars().count() > 10 { "…" } else { "" };
    let name = clean_name(name);
    if name.is_empty() { format!("device {short}{more}") } else { format!("“{name}” (self-reported name, device {short}{more})") }
}

/// A self-reported device name as shown: control characters out, at most
/// 64 characters.
fn clean_name(name: &str) -> String {
    name.chars().filter(|c| !c.is_control()).take(64).collect::<String>().trim().to_string()
}

/// A proposal as another device shared it (`packed` or plain), if this
/// version reads it.
fn read_shared_proposal(bytes: &[u8]) -> Option<Proposal> {
    let v: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    if v.get("format")?.as_u64()? > u64::from(PROPOSAL_FORMAT) {
        return None;
    }
    let p: Proposal = match v.get("packed").and_then(|p| p.as_str()) {
        Some(packed) => serde_json::from_slice(&gunzip_b64(packed)?).ok()?,
        None => serde_json::from_value(v).ok()?,
    };
    (p.format <= PROPOSAL_FORMAT).then_some(p)
}

/// The proposal as its own device has it now (`ext`), with everything
/// decided here kept: decisions and decided content are combined, and each
/// change keeps the higher-ranked state.
fn merge_body(local: &Proposal, mut ext: Proposal, body: &str, now: i64) -> Proposal {
    for c in &mut ext.changes {
        c.review = None;
        let Some(subject) = c.subject().cloned() else { continue };
        let Some(l) = local.changes.iter().find(|l| l.subject() == Some(&subject) && (l.op == c.op || l.op == ChangeOp::Edit)) else {
            continue;
        };
        for d in &l.decisions {
            if !c.decisions.iter().any(|x| same_decision(x, d)) {
                c.decisions.push(d.clone());
            }
        }
        for d in &l.decided {
            if !c.decided.contains(d) {
                c.decided.push(d.clone());
            }
        }
        match (l.state, c.state) {
            (Some(ls), Some(cs)) if ls.rank() > cs.rank() => {
                c.state = Some(ls);
                for k in ["state_device", "state_at"] {
                    if let Some(v) = l.other.get(k) {
                        c.other.insert(k.into(), v.clone());
                    }
                }
            }
            // Decided hunk by hunk: the agent may have added something
            // since, so it is worked out again (`settle_quiet`); hunks
            // decided here stay hidden.
            (Some(ls), None) if l.decisions.is_empty() => {
                c.state = Some(ls);
                for k in ["state_device", "state_at"] {
                    if let Some(v) = l.other.get(k) {
                        c.other.insert(k.into(), v.clone());
                    }
                }
            }
            _ => {}
        }
    }
    for c in &mut ext.changes {
        c.drop_rejected();
    }
    ext.other.insert("origin_body".into(), body.into());
    ext.updated = now.max(local.updated + 1).max(ext.updated);
    ext
}
