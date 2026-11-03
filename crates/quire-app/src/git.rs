// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Git integration (design §F.5): detection, auto-commit timer, last result.
//!
//! Quire runs the system `git` (so your credential helpers and ssh-agent
//! work) on a background thread, never on the UI thread, with prompts off
//! and a time limit. It commits notes, attachments and `.quire/shared/`
//! (never the rest of `.quire/`), can commit by itself after a quiet spell,
//! and pulls and pushes when asked. While Quire Sync is on for the vault,
//! pull and push are off so two systems never write the same files.
//!
//! Settings are in the vault (`[git]` in `.quire/shared/vault.toml`).
//! Owned by WS-F; kept in `ws.features.git`.

use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::process::{Command as Proc, Stdio};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender, unbounded};
use serde::{Deserialize, Serialize};

use quire_vault::{NotePath, VaultEvent, VaultPrefs};

use crate::decor::StatusItem;
use crate::workspace::{NoticeLevel, Workspace};

/// A git call taking longer than this is stopped.
pub const TIMEOUT: Duration = Duration::from_mins(1);
/// The oldest git Quire works with.
pub const MIN_VERSION: (u32, u32) = (2, 30);
/// How often the working tree's status is refreshed while idle.
const STATUS_EVERY: Duration = Duration::from_mins(1);
/// After a change in the vault, wait this long before asking git again.
const STATUS_AFTER_CHANGE: Duration = Duration::from_secs(2);
/// Why pull and push are off.
pub const SYNC_ON: &str =
    "Quire Sync is on for this vault, so git only commits here: pulling or pushing as well would have two systems writing the same files.";

/// `[git]` in vault.toml.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct GitPrefs {
    /// Commit changes by itself after `commit_after_idle_s` without edits.
    pub auto_commit: bool,
    pub commit_after_idle_s: u64,
    /// The commit message; `{{count}}` is the number of files.
    pub message: String,
    pub pull_on_start: bool,
    pub push_after_commit: bool,
    #[serde(flatten)]
    pub other: toml::Table,
}

impl Default for GitPrefs {
    fn default() -> Self {
        Self {
            auto_commit: false,
            commit_after_idle_s: 600,
            message: "Quire: {{count}} notes".into(),
            pull_on_start: false,
            push_after_commit: false,
            other: toml::Table::new(),
        }
    }
}

/// One commit in a note's history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogEntry {
    pub hash: String,
    pub short: String,
    pub author: String,
    /// Unix seconds.
    pub at: i64,
    pub subject: String,
    /// The note's path in that commit (it may have been renamed since).
    pub path: String,
}

/// The working tree, from `git status --porcelain=v2 --branch`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Status {
    pub branch: String,
    pub upstream: Option<String>,
    pub ahead: u32,
    pub behind: u32,
    /// Changed, added, deleted and untracked files Quire would commit.
    pub changed: Vec<String>,
}

#[derive(Debug)]
enum Op {
    Detect,
    Status,
    Commit { auto: bool },
    Pull,
    Push,
    Log(NotePath),
    Show { hash: String, path: String },
}

#[derive(Debug)]
enum Done {
    Detected(Result<(PathBuf, String), String>),
    Status(Result<Status, String>),
    Committed { auto: bool, result: Result<Option<usize>, String> },
    Pulled(Result<String, String>),
    Pushed(Result<String, String>),
    Log(NotePath, Result<Vec<LogEntry>, String>),
    Shown { hash: String, result: Result<String, String> },
}

