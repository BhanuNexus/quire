// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Excalidraw drawings as editable files (0.13, docs/formats/excalidraw.md).
//!
//! A [`Drawing`] is a `.excalidraw` file (the scene's JSON) or an
//! `.excalidraw.md` note (the Obsidian plugin's form: Markdown with
//! `## Text Elements`, `## Element Links`, `## Embedded Files` and the
//! scene under `## Drawing`, plain or LZ-string compressed).
//!
//! Fidelity: elements are kept as ordered JSON objects ([`crate::ojson`]),
//! so every field Quire doesn't know stays as it was, in its place; an
//! element Quire doesn't change is written back byte for byte. Saving keeps
//! the file's form (JSON or note, compressed or plain, its indent), and a
//! note's own text above the drawing and its sections are kept; the
//! `## Text Elements` and `## Element Links` sections are written again
//! only when the texts or links changed.
//!
//! Edits follow Excalidraw's own rules ([`Cx::bump`]): each changed element
//! gets `version + 1`, a new random `versionNonce` and `updated` set to
//! now; new elements get a random `seed`; the fractional `index` stays in
//! array order.

use std::fmt::Write as _;
use std::sync::Arc;

use crate::excalidraw::{self, Element};
use crate::ojson::{self, Doc, Indent, Json, Obj};

/// An element: its JSON object, shared between undo steps.
pub type El = Arc<Obj>;

/// What is undone and redone: the elements, the scene's `files` and a
/// note's embedded files.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct State {
    pub elements: Vec<El>,
    pub files: Arc<Obj>,
    /// `## Embedded Files` added since the note was read: (file id, link).
    pub embedded: Arc<Vec<(String, String)>>,
}

/// Where the drawing's JSON lives.
#[derive(Debug, Clone, PartialEq)]
enum Form {
    /// A `.excalidraw` file: the JSON is the file.
    Json,
    /// An `.excalidraw.md` note.
    Note(Note),
}

/// The fenced block's encoding in a note.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Encoding {
    /// ```` ```json ````
    Plain,
    /// ```` ```compressed-json ````: LZ-string, base64, 256-character lines.
    Compressed,
}

#[derive(Debug, Clone, PartialEq)]
struct Section {
    /// The heading line, without its newline (`## Text Elements`).
    title: String,
    /// Everything after the heading line up to the next section.
    body: String,
}

#[derive(Debug, Clone, PartialEq)]
struct Note {
    /// The note's own text, up to `# Excalidraw Data` (or up to the drawing).
    head: String,
    /// `# Excalidraw Data` and the blank lines after it.
    data_heading: String,
    sections: Vec<Section>,
    /// `%%` lines between the sections and `## Drawing`.
    pre_drawing: String,
    /// `## Drawing` up to and including the fence's line (```` ```json\n ````).
    drawing_open: String,
    encoding: Encoding,
    /// The fenced block's text as read.
    body: String,
    /// The closing fence and everything after it.
    tail: String,
}

/// A drawing being edited.
#[derive(Debug, Clone)]
pub struct Drawing {
    form: Form,
    /// The scene's JSON document; its `elements` and `files` are replaced
    /// by [`Self::state`]'s when written.
    doc: Doc,
    pub state: State,
    /// The scene's JSON as read (to keep an unedited drawing byte for byte).
    json_read: String,
    texts_read: String,
    links_read: String,
    /// Embedded files in the note: (file id, link target), as read.
    embedded_read: Vec<(String, String)>,
}

// ---------------------------------------------------------------- reading

/// The line ranges of `text`: (start, end without the newline, next line's start).
fn lines(text: &str) -> impl Iterator<Item = (usize, usize, usize)> + '_ {
    let mut at = 0;
    std::iter::from_fn(move || {
        if at >= text.len() {
            return None;
        }
        let start = at;
        let end = text[start..].find('\n').map_or(text.len(), |i| start + i);
        at = (end + 1).min(text.len() + 1);
        let next = end.saturating_add(1).min(text.len());
        Some((start, end, next))
    })
}

impl Drawing {
    /// Read a drawing: `.excalidraw` JSON or an `.excalidraw.md` note.
    pub fn parse(text: &str) -> Result<Self, String> {
        if text.len() > excalidraw::limits::MAX_TEXT {
            return Err(excalidraw::TOO_LARGE.into());
        }
        if text.trim_start().starts_with('{') {
            return Self::from_json(Form::Json, text);
        }
        let note = parse_note(text)?;
        let json = match note.encoding {
            Encoding::Plain => note.body.clone(),
            Encoding::Compressed => match excalidraw::lz_unpack_base64(&note.body) {
                Ok(j) => j,
                Err(excalidraw::Lz::TooLarge) => return Err(excalidraw::TOO_LARGE.into()),
                Err(excalidraw::Lz::Invalid) => return Err("the compressed drawing can't be read".into()),
            },
        };
        let embedded = embedded_files(&note);
        let mut d = Self::from_json(Form::Note(note), &json)?;
        d.embedded_read = embedded;
        Ok(d)
    }

    fn from_json(form: Form, json: &str) -> Result<Self, String> {
        let doc = Doc::parse(json).map_err(|e| format!("the drawing can't be read: {e}"))?;
        let root = doc.value.as_obj().ok_or("the drawing isn't a JSON object")?;
        let elements: Vec<El> = root
            .arr("elements")
            .ok_or("the drawing has no elements")?
            .iter()
            .map(|e| e.as_obj().cloned().map(Arc::new).ok_or("an element isn't an object"))
            .collect::<Result<_, _>>()?;
        let points: usize = elements.iter().filter_map(|e| e.arr("points")).map(<[Json]>::len).sum();
        if elements.len() > excalidraw::limits::MAX_ELEMENTS || points > excalidraw::limits::MAX_POINTS {
            return Err(excalidraw::TOO_LARGE.into());
        }
        let files = Arc::new(root.obj("files").cloned().unwrap_or_default());
        let state = State { elements, files, embedded: Arc::default() };
        let mut d = Self {
            form,
            doc,
            state,
            json_read: json.to_string(),
            texts_read: String::new(),
            links_read: String::new(),
            embedded_read: Vec::new(),
        };
        d.texts_read = d.text_elements_md();
        d.links_read = d.element_links_md();
        Ok(d)
    }

    /// A new, empty drawing. `note`: as an `.excalidraw.md` note (compressed,
    /// as the Obsidian plugin writes by default), else a `.excalidraw` file.
    pub fn new_empty(note: bool) -> Self {
        let i = if note { "\t" } else { "  " };
        let json = format!(
            "{{\n{i}\"type\": \"excalidraw\",\n{i}\"version\": 2,\n{i}\"source\": \"https://github.com/BhanuNexus/quire\",\n{i}\"elements\": [],\n{i}\"appState\": {{\n{i}{i}\"gridSize\": 20,\n{i}{i}\"viewBackgroundColor\": \"#ffffff\"\n{i}}},\n{i}\"files\": {{}}\n}}"
        );
        if !note {
            return Self::from_json(Form::Json, &format!("{json}\n")).expect("a fixed drawing reads");
        }
        let text = format!(
            "{NOTE_HEAD}# Excalidraw Data\n\n## Text Elements\n%%\n## Drawing\n```compressed-json\n{}\n```\n%%",
            excalidraw::lz_compress_note(&json)
        );
        Self::parse(&text).expect("a fixed drawing reads")
    }

    /// Whether it is an `.excalidraw.md` note.
    pub const fn is_note(&self) -> bool {
        matches!(self.form, Form::Note(_))
    }

    /// The note's encoding (`None` for a `.excalidraw` file).
    pub fn encoding(&self) -> Option<Encoding> {
        match &self.form {
            Form::Note(n) => Some(n.encoding),
            Form::Json => None,
        }
    }

    /// `appState.viewBackgroundColor`.
    pub fn background(&self) -> Option<excalidraw::Rgba> {
        self.doc.value.as_obj()?.obj("appState")?.str("viewBackgroundColor").and_then(excalidraw::Rgba::parse)
    }

    /// Pictures by file id: the scene's `files` (data URLs) and a note's `## Embedded Files`.
    pub fn file_ref(&self, id: &str) -> Option<excalidraw::FileRef> {
        if let Some(url) = self.state.files.obj(id).and_then(|f| f.str("dataURL")) {
            return Some(excalidraw::FileRef::DataUrl(url.to_string()));
        }
        self.embedded_read
            .iter()
            .chain(self.state.embedded.iter())
            .find(|(i, _)| i == id)
            .map(|(_, l)| excalidraw::FileRef::Vault(l.clone()))
    }

