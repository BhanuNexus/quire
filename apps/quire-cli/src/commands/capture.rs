// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! `quire-cli capture`: add a line to today's note, or with `--url`
//! capture a web page into `raw/` (design §B.3).

use quire_agent::{Actor, AgentStore, SubmitOutcome, SystemClock};
use quire_app::vault::Vault;

use super::CliCommand;

pub(crate) const COMMANDS: &[CliCommand] = &[CliCommand {
    name: "capture",
    rank: 10,
    usage: "  quire-cli capture   <vault> <text…>         add a timestamped line to today's daily note (\\\"-\\\" reads stdin)
  quire-cli capture   <vault> --url URL [--folder raw/] [--title T]   save a web page's article (and images) as a note",
    run: capture,
}];

fn flag<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).map(String::as_str)
}

fn capture(args: &[String]) -> Result<(), String> {
    let arg = |i: usize| super::arg(args, i);
    let v = quire_app::secure_state::open_vault(std::path::Path::new(arg(1)?))?;
    if let Some(url) = flag(args, "--url") {
        return capture_url(&v, url, args);
    }
    let mut text = args.get(2..).map(|a| a.join(" ")).unwrap_or_default();
    if text == "-" {
        text.clear();
        std::io::Read::read_to_string(&mut std::io::stdin(), &mut text).map_err(|e| e.to_string())?;
    }
    let p = quire_app::capture::capture_now(&v, &text)?;
    println!("added to {p}");
    Ok(())
}

fn capture_url(v: &Vault, url: &str, args: &[String]) -> Result<(), String> {
    let store = AgentStore::open(v, quire_app::device::this_device(), std::sync::Arc::new(SystemClock));
    let actor = Actor::import("url", &store.device().id);
    let opts = quire_mcp::capture::Options {
        folder: flag(args, "--folder").unwrap_or("raw/").to_string(),
        title: flag(args, "--title").map(str::to_string),
        allow_private: args.iter().any(|a| a == "--allow-private-urls"),
    };
    let done = quire_mcp::capture::capture(v, &store, &actor, url, &opts).map_err(|e| match e {
        quire_mcp::capture::CaptureError::Web(quire_import::web::WebError::Blocked(_)) => {
            format!("{e} (add --allow-private-urls to capture it anyway)")
        }
        e => e.to_string(),
    })?;
    for w in &done.warnings {
        eprintln!("{w}");
    }
    match done.outcome {
        SubmitOutcome::Applied { .. } => println!("saved {} ({} image{})", done.path, done.images, if done.images == 1 { "" } else { "s" }),
        SubmitOutcome::Proposed { id, .. } => println!("suggested {} for review in Quire ({id})", done.path),
    }
    Ok(())
}