/// State for this feature.
#[derive(Debug)]
pub struct GitState {
    /// The vault is in a git repository and git works.
    pub repo: bool,
    /// Why git can't be used (not installed, too old), once checked.
    pub unavailable: Option<String>,
    pub version: String,
    /// The repository's top folder.
    pub root: Option<PathBuf>,
    pub prefs: GitPrefs,
    pub status: Option<Status>,
    /// What's running (`"Committing…"`).
    pub busy: Vec<&'static str>,
    /// The last result, and whether it was an error.
    pub last: Option<(String, bool)>,
    /// The history of one note, newest first.
    pub log: Option<(NotePath, Result<Vec<LogEntry>, String>)>,
    /// A commit's change to a note.
    pub shown: Option<(String, Result<String, String>)>,
    pending_change: bool,
    last_change: Option<Instant>,
    status_at: Option<Instant>,
    started: bool,
    tx: Sender<Done>,
    rx: Receiver<Done>,
}

impl Default for GitState {
    fn default() -> Self {
        let (tx, rx) = unbounded();
        Self {
            repo: false,
            unavailable: None,
            version: String::new(),
            root: None,
            prefs: GitPrefs::default(),
            status: None,
            busy: Vec::new(),
            last: None,
            log: None,
            shown: None,
            pending_change: false,
            last_change: None,
            status_at: None,
            started: false,
            tx,
            rx,
        }
    }
}

// ------------------------------------------------------------ running git

/// Run `git` in `dir` with prompts off and a time limit: stdout, or an error
/// in plain words.
pub fn git(dir: &Path, args: &[&str]) -> Result<String, String> {
    let mut child = Proc::new("git")
        .arg("-c")
        .arg("core.quotepath=off")
        .args(args)
        .current_dir(dir)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_ASKPASS", "")
        .env("SSH_ASKPASS", "")
        .env("GCM_INTERACTIVE", "never")
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                "Git isn't installed (or isn't on the PATH). Install git to use it from Quire.".to_string()
            } else {
                format!("Couldn't run git: {e}")
            }
        })?;
    // Read output on threads so a chatty git never blocks on a full pipe.
    let mut out = child.stdout.take().ok_or("no stdout")?;
    let mut err = child.stderr.take().ok_or("no stderr")?;
    let out_t = std::thread::spawn(move || {
        let mut s = Vec::new();
        let _ = out.read_to_end(&mut s);
        s
    });
    let err_t = std::thread::spawn(move || {
        let mut s = Vec::new();
        let _ = err.read_to_end(&mut s);
        s
    });
    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) if started.elapsed() > TIMEOUT => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("git {} took longer than {} s and was stopped", args.first().unwrap_or(&""), TIMEOUT.as_secs()));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(e) => return Err(format!("Couldn't wait for git: {e}")),
        }
    };
    let stdout = String::from_utf8_lossy(&out_t.join().unwrap_or_default()).into_owned();
    let stderr = String::from_utf8_lossy(&err_t.join().unwrap_or_default()).into_owned();
    if status.success() {
        Ok(stdout)
    } else {
        let msg = stderr.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with("hint:")).collect::<Vec<_>>().join(" ");
        Err(if msg.is_empty() { format!("git {} failed ({status})", args.first().unwrap_or(&"")) } else { msg })
    }
}

/// `git version 2.43.0` → (2, 43).
pub fn parse_version(s: &str) -> Option<(u32, u32)> {
    let v = s.split_whitespace().find(|w| w.chars().next().is_some_and(|c| c.is_ascii_digit()))?;
    let mut it = v.split('.');
    Some((it.next()?.parse().ok()?, it.next()?.parse().ok()?))
}

fn detect(vault: &Path) -> Result<(PathBuf, String), String> {
    let v = git(vault, &["--version"])?;
    let version = v.trim().to_string();
    match parse_version(&version) {
        Some(found) if found >= MIN_VERSION => {}
        _ => return Err(format!("{version} is too old; Quire needs git {}.{} or newer", MIN_VERSION.0, MIN_VERSION.1)),
    }
    let top = git(vault, &["rev-parse", "--show-toplevel"]).map_err(|_| "not a repository".to_string())?;
    Ok((PathBuf::from(top.trim()), version))
}

