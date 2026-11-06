// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! `quire-cli sync`: Quire Sync without the app, for a server or a home box
//! that keeps a copy of the vault (design 0.10-serve §1).
//!
//! The same engine, transports, conflict handling and history as the app:
//! `join` sets a vault folder up like Settings → Sync does, `once` runs one
//! pass, `watch` keeps syncing until SIGINT or SIGTERM, `status` says how
//! it went. `--only` and `--exclude` choose folders (selective sync): notes
//! outside the selection are never written here and never sent as deleted.
//!
//! Secrets never go on the command line or the screen: the passphrase
//! comes from a prompt, `QUIRE_SYNC_PASSPHRASE` or `--passphrase-file`; a
//! server token from a prompt, `QUIRE_SYNC_TOKEN` or `--token-file`. The
//! vault key and token are kept in the keychain or the private secrets file
//! (`QUIRE_SECRETS_FILE` picks another), as the app keeps them.

use std::fmt::Write as _;
use std::io::{BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use quire_app::vault_sync::{self, Selection, SyncEvent, SyncStatus, SyncTarget, SyncWorker};

use super::{CliCommand, arg};

pub(crate) const COMMANDS: &[CliCommand] = &[CliCommand {
    name: "sync",
    rank: 40,
    usage: "  quire-cli sync join   <vault> (--folder DIR | --server URL) [--space ID | --create] [--device NAME]
                        [--passphrase-file F] [--token-file F] [--only FOLDER]… [--exclude FOLDER]…
                        set this folder up to sync (passphrase: prompt, QUIRE_SYNC_PASSPHRASE or the file;
                        server token: prompt, QUIRE_SYNC_TOKEN or the file; --create needs --recovery-key-file F)
  quire-cli sync once   <vault>                 one sync pass
  quire-cli sync watch  <vault> [--poll 60s]    keep syncing until Ctrl+C or SIGTERM
  quire-cli sync status <vault>                 how sync is set up and how the last pass went
  quire-cli sync scope  <vault> (--only FOLDER… [--exclude FOLDER…] | --all)   change which folders sync (not while a sync runs)
  quire-cli sync share  <vault> on|off          also sync vault settings and review history (agents' suggestions
                        and runs reach your other devices; every device needs Quire 0.9 or later)",
    run,
}];

fn flag<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).map(String::as_str)
}

fn flags(args: &[String], name: &str) -> Vec<String> {
    args.windows(2).filter(|w| w[0] == name).map(|w| w[1].clone()).collect()
}

fn has(args: &[String], name: &str) -> bool {
    args.iter().any(|a| a == name)
}

fn run(args: &[String]) -> Result<(), String> {
    let sub = arg(args, 1)?;
    let vault = PathBuf::from(arg(args, 2)?);
    if !vault.is_dir() {
        return Err(format!("{} isn't a folder", vault.display()));
    }
    match sub {
        "join" => join(&vault, args),
        "once" => once(&vault),
        "watch" => {
            let poll = flag(args, "--poll").map_or(Ok(Duration::from_mins(1)), parse_duration)?;
            watch(&vault, poll)
        }
        "status" => {
            print!("{}", status(&vault));
            Ok(())
        }
        "scope" => scope(&vault, args),
        "share" => {
            let on = match arg(args, 3)? {
                "on" => true,
                "off" => false,
                other => return Err(format!("sync share takes on or off, not “{other}”")),
            };
            vault_sync::sync_share_at(&vault, on)?;
            println!(
                "{}",
                if on {
                    "Vault settings and review history now sync from this device: agents' suggestions made here can be reviewed on your other devices."
                } else {
                    "Vault settings and review history no longer sync from this device."
                }
            );
            Ok(())
        }
        other => Err(format!("unknown sync command “{other}”\n\n{}", super::usage())),
    }
}

