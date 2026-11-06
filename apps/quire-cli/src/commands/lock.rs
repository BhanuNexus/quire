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
  quire-cli lock      <vault> <folder> --change-passphrase [--recovery-key] [--new-passphrase-file F]
                        a new passphrase (with the current one, or the recovery key)",
        run: lock_cmd,
    },
    CliCommand {
        name: "unlock",
        rank: 46,
        usage: "  quire-cli unlock    <vault> <folder> --check | --cat NOTE | --convert [--passphrase-file F]
                        test the passphrase, print one note, or decrypt the folder for good
                        (its encrypted files go to .trash/)",
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
    let v = Vault::open_local(&root).map_err(|e| e.to_string())?;
    Ok((root, v))
}

fn folder_arg(args: &[String]) -> Result<String, String> {
    Ok(arg(args, 2)?.trim().trim_matches('/').to_string())
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

fn passphrase(what: &str, args: &[String]) -> Result<String, String> {
    read_secret(what, "QUIRE_LOCK_PASSPHRASE", flag(args, "--passphrase-file"))
}

fn lock_cmd(args: &[String]) -> Result<(), String> {
    let (root, vault) = open(args)?;
    let folder = folder_arg(args)?;
    if has(args, "--change-passphrase") {
        return change_passphrase(&vault, &folder, args);
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
            if again != p {
                return Err("The passphrases differ; nothing was changed.".into());
            }
        }
        p
    } else {
        passphrase(&format!("Passphrase of {folder}/"), args)?
    };
    let report = lock::lock_folder(&vault, &folder, Some(&pass), Kdf::DEFAULT, now_ms()).map_err(|e| e.to_string())?;
    if let (Some(out), Some(rk)) = (&out, &report.recovery_key) {
        quire_app::atomic::write_new_private(out, format!("{rk}\n").as_bytes()).map_err(|e| {
            format!("{folder}/ is locked, but the recovery key couldn't be written to {}: {e}. Keep the passphrase safe.", out.display())
        })?;
    }
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
    if let Some(out) = out {
        println!("The recovery key is in {} (readable only by you). Keep it somewhere safe and delete it here.", out.display());
    }
    println!("Copies made before locking (backups, git history, other devices' history) aren't changed.");
    Ok(())
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

fn change_passphrase(vault: &Vault, folder: &str, args: &[String]) -> Result<(), String> {
    let key = if has(args, "--recovery-key") {
        let rk = read_secret("Recovery key", "QUIRE_LOCK_RECOVERY_KEY", flag(args, "--recovery-key-file"))?;
        FolderKey::from_recovery_key(&rk).map_err(|e| e.to_string())?
    } else {
        let (_, kf) = vault.key_file(folder).map_err(|e| e.to_string())?;
        let cur = passphrase(&format!("Current passphrase of {folder}/"), args)?;
        kf.unwrap_key(&cur).map_err(|e| e.to_string())?
    };
    let new = read_secret("New passphrase (8 characters or more)", "QUIRE_LOCK_NEW_PASSPHRASE", flag(args, "--new-passphrase-file"))?;
    vault.change_passphrase(folder, Some(key), &new, now_ms()).map_err(|e| e.to_string())?;
    println!("{folder}/ has a new passphrase. Its files didn't change; the recovery key still works.");
    Ok(())
}

fn unlock_cmd(args: &[String]) -> Result<(), String> {
    let (_, vault) = open(args)?;
    let folder = folder_arg(args)?;
    let (f, _) = vault.key_file(&folder).map_err(|_| format!("{folder}/ isn't a locked folder"))?;
    let pass = passphrase(&format!("Passphrase of {folder}/"), args)?;
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
        if f.folder.unlocking {
            let _ = write!(s, "  — being decrypted for good; finish with `quire-cli unlock {} {path} --convert`", root.display());
        } else if let Some(l) = left.iter().find(|l| &l.folder == path) {
            let _ = write!(
                s,
                "  — {} file{} not encrypted yet; finish with `quire-cli lock {} {path}`",
                l.files.len(),
                plural(l.files.len()),
                root.display()
            );
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