/// Paths are relative to the repository; keep only what Quire commits.
fn quire_commits(path: &str, prefix: &str) -> bool {
    let rel = path.strip_prefix(prefix).unwrap_or(path);
    !rel.starts_with(".quire/") || rel.starts_with(".quire/shared/")
}

/// The vault's folder inside the repository (`""` when the vault is the repository).
fn vault_prefix(root: &Path, vault: &Path) -> String {
    let canon = |p: &Path| p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
    canon(vault).strip_prefix(canon(root)).map_or_else(
        |_| String::new(),
        |p| {
            let s = p.to_string_lossy().replace('\\', "/");
            if s.is_empty() { s } else { format!("{s}/") }
        },
    )
}

pub fn parse_status(out: &str, prefix: &str) -> Status {
    let mut st = Status::default();
    for line in out.lines() {
        if let Some(b) = line.strip_prefix("# branch.head ") {
            st.branch = b.to_string();
        } else if let Some(u) = line.strip_prefix("# branch.upstream ") {
            st.upstream = Some(u.to_string());
        } else if let Some(ab) = line.strip_prefix("# branch.ab ") {
            for part in ab.split_whitespace() {
                if let Some(a) = part.strip_prefix('+') {
                    st.ahead = a.parse().unwrap_or(0);
                } else if let Some(b) = part.strip_prefix('-') {
                    st.behind = b.parse().unwrap_or(0);
                }
            }
        } else {
            let path = match line.as_bytes().first() {
                // `1 XY sub mH mI mW hH hI path`
                Some(b'1') => line.splitn(9, ' ').nth(8),
                // `2 XY sub mH mI mW hH hI Xscore path\torig`
                Some(b'2') => line.splitn(10, ' ').nth(9).map(|p| p.split('\t').next().unwrap_or(p)),
                Some(b'u') => line.splitn(11, ' ').nth(10),
                Some(b'?') => line.strip_prefix("? "),
                _ => None,
            };
            if let Some(p) = path
                && p.starts_with(prefix)
                && quire_commits(p, prefix)
            {
                st.changed.push(p.to_string());
            }
        }
    }
    st
}

fn git_status(root: &Path, prefix: &str) -> Result<Status, String> {
    let dir = if prefix.is_empty() { root.to_path_buf() } else { root.join(prefix) };
    git(&dir, &["status", "--porcelain=v2", "--branch", "--untracked-files=all", "--", "."]).map(|o| parse_status(&o, prefix))
}

/// Stage what Quire commits (notes, attachments, `.quire/shared/`) and
/// commit. `Ok(None)`: nothing to commit.
fn commit(root: &Path, prefix: &str, message: &str) -> Result<Option<usize>, String> {
    let dir = if prefix.is_empty() { root.to_path_buf() } else { root.join(prefix) };
    let before = git_status(root, prefix)?;
    if before.changed.is_empty() {
        return Ok(None);
    }
    git(&dir, &["add", "-A", "--", ".", ":(exclude).quire"])?;
    if dir.join(".quire/shared").is_dir() {
        git(&dir, &["add", "-A", "--", ".quire/shared"])?;
    }
    let n = before.changed.len();
    let msg = message.replace("{{count}}", &n.to_string());
    let msg = if msg.trim().is_empty() { format!("Quire: {n} notes") } else { msg };
    // The index: what was just staged, plus anything you staged yourself.
    match git(&dir, &["commit", "-q", "-m", &msg]) {
        Ok(_) => Ok(Some(n)),
        Err(e) if e.contains("nothing to commit") || e.contains("no changes added") => Ok(None),
        Err(e) if e.contains("Please tell me who you are") || e.contains("unable to auto-detect email") => {
            Err("Git doesn't know your name and email yet. Set them with git config user.name and user.email.".into())
        }
        Err(e) => Err(e),
    }
}

