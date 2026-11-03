// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! `quire-cli mcp`: the MCP server (design §B.4, §4.6).
//!
//! * `mcp <vault> [--allow-write] [--client KEY]`: stdio, for clients that
//!   start Quire themselves (Claude Desktop, Claude Code).
//! * `mcp serve-http <vault>`: Streamable HTTP with OAuth, headless.
//! * `mcp requests | approve <id> | deny <id>`: answer an HTTP client's
//!   request for access when the app isn't open.
//! * `mcp token create | list | revoke`: personal tokens for HTTP clients.

use std::fmt::Write as _;
use std::net::IpAddr;

use quire_mcp::consent::ConsentStore;
use quire_mcp::tokens::{Kind, Scopes, TokenStore, parse_expiry};
use quire_mcp::{Server, ServerConfig};

use super::{CliCommand, arg};

pub(crate) const COMMANDS: &[CliCommand] = &[CliCommand {
    name: "mcp",
    rank: 14,
    usage: "  quire-cli mcp       <vault> [--allow-write] [--client KEY]  MCP server on stdio for AI assistants (read-only by default;
                      with --allow-write, writes follow the vault's agent policy: direct in wiki/, suggestions elsewhere)
  quire-cli mcp serve-http <vault> [--port N] [--bind ADDR] [--public-url URL]   MCP over HTTP with sign-in (127.0.0.1:7311)
  quire-cli mcp requests | approve <id> | deny <id>    answer an app's request to connect over HTTP
  quire-cli mcp token create --name N --scope notes:read[,notes:propose,notes:write,notes:ask] [--expires 30d] | list | revoke <name>",
    run,
}];

fn flag<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).map(String::as_str)
}

fn has(args: &[String], name: &str) -> bool {
    args.iter().any(|a| a == name)
}

fn now() -> i64 {
    quire_app::history_ops::now_ms()
}

fn run(args: &[String]) -> Result<(), String> {
    let first = arg(args, 1)?;
    // A subcommand, unless a folder of that name was meant.
    let sub = matches!(first, "serve-http" | "approve" | "deny" | "requests" | "token") && !std::path::Path::new(first).is_dir();
    if !sub {
        return stdio(args);
    }
    match first {
        "serve-http" => serve_http(args),
        "approve" | "deny" => {
            let id = arg(args, 2)?;
            let r = consent()?.decide(id, first == "approve", now())?;
            println!(
                "{} {} ({}) for {}.",
                if first == "approve" { "Approved" } else { "Denied" },
                r.client_name,
                r.client_host,
                r.scopes.join(", ")
            );
            Ok(())
        }
        "requests" => {
            print!("{}", requests(&consent()?, now()));
            Ok(())
        }
        _ => token(args),
    }
}

fn consent() -> Result<ConsentStore, String> {
    ConsentStore::default_store().ok_or_else(|| "no config folder on this system".to_string())
}

fn tokens() -> Result<TokenStore, String> {
    TokenStore::default_store().ok_or_else(|| "no config folder on this system".to_string())
}

fn stdio(args: &[String]) -> Result<(), String> {
    let allow_write = has(args, "--allow-write");
    let client_key = flag(args, "--client").map(str::to_string);
    // The same rule the server uses: `claude-code`, or `acp:<name>` for an
    // external agent Quire started.
    if let Some(k) = &client_key {
        quire_mcp::server::client_actor_id(k)?;
    }
    if allow_write {
        eprintln!(
            "quire mcp: since Quire 0.9, --allow-write writes per the vault's agent policy: directly in wiki/, as suggestions you review in Quire elsewhere."
        );
    }
    let cfg = ServerConfig {
        allow_write,
        client_key,
        allow_private_urls: has(args, "--allow-private-urls"),
        // WS-D: the built-in prompts and the person's AI for `ask` in model mode.
        prompts: quire_assistant::prompts::mcp_defs(),
        chat: quire_app::ai::chat_model(&quire_app::Settings::load()),
        ..ServerConfig::default()
    };
    let srv = Server::open_with(arg(args, 1)?, cfg)?;
    srv.start_pump();
    let stdin = std::io::stdin();
    srv.serve(stdin.lock(), std::io::stdout()).map_err(|e| e.to_string())
}