    /// The scene as a picture model (for `excalidraw::to_svg` and the loaders).
    pub fn scene(&self) -> excalidraw::Scene {
        let elements = self.state.elements.iter().filter_map(|e| to_element(e)).collect();
        let mut files = std::collections::BTreeMap::new();
        for (id, f) in &self.state.files.0 {
            if let Some(u) = f.as_obj().and_then(|f| f.str("dataURL")) {
                files.insert(id.clone(), excalidraw::FileRef::DataUrl(u.to_string()));
            }
        }
        for (id, l) in self.embedded_read.iter().chain(self.state.embedded.iter()) {
            files.entry(id.clone()).or_insert_with(|| excalidraw::FileRef::Vault(l.clone()));
        }
        excalidraw::Scene { elements, background: self.background(), files }
    }

    // ------------------------------------------------------------ writing

    /// The scene's JSON now.
    pub fn json(&self) -> String {
        let mut doc = self.doc.clone();
        if let Some(root) = doc.value.as_obj_mut() {
            root.set("elements", Json::Arr(self.state.elements.iter().map(|e| Json::Obj((**e).clone())).collect()));
            if root.get("files").is_some() || !self.state.files.0.is_empty() {
                root.set("files", Json::Obj((*self.state.files).clone()));
            }
        }
        doc.write()
    }

    /// The file's text now, in its own form.
    pub fn to_text(&self) -> String {
        let json = self.json();
        let Form::Note(n) = &self.form else { return json };
        let mut sections = n.sections.clone();
        let texts = self.text_elements_md();
        if texts != self.texts_read {
            set_section(&mut sections, "## Text Elements", texts, 0);
        }
        let links = self.element_links_md();
        if links != self.links_read {
            if links.is_empty() {
                sections.retain(|s| s.title.trim_end() != "## Element Links");
            } else {
                let at = sections.iter().position(|s| s.title.trim_end() == "## Text Elements").map_or(0, |i| i + 1);
                set_section(&mut sections, "## Element Links", links, at);
            }
        }
        if !self.state.embedded.is_empty() {
            let add = self.state.embedded.iter().fold(String::new(), |mut s, (id, l)| {
                let _ = write!(s, "{id}: [[{l}]]\n\n");
                s
            });
            match sections.iter_mut().find(|s| s.title.trim_end() == "## Embedded Files") {
                Some(s) => {
                    if !s.body.ends_with("\n\n") && !s.body.is_empty() {
                        s.body.push('\n');
                    }
                    s.body.push_str(&add);
                }
                None => sections.push(Section { title: "## Embedded Files".into(), body: add }),
            }
        }
        let body = if json == self.json_read {
            n.body.clone()
        } else {
            match n.encoding {
                Encoding::Plain => format!("{json}\n"),
                Encoding::Compressed => format!("{}\n", excalidraw::lz_compress_note(&json)),
            }
        };
        let mut out = String::with_capacity(n.head.len() + body.len() + 256);
        out.push_str(&n.head);
        out.push_str(&n.data_heading);
        for s in &sections {
            out.push_str(&s.title);
            out.push('\n');
            out.push_str(&s.body);
        }
        out.push_str(&n.pre_drawing);
        out.push_str(&n.drawing_open);
        out.push_str(&body);
        out.push_str(&n.tail);
        out
    }

    /// `## Text Elements`, as the plugin writes it: each text and ` ^id`.
    fn text_elements_md(&self) -> String {
        let mut s = String::new();
        for e in live(&self.state.elements).filter(|e| e.str("type") == Some("text")) {
            let text = e.str("rawText").or_else(|| e.str("originalText")).or_else(|| e.str("text")).unwrap_or_default();
            s.push_str(text);
            s.push_str(" ^");
            s.push_str(e.str("id").unwrap_or_default());
            s.push_str("\n\n");
        }
        s
    }

    /// `## Element Links`: `id: link` for each element with a link.
    fn element_links_md(&self) -> String {
        let mut s = String::new();
        for e in live(&self.state.elements) {
            if let Some(l) = e.str("link").filter(|l| !l.trim().is_empty()) {
                s.push_str(e.str("id").unwrap_or_default());
                s.push_str(": ");
                s.push_str(l);
                s.push_str("\n\n");
            }
        }
        s
    }

    /// The links in the drawing's texts and element links (`[[Note]]`), for the index.
    pub fn wikilinks(&self) -> Vec<String> {
        let mut out = Vec::new();
        for e in live(&self.state.elements) {
            for f in ["rawText", "originalText", "text", "link"] {
                if let Some(t) = e.str(f) {
                    out.extend(wikilinks_in(t));
                    if f != "link" && !out.is_empty() {
                        break;
                    }
                }
            }
        }
        out.sort();
        out.dedup();
        out
    }
}

/// The plugin's header for a new note (front matter it looks for).
const NOTE_HEAD: &str = "---\n\nexcalidraw-plugin: parsed\ntags: [excalidraw]\n\n---\nExcalidraw drawing. Open it in Quire, or in Obsidian with the Excalidraw plugin.\n\n\n";

/// Elements not deleted.
pub fn live(elements: &[El]) -> impl Iterator<Item = &El> {
    elements.iter().filter(|e| e.bool("isDeleted") != Some(true))
}

/// `[[Target]]`, `[[Target|alias]]`, `[[Target#heading]]` in `text`: the targets as written.
pub fn wikilinks_in(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(i) = rest.find("[[") {
        let after = &rest[i + 2..];
        let Some(j) = after.find("]]") else { break };
        let inner = &after[..j];
        if !inner.contains('\n') && !inner.trim().is_empty() {
            out.push(inner.split('|').next().unwrap_or(inner).trim().to_string());
        }
        rest = &after[j + 2..];
    }
    out
}

fn set_section(sections: &mut Vec<Section>, title: &str, body: String, at: usize) {
    match sections.iter_mut().find(|s| s.title.trim_end() == title) {
        Some(s) => s.body = body,
        None => sections.insert(at.min(sections.len()), Section { title: title.into(), body }),
    }
}

/// `## Embedded Files` lines: `id: [[path]]` (a colour map may follow).
fn embedded_files(n: &Note) -> Vec<(String, String)> {
    let Some(s) = n.sections.iter().find(|s| s.title.trim_end() == "## Embedded Files") else { return Vec::new() };
    s.body
        .lines()
        .filter_map(|l| {
            let (id, target) = l.split_once(": ")?;
            let inner = target.trim().strip_prefix("[[")?;
            let inner = &inner[..inner.find("]]")?];
            Some((id.trim().to_string(), inner.split('|').next().unwrap_or(inner).to_string()))
        })
        .collect()
}

fn parse_note(text: &str) -> Result<Note, String> {
    // `## Drawing` (or `# Drawing`) followed by a ```json or ```compressed-json fence.
    let mut drawing = None;
    let all: Vec<(usize, usize, usize)> = lines(text).collect();
    for (k, &(s, e, _)) in all.iter().enumerate() {
        let l = text[s..e].trim_end_matches('\r');
        if l == "## Drawing" || l == "# Drawing" {
            // The fence: the next line starting with ``` (no backticks before it).
            if let Some(&(fs, fe, fnext)) =
                all[k + 1..].iter().find(|(a, b, _)| text[*a..*b].starts_with("```") || text[*a..*b].contains('`'))
                && text[fs..fe].starts_with("```")
            {
                let lang = text[fs + 3..fe].trim();
                let enc = match lang {
                    "json" => Encoding::Plain,
                    "compressed-json" => Encoding::Compressed,
                    _ => continue,
                };
                drawing = Some((s, fnext, enc));
                break;
            }
        }
    }
    let (dstart, body_start, encoding) = drawing.ok_or("this note has no ## Drawing section")?;
    // The closing fence: a line starting with ``` after the body.
    let close = lines(&text[body_start..])
        .find(|(s, e, _)| text[body_start + s..body_start + e].starts_with("```"))
        .map(|(s, _, _)| body_start + s)
        .ok_or("the drawing's block isn't closed")?;
    let body = text[body_start..close].to_string();
    let tail = text[close..].to_string();
    let drawing_open = text[dstart..body_start].to_string();
    // `# Excalidraw Data` before the drawing.
    let data = lines(&text[..dstart]).find(|(s, e, _)| text[*s..*e].trim_end() == "# Excalidraw Data").map(|(s, _, _)| s);
    let Some(data) = data else {
        return Ok(Note {
            head: text[..dstart].to_string(),
            data_heading: String::new(),
            sections: Vec::new(),
            pre_drawing: String::new(),
            drawing_open,
            encoding,
            body,
            tail,
        });
    };
    let region = &text[data..dstart];
    let mut sections: Vec<Section> = Vec::new();
    let mut data_heading = String::new();
    for (s, e, next) in lines(region) {
        let line = &region[s..e];
        if line.starts_with("## ") {
            sections.push(Section { title: line.to_string(), body: String::new() });
        } else {
            let whole = &region[s..next];
            match sections.last_mut() {
                Some(sec) => sec.body.push_str(whole),
                None => data_heading.push_str(whole),
            }
        }
    }
    // A final `%%` line (with the blank lines around it) belongs before `## Drawing`.
    let mut pre_drawing = String::new();
    let last_body = sections.last_mut().map_or(&mut data_heading, |s| &mut s.body);
    if let Some(i) = last_body.trim_end_matches('\n').rfind('\n').map(|i| i + 1).or(Some(0))
        && last_body[i..].trim() == "%%"
    {
        pre_drawing = last_body[i..].to_string();
        last_body.truncate(i);
    }
    Ok(Note { head: text[..data].to_string(), data_heading, sections, pre_drawing, drawing_open, encoding, body, tail })
}