pub fn parse_log(out: &str) -> Vec<LogEntry> {
    // Records start with \x1e; the fields are \x1f-separated, then the file name.
    out.split('\x1e')
        .filter_map(|rec| {
            let mut lines = rec.trim_matches('\n').lines();
            let head = lines.next()?;
            let f: Vec<&str> = head.split('\x1f').collect();
            if f.len() < 5 {
                return None;
            }
            let path = lines.map(str::trim).find(|l| !l.is_empty()).unwrap_or_default().to_string();
            Some(LogEntry {
                hash: f[0].into(),
                short: f[1].into(),
                author: f[2].into(),
                at: f[3].parse().unwrap_or(0),
                subject: f[4].into(),
                path,
            })
        })
        .collect()
}

fn log(root: &Path, rel: &str) -> Result<Vec<LogEntry>, String> {
    git(root, &["log", "--follow", "-n", "200", "--name-only", "--format=%x1e%H%x1f%h%x1f%an%x1f%at%x1f%s", "--", rel])
        .map(|o| parse_log(&o))
}

fn run_op(root: Option<&Path>, vault: &Path, prefix: &str, op: Op, message: &str) -> Done {
    let need = || root.ok_or_else(|| "not a repository".to_string());
    match op {
        Op::Detect => Done::Detected(detect(vault)),
        Op::Status => Done::Status(need().and_then(|r| git_status(r, prefix))),
        Op::Commit { auto } => Done::Committed { auto, result: need().and_then(|r| commit(r, prefix, message)) },
        Op::Pull => Done::Pulled(need().and_then(|r| git(r, &["pull", "--ff-only", "-q"]))),
        Op::Push => Done::Pushed(need().and_then(|r| git(r, &["push", "-q"]))),
        Op::Log(p) => {
            let rel = format!("{prefix}{}", p.as_str());
            let r = need().and_then(|r| log(r, &rel));
            Done::Log(p, r)
        }
        Op::Show { hash, path } => {
            let r = need().and_then(|r| git(r, &["show", "--format=", "--patch", "--no-color", &hash, "--", &path]));
            Done::Shown { hash, result: r }
        }
    }
}

fn label(op: &Op) -> &'static str {
    match op {
        Op::Detect => "Checking git…",
        Op::Status => "Reading git status…",
        Op::Commit { .. } => "Committing…",
        Op::Pull => "Pulling…",
        Op::Push => "Pushing…",
        Op::Log(_) => "Reading history…",
        Op::Show { .. } => "Reading a commit…",
    }
}

/// Start `op` on a background thread.
fn spawn(ws: &mut Workspace, op: Op) {
    let Some(vault) = ws.vault_root().map(Path::to_path_buf) else { return };
    let st = &mut ws.features.git;
    let what = label(&op);
    if !matches!(op, Op::Log(_) | Op::Show { .. }) && st.busy.contains(&what) {
        return;
    }
    st.busy.push(what);
    let root = st.root.clone();
    let prefix = root.as_deref().map(|r| vault_prefix(r, &vault)).unwrap_or_default();
    let message = st.prefs.message.clone();
    let tx = st.tx.clone();
    let _ = std::thread::Builder::new().name("quire-git".into()).spawn(move || {
        let _ = tx.send(run_op(root.as_deref(), &vault, &prefix, op, &message));
    });
}

// ------------------------------------------------------------ the feature

fn sync_on(ws: &Workspace) -> bool {
    ws.sync.config.is_some()
}

pub(crate) fn start(ws: &mut Workspace) {
    if ws.vault_root().is_some() && !ws.features.git.started {
        ws.features.git.started = true;
        spawn(ws, Op::Detect);
    }
}

pub(crate) fn read_prefs(ws: &mut Workspace, prefs: &VaultPrefs) {
    ws.features.git.prefs = prefs.section("git");
}

/// Change `[git]` in vault.toml.
pub fn set_prefs(ws: &mut Workspace, prefs: &GitPrefs) -> Result<(), String> {
    crate::nav::edit_prefs(ws, |p| p.set_section("git", prefs))
}

pub(crate) fn vault_event(ws: &mut Workspace, ev: &VaultEvent) {
    let _ = ev;
    ws.features.git.pending_change = true;
}

