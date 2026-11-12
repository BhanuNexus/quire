// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! `quire-cli serve` (Beta, for self-hosters): one HTTP server for a vault,
//! with the web app for people and MCP (with its OAuth sign-in) for agents
//! on the same port (design 0.10-serve §2). `--sync` keeps the vault synced
//! in the same process. The server reads the plain notes, so it holds a
//! decrypted copy of what it syncs; it is not "Quire on the web" (a later,
//! end-to-end encrypted browser app).
//!
//! Sign-in is always needed, even on 127.0.0.1. Quire terminates no TLS:
//! for use from other computers, put a reverse proxy (Caddy, nginx) or a
//! tunnel in front and pass `--public-url https://…` (docs/serve.md).

use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use quire_app::vault_sync::{SyncEvent, SyncStatus, SyncWorker};
use quire_mcp::{Server, ServerConfig};

use super::sync::{prompt_hidden, read_secret, shutdown_signal};
use super::{CliCommand, arg};

pub(crate) const COMMANDS: &[CliCommand] = &[CliCommand {
    name: "serve",
    rank: 41,
    usage: "  quire-cli serve <vault> [--port 7311] [--bind ADDR] [--public-url URL] [--read-only] [--hide FOLDER]…
                        [--sync [--poll 60s]]   Beta: the web app and MCP (with sign-in) on one port, on a server you own
  quire-cli serve passwd [--passphrase-file F]   set the web app's passphrase (or QUIRE_WEB_PASSPHRASE)",
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

fn now() -> i64 {
    quire_app::history_ops::now_ms()
}

/// `quire-cli serve --help`.
pub(crate) const HELP: &str = "\
quire-cli serve (Beta, for self-hosters): your agents and your browser, on a server you own.

One port serves a web app (sign in, browse, search, read and edit notes) and MCP over
HTTP for AI agents, with OAuth sign-in. It listens on 127.0.0.1 unless you say otherwise;
put HTTPS (a reverse proxy or a tunnel) in front of it for use from other computers.

The server reads your notes as plain files, so it holds a decrypted copy of the vault and
of every folder it syncs (--sync). That is unlike Quire Sync, whose server only ever sees
encrypted data. Run it only on a machine you trust. This is not \"Quire on the web\": an
end-to-end encrypted browser app is planned separately.

USAGE:
  quire-cli serve <vault> [options]
  quire-cli serve passwd [--passphrase-file F]   set the web passphrase (or QUIRE_WEB_PASSPHRASE)

OPTIONS:
  --port N               port (default 7311)
  --bind ADDR            address to listen on (default 127.0.0.1)
  --public-url URL       the https:// address people and agents use (needed with --bind 0.0.0.0)
  --read-only            no edits from the web app or agents
  --hide FOLDER          keep a folder out of the web app (repeatable; also [web] hide in .quire/shared/vault.toml)
  --sync [--poll 60s]    sync the vault in the same process (after `quire-cli sync join`)
  --allow-private-urls   let agents fetch addresses on private networks

Setup guide: docs/serve.md";

fn run(args: &[String]) -> Result<(), String> {
    let first = arg(args, 1)?;
    if matches!(first, "--help" | "-h" | "help") && !Path::new(first).is_dir() {
        println!("{HELP}");
        return Ok(());
    }
    if first == "passwd" && !Path::new(first).is_dir() {
        return passwd(args);
    }
    serve(Path::new(first), args)
}

fn passwd(args: &[String]) -> Result<(), String> {
    let store = quire_web::auth::LoginStore::default_store().ok_or("no config folder on this system")?;
    let from_env = std::env::var_os("QUIRE_WEB_PASSPHRASE").is_some();
    let file = flag(args, "--passphrase-file");
    let pass =
        read_secret(&format!("New web passphrase ({} characters or more)", quire_web::auth::MIN_PASSPHRASE), "QUIRE_WEB_PASSPHRASE", file)?;
    if !from_env && file.is_none() && std::io::IsTerminal::is_terminal(&std::io::stdin()) {
        let again = prompt_hidden("Type it again: ")?;
        if again != pass {
            return Err("The passphrases differ; nothing changed.".into());
        }
    }
    store.set(&pass, now())?;
    println!("Web passphrase set (stored as an Argon2id hash in {}). Anyone signed in is signed out.", store.path().display());
    Ok(())
}

fn serve(vault: &Path, args: &[String]) -> Result<(), String> {
    if !vault.is_dir() {
        return Err(format!("{} isn't a folder", vault.display()));
    }
    let port: u16 = flag(args, "--port").map_or(Ok(7311), |p| p.parse().map_err(|_| format!("--port takes a number, not “{p}”")))?;
    let bind: IpAddr = flag(args, "--bind")
        .map_or_else(|| Ok(IpAddr::from([127, 0, 0, 1])), |b| b.parse().map_err(|_| format!("--bind takes an IP address, not “{b}”")))?;
    let public_url = flag(args, "--public-url").unwrap_or_default().to_string();
    if bind.is_unspecified() && public_url.is_empty() {
        return Err(format!(
            "Listening on every address ({bind}) needs --public-url, the address people and apps use (for example https://notes.example.com behind a reverse proxy)."
        ));
    }
    let read_only = has(args, "--read-only");
    let login = quire_web::auth::LoginStore::default_store().ok_or("no config folder on this system")?;
    if !login.is_set() {
        return Err("Set the web app's passphrase first: quire-cli serve passwd".into());
    }
    let poll = flag(args, "--poll").map_or(Ok(Duration::from_mins(1)), super::sync::parse_duration)?;
    let sync = has(args, "--sync");
    let sync_cfg = quire_app::vault_sync::SyncConfig::load(vault);
    if sync && sync_cfg.is_none() {
        return Err(format!("--sync: this vault doesn't sync yet. Run `quire-cli sync join {}` first.", vault.display()));
    }

    let stop = shutdown_signal()?;
    let root = vault.to_str().ok_or("the vault's path isn't valid UTF-8")?;
    // Over HTTP, an agent's writes depend on its token's scopes and the
    // vault's agent policy; --read-only allows none.
    let srv = Server::open_with(
        root,
        ServerConfig {
            allow_write: true,
            read_only,
            allow_private_urls: has(args, "--allow-private-urls"),
            prompts: quire_assistant::prompts::mcp_defs(),
            chat: quire_app::ai::chat_model(&quire_app::Settings::load()),
            ..ServerConfig::default()
        },
    )?;
    let mut web = quire_web::WebConfig::new(srv.clone())?;
    web.read_only = read_only;
    web.hide = flags(args, "--hide");
    if let Some(c) = &sync_cfg {
        // Web edits are recorded as made on this server, under its sync name.
        web.device.name.clone_from(&c.device_name);
    }
    let mut cfg = quire_mcp::http::HttpConfig::local(port).ok_or("no config folder on this system")?;
    cfg.bind = bind;
    cfg.public_url = public_url;
    if !bind.is_loopback() {
        eprintln!(
            "Warning: listening on {bind} lets other computers reach this server. Every request still needs the web passphrase or an MCP token, \
but use HTTPS (a reverse proxy or tunnel) for anything beyond your own network."
        );
    }
    let http = quire_mcp::http::HttpServer::start_with(srv, cfg, |urls| quire_web::mount(web, urls))?;
    let urls = http.urls().clone();
    eprintln!("Quire serve (Beta) is serving {} at {}/{}", vault.display(), urls.issuer, if read_only { " (read-only)" } else { "" });
    eprintln!("  This server holds a decrypted copy of the vault: run it only on a machine you trust.");
    eprintln!("  web app: {}/      MCP: {}", urls.issuer, urls.resource);
    eprintln!("Stop with Ctrl+C.");

    let worker = if sync {
        let w = SyncWorker::start_headless(vault, poll)?;
        eprintln!("Syncing every {}s and whenever something changes.", poll.as_secs());
        Some(w)
    } else {
        None
    };
    let local = if sync {
        quire_app::vault::VaultWatcher::start(&PathBuf::from(root).canonicalize().map_err(|e| e.to_string())?).ok()
    } else {
        None
    };
    let consent = quire_mcp::consent::ConsentStore::default_store().ok_or("no config folder on this system")?;
    let mut seen = std::collections::HashSet::new();
    let mut last_sync = String::new();
    loop {
        if let Ok(sig) = stop.recv_timeout(Duration::from_millis(500)) {
            eprintln!("{sig}: stopping…");
            if let Some(w) = worker {
                w.stop_and_wait();
            }
            http.stop();
            eprintln!("Stopped.");
            return Ok(());
        }
        for r in consent.pending(now()).into_iter().filter(|r| r.resource == urls.resource) {
            if seen.insert(r.id.clone()) {
                eprintln!(
                    "\n{} (calls itself {}) asks for {}.\n  Its browser page shows the code {}: approve only if it matches.\n  Approve in the web app ({}/consent), or: quire-cli mcp approve {}",
                    r.client_host,
                    r.client_name,
                    r.scopes().describe().join("; "),
                    r.verify_code(),
                    urls.issuer,
                    r.id
                );
            }
        }
        if let Some(w) = &worker {
            if let Some(l) = &local
                && l.events.try_iter().count() > 0
            {
                w.nudge();
            }
            for e in w.events() {
                let line = match e {
                    SyncEvent::Status(SyncStatus::Offline(e)) => format!("Sync offline: {e} (retrying)"),
                    SyncEvent::Status(SyncStatus::Error(e)) => format!("Sync needs attention: {e}"),
                    SyncEvent::Status(SyncStatus::Waiting(w)) => format!("Sync waiting for files: {w}"),
                    SyncEvent::Status(SyncStatus::Elsewhere(e)) => format!("Sync paused: {e}"),
                    SyncEvent::Report(r) => {
                        for p in &r.conflict_copies {
                            eprintln!("Sync conflict: the other version was saved as “{p}”.");
                        }
                        format!("Synced: received {}, sent {}", r.pulled, r.pushed)
                    }
                    SyncEvent::Notice(n) => format!("Sync: {n}"),
                    SyncEvent::Status(_) => continue,
                };
                if line != last_sync {
                    eprintln!("{line}");
                    last_sync = line;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn help_says_beta_and_that_the_server_holds_plain_notes() {
        let args: Vec<String> = ["serve", "--help"].map(String::from).to_vec();
        assert_eq!(super::run(&args), Ok(()));
        assert!(super::HELP.contains("Beta, for self-hosters"));
        assert!(super::HELP.contains("decrypted copy"));
        assert!(super::COMMANDS[0].usage.contains("Beta"));
    }
}