// ---------------------------------------------------------------- elements

/// The picture model of one element (`None` when deleted).
pub fn to_element(e: &Obj) -> Option<Element> {
    excalidraw::element(&Json::Obj(e.clone()).to_value())
}

/// Clock and randomness for edits (fixed in tests).
#[derive(Debug, Clone)]
pub struct Cx {
    pub now_ms: i64,
    rng: u64,
}

impl Cx {
    pub fn new(now_ms: i64, seed: u64) -> Self {
        Self { now_ms, rng: seed | 1 }
    }

    /// Now, and a seed from the clock.
    pub fn now() -> Self {
        let d = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
        Self::new(d.as_millis() as i64, d.as_nanos() as u64 ^ 0x9E37_79B9_7F4A_7C15)
    }

    /// Move the clock to now (before each edit).
    pub fn tick(&mut self) {
        let d = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
        self.now_ms = self.now_ms.max(d.as_millis() as i64);
    }

    fn next(&mut self) -> u64 {
        // xorshift64*
        self.rng ^= self.rng >> 12;
        self.rng ^= self.rng << 25;
        self.rng ^= self.rng >> 27;
        self.rng.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// A random integer in `0..2^31`, as Excalidraw's `randomInteger`.
    pub fn int(&mut self) -> i64 {
        (self.next() >> 33) as i64
    }

    /// A new id: 21 characters as Excalidraw's nanoid, or 8 (text in a note, as the plugin).
    pub fn id(&mut self, len: usize) -> String {
        const ALPHA: &[u8] = b"ModuleSymbhasOwnPr-0123456789ABCDEFGHNRVfgctiUvz_KqYTJkLxpZXIjQW";
        (0..len).map(|_| ALPHA[(self.next() >> 58) as usize] as char).collect()
    }

    /// An edit to `o`, as Excalidraw's `mutateElement`: `version + 1`, a new
    /// `versionNonce`, `updated` now.
    pub fn bump(&mut self, o: &mut Obj) {
        let v = o.num("version").unwrap_or(0.0);
        o.set_num("version", v + 1.0);
        let nonce = self.int();
        o.set_num("versionNonce", nonce as f64);
        o.set_num("updated", self.now_ms as f64);
    }
}

/// An element's box: (x, y, width, height), unrotated, in scene units.
/// Lines, arrows and freehand strokes: the box of their points.
pub fn bbox(e: &Obj) -> (f64, f64, f64, f64) {
    let (x, y) = (e.num("x").unwrap_or(0.0), e.num("y").unwrap_or(0.0));
    if let Some(pts) = e.arr("points").filter(|p| !p.is_empty()) {
        let (mut x0, mut y0, mut x1, mut y1) = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
        for p in pts {
            let (px, py) = point(p);
            x0 = x0.min(px);
            y0 = y0.min(py);
            x1 = x1.max(px);
            y1 = y1.max(py);
        }
        return (x + x0, y + y0, x1 - x0, y1 - y0);
    }
    (x, y, e.num("width").unwrap_or(0.0), e.num("height").unwrap_or(0.0))
}

fn point(p: &Json) -> (f64, f64) {
    let a = p.as_arr().map(Vec::as_slice).unwrap_or_default();
    (a.first().and_then(Json::as_f64).unwrap_or(0.0), a.get(1).and_then(Json::as_f64).unwrap_or(0.0))
}

/// An element's points, relative to its (x, y).
pub fn points(e: &Obj) -> Vec<(f64, f64)> {
    e.arr("points").map(|ps| ps.iter().map(point).collect()).unwrap_or_default()
}

fn points_json(pts: &[(f64, f64)]) -> Json {
    Json::Arr(pts.iter().map(|(x, y)| Json::Arr(vec![Json::num(*x), Json::num(*y)])).collect())
}

/// Set an element's points (relative), keeping its width and height their box.
pub fn set_points(e: &mut Obj, pts: &[(f64, f64)]) {
    e.set("points", points_json(pts));
    let (x0, x1) = pts.iter().fold((f64::MAX, f64::MIN), |(a, b), p| (a.min(p.0), b.max(p.0)));
    let (y0, y1) = pts.iter().fold((f64::MAX, f64::MIN), |(a, b), p| (a.min(p.1), b.max(p.1)));
    if !pts.is_empty() {
        e.set_num("width", x1 - x0);
        e.set_num("height", y1 - y0);
    }
}

/// The element's centre (its rotation centre).
pub fn center(e: &Obj) -> (f64, f64) {
    let (x, y, w, h) = bbox(e);
    (x + w / 2.0, y + h / 2.0)
}

fn rotate(p: (f64, f64), c: (f64, f64), a: f64) -> (f64, f64) {
    if a == 0.0 {
        return p;
    }
    let (s, co) = a.sin_cos();
    let (dx, dy) = (p.0 - c.0, p.1 - c.1);
    (c.0 + dx * co - dy * s, c.1 + dx * s + dy * co)
}

/// The axis-aligned box around the rotated element.
pub fn aabb(e: &Obj) -> (f64, f64, f64, f64) {
    let (x, y, w, h) = bbox(e);
    let a = e.num("angle").unwrap_or(0.0);
    if a == 0.0 {
        return (x, y, w, h);
    }
    let c = (x + w / 2.0, y + h / 2.0);
    let corners = [(x, y), (x + w, y), (x, y + h), (x + w, y + h)].map(|p| rotate(p, c, a));
    let (x0, x1) = corners.iter().fold((f64::MAX, f64::MIN), |(m, n), p| (m.min(p.0), n.max(p.0)));
    let (y0, y1) = corners.iter().fold((f64::MAX, f64::MIN), |(m, n), p| (m.min(p.1), n.max(p.1)));
    (x0, y0, x1 - x0, y1 - y0)
}

fn dist_to_segment(p: (f64, f64), a: (f64, f64), b: (f64, f64)) -> f64 {
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let len2 = dx * dx + dy * dy;
    let t = if len2 > 0.0 { (((p.0 - a.0) * dx + (p.1 - a.1) * dy) / len2).clamp(0.0, 1.0) } else { 0.0 };
    (p.0 - a.0 - t * dx).hypot(p.1 - a.1 - t * dy)
}

/// Whether `p` (scene units) hits the element, within `tol` of its outline.
/// Shapes with a fill, text and pictures are hit anywhere inside.
pub fn hit(e: &Obj, p: (f64, f64), tol: f64) -> bool {
    let kind = e.str("type").unwrap_or_default();
    let (x, y, w, h) = bbox(e);
    let c = (x + w / 2.0, y + h / 2.0);
    // Into the element's own (unrotated) frame.
    let q = rotate(p, c, -e.num("angle").unwrap_or(0.0));
    let filled = e.str("backgroundColor").is_some_and(|b| b != "transparent" && !b.is_empty());
    match kind {
        "line" | "arrow" | "freedraw" => {
            let (ox, oy) = (e.num("x").unwrap_or(0.0), e.num("y").unwrap_or(0.0));
            let pts: Vec<(f64, f64)> = points(e).into_iter().map(|(a, b)| (ox + a, oy + b)).collect();
            let tol = tol + e.num("strokeWidth").unwrap_or(2.0) / 2.0;
            if pts.len() == 1 {
                return (q.0 - pts[0].0).hypot(q.1 - pts[0].1) <= tol;
            }
            pts.windows(2).any(|s| dist_to_segment(q, s[0], s[1]) <= tol)
        }
        "ellipse" => {
            let (rx, ry) = ((w / 2.0).max(1e-6), (h / 2.0).max(1e-6));
            let n = ((q.0 - c.0) / rx).hypot((q.1 - c.1) / ry);
            if filled { n <= 1.0 + tol / rx.min(ry) } else { (n - 1.0).abs() * rx.min(ry) <= tol }
        }
        "diamond" => {
            let (rx, ry) = ((w / 2.0).max(1e-6), (h / 2.0).max(1e-6));
            let n = ((q.0 - c.0) / rx).abs() + ((q.1 - c.1) / ry).abs();
            if filled { n <= 1.0 + tol / rx.min(ry) } else { (n - 1.0).abs() * rx.min(ry) <= tol }
        }
        "rectangle" | "frame" | "magicframe" | "embeddable" | "iframe" if !filled => {
            let inside = q.0 >= x - tol && q.0 <= x + w + tol && q.1 >= y - tol && q.1 <= y + h + tol;
            let deep = q.0 > x + tol && q.0 < x + w - tol && q.1 > y + tol && q.1 < y + h - tol;
            inside && !deep
        }
        _ => q.0 >= x - tol && q.0 <= x + w + tol && q.1 >= y - tol && q.1 <= y + h + tol,
    }
}

// ---------------------------------------------------------------- fractional indices

/// Fractional indices (`index`), as Excalidraw's `fractional-indexing`
/// (base 62): keys that sort in the elements' order.
pub mod fractional {
    const DIGITS: &[u8] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
    const ZERO: u8 = b'0';
    const SMALLEST: &str = "A00000000000000000000000000";

