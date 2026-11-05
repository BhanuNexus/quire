// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The pages. [`handle`] routes a request: assets and sign-in are open,
//! everything else needs a session, and anything that changes something
//! needs a form from this server (its `Origin`) with the session's CSRF
//! token.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use axum::http::{Method, StatusCode, header};
use quire_app::index::Index;
use quire_app::vault::{ContentHash, NotePath};
use serde_json::json;

use crate::auth::{self, Session};
use crate::edit::{self, Saved};
use crate::html::{self, Frame, csrf_field, esc};
use crate::paths::Guard;
use crate::render::{self, file_href, note_href};
use crate::{Req, Res, Web, encode_query, url_path};

/// The sign-in form's double-submit cookie.
const LOGIN_COOKIE: &str = "quire_login";

pub(crate) fn handle(web: &Web, req: &Req) -> Res {
    let path = req.path.as_str();
    if req.method == Method::GET {
        match path {
            "/assets/app.css" => return asset("text/css; charset=utf-8", html::css()),
            "/assets/app.js" => return asset("text/javascript; charset=utf-8", html::JS),
            "/assets/icon.svg" | "/favicon.ico" => return asset("image/svg+xml", html::ICON),
            "/login" => return login_page(web, req, StatusCode::OK, ""),
            _ => {}
        }
    }
    if req.method == Method::POST && path == "/login" {
        return login(web, req);
    }
    if req.method != Method::GET && req.method != Method::POST {
        return Res::html(StatusCode::METHOD_NOT_ALLOWED, html::error_page("That isn't something this page does."))
            .with(header::ALLOW, "GET, POST");
    }
    let Some(session) = session(web, req) else {
        if path.starts_with("/api/") {
            return Res::json(StatusCode::UNAUTHORIZED, &json!({"error": "sign in first"}));
        }
        if req.method == Method::POST {
            return Res::html(StatusCode::FORBIDDEN, html::error_page("You were signed out. Sign in again; nothing was changed."));
        }
        let next = if req.query.is_empty() {
            path.to_string()
        } else {
            let q: Vec<String> = req.query.iter().map(|(k, v)| format!("{}={}", encode_query(k), encode_query(v))).collect();
            format!("{path}?{}", q.join("&"))
        };
        return Res::redirect(&format!("/login?next={}", encode_query(&next)));
    };
    if req.method == Method::POST {
        // A form from one of this server's pages, in this session.
        if !web.same_origin(&req.headers) || !auth::same(req.f("_csrf"), &session.csrf) {
            return Res::html(
                StatusCode::FORBIDDEN,
                html::error_page("This form expired or came from another site. Reload the page and try again."),
            );
        }
    }
    let cx = Cx { web, req, session: &session };
    match (req.method == Method::POST, path) {
        (false, "/") => cx.home(),
        (false, "/search") => cx.search(),
        (false, "/tags") => cx.tags(),
        (false, "/new") => cx.new_form(""),
        (true, "/new") => cx.create(),
        (false, "/consent") => cx.consent(),
        (true, "/consent") => cx.decide(),
        (true, "/logout") => logout(web, req),
        (false, "/api/quick") => cx.quick(),
        (false, "/api/tree") => cx.tree_fragment(),
        (true, "/api/preview") => cx.preview(),
        (false, p) if p.starts_with("/n/") => cx.view(&p[3..]),
        (false, p) if p.starts_with("/f/") => cx.file(&p[3..]),
        (false, p) if p.starts_with("/d/") => cx.folder(&p[3..]),
        (false, p) if p.starts_with("/tags/") => cx.tag(&p[6..]),
        (false, p) if p.starts_with("/edit/") => cx.edit_form(&p[6..]),
        (true, p) if p.starts_with("/edit/") => cx.save(&p[6..]),
        _ => cx.not_found(),
    }
}

fn asset(kind: &str, body: &str) -> Res {
    Res { status: StatusCode::OK, headers: vec![(header::CONTENT_TYPE, kind.into())], body: body.as_bytes().to_vec() }
        .with(header::CACHE_CONTROL, "public, max-age=31536000, immutable")
}

fn session(web: &Web, req: &Req) -> Option<Session> {
    let token = req.cookie(web.session_cookie())?;
    let changed = web.cfg.login.changed();
    web.sessions.lock().unwrap_or_else(std::sync::PoisonError::into_inner).check(&token, web.now(), web.cfg.limits, changed)
}

/// Only a path on this server (`/…`, not `//host`).
fn safe_next(next: &str) -> &str {
    if next.starts_with('/') && !next.starts_with("//") && !next.contains('\\') && !next.starts_with("/login") { next } else { "/" }
}

