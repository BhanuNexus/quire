// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! On-device AI and the wiki loop from the command line (`docs/design/0.9.md` §D.2, §D.7).
//!
//! * `quire-cli ai models | download <id> | install <file>… | delete <id> | engine`
//! * `quire-cli ingest <vault> <source|url> [--yes] [--dry-run]`

use quire_base::dates;
use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::sync::Arc;

use quire_ai::catalog::{Catalog, human_size};
use quire_ai::download::{Control, Downloader, Installed1, Progress, install_from_files_if};
use quire_app::agent::{Actor, AgentStore, SystemClock, Via};
use quire_app::intel::chat::{CancelFlag, Locality};
use quire_app::vault::NotePath;
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

/// While the pack is off (`[ai_local] pack`, `docs/design/0.14-local-ai.md` §3.8).
const PACK_OFF: &str = "on-device AI is off: `quire-cli ai download <id>` turns it on for that model, \
     or use Settings → AI → On this computer. Nothing is downloaded until then.";

/// Voice models stay behind the experimental switch (0.14).
fn voice_needs_experimental() -> String {
    format!(
        "voice transcription on this computer is experimental: tick Settings → AI → On this computer → Advanced → \
         \"Experimental: voice transcription on this computer, and older models\" (or set {}=1) first",
        quire_app::settings::EXPERIMENTAL_ENV
    )
}

/// `download` and `install` are explicit acts: they turn the pack on for a
/// model of the pack's kinds (writing, OCR, embedder) before anything
/// downloads; voice models still need the experimental switch.
fn allow(entry: &quire_ai::catalog::CatalogEntry) -> Result<(), String> {
    let mut settings = super::load_settings();
    if allow_in(&mut settings.ai_local, entry)? {
        settings.save().map_err(|e| format!("couldn't save the settings, so nothing was downloaded: {e}"))?;
    }
    Ok(())
}

/// [`allow`] on the settings alone: turns the pack on with only this model's
/// capability (nothing ticked under 0.9–0.13 wakes), or refuses. `true` when
/// the settings changed.
fn allow_in(a: &mut quire_app::settings::AiLocalSettings, entry: &quire_ai::catalog::CatalogEntry) -> Result<bool, String> {
    use quire_ai::catalog::Purpose;
    use quire_app::settings::PackPick;
    if entry.purpose == Purpose::Transcribe {
        return if a.on_device() { Ok(false) } else { Err(voice_needs_experimental()) };
    }
    if !entry.is_pinned() {
        return Err(format!("{} isn't available in this build of Quire", entry.display_name()));
    }
    let pick = match entry.purpose {
        Purpose::Llm => PackPick::Llm(&entry.id),
        Purpose::Ocr => PackPick::Ocr,
        Purpose::Embed => PackPick::Embed(&entry.id),
        // The engine itself turns nothing on (voice was handled above).
        Purpose::Engine | Purpose::Transcribe => return Ok(false),
    };
    let before = a.clone();
    a.turn_pack_on(&[pick]);
    Ok(*a != before)
}

fn run_ai(args: &[String]) -> Result<(), String> {
    let catalog = Catalog::builtin();
    let dir = models_dir()?;
    let on = super::load_settings().ai_local.pack_on();
    let sub = args.get(1).map(String::as_str);
    match sub {
        None | Some("models") => {
            if !on {
                println!("Note: {PACK_OFF}\n");
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
            // The same free-space rule as the dialog (files + 10% + 500 MB).
            if e.is_pinned() {
                quire_app::ai::pack::check_space(e.size(), quire_app::ai::pack::free_disk_bytes(&dir))?;
            }
            allow(e)?;
            let settings = super::load_settings();
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
            // Each matched model is allowed (and the pack turned on for it)
            // before its file is copied: a refused one leaves nothing behind.
            for r in install_from_files_if(&catalog, &files, &dir, &mut |e| allow(e)) {
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
            if !on {
                return Err(PACK_OFF.into());
            }
            let settings = super::load_settings();
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
    let vault = super::open_vault(arg(args, 1)?)?;
    let source = arg(args, 2)?;
    let yes = super::has_flag(args, "--yes") || super::has_flag(args, "-y");
    let dry = super::has_flag(args, "--dry-run");
    let settings = super::load_settings();
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
    let now = dates::now_local();
    let job = Ingest {
        model: model.as_ref(),
        agent: &agent,
        actor: &actor,
        source: path,
        today: dates::fmt_zoned(&now, "%Y-%m-%d"),
        now: dates::fmt_zoned(&now, "%Y-%m-%d %H:%M"),
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

#[cfg(test)]
mod tests {
    use quire_ai::catalog::Catalog;

    fn entry(id: &str, purpose: &str) -> quire_ai::catalog::CatalogEntry {
        let text = format!(
            "format = 1\n[[model]]\nid = \"{id}\"\npurpose = \"{purpose}\"\nlabel = \"T\"\nlicence = \"MIT\"\nram_mb = 1\n[[model.files]]\nname = \"m\"\nurl = \"https://example.invalid/m\"\nsha256 = \"{}\"\nsize = 1\n",
            "a".repeat(64)
        );
        Catalog::parse(&text).unwrap().models.remove(0)
    }

    /// From a 0.13 settings file (llm, embeddings and an old model ticked,
    /// experimental off): downloading one model turns on that model alone, and
    /// `ai download <writing model>` chooses it.
    #[test]
    fn a_download_turns_on_only_its_own_capability() {
        if std::env::var_os(quire_app::settings::EXPERIMENTAL_ENV).is_some() {
            return;
        }
        let old = || quire_app::settings::AiLocalSettings {
            experimental: false,
            engine: true,
            llm: true,
            embeddings: true,
            llm_model: "qwen3-1.7b-q4km".into(),
            ..quire_app::settings::AiLocalSettings::default()
        };
        let mut a = old();
        assert_eq!(super::allow_in(&mut a, &entry("ocrs", "ocr")), Ok(true));
        assert!(a.pack && a.ocr_on() && !a.llm_on() && !a.embeddings_on() && a.llm_model.is_empty());
        let mut a = old();
        assert_eq!(super::allow_in(&mut a, &entry("qwen3.5-4b-q4km", "llm")), Ok(true));
        assert!(a.llm_on() && a.llm_model == "qwen3.5-4b-q4km" && !a.embeddings_on() && !a.ocr_on());
        // Unpinned or voice: refused, nothing changes.
        let mut a = old();
        let mut unpinned = entry("x", "llm");
        unpinned.files[0].sha256.clear();
        assert!(super::allow_in(&mut a, &unpinned).is_err());
        assert!(super::allow_in(&mut a, &entry("whisper-base", "transcribe")).unwrap_err().contains("experimental"));
        assert_eq!(a, old());
    }
}
