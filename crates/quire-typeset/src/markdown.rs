// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Markdown (as Quire writes it) -> Typst markup -> PDF.
//!
//! Every piece of note text is escaped for Typst markup or passed as a Typst
//! string literal, so nothing the user typed is interpreted as Typst syntax.
//! Formulas, diagrams and images are checked on their own first; one that
//! fails is shown as source text instead of failing the whole document.

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::ops::Range;
use std::panic::{AssertUnwindSafe, catch_unwind};

use pulldown_cmark::{Alignment, CodeBlockKind, Event, LinkType, MetadataBlockKind, Options, Parser, Tag, TagEnd};
use typst::foundations::{Bytes, Smart};
use typst::visualize::{ExchangeFormat, RasterImage, SvgImage};

use crate::world::TypesetWorld;
use crate::{MathStyle, TypesetError, diagram_svg, math_svg, typst_string};

const PREAMBLE: &str = include_str!("pdf-preamble.typ");

/// Paper size for [`markdown_pdf`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PageSize {
    /// ISO A4 (210 × 297 mm).
    #[default]
    A4,
    /// US Letter (8.5 × 11 in).
    Letter,
}

/// Loads an image's bytes by the path written in the note.
pub type ImageLoader<'a> = Box<dyn Fn(&str) -> Option<Vec<u8>> + 'a>;

/// Options for [`markdown_pdf`].
pub struct PdfOptions<'a> {
    /// Document title: shown at the top (unless the note starts with the same
    /// `# heading`) and stored in the PDF metadata. A `title` in the note's
    /// YAML front matter takes precedence. May be empty.
    pub title: String,
    /// Loads an embedded image by the path written in the note (`![](path)`
    /// or `![[name]]`). Return `None` when it can't be found; the PDF then
    /// shows the name instead. PNG, JPEG, GIF, WebP and SVG are supported.
    pub load_image: ImageLoader<'a>,
    /// Paper size.
    pub page: PageSize,
}

impl Default for PdfOptions<'_> {
    fn default() -> Self {
        Self { title: String::new(), load_image: Box::new(|_| None), page: PageSize::A4 }
    }
}

impl std::fmt::Debug for PdfOptions<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PdfOptions").field("title", &self.title).field("page", &self.page).finish_non_exhaustive()
    }
}

/// Typesets a Markdown note as a PDF document.
pub fn markdown_pdf(markdown: &str, opts: &PdfOptions<'_>) -> Result<Vec<u8>, TypesetError> {
    let doc = to_typst(markdown, opts);
    let mut world = TypesetWorld::new(doc.source);
    for (path, data) in doc.files {
        world.add_file(&path, data);
    }
    let paged = world.compile()?;
    let options = typst_pdf::PdfOptions { creator: Smart::Custom(Some("Quire".into())), ..typst_pdf::PdfOptions::default() };
    match catch_unwind(AssertUnwindSafe(|| typst_pdf::pdf(&paged, &options))) {
        Ok(Ok(bytes)) => Ok(bytes),
        Ok(Err(errors)) => Err(TypesetError::Internal(format!("PDF export failed: {}", crate::world::describe(&errors)))),
        Err(_) => Err(TypesetError::Internal("PDF export crashed".into())),
    }
}

/// The generated Typst document and the files it references.
pub(crate) struct TypstDoc {
    pub(crate) source: String,
    pub(crate) files: Vec<(String, Bytes)>,
}

/// The pulldown-cmark options Quire uses (see `quire-markdown`), with
/// wikilinks on and GitHub alerts off: callouts are detected here, the
/// Obsidian way (`> [!type] Optional title`).
fn parser_options() -> Options {
    Options::ENABLE_TABLES
        | Options::ENABLE_FOOTNOTES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_YAML_STYLE_METADATA_BLOCKS
        | Options::ENABLE_MATH
        | Options::ENABLE_WIKILINKS
}