    fn digit(c: u8) -> Option<usize> {
        DIGITS.iter().position(|d| *d == c)
    }

    fn int_len(head: u8) -> Option<usize> {
        match head {
            b'a'..=b'z' => Some(usize::from(head - b'a') + 2),
            b'A'..=b'Z' => Some(usize::from(b'Z' - head) + 2),
            _ => None,
        }
    }

    fn int_part(key: &str) -> Option<&str> {
        key.get(..int_len(*key.as_bytes().first()?)?)
    }

    /// Whether `key` is a valid key.
    pub fn valid(key: &str) -> bool {
        let Some(i) = int_part(key) else { return false };
        if key == SMALLEST || !key.bytes().all(|b| digit(b).is_some()) {
            return false;
        }
        let f = &key[i.len()..];
        !f.ends_with('0')
    }

    fn midpoint(a: &[u8], b: Option<&[u8]>) -> Option<Vec<u8>> {
        if let Some(b) = b {
            if a >= b {
                return None;
            }
            let n = (0..b.len()).take_while(|&i| a.get(i).copied().unwrap_or(ZERO) == b[i]).count();
            if n > 0 {
                let mut out = b[..n].to_vec();
                out.extend(midpoint(a.get(n..).unwrap_or_default(), Some(&b[n..]))?);
                return Some(out);
            }
        }
        let da = a.first().map_or(Some(0), |c| digit(*c))?;
        let db = match b {
            Some(b) => digit(*b.first()?)?,
            None => DIGITS.len(),
        };
        if db > da + 1 {
            let mid = (da + db).div_ceil(2);
            return Some(vec![DIGITS[mid]]);
        }
        if let Some(b) = b.filter(|b| b.len() > 1) {
            return Some(b[..1].to_vec());
        }
        let mut out = vec![DIGITS[da]];
        out.extend(midpoint(a.get(1..).unwrap_or_default(), None)?);
        Some(out)
    }

    fn increment(x: &str) -> Option<String> {
        let b = x.as_bytes();
        let head = b[0];
        let mut digs = b[1..].to_vec();
        let mut carry = true;
        for d in digs.iter_mut().rev() {
            let n = digit(*d)? + 1;
            if n == DIGITS.len() {
                *d = ZERO;
            } else {
                *d = DIGITS[n];
                carry = false;
                break;
            }
        }
        if carry {
            if head == b'Z' {
                return Some("a0".into());
            }
            if head == b'z' {
                return None;
            }
            let h = head + 1;
            if h > b'a' {
                digs.push(ZERO);
            } else {
                digs.pop();
            }
            let mut out = vec![h];
            out.extend(digs);
            return String::from_utf8(out).ok();
        }
        let mut out = vec![head];
        out.extend(digs);
        String::from_utf8(out).ok()
    }

    fn decrement(x: &str) -> Option<String> {
        let b = x.as_bytes();
        let head = b[0];
        let mut digs = b[1..].to_vec();
        let mut borrow = true;
        let last = *DIGITS.last()?;
        for d in digs.iter_mut().rev() {
            let n = digit(*d)?;
            if n == 0 {
                *d = last;
            } else {
                *d = DIGITS[n - 1];
                borrow = false;
                break;
            }
        }
        if borrow {
            if head == b'a' {
                return Some(format!("Z{}", last as char));
            }
            if head == b'A' {
                return None;
            }
            let h = head - 1;
            if h < b'Z' {
                digs.push(last);
            } else {
                digs.pop();
            }
            let mut out = vec![h];
            out.extend(digs);
            return String::from_utf8(out).ok();
        }
        let mut out = vec![head];
        out.extend(digs);
        String::from_utf8(out).ok()
    }

    /// A key between `a` and `b` (either may be open), as `generateKeyBetween`.
    pub fn between(a: Option<&str>, b: Option<&str>) -> Option<String> {
        if a.is_some_and(|a| !valid(a)) || b.is_some_and(|b| !valid(b)) {
            return None;
        }
        let join = |i: &str, f: Vec<u8>| String::from_utf8(f).ok().map(|f| format!("{i}{f}"));
        match (a, b) {
            (None, None) => Some("a0".into()),
            (None, Some(b)) => {
                let ib = int_part(b)?;
                let fb = &b[ib.len()..];
                if ib == SMALLEST {
                    return join(ib, midpoint(b"", Some(fb.as_bytes()))?);
                }
                if ib < b {
                    return Some(ib.to_string());
                }
                decrement(ib)
            }
            (Some(a), None) => {
                let ia = int_part(a)?;
                let fa = &a[ia.len()..];
                match increment(ia) {
                    Some(i) => Some(i),
                    None => join(ia, midpoint(fa.as_bytes(), None)?),
                }
            }
            (Some(a), Some(b)) => {
                if a >= b {
                    return None;
                }
                let (ia, ib) = (int_part(a)?, int_part(b)?);
                let (fa, fb) = (&a[ia.len()..], &b[ib.len()..]);
                if ia == ib {
                    return join(ia, midpoint(fa.as_bytes(), Some(fb.as_bytes()))?);
                }
                let i = increment(ia)?;
                if i.as_str() < b {
                    return Some(i);
                }
                join(ia, midpoint(fa.as_bytes(), None)?)
            }
        }
    }
}

// ---------------------------------------------------------------- edits

/// Defaults for new elements (the style chosen in the editor).
#[derive(Debug, Clone, PartialEq)]
#[expect(clippy::struct_field_names, reason = "the names of Excalidraw's own fields (fillStyle, strokeStyle)")]
pub struct Style {
    pub stroke: String,
    pub background: String,
    pub fill_style: String,
    pub stroke_width: f64,
    pub stroke_style: String,
    pub roughness: f64,
    pub opacity: f64,
    pub font_family: i64,
    pub font_size: f64,
    pub text_align: String,
    pub rounded: bool,
    pub start_arrow: Option<String>,
    pub end_arrow: Option<String>,
}

impl Default for Style {
    fn default() -> Self {
        Self {
            stroke: "#1e1e1e".into(),
            background: "transparent".into(),
            fill_style: "solid".into(),
            stroke_width: 2.0,
            stroke_style: "solid".into(),
            roughness: 1.0,
            opacity: 100.0,
            font_family: 5,
            font_size: 20.0,
            text_align: "left".into(),
            rounded: true,
            start_arrow: None,
            end_arrow: Some("arrow".into()),
        }
    }
}

/// How to align elements.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Align {
    Left,
    CenterX,
    Right,
    Top,
    CenterY,
    Bottom,
}

/// Z-order moves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Z {
    Forward,
    Backward,
    Front,
    Back,
}

fn opt_str(v: Option<&String>) -> Json {
    v.map_or(Json::Null, |s| Json::str(s))
}

impl Drawing {
    pub fn index_of(&self, id: &str) -> Option<usize> {
        self.state.elements.iter().position(|e| e.str("id") == Some(id))
    }

    pub fn get(&self, id: &str) -> Option<&El> {
        self.state.elements.iter().find(|e| e.str("id") == Some(id))
    }

