// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! `quire-sync-server`: run your own end-to-end encrypted sync for Quire.
//!
//! Configuration comes from the environment:
//!
//! | Variable | Default | Meaning |
//! |---|---|---|
//! | `QUIRE_SYNC_TOKEN` | (required) | Shared secret devices send as a bearer token. Use 32+ random characters |
//! | `QUIRE_SYNC_TOKEN_FILE` | | Read the token from this file instead |
//! | `QUIRE_SYNC_ADDR` | `127.0.0.1:8787` | Address to listen on. Put it behind HTTPS (Caddy, nginx) |
//! | `QUIRE_SYNC_DB` | `quire-sync.sqlite` | Database file |

use std::process::ExitCode;
use std::sync::Arc;

use quire_sync_server::{AppState, Store, app};

fn token() -> Result<String, String> {
    let t = match (std::env::var("QUIRE_SYNC_TOKEN"), std::env::var("QUIRE_SYNC_TOKEN_FILE")) {
        (Ok(t), _) => t,
        (Err(_), Ok(path)) => std::fs::read_to_string(&path).map_err(|e| format!("can't read {path}: {e}"))?,
        _ => return Err("set QUIRE_SYNC_TOKEN (or QUIRE_SYNC_TOKEN_FILE) to a long random secret".into()),
    };
    let t = t.trim().to_string();
    if t.len() < 24 {
        return Err("the token is too short: use at least 24 random characters".into());
    }
    Ok(t)
}

async fn shutdown() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let term = async {
        if let Ok(mut s) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            s.recv().await;
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! {
        () = ctrl_c => {},
        () = term => {},
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    if std::env::args().nth(1).is_some_and(|a| a == "--version" || a == "-V") {
        println!("quire-sync-server {}", env!("CARGO_PKG_VERSION"));
        return ExitCode::SUCCESS;
    }
    let token = match token() {
        Ok(t) => t,
        Err(e) => {
            eprintln!("quire-sync-server: {e}");
            return ExitCode::FAILURE;
        }
    };
    let addr = std::env::var("QUIRE_SYNC_ADDR").unwrap_or_else(|_| "127.0.0.1:8787".into());
    let db_path = std::env::var("QUIRE_SYNC_DB").unwrap_or_else(|_| "quire-sync.sqlite".into());
    let store = match Store::open(std::path::Path::new(&db_path)) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("quire-sync-server: can't open {db_path}: {e}");
            return ExitCode::FAILURE;
        }
    };
    let listener = match tokio::net::TcpListener::bind(&addr).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("quire-sync-server: can't listen on {addr}: {e}");
            return ExitCode::FAILURE;
        }
    };
    eprintln!("quire-sync-server {} listening on {addr}, database {db_path}", env!("CARGO_PKG_VERSION"));
    let state = Arc::new(AppState::new(store, &token));
    if let Err(e) = axum::serve(listener, app(state)).with_graceful_shutdown(shutdown()).await {
        eprintln!("quire-sync-server: {e}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}
