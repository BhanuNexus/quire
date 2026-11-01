// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! A small HTML → Markdown converter for Evernote's ENML.
//!
//! ENML is restricted XHTML: divs per line, lists, simple tables, inline
//! marks, plus `<en-media>` and `<en-todo>`. A streaming tag walk covers it
//! without pulling a full HTML5 parser into the build.

/// What to emit for an `<en-media hash=…>` element.
pub type MediaFn<'a> = &'a mut dyn FnMut(&str) -> Option<String>;

struct List {
    ordered: bool,
    n: usize,
}

struct Writer<'a> {
    out: String,
    lists: Vec<List>,
    quote: usize,
    pre: usize,
    links: Vec<Option<String>>,
    skip: usize,
    table_row: usize,
    row_cells: usize,
    first_row_cells: usize,
    media: MediaFn<'a>,
}

impl Writer<'_> {
    fn at_line_start(&self) -> bool {
        self.out.is_empty() || self.out.ends_with('\n')
    }

    fn newline(&mut self) {
        if !self.at_line_start() {
            self.out.push('\n');
        }
    }

    fn blank_line(&mut self) {
        self.newline();
        if !self.out.is_empty() && !self.out.ends_with("\n\n") {
            self.out.push('\n');
        }
    }

    fn prefix(&mut self) {
        if self.at_line_start() {
            for _ in 0..self.quote {
                self.out.push_str("> ");
            }
            if !self.lists.is_empty() && self.pre == 0 {
                // Continuation lines inside a list item.
                let depth = self.lists.len();
                self.out.push_str(&"  ".repeat(depth));
            }
        }
    }

    fn text(&mut self, t: &str) {
        if self.skip > 0 {
            return;
        }
        let t = unescape(t);
        if self.pre > 0 {
            self.out.push_str(&t);
            return;
        }
        let collapsed: String = t.split_whitespace().collect::<Vec<_>>().join(" ");
        if collapsed.is_empty() {
            if t.chars().any(char::is_whitespace) && !self.at_line_start() && !self.out.ends_with(' ') {
                self.out.push(' ');
            }
            return;
        }
        if self.at_line_start() {
            self.prefix();
        } else if t.starts_with(char::is_whitespace) && !self.out.ends_with(' ') {
            self.out.push(' ');
        }
        self.out.push_str(&collapsed);
        if t.ends_with(char::is_whitespace) {
            self.out.push(' ');
        }
    }

    fn raw(&mut self, s: &str) {
        if self.skip == 0 {
            if self.at_line_start() {
                self.prefix();
            }
            self.out.push_str(s);
        }
    }

    fn trim_trailing_space(&mut self) {
        while self.out.ends_with(' ') {
            self.out.pop();
        }
    }

    fn open(&mut self, name: &str, attrs: &str, self_closing: bool) {
        if matches!(name, "style" | "script" | "head" | "title") && !self_closing {
            self.skip += 1;
            return;
        }
        if self.skip > 0 {
            return;
        }
        match name {
            "div" | "p" | "section" | "article" | "center" => self.newline(),
            "br" => {
                self.trim_trailing_space();
                self.out.push('\n');
            }
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                self.blank_line();
                let level = name[1..].parse::<usize>().unwrap_or(1);
                self.raw(&format!("{} ", "#".repeat(level)));
            }
            "b" | "strong" => self.raw("**"),
            "i" | "em" => self.raw("*"),
            "s" | "strike" | "del" => self.raw("~~"),
            "code" if self.pre == 0 => self.raw("`"),
            "pre" => {
                self.blank_line();
                self.raw("```\n");
                self.pre += 1;
            }
            "blockquote" => {
                self.newline();
                self.quote += 1;
            }
            "hr" => {
                self.blank_line();
                self.out.push_str("---\n\n");
            }
            "ul" | "ol" => {
                self.newline();
                self.lists.push(List { ordered: name == "ol", n: 0 });
            }
            "li" => {
                self.newline();
                let depth = self.lists.len().max(1);
                for _ in 0..self.quote {
                    self.out.push_str("> ");
                }
                self.out.push_str(&"  ".repeat(depth - 1));
                let marker = match self.lists.last_mut() {
                    Some(l) if l.ordered => {
                        l.n += 1;
                        format!("{}. ", l.n)
                    }
                    _ => "- ".to_string(),
                };
                self.out.push_str(&marker);
            }
            "a" => {
                let href = attr(attrs, "href").filter(|h| !h.is_empty());
                if href.is_some() {
                    self.raw("[");
                }
                self.links.push(href);
            }
            "img" => {
                let src = attr(attrs, "src").unwrap_or_default();
                let alt = attr(attrs, "alt").unwrap_or_default();
                self.raw(&format!("![{alt}]({src})"));
            }
            "en-media" => {
                if let Some(rep) = attr(attrs, "hash").and_then(|h| (self.media)(&h)) {
                    self.raw(&rep);
                }
            }
            "en-todo" => {
                let checked = attr(attrs, "checked").is_some_and(|c| c == "true");
                let mark = if checked { "[x] " } else { "[ ] " };
                if self.at_line_start() {
                    self.raw(&format!("- {mark}"));
                } else {
                    self.raw(mark);
                }
            }
            "table" => {
                self.blank_line();
                self.table_row = 0;
            }
            "tr" => {
                self.newline();
                self.row_cells = 0;
                self.raw("|");
            }
            "td" | "th" => self.raw(" "),
            _ => {}
        }
    }

    fn close(&mut self, name: &str) {
        if matches!(name, "style" | "script" | "head" | "title") {
            self.skip = self.skip.saturating_sub(1);
            return;
        }
        if self.skip > 0 {
            return;
        }
        match name {
            "div" | "p" | "section" | "article" | "center" => {
                self.trim_trailing_space();
                self.newline();
                if name == "p" {
                    self.blank_line();
                }
            }
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                self.trim_trailing_space();
                self.blank_line();
            }
            "b" | "strong" => self.out.push_str("**"),
            "i" | "em" => self.out.push('*'),
            "s" | "strike" | "del" => self.out.push_str("~~"),
            "code" if self.pre == 0 => self.out.push('`'),
            "pre" => {
                self.pre = self.pre.saturating_sub(1);
                self.newline();
                self.out.push_str("```\n\n");
            }
            "blockquote" => {
                self.quote = self.quote.saturating_sub(1);
                self.newline();
            }
            "ul" | "ol" => {
                self.lists.pop();
                self.newline();
                if self.lists.is_empty() {
                    self.blank_line();
                }
            }
            "li" => {
                self.trim_trailing_space();
                self.newline();
            }
            "a" => {
                if let Some(Some(href)) = self.links.pop() {
                    self.out.push_str(&format!("]({href})"));
                }
            }
            "td" | "th" => {
                self.trim_trailing_space();
                self.out.push_str(" |");
                self.row_cells += 1;
            }
            "tr" => {
                self.out.push('\n');
                if self.table_row == 0 {
                    self.first_row_cells = self.row_cells.max(1);
                    self.out.push_str(&format!("|{}\n", " --- |".repeat(self.first_row_cells)));
                }
                self.table_row += 1;
            }
            "table" => self.blank_line(),
            _ => {}
        }
    }
}

