// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! `quire-cli lock | unlock | status`: locked folders without the app
//! (docs/design/0.11-secure.md).
//!
//! The CLI keeps no unlocked state: `lock` converts a folder (or finishes
//! an interrupted conversion), `unlock --check` tests a passphrase,
//! `unlock --convert` turns a locked folder back into a plain one, and
//! `unlock --cat NOTE` prints one note. Passphrases come from a prompt
//! that doesn't echo, `QUIRE_LOCK_PASSPHRASE` or `--passphrase-file`,
//! never the command line. A new folder's recovery key is written to a new
//! file only you can read (`--recovery-key-file`), never shown.

use std::io::IsTerminal;
use std::path::{Path, PathBuf};

use quire_app::vault::lock::zeroize::Zeroizing;
use quire_app::vault::lock::{self, FolderKey, Kdf};
use quire_app::vault::{NotePath, Vault};

use super::sync::{prompt_hidden, read_secret};
use super::{CliCommand, arg};

pub(crate) const COMMANDS: &[CliCommand] = &[
    CliCommand {
        name: "lock",
        rank: 45,
        usage: "  quire-cli lock      <vault> <folder> [--recovery-key-file F] [--passphrase-file F]
                        lock a folder: its notes and files are encrypted on disk (a new one needs
                        --recovery-key-file F: the recovery key is written there, never shown); again: finish
  quire-cli lock      <vault> <folder> --keep-both [--passphrase-file F]
                        plain files next to an encrypted version: encrypt each under a new name
  quire-cli lock      <vault> <folder> --change-passphrase [--recovery-key] [--new-passphrase-file F]
                        a new passphrase (with the current one, or the recovery key)",
        run: lock_cmd,
    },
    CliCommand {
        name: "unlock",
        rank: 46,
        usage: "  quire-cli unlock    <vault> <folder> --check | --cat NOTE | --convert [--passphrase-file F]
                        test the passphrase, print one note, or decrypt the folder for good
                        (its encrypted files go to .trash/)
  quire-cli unlock    <vault> <folder> --recovery-key-file F [--passphrase-file F]
                        write the folder's recovery key to a new file F, readable only by you",
        run: unlock_cmd,
    },
    CliCommand {
        name: "status",
        rank: 47,
        usage: "  quire-cli status    <vault>                 locked folders and what state they're in",
        run: status_cmd,
    },
];

fn flag<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).map(String::as_str)
}

fn has(args: &[String], name: &str) -> bool {
    args.iter().any(|a| a == name)
}

fn open(args: &[String]) -> Result<(PathBuf, Vault), String> {
    let root = PathBuf::from(arg(args, 1)?);
    if !root.is_dir() {
        return Err(format!("{} isn't a folder", root.display()));
    }
    let v = quire_app::secure_state::open_vault(&root)?;
    Ok((root, v))
}