fn finish(ws: &mut Workspace, done: Done) {
    let st = &mut ws.features.git;
    let what = match &done {
        Done::Detected(_) => "Checking git…",
        Done::Status(_) => "Reading git status…",
        Done::Committed { .. } => "Committing…",
        Done::Pulled(_) => "Pulling…",
        Done::Pushed(_) => "Pushing…",
        Done::Log(..) => "Reading history…",
        Done::Shown { .. } => "Reading a commit…",
    };
    if let Some(i) = st.busy.iter().position(|b| *b == what) {
        st.busy.remove(i);
    }
    match done {
        Done::Detected(Ok((root, version))) => {
            st.repo = true;
            st.root = Some(root);
            st.version = version;
            st.unavailable = None;
            // Opening the vault counts as activity: auto-commit waits a quiet spell first.
            st.pending_change = true;
            spawn(ws, Op::Status);
            if ws.features.git.prefs.pull_on_start && !sync_on(ws) {
                spawn(ws, Op::Pull);
            }
        }
        Done::Detected(Err(e)) => {
            st.repo = false;
            st.unavailable = Some(e);
        }
        Done::Status(r) => {
            st.status_at = Some(Instant::now());
            match r {
                Ok(s) => st.status = Some(s),
                Err(e) => st.last = Some((e, true)),
            }
        }
        Done::Committed { auto, result } => {
            match result {
                Ok(Some(n)) => {
                    st.last = Some((format!("Committed {n} file{}", if n == 1 { "" } else { "s" }), false));
                    if st.prefs.push_after_commit && !sync_on(ws) {
                        spawn(ws, Op::Push);
                    }
                }
                Ok(None) => {
                    if !auto {
                        ws.notify(NoticeLevel::Info, "Nothing to commit");
                    }
                }
                Err(e) => {
                    ws.features.git.last = Some((e.clone(), true));
                    ws.notify(NoticeLevel::Error, format!("Git commit: {e}"));
                }
            }
            spawn(ws, Op::Status);
        }
        Done::Pulled(r) | Done::Pushed(r) => {
            let pulled = what == "Pulling…";
            match r {
                Ok(_) => {
                    st.last = Some((if pulled { "Pulled" } else { "Pushed" }.into(), false));
                }
                Err(e) => {
                    let e = if pulled && (e.contains("Not possible to fast-forward") || e.contains("diverging")) {
                        "Your notes and the remote both have new commits; merge them with git (Quire doesn't merge commits).".into()
                    } else if e.contains("no upstream") || e.contains("no tracking information") {
                        "This branch has no upstream branch to pull from or push to.".into()
                    } else {
                        e
                    };
                    st.last = Some((e.clone(), true));
                    ws.notify(NoticeLevel::Error, format!("Git {}: {e}", if pulled { "pull" } else { "push" }));
                }
            }
            spawn(ws, Op::Status);
        }
        Done::Log(p, r) => st.log = Some((p, r)),
        Done::Shown { hash, result } => st.shown = Some((hash, result)),
    }
}