    /// Change element `id` (bumping its version). Returns false if missing.
    pub fn edit(&mut self, id: &str, cx: &mut Cx, f: impl FnOnce(&mut Obj)) -> bool {
        let Some(i) = self.index_of(id) else { return false };
        let o = Arc::make_mut(&mut self.state.elements[i]);
        f(o);
        cx.bump(o);
        true
    }

    /// Whether the drawing keeps fractional indices (Excalidraw 0.17 on, or a new drawing).
    fn uses_indices(&self) -> bool {
        self.state.elements.is_empty() || self.state.elements.iter().any(|e| e.get("index").is_some())
    }

    /// A new element of `kind` at (x, y), with the base fields Excalidraw writes.
    pub fn new_element(&self, kind: &str, x: f64, y: f64, style: &Style, cx: &mut Cx) -> Obj {
        let mut o = Obj::default();
        let id_len = if kind == "text" && self.is_note() { 8 } else { 21 };
        o.set_str("id", &cx.id(id_len));
        o.set_str("type", kind);
        o.set_num("x", x);
        o.set_num("y", y);
        o.set_num("width", 0.0);
        o.set_num("height", 0.0);
        o.set_num("angle", 0.0);
        o.set_str("strokeColor", if kind == "image" { "transparent" } else { &style.stroke });
        let bg = if matches!(kind, "rectangle" | "ellipse" | "diamond" | "line" | "freedraw") {
            style.background.as_str()
        } else {
            "transparent"
        };
        o.set_str("backgroundColor", bg);
        o.set_str("fillStyle", &style.fill_style);
        o.set_num("strokeWidth", style.stroke_width);
        o.set_str("strokeStyle", &style.stroke_style);
        o.set_num("roughness", style.roughness);
        o.set_num("opacity", style.opacity);
        o.set("groupIds", Json::Arr(Vec::new()));
        o.set("frameId", Json::Null);
        if self.uses_indices() {
            o.set("index", Json::Null);
        }
        // Excalidraw's radius types: adaptive (3) for rectangles, proportional (2) for the rest.
        let roundness = match kind {
            "rectangle" if style.rounded => Json::Obj(Obj(vec![("type".into(), Json::num(3.0))])),
            "diamond" | "arrow" | "line" if style.rounded => Json::Obj(Obj(vec![("type".into(), Json::num(2.0))])),
            _ => Json::Null,
        };
        o.set("roundness", roundness);
        o.set_num("seed", cx.int() as f64);
        o.set_num("version", 1.0);
        o.set_num("versionNonce", cx.int() as f64);
        o.set("isDeleted", Json::Bool(false));
        o.set("boundElements", Json::Null);
        o.set_num("updated", cx.now_ms as f64);
        o.set("link", Json::Null);
        o.set("locked", Json::Bool(false));
        match kind {
            "text" => {
                o.set_str("text", "");
                o.set_num("fontSize", style.font_size);
                o.set_num("fontFamily", style.font_family as f64);
                o.set_str("textAlign", &style.text_align);
                o.set_str("verticalAlign", "top");
                o.set("containerId", Json::Null);
                o.set_str("originalText", "");
                o.set("autoResize", Json::Bool(true));
                o.set_num("lineHeight", 1.25);
            }
            "line" | "arrow" => {
                o.set("points", points_json(&[(0.0, 0.0)]));
                o.set("lastCommittedPoint", Json::Null);
                o.set("startBinding", Json::Null);
                o.set("endBinding", Json::Null);
                let (s, e) = if kind == "arrow" {
                    (opt_str(style.start_arrow.as_ref()), opt_str(style.end_arrow.as_ref()))
                } else {
                    (Json::Null, Json::Null)
                };
                o.set("startArrowhead", s);
                o.set("endArrowhead", e);
                if kind == "arrow" {
                    o.set("elbowed", Json::Bool(false));
                }
            }
            "freedraw" => {
                o.set("points", points_json(&[(0.0, 0.0)]));
                o.set("pressures", Json::Arr(Vec::new()));
                o.set("simulatePressure", Json::Bool(true));
                o.set("lastCommittedPoint", Json::Null);
            }
            "image" => {
                o.set("fileId", Json::Null);
                o.set_str("status", "saved");
                o.set("scale", Json::Arr(vec![Json::num(1.0), Json::num(1.0)]));
                o.set("crop", Json::Null);
            }
            _ => {}
        }
        o
    }

    /// Add an element on top. Returns its id.
    pub fn add(&mut self, o: Obj, cx: &mut Cx) -> String {
        let id = o.str("id").unwrap_or_default().to_string();
        self.state.elements.push(Arc::new(o));
        self.sync_indices(cx);
        id
    }

    /// Give elements whose `index` is missing or out of order a key between
    /// their neighbours (Excalidraw's `syncInvalidIndices`).
    pub fn sync_indices(&mut self, cx: &mut Cx) {
        if !self.state.elements.iter().any(|e| e.get("index").is_some()) {
            return;
        }
        let n = self.state.elements.len();
        let key = |e: &El| e.str("index").filter(|k| fractional::valid(k)).map(str::to_string);
        let mut prev: Option<String> = None;
        let mut i = 0;
        while i < n {
            let k = key(&self.state.elements[i]);
            if let Some(k) = k.filter(|k| prev.as_ref().is_none_or(|p| k > p)) {
                prev = Some(k);
                i += 1;
                continue;
            }
            // Elements i..j need keys between prev and element j's key.
            let mut j = i + 1;
            let upper = loop {
                if j >= n {
                    break None;
                }
                if let Some(k) = key(&self.state.elements[j]).filter(|k| prev.as_ref().is_none_or(|p| k > p)) {
                    break Some(k);
                }
                j += 1;
            };
            for e in &mut self.state.elements[i..j] {
                let k = fractional::between(prev.as_deref(), upper.as_deref())
                    .unwrap_or_else(|| format!("{}V", prev.clone().unwrap_or_else(|| "a0".into())));
                let o = Arc::make_mut(e);
                o.set_str("index", &k);
                cx.bump(o);
                prev = Some(k);
            }
            i = j;
        }
    }

    /// Delete elements (Excalidraw marks them `isDeleted`), with the text
    /// bound inside them; arrows bound to them let go.
    pub fn erase(&mut self, ids: &[String], cx: &mut Cx) {
        let mut gone: Vec<String> = ids.to_vec();
        for e in live(&self.state.elements) {
            if let Some(c) = e.str("containerId")
                && ids.iter().any(|i| i == c)
                && let Some(id) = e.str("id")
            {
                gone.push(id.to_string());
            }
        }
        for id in &gone {
            self.edit(id, cx, |o| o.set("isDeleted", Json::Bool(true)));
        }
        // Bindings to and from what was deleted.
        let others: Vec<String> = live(&self.state.elements).filter_map(|e| e.str("id").map(str::to_string)).collect();
        for id in others {
            let Some(e) = self.get(&id) else { continue };
            let mut changed = false;
            let mut o = (**e).clone();
            for b in ["startBinding", "endBinding"] {
                if o.obj(b).and_then(|b| b.str("elementId")).is_some_and(|t| gone.iter().any(|g| g == t)) {
                    o.set(b, Json::Null);
                    changed = true;
                }
            }
            if let Some(Json::Arr(list)) = o.get_mut("boundElements") {
                let before = list.len();
                list.retain(|b| b.as_obj().and_then(|b| b.str("id")).is_none_or(|t| !gone.iter().any(|g| g == t)));
                changed |= list.len() != before;
            }
            if o.str("containerId").is_some_and(|c| gone.iter().any(|g| g == c)) {
                o.set("containerId", Json::Null);
                changed = true;
            }
            if changed {
                self.edit(&id, cx, |x| *x = o);
            }
        }
    }

    /// Move elements by (dx, dy), with the text inside them; arrows bound
    /// to them follow.
    pub fn translate(&mut self, ids: &[String], dx: f64, dy: f64, cx: &mut Cx) {
        let mut all: Vec<String> = ids.to_vec();
        for e in live(&self.state.elements) {
            if let (Some(c), Some(id)) = (e.str("containerId"), e.str("id"))
                && ids.iter().any(|i| i == c)
                && !all.iter().any(|a| a == id)
            {
                all.push(id.to_string());
            }
        }
        for id in &all {
            self.edit(id, cx, |o| {
                let (x, y) = (o.num("x").unwrap_or(0.0), o.num("y").unwrap_or(0.0));
                o.set_num("x", x + dx);
                o.set_num("y", y + dy);
            });
        }
        // An arrow moved on its own lets go of what it was bound to.
        for id in ids {
            let is_arrow = self.get(id).is_some_and(|e| e.str("type") == Some("arrow"));
            if is_arrow {
                self.unbind_arrow(id, &all, cx);
            }
        }
        self.update_bound_arrows(&all, cx);
    }

