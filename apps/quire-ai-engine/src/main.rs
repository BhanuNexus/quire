// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! `quire-ai-engine`: runs on-device models for Quire in a separate process
//! (design 0.9 §D.3). It reads JSON-lines requests on stdin and answers on
//! stdout ([`quire_ai::protocol`]). This build answers `hello` and
//! `shutdown`; the models (candle, tokenizers, OCR) join after WS-D's spike.

use std::io::{BufRead, Write};

use quire_ai::protocol::{HelloParams, HelloResult, PROTOCOL_VERSION, Reply, Request, code, method};
use serde_json::json;

/// The reply to one request line, and whether to keep reading.
fn handle(line: &str) -> (Reply, bool) {
    let req: Request = match serde_json::from_str(line) {
        Ok(r) => r,
        Err(e) => return (Reply::err(0, code::BAD_PARAMS, format!("not a request: {e}")), true),
    };
    let reply = match req.method.as_str() {
        method::HELLO => match serde_json::from_value::<HelloParams>(req.params) {
            Ok(h) if h.app_version != env!("CARGO_PKG_VERSION") || h.protocol != PROTOCOL_VERSION => Reply::err(
                req.id,
                code::VERSION_MISMATCH,
                format!(
                    "this engine is {} (protocol {PROTOCOL_VERSION}); the app is {} (protocol {})",
                    env!("CARGO_PKG_VERSION"),
                    h.app_version,
                    h.protocol
                ),
            ),
            Ok(_) => {
                let threads = std::thread::available_parallelism().map_or(1, std::num::NonZero::get);
                let hello = HelloResult {
                    engine_version: env!("CARGO_PKG_VERSION").into(),
                    protocol: PROTOCOL_VERSION,
                    devices: vec!["cpu".into()],
                    threads: u32::try_from(threads).unwrap_or(1),
                };
                Reply::ok(req.id, serde_json::to_value(hello).unwrap_or_default())
            }
            Err(e) => Reply::err(req.id, code::BAD_PARAMS, e.to_string()),
        },
        method::SHUTDOWN => return (Reply::ok(req.id, json!({})), false),
        other => Reply::err(req.id, code::UNKNOWN_METHOD, format!("{other} isn't available in this engine build")),
    };
    (reply, true)
}

fn main() {
    if std::env::args().nth(1).is_some_and(|a| a == "--version" || a == "-V") {
        println!("quire-ai-engine {}", env!("CARGO_PKG_VERSION"));
        return;
    }
    let stdin = std::io::stdin();
    let mut out = std::io::stdout().lock();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let (reply, go_on) = handle(&line);
        if writeln!(out, "{}", serde_json::to_string(&reply).unwrap_or_default()).and_then(|()| out.flush()).is_err() || !go_on {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn answers_hello_and_refuses_the_rest() {
        let hello = format!(r#"{{"id":1,"method":"hello","params":{{"app_version":"{}","protocol":1}}}}"#, env!("CARGO_PKG_VERSION"));
        let (r, go_on) = handle(&hello);
        assert!(go_on);
        let h: HelloResult = serde_json::from_value(r.result.unwrap()).unwrap();
        assert_eq!(h.protocol, PROTOCOL_VERSION);
        assert_eq!(h.devices, ["cpu"]);
        let (r, _) = handle(r#"{"id":2,"method":"hello","params":{"app_version":"0.0.1","protocol":1}}"#);
        assert_eq!(r.error.unwrap().code, code::VERSION_MISMATCH);
        let (r, _) = handle(r#"{"id":3,"method":"embed","params":{}}"#);
        assert_eq!(r.error.unwrap().code, code::UNKNOWN_METHOD);
        let (r, go_on) = handle(r#"{"id":4,"method":"shutdown","params":{}}"#);
        assert!(r.error.is_none() && !go_on);
        assert!(handle("garbage").0.error.is_some());
    }
}