/// Collect results, refresh the status, and commit by itself when it's time.
pub(crate) fn tick(ws: &mut Workspace, now: Instant) -> bool {
    let mut changed = false;
    while let Ok(done) = ws.features.git.rx.try_recv() {
        finish(ws, done);
        changed = true;
    }
    if !ws.features.git.repo {
        return changed;
    }
    let st = &mut ws.features.git;
    if std::mem::take(&mut st.pending_change) {
        st.last_change = Some(now);
    }
    let edited = ws.docs().iter().filter_map(crate::Document::last_edit).max();
    let st = &ws.features.git;
    let busy = !st.busy.is_empty();
    let last_activity = [st.last_change, edited].into_iter().flatten().max();
    let status_due = st.status_at.is_none_or(|t| now.duration_since(t) >= STATUS_EVERY)
        || st.last_change.is_some_and(|c| st.status_at.is_none_or(|t| c > t) && now.duration_since(c) >= STATUS_AFTER_CHANGE);
    if !busy && status_due {
        ws.features.git.status_at = Some(now);
        spawn(ws, Op::Status);
        return true;
    }
    let st = &ws.features.git;
    let idle = Duration::from_secs(st.prefs.commit_after_idle_s.max(10));
    let has_changes = st.status.as_ref().is_some_and(|s| !s.changed.is_empty());
    let quiet = last_activity.is_none_or(|t| now.saturating_duration_since(t) >= idle);
    let saved = ws.docs().iter().chain(&ws.features.nav.hidden).all(|d| !d.is_dirty());
    if st.prefs.auto_commit && !busy && has_changes && quiet && saved {
        // Commit once per quiet spell: the status refresh after it clears `changed`.
        if let Some(s) = ws.features.git.status.as_mut() {
            s.changed.clear();
        }
        spawn(ws, Op::Commit { auto: true });
        changed = true;
    }
    changed
}

pub(crate) fn next_deadline(ws: &Workspace) -> Option<Instant> {
    let st = &ws.features.git;
    if !st.busy.is_empty() {
        return Some(Instant::now() + Duration::from_millis(100));
    }
    if !st.repo {
        return None;
    }
    let status = st.status_at.map(|t| t + STATUS_EVERY);
    let commit = (st.prefs.auto_commit && st.status.as_ref().is_some_and(|s| !s.changed.is_empty()))
        .then(|| st.last_change.map(|t| t + Duration::from_secs(st.prefs.commit_after_idle_s.max(10))))
        .flatten();
    [status, commit].into_iter().flatten().min()
}

pub fn commit_now(ws: &mut Workspace) {
    if !ws.features.git.repo {
        let why = ws.features.git.unavailable.clone().unwrap_or_else(|| "This vault isn't in a git repository".into());
        ws.notify(NoticeLevel::Info, why);
        return;
    }
    ws.save_all();
    spawn(ws, Op::Commit { auto: false });
}

pub fn pull(ws: &mut Workspace) {
    if sync_on(ws) {
        ws.notify(NoticeLevel::Info, SYNC_ON);
    } else if ws.features.git.repo {
        ws.save_all();
        spawn(ws, Op::Pull);
    }
}

pub fn push(ws: &mut Workspace) {
    if sync_on(ws) {
        ws.notify(NoticeLevel::Info, SYNC_ON);
    } else if ws.features.git.repo {
        spawn(ws, Op::Push);
    }
}

/// Whether pull and push are offered (not while Quire Sync is on).
pub fn can_pull_push(ws: &Workspace) -> bool {
    ws.features.git.repo && !sync_on(ws)
}

/// Ask for a note's history (it arrives in `ws.features.git.log`).
pub fn request_log(ws: &mut Workspace, path: &NotePath) {
    if ws.features.git.repo {
        spawn(ws, Op::Log(path.clone()));
    }
}

/// Ask for a commit's change to a note (it arrives in `ws.features.git.shown`).
pub fn request_show(ws: &mut Workspace, entry: &LogEntry) {
    if ws.features.git.repo {
        spawn(ws, Op::Show { hash: entry.hash.clone(), path: entry.path.clone() });
    }
}

/// Whether the vault's `.gitignore` keeps `.quire/` out except `.quire/shared/`.
pub fn gitignore_ok(ws: &Workspace) -> bool {
    let Some(root) = ws.vault_root() else { return true };
    std::fs::read_to_string(root.join(".gitignore")).is_ok_and(|t| t.lines().any(|l| l.trim() == "!.quire/shared/"))
}