fn login_page(web: &Web, req: &Req, status: StatusCode, message: &str) -> Res {
    let Some(token) = auth::random_token() else {
        return Res::html(StatusCode::INTERNAL_SERVER_ERROR, html::error_page("No randomness available."));
    };
    let next = safe_next(if req.f("next").is_empty() { req.q("next") } else { req.f("next") });
    let set = web.cfg.login.is_set();
    let body = if set {
        format!(
            "<h1>Sign in to Quire</h1>{}\
<form method=\"post\" action=\"/login\" class=\"stack\">\
<label for=\"pass\">Web passphrase</label>\
<input id=\"pass\" type=\"password\" name=\"passphrase\" autocomplete=\"current-password\" required autofocus>\
<input type=\"hidden\" name=\"_login\" value=\"{token}\"><input type=\"hidden\" name=\"next\" value=\"{}\">\
<button class=\"btn primary\">Sign in</button></form>\
<p class=\"muted\">Set or change it on the server with <code>quire-cli serve passwd</code>.</p>",
            if message.is_empty() { String::new() } else { format!("<p class=\"alert\" role=\"alert\">{}</p>", esc(message)) },
            esc(next)
        )
    } else {
        "<h1>Sign in to Quire</h1><p>No web passphrase is set. On the server, run <code>quire-cli serve passwd</code>, then reload this page.</p>"
            .to_string()
    };
    Res::html(status, html::plain("Sign in", &body)).with(header::SET_COOKIE, web.cookie(LOGIN_COOKIE, &token, 3600))
}

fn login(web: &Web, req: &Req) -> Res {
    let cookie = req.cookie(LOGIN_COOKIE).unwrap_or_default();
    if !web.same_origin(&req.headers) || cookie.is_empty() || !auth::same(&cookie, req.f("_login")) {
        return login_page(web, req, StatusCode::FORBIDDEN, "The sign-in form expired. Try again.");
    }
    let now = web.now();
    let allowed = web.limiter.lock().unwrap_or_else(std::sync::PoisonError::into_inner).check(now);
    if let Err(wait) = allowed {
        let secs = (wait + 999) / 1000;
        return login_page(web, req, StatusCode::TOO_MANY_REQUESTS, &format!("Too many wrong tries. Wait {secs} s and try again."))
            .with(header::RETRY_AFTER, secs.to_string());
    }
    if !web.cfg.login.verify(req.f("passphrase")) {
        web.limiter.lock().unwrap_or_else(std::sync::PoisonError::into_inner).failed(web.now());
        return login_page(web, req, StatusCode::UNAUTHORIZED, "That passphrase isn't right.");
    }
    web.limiter.lock().unwrap_or_else(std::sync::PoisonError::into_inner).succeeded();
    let Some((token, _)) = web.sessions.lock().unwrap_or_else(std::sync::PoisonError::into_inner).create(web.now()) else {
        return Res::html(StatusCode::INTERNAL_SERVER_ERROR, html::error_page("No randomness available."));
    };
    Res::redirect(safe_next(req.f("next")))
        .with(header::SET_COOKIE, web.cookie(web.session_cookie(), &token, web.cfg.limits.max_ms / 1000))
        .with(header::SET_COOKIE, web.cookie(LOGIN_COOKIE, "", 0))
}

fn logout(web: &Web, req: &Req) -> Res {
    if let Some(t) = req.cookie(web.session_cookie()) {
        web.sessions.lock().unwrap_or_else(std::sync::PoisonError::into_inner).end(&t);
    }
    Res::redirect("/login").with(header::SET_COOKIE, web.cookie(web.session_cookie(), "", 0))
}

/// One request with a session.
struct Cx<'a> {
    web: &'a Web,
    req: &'a Req,
    session: &'a Session,
}

/// Display name of a vault file: a note without `.md`.
fn display_name(p: &str) -> &str {
    let name = p.rsplit('/').next().unwrap_or(p);
    name.strip_suffix(".md").unwrap_or(name)
}

fn kind_class(p: &NotePath) -> &'static str {
    if p.is_markdown() {
        return "note";
    }
    let ext = std::path::Path::new(p.as_str()).extension().and_then(|e| e.to_str()).unwrap_or_default().to_ascii_lowercase();
    match ext.as_str() {
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "svg" | "bmp" | "avif" => "image",
        "pdf" => "pdf",
        _ => "other",
    }
}