/// `30s`, `5m`, `1h`.
pub(crate) fn parse_duration(s: &str) -> Result<Duration, String> {
    let bad = || format!("--poll takes a time such as 30s, 5m or 1h, not “{s}”");
    let (n, unit) = s.split_at(s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len()));
    let n: u64 = n.parse().map_err(|_| bad())?;
    let secs = match unit {
        "" | "s" => n,
        "m" => n * 60,
        "h" => n * 3600,
        _ => return Err(bad()),
    };
    if secs < 5 {
        return Err("--poll must be at least 5s".into());
    }
    Ok(Duration::from_secs(secs))
}

fn selection(args: &[String]) -> Result<Selection, String> {
    Selection::new(&flags(args, "--only"), &flags(args, "--exclude"))
}

/// A secret from an environment variable, a file, or a prompt that
/// doesn't echo. Never from the command line.
pub(crate) fn read_secret(what: &str, env: &str, file: Option<&str>) -> Result<String, String> {
    if let Some(f) = file {
        let s = std::fs::read_to_string(f).map_err(|e| format!("can't read {f}: {e}"))?;
        return Ok(s.trim_end_matches(['\r', '\n']).to_string());
    }
    if let Ok(v) = std::env::var(env)
        && !v.is_empty()
    {
        return Ok(v);
    }
    prompt_hidden(&format!("{what}: "))
}

/// Ask on the terminal without echoing what's typed (Unix: `stty -echo`).
/// With stdin not a terminal, one line is read from it.
pub(crate) fn prompt_hidden(prompt: &str) -> Result<String, String> {
    let stdin = std::io::stdin();
    if !stdin.is_terminal() {
        let mut line = String::new();
        stdin.lock().read_line(&mut line).map_err(|e| e.to_string())?;
        return Ok(line.trim_end_matches(['\r', '\n']).to_string());
    }
    eprint!("{prompt}");
    let _ = std::io::stderr().flush();
    let hidden = cfg!(unix) && stty(&["-echo"]);
    if !hidden {
        eprint!("(this terminal shows what you type) ");
    }
    let mut line = String::new();
    let read = stdin.lock().read_line(&mut line);
    if hidden {
        stty(&["echo"]);
        eprintln!();
    }
    read.map_err(|e| e.to_string())?;
    Ok(line.trim_end_matches(['\r', '\n']).to_string())
}

fn stty(args: &[&str]) -> bool {
    std::process::Command::new("stty").args(args).stdin(std::process::Stdio::inherit()).status().is_ok_and(|s| s.success())
}

fn prompt_line(prompt: &str) -> Result<String, String> {
    eprint!("{prompt}");
    let _ = std::io::stderr().flush();
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line).map_err(|e| e.to_string())?;
    Ok(line.trim().to_string())
}