/// Add the suggested lines to the vault's `.gitignore`.
pub fn add_gitignore(ws: &Workspace) -> Result<(), String> {
    let root = ws.vault_root().ok_or("Open a vault first")?.to_path_buf();
    let path = root.join(".gitignore");
    let mut text = std::fs::read_to_string(&path).unwrap_or_default();
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str("# Quire: app data stays on this device; vault settings travel.\n.quire/*\n!.quire/shared/\n");
    crate::atomic::write(&path, text.as_bytes(), false).map_err(|e| e.to_string())
}

pub(crate) fn status(ws: &Workspace, out: &mut Vec<StatusItem>) {
    let st = &ws.features.git;
    if !st.repo {
        return;
    }
    let (text, warn) = match (&st.busy.first(), &st.last, &st.status) {
        (Some(b), _, _) => ((*b).to_string(), false),
        (None, Some((_, true)), _) => ("git: error".to_string(), true),
        (None, _, Some(s)) if !s.changed.is_empty() => (format!("git: {} changed", s.changed.len()), false),
        (None, _, Some(s)) if s.ahead > 0 => (format!("git: {} to push", s.ahead), false),
        _ => ("git".to_string(), false),
    };
    let tooltip = match (&st.last, &st.status) {
        (Some((e, true)), _) => e.clone(),
        (_, Some(s)) => format!("Branch {}{}", s.branch, s.upstream.as_ref().map(|u| format!(" → {u}")).unwrap_or_default()),
        _ => st.version.clone(),
    };
    out.push(StatusItem { owner: crate::Owner::Editor, text, tooltip, command: Some(crate::Command::GitCommitNow), warn });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Settings;

    fn have_git() -> bool {
        Proc::new("git").arg("--version").output().is_ok_and(|o| o.status.success())
    }

    /// A temporary repository with its own identity (never the user's config).
    fn temp_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        for args in [
            &["init", "-q", "-b", "main"][..],
            &["config", "user.name", "Quire Test"],
            // An empty address: git accepts it, and no address lands in the code.
            &["config", "user.email", ""],
            &["config", "commit.gpgsign", "false"],
        ] {
            git(p, args).unwrap();
        }
        dir
    }

    fn wait(ws: &mut Workspace, now: Instant) {
        let until = Instant::now() + Duration::from_secs(20);
        while Instant::now() < until {
            ws.tick(now);
            if ws.features.git.busy.is_empty() {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("git didn't finish: {:?}", ws.features.git.busy);
    }

    #[test]
    fn porcelain_status_and_log_parse() {
        let out = "# branch.oid abc\n# branch.head main\n# branch.upstream origin/main\n# branch.ab +2 -1\n\
                   1 .M N... 100644 100644 100644 a b notes/A b.md\n\
                   2 R. N... 100644 100644 100644 a b R100 notes/New.md\tnotes/Old.md\n\
                   ? notes/.quire/cache/x.bin\n? notes/.quire/shared/vault.toml\n? other/x.md\n";
        let s = parse_status(out, "notes/");
        assert_eq!((s.branch.as_str(), s.upstream.as_deref(), s.ahead, s.behind), ("main", Some("origin/main"), 2, 1));
        assert_eq!(s.changed, ["notes/A b.md", "notes/New.md", "notes/.quire/shared/vault.toml"]);
        let log = parse_log("\x1eh1\x1fs1\x1fAda\x1f100\x1fFirst\n\nA.md\n\x1eh2\x1fs2\x1fBo\x1f50\x1fOld name\n\nOld.md\n");
        assert_eq!(log.len(), 2);
        assert_eq!((log[1].path.as_str(), log[1].at, log[0].subject.as_str()), ("Old.md", 50, "First"));
        assert_eq!(parse_version("git version 2.43.0"), Some((2, 43)));
        assert_eq!(parse_version("git version 2.39.3 (Apple Git-146)"), Some((2, 39)));
    }

    #[test]
    fn auto_commit_makes_the_expected_commits() {
        if !have_git() {
            eprintln!("git isn't installed; skipped");
            return;
        }
        let repo = temp_repo();
        let root = repo.path();
        std::fs::write(root.join("A.md"), "# A\n").unwrap();
        std::fs::create_dir_all(root.join(".quire/shared")).unwrap();
        std::fs::create_dir_all(root.join(".quire/cache")).unwrap();
        std::fs::write(root.join(".quire/shared/vault.toml"), "format = 1\n[git]\nauto_commit = true\ncommit_after_idle_s = 30\n").unwrap();
        std::fs::write(root.join(".quire/cache/junk.bin"), "x").unwrap();
        let mut ws = Workspace::new(Settings::default());
        ws.open_vault(root).unwrap();
        let t0 = Instant::now();
        wait(&mut ws, t0);
        assert!(ws.features.git.repo, "{:?}", ws.features.git.unavailable);
        assert!(ws.command_available(crate::Command::GitCommitNow));
        wait(&mut ws, t0);
        assert_eq!(ws.features.git.status.as_ref().map(|s| s.changed.len()), Some(2), "A.md and vault.toml, not the cache");
        assert!(ws.features.git.prefs.auto_commit);
        // Not before the quiet spell is over…
        ws.features.git.last_change = Some(t0);
        ws.tick(t0 + Duration::from_secs(5));
        assert!(ws.features.git.busy.is_empty());
        // …then one commit.
        wait(&mut ws, t0 + Duration::from_secs(31));
        wait(&mut ws, t0 + Duration::from_secs(31));
        let log = git(root, &["log", "--format=%s", "--name-only"]).unwrap();
        assert!(log.starts_with("Quire: 2 notes"), "{log}");
        assert!(log.contains("A.md") && log.contains(".quire/shared/vault.toml") && !log.contains("junk"), "{log}");
        assert_eq!(ws.features.git.status.as_ref().map(|s| s.changed.len()), Some(0));
        // A note's history.
        request_log(&mut ws, &NotePath::new("A.md"));
        wait(&mut ws, t0 + Duration::from_secs(32));
        let entries = ws.features.git.log.clone().unwrap().1.unwrap();
        assert_eq!(entries.len(), 1);
        request_show(&mut ws, &entries[0]);
        wait(&mut ws, t0 + Duration::from_secs(32));
        assert!(ws.features.git.shown.as_ref().unwrap().1.as_ref().unwrap().contains("+# A"));
        ws.shutdown();
    }

    #[test]
    fn push_is_off_while_quire_sync_is_on() {
        if !have_git() {
            return;
        }
        let repo = temp_repo();
        let mut ws = Workspace::new(Settings::default());
        ws.open_vault(repo.path()).unwrap();
        wait(&mut ws, Instant::now());
        assert!(can_pull_push(&ws));
        ws.sync.config = Some(crate::vault_sync::SyncConfig {
            server: String::new(),
            folder: None,
            space: quire_sync::SpaceId::random().unwrap(),
            device: quire_sync::DeviceId::random().unwrap(),
            device_name: "test".into(),
            shared_state: false,
        });
        assert!(!can_pull_push(&ws));
        let n = ws.notices.len();
        push(&mut ws);
        assert!(ws.features.git.busy.is_empty(), "nothing ran");
        assert!(ws.notices[n..].iter().any(|m| m.text == SYNC_ON));
        ws.sync.config = None;
        // Commit still works, and says when there's nothing to commit.
        commit_now(&mut ws);
        wait(&mut ws, Instant::now());
        assert!(ws.notices.iter().any(|m| m.text == "Nothing to commit"));
        ws.shutdown();
    }

    #[test]
    fn a_missing_git_is_explained() {
        let dir = tempfile::tempdir().unwrap();
        let mut ws = Workspace::new(Settings::default());
        ws.open_vault(dir.path()).unwrap();
        wait(&mut ws, Instant::now());
        assert!(!ws.features.git.repo);
        assert!(ws.features.git.unavailable.is_some());
        let n = ws.notices.len();
        commit_now(&mut ws);
        assert!(ws.notices.len() > n);
        ws.shutdown();
    }
}