    /// Let an arrow go of its bindings, except to elements in `keep`.
    fn unbind_arrow(&mut self, arrow: &str, keep: &[String], cx: &mut Cx) {
        let Some(e) = self.get(arrow) else { return };
        let targets: Vec<(String, String)> = ["startBinding", "endBinding"]
            .iter()
            .filter_map(|b| Some((b.to_string(), e.obj(b)?.str("elementId")?.to_string())))
            .filter(|(_, t)| !keep.iter().any(|k| k == t))
            .collect();
        for (b, t) in targets {
            self.edit(arrow, cx, |o| o.set(&b, Json::Null));
            let still = self
                .get(arrow)
                .is_some_and(|e| ["startBinding", "endBinding"].iter().any(|b| e.obj(b).and_then(|b| b.str("elementId")) == Some(&t)));
            if !still {
                self.edit(&t, cx, |o| {
                    if let Some(Json::Arr(list)) = o.get_mut("boundElements") {
                        list.retain(|b| b.as_obj().and_then(|b| b.str("id")) != Some(arrow));
                    }
                });
            }
        }
    }

    /// Bind an arrow's end (`start`: its first point) to `target`.
    pub fn bind(&mut self, arrow: &str, start: bool, target: &str, cx: &mut Cx) {
        let key = if start { "startBinding" } else { "endBinding" };
        let binding =
            Json::Obj(Obj(vec![("elementId".into(), Json::str(target)), ("focus".into(), Json::num(0.0)), ("gap".into(), Json::num(4.0))]));
        self.edit(arrow, cx, |o| o.set(key, binding));
        let already = self
            .get(target)
            .and_then(|t| t.arr("boundElements"))
            .is_some_and(|l| l.iter().any(|b| b.as_obj().and_then(|b| b.str("id")) == Some(arrow)));
        if !already {
            self.edit(target, cx, |o| {
                let mut list = o.arr("boundElements").map(<[Json]>::to_vec).unwrap_or_default();
                list.push(Json::Obj(Obj(vec![("id".into(), Json::str(arrow)), ("type".into(), Json::str("arrow"))])));
                o.set("boundElements", Json::Arr(list));
            });
        }
        self.update_bound_arrows(&[target.to_string()], cx);
    }

    /// Bring the ends of arrows bound to `moved` back onto their shapes.
    pub fn update_bound_arrows(&mut self, moved: &[String], cx: &mut Cx) {
        let arrows: Vec<String> = live(&self.state.elements)
            .filter(|e| e.str("type") == Some("arrow"))
            .filter(|e| {
                ["startBinding", "endBinding"]
                    .iter()
                    .any(|b| e.obj(b).and_then(|b| b.str("elementId")).is_some_and(|t| moved.iter().any(|m| m == t)))
            })
            .filter_map(|e| e.str("id").map(str::to_string))
            .collect();
        for a in arrows {
            let Some(e) = self.get(&a).cloned() else { continue };
            let (ox, oy) = (e.num("x").unwrap_or(0.0), e.num("y").unwrap_or(0.0));
            let mut abs: Vec<(f64, f64)> = points(&e).into_iter().map(|(x, y)| (ox + x, oy + y)).collect();
            if abs.len() < 2 {
                continue;
            }
            let n = abs.len();
            for (start, b) in [(true, "startBinding"), (false, "endBinding")] {
                let Some(t) = e.obj(b).and_then(|b| b.str("elementId")).and_then(|t| self.get(t)) else { continue };
                let gap = e.obj(b).and_then(|b| b.num("gap")).unwrap_or(4.0);
                let (end, near) = if start { (0, 1) } else { (n - 1, n - 2) };
                // From the next point toward the target; a straight arrow bound at
                // both ends aims at the other shape's centre, so it doesn't drift.
                let other =
                    e.obj(if start { "endBinding" } else { "startBinding" }).and_then(|b| b.str("elementId")).and_then(|o| self.get(o));
                let from = match other {
                    Some(o) if n == 2 => center(o),
                    _ => abs[near],
                };
                abs[end] = edge_point(t, from, gap);
            }
            let (nx, ny) = abs[0];
            let rel: Vec<(f64, f64)> = abs.iter().map(|(x, y)| (x - nx, y - ny)).collect();
            self.edit(&a, cx, |o| {
                o.set_num("x", nx);
                o.set_num("y", ny);
                set_points(o, &rel);
            });
        }
    }

    /// Resize element `id` to the box (x, y, w, h) (scene units, unrotated).
    pub fn resize(&mut self, id: &str, to: (f64, f64, f64, f64), cx: &mut Cx) {
        let Some(e) = self.get(id).cloned() else { return };
        let (bx, by, bw, bh) = bbox(&e);
        let (x, y, w, h) = to;
        let kind = e.str("type").unwrap_or_default().to_string();
        match kind.as_str() {
            "line" | "arrow" | "freedraw" => {
                let (sx, sy) = (if bw > 0.0 { w / bw } else { 1.0 }, if bh > 0.0 { h / bh } else { 1.0 });
                let (ox, oy) = (e.num("x").unwrap_or(0.0), e.num("y").unwrap_or(0.0));
                let abs: Vec<(f64, f64)> =
                    points(&e).into_iter().map(|(px, py)| (x + (ox + px - bx) * sx, y + (oy + py - by) * sy)).collect();
                let (nx, ny) = abs.first().copied().unwrap_or((x, y));
                let rel: Vec<(f64, f64)> = abs.iter().map(|(a, b)| (a - nx, b - ny)).collect();
                self.edit(id, cx, |o| {
                    o.set_num("x", nx);
                    o.set_num("y", ny);
                    set_points(o, &rel);
                });
            }
            "text" => {
                let s = if bh > 0.0 { h / bh } else { 1.0 };
                let size = e.num("fontSize").unwrap_or(20.0) * s;
                self.edit(id, cx, |o| {
                    o.set_num("x", x);
                    o.set_num("y", y);
                    o.set_num("width", bw * s);
                    o.set_num("height", h);
                    o.set_num("fontSize", size);
                });
            }
            _ => {
                self.edit(id, cx, |o| {
                    o.set_num("x", x);
                    o.set_num("y", y);
                    o.set_num("width", w.max(0.0));
                    o.set_num("height", h.max(0.0));
                });
                self.center_bound_text(id, cx);
            }
        }
        self.update_bound_arrows(&[id.to_string()], cx);
    }

    /// The text bound inside a container, centred in it.
    pub fn center_bound_text(&mut self, container: &str, cx: &mut Cx) {
        let Some(c) = self.get(container).cloned() else { return };
        let text =
            live(&self.state.elements).find(|e| e.str("containerId") == Some(container)).and_then(|e| e.str("id")).map(str::to_string);
        let Some(t) = text else { return };
        let (x, y, w, h) = bbox(&c);
        let angle = c.num("angle").unwrap_or(0.0);
        self.edit(&t, cx, |o| {
            let (tw, th) = (o.num("width").unwrap_or(0.0), o.num("height").unwrap_or(0.0));
            o.set_num("x", x + (w - tw) / 2.0);
            o.set_num("y", y + (h - th) / 2.0);
            o.set_num("angle", angle);
        });
    }

    /// Rotate elements by `delta` radians around (cx, cy).
    pub fn rotate(&mut self, ids: &[String], delta: f64, about: (f64, f64), cx: &mut Cx) {
        let tau = std::f64::consts::TAU;
        for id in ids {
            let Some(e) = self.get(id).cloned() else { continue };
            let c = center(&e);
            let nc = rotate(c, about, delta);
            let (x, y) = (e.num("x").unwrap_or(0.0), e.num("y").unwrap_or(0.0));
            let a = (e.num("angle").unwrap_or(0.0) + delta).rem_euclid(tau);
            self.edit(id, cx, |o| {
                o.set_num("x", x + nc.0 - c.0);
                o.set_num("y", y + nc.1 - c.1);
                o.set_num("angle", a);
            });
            self.center_bound_text(id, cx);
        }
        self.update_bound_arrows(ids, cx);
    }

    /// Set one style field on elements (text inside a container too, for colours).
    pub fn set_field(&mut self, ids: &[String], key: &str, v: &Json, cx: &mut Cx) {
        for id in ids {
            self.edit(id, cx, |o| o.set(key, v.clone()));
        }
    }