fn join(vault: &Path, args: &[String]) -> Result<(), String> {
    let interactive = std::io::stdin().is_terminal();
    let target = match (flag(args, "--folder"), flag(args, "--server")) {
        (Some(_), Some(_)) => return Err("use --folder or --server, not both".into()),
        (Some(f), None) => SyncTarget::Folder(PathBuf::from(f)),
        (None, Some(url)) => {
            let token = read_secret("Sync server token", "QUIRE_SYNC_TOKEN", flag(args, "--token-file"))?;
            SyncTarget::Server { url: url.to_string(), token }
        }
        (None, None) if interactive => {
            let answer =
                prompt_line("Sync through a folder (Dropbox, Syncthing…) or a Quire Sync server? Type a folder path or an https:// URL: ")?;
            if answer.starts_with("http://") || answer.starts_with("https://") {
                let token = read_secret("Sync server token", "QUIRE_SYNC_TOKEN", flag(args, "--token-file"))?;
                SyncTarget::Server { url: answer, token }
            } else {
                SyncTarget::Folder(PathBuf::from(answer))
            }
        }
        (None, None) => return Err("say where the vault syncs: --folder DIR or --server URL".into()),
    };
    let sel = selection(args)?;
    let device = flag(args, "--device").map_or_else(vault_sync::default_device_name, str::to_string);
    if vault_sync::SyncConfig::load(vault).is_some() {
        return Err(format!(
            "{} already syncs. To join another vault, move .quire/sync/config.json out of the way first.",
            vault.display()
        ));
    }
    let passphrase_file = flag(args, "--passphrase-file");
    if has(args, "--create") {
        let out = flag(args, "--recovery-key-file")
            .ok_or("--create needs --recovery-key-file F: the recovery key is written there (and never shown), to keep somewhere safe")?;
        let out = PathBuf::from(out);
        if out.exists() {
            return Err(format!("{} already exists; pick a new file for the recovery key", out.display()));
        }
        let pass = read_secret("New sync passphrase (8 characters or more)", "QUIRE_SYNC_PASSPHRASE", passphrase_file)?;
        if interactive && passphrase_file.is_none() && std::env::var_os("QUIRE_SYNC_PASSPHRASE").is_none() {
            let again = prompt_hidden("Type it again: ")?;
            if again != pass {
                return Err("The passphrases differ; nothing was set up.".into());
            }
        }
        let rk = vault_sync::sync_create_at(vault, &target, &pass, &device, &sel)?;
        quire_app::atomic::write_new_private(&out, format!("{rk}\n").as_bytes())
            .map_err(|e| format!("can't write {}: {e}", out.display()))?;
        println!("Created a new sync vault and joined it as “{device}” ({}).", sel.describe());
        println!("The recovery key is in {} (readable only by you). Keep it somewhere safe and delete it here.", out.display());
        return Ok(());
    }
    let spaces = vault_sync::sync_list(&target)?;
    let space = match flag(args, "--space") {
        Some(id) => spaces
            .iter()
            .find(|s| s.space.0 == id)
            .map(|s| s.space.clone())
            .ok_or_else(|| format!("no sync vault {id} there; `--space` takes one of: {}", ids(&spaces)))?,
        None if spaces.len() == 1 => spaces[0].space.clone(),
        None if spaces.is_empty() => return Err("No sync vault there yet. Set one up in the app, or use --create.".into()),
        None if interactive => {
            for (i, s) in spaces.iter().enumerate() {
                eprintln!("  {}. {}  created {}  {}", i + 1, s.space.0, quire_app::chrono_fmt(s.created * 1000), s.detail);
            }
            let n: usize = prompt_line("Which one? ")?.parse().map_err(|_| "type the number".to_string())?;
            spaces.get(n.wrapping_sub(1)).ok_or("no such number")?.space.clone()
        }
        None => return Err(format!("there are several sync vaults; pick one with --space: {}", ids(&spaces))),
    };
    let secret = read_secret("Sync passphrase (or recovery key)", "QUIRE_SYNC_PASSPHRASE", passphrase_file)?;
    vault_sync::sync_join_at(vault, &target, &space, &secret, &device, &sel)?;
    println!("Joined sync vault {} as “{device}” ({}). Run `quire-cli sync watch {}`.", space.0, sel.describe(), vault.display());
    Ok(())
}

fn ids(spaces: &[vault_sync::SyncVaultInfo]) -> String {
    spaces.iter().map(|s| s.space.0.as_str()).collect::<Vec<_>>().join(", ")
}

fn report_line(r: &vault_sync::SyncReport) -> String {
    let mut s = format!("received {}, sent {}", r.pulled, r.pushed);
    if !r.merged.is_empty() {
        let _ = write!(s, ", merged {}", r.merged.len());
    }
    s
}

fn once(vault: &Path) -> Result<(), String> {
    let r = vault_sync::sync_once_at(vault)?;
    println!("Synced: {}.", report_line(&r));
    for p in &r.conflict_copies {
        println!("Conflict: two devices changed the same lines; the other version was saved as “{p}”.");
    }
    for p in &r.kept_over_delete {
        println!("Kept “{p}”: it was edited on one device and deleted on another.");
    }
    Ok(())
}

