// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Quire over the Model Context Protocol (design 0.9 §B, §4).
//!
//! * [`tools`] — the note tools: one registry shared by MCP, the agent panel and ACP
//! * [`protocol`] — protocol versions, eras (2026-07-28 and older), result shapes
//! * [`server`] — the transport-agnostic server; [`stdio`] and `http` carry it
//! * [`resources`], [`subs`] — resources, prompts and change notifications
//! * [`served`] — the served-version cache behind optimistic concurrency
//! * [`mrtr`] — approval in the agent's client
//! * [`tokens`] — personal and OAuth tokens for HTTP
//! * [`capture`] — web pages into `raw/`
//!
//! Clients of 2024-11-05 to 2025-11-25 get what Quire 0.8 sent, plus what
//! their version defines. A 2026-07-28 client names its version in every
//! request's `_meta` and can start with `server/discover`.

#![cfg_attr(test, allow(clippy::disallowed_methods, reason = "tests build fixtures with std directly"))]

/// A JSON value written as JSON, for the tool and resource schemas: kept in
/// the binary as its text and parsed when the tool list is made (0.13.1),
/// instead of the code `json!` expands each one to (about 0.2 MB a binary).
/// Literals only: strings, numbers, `true`, `false`, `null`; a trailing comma
/// is allowed, as in `json!`. The tests build every schema.
macro_rules! schema {
    ($($t:tt)+) => {
        $crate::schema_text(stringify!($($t)+))
    };
}

/// Parse [`schema!`]'s text (trailing commas dropped).
pub(crate) fn schema_text(text: &str) -> serde_json::Value {
    let mut clean = String::with_capacity(text.len());
    let (mut in_str, mut escaped) = (false, false);
    for (i, c) in text.char_indices() {
        if in_str {
            in_str = escaped || c != '"';
            escaped = !escaped && c == '\\';
        } else if c == '"' {
            in_str = true;
        } else if c == ',' && text[i + 1..].trim_start().starts_with(['}', ']']) {
            continue;
        }
        clean.push(c);
    }
    serde_json::from_str(&clean).expect("a schema! is JSON")
}

pub mod bounds;
pub mod capture;
pub mod consent;
pub mod edit;
pub mod eval;
#[cfg(feature = "http")]
pub mod http;
pub mod instructions;
pub mod mrtr;
pub mod protocol;
pub mod resources;
pub mod served;
pub mod server;
pub mod stdio;
pub mod subs;
pub mod tokens;
pub mod tools;

/// An existing folder, or why not: the entry points (`quire-cli`, `quire-cli
/// mcp`) open a vault that is there and never make one from a mistyped path.
///
/// # Errors
/// `No folder at <path>` with what to do next, when `root` isn't a folder.
pub fn require_folder(root: &std::path::Path) -> Result<(), String> {
    let shown = root.display();
    match std::fs::metadata(root) {
        Ok(m) if m.is_dir() => Ok(()),
        Ok(_) => Err(format!("{shown} is a file, not a folder. Give the folder that holds your notes.")),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if std::fs::symlink_metadata(root).is_ok() {
                return Err(format!(
                    "No folder at {shown}: it is a link to a place that isn't there ({e}). Check where the link points, and that the drive or share is connected."
                ));
            }
            // `mkdir` is only the fix when the folder above it is there: on an
            // unmounted drive it would make an empty vault on the wrong disk.
            let parent_there = root.parent().is_none_or(|p| p.as_os_str().is_empty() || p.is_dir());
            if parent_there {
                Err(format!(
                    "No folder at {shown}. Check the path. To start a new vault, create the folder first (mkdir \"{shown}\"), then run this command again."
                ))
            } else {
                Err(format!(
                    "No folder at {shown}, and the folder it would go in isn't there either. Check the path, and that the drive or share is connected; don't make the folder until it is."
                ))
            }
        }
        Err(e) => Err(format!("No folder at {shown}: {e}. Check the path, and that the drive or share is connected.")),
    }
}

pub use server::{Conn, Principal, Reply, Server, ServerConfig};
pub use tools::{Tool, ToolCx, ToolOutput, ToolRegistry, ToolSpec};

#[cfg(test)]
mod schema_tests {
    use serde_json::json;

    /// `schema!` gives what `json!` gives for the same literal (0.13.1).
    #[test]
    fn schema_matches_json() {
        assert_eq!(
            schema!({"type": "object", "properties": {"q": {"type": "string", "description": "A \"phrase\", a , ] } and \\ too.\n"},
                "n": {"type": "integer", "minimum": 1, "maximum": 50, "default": 0.5}, "e": {"enum": ["a", "b",], "x": [true, false, null]},},
                "required": ["q"],}),
            json!({"type": "object", "properties": {"q": {"type": "string", "description": "A \"phrase\", a , ] } and \\ too.\n"},
                "n": {"type": "integer", "minimum": 1, "maximum": 50, "default": 0.5}, "e": {"enum": ["a", "b",], "x": [true, false, null]},},
                "required": ["q"],})
        );
    }
}

#[cfg(test)]
mod require_folder_tests {
    use super::require_folder;

    #[test]
    fn mkdir_is_suggested_only_when_the_parent_folder_is_there() {
        let dir = std::env::temp_dir().join(format!("quire-require-folder-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert!(require_folder(&dir).is_ok());
        let typo = require_folder(&dir.join("Nots")).unwrap_err();
        assert!(typo.starts_with("No folder at ") && typo.contains("mkdir"), "{typo}");
        let unmounted = require_folder(&dir.join("media").join("Notes")).unwrap_err();
        assert!(unmounted.starts_with("No folder at ") && !unmounted.contains("mkdir \""), "{unmounted}");
        assert!(unmounted.contains("connected"), "{unmounted}");
        std::fs::write(dir.join("f"), "x").unwrap();
        assert!(require_folder(&dir.join("f")).unwrap_err().contains("is a file"));
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(dir.join("gone"), dir.join("dangling")).unwrap();
            let e = require_folder(&dir.join("dangling")).unwrap_err();
            assert!(e.contains("link") && !e.contains("mkdir \""), "{e}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