    /// Copies of elements, offset by (dx, dy), on top; groups and bound
    /// texts are copied together. Returns the copies' ids, in order.
    pub fn duplicate(&mut self, ids: &[String], dx: f64, dy: f64, cx: &mut Cx) -> Vec<String> {
        let mut take: Vec<String> = ids.to_vec();
        for e in live(&self.state.elements) {
            if let (Some(c), Some(id)) = (e.str("containerId"), e.str("id"))
                && ids.iter().any(|i| i == c)
                && !take.iter().any(|t| t == id)
            {
                take.push(id.to_string());
            }
        }
        let objs: Vec<Obj> = self
            .state
            .elements
            .iter()
            .filter(|e| e.str("id").is_some_and(|i| take.iter().any(|t| t == i)))
            .map(|e| (**e).clone())
            .collect();
        let (copies, map) = copies(objs, dx, dy, self.is_note(), cx);
        self.state.elements.extend(copies.into_iter().map(Arc::new));
        self.sync_indices(cx);
        ids.iter().filter_map(|i| map.iter().find(|(a, _)| a == i).map(|(_, b)| b.clone())).collect()
    }

    /// Group elements: a new group id, outermost, on each.
    pub fn group(&mut self, ids: &[String], cx: &mut Cx) -> String {
        let g = cx.id(21);
        for id in ids {
            self.edit(id, cx, |o| {
                let mut list = o.arr("groupIds").map(<[Json]>::to_vec).unwrap_or_default();
                list.push(Json::str(&g));
                o.set("groupIds", Json::Arr(list));
            });
        }
        g
    }

    /// Take elements out of their outermost group.
    pub fn ungroup(&mut self, ids: &[String], cx: &mut Cx) {
        for id in ids {
            if self.get(id).and_then(|e| e.arr("groupIds")).is_some_and(|g| !g.is_empty()) {
                self.edit(id, cx, |o| {
                    let mut list = o.arr("groupIds").map(<[Json]>::to_vec).unwrap_or_default();
                    list.pop();
                    o.set("groupIds", Json::Arr(list));
                });
            }
        }
    }

    /// The elements that select together with `id`: its outermost group.
    pub fn group_of(&self, id: &str) -> Vec<String> {
        let Some(g) = self.get(id).and_then(|e| e.arr("groupIds")).and_then(|g| g.last()).and_then(Json::as_str).map(str::to_string) else {
            return vec![id.to_string()];
        };
        live(&self.state.elements)
            .filter(|e| e.arr("groupIds").is_some_and(|l| l.iter().any(|x| x.as_str() == Some(&g))))
            .filter_map(|e| e.str("id").map(str::to_string))
            .collect()
    }

    /// Move elements in the stacking order.
    pub fn reorder(&mut self, ids: &[String], z: Z, cx: &mut Cx) {
        let sel = |e: &El| e.str("id").is_some_and(|i| ids.iter().any(|s| s == i));
        let els = &mut self.state.elements;
        match z {
            Z::Front | Z::Back => {
                let (picked, rest): (Vec<El>, Vec<El>) = els.drain(..).partition(|e| sel(e));
                *els = if z == Z::Front { rest.into_iter().chain(picked).collect() } else { picked.into_iter().chain(rest).collect() };
            }
            Z::Forward => {
                let mut i = els.len();
                while i > 0 {
                    i -= 1;
                    if sel(&els[i]) && i + 1 < els.len() && !sel(&els[i + 1]) {
                        els.swap(i, i + 1);
                    }
                }
            }
            Z::Backward => {
                for i in 1..els.len() {
                    if sel(&els[i]) && !sel(&els[i - 1]) {
                        els.swap(i, i - 1);
                    }
                }
            }
        }
        // Moved elements take new keys between their neighbours.
        for id in ids {
            if let Some(i) = self.index_of(id) {
                let prev = i.checked_sub(1).and_then(|p| self.state.elements[p].str("index")).map(str::to_string);
                let next = self.state.elements.get(i + 1).and_then(|n| n.str("index")).map(str::to_string);
                let cur = self.state.elements[i].str("index").map(str::to_string);
                let ok = cur.as_ref().is_some_and(|c| prev.as_ref().is_none_or(|p| c > p) && next.as_ref().is_none_or(|n| c < n));
                if !ok && cur.is_some() {
                    self.edit(id, cx, |o| o.set("index", Json::Null));
                }
            }
        }
        self.sync_indices(cx);
    }

    /// Align elements' boxes to the selection's box.
    pub fn align(&mut self, ids: &[String], how: Align, cx: &mut Cx) {
        let boxes: Vec<(String, (f64, f64, f64, f64))> = ids.iter().filter_map(|id| Some((id.clone(), aabb(self.get(id)?)))).collect();
        let Some(all) = union(boxes.iter().map(|(_, b)| *b)) else { return };
        for (id, (x, y, w, h)) in boxes {
            let (dx, dy) = match how {
                Align::Left => (all.0 - x, 0.0),
                Align::CenterX => (all.0 + all.2 / 2.0 - (x + w / 2.0), 0.0),
                Align::Right => (all.0 + all.2 - (x + w), 0.0),
                Align::Top => (0.0, all.1 - y),
                Align::CenterY => (0.0, all.1 + all.3 / 2.0 - (y + h / 2.0)),
                Align::Bottom => (0.0, all.1 + all.3 - (y + h)),
            };
            if dx != 0.0 || dy != 0.0 {
                self.translate(std::slice::from_ref(&id), dx, dy, cx);
            }
        }
    }

    /// Lock or unlock elements.
    pub fn lock(&mut self, ids: &[String], locked: bool, cx: &mut Cx) {
        for id in ids {
            self.edit(id, cx, |o| o.set("locked", Json::Bool(locked)));
        }
    }

    /// Set a text element's text (`raw` may hold `[[links]]`), with its measured size.
    pub fn set_text(&mut self, id: &str, raw: &str, size: (f64, f64), cx: &mut Cx) {
        let shown = display_text(raw);
        let link = wikilinks_in(raw).into_iter().next().map(|l| format!("[[{l}]]"));
        let note = self.is_note();
        let had_raw = self.get(id).is_some_and(|e| e.get("rawText").is_some());
        let container = self.get(id).and_then(|e| e.str("containerId")).map(str::to_string);
        self.edit(id, cx, |o| {
            o.set_str("text", &shown);
            o.set_str("originalText", &shown);
            if note || had_raw {
                o.set_str("rawText", raw);
            }
            o.set_num("width", size.0);
            o.set_num("height", size.1);
            if let Some(l) = link {
                o.set_str("link", &l);
            } else if o.str("link").is_some_and(|l| l.starts_with("[[")) && note {
                o.set("link", Json::Null);
            }
        });
        if let Some(c) = container {
            self.center_bound_text(&c, cx);
        }
    }

    /// Add a picture: a vault file in a note (`## Embedded Files`), else a
    /// `data:` URL in the scene's `files`. Returns the file id.
    pub fn add_file(&mut self, link: &str, mime: &str, data_url: Option<String>, cx: &mut Cx) -> String {
        let id: String = (0..40).map(|_| char::from(b"0123456789abcdef"[(cx.int() & 15) as usize])).collect();
        match data_url {
            Some(u) if !self.is_note() => {
                let f = Obj(vec![
                    ("mimeType".into(), Json::str(mime)),
                    ("id".into(), Json::str(&id)),
                    ("dataURL".into(), Json::Str(u)),
                    ("created".into(), Json::num(cx.now_ms as f64)),
                    ("lastRetrieved".into(), Json::num(cx.now_ms as f64)),
                ]);
                Arc::make_mut(&mut self.state.files).set(&id, Json::Obj(f));
            }
            _ => Arc::make_mut(&mut self.state.embedded).push((id.clone(), link.to_string())),
        }
        id
    }
}

/// Text as shown: `[[Note|alias]]` shows `alias`, `[[Note]]` shows `Note`.
pub fn display_text(raw: &str) -> String {
    let mut out = String::new();
    let mut rest = raw;
    while let Some(i) = rest.find("[[") {
        let after = &rest[i + 2..];
        let Some(j) = after.find("]]").filter(|j| !after[..*j].contains('\n')) else { break };
        out.push_str(&rest[..i]);
        let inner = &after[..j];
        out.push_str(inner.split_once('|').map_or(inner, |(_, a)| a));
        rest = &after[j + 2..];
    }
    out.push_str(rest);
    out
}

