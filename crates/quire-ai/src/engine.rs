// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The engine process: finding the binary and saying hello (design §D.3).
//!
//! Lazy start, restarts, idle unload and the request multiplexer arrive
//! with WS-D; this is the handle the rest of the app holds.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use crate::protocol::{HelloParams, HelloResult, PROTOCOL_VERSION, Reply, Request, method};

/// Name of the engine executable.
pub const ENGINE_BIN: &str = if cfg!(windows) { "quire-ai-engine.exe" } else { "quire-ai-engine" };

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EngineError {
    /// No engine binary was found (not downloaded yet).
    NotInstalled,
    /// It didn't start, answered nonsense, or not in time.
    Failed(String),
    /// It answered with an error.
    Refused(String),
}

impl std::fmt::Display for EngineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotInstalled => f.write_str("the on-device AI engine isn't installed"),
            Self::Failed(why) => write!(f, "the on-device AI engine didn't work: {why}"),
            Self::Refused(why) => write!(f, "the on-device AI engine said: {why}"),
        }
    }
}

impl std::error::Error for EngineError {}

/// A handle to the engine (not started until used).
#[derive(Debug, Clone)]
pub struct Engine {
    path: Option<PathBuf>,
}

impl Engine {
    /// The engine at `$QUIRE_AI_ENGINE`, or next to this executable, or in
    /// `models_dir` (where downloads go).
    pub fn locate(models_dir: Option<&Path>) -> Self {
        let from_env = std::env::var_os("QUIRE_AI_ENGINE").map(PathBuf::from);
        let beside = std::env::current_exe().ok().and_then(|e| e.parent().map(|d| d.join(ENGINE_BIN)));
        let downloaded = models_dir.map(|d| d.join("engine").join(ENGINE_BIN));
        Self { path: [from_env, beside, downloaded].into_iter().flatten().find(|p| p.is_file()) }
    }

    pub fn at(path: impl Into<PathBuf>) -> Self {
        Self { path: Some(path.into()) }
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    pub const fn is_installed(&self) -> bool {
        self.path.is_some()
    }

    /// Start the engine, exchange `hello`, and shut it down: a health check
    /// for Settings. The engine must be the same version as the app.
    pub fn hello(&self, timeout: Duration) -> Result<HelloResult, EngineError> {
        let path = self.path.as_ref().ok_or(EngineError::NotInstalled)?;
        let mut child = Command::new(path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| EngineError::Failed(e.to_string()))?;
        let mut stdin = child.stdin.take().ok_or_else(|| EngineError::Failed("no stdin".into()))?;
        let stdout = child.stdout.take().ok_or_else(|| EngineError::Failed("no stdout".into()))?;
        let params = HelloParams { app_version: env!("CARGO_PKG_VERSION").into(), protocol: PROTOCOL_VERSION };
        let hello = Request { id: 1, method: method::HELLO.into(), params: serde_json::to_value(params).unwrap_or_default() };
        let shutdown = Request { id: 2, method: method::SHUTDOWN.into(), params: serde_json::json!({}) };
        let lines = [hello, shutdown].iter().map(|r| serde_json::to_string(r).unwrap_or_default()).collect::<Vec<_>>().join("\n");
        writeln!(stdin, "{lines}").map_err(|e| EngineError::Failed(e.to_string()))?;
        drop(stdin);
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut first = String::new();
            let _ = BufReader::new(stdout).read_line(&mut first);
            let _ = tx.send(first);
        });
        let line = rx.recv_timeout(timeout);
        let _ = child.kill();
        let _ = child.wait();
        let line = line.map_err(|_| EngineError::Failed("no answer in time".into()))?;
        let reply: Reply = serde_json::from_str(&line).map_err(|e| EngineError::Failed(format!("bad reply: {e}")))?;
        if let Some(e) = reply.error {
            return Err(EngineError::Refused(e.message));
        }
        serde_json::from_value(reply.result.unwrap_or_default()).map_err(|e| EngineError::Failed(format!("bad hello: {e}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_engine_is_reported_as_such() {
        let e = Engine { path: None };
        assert_eq!(e.hello(Duration::from_millis(10)), Err(EngineError::NotInstalled));
        let missing = Engine::at("/nonexistent/quire-ai-engine");
        assert!(matches!(missing.hello(Duration::from_millis(100)), Err(EngineError::Failed(_))));
    }
}