/// Obsidian's paragraph ids aren't for print: drop a trailing ` ^id`, and
/// show `[[Note#^id]]` as `Note` and `[[Note#Heading]]` as `Note › Heading`.
/// Code blocks are left alone.
fn tidy_links(markdown: &str) -> std::borrow::Cow<'_, str> {
    if !markdown.contains('^') && !markdown.contains("[[") {
        return std::borrow::Cow::Borrowed(markdown);
    }
    let mut out = String::with_capacity(markdown.len());
    let mut fence: Option<String> = None;
    let mut in_math = false;
    for line in markdown.split_inclusive('\n') {
        let body = line.trim_end_matches(['\n', '\r']);
        let eol = &line[body.len()..];
        let t = body.trim_start();
        if let Some(f) = &fence {
            if t.starts_with(f.as_str()) {
                fence = None;
            }
            out.push_str(line);
            continue;
        }
        if t.starts_with("```") || t.starts_with("~~~") {
            fence = Some(t.chars().take_while(|c| *c == '`' || *c == '~').collect());
            out.push_str(line);
            continue;
        }
        // Display math (`$$` … `$$`) keeps its `^`.
        let dollars = t.matches("$$").count();
        if in_math || dollars % 2 == 1 {
            if dollars % 2 == 1 {
                in_math = !in_math;
            }
            out.push_str(line);
            continue;
        }
        let mut b = body;
        // ` ^id` at the end (letters, digits and `-`), after a space or alone.
        if let Some(caret) = b.trim_end().rfind('^') {
            let id = &b.trim_end()[caret + 1..];
            let before = &b[..caret];
            if !id.is_empty() && id.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-') && (before.is_empty() || before.ends_with(' '))
            {
                b = before.trim_end();
            }
        }
        let mut rest = b;
        while let Some(open) = rest.find("[[") {
            let Some(close) = rest[open + 2..].find("]]") else { break };
            out.push_str(&rest[..open + 2]);
            let inner = &rest[open + 2..open + 2 + close];
            let (target, alias) = inner.split_once('|').map_or((inner, None), |(t, a)| (t, Some(a)));
            match (target.split_once('#'), alias) {
                (Some((note, frag)), None) if !rest[..open].ends_with('!') => {
                    if frag.starts_with('^') || frag.is_empty() {
                        let _ = write!(out, "{note}|{note}");
                    } else {
                        let _ = write!(out, "{note}|{note} › {frag}");
                    }
                }
                _ => out.push_str(inner),
            }
            out.push_str("]]");
            rest = &rest[open + 2 + close + 2..];
        }
        out.push_str(rest);
        out.push_str(eol);
    }
    std::borrow::Cow::Owned(out)
}