fn attr(attrs: &str, name: &str) -> Option<String> {
    let mut rest = attrs;
    while let Some(i) = rest.find(name) {
        let before_ok = i == 0 || rest.as_bytes()[i - 1].is_ascii_whitespace();
        let after = rest[i + name.len()..].trim_start();
        if before_ok && let Some(v) = after.strip_prefix('=') {
            let v = v.trim_start();
            let (q, body) = match v.chars().next() {
                Some(c @ ('"' | '\'')) => (Some(c), &v[1..]),
                _ => (None, v),
            };
            let end = match q {
                Some(c) => body.find(c).unwrap_or(body.len()),
                None => body.find(|c: char| c.is_whitespace() || c == '/' || c == '>').unwrap_or(body.len()),
            };
            return Some(unescape(&body[..end]));
        }
        rest = &rest[i + name.len()..];
    }
    None
}

/// XML/HTML entities: named basics and numeric references.
pub fn unescape(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        let after = &rest[i + 1..];
        let end = after.find(';').filter(|&e| e <= 10);
        let decoded = end.and_then(|e| {
            let name = &after[..e];
            let c = match name {
                "amp" => '&',
                "lt" => '<',
                "gt" => '>',
                "quot" => '"',
                "apos" => '\'',
                "nbsp" => ' ',
                _ => {
                    let n = name
                        .strip_prefix("#x")
                        .or_else(|| name.strip_prefix("#X"))
                        .map(|h| u32::from_str_radix(h, 16))
                        .or_else(|| name.strip_prefix('#').map(|d| d.parse()))?;
                    char::from_u32(n.ok()?)?
                }
            };
            Some((c, e))
        });
        match decoded {
            Some((c, e)) => {
                out.push(c);
                rest = &after[e + 1..];
            }
            None => {
                out.push('&');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Convert ENML/HTML to Markdown.
pub fn to_markdown(html: &str, media: MediaFn<'_>) -> String {
    let mut w = Writer {
        out: String::new(),
        lists: Vec::new(),
        quote: 0,
        pre: 0,
        links: Vec::new(),
        skip: 0,
        table_row: 0,
        row_cells: 0,
        first_row_cells: 0,
        media,
    };
    let mut rest = html;
    while let Some(lt) = rest.find('<') {
        w.text(&rest[..lt]);
        let after = &rest[lt + 1..];
        if let Some(c) = after.strip_prefix("!--") {
            rest = c.find("-->").map_or("", |e| &c[e + 3..]);
            continue;
        }
        if let Some(c) = after.strip_prefix("![CDATA[") {
            let e = c.find("]]>").unwrap_or(c.len());
            w.text(&c[..e]);
            rest = c.get(e + 3..).unwrap_or("");
            continue;
        }
        let Some(gt) = after.find('>') else {
            w.text(&rest[lt..]);
            rest = "";
            break;
        };
        let tag = &after[..gt];
        rest = &after[gt + 1..];
        if tag.starts_with('!') || tag.starts_with('?') {
            continue;
        }
        if let Some(name) = tag.strip_prefix('/') {
            w.close(&name.trim().to_ascii_lowercase());
            continue;
        }
        let self_closing = tag.ends_with('/');
        let tag = tag.trim_end_matches('/');
        let (name, attrs) = tag.split_once(char::is_whitespace).unwrap_or((tag, ""));
        let name = name.to_ascii_lowercase();
        w.open(&name, attrs, self_closing);
        if self_closing && !matches!(name.as_str(), "br" | "hr" | "img" | "en-media" | "en-todo") {
            w.close(&name);
        }
    }
    w.text(rest);
    // Tidy: trim trailing spaces per line, collapse 3+ newlines.
    let mut out = String::with_capacity(w.out.len());
    let mut blank = 0;
    for line in w.out.lines() {
        let line = line.trim_end();
        if line.is_empty() {
            blank += 1;
            if blank > 1 {
                continue;
            }
        } else {
            blank = 0;
        }
        out.push_str(line);
        out.push('\n');
    }
    out.trim_start_matches('\n').trim_end().to_string() + "\n"
}

#[cfg(test)]
mod tests {
    use super::*;

    fn md(html: &str) -> String {
        let mut none = |_: &str| None;
        to_markdown(html, &mut none)
    }

    #[test]
    fn blocks_and_inline() {
        assert_eq!(
            md("<en-note><div>Hello <b>bold</b> &amp; <i>it</i></div><div><br/></div><div>Next</div></en-note>"),
            "Hello **bold** & *it*\n\nNext\n"
        );
        assert_eq!(
            md("<h2>Title</h2><p>Para with <a href=\"https://x.org/?a=1&amp;b=2\">link</a>.</p>"),
            "## Title\n\nPara with [link](https://x.org/?a=1&b=2).\n"
        );
        assert_eq!(md("<ul><li>a</li><li>b<ol><li>c</li></ol></li></ul>"), "- a\n- b\n  1. c\n");
        assert_eq!(md("<div><en-todo checked=\"true\"/>done</div><div><en-todo/>open</div>"), "- [x] done\n- [ ] open\n");
        assert_eq!(md("<pre>let x = 1;\n  y</pre>"), "```\nlet x = 1;\n  y\n```\n");
        assert_eq!(
            md("<table><tr><td>A</td><td>B</td></tr><tr><td>1</td><td>2</td></tr></table>"),
            "| A | B |\n| --- | --- |\n| 1 | 2 |\n"
        );
        assert_eq!(md("<style>x{}</style><div>&#8212;&#x41;&nbsp;</div>"), "—A\n");
    }

    #[test]
    fn media_callback() {
        let mut f = |h: &str| (h == "abc").then(|| "![[a.png]]".to_string());
        assert_eq!(to_markdown("<div>see <en-media hash=\"abc\" type=\"image/png\"/></div>", &mut f), "see ![[a.png]]\n");
    }
}