fn serve_http(args: &[String]) -> Result<(), String> {
    let vault = arg(args, 2)?;
    let port: u16 = flag(args, "--port").map_or(Ok(7311), |p| p.parse().map_err(|_| format!("--port takes a number, not “{p}”")))?;
    let bind: IpAddr = flag(args, "--bind")
        .map_or_else(|| Ok(IpAddr::from([127, 0, 0, 1])), |b| b.parse().map_err(|_| format!("--bind takes an IP address, not “{b}”")))?;
    let mut cfg = quire_mcp::http::HttpConfig::local(port).ok_or("no config folder on this system")?;
    cfg.bind = bind;
    cfg.public_url = flag(args, "--public-url").unwrap_or_default().to_string();
    if !bind.is_loopback() {
        eprintln!(
            "Warning: listening on {bind} lets other computers reach this vault. Every request still needs a token; use HTTPS (a reverse proxy) for anything beyond your own network."
        );
    }
    // Over HTTP, writes depend on each token's scopes.
    let srv = Server::open_with(
        vault,
        ServerConfig {
            allow_write: true,
            allow_private_urls: has(args, "--allow-private-urls"),
            prompts: quire_assistant::prompts::mcp_defs(),
            chat: quire_app::ai::chat_model(&quire_app::Settings::load()),
            ..ServerConfig::default()
        },
    )?;
    let http = quire_mcp::http::HttpServer::start(srv, cfg)?;
    let consent = consent()?;
    eprintln!("Quire MCP is listening at {}", http.urls().resource);
    eprintln!("Clients sign in with OAuth (approve them here or in the app), or use a token from `quire-cli mcp token create`.");
    let mut seen = std::collections::HashSet::new();
    loop {
        for r in consent.pending(now()) {
            if seen.insert(r.id.clone()) {
                eprintln!(
                    "\n{} (calls itself {}) asks for {}.\n  Its browser page shows the code {}: approve only if it matches.\n  Approve: quire-cli mcp approve {}\n  Deny:    quire-cli mcp deny {}",
                    r.client_host,
                    r.client_name,
                    r.scopes().describe().join("; "),
                    r.verify_code(),
                    r.id,
                    r.id
                );
            }
        }
        std::thread::sleep(std::time::Duration::from_secs(1));
    }
}

fn requests(store: &ConsentStore, now: i64) -> String {
    let list = store.pending(now);
    if list.is_empty() {
        return "No requests waiting.\n".into();
    }
    let mut s = String::new();
    for r in list {
        let code = if r.verify_code().is_empty() { String::new() } else { format!("  code {}", r.verify_code()) };
        let _ = writeln!(s, "{}  {} (calls itself {}){code}  {}", r.id, r.client_host, r.client_name, r.scopes.join(", "));
    }
    s
}

fn token(args: &[String]) -> Result<(), String> {
    let store = tokens()?;
    match arg(args, 2)? {
        "create" => {
            let name = flag(args, "--name").ok_or("token create needs --name")?;
            let scopes = Scopes::parse(flag(args, "--scope").unwrap_or("notes:read"))?;
            let expires = parse_expiry(flag(args, "--expires").unwrap_or("90d"), now())?;
            let t = store.create_personal(name, scopes, expires, now())?;
            println!("{t}");
            eprintln!(
                "This is the only time the token is shown. Give it to the app as a bearer token; revoke it with `quire-cli mcp token revoke {name}`."
            );
            Ok(())
        }
        "list" => {
            print!("{}", token_list(&store, now()));
            Ok(())
        }
        "revoke" => {
            let n = store.revoke(arg(args, 3)?)?;
            if n == 0 {
                return Err(format!("no token or app named {}", arg(args, 3)?));
            }
            println!("Revoked.");
            Ok(())
        }
        other => Err(format!("unknown token command “{other}”\n\n{}", super::usage())),
    }
}

fn token_list(store: &TokenStore, now: i64) -> String {
    let rows = store.summary(now);
    if rows.is_empty() {
        return "No tokens.\n".into();
    }
    let mut s = String::new();
    for r in rows {
        let (kind, who) = if r.kind == Kind::Personal { ("token", r.name.clone()) } else { ("app", format!("{} ({})", r.name, r.client)) };
        let used = r.last_used.map_or_else(|| "never used".into(), |t| format!("used {}", quire_app::chrono_fmt(t)));
        let exp = r.expires.map_or_else(|| "no expiry".into(), |t| format!("until {}", quire_app::chrono_fmt(t)));
        let _ = writeln!(s, "{kind:<5} {who}  {}  {used}, {exp}", r.scopes.join(","));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_and_requests_are_listed() {
        let dir = tempfile::tempdir().unwrap();
        let st = TokenStore::in_dir(dir.path());
        assert_eq!(token_list(&st, 0), "No tokens.\n");
        st.create_personal("laptop", Scopes::parse("notes:propose").unwrap(), None, 0).unwrap();
        let l = token_list(&st, 1);
        assert!(l.starts_with("token laptop  notes:read,notes:propose  never used, no expiry"), "{l}");
        let c = ConsentStore::in_dir(dir.path());
        assert_eq!(requests(&c, 0), "No requests waiting.\n");
        let id = c.create("https://a.example/c.json", "App", "a.example", "http://127.0.0.1:1/cb", &["notes:read"], "r", 0).unwrap();
        assert!(requests(&c, 1).starts_with(&format!("{id}  a.example (calls itself App)  notes:read")));
    }
}