pub(crate) fn to_typst(markdown: &str, opts: &PdfOptions<'_>) -> TypstDoc {
    let markdown = &*tidy_links(markdown);
    let events: Vec<(Event<'_>, Range<usize>)> = Parser::new_ext(markdown, parser_options()).into_offset_iter().collect();

    let front_title = front_matter_title(&events);
    let title = front_title.unwrap_or_else(|| opts.title.trim().to_owned());

    let mut renderer = Renderer::new(markdown, opts, &events);
    let body = renderer.render_all();

    let mut source = String::with_capacity(PREAMBLE.len() + body.len() + 256);
    source.push_str("#import \"/mitex.typ\": mitex-math\n");
    let _ = writeln!(source, "#let quire-paper = \"{}\"", if opts.page == PageSize::Letter { "us-letter" } else { "a4" });
    let _ = writeln!(source, "#let quire-title = {}", typst_string(&title));
    source.push_str(PREAMBLE);
    source.push('\n');
    if !title.is_empty() && !first_heading_is(&events, &title) {
        let _ = writeln!(source, "#title-block({})", typst_string(&title));
    }
    source.push_str(&body);
    TypstDoc { source, files: renderer.files }
}

/// `title:` from YAML front matter, if the note starts with some.
fn front_matter_title(events: &[(Event<'_>, Range<usize>)]) -> Option<String> {
    let mut iter = events.iter();
    if !matches!(iter.next(), Some((Event::Start(Tag::MetadataBlock(MetadataBlockKind::YamlStyle)), _))) {
        return None;
    }
    let mut yaml = String::new();
    for (event, _) in iter {
        match event {
            Event::Text(t) => yaml.push_str(t),
            Event::End(TagEnd::MetadataBlock(_)) => break,
            _ => {}
        }
    }
    yaml.lines().find_map(|line| {
        let value = line.strip_prefix("title:")?.trim();
        let value = value
            .strip_prefix('"')
            .and_then(|v| v.strip_suffix('"'))
            .or_else(|| value.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')))
            .unwrap_or(value)
            .trim();
        (!value.is_empty() && value != "|" && value != ">").then(|| value.to_owned())
    })
}

/// Whether the first block (after front matter) is a level-1 heading reading `title`.
fn first_heading_is(events: &[(Event<'_>, Range<usize>)], title: &str) -> bool {
    let mut iter = events.iter().map(|(e, _)| e).skip_while(|e| !matches!(e, Event::Start(_)));
    if matches!(iter.clone().next(), Some(Event::Start(Tag::MetadataBlock(_)))) {
        iter.by_ref().find(|e| matches!(e, Event::End(TagEnd::MetadataBlock(_))));
    }
    let Some(Event::Start(Tag::Heading { level: pulldown_cmark::HeadingLevel::H1, .. })) = iter.next() else {
        return false;
    };
    let mut text = String::new();
    for event in iter {
        match event {
            Event::Text(t) | Event::Code(t) => text.push_str(t),
            Event::End(TagEnd::Heading(_)) => break,
            _ => {}
        }
    }
    text.trim().eq_ignore_ascii_case(title.trim())
}

/// Escapes `text` for Typst markup: every ASCII punctuation character gets a
/// backslash, line breaks become spaces and other control characters are
/// dropped, so the text always comes out literally.
pub(crate) fn escape_markup(text: &str, out: &mut String) {
    for c in text.chars() {
        if c.is_ascii_punctuation() {
            out.push('\\');
            out.push(c);
        } else if c == '\n' || c == '\r' || c == '\t' {
            out.push(' ');
        } else if !c.is_control() {
            out.push(c);
        }
    }
}

fn escaped(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 8);
    escape_markup(text, &mut out);
    out
}

/// What an open Markdown container needs at its end.
enum Open {
    /// Ends with the given Typst text.
    Close(&'static str),
    /// A table header or row: padded to the column count, then closed.
    Row(&'static str),
    /// A table: column count and cells written in the current row.
    Table { columns: usize, cell: usize },
    /// A cell beyond the header's column count: its output is dropped.
    DroppedCell,
    /// A fenced or indented code block (its text is collected).
    Code { lang: String },
    /// An image (its alt text is collected).
    Image { dest: String, wiki_pothole: bool },
    /// A link that is written as a Typst link.
    Link,
    /// A link that is written as plain text.
    PlainLink,
    /// A block quote or callout.
    Quote { skip_until: Option<usize> },
    /// Anything that needs nothing at its end.
    Nothing,
}

struct Renderer<'r, 'a> {
    src: &'r str,
    opts: &'r PdfOptions<'a>,
    events: &'r [(Event<'r>, Range<usize>)],
    /// Output buffers; nested buffers collect text (code, alt text).
    out: Vec<String>,
    open: Vec<Open>,
    files: Vec<(String, Bytes)>,
    images: HashMap<String, Option<(String, f64)>>,
    diagrams: usize,
    /// Footnote label -> event range of its definition (exclusive of Start/End).
    footnote_defs: HashMap<String, Range<usize>>,
    /// Footnote label -> Typst label number, once referenced.
    footnote_ids: HashMap<String, usize>,
    rendering_footnotes: HashSet<String>,
    /// Inline events starting before this source offset are a callout header.
    skip_until: Option<usize>,
}

impl<'r, 'a> Renderer<'r, 'a> {
    fn new(src: &'r str, opts: &'r PdfOptions<'a>, events: &'r [(Event<'r>, Range<usize>)]) -> Self {
        let mut footnote_defs = HashMap::new();
        let mut i = 0;
        while i < events.len() {
            if let Event::Start(Tag::FootnoteDefinition(label)) = &events[i].0 {
                let end = matching_end(events, i);
                footnote_defs.entry(label.to_string()).or_insert(i + 1..end);
                i = end;
            }
            i += 1;
        }
        Self {
            src,
            opts,
            events,
            out: vec![String::new()],
            open: Vec::new(),
            files: Vec::new(),
            images: HashMap::new(),
            diagrams: 0,
            footnote_defs,
            footnote_ids: HashMap::new(),
            rendering_footnotes: HashSet::new(),
            skip_until: None,
        }
    }

    fn w(&mut self) -> &mut String {
        if self.out.is_empty() {
            self.out.push(String::new());
        }
        let last = self.out.len() - 1;
        &mut self.out[last]
    }

    fn render_all(&mut self) -> String {
        self.render_range(0..self.events.len());
        // Close anything left open (pulldown-cmark balances its events, but be safe).
        while let Some(open) = self.open.pop() {
            self.close(open);
        }
        let mut body = std::mem::take(&mut self.out).join("");
        body.push('\n');
        body
    }

    fn render_range(&mut self, range: Range<usize>) {
        let mut i = range.start;
        while i < range.end {
            let (event, span) = &self.events[i];
            match event {
                // Footnotes are rendered where referenced; front matter is metadata.
                Event::Start(Tag::FootnoteDefinition(_) | Tag::MetadataBlock(_)) => {
                    i = matching_end(self.events, i);
                }
                Event::Start(Tag::List(start)) => {
                    let end = matching_end(self.events, i);
                    self.list(*start, i, end);
                    i = end;
                }
                Event::Start(tag) => {
                    if self.skip_inline(tag_is_inline(tag), span) {
                        // Skip the whole inline element of a callout header.
                        i = matching_end(self.events, i);
                    } else {
                        self.start(tag, span);
                    }
                }
                Event::End(_) => {
                    if let Some(open) = self.open.pop() {
                        self.close(open);
                    }
                }
                other => {
                    if !self.skip_inline(true, span) {
                        self.leaf(other);
                    }
                }
            }
            i += 1;
        }
    }

    /// Whether an inline event at `span` belongs to a callout's `[!type] title` line.
    fn skip_inline(&self, inline: bool, span: &Range<usize>) -> bool {
        inline && self.skip_until.is_some_and(|until| span.start <= until)
    }

    fn start(&mut self, tag: &Tag<'_>, span: &Range<usize>) {
        if self.collecting_text() {
            self.open.push(Open::Nothing);
            return;
        }
        let open = match tag {
            Tag::Paragraph | Tag::HtmlBlock => Open::Close("\n\n"),
            Tag::Heading { level, .. } => {
                let _ = write!(self.w(), "\n#heading(level: {})[", *level as usize);
                Open::Close("]\n")
            }
            Tag::BlockQuote(_) => self.quote(span),
            Tag::CodeBlock(kind) => {
                let lang = match kind {
                    CodeBlockKind::Fenced(info) => info.split_whitespace().next().unwrap_or("").to_owned(),
                    CodeBlockKind::Indented => String::new(),
                };
                self.out.push(String::new());
                Open::Code { lang }
            }
            Tag::Item => {
                self.w().push('[');
                Open::Close("]")
            }
            Tag::Table(aligns) => {
                let aligns: Vec<&str> = aligns
                    .iter()
                    .map(|a| match a {
                        Alignment::Center => "center",
                        Alignment::Right => "right",
                        Alignment::None | Alignment::Left => "left",
                    })
                    .collect();
                let _ = write!(self.w(), "\n#md-table(({},), ", aligns.join(", "));
                Open::Table { columns: aligns.len(), cell: 0 }
            }
            Tag::TableHead => {
                self.w().push_str("table.header(");
                self.reset_row();
                Open::Row("), ")
            }
            Tag::TableRow => {
                self.reset_row();
                Open::Row("")
            }
            Tag::TableCell => {
                if self.next_cell_fits() {
                    self.w().push('[');
                    Open::Close("], ")
                } else {
                    self.out.push(String::new());
                    Open::DroppedCell
                }
            }
            Tag::Emphasis => {
                self.w().push_str("#emph[");
                Open::Close("]")
            }
            Tag::Strong => {
                self.w().push_str("#strong[");
                Open::Close("]")
            }
            Tag::Strikethrough => {
                self.w().push_str("#strike[");
                Open::Close("]")
            }
            Tag::Superscript => {
                self.w().push_str("#super[");
                Open::Close("]")
            }
            Tag::Subscript => {
                self.w().push_str("#sub[");
                Open::Close("]")
            }
            Tag::Link { link_type, dest_url, .. } => {
                if matches!(link_type, LinkType::Email) && !dest_url.contains(':') {
                    let _ = write!(self.w(), "#link({})[", typst_string(&format!("mailto:{dest_url}")));
                    Open::Link
                } else if !matches!(link_type, LinkType::WikiLink { .. }) && is_external(dest_url) {
                    let _ = write!(self.w(), "#link({})[", typst_string(dest_url));
                    Open::Link
                } else {
                    Open::PlainLink
                }
            }
            Tag::Image { link_type, dest_url, .. } => {
                self.out.push(String::new());
                Open::Image { dest: dest_url.to_string(), wiki_pothole: matches!(link_type, LinkType::WikiLink { has_pothole: true }) }
            }
            Tag::List(_)
            | Tag::FootnoteDefinition(_)
            | Tag::MetadataBlock(_)
            | Tag::DefinitionList
            | Tag::DefinitionListTitle
            | Tag::DefinitionListDefinition => Open::Nothing,
        };
        self.open.push(open);
    }

    fn reset_row(&mut self) {
        if let Some(Open::Table { cell, .. }) = self.open.iter_mut().rev().find(|o| matches!(o, Open::Table { .. })) {
            *cell = 0;
        }
    }

    /// Counts a new cell; `false` when the row already has all its columns.
    fn next_cell_fits(&mut self) -> bool {
        if let Some(Open::Table { columns, cell }) = self.open.iter_mut().rev().find(|o| matches!(o, Open::Table { .. })) {
            *cell += 1;
            *cell <= *columns
        } else {
            true
        }
    }

    /// Pads the current row with empty cells.
    fn pad_row(&mut self) {
        let missing = match self.open.iter().rev().find(|o| matches!(o, Open::Table { .. })) {
            Some(Open::Table { columns, cell }) => columns.saturating_sub(*cell),
            _ => 0,
        };
        for _ in 0..missing {
            self.w().push_str("[], ");
        }
    }

    fn close(&mut self, open: Open) {
        match open {
            Open::Close(text) => self.w().push_str(text),
            Open::Row(text) => {
                self.pad_row();
                self.w().push_str(text);
            }
            Open::Table { .. } => self.w().push_str(")\n"),
            Open::DroppedCell => {
                self.out.pop();
            }
            Open::Code { lang } => {
                let code = self.out.pop().unwrap_or_default();
                self.code_block(&lang, &code);
            }
            Open::Image { dest, wiki_pothole } => {
                let alt = self.out.pop().unwrap_or_default();
                self.image(&dest, &alt, wiki_pothole);
            }
            Open::Link => self.w().push(']'),
            Open::Quote { skip_until } => {
                if skip_until.is_some() {
                    self.skip_until = None;
                }
                self.w().push_str("]\n");
            }
            Open::PlainLink | Open::Nothing => {}
        }
    }

    /// Inside a code block or an image's alt text, only plain text is collected.
    fn collecting_text(&self) -> bool {
        self.open.iter().any(|o| matches!(o, Open::Code { .. } | Open::Image { .. }))
    }

    fn leaf(&mut self, event: &Event<'_>) {
        if self.collecting_text() {
            if let Event::Text(t) | Event::Code(t) | Event::InlineMath(t) | Event::DisplayMath(t) = event {
                self.w().push_str(t);
            } else if matches!(event, Event::SoftBreak | Event::HardBreak) {
                self.w().push(' ');
            }
            return;
        }
        match event {
            Event::Text(t) => escape_markup(t, self.w()),
            Event::Code(t) => {
                let lit = typst_string(t);
                let _ = write!(self.w(), "#raw({lit})");
            }
            Event::InlineMath(t) => self.math(t, false),
            Event::DisplayMath(t) => self.math(t, true),
            Event::Html(html) => self.w().push_str(&escaped(&strip_tags(html))),
            Event::InlineHtml(html) => {
                if is_br(html) {
                    self.w().push_str("#linebreak()");
                }
            }
            Event::FootnoteReference(label) => self.footnote(label),
            Event::SoftBreak => self.w().push(' '),
            Event::HardBreak => self.w().push_str("#linebreak()"),
            Event::Rule => self.w().push_str("\n#md-rule()\n"),
            Event::TaskListMarker(checked) => {
                let _ = write!(self.w(), "#checkbox({checked})");
            }
            Event::Start(_) | Event::End(_) => {}
        }
    }

    /// A block quote, or a callout when its first line is `[!type] Optional title`.
    fn quote(&mut self, span: &Range<usize>) -> Open {
        let start = span.start.min(self.src.len());
        let line_end = self.src[start..].find('\n').map_or(span.end, |n| start + n).min(self.src.len());
        let line = &self.src[start..line_end];
        if let Some((kind, title)) = parse_callout(line) {
            let _ = write!(self.w(), "\n#callout({}, {})[", typst_string(&kind), typst_string(&title));
            self.skip_until = Some(line_end);
            Open::Quote { skip_until: Some(line_end) }
        } else {
            self.w().push_str("\n#md-quote[");
            Open::Quote { skip_until: None }
        }
    }

    /// A list, rendered as a Typst `list`/`enum` call with one content block per item.
    fn list(&mut self, start: Option<u64>, open_index: usize, end_index: usize) {
        let (tight, has_tasks) = list_shape(self.events, open_index, end_index);
        let w = self.w();
        w.push('\n');
        if let Some(n) = start {
            let _ = write!(w, "#enum(start: {n}, tight: {tight})");
        } else if has_tasks {
            let _ = write!(w, "#task-list({tight})");
        } else {
            let _ = write!(w, "#list(tight: {tight})");
        }
        // Items: render each child range, adding a bullet to non-task items of task lists.
        let mut i = open_index + 1;
        while i < end_index {
            if matches!(self.events[i].0, Event::Start(Tag::Item)) {
                let item_end = matching_end(self.events, i);
                self.w().push('[');
                if has_tasks && start.is_none() && !item_has_task_marker(self.events, i, item_end) {
                    self.w().push_str("#(task-bullet)");
                }
                self.open.push(Open::Nothing);
                self.render_range(i + 1..item_end);
                self.open.pop();
                let w = self.w();
                while w.ends_with(['\n', ' ']) {
                    w.pop();
                }
                w.push(']');
                i = item_end;
            }
            i += 1;
        }
        self.w().push('\n');
    }

    fn code_block(&mut self, lang: &str, code: &str) {
        if lang.eq_ignore_ascii_case("mermaid") {
            match self.diagram(code) {
                Ok(()) => return,
                Err(e) => {
                    let _ = write!(self.w(), "\n#diagram-error({})\n", typst_string(&e.to_string()));
                }
            }
        }
        let code = code.strip_suffix('\n').unwrap_or(code);
        let lang = lang.trim_start_matches('{').trim_end_matches('}');
        let lang = lang.split([',', '{']).next().unwrap_or("");
        if lang.is_empty() {
            let _ = write!(self.w(), "\n#raw(block: true, {})\n", typst_string(code));
        } else {
            let _ = write!(self.w(), "\n#raw(block: true, lang: {}, {})\n", typst_string(lang), typst_string(code));
        }
    }

    fn diagram(&mut self, code: &str) -> Result<(), TypesetError> {
        let svg = diagram_svg(code, false)?;
        let data = Bytes::new(svg.into_bytes());
        let image = catch_unwind(AssertUnwindSafe(|| SvgImage::new(data.clone())))
            .map_err(|_| TypesetError::Diagram("unreadable SVG".into()))?
            .map_err(|_| TypesetError::Diagram("unreadable SVG".into()))?;
        let path = format!("/diagram/{}.svg", self.diagrams);
        self.diagrams += 1;
        let natural = image.width() * 0.75;
        self.files.push((path.clone(), data));
        let _ = write!(self.w(), "\n#md-diagram({}, {natural:.2}pt)\n", typst_string(&path));
        Ok(())
    }

    fn math(&mut self, latex: &str, display: bool) {
        // Each formula is checked on its own (and cached), so one bad formula
        // can't break the document.
        let checked = math_svg(latex, display, &MathStyle::default()).and_then(|_| crate::math::latex_to_typst(latex));
        if let Ok(code) = checked {
            let _ = write!(self.w(), "#mitex-math({}, block: {display})", typst_string(&code));
        } else {
            let shown = if display { format!("$${latex}$$") } else { format!("${latex}$") };
            let _ = write!(self.w(), "#math-error({})", typst_string(&shown));
        }
    }

    fn image(&mut self, dest: &str, alt: &str, wiki_pothole: bool) {
        // `![[image.png|300]]` sets a width in pixels.
        let width_override = if wiki_pothole { parse_wiki_size(alt) } else { None };
        let alt = if wiki_pothole && width_override.is_some() { "" } else { alt };
        if let Some((path, natural)) = self.load_image(dest) {
            let natural = width_override.unwrap_or(natural);
            let _ = write!(self.w(), "#md-image({}, {natural:.2}pt, alt: {})", typst_string(&path), typst_string(alt));
        } else {
            let name = if alt.trim().is_empty() { dest } else { alt };
            let _ = write!(self.w(), "#missing-image({})", typst_string(name));
        }
    }

    /// Loads, checks and registers an image; returns its virtual path and natural width in pt.
    fn load_image(&mut self, dest: &str) -> Option<(String, f64)> {
        if let Some(known) = self.images.get(dest) {
            return known.clone();
        }
        let loaded = (self.opts.load_image)(dest).or_else(|| {
            let decoded = percent_decode(dest);
            (decoded != dest).then(|| (self.opts.load_image)(&decoded)).flatten()
        });
        let result = loaded.and_then(|bytes| {
            let data = Bytes::new(bytes);
            let checked = catch_unwind(AssertUnwindSafe(|| check_image(&data))).ok().flatten();
            checked.map(|(ext, natural)| {
                let path = format!("/img/{}.{ext}", self.files.len());
                self.files.push((path.clone(), data));
                (path, natural)
            })
        });
        self.images.insert(dest.to_owned(), result.clone());
        result
    }

    fn footnote(&mut self, label: &str) {
        if let Some(&id) = self.footnote_ids.get(label) {
            let _ = write!(self.w(), "#footnote(<quire-fn-{id}>)");
            return;
        }
        let Some(range) = self.footnote_defs.get(label).cloned() else {
            let _ = write!(self.w(), "{}", escaped(&format!("[^{label}]")));
            return;
        };
        if !self.rendering_footnotes.insert(label.to_owned()) {
            // A footnote that (indirectly) references itself.
            let _ = write!(self.w(), "{}", escaped(&format!("[^{label}]")));
            return;
        }
        let id = self.footnote_ids.len();
        self.footnote_ids.insert(label.to_owned(), id);
        // Render the definition into its own buffer with fresh container state.
        let saved_open = std::mem::take(&mut self.open);
        let saved_skip = self.skip_until.take();
        self.out.push(String::new());
        self.render_range(range);
        while let Some(open) = self.open.pop() {
            self.close(open);
        }
        let content = self.out.pop().unwrap_or_default();
        self.open = saved_open;
        self.skip_until = saved_skip;
        self.rendering_footnotes.remove(label);
        let _ = write!(self.w(), "#footnote[{}]<quire-fn-{id}>", content.trim());
    }
}

/// Inline tags (whose events can belong to a callout header line).
fn tag_is_inline(tag: &Tag<'_>) -> bool {
    matches!(
        tag,
        Tag::Emphasis | Tag::Strong | Tag::Strikethrough | Tag::Superscript | Tag::Subscript | Tag::Link { .. } | Tag::Image { .. }
    )
}

/// Index of the `End` matching the `Start` at `start` (or the last index).
fn matching_end(events: &[(Event<'_>, Range<usize>)], start: usize) -> usize {
    let mut depth = 0usize;
    for (i, (event, _)) in events.iter().enumerate().skip(start) {
        match event {
            Event::Start(_) => depth += 1,
            Event::End(_) => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return i;
                }
            }
            _ => {}
        }
    }
    events.len().saturating_sub(1)
}

/// (tight, has task items) for the list whose `Start`/`End` are at the given indices.
fn list_shape(events: &[(Event<'_>, Range<usize>)], start: usize, end: usize) -> (bool, bool) {
    let mut depth = 0usize;
    let mut tight = true;
    let mut tasks = false;
    for (event, _) in &events[start + 1..end] {
        match event {
            Event::Start(Tag::Paragraph) if depth == 1 => tight = false,
            Event::TaskListMarker(_) if depth <= 2 => tasks = true,
            Event::Start(_) => depth += 1,
            Event::End(_) => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    (tight, tasks)
}

fn item_has_task_marker(events: &[(Event<'_>, Range<usize>)], start: usize, end: usize) -> bool {
    let mut depth = 0usize;
    for (event, _) in &events[start + 1..end] {
        match event {
            Event::TaskListMarker(_) if depth <= 1 => return true,
            Event::Start(_) => depth += 1,
            Event::End(_) => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    false
}

/// `[!type]` + optional fold marker + optional title from a block quote's first line.
fn parse_callout(line: &str) -> Option<(String, String)> {
    let line = line.trim_start().strip_prefix('>')?.trim_start();
    let rest = line.strip_prefix("[!")?;
    let close = rest.find(']')?;
    let kind = &rest[..close];
    if kind.is_empty() || !kind.chars().all(|c| c.is_alphanumeric() || c == '-' || c == '_') {
        return None;
    }
    let kind = kind.to_lowercase();
    let title = rest[close + 1..].trim_start_matches(['+', '-']).trim();
    let title = if title.is_empty() {
        let mut chars = kind.chars();
        chars.next().map(|c| c.to_uppercase().chain(chars).collect()).unwrap_or_default()
    } else {
        unwiki(title)
    };
    Some((kind, title))
}

/// `[[note|alias]]` -> `alias`, `[[note]]` -> `note` (for plain-text titles).
fn unwiki(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(open) = rest.find("[[") {
        let Some(close) = rest[open + 2..].find("]]") else { break };
        out.push_str(&rest[..open]);
        let inner = &rest[open + 2..open + 2 + close];
        out.push_str(inner.rsplit('|').next().unwrap_or(inner));
        rest = &rest[open + 2 + close + 2..];
    }
    out.push_str(rest);
    out
}

fn is_external(url: &str) -> bool {
    let lower = url.trim_start().to_ascii_lowercase();
    ["http://", "https://", "mailto:", "ftp://"].iter().any(|scheme| lower.starts_with(scheme))
}

fn is_br(html: &str) -> bool {
    let tag: String = html.chars().filter(|c| !c.is_whitespace() && *c != '/').collect::<String>().to_ascii_lowercase();
    tag == "<br>"
}

/// Text content of an HTML fragment (tags and comments removed).
fn strip_tags(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut rest = html;
    while let Some(lt) = rest.find('<') {
        out.push_str(&rest[..lt]);
        let after = &rest[lt..];
        let end = if after.starts_with("<!--") { after.find("-->").map(|e| e + 3) } else { after.find('>').map(|e| e + 1) };
        if let Some(end) = end {
            rest = &after[end..];
        } else {
            out.push_str(after);
            rest = "";
        }
    }
    out.push_str(rest);
    out.trim().to_owned()
}

/// `300` or `300x200` (pixels) -> width in points.
fn parse_wiki_size(spec: &str) -> Option<f64> {
    let width = spec.trim().split('x').next()?.trim();
    let px: f64 = width.parse().ok()?;
    (px > 0.0 && px < 100_000.0).then_some(px * 0.75)
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Ok(v) = u8::from_str_radix(s.get(i + 1..i + 3).unwrap_or(""), 16)
        {
            out.push(v);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8(out).unwrap_or_else(|_| s.to_owned())
}

/// The file extension Typst should see and the natural width in points, if
/// `data` is an image Typst can draw.
fn check_image(data: &Bytes) -> Option<(&'static str, f64)> {
    if let Some(format) = ExchangeFormat::detect(data) {
        let image = RasterImage::new(data.clone(), format, Smart::Auto).ok()?;
        let dpi = image.dpi().filter(|d| d.is_finite() && *d > 0.0).unwrap_or(72.0);
        let ext = match format {
            ExchangeFormat::Png => "png",
            ExchangeFormat::Jpg => "jpg",
            ExchangeFormat::Gif => "gif",
            ExchangeFormat::Webp => "webp",
        };
        return Some((ext, f64::from(image.width()) * 72.0 / dpi));
    }
    let head = &data[..data.len().min(1024)];
    let text = String::from_utf8_lossy(head);
    if text.contains("<svg") {
        let image = SvgImage::new(data.clone()).ok()?;
        return Some(("svg", image.width() * 0.75));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use typst::layout::{Frame, FrameItem};

    fn collect_text(frame: &Frame, out: &mut String) {
        for (_, item) in frame.items() {
            match item {
                FrameItem::Group(group) => collect_text(&group.frame, out),
                FrameItem::Text(text) => out.push_str(&text.text),
                _ => {}
            }
        }
    }

    fn compiled_text(source: String, files: Vec<(String, Bytes)>) -> String {
        let mut world = TypesetWorld::new(source);
        for (path, data) in files {
            world.add_file(&path, data);
        }
        let doc = world.compile().unwrap();
        let mut text = String::new();
        for page in doc.pages() {
            collect_text(&page.frame, &mut text);
        }
        text.split_whitespace().collect()
    }

    fn squash(s: &str) -> String {
        s.split_whitespace().collect()
    }

    #[test]
    fn paragraph_ids_and_fragments_are_tidied_for_print() {
        let md = "See [[Note#^abc]] and [[Other#Part two]] and ![[pic.png]]. ^para1\n\n```\ncode ^keep\n```\n";
        let t = tidy_links(md);
        assert!(t.contains("[[Note|Note]]") && t.contains("[[Other|Other › Part two]]") && t.contains("![[pic.png]]"), "{t}");
        assert!(!t.contains("^para1") && t.contains("code ^keep"), "{t}");
        let m = tidy_links("$$\nx = y ^2\n$$\nafter ^id\n");
        assert!(m.contains("y ^2") && !m.contains("^id"), "{m}");
    }

    #[test]
    fn escaped_text_comes_out_literally() {
        let nasty = [
            r#"# $ * _ @ < > [ ] ` \ ~ / = - + . ' " ^ { } ( ) ; : | ! ? % &"#,
            r"\\ \ \n #let x = 1 #import \u{1F600} \u{",
            r#"#set page(width: 1pt) #read("/etc/passwd") #include "x.typ""#,
            "// line comment /* block comment */ https://example.com <label> @ref",
            "= heading - list + enum / Term: x 1. 2) -- --- ... -? ~nbsp",
            "]]]]] [[[[ ))))) ((((( }}}}} {{{{{",
            "*strong* _emph_ `raw` $math$ $$block$$",
            "text#strong[x](y).field;more",
            // (No CJK here: whether it has glyphs depends on the machine's fallback fonts.)
            "Ünïcödé — “curly” «quotes» emoji 😀",
        ];
        for s in nasty {
            let source = format!("#set page(width: 400pt, height: auto)\n{}", escaped(s));
            assert_eq!(compiled_text(source, Vec::new()), squash(s), "{s}");
        }
    }

    #[test]
    fn markdown_text_comes_out_literally() {
        // Markdown that pulldown-cmark passes through as plain text.
        let md = "Costs 5 $ or #3 @home = [x] / {y} ~z + 1. 'a' \"b\" ^c | d; e: f % g & h";
        let doc = to_typst(md, &PdfOptions::default());
        let text = compiled_text(doc.source, doc.files);
        assert!(text.contains(&squash(md)), "{text}");

        let md = "> [!warning] Careful ] [ # $\n> Body text\n\n[[Some Note|the alias]] and [[Plain Name]]";
        let doc = to_typst(md, &PdfOptions::default());
        let text = compiled_text(doc.source, doc.files);
        assert!(text.contains("Careful][#$"), "{text}");
        assert!(text.contains("Bodytext"), "{text}");
        assert!(!text.contains("[!warning]"), "{text}");
        assert!(text.contains("thealiasandPlainName"), "{text}");

        let md = "- [ ] open\n- [x] done\n- plain (item)\n\n1. one\n2. [two]";
        let doc = to_typst(md, &PdfOptions::default());
        let text = compiled_text(doc.source, doc.files);
        assert!(text.contains("open") && text.contains("done") && text.contains("plain(item)"), "{text}");
        assert!(text.contains("[two]"), "{text}");
    }

    #[test]
    fn callout_headers() {
        assert_eq!(parse_callout("> [!note]"), Some(("note".into(), "Note".into())));
        assert_eq!(parse_callout("> [!WARNING] Mind the gap"), Some(("warning".into(), "Mind the gap".into())));
        assert_eq!(parse_callout(">[!tip]- Folded"), Some(("tip".into(), "Folded".into())));
        assert_eq!(parse_callout("> [!my-type]"), Some(("my-type".into(), "My-type".into())));
        assert_eq!(parse_callout("> just a quote"), None);
        assert_eq!(parse_callout("> [!] nope"), None);
        assert_eq!(parse_callout("> [!a b] nope"), None);
        assert_eq!(parse_callout("> [!info] See [[Note|this]] and [[Other]]"), Some(("info".into(), "See this and Other".into())));
        assert_eq!(unwiki("[[unclosed"), "[[unclosed");
    }

    #[test]
    fn escaping_covers_markup_syntax() {
        let nasty = r#"# $x$ *b* _i_ @ref <lbl> [a] `c` \ ~ // /* = - + 1. "q" 'q' ... -- -? ^ { } ( ) ; : https://x.y"#;
        let out = escaped(nasty);
        // Every punctuation character is the second half of a `\x` pair.
        let mut chars = out.chars();
        while let Some(c) = chars.next() {
            if c == '\\' {
                assert!(chars.next().is_some_and(|n| n.is_ascii_punctuation()));
            } else {
                assert!(!c.is_ascii_punctuation(), "unescaped {c:?} in {out}");
            }
        }
        assert!(out.starts_with("\\# \\$x\\$"));
        assert_eq!(escaped("a\nb\u{0}c\td"), "a bc d");
    }

    #[test]
    fn front_matter_titles() {
        let md = "---\ntitle: \"Hello: world\"\ntags: [a]\n---\n\n# Other\n";
        let events: Vec<_> = Parser::new_ext(md, parser_options()).into_offset_iter().collect();
        assert_eq!(front_matter_title(&events).as_deref(), Some("Hello: world"));
        assert!(!first_heading_is(&events, "Hello: world"));
        assert!(first_heading_is(&events, "other"));
    }

    #[test]
    fn helpers() {
        assert_eq!(percent_decode("my%20pic.png"), "my pic.png");
        assert_eq!(percent_decode("100%"), "100%");
        assert_eq!(percent_decode("%zz%2"), "%zz%2");
        assert_eq!(parse_wiki_size("300"), Some(225.0));
        assert_eq!(parse_wiki_size("300x200"), Some(225.0));
        assert_eq!(parse_wiki_size("alias"), None);
        assert_eq!(strip_tags("<div>hi <b>there</b></div><!-- x -->"), "hi there");
        assert!(is_br("<br/>") && is_br("<BR >") && !is_br("<b>"));
        assert!(is_external("https://example.com") && !is_external("notes/a.md"));
    }
}