impl Cx<'_> {
    fn hidden(&self) -> Vec<String> {
        self.web.guard.hidden()
    }

    /// Every file the web may list, from a short-lived cache.
    fn files(&self) -> std::sync::Arc<Vec<NotePath>> {
        let now = self.web.now();
        let mut c = self.web.files.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let fresh = c.as_ref().is_some_and(|(at, _)| now >= *at && now - at <= 3000);
        if !fresh {
            let hidden = self.hidden();
            let list: Vec<NotePath> =
                self.web.guard.vault().storage().list().unwrap_or_default().into_iter().filter(|p| Guard::listed(&hidden, p)).collect();
            *c = Some((now, std::sync::Arc::new(list)));
        }
        c.as_ref().map(|(_, l)| std::sync::Arc::clone(l)).unwrap_or_default()
    }

    fn page(&self, title: &str, folder: &str, current: Option<&NotePath>, main: &str) -> Res {
        let tree = self.tree(folder, current);
        let frame = Frame {
            title,
            csrf: &self.session.csrf,
            tree: &tree,
            read_only: self.web.cfg.read_only,
            query: if self.req.path == "/search" { self.req.q("q") } else { "" },
            folder,
        };
        Res::html(StatusCode::OK, html::page(&frame, main))
    }

    fn not_found(&self) -> Res {
        let mut r = self.page(
            "Not found",
            "",
            None,
            "<h1>Not here</h1><p>There's no note or page at this address.</p><p><a href=\"/\">Your notes</a></p>",
        );
        r.status = StatusCode::NOT_FOUND;
        r
    }

    /// The children of `folder`: subfolders, then files.
    fn children(&self, folder: &str) -> (Vec<String>, Vec<NotePath>) {
        let prefix = if folder.is_empty() { String::new() } else { format!("{folder}/") };
        let mut dirs = BTreeSet::new();
        let mut files = Vec::new();
        for p in self.files().iter() {
            let Some(rest) = p.as_str().strip_prefix(prefix.as_str()) else { continue };
            match rest.split_once('/') {
                Some((d, _)) => {
                    dirs.insert(d.to_string());
                }
                None => files.push(p.clone()),
            }
        }
        let mut dirs: Vec<String> = dirs.into_iter().collect();
        dirs.sort_by_key(|d| d.to_lowercase());
        files.sort_by_key(|f| f.as_str().to_lowercase());
        (dirs, files)
    }

    /// The tree's list for `folder`, opening the folders on the way to `open`.
    fn tree_list(&self, folder: &str, open: &str, current: Option<&NotePath>) -> String {
        let (dirs, files) = self.children(folder);
        let mut out = String::from("<ul>");
        for d in dirs {
            let full = if folder.is_empty() { d.clone() } else { format!("{folder}/{d}") };
            let is_open = open == full || open.starts_with(&format!("{full}/"));
            let _ = write!(
                out,
                "<li class=\"dir\"><details{} data-dir=\"{}\"><summary>{}</summary>",
                if is_open { " open" } else { "" },
                esc(&full),
                esc(&d)
            );
            if is_open {
                out.push_str(&self.tree_list(&full, open, current));
            } else {
                out.push_str("<ul></ul>");
            }
            out.push_str("</details></li>");
        }
        for f in files {
            let here = current == Some(&f);
            let _ = write!(
                out,
                "<li class=\"file {}\"><a href=\"{}\"{}>{}</a></li>",
                kind_class(&f),
                esc(&note_href(&f)),
                if here { " aria-current=\"page\"" } else { "" },
                esc(display_name(f.as_str()))
            );
        }
        out.push_str("</ul>");
        out
    }

    fn tree(&self, open: &str, current: Option<&NotePath>) -> String {
        format!("<div class=\"tree\">{}</div>", self.tree_list("", open, current))
    }

    fn tree_fragment(&self) -> Res {
        match self.web.guard.check_folder(self.req.q("dir")) {
            Ok(dir) => Res::html(StatusCode::OK, self.tree_list(&dir, "", None)),
            Err(_) => Res::html(StatusCode::NOT_FOUND, "<ul></ul>".into()),
        }
    }

    fn crumbs(folder: &str, last: Option<&str>) -> String {
        let mut out = String::from("<nav class=\"crumbs\" aria-label=\"Where this is\"><a href=\"/\">Notes</a>");
        let mut acc = String::new();
        for seg in folder.split('/').filter(|s| !s.is_empty()) {
            if !acc.is_empty() {
                acc.push('/');
            }
            acc.push_str(seg);
            let _ = write!(out, " <span aria-hidden=\"true\">/</span> <a href=\"/d/{}\">{}</a>", url_path(&acc), esc(seg));
        }
        if let Some(l) = last {
            let _ = write!(out, " <span aria-hidden=\"true\">/</span> <span aria-current=\"page\">{}</span>", esc(l));
        }
        out.push_str("</nav>");
        out
    }

    fn listing(&self, folder: &str) -> String {
        let (dirs, files) = self.children(folder);
        let mut out = String::from("<ul class=\"listing\">");
        for d in &dirs {
            let full = if folder.is_empty() { d.clone() } else { format!("{folder}/{d}") };
            let _ = write!(out, "<li class=\"dir\"><a href=\"/d/{}\">{}/</a></li>", url_path(&full), esc(d));
        }
        for f in &files {
            let _ = write!(
                out,
                "<li class=\"file {}\"><a href=\"{}\">{}</a></li>",
                kind_class(f),
                esc(&note_href(f)),
                esc(display_name(f.as_str()))
            );
        }
        if dirs.is_empty() && files.is_empty() {
            out.push_str("<li class=\"muted\">Nothing here yet.</li>");
        }
        out.push_str("</ul>");
        out
    }

    fn home(&self) -> Res {
        let hidden = self.hidden();
        let ix = self.web.cfg.server.index().read();
        let mut recent: Vec<(&NotePath, i64, &str)> = ix
            .records()
            .filter(|r| Guard::listed(&hidden, &r.path))
            .map(|r| (&r.path, ix.modified(&r.path).unwrap_or(0), r.title.as_str()))
            .collect();
        recent.sort_by_key(|(p, m, _)| (std::cmp::Reverse(*m), (*p).clone()));
        let mut main = String::from("<h1>Notes</h1>");
        if !recent.is_empty() {
            main.push_str("<section class=\"recent\"><h2>Recently changed</h2><ul class=\"listing\">");
            for (p, m, title) in recent.into_iter().take(12) {
                let when = if m > 0 { format!("<time>{}</time>", esc(&quire_app::chrono_fmt(m))) } else { String::new() };
                let _ = write!(
                    main,
                    "<li class=\"file note\"><a href=\"{}\">{}</a> <span class=\"muted path\">{}</span> {when}</li>",
                    esc(&note_href(p)),
                    esc(title),
                    esc(p.folder())
                );
            }
            main.push_str("</ul></section>");
        }
        drop(ix);
        main.push_str("<section><h2>All folders</h2>");
        main.push_str(&self.listing(""));
        main.push_str("</section>");
        self.page("Notes", "", None, &main)
    }

    fn folder(&self, raw: &str) -> Res {
        let Ok(folder) = self.web.guard.check_folder(raw) else { return self.not_found() };
        let name = folder.rsplit('/').next().unwrap_or(&folder).to_string();
        let parent = folder.rsplit_once('/').map_or("", |(a, _)| a);
        let main = format!("{}<h1>{}</h1>{}", Self::crumbs(parent, Some(&name)), esc(&name), self.listing(&folder));
        self.page(&name, &folder, None, &main)
    }

    fn view(&self, raw: &str) -> Res {
        let Ok(path) = self.web.guard.check(raw) else { return self.not_found() };
        let vault = self.web.guard.vault();
        if !vault.exists(&path) {
            if path.is_markdown() && !self.web.cfg.read_only {
                let main = format!(
                    "{}<h1>{}</h1><p>This note doesn't exist yet.</p><form method=\"post\" action=\"/new\">{}\
<input type=\"hidden\" name=\"folder\" value=\"{}\"><input type=\"hidden\" name=\"title\" value=\"{}\">\
<button class=\"btn primary\">Create it</button></form>",
                    Self::crumbs(path.folder(), None),
                    esc(path.stem()),
                    csrf_field(&self.session.csrf),
                    esc(path.folder()),
                    esc(path.stem())
                );
                let mut r = self.page(path.stem(), path.folder(), None, &main);
                r.status = StatusCode::NOT_FOUND;
                return r;
            }
            return self.not_found();
        }
        let crumbs = Self::crumbs(path.folder(), Some(display_name(path.as_str())));
        if !path.is_markdown() {
            let href = esc(&file_href(&path));
            let body = match kind_class(&path) {
                "image" => format!("<figure class=\"attachment\"><img src=\"{href}\" alt=\"{}\"></figure>", esc(path.file_name())),
                "pdf" => format!(
                    "<object class=\"pdf\" data=\"{href}\" type=\"application/pdf\" aria-label=\"{}\"><p>This browser doesn't show PDFs here.</p></object>",
                    esc(path.file_name())
                ),
                _ => String::new(),
            };
            let main = format!(
                "{crumbs}<h1>{}</h1>{body}<p class=\"actions\"><a class=\"btn\" href=\"{href}\">Open</a> <a class=\"btn\" href=\"{href}?download=1\">Download</a></p>",
                esc(path.file_name())
            );
            return self.page(path.file_name(), path.folder(), Some(&path), &main);
        }
        let Ok(src) = vault.read(&path) else { return self.not_found() };
        let hidden = self.hidden();
        let ix = self.web.cfg.server.index().read();
        let body = render::note(self.web, &ix, &hidden, &path, &src);
        let title = ix.get(&path).map_or_else(|| path.stem().to_string(), |r| r.title.clone());
        let aside = Self::note_aside(&ix, &hidden, &path);
        drop(ix);
        let edit = if self.web.cfg.read_only {
            String::new()
        } else {
            format!("<a class=\"btn\" href=\"/edit/{}\" accesskey=\"e\">Edit</a>", url_path(path.as_str()))
        };
        let main = format!(
            "<div class=\"note-head\">{crumbs}<div class=\"actions\">{edit}</div></div><article class=\"note\">{body}</article>{aside}"
        );
        self.page(&title, path.folder(), Some(&path), &main)
    }

    fn note_aside(ix: &Index, hidden: &[String], path: &NotePath) -> String {
        let mut seen = BTreeSet::new();
        let mut back = String::new();
        for b in ix.backlinks(path) {
            if b.from == *path || !Guard::listed(hidden, &b.from) || !seen.insert(b.from.clone()) {
                continue;
            }
            let title = ix.get(&b.from).map_or_else(|| b.from.stem().to_string(), |r| r.title.clone());
            let _ = write!(
                back,
                "<li><a href=\"{}\">{}</a><span class=\"ctx\">{}</span></li>",
                esc(&note_href(&b.from)),
                esc(&title),
                esc(b.context.trim().trim_start_matches(['-', '*', '>', ' ']))
            );
        }
        let tags: Vec<String> = ix
            .get(path)
            .map(|r| r.tags.iter().map(|t| format!("<a class=\"tag\" href=\"/tags/{}\">#{}</a>", url_path(t), esc(t))).collect())
            .unwrap_or_default();
        if back.is_empty() && tags.is_empty() {
            return String::new();
        }
        let mut out = String::from("<aside class=\"meta\">");
        if !back.is_empty() {
            let _ = write!(out, "<section><h2>Linked from</h2><ul class=\"backlinks\">{back}</ul></section>");
        }
        if !tags.is_empty() {
            let _ = write!(out, "<section><h2>Tags</h2><p class=\"tags\">{}</p></section>", tags.join(" "));
        }
        out.push_str("</aside>");
        out
    }

    fn file(&self, raw: &str) -> Res {
        let Ok(path) = self.web.guard.check(raw) else { return self.not_found() };
        if path.is_markdown() && self.req.q("download").is_empty() {
            return Res::redirect(&note_href(&path));
        }
        let Ok(bytes) = self.web.guard.vault().storage().read_bytes(&path) else { return self.not_found() };
        let lower = path.as_str().to_lowercase();
        let ext = lower.rsplit('.').next().unwrap_or_default();
        let shown = match ext {
            "png" => Some("image/png"),
            "jpg" | "jpeg" => Some("image/jpeg"),
            "gif" => Some("image/gif"),
            "webp" => Some("image/webp"),
            "avif" => Some("image/avif"),
            "bmp" => Some("image/bmp"),
            "svg" => Some("image/svg+xml"),
            "pdf" => Some("application/pdf"),
            "md" | "txt" | "csv" => Some("text/plain; charset=utf-8"),
            "mp3" => Some("audio/mpeg"),
            "m4a" => Some("audio/mp4"),
            "ogg" => Some("audio/ogg"),
            "wav" => Some("audio/wav"),
            "mp4" => Some("video/mp4"),
            "webm" => Some("video/webm"),
            _ => None,
        };
        let download = !self.req.q("download").is_empty() || shown.is_none();
        let name = path.file_name().replace(['"', '\\', '\r', '\n'], "_");
        let disposition = format!(
            "{}; filename=\"{}\"; filename*=UTF-8''{}",
            if download { "attachment" } else { "inline" },
            name.chars().map(|c| if c.is_ascii() { c } else { '_' }).collect::<String>(),
            encode_query(path.file_name())
        );
        let mut r = Res {
            status: StatusCode::OK,
            headers: vec![
                (header::CONTENT_TYPE, shown.unwrap_or("application/octet-stream").into()),
                (header::CONTENT_DISPOSITION, disposition),
            ],
            body: bytes,
        };
        // A file from the vault never runs as a page of this site: anything
        // but a PDF (which the browser's viewer needs) is sandboxed.
        if ext != "pdf" {
            r = r.with(
                header::CONTENT_SECURITY_POLICY,
                "sandbox; default-src 'none'; img-src 'self' data:; style-src 'unsafe-inline'; media-src 'self'",
            );
        }
        r
    }

    fn search(&self) -> Res {
        let q = self.req.q("q").trim().to_string();
        let mut main = format!(
            "<h1>Search</h1><form action=\"/search\" role=\"search\" class=\"search-big\"><input type=\"search\" name=\"q\" value=\"{}\" aria-label=\"Search notes\" autofocus><button class=\"btn primary\">Search</button></form>",
            esc(&q)
        );
        if q.is_empty() {
            main.push_str("<p class=\"muted\">Words, <code>\"exact phrases\"</code> and <code>#tags</code>. Accents and letter case don't matter.</p>");
            return self.page("Search", "", None, &main);
        }
        let hidden = self.hidden();
        let ix = self.web.cfg.server.index().read();
        let found = ix.search(&q, 200);
        drop(ix);
        // The full check (symlinks) for what is shown.
        let hits: Vec<_> = found
            .into_iter()
            .filter(|h| Guard::listed(&hidden, &h.path) && self.web.guard.check(h.path.as_str()).is_ok())
            .take(50)
            .collect();
        let _ = write!(
            main,
            "<p class=\"muted\" role=\"status\">{}</p><ol class=\"results\">",
            match hits.len() {
                0 => "No notes match.".to_string(),
                1 => "1 note".to_string(),
                n => format!("{n} notes"),
            }
        );
        for h in &hits {
            let mut snippet = String::new();
            let mut at = 0;
            for r in &h.highlights {
                if r.start < at || r.end > h.snippet.len() || !h.snippet.is_char_boundary(r.start) || !h.snippet.is_char_boundary(r.end) {
                    continue;
                }
                snippet.push_str(&esc(&h.snippet[at..r.start]));
                let _ = write!(snippet, "<mark>{}</mark>", esc(&h.snippet[r.start..r.end]));
                at = r.end;
            }
            snippet.push_str(&esc(&h.snippet[at..]));
            let _ = write!(
                main,
                "<li><a href=\"{}\">{}</a> <span class=\"muted path\">{}</span><p class=\"snippet\">{snippet}</p></li>",
                esc(&note_href(&h.path)),
                esc(&h.title),
                esc(h.path.folder())
            );
        }
        main.push_str("</ol>");
        self.page(&format!("Search: {q}"), "", None, &main)
    }

    fn quick(&self) -> Res {
        let q = self.req.q("q");
        let hidden = self.hidden();
        let ix = self.web.cfg.server.index().read();
        let hits: Vec<serde_json::Value> = if q.trim().is_empty() {
            let mut all: Vec<_> =
                ix.records().filter(|r| Guard::listed(&hidden, &r.path)).map(|r| (ix.modified(&r.path).unwrap_or(0), r)).collect();
            all.sort_by_key(|(m, r)| (std::cmp::Reverse(*m), r.path.clone()));
            all.into_iter().take(12).map(|(_, r)| json!({"title": r.title, "path": r.path.as_str(), "url": note_href(&r.path)})).collect()
        } else {
            ix.quick_open(q, 60)
                .into_iter()
                .filter(|h| Guard::listed(&hidden, &h.path))
                .take(12)
                .map(|h| json!({"title": h.title, "path": h.path.as_str(), "url": note_href(&h.path)}))
                .collect()
        };
        Res::json(StatusCode::OK, &json!({ "hits": hits }))
    }

    fn tags(&self) -> Res {
        let hidden = self.hidden();
        let ix = self.web.cfg.server.index().read();
        let mut counts: BTreeMap<String, (String, usize)> = BTreeMap::new();
        for r in ix.records().filter(|r| Guard::listed(&hidden, &r.path)) {
            let mut mine = BTreeSet::new();
            for t in &r.tags {
                // `a/b` counts for `a` too.
                let mut acc = String::new();
                for part in t.split('/') {
                    if !acc.is_empty() {
                        acc.push('/');
                    }
                    acc.push_str(part);
                    mine.insert(acc.clone());
                }
            }
            for t in mine {
                counts.entry(t.to_lowercase()).or_insert_with(|| (t.clone(), 0)).1 += 1;
            }
        }
        drop(ix);
        let mut main = String::from("<h1>Tags</h1>");
        if counts.is_empty() {
            main.push_str("<p class=\"muted\">No tags yet.</p>");
        } else {
            main.push_str("<ul class=\"tag-list\">");
            for (shown, n) in counts.values() {
                let _ = write!(
                    main,
                    "<li><a class=\"tag\" href=\"/tags/{}\">#{}</a> <span class=\"muted\">{n}</span></li>",
                    url_path(shown),
                    esc(shown)
                );
            }
            main.push_str("</ul>");
        }
        self.page("Tags", "", None, &main)
    }

    fn tag(&self, raw: &str) -> Res {
        let tag = raw.trim_start_matches('#');
        let hidden = self.hidden();
        let ix = self.web.cfg.server.index().read();
        let mut notes: Vec<(String, NotePath)> = ix
            .notes_with_tag(tag)
            .into_iter()
            .filter(|p| Guard::listed(&hidden, p))
            .map(|p| (ix.get(&p).map_or_else(|| p.stem().to_string(), |r| r.title.clone()), p))
            .collect();
        drop(ix);
        notes.sort_by_key(|(t, _)| t.to_lowercase());
        let mut main = format!("<nav class=\"crumbs\"><a href=\"/tags\">Tags</a></nav><h1>#{}</h1><ul class=\"listing\">", esc(tag));
        for (t, p) in &notes {
            let _ = write!(
                main,
                "<li class=\"file note\"><a href=\"{}\">{}</a> <span class=\"muted path\">{}</span></li>",
                esc(&note_href(p)),
                esc(t),
                esc(p.folder())
            );
        }
        if notes.is_empty() {
            main.push_str("<li class=\"muted\">No notes have this tag.</li>");
        }
        main.push_str("</ul>");
        self.page(&format!("#{tag}"), "", None, &main)
    }

    fn read_only(&self) -> Res {
        let mut r = self.page(
            "Read-only",
            "",
            None,
            "<h1>Read-only</h1><p>This server was started with <code>--read-only</code>: notes can't be changed here.</p>",
        );
        r.status = StatusCode::FORBIDDEN;
        r
    }

    /// A Markdown note the person may write: visible, `.md`.
    fn writable(&self, raw: &str) -> Option<NotePath> {
        self.web.guard.check(raw).ok().filter(NotePath::is_markdown)
    }

    fn editor(&self, path: &NotePath, text: &str, base_text: &str, notice: &str, preview: Option<&str>) -> String {
        let base_text = base_text.replace("\r\n", "\n");
        let base = if self.web.guard.vault().exists(path) { ContentHash::of(&base_text).as_str().to_string() } else { String::new() };
        let title = display_name(path.as_str());
        format!(
            "{crumbs}{notice}<form method=\"post\" action=\"/edit/{url}\" class=\"editor\" id=\"editor\">{csrf}\
<input type=\"hidden\" name=\"base\" value=\"{base}\"><textarea name=\"orig\" hidden aria-hidden=\"true\" tabindex=\"-1\">\n{orig}</textarea>\
<div class=\"editor-bar\"><h1>Editing <span>{t}</span></h1><div class=\"actions\">\
<button class=\"btn primary\" name=\"action\" value=\"save\" title=\"Save (Ctrl+S)\">Save</button>\
<button class=\"btn\" name=\"action\" value=\"preview\" id=\"preview-btn\" aria-controls=\"preview\">Preview</button>\
<a class=\"btn ghost\" href=\"/n/{url}\">Cancel</a></div></div>\
<div class=\"panes{split}\"><label for=\"text\" class=\"visually-hidden\">Markdown text of {t}</label>\
<textarea name=\"text\" id=\"text\" spellcheck=\"true\" autofocus>\n{text}</textarea>\
<div id=\"preview\" class=\"note preview\" aria-live=\"polite\"{hidden}>{pv}</div></div></form>",
            crumbs = Self::crumbs(path.folder(), Some(title)),
            url = url_path(path.as_str()),
            csrf = csrf_field(&self.session.csrf),
            orig = esc(&base_text),
            t = esc(title),
            text = esc(&text.replace("\r\n", "\n")),
            split = if preview.is_some() { " split" } else { "" },
            hidden = if preview.is_some() { "" } else { " hidden" },
            pv = preview.unwrap_or_default(),
        )
    }

    fn edit_form(&self, raw: &str) -> Res {
        if self.web.cfg.read_only {
            return self.read_only();
        }
        let Some(path) = self.writable(raw) else { return self.not_found() };
        let text = self.web.guard.vault().read(&path).unwrap_or_default();
        let main = self.editor(&path, &text, &text, "", None);
        self.page(&format!("Edit {}", display_name(path.as_str())), path.folder(), Some(&path), &main)
    }

    fn preview_html(&self, path: &NotePath, text: &str) -> String {
        let hidden = self.hidden();
        let ix = self.web.cfg.server.index().read();
        render::note(self.web, &ix, &hidden, path, text)
    }

    fn preview(&self) -> Res {
        let path = self.writable(self.req.f("path")).unwrap_or_else(|| NotePath::new("Preview.md"));
        Res::html(StatusCode::OK, self.preview_html(&path, self.req.f("text")))
    }

    fn save(&self, raw: &str) -> Res {
        if self.web.cfg.read_only {
            return self.read_only();
        }
        let Some(path) = self.writable(raw) else { return self.not_found() };
        let text = self.req.f("text").replace("\r\n", "\n");
        let orig = self.req.f("orig").replace("\r\n", "\n");
        let title = format!("Edit {}", display_name(path.as_str()));
        match self.req.f("action") {
            "preview" => {
                let pv = self.preview_html(&path, &text);
                // The editor keeps the version it started from.
                let main = self.editor_keeping_base(&path, &text, &orig, Some(&pv));
                return self.page(&title, path.folder(), Some(&path), &main);
            }
            "copy" => {
                let today = chrono::Local::now().format("%Y-%m-%d").to_string();
                let copy = edit::copy_path(self.web.guard.vault(), &path, &today);
                return match edit::write(self.web, &copy, None, &text, quire_agent::Op::Create) {
                    Ok(()) => Res::redirect(&note_href(&copy)),
                    Err(e) => self.failed(&e),
                };
            }
            _ => {}
        }
        match edit::save(self.web, &path, self.req.f("base"), &orig, &text) {
            Ok(Saved::Written | Saved::Unchanged) => Res::redirect(&note_href(&path)),
            Ok(Saved::Conflict { current, merged }) => {
                let main = self.conflict(&path, &text, &current, merged.as_deref());
                let mut r = self.page(&title, path.folder(), Some(&path), &main);
                r.status = StatusCode::CONFLICT;
                r
            }
            Err(e) => self.failed(&e),
        }
    }

    /// The editor with a given start version (a preview mid-edit).
    fn editor_keeping_base(&self, path: &NotePath, text: &str, orig: &str, preview: Option<&str>) -> String {
        self.editor(path, text, orig, "", preview)
    }

    fn failed(&self, e: &str) -> Res {
        let mut r = self.page("Not saved", "", None, &format!("<h1>Not saved</h1><p>{}</p><p>Go back to keep your text.</p>", esc(e)));
        r.status = StatusCode::INTERNAL_SERVER_ERROR;
        r
    }

    fn conflict(&self, path: &NotePath, mine: &str, current: &str, merged: Option<&str>) -> String {
        if let Some(m) = merged {
            let notice = "<div class=\"alert\" role=\"alert\"><strong>This note changed while you were editing.</strong> \
Your changes and the other ones don't overlap, so both are combined below. Nothing is saved until you check it and save.</div>";
            return self.editor(path, m, current, notice, None);
        }
        let gone = current.is_empty() && !self.web.guard.vault().exists(path);
        let mut out = format!(
            "{}<div class=\"alert\" role=\"alert\"><strong>{}</strong> Nothing was overwritten. Choose what to do with your text.</div>",
            Self::crumbs(path.folder(), Some(display_name(path.as_str()))),
            if gone {
                "This note was deleted or moved while you were editing."
            } else {
                "This note changed while you were editing, on the same lines you changed."
            }
        );
        if !gone {
            out.push_str("<h2>What differs</h2><p class=\"muted\">Lines only in the saved version are marked −, lines only in yours +.</p><pre class=\"diff\">");
            let diff = similar::TextDiff::from_lines(current, mine);
            for change in diff.iter_all_changes() {
                let (class, sign) = match change.tag() {
                    similar::ChangeTag::Delete => ("del", "−"),
                    similar::ChangeTag::Insert => ("ins", "+"),
                    similar::ChangeTag::Equal => ("eq", " "),
                };
                let _ = write!(
                    out,
                    "<span class=\"{class}\">{sign} {}</span>",
                    esc(change.as_str().unwrap_or_default().trim_end_matches('\n'))
                );
                out.push('\n');
            }
            out.push_str("</pre>");
        }
        let _ = write!(
            out,
            "<div class=\"choices\"><form method=\"post\" action=\"/edit/{url}\">{csrf}<input type=\"hidden\" name=\"action\" value=\"copy\">\
<textarea name=\"text\" hidden aria-hidden=\"true\" tabindex=\"-1\">\n{mine}</textarea>\
<button class=\"btn primary\">Save mine as a copy</button></form>",
            url = url_path(path.as_str()),
            csrf = csrf_field(&self.session.csrf),
            mine = esc(mine)
        );
        if !gone {
            out.push_str("<details class=\"redo\"><summary class=\"btn\">Edit mine against the saved version</summary>");
            out.push_str(&self.editor(path, mine, current, "", None));
            out.push_str("</details>");
        }
        out.push_str("</div>");
        out
    }

    fn new_form(&self, message: &str) -> Res {
        if self.web.cfg.read_only {
            return self.read_only();
        }
        let folder = self.web.guard.check_folder(self.req.q("folder")).unwrap_or_default();
        let mut folders: Vec<String> = self
            .files()
            .iter()
            .filter_map(|p| p.as_str().rsplit_once('/').map(|(d, _)| d.to_string()))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        folders.sort_by_key(|f| f.to_lowercase());
        let options: String = folders.iter().fold(String::new(), |mut s, f| {
            let _ = write!(s, "<option value=\"{}\">", esc(f));
            s
        });
        let main = format!(
            "<h1>New note</h1>{}<form method=\"post\" action=\"/new\" class=\"stack narrow-form\">{}\
<label for=\"title\">Name</label><input id=\"title\" name=\"title\" required autofocus autocomplete=\"off\">\
<label for=\"folder\">Folder <span class=\"muted\">(empty: the top of the vault)</span></label>\
<input id=\"folder\" name=\"folder\" value=\"{}\" list=\"folders\" autocomplete=\"off\"><datalist id=\"folders\">{options}</datalist>\
<button class=\"btn primary\">Create</button></form>",
            if message.is_empty() { String::new() } else { format!("<p class=\"alert\" role=\"alert\">{}</p>", esc(message)) },
            csrf_field(&self.session.csrf),
            esc(&folder)
        );
        self.page("New note", &folder, None, &main)
    }

    fn create(&self) -> Res {
        if self.web.cfg.read_only {
            return self.read_only();
        }
        let Ok(folder) = self.web.guard.check_folder(self.req.f("folder").trim()) else {
            return self.new_form("That folder can't be used here.");
        };
        let title = self.req.f("title").trim();
        if title.is_empty() || title.contains(['/', '\\']) || title.starts_with('.') {
            return self.new_form("Give the note a name without / or \\, not starting with a dot.");
        }
        let probe = if folder.is_empty() { format!("{title}.md") } else { format!("{folder}/{title}.md") };
        if self.writable(&probe).is_none() {
            return self.new_form("That name can't be used here.");
        }
        match edit::create(self.web, &folder, title) {
            Ok(p) => Res::redirect(&format!("/edit/{}", url_path(p.as_str()))),
            Err(e) => self.new_form(&e),
        }
    }

    fn consent(&self) -> Res {
        let store = &self.web.cfg.consent;
        let now = quire_app::history_ops::now_ms();
        let id = self.req.q("request");
        let mine = |r: &quire_mcp::consent::Request| r.resource == self.web.urls.resource;
        if id.is_empty() {
            let list: Vec<_> = store.pending(now).into_iter().filter(mine).collect();
            let mut main = String::from("<h1>Apps asking to connect</h1>");
            if list.is_empty() {
                main.push_str("<p class=\"muted\">No app is waiting.</p>");
            }
            main.push_str("<ul class=\"listing\">");
            for r in list {
                let _ = write!(
                    main,
                    "<li><a href=\"/consent?request={}\">{}</a> <span class=\"muted\">calls itself {}</span></li>",
                    esc(&r.id),
                    esc(&r.client_host),
                    esc(&r.client_name)
                );
            }
            main.push_str("</ul>");
            return self.page("Apps asking to connect", "", None, &main);
        }
        let Some(r) = store.get(id).filter(mine) else { return self.not_found() };
        if r.state != quire_mcp::consent::State::Pending || r.expires <= now {
            return self.page(
                "Connect an app",
                "",
                None,
                "<h1>Connect an app</h1><p>This request was already answered or expired. Start connecting again from the app.</p>",
            );
        }
        let wants = r.scopes().describe().iter().fold(String::new(), |mut s, d| {
            let _ = write!(s, "<li>{}</li>", esc(d));
            s
        });
        let main = format!(
            "<div class=\"narrow\"><h1>Connect an app to your notes?</h1>\
<p>An app at <strong class=\"host\">{}</strong> (it calls itself “{}”) asks to:</p><ul>{wants}</ul>\
<p>Its sign-in page shows a code. Approve only if it is this one:</p><p class=\"code\">{}</p>\
<form method=\"post\" action=\"/consent\" class=\"row\">{}<input type=\"hidden\" name=\"request\" value=\"{}\">\
<button class=\"btn primary\" name=\"decision\" value=\"approve\">Approve</button>\
<button class=\"btn\" name=\"decision\" value=\"deny\">Deny</button></form>\
<p class=\"muted\">The app gets access only in the browser window where it started signing in.</p></div>",
            esc(&r.client_host),
            esc(&r.client_name),
            esc(r.verify_code()),
            csrf_field(&self.session.csrf),
            esc(&r.id)
        );
        self.page("Connect an app", "", None, &main)
    }

    fn decide(&self) -> Res {
        let id = self.req.f("request");
        let store = &self.web.cfg.consent;
        let Some(r) = store.get(id).filter(|r| r.resource == self.web.urls.resource) else { return self.not_found() };
        let approve = self.req.f("decision") == "approve";
        if let Err(e) = store.decide(&r.id, approve, quire_app::history_ops::now_ms()) {
            return self.page("Connect an app", "", None, &format!("<h1>Connect an app</h1><p>{}</p>", esc(&e)));
        }
        // The browser that started signing in finishes it (it holds the
        // request's secret); any other browser is told so.
        Res::redirect(&format!("/oauth/authorize/wait?request={}", encode_query(&r.id)))
    }
}
