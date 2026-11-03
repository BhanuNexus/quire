// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Agent panel conversations (design §D.5).
//!
//! They are kept in `.quire/agent/conversations/<id>.json` (format 1,
//! docs/formats/agent-state.md) on this device, deleted 30 days after their
//! last message, and savable as a note.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use quire_app::intel::chat::ChatMessage;
use serde::{Deserialize, Serialize};

pub const FORMAT: u32 = 1;
/// Conversations untouched this long are deleted.
pub const KEEP_MS: i64 = 30 * 24 * 60 * 60 * 1000;

/// What the panel draws, in order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Item {
    User {
        text: String,
    },
    Assistant {
        text: String,
    },
    /// A tool call: its summary, whether it failed, and what the model saw.
    Tool {
        id: String,
        summary: String,
        #[serde(default)]
        detail: String,
        #[serde(default)]
        is_error: bool,
        #[serde(default)]
        running: bool,
    },
    /// A change that became a suggestion.
    Proposal {
        proposal: String,
        path: String,
    },
    /// A change written directly (agent zone).
    Applied {
        path: String,
    },
    /// "Stopped", errors, permission answers.
    Notice {
        text: String,
        #[serde(default)]
        error: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Conversation {
    pub format: u32,
    pub id: String,
    pub created: i64,
    pub updated: i64,
    /// The first message, shortened.
    pub title: String,
    /// The model that answered ("claude-sonnet-5-5", "Qwen3 1.7B", "External: Codex").
    #[serde(default)]
    pub model: String,
    /// The turns the model sees.
    #[serde(default)]
    pub messages: Vec<ChatMessage>,
    /// What the panel shows.
    #[serde(default)]
    pub items: Vec<Item>,
    #[serde(flatten)]
    pub other: serde_json::Map<String, serde_json::Value>,
}

impl Conversation {
    pub fn new(now_ms: i64) -> Self {
        Self {
            format: FORMAT,
            id: quire_agent::ids::new_id("c_", now_ms),
            created: now_ms,
            updated: now_ms,
            title: String::new(),
            model: String::new(),
            messages: Vec::new(),
            items: Vec::new(),
            other: serde_json::Map::new(),
        }
    }

    /// Append to the last assistant text, or start one.
    pub fn push_text(&mut self, t: &str) {
        if let Some(Item::Assistant { text }) = self.items.last_mut() {
            text.push_str(t);
        } else {
            self.items.push(Item::Assistant { text: t.to_string() });
        }
    }

    pub fn set_title_from(&mut self, first: &str) {
        if self.title.is_empty() {
            let one = first.lines().next().unwrap_or_default().trim();
            let mut t: String = one.chars().take(60).collect();
            if one.chars().count() > 60 {
                t.push('…');
            }
            self.title = if t.is_empty() { "Conversation".into() } else { t };
        }
    }

    /// The transcript as a note: front matter, an `ai` callout, and the turns.
    pub fn to_markdown(&self, date: &str) -> String {
        let mut s = format!(
            "---\ntype: conversation\ncreated: {date}\nmodel: {}\nai_written: true\n---\n# {}\n\n> [!ai] Conversation with {} in Quire's agent panel, {date}\n\n",
            yaml_scalar(&self.model),
            self.title,
            if self.model.is_empty() { "an agent" } else { &self.model }
        );
        for it in &self.items {
            match it {
                Item::User { text } => {
                    let _ = write!(s, "**You:** {}\n\n", text.trim());
                }
                Item::Assistant { text } if !text.trim().is_empty() => {
                    let _ = write!(s, "**Agent:** {}\n\n", text.trim());
                }
                Item::Tool { summary, is_error, .. } => {
                    let _ = write!(s, "- {}{summary}\n\n", if *is_error { "⚠ " } else { "" });
                }
                Item::Proposal { path, .. } => {
                    let _ = write!(s, "- Suggested a change to [[{}]]\n\n", path.trim_end_matches(".md"));
                }
                Item::Applied { path } => {
                    let _ = write!(s, "- Changed [[{}]]\n\n", path.trim_end_matches(".md"));
                }
                Item::Notice { text, .. } => {
                    let _ = write!(s, "*{}*\n\n", text.trim());
                }
                Item::Assistant { .. } => {}
            }
        }
        s.trim_end().to_string() + "\n"
    }
}

fn yaml_scalar(s: &str) -> String {
    if s.is_empty() || s.contains([':', '#', '"', '\'']) { format!("\"{}\"", s.replace('"', "'")) } else { s.to_string() }
}

/// The folder for a vault on disk.
pub fn dir(root: &Path) -> PathBuf {
    root.join(".quire").join("agent").join("conversations")
}

/// Save atomically. A file from a newer Quire is left alone.
pub fn save(root: &Path, c: &Conversation) -> std::io::Result<()> {
    let d = dir(root);
    std::fs::create_dir_all(&d)?;
    let path = d.join(format!("{}.json", c.id));
    if let Ok(t) = std::fs::read_to_string(&path)
        && serde_json::from_str::<serde_json::Value>(&t).ok().and_then(|v| v["format"].as_u64()).is_some_and(|f| f > u64::from(FORMAT))
    {
        return Ok(());
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(c).map_err(std::io::Error::other)?)?;
    std::fs::rename(tmp, path)
}

/// Every conversation kept, newest first; ones older than 30 days are deleted.
pub fn list(root: &Path, now_ms: i64) -> Vec<Conversation> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(dir(root)) else { return out };
    for e in rd.flatten() {
        let p = e.path();
        if p.extension().is_none_or(|x| x != "json") {
            continue;
        }
        let Some(c) = std::fs::read_to_string(&p).ok().and_then(|t| serde_json::from_str::<Conversation>(&t).ok()) else { continue };
        if now_ms - c.updated > KEEP_MS {
            let _ = std::fs::remove_file(&p);
            continue;
        }
        out.push(c);
    }
    out.sort_by_key(|c| std::cmp::Reverse(c.updated));
    out
}

pub fn delete(root: &Path, id: &str) {
    if id.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        let _ = std::fs::remove_file(dir(root).join(format!("{id}.json")));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conversations_persist_expire_and_become_notes() {
        let tmp = tempfile::tempdir().unwrap();
        let mut c = Conversation::new(1_000);
        c.set_title_from("What do my notes say about ropes and why are they good for editors?");
        c.model = "claude-sonnet-5-5".into();
        c.items.push(Item::User { text: "What about ropes?".into() });
        c.push_text("Ropes share ");
        c.push_text("subtrees.");
        c.items.push(Item::Tool {
            id: "t".into(),
            summary: "Searched notes for *ropes* · 2 results".into(),
            detail: String::new(),
            is_error: false,
            running: false,
        });
        c.items.push(Item::Proposal { proposal: "p_1".into(), path: "Rope Notes.md".into() });
        save(tmp.path(), &c).unwrap();
        let listed = list(tmp.path(), 2_000);
        assert_eq!(listed, vec![c.clone()]);
        assert!(listed[0].title.ends_with('…') && listed[0].title.chars().count() == 61);
        let md = c.to_markdown("2026-11-03");
        assert!(
            md.contains("**Agent:** Ropes share subtrees.")
                && md.contains("Suggested a change to [[Rope Notes]]")
                && md.contains("> [!ai]"),
            "{md}"
        );
        assert!(list(tmp.path(), 1_000 + KEEP_MS + 1).is_empty(), "30 days after the last message it is gone");
        assert!(!dir(tmp.path()).join(format!("{}.json", c.id)).exists());
    }
}