/// A channel that receives once on SIGINT or SIGTERM (Ctrl+C on Windows).
pub(crate) fn shutdown_signal() -> Result<mpsc::Receiver<&'static str>, String> {
    let (tx, rx) = mpsc::channel();
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().map_err(|e| e.to_string())?;
    std::thread::Builder::new()
        .name("signals".into())
        .spawn(move || {
            rt.block_on(async move {
                #[cfg(unix)]
                {
                    use tokio::signal::unix::{SignalKind, signal};
                    let Ok(mut term) = signal(SignalKind::terminate()) else { return };
                    let name = tokio::select! {
                        _ = tokio::signal::ctrl_c() => "SIGINT",
                        _ = term.recv() => "SIGTERM",
                    };
                    let _ = tx.send(name);
                }
                #[cfg(not(unix))]
                {
                    let _ = tokio::signal::ctrl_c().await;
                    let _ = tx.send("Ctrl+C");
                }
            });
        })
        .map_err(|e| e.to_string())?;
    Ok(rx)
}

fn now() -> String {
    chrono::Local::now().format("%H:%M:%S").to_string()
}

/// Sync until a signal; returns once the pass under way has finished.
pub(crate) fn watch_until(vault: &Path, poll: Duration, stop: &mpsc::Receiver<&'static str>, quiet: bool) -> Result<(), String> {
    let worker = SyncWorker::start_headless(vault, poll)?;
    // Edits made here (the web app, an agent, a text editor) sync soon.
    let root = vault.canonicalize().map_err(|e| e.to_string())?;
    let local = quire_app::vault::VaultWatcher::start(&root).ok();
    let mut last = String::new();
    let mut last_change = Instant::now();
    loop {
        if let Ok(sig) = stop.try_recv() {
            if !quiet {
                eprintln!("{} {sig}: finishing the current pass…", now());
            }
            worker.stop_and_wait();
            if !quiet {
                eprintln!("{} Stopped.", now());
            }
            return Ok(());
        }
        if let Some(w) = &local
            && w.events.try_iter().count() > 0
        {
            worker.nudge();
        }
        while let Some(e) = worker.next_event(Duration::from_millis(200)) {
            let line = match e {
                SyncEvent::Status(SyncStatus::Syncing | SyncStatus::Off) => continue,
                SyncEvent::Status(s @ SyncStatus::Synced(_)) => {
                    if last_change.elapsed() < Duration::from_secs(1) {
                        continue;
                    }
                    s.label()
                }
                SyncEvent::Status(SyncStatus::Offline(e)) => format!("Offline: {e} (retrying)"),
                SyncEvent::Status(SyncStatus::Waiting(w)) => format!("Waiting for files: {w}"),
                SyncEvent::Status(SyncStatus::Error(e)) => format!("Needs attention: {e}"),
                SyncEvent::Status(SyncStatus::Elsewhere(e)) => format!("Waiting: {e}"),
                SyncEvent::Report(r) => {
                    last_change = Instant::now();
                    for p in &r.conflict_copies {
                        println!("{} Conflict: the other version was saved as “{p}”.", now());
                    }
                    for p in &r.kept_over_delete {
                        println!("{} Kept “{p}”: edited on one device, deleted on another.", now());
                    }
                    format!("Synced: {}", report_line(&r))
                }
            };
            if line != last && !quiet {
                println!("{} {line}", now());
            }
            last = line;
        }
    }
}

fn watch(vault: &Path, poll: Duration) -> Result<(), String> {
    let stop = shutdown_signal()?;
    let summary = vault_sync::sync_summary_at(vault).ok_or("Sync isn't set up for this vault. Run `quire-cli sync join` first.")?;
    eprintln!(
        "Syncing {} as “{}” ({}), every {}s and whenever something changes. Ctrl+C stops.",
        vault.display(),
        summary.config.device_name,
        summary.config.selection().describe(),
        poll.as_secs()
    );
    watch_until(vault, poll, &stop, false)
}

