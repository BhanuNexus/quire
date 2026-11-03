// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! On-device AI and the wiki loop from the command line (design §D.2,
//! §D.7). Owned by WS-D.
//!
//! * `quire-cli ai models | download <id> | install <file>… | delete <id> | engine`
//! * `quire-cli ingest <vault> <source|url> [--yes] [--dry-run]`

use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::sync::Arc;

use quire_ai::catalog::{Catalog, human_size};
use quire_ai::download::{Control, Downloader, Installed1, Progress, install_from_files};
use quire_app::Settings;
use quire_app::agent::{Actor, AgentStore, SystemClock, Via};
use quire_app::intel::chat::{CancelFlag, Locality};
use quire_app::vault::{NotePath, Vault};
use quire_assistant::wiki::{Ingest, IngestEvent, estimate, ingest};

use super::{CliCommand, arg};

pub(crate) const COMMANDS: &[CliCommand] = &[
    CliCommand {
        name: "ai",
        rank: 40,
        usage: "  quire-cli ai        models | download <id> | install <file>… | delete <id> | engine
                      on-device models: list them, download one (resumes; checked by SHA-256), install files
                      downloaded elsewhere, delete one, or check the engine",
        run: run_ai,
    },
    CliCommand {
        name: "ingest",
        rank: 41,
        usage: "  quire-cli ingest    <vault> <raw/note.md | URL> [--yes] [--dry-run]
                      summarize a source into wiki/ with concept pages, cited; suggests links from your notes
                      (uses the AI set up in Quire; shows the cost on a cloud model first)",
        run: run_ingest,
    },
];

fn models_dir() -> Result<PathBuf, String> {
    quire_app::ai::models_dir().ok_or_else(|| "no folder for models on this system".to_string())
}

/// While on-device AI is off (`[ai_local] experimental`, design §D.1).
fn later() -> String {
    format!(
        "on-device AI is coming in an update, so this build doesn't download or run models. \
         Use a provider such as Ollama or LM Studio to keep everything on this computer today. \
         To try it anyway, tick Settings → AI → On this computer → Advanced → \"Experimental: show on-device AI\" \
         (or set {}=1).",
        quire_app::settings::EXPERIMENTAL_ENV
    )
}

fn run_ai(args: &[String]) -> Result<(), String> {
    let catalog = Catalog::builtin();
    let dir = models_dir()?;
    let on = Settings::load().ai_local.on_device();
    let sub = args.get(1).map(String::as_str);
    if !on && matches!(sub, Some("download" | "install" | "engine")) {
        return Err(later());
    }
    match sub {
        None | Some("models") => {
            if !on {
                println!("Note: {}\n", later());
            }
            let m = quire_ai::download::Manifest::load(&dir);
            for e in &catalog.models {
                let state = if quire_app::ai::installed(e) {
                    "installed"
                } else if !e.is_pinned() {
                    "not available in this build"
                } else if m.models.contains_key(&e.id) || quire_ai::download::partial_bytes(&dir, e) > 0 {
                    "partly downloaded"
                } else {
                    "not installed"
                };
                let size = if e.size() > 0 { human_size(e.size()) } else { "—".into() };
                println!("{:<24} {:<26} {:>8}  {}  ({})", e.id, e.display_name(), size, state, e.licence);
            }
            println!("\nModels are stored in {}", dir.display());
            Ok(())
        }
        Some("download") => {
            let id = arg(args, 2)?;
            let e = catalog.get(id).ok_or_else(|| format!("no model called {id}; see `quire-cli ai models`"))?;
            let settings = Settings::load();
            let d = Downloader::new(&settings.ai_local.mirror_url);
            let tty = std::io::stderr().is_terminal();
            let mut last = 0;
            d.fetch(e, &dir, &Control::new(), &mut |p| {
                if let Progress::Bytes { done, total, bytes_per_s } = p
                    && tty
                    && done.saturating_sub(last) > 1_000_000
                {
                    last = done;
                    let pct = (done * 100).checked_div(total).unwrap_or(0);
                    eprint!("\r{id}: {pct}% of {} · {}/s   ", human_size(total), human_size(bytes_per_s as u64));
                    let _ = std::io::stderr().flush();
                }
            })
            .map_err(|e| e.to_string())?;
            if tty {
                eprintln!();
            }
            println!("{} is installed and verified.", e.display_name());
            Ok(())
        }
        Some("install") => {
            let files: Vec<PathBuf> = args.iter().skip(2).map(PathBuf::from).collect();
            if files.is_empty() {
                return Err("give the files to install".into());
            }
            for r in install_from_files(&catalog, &files, &dir) {
                match r {
                    Installed1::Ok { id, file } => println!("{file}: installed for {id}"),
                    Installed1::Unknown { path } => {
                        println!("{}: doesn't match any model this version knows (checked by SHA-256)", path.display());
                    }
                    Installed1::Failed { path, error } => println!("{}: {error}", path.display()),
                }
            }
            Ok(())
        }
        Some("delete") => {
            let id = arg(args, 2)?;
            quire_ai::download::delete(&dir, id).map_err(|e| e.to_string())?;
            println!("Deleted {id}.");
            Ok(())
        }
        Some("engine") => {
            let settings = Settings::load();
            let e = quire_app::ai::engine(&settings);
            let Some(path) = e.path() else {
                return Err("the engine isn't installed: `quire-cli ai download engine`".into());
            };
            let h = e.hello(std::time::Duration::from_secs(20)).map_err(|err| err.to_string())?;
            println!(
                "{} · engine {} · protocol {} · {} · {} threads",
                path.display(),
                h.engine_version,
                h.protocol,
                h.devices.join(", "),
                h.threads
            );
            e.stop();
            Ok(())
        }
        Some(other) => Err(format!("unknown `ai` command {other}; try models, download, install, delete or engine")),
    }
}