fn folder_arg(args: &[String]) -> Result<String, String> {
    Ok(arg(args, 2)?.trim().trim_matches('/').to_string())
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

fn passphrase(what: &str, args: &[String]) -> Result<Zeroizing<String>, String> {
    read_secret(what, "QUIRE_LOCK_PASSPHRASE", flag(args, "--passphrase-file")).map(Zeroizing::new)
}

/// Write a recovery key to a new file only this user can read.
fn write_recovery_key(out: &Path, rk: &str) -> Result<(), String> {
    let line = Zeroizing::new(format!("{rk}\n"));
    quire_app::atomic::write_new_private(out, line.as_bytes()).map_err(|e| format!("couldn't write {}: {e}", out.display()))
}

fn lock_cmd(args: &[String]) -> Result<(), String> {
    let (root, vault) = open(args)?;
    let folder = folder_arg(args)?;
    if has(args, "--change-passphrase") {
        return change_passphrase(&vault, &folder, args);
    }
    if has(args, "--keep-both") {
        let pass = passphrase(&format!("Passphrase of {folder}/"), args)?;
        vault.unlock(&folder, &pass).map_err(|e| e.to_string())?;
        let kept = lock::keep_both(&vault, &folder).map_err(|e| e.to_string())?;
        for k in &kept {
            println!("kept the plain copy as {k} (encrypted)");
        }
        println!("{} plain cop{} encrypted under new names.", kept.len(), if kept.len() == 1 { "y" } else { "ies" });
        return Ok(());
    }
    if let Some(why) = lock::refuse_to_lock(&vault, &folder) {
        return Err(why);
    }
    if !root.join(&folder).is_dir() {
        return Err(format!("{folder}/ isn't a folder in {}", root.display()));
    }
    let new = vault.key_file(&folder).is_err();
    let out = if new {
        let f = flag(args, "--recovery-key-file").ok_or(
            "a new locked folder needs --recovery-key-file F: its recovery key is written there (and never shown), to keep somewhere safe",
        )?;
        let out = PathBuf::from(f);
        if out.exists() {
            return Err(format!("{} already exists; pick a new file for the recovery key", out.display()));
        }
        Some(out)
    } else {
        None
    };
    let pass = if new {
        let p = passphrase("New passphrase for the folder (8 characters or more)", args)?;
        if std::io::stdin().is_terminal()
            && flag(args, "--passphrase-file").is_none()
            && std::env::var_os("QUIRE_LOCK_PASSPHRASE").is_none()
        {
            let again = prompt_hidden("Type it again: ")?;
            if *Zeroizing::new(again) != *p {
                return Err("The passphrases differ; nothing was changed.".into());
            }
        }
        p
    } else {
        passphrase(&format!("Passphrase of {folder}/"), args)?
    };
    // The key file first; its recovery key is written out before any file is
    // converted, so a conversion that stops can't lose it.
    let rk = lock::begin_lock(&vault, &folder, Some(&pass), Kdf::DEFAULT, now_ms()).map_err(|e| e.to_string())?.map(Zeroizing::new);
    if let (Some(out), Some(rk)) = (&out, &rk) {
        write_recovery_key(out, rk).map_err(|e| {
            format!(
                "{folder}/ has its key file now, but the recovery key wasn't saved: {e}. Write it with `quire-cli unlock {} {folder} --recovery-key-file F`, then run this again.",
                root.display()
            )
        })?;
    }
    let report = lock::lock_folder(&vault, &folder, Some(&pass), Kdf::DEFAULT, now_ms()).map_err(|e| {
        let saved = out.as_ref().map(|o| format!(" The recovery key is in {}.", o.display())).unwrap_or_default();
        format!("{e}. Run the same command again to finish.{saved}")
    })?;
    // What this device kept that came from the plain notes.
    let device = quire_app::device::this_device();
    let store = quire_app::agent::AgentStore::open(&vault, device, std::sync::Arc::new(quire_app::agent::SystemClock));
    let gone = store.forget_suggestions_under(&folder);
    quire_app::secure_state::clear_caches(&root);
    println!(
        "{folder}/ is locked: {} file{} encrypted, {} history version{}.",
        report.files,
        plural(report.files),
        report.history,
        plural(report.history)
    );
    if gone > 0 {
        println!("Removed {gone} agent suggestion{} about its notes (they held their text).", plural(gone));
    }
    if !report.conflicts.is_empty() {
        println!(
            "{} plain file{} next to an encrypted version with other content {} left as {} (the encrypted one is the note):",
            report.conflicts.len(),
            plural(report.conflicts.len()),
            if report.conflicts.len() == 1 { "was" } else { "were" },
            if report.conflicts.len() == 1 { "it is" } else { "they are" }
        );
        for c in &report.conflicts {
            println!("  {c}");
        }
        println!("Keep both with `quire-cli lock {} {folder} --keep-both`.", root.display());
    }
    if let Some(out) = out {
        println!("The recovery key is in {} (readable only by you). Keep it somewhere safe and delete it here.", out.display());
    }
    println!(
        "Copies made before locking aren't changed: backups, git history, a cloud folder's version history, other devices' history, \
         and notes already in .trash/ (empty the trash to remove those)."
    );
    Ok(())
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

fn change_passphrase(vault: &Vault, folder: &str, args: &[String]) -> Result<(), String> {
    let key = if has(args, "--recovery-key") {
        let rk = Zeroizing::new(read_secret("Recovery key", "QUIRE_LOCK_RECOVERY_KEY", flag(args, "--recovery-key-file"))?);
        let key = FolderKey::from_recovery_key(&rk).map_err(|e| e.to_string())?;
        // Checked before anything is written (against the key file, or the folder's encrypted files).
        vault.unlock_with_key(folder, key.clone()).map_err(|e| e.to_string())?;
        key
    } else {
        let (_, kf) = vault.key_file(folder).map_err(|e| e.to_string())?;
        let cur = passphrase(&format!("Current passphrase of {folder}/"), args)?;
        kf.unwrap_key(&cur).map_err(|e| e.to_string())?
    };
    let new = Zeroizing::new(read_secret(
        "New passphrase (8 characters or more)",
        "QUIRE_LOCK_NEW_PASSPHRASE",
        flag(args, "--new-passphrase-file"),
    )?);
    vault.change_passphrase(folder, Some(key), &new, now_ms()).map_err(|e| e.to_string())?;
    println!(
        "{folder}/ has a new passphrase. Its files didn't change: the recovery key still works, and so does an old copy of the key file \
         (a backup, git history) with the old passphrase."
    );
    Ok(())
}

fn unlock_cmd(args: &[String]) -> Result<(), String> {
    let (_, vault) = open(args)?;
    let folder = folder_arg(args)?;
    let (f, kf) = vault.key_file(&folder).map_err(|_| {
        if vault.known_locked().contains(&folder) {
            quire_app::secure_state::key_missing_text(&folder)
        } else {
            format!("{folder}/ isn't a locked folder")
        }
    })?;
    let pass = passphrase(&format!("Passphrase of {folder}/"), args)?;
    if let Some(out) = flag(args, "--recovery-key-file").filter(|_| !has(args, "--convert")) {
        let out = PathBuf::from(out);
        if out.exists() {
            return Err(format!("{} already exists; pick a new file for the recovery key", out.display()));
        }
        let key = kf.unwrap_key(&pass).map_err(|e| match e {
            lock::LockError::WrongPassphrase => "Wrong passphrase.".to_string(),
            e => e.to_string(),
        })?;
        write_recovery_key(&out, &Zeroizing::new(key.recovery_key()))?;
        println!(
            "The recovery key of {folder}/ is in {} (readable only by you). Keep it somewhere safe and delete it here.",
            out.display()
        );
        return Ok(());
    }
    if !f.unlocking {
        vault.unlock(&folder, &pass).map_err(|e| match e {
            quire_app::vault::VaultError::Locked(_) => "Wrong passphrase.".to_string(),
            e => e.to_string(),
        })?;
    }
    if has(args, "--convert") {
        let r = lock::unlock_folder_permanently(&vault, &folder, Some(&pass)).map_err(|e| e.to_string())?;
        println!(
            "{folder}/ is an ordinary folder again: {} file{} decrypted, {} history version{}. The encrypted files are in .trash/.",
            r.files,
            plural(r.files),
            r.history,
            plural(r.history)
        );
        return Ok(());
    }
    if let Some(note) = flag(args, "--cat") {
        let mut p = note.trim().trim_start_matches("./").to_string();
        if !p.contains('/') {
            p = format!("{folder}/{p}");
        }
        if !p.to_lowercase().ends_with(".md") {
            p.push_str(".md");
        }
        let text = vault.read(&NotePath::new(&p)).map_err(|e| e.to_string())?;
        print!("{text}");
        return Ok(());
    }
    if has(args, "--check") {
        println!("The passphrase opens {folder}/ ({} notes).", vault.locked_notes(&folder).len());
        return Ok(());
    }
    Err(super::usage())
}

fn status_cmd(args: &[String]) -> Result<(), String> {
    let (root, vault) = open(args)?;
    print!("{}", status(&root, &vault));
    Ok(())
}

pub(crate) fn status(root: &Path, vault: &Vault) -> String {
    use std::fmt::Write as _;
    let folders = vault.locked_folders();
    if folders.is_empty() {
        return format!("No locked folders in {}.\n", root.display());
    }
    let left = lock::plain_left(vault);
    let mut s = format!("Locked folders in {}:\n", root.display());
    for f in folders {
        let path = &f.folder.path;
        let files = vault.locked_files(path);
        let notes = files.iter().filter(|p| p.is_markdown()).count();
        let _ = write!(s, "  {path}/  {notes} note{}, {} other file{}", plural(notes), files.len() - notes, plural(files.len() - notes));
        if f.key_missing {
            let _ =
                write!(s, "  — its key file ({}) is missing: restore it, or set a new passphrase with the recovery key", lock::KEY_FILE);
        } else if f.folder.unlocking {
            let _ = write!(s, "  — being decrypted for good; finish with `quire-cli unlock {} {path} --convert`", root.display());
        } else if let Some(l) = left.iter().find(|l| &l.folder == path) {
            if !l.files.is_empty() {
                let _ = write!(
                    s,
                    "  — {} file{} not encrypted yet; finish with `quire-cli lock {} {path}`",
                    l.files.len(),
                    plural(l.files.len()),
                    root.display()
                );
            }
            if !l.conflicts.is_empty() {
                let _ = write!(
                    s,
                    "  — {} plain file{} next to an encrypted version; keep both with `quire-cli lock {} {path} --keep-both`",
                    l.conflicts.len(),
                    plural(l.conflicts.len()),
                    root.display()
                );
            }
        }
        s.push('\n');
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_reports_folders_and_unfinished_work() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("Journal")).unwrap();
        std::fs::write(root.join("Journal/Day.md"), "x").unwrap();
        std::fs::write(root.join("Journal/pic.png"), "x").unwrap();
        let v = Vault::open_local(root).unwrap();
        assert!(status(root, &v).starts_with("No locked folders"));
        lock::lock_folder(&v, "Journal", Some("pass phrase"), Kdf::TEST, 1).unwrap();
        let s = status(root, &v);
        assert!(s.contains("Journal/  1 note, 1 other file\n"), "{s}");
        // A plain file that arrived from elsewhere (written around Quire's refusal).
        std::fs::write(root.join("Journal/New.md"), "plain").unwrap();
        assert!(status(root, &v).contains("1 file not encrypted yet"));
    }
}