fn status(vault: &Path) -> String {
    let Some(s) = vault_sync::sync_summary_at(vault) else {
        return "Sync isn't set up for this vault.\n".into();
    };
    let c = &s.config;
    let mut out = String::new();
    let via = c.folder.as_ref().map_or_else(|| format!("server {}", c.server), |f| format!("folder {}", f.display()));
    let _ = writeln!(out, "Syncs through {via}");
    let _ = writeln!(out, "Vault {}  device “{}”", c.space.0, c.device_name);
    let _ = writeln!(out, "Folders: {}", c.selection().describe());
    if c.shared_state {
        out.push_str("Vault settings and review history sync too.\n");
    }
    if !s.has_secrets {
        out.push_str("The sync key is missing on this computer: join again.\n");
    }
    let outside = if s.outside > 0 { format!(" ({} outside the selection, not stored here)", s.outside) } else { String::new() };
    let _ = writeln!(out, "Tracking {} files{outside}", s.tracked);
    match &s.last {
        None => out.push_str("Not synced yet.\n"),
        Some(l) => {
            let what = match l.state.as_str() {
                "synced" => "synced".to_string(),
                "waiting" => format!("waiting for files: {}", l.detail),
                "offline" => format!("offline: {}", l.detail),
                "error" => format!("needs attention: {}", l.detail),
                "elsewhere" => format!("waiting: {}", l.detail),
                other => other.to_string(),
            };
            let _ = write!(out, "Last pass {}: {what}", quire_app::chrono_fmt(l.at));
            if l.pulled + l.pushed > 0 {
                let _ = write!(out, " (received {}, sent {})", l.pulled, l.pushed);
            }
            out.push('\n');
            for p in &l.conflicts {
                let _ = writeln!(out, "Conflict copy: {p}");
            }
        }
    }
    out
}

fn scope(vault: &Path, args: &[String]) -> Result<(), String> {
    let sel = if has(args, "--all") {
        Selection::default()
    } else {
        let s = selection(args)?;
        if s.is_everything() {
            return Err("give --only and/or --exclude folders, or --all".into());
        }
        s
    };
    let before = vault_sync::SyncConfig::load(vault).map(|c| c.selection());
    vault_sync::sync_set_selection_at(vault, &sel)?;
    print!("{}", scope_message(before.as_ref(), &sel));
    Ok(())
}

/// What `sync scope` tells the person: only what is true once the next sync
/// starts (no sync runs meanwhile: `sync scope` refuses while one does).
fn scope_message(before: Option<&Selection>, after: &Selection) -> String {
    let mut out = format!("This device now syncs {}. The next `sync once`, `sync watch` or `serve --sync` uses it.\n", after.describe());
    if !after.is_everything() && before != Some(after) {
        out.push_str(
            "Files left out stay on this disk and stop syncing: other devices keep their copies, and nothing done to \
these files here is sent while they're left out. A note moved here into a folder left out is sent as a move.\n",
        );
    }
    if before.is_some_and(|b| !b.is_everything()) && before != Some(after) {
        out.push_str(
            "Notes now included are fetched as they are on the other devices. A copy here that was changed meanwhile is \
merged with them, or kept beside them if the changes overlap; an unchanged old copy of a note that moved or was deleted \
elsewhere goes to this vault's .trash. Nothing is deleted on other devices.\n",
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_and_selections_parse() {
        assert_eq!(parse_duration("30s").unwrap(), Duration::from_secs(30));
        assert_eq!(parse_duration("2m").unwrap(), Duration::from_mins(2));
        assert_eq!(parse_duration("90").unwrap(), Duration::from_secs(90));
        assert!(parse_duration("1s").is_err());
        assert!(parse_duration("x").is_err());
        let args: Vec<String> = ["sync", "join", "v", "--only", "Work", "--only", "./Projects/2026/", "--exclude", "Work/Old"]
            .iter()
            .map(|s| (*s).to_string())
            .collect();
        let s = selection(&args).unwrap();
        assert_eq!(s.only, ["Projects/2026/", "Work/"]);
        assert_eq!(s.exclude, ["Work/Old/"]);
        assert!(s.includes("Work/a.md") && !s.includes("Work/Old/a.md") && !s.includes("Home/a.md"));
        for bad in ["../x", ".git", "a/../b", ""] {
            assert!(Selection::new(&[bad.to_string()], &[]).is_err(), "{bad}");
        }
    }
}