fn run_ingest(args: &[String]) -> Result<(), String> {
    let vault = Vault::open_local(arg(args, 1)?).map_err(|e| e.to_string())?;
    let source = arg(args, 2)?;
    let yes = args.iter().any(|a| a == "--yes" || a == "-y");
    let dry = args.iter().any(|a| a == "--dry-run");
    let settings = Settings::load();
    let agent = AgentStore::open(&vault, quire_app::device::this_device(), Arc::new(SystemClock));
    let Some(model) = quire_app::ai::chat_model(&settings) else {
        return Err("ingest needs an AI model: set one up in Quire (Settings → AI)".into());
    };
    let device = quire_app::device::this_device().id;
    let actor = Actor::agent(format!("cli:ingest:{}", model.name()), format!("Ingest ({})", model.name()), Via::Cli, &device);
    let path = if source.starts_with("http://") || source.starts_with("https://") {
        if dry {
            println!("Would capture {source} into raw/ first.");
            return Ok(());
        }
        let done =
            quire_mcp::capture::capture(&vault, &agent, &Actor::import("url", &device), source, &quire_mcp::capture::Options::default())
                .map_err(|e| e.to_string())?;
        eprintln!("Captured {source} as {}", done.path);
        done.path
    } else {
        let p = NotePath::from(source.trim_start_matches("./"));
        if !vault.exists(&p) {
            return Err(format!("{p} isn't in the vault"));
        }
        p
    };
    let text = vault.read(&path).map_err(|e| e.to_string())?;
    let est = estimate(&text);
    let cloud = model.locality() == Locality::Cloud;
    println!("Ingesting {path} with {} ({}): {}.", model.name(), quire_app::ai::locality_words(model.locality()), est.describe());
    if dry {
        return Ok(());
    }
    if cloud && !yes {
        if !std::io::stdin().is_terminal() {
            return Err("this sends the source to a cloud provider; add --yes to go ahead".into());
        }
        eprint!("Send it to your provider? [y/N] ");
        let _ = std::io::stderr().flush();
        let mut line = String::new();
        let _ = std::io::stdin().read_line(&mut line);
        if !line.trim().eq_ignore_ascii_case("y") {
            return Err("cancelled".into());
        }
    }
    let now = chrono::Local::now();
    let job = Ingest {
        model: model.as_ref(),
        agent: &agent,
        actor: &actor,
        source: path,
        today: now.format("%Y-%m-%d").to_string(),
        now: now.format("%Y-%m-%d %H:%M").to_string(),
    };
    let report = ingest(&job, &CancelFlag::new(), &mut |e| {
        if let IngestEvent::Step(s) = e {
            eprintln!("· {s}");
        }
    })?;
    for p in &report.created {
        println!("created  {p}");
    }
    for p in &report.updated {
        println!("updated  {p}");
    }
    if !report.proposals.is_empty() {
        println!("Suggestions for your own notes wait for review in Quire (or `quire-cli agent proposals`).");
    }
    if let Some(run) = &report.run {
        println!(
            "Undo it all: quire-cli agent revert {} --run {run} --write",
            vault.storage().absolute(&NotePath::new("")).map_or_else(|| ".".into(), |p| p.display().to_string())
        );
    }
    Ok(())
}
