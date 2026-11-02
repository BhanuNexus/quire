// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Capture a web page into the vault (design §B.3): the MCP tool
//! `capture_url`, `quire-cli capture --url` and the app's "Capture web
//! page…" all come here.
//!
//! The page is fetched and converted by `quire_import::web`. Its images are
//! saved as attachments (new files only, reused when the same bytes are
//! already there), and the note goes through `AgentStore::submit` as the
//! caller (`import:url` for the CLI and the app, the agent for MCP), so
//! policy decides whether it is written or proposed. It lands in `raw/` by
//! default, with front matter that marks it untrusted.

use std::collections::HashMap;
use std::fmt::Write as _;

use quire_agent::{Actor, AgentStore, Change, ChangeSet, LedgerEntry, Mode, Op, OpKind, SourceRef, SubmitOutcome, WriteError};
use quire_app::markdown::frontmatter::yaml_scalar;
use quire_app::vault::{NotePath, Vault, sanitize_title};
use quire_import::web::{self, FetchOptions, WebError};
use serde_json::json;

use crate::tools::ToolOutput;
use crate::tools::common::write_error;

/// Where and how to capture.
#[derive(Debug, Clone)]
pub struct Options {
    /// Folder in the vault (`raw/`).
    pub folder: String,
    /// Use this title instead of the page's.
    pub title: Option<String>,
    /// Allow private and loopback addresses (`allow_private_urls`).
    pub allow_private: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self { folder: "raw/".into(), title: None, allow_private: false }
    }
}

/// A finished capture.
#[derive(Debug, Clone)]
pub struct Done {
    pub path: NotePath,
    pub outcome: SubmitOutcome,
    pub images: usize,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone)]
pub enum CaptureError {
    Web(WebError),
    Write(WriteError),
}

impl std::fmt::Display for CaptureError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Web(e) => e.fmt(f),
            Self::Write(e) => e.fmt(f),
        }
    }
}

impl CaptureError {
    pub fn to_output(&self) -> ToolOutput {
        match self {
            Self::Web(e) => ToolOutput::Err {
                code: e.code(),
                message: format!("{e}."),
                structured: json!({"error": e.code(), "message": e.to_string()}),
            },
            Self::Write(e) => write_error(e),
        }
    }
}

/// The note's text: front matter, title and article.
pub fn note_text(c: &web::Captured, title: &str, markdown: &str, captured: &str) -> String {
    let a = &c.article;
    let mut fm = format!(
        "---\nsource: {}\ncaptured: {captured}\nsite: {}\n",
        yaml_scalar(&c.url.as_string()),
        yaml_scalar(a.site_name.as_deref().unwrap_or(&c.url.site()))
    );
    if let Some(b) = &a.byline {
        let _ = writeln!(fm, "author: {}", yaml_scalar(b));
    }
    if let Some(p) = &a.published {
        let _ = writeln!(fm, "published: {}", yaml_scalar(p));
    }
    fm.push_str("trust: untrusted\n---\n\n");
    let body = markdown.trim_start();
    // The article often starts with its own title.
    if body.lines().next().is_some_and(|l| l.trim_start_matches('#').trim() == title.trim() && l.starts_with('#')) {
        let rest = body.split_once('\n').map_or("", |(_, r)| r).trim_start();
        return format!("{fm}# {title}\n\n{rest}");
    }
    format!("{fm}# {title}\n\n{body}")
}

/// Capture `url` into `vault` as `actor`.
pub fn capture(vault: &Vault, agent: &AgentStore, actor: &Actor, url: &str, opts: &Options) -> Result<Done, CaptureError> {
    let fetch = FetchOptions { allow_private: opts.allow_private, ..FetchOptions::default() };
    let c = web::capture(url, &fetch).map_err(CaptureError::Web)?;
    let title = opts.title.clone().filter(|t| !t.trim().is_empty()).unwrap_or_else(|| c.article.title.clone());
    let folder = opts.folder.trim().trim_matches('/').to_string();
    let site = sanitize_title(&c.url.site());
    let mut stem = sanitize_title(&format!("{site} - {title}"));
    if let Some((i, _)) = stem.char_indices().nth(120) {
        stem.truncate(i);
    }
    let path =
        vault.unique_path(&folder, stem.trim()).map_err(|e| CaptureError::Write(WriteError::InvalidPath { reason: e.to_string() }))?;
    let mut warnings = c.warnings.clone();
    // Images: binary attachments can't be proposals, so they are added as new
    // files when the caller may see the attachments folder at all.
    let att_probe = NotePath::join(&vault.config.attachments_folder, "probe.png");
    let images_ok = agent.effective_mode(actor, &att_probe, OpKind::Create) != Mode::Deny;
    let mut names: HashMap<String, String> = HashMap::new();
    let mut by_hash: HashMap<String, String> = HashMap::new();
    let now = agent.now_ms();
    if images_ok {
        for img in &c.images {
            if let Some(n) = by_hash.get(&img.sha256) {
                names.insert(img.src.clone(), n.clone());
                continue;
            }
            let file = format!("{}.{}", img.name, img.ext);
            let existing = NotePath::join(&vault.config.attachments_folder, &file);
            let saved = if vault.storage().read_bytes(&existing).is_ok_and(|b| b == img.bytes) {
                Ok(existing)
            } else {
                vault.save_attachment(&file, &img.bytes)
            };
            match saved {
                Ok(p) => {
                    let mut e = LedgerEntry::new(actor.clone(), Op::Create, now).path(&p);
                    e.bytes = Some(img.bytes.len() as u64);
                    e.other.insert("source".into(), json!(img.src));
                    agent.record(&e);
                    let name = p.file_name().to_string();
                    by_hash.insert(img.sha256.clone(), name.clone());
                    names.insert(img.src.clone(), name);
                }
                Err(e) => warnings.push(format!("An image couldn't be saved: {e}.")),
            }
        }
    } else if !c.images.is_empty() {
        warnings.push("Images were left as links: you can't add files to the attachments folder.".into());
    }
    let markdown = web::embed_images(&c.article.markdown, &names);
    let captured = chrono::Local::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, false);
    let text = note_text(&c, &title, &markdown, &captured);
    let cs = ChangeSet {
        changes: vec![Change::Create { path: path.clone(), text }],
        title: format!("Capture {title}"),
        rationale: format!("Web page captured from {}", c.url.as_string()),
        sources: vec![SourceRef::uri(c.url.as_string())],
        ..ChangeSet::default()
    };
    let outcome = agent.submit(actor, cs).map_err(CaptureError::Write)?;
    Ok(Done { path, outcome, images: by_hash.len(), warnings })
}