fn union(boxes: impl Iterator<Item = (f64, f64, f64, f64)>) -> Option<(f64, f64, f64, f64)> {
    boxes
        .map(|(x, y, w, h)| (x, y, x + w, y + h))
        .reduce(|a, b| (a.0.min(b.0), a.1.min(b.1), a.2.max(b.2), a.3.max(b.3)))
        .map(|(x0, y0, x1, y1)| (x0, y0, x1 - x0, y1 - y0))
}

/// The box around several elements.
pub fn union_box<'a>(els: impl Iterator<Item = &'a El>) -> Option<(f64, f64, f64, f64)> {
    union(els.map(|e| aabb(e)))
}

/// Where a line from `from` toward the element's centre meets its outline,
/// `gap` outside it.
fn edge_point(t: &Obj, from: (f64, f64), gap: f64) -> (f64, f64) {
    let (x, y, w, h) = bbox(t);
    let c = (x + w / 2.0, y + h / 2.0);
    let a = t.num("angle").unwrap_or(0.0);
    let f = rotate(from, c, -a);
    let (dx, dy) = (f.0 - c.0, f.1 - c.1);
    let len = dx.hypot(dy);
    if len < 1e-9 {
        return from;
    }
    let (ux, uy) = (dx / len, dy / len);
    let (rx, ry) = ((w / 2.0).max(1e-6), (h / 2.0).max(1e-6));
    let s = match t.str("type").unwrap_or_default() {
        "ellipse" => 1.0 / (ux / rx).hypot(uy / ry),
        "diamond" => 1.0 / ((ux / rx).abs() + (uy / ry).abs()),
        _ => (rx / ux.abs().max(1e-9)).min(ry / uy.abs().max(1e-9)),
    };
    let p = (c.0 + ux * (s + gap), c.1 + uy * (s + gap));
    rotate(p, c, a)
}

/// Undo and redo for a drawing.
#[derive(Debug, Default, Clone)]
pub struct History {
    undo: Vec<State>,
    redo: Vec<State>,
}

/// How many steps are kept.
const UNDO_STEPS: usize = 200;

impl History {
    /// Remember `state` before a change.
    pub fn checkpoint(&mut self, state: &State) {
        self.undo.push(state.clone());
        if self.undo.len() > UNDO_STEPS {
            self.undo.remove(0);
        }
        self.redo.clear();
    }

    /// Undo into `state`. Returns false when there's nothing to undo.
    pub fn undo(&mut self, state: &mut State) -> bool {
        let Some(prev) = self.undo.pop() else { return false };
        self.redo.push(std::mem::replace(state, prev));
        true
    }

    pub fn redo(&mut self, state: &mut State) -> bool {
        let Some(next) = self.redo.pop() else { return false };
        self.undo.push(std::mem::replace(state, next));
        true
    }

    /// Go back to the last checkpoint without a redo step (an edit that
    /// came to nothing, such as an empty new text).
    pub fn cancel(&mut self, state: &mut State) -> bool {
        let Some(prev) = self.undo.pop() else { return false };
        *state = prev;
        true
    }

    /// Forget every step (the drawing was read again).
    pub fn clear(&mut self) {
        self.undo.clear();
        self.redo.clear();
    }

    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }
}

/// The indent of a JSON text (for tests and docs).
pub fn indent_of(json: &str) -> Indent {
    Doc::parse(json).map_or(Indent::Compact, |d| d.indent)
}

/// Elements as Excalidraw's clipboard JSON (copy and paste), with the files they use.
pub fn clipboard_json(els: &[El], files: &Obj) -> String {
    let used: Vec<&str> = els.iter().filter_map(|e| e.str("fileId")).collect();
    let files = Obj(files.0.iter().filter(|(k, _)| used.contains(&k.as_str())).cloned().collect());
    let o = Obj(vec![
        ("type".into(), Json::str("excalidraw/clipboard")),
        ("elements".into(), Json::Arr(els.iter().map(|e| Json::Obj((**e).clone())).collect())),
        ("files".into(), Json::Obj(files)),
    ]);
    ojson::to_compact(&Json::Obj(o))
}

/// Elements (and files) from Excalidraw's clipboard JSON.
pub fn from_clipboard(text: &str) -> Option<(Vec<Obj>, Obj)> {
    let v = ojson::parse(text.trim()).ok()?;
    let o = v.as_obj()?;
    if o.str("type") != Some("excalidraw/clipboard") && o.str("type") != Some("excalidraw") {
        return None;
    }
    let els = o.arr("elements")?.iter().filter_map(|e| e.as_obj().cloned()).filter(|e| e.bool("isDeleted") != Some(true)).collect();
    Some((els, o.obj("files").cloned().unwrap_or_default()))
}

impl Drawing {
    /// Paste elements (from the clipboard) with new ids, their box's corner
    /// at `at`. Returns the new ids of the top-level ones (not bound texts).
    pub fn paste(&mut self, els: Vec<Obj>, files: &Obj, at: (f64, f64), cx: &mut Cx) -> Vec<String> {
        let b = union(els.iter().map(aabb)).unwrap_or_default();
        let top: Vec<String> =
            els.iter().filter(|e| e.get("containerId").is_none_or(Json::is_null)).filter_map(|e| e.str("id").map(str::to_string)).collect();
        for (k, v) in &files.0 {
            if self.state.files.get(k).is_none() {
                Arc::make_mut(&mut self.state.files).set(k, v.clone());
            }
        }
        let (copies, map) = copies(els, at.0 - b.0, at.1 - b.1, self.is_note(), cx);
        self.state.elements.extend(copies.into_iter().map(Arc::new));
        self.sync_indices(cx);
        top.iter().filter_map(|i| map.iter().find(|(a, _)| a == i).map(|(_, b)| b.clone())).collect()
    }
}

/// New elements copied from `objs`: new ids, seeds and groups, moved by
/// (dx, dy); links between them (bound text, arrow bindings) follow, links
/// to elements not copied are dropped. Returns them and (old id, new id).
fn copies(objs: Vec<Obj>, dx: f64, dy: f64, note: bool, cx: &mut Cx) -> (Vec<Obj>, Vec<(String, String)>) {
    let mut map: Vec<(String, String)> = Vec::new();
    let mut groups: Vec<(String, String)> = Vec::new();
    let mut out: Vec<Obj> = objs
        .into_iter()
        .map(|mut o| {
            let old = o.str("id").unwrap_or_default().to_string();
            let new = cx.id(if note && o.str("type") == Some("text") { 8 } else { 21 });
            map.push((old, new.clone()));
            o.set_str("id", &new);
            o.set_num("x", o.num("x").unwrap_or(0.0) + dx);
            o.set_num("y", o.num("y").unwrap_or(0.0) + dy);
            o.set_num("seed", cx.int() as f64);
            o.set_num("version", 1.0);
            o.set_num("versionNonce", cx.int() as f64);
            o.set_num("updated", cx.now_ms as f64);
            o.set("isDeleted", Json::Bool(false));
            if o.get("index").is_some() {
                o.set("index", Json::Null);
            }
            if let Some(Json::Arr(g)) = o.get_mut("groupIds") {
                for x in g.iter_mut() {
                    if let Json::Str(s) = x {
                        let n = if let Some((_, n)) = groups.iter().find(|(a, _)| a == s) {
                            n.clone()
                        } else {
                            let n = cx.id(21);
                            groups.push((s.clone(), n.clone()));
                            n
                        };
                        *s = n;
                    }
                }
            }
            o
        })
        .collect();
    let rename = |s: &str| map.iter().find(|(a, _)| a == s).map(|(_, b)| b.clone());
    for o in &mut out {
        match o.str("containerId").map(&rename) {
            Some(Some(c)) => o.set_str("containerId", &c),
            Some(None) => o.set("containerId", Json::Null),
            None => {}
        }
        if let Some(Json::Arr(list)) = o.get_mut("boundElements") {
            list.retain_mut(|b| match b.as_obj_mut() {
                Some(b) => match b.str("id").and_then(rename) {
                    Some(n) => {
                        b.set_str("id", &n);
                        true
                    }
                    None => false,
                },
                None => false,
            });
        }
        for k in ["startBinding", "endBinding"] {
            let t = o.obj(k).and_then(|b| b.str("elementId")).map(str::to_string);
            if let Some(t) = t {
                match rename(&t) {
                    Some(n) => {
                        if let Some(b) = o.get_mut(k).and_then(Json::as_obj_mut) {
                            b.set_str("elementId", &n);
                        }
                    }
                    None => o.set(k, Json::Null),
                }
            }
        }
    }
    (out, map)
}

#[cfg(test)]
mod tests;
