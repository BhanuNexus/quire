// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! MCP over stdio (design §B.1): newline-delimited JSON-RPC.
//!
//! The calling thread reads; two workers answer requests, so a slow `ask`
//! doesn't hold up `notifications/cancelled`; one writer owns the output,
//! and subscriptions push through it. Answers may come out of order (each
//! carries its request's id). A cancelled request gets no answer.

use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, PoisonError};

use quire_app::intel::chat::CancelFlag;
use serde_json::{Value, json};

use crate::protocol::code;
use crate::server::{Conn, Reply, Server};
use crate::subs::Sink;

const WORKERS: usize = 2;

struct Job {
    msg: Value,
    id: Value,
    conn: Conn,
    cancel: CancelFlag,
}

/// Serve until `input` closes. Subscriptions end gracefully at the end.
pub fn serve(server: &Server, input: impl BufRead, output: impl Write + Send) -> std::io::Result<()> {
    let (out_tx, out_rx) = mpsc::channel::<Value>();
    let (job_tx, job_rx) = mpsc::channel::<Job>();
    let job_rx = Arc::new(Mutex::new(job_rx));
    let in_flight: Arc<Mutex<HashMap<String, CancelFlag>>> = Arc::default();
    let conn_id = server.new_conn();
    let mut conn = Conn::stdio(conn_id);
    let mut write_err = None;
    std::thread::scope(|scope| {
        let writer = scope.spawn(move || -> std::io::Result<()> {
            let mut output = output;
            for v in out_rx {
                writeln!(output, "{v}")?;
                output.flush()?;
            }
            Ok(())
        });
        for _ in 0..WORKERS {
            let rx = Arc::clone(&job_rx);
            let tx = out_tx.clone();
            let flights = Arc::clone(&in_flight);
            let server = server.clone();
            scope.spawn(move || {
                loop {
                    let job = { rx.lock().unwrap_or_else(PoisonError::into_inner).recv() };
                    let Ok(job) = job else { break };
                    let reply = server.handle(&job.msg, &job.conn, &job.cancel);
                    flights.lock().unwrap_or_else(PoisonError::into_inner).remove(&job.id.to_string());
                    if job.cancel.is_cancelled() {
                        continue;
                    }
                    if let Reply::Response(v) = reply {
                        let _ = tx.send(v);
                    }
                }
            });
        }
        let sink: Sink = {
            let tx = Mutex::new(out_tx.clone());
            Arc::new(move |v| {
                let _ = tx.lock().unwrap_or_else(PoisonError::into_inner).send(v);
            })
        };
        for line in input.lines() {
            let Ok(line) = line else { break };
            if line.trim().is_empty() {
                continue;
            }
            let msg = match serde_json::from_str::<Value>(&line) {
                Ok(m) => m,
                Err(e) => {
                    let _ = out_tx
                        .send(json!({ "jsonrpc": "2.0", "id": null, "error": { "code": code::PARSE_ERROR, "message": e.to_string() } }));
                    continue;
                }
            };
            let method = msg.get("method").and_then(Value::as_str).unwrap_or_default();
            if method == "notifications/cancelled" {
                let rid = msg.pointer("/params/requestId").cloned().unwrap_or(Value::Null);
                if let Some(flag) = in_flight.lock().unwrap_or_else(PoisonError::into_inner).get(&rid.to_string()) {
                    flag.cancel();
                }
                server.cancel_listen(conn_id, &rid);
                continue;
            }
            let id = msg.get("id").cloned();
            // The handshake and subscriptions change this connection's state, so
            // they're handled here, in order.
            if method == "initialize"
                || method == "subscriptions/listen"
                || (method.starts_with("resources/") && method.ends_with("subscribe"))
            {
                match server.handle(&msg, &conn, &CancelFlag::new()) {
                    Reply::Initialized { response, version, client, caps } => {
                        conn.legacy = Some(version);
                        conn.client = client;
                        conn.client_caps = caps;
                        let _ = out_tx.send(response);
                    }
                    Reply::Listen { id, filter } => {
                        server.listen(conn_id, id, &filter, Arc::clone(&sink));
                    }
                    Reply::Subscribe { response, uri, subscribe } => {
                        server.subscribe_legacy(conn_id, &uri, subscribe, Arc::clone(&sink));
                        let _ = out_tx.send(response);
                    }
                    Reply::Response(v) => {
                        let _ = out_tx.send(v);
                    }
                    Reply::None => {}
                }
                continue;
            }
            let Some(id) = id else { continue };
            let cancel = CancelFlag::new();
            in_flight.lock().unwrap_or_else(PoisonError::into_inner).insert(id.to_string(), cancel.clone());
            let _ = job_tx.send(Job { msg, id, conn: conn.clone(), cancel });
        }
        drop(job_tx);
        // Workers finish what they have; then subscriptions end with their results.
        drop(sink);
        // The scope joins workers before the writer is dropped below; close
        // subscriptions now so their results go out before the writer stops.
        while Arc::strong_count(&job_rx) > 1 {
            std::thread::yield_now();
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        server.close_conn(conn_id);
        drop(out_tx);
        if let Ok(Err(e)) = writer.join() {
            write_err = Some(e);
        }
    });
    write_err.map_or(Ok(()), Err)
}
