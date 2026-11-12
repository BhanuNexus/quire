// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The Excalidraw drawing editor (0.13, docs/design/0.13-excalidraw.md): a
//! surface for `.excalidraw` files and `.excalidraw.md` drawing notes.
//!
//! Tools: select (move, resize, rotate, marquee), hand, rectangle, diamond,
//! ellipse, arrow and line (bound to shapes at their ends), freehand, text
//! (in shapes too), pictures from the vault, and the eraser. Style: stroke
//! and fill colours, fill style, stroke width and style, roughness, edges,
//! arrowheads, opacity, font and size, alignment. Undo and redo, copy and
//! paste (Excalidraw's clipboard JSON), duplicate, group, layers, align,
//! lock, element links (`[[Note]]`, Ctrl+click or the ↗ badge).
//!
//! The file model is `quire_canvas::drawing` (every field kept); saving is
//! `quire_app::drawings::save_drawing` (checked against the text last read,
//! history, encrypted in a locked folder), 0.6 s after the last change and
//! when the editor closes. Drawn in a clean style: no hand-drawn wobble.

mod paint;

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use egui::{Align2, Color32, CursorIcon, FontId, Id, Key, Pos2, Rect, RichText, Sense, Stroke, StrokeKind, TextureHandle, UiBuilder, Vec2};
use quire_app::Workspace;
use quire_app::drawings::{self, Saved};
use quire_app::vault::{NotePath, Vault};
use quire_app::workspace::NoticeLevel;
use quire_canvas::drawing::{self, Align, Cx, Drawing, History, Style, Z};
use quire_canvas::excalidraw;
use quire_canvas::ojson::Json;

use self::paint::View;
use crate::panels::PanelOut;
use crate::surfaces::Surface;
use crate::theme::Palette;

const SAVE_AFTER: Duration = Duration::from_millis(600);
/// After a failed save, the next try.
const RETRY_AFTER: Duration = Duration::from_secs(10);
const MIN_ZOOM: f32 = 0.1;
const MAX_ZOOM: f32 = 10.0;
/// Excalidraw's grid.
const GRID: f64 = 20.0;
/// Meshes are made again this long after the zoom last changed.
const ZOOM_REST: Duration = Duration::from_millis(150);
const PANEL_W: f32 = 228.0;

/// A tool, with its Excalidraw keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Tool {
    Select,
    Hand,
    Rectangle,
    Diamond,
    Ellipse,
    Arrow,
    Line,
    Draw,
    Text,
    Image,
    Eraser,
}

impl Tool {
    /// (tool, icon, name, keys)
    pub(crate) const ALL: [(Self, &'static str, &'static str, &'static str); 11] = [
        (Self::Hand, "✋", "Hand (pan)", "H"),
        (Self::Select, "↖", "Select", "V or 1"),
        (Self::Rectangle, "□", "Rectangle", "R or 2"),
        (Self::Diamond, "◇", "Diamond", "D or 3"),
        (Self::Ellipse, "◯", "Ellipse", "O or 4"),
        (Self::Arrow, "→", "Arrow", "A or 5"),
        (Self::Line, "—", "Line", "L or 6"),
        (Self::Draw, "✏", "Draw", "P or 7"),
        (Self::Text, "A", "Text", "T or 8"),
        (Self::Image, "🖼", "Picture from the vault", "9"),
        (Self::Eraser, "⌫", "Eraser", "E or 0"),
    ];

    /// The element type it makes.
    const fn kind(self) -> Option<&'static str> {
        match self {
            Self::Rectangle => Some("rectangle"),
            Self::Diamond => Some("diamond"),
            Self::Ellipse => Some("ellipse"),
            Self::Arrow => Some("arrow"),
            Self::Line => Some("line"),
            Self::Draw => Some("freedraw"),
            _ => None,
        }
    }

    fn for_key(k: Key) -> Option<Self> {
        Some(match k {
            Key::V | Key::Num1 => Self::Select,
            Key::H => Self::Hand,
            Key::R | Key::Num2 => Self::Rectangle,
            Key::D | Key::Num3 => Self::Diamond,
            Key::O | Key::Num4 => Self::Ellipse,
            Key::A | Key::Num5 => Self::Arrow,
            Key::L | Key::Num6 => Self::Line,
            Key::P | Key::X | Key::Num7 => Self::Draw,
            Key::T | Key::Num8 => Self::Text,
            Key::Num9 => Self::Image,
            Key::E | Key::Num0 => Self::Eraser,
            _ => return None,
        })
    }
}

/// Keyboard shortcuts with modifiers (Excalidraw's, where Quire's own don't
/// take them; `keys_dont_conflict` checks this against Quire's keymap).
/// (keys as Settings writes them, what they do)
pub(crate) const SHORTCUTS: &[(&str, &str)] = &[
    ("Ctrl+Z", "Undo"),
    ("Ctrl+Shift+Z", "Redo"),
    ("Ctrl+Y", "Redo"),
    ("Ctrl+A", "Select all"),
    ("Ctrl+D", "Duplicate"),
    ("Ctrl+Shift+L", "Lock or unlock"),
    ("Ctrl+CloseBracket", "Bring forward"),
    ("Ctrl+OpenBracket", "Send backward"),
    ("Ctrl+Shift+ArrowLeft", "Align left"),
    ("Ctrl+Shift+ArrowRight", "Align right"),
    ("Ctrl+Shift+ArrowUp", "Align top"),
    ("Ctrl+Shift+ArrowDown", "Align bottom"),
    ("Ctrl+0", "Zoom to 100 %"),
    ("Shift+1", "Zoom to fit"),
    ("Ctrl+Quote", "Grid on or off"),
];

/// What a drag is doing.
#[derive(Debug, Clone)]
enum Drag {
    Pan,
    Move { last: (f64, f64), moved: bool, dup: bool },
    Resize { id: String, handle: usize, start: (f64, f64, f64, f64), angle: f64, aspect: f64 },
    Rotate { ids: Vec<String>, center: (f64, f64), last: f64 },
    Marquee { start: (f64, f64), now: (f64, f64), additive: bool },
    Create { id: String, start: (f64, f64) },
    Point { id: String, index: usize },
    Erase { hits: Vec<String> },
}

/// The file as changed elsewhere, while there were edits here.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Elsewhere {
    Changed(String),
    Deleted,
}

impl Elsewhere {
    fn from(theirs: Option<String>) -> Self {
        theirs.map_or(Self::Deleted, Self::Changed)
    }

    fn text(&self) -> Option<&str> {
        match self {
            Self::Changed(t) => Some(t),
            Self::Deleted => None,
        }
    }
}

/// A text being typed in.
#[derive(Debug, Clone)]
struct Typing {
    id: String,
    buf: String,
    /// Made for this typing: removed again when left empty.
    fresh: bool,
    focused: bool,
}

thread_local! {
    /// Paths the person chose to see as Markdown (this session).
    static AS_MARKDOWN: std::cell::RefCell<HashSet<NotePath>> = std::cell::RefCell::new(HashSet::new());
}

/// The editor for one drawing.
pub(crate) struct DrawingEditor {
    path: NotePath,
    loaded: bool,
    d: Option<Drawing>,
    /// The file can't be read as a drawing: shown, never written.
    error: Option<String>,
    /// The file's text as last read or written: saves are checked against it.
    disk_text: String,
    disk_gen: u64,
    dirty: bool,
    last_edit: Instant,
    /// Changed elsewhere while there were edits here.
    conflict: Option<Elsewhere>,
    save_failed: Option<Instant>,
    hist: History,
    cx: Cx,
    offset: Vec2,
    zoom: f32,
    fitted: bool,
    zoomed_at: Option<Instant>,
    tool: Tool,
    style: Style,
    sel: Vec<String>,
    drag: Option<Drag>,
    typing: Option<Typing>,
    grid: bool,
    snap: bool,
    cache: paint::Cache,
    images: HashMap<String, Option<TextureHandle>>,
    panel: bool,
    picker: Option<String>,
    /// The link being typed in the panel: (element, its link then, text).
    link_buf: Option<(String, String, String)>,
    vault: Option<Vault>,
    ctx: Option<egui::Context>,
    locked_folder: bool,
    done: bool,
    last_frame_ms: f32,
    drawn: paint::Drawn,
    /// The canvas's rect last frame.
    last_rect: Rect,
}

impl DrawingEditor {
    pub(crate) fn new(path: &NotePath) -> Self {
        Self {
            path: path.clone(),
            loaded: false,
            d: None,
            error: None,
            disk_text: String::new(),
            disk_gen: 0,
            dirty: false,
            last_edit: Instant::now(),
            conflict: None,
            save_failed: None,
            hist: History::default(),
            cx: Cx::now(),
            offset: Vec2::ZERO,
            zoom: 1.0,
            fitted: false,
            zoomed_at: None,
            tool: Tool::Select,
            style: Style::default(),
            sel: Vec::new(),
            drag: None,
            typing: None,
            grid: false,
            snap: false,
            cache: paint::Cache::default(),
            images: HashMap::new(),
            panel: true,
            picker: None,
            link_buf: None,
            vault: None,
            ctx: None,
            locked_folder: false,
            done: false,
            last_frame_ms: 0.0,
            drawn: paint::Drawn::default(),
            last_rect: Rect::NOTHING,
        }
    }

    // ------------------------------------------------------------ file

    fn load(&mut self, ws: &Workspace) {
        self.loaded = true;
        self.vault = ws.vault().cloned();
        self.disk_gen = ws.features.canvas.disk_generation(&self.path);
        self.locked_folder = ws.vault().is_some_and(|v| v.locked_folder(&self.path).is_some());
        match drawings::read_drawing(ws, &self.path) {
            Ok(t) => {
                match Drawing::parse(&t) {
                    Ok(d) => {
                        self.grid = d.state.elements.is_empty() || self.grid;
                        self.d = Some(d);
                        self.error = None;
                    }
                    Err(e) => self.error = Some(e),
                }
                self.disk_text = t;
            }
            Err(e) => self.error = Some(e),
        }
    }

    /// Read the file again (changed elsewhere, no edits here).
    fn reload(&mut self, text: String) {
        match Drawing::parse(&text) {
            Ok(d) => {
                let ids: HashSet<String> = d.state.elements.iter().filter_map(|e| e.str("id").map(str::to_string)).collect();
                self.sel.retain(|s| ids.contains(s));
                self.d = Some(d);
                self.error = None;
            }
            Err(e) => self.error = Some(e),
        }
        self.disk_text = text;
        self.hist.clear();
        self.cache.clear();
        self.dirty = false;
        self.typing = None;
        self.drag = None;
    }

    /// Take in a change made elsewhere: reload, or hold it as a conflict.
    fn absorb(&mut self, ws: &mut Workspace) {
        let generation = ws.features.canvas.disk_generation(&self.path);
        if generation == self.disk_gen {
            return;
        }
        self.disk_gen = generation;
        let theirs = drawings::read_drawing(ws, &self.path).ok();
        if theirs.as_deref() == Some(self.disk_text.as_str()) {
            return; // our own save
        }
        match theirs {
            Some(t) if !self.dirty && self.conflict.is_none() => {
                self.reload(t);
                ws.notify(NoticeLevel::Info, format!("Reloaded “{}” (changed on disk)", self.title_text()));
            }
            theirs => {
                self.conflict = Some(Elsewhere::from(theirs));
                ws.notify(
                    NoticeLevel::Warn,
                    format!("“{}” changed elsewhere while you were editing it: choose what to keep", self.title_text()),
                );
            }
        }
    }

    fn title_text(&self) -> String {
        let n = self.path.file_name();
        n.strip_suffix(".md").unwrap_or(n).to_string()
    }

    fn save(&mut self, ws: &mut Workspace) {
        let Some(d) = &self.d else { return };
        if self.error.is_some() || self.conflict.is_some() {
            return;
        }
        let text = d.to_text();
        if text == self.disk_text {
            self.dirty = false;
            return;
        }
        match drawings::save_drawing(ws, &self.path, &self.disk_text, &text) {
            Saved::Written => {
                self.disk_text = text;
                self.dirty = false;
                self.save_failed = None;
                self.refresh_pictures(ws);
            }
            Saved::ChangedElsewhere(theirs) => {
                self.conflict = Some(Elsewhere::from(theirs));
                ws.notify(NoticeLevel::Warn, format!("“{}” changed elsewhere: choose what to keep", self.title_text()));
            }
            Saved::Failed(e) => {
                if self.save_failed.is_none() {
                    ws.notify(NoticeLevel::Error, format!("Couldn't save “{}”: {e}", self.title_text()));
                }
                self.save_failed = Some(Instant::now());
            }
        }
    }

    /// Pictures of this drawing elsewhere (embeds in notes) are drawn again.
    fn refresh_pictures(&self, ws: &Workspace) {
        if let (Some(ctx), Some(abs)) = (&self.ctx, ws.vault().and_then(|v| v.storage().absolute(&self.path))) {
            ctx.forget_image(&format!("file://{}", abs.display()));
        }
    }

    /// Keep this version and put theirs beside it, or the other way round.
    fn resolve(&mut self, ws: &mut Workspace, keep_mine: bool) {
        let Some(theirs) = self.conflict.take() else { return };
        let mine = self.d.as_ref().map(Drawing::to_text);
        let (copy_text, label) = if keep_mine { (theirs.text().map(str::to_string), "changed elsewhere") } else { (mine, "my edits") };
        if let Some(t) = copy_text {
            let Some(copy) = drawings::copy_path(ws, &self.path, label) else { return };
            let Some(vault) = ws.vault().cloned() else { return };
            if let Err(e) = vault.write(&copy, &t) {
                ws.notify(NoticeLevel::Error, format!("Couldn't keep a copy: {e}"));
                self.conflict = Some(theirs);
                return;
            }
            ws.notify(NoticeLevel::Info, format!("The other version is in “{}”", copy.file_name()));
        }
        if keep_mine {
            // Save ours over theirs (the copy holds theirs).
            self.disk_text = theirs.text().unwrap_or_default().to_string();
            self.dirty = true;
            self.save(ws);
        } else if let Elsewhere::Changed(t) = theirs {
            self.reload(t);
        }
    }

    // ------------------------------------------------------------ edits

    fn checkpoint(&mut self) {
        if let Some(d) = &self.d {
            self.hist.checkpoint(&d.state);
        }
        self.cx.tick();
    }

    fn touch(&mut self) {
        self.dirty = true;
        self.last_edit = Instant::now();
    }

    fn undo(&mut self, redo: bool) {
        let Some(d) = &mut self.d else { return };
        let done = if redo { self.hist.redo(&mut d.state) } else { self.hist.undo(&mut d.state) };
        if done {
            let live: HashSet<&str> = drawing::live(&d.state.elements).filter_map(|e| e.str("id")).collect();
            self.sel.retain(|s| live.contains(s.as_str()));
            self.typing = None;
            self.touch();
        }
    }

    /// Run an edit on the selection as one undo step.
    fn on_sel(&mut self, f: impl FnOnce(&mut Drawing, &[String], &mut Cx)) {
        if self.sel.is_empty() || self.d.is_none() {
            return;
        }
        self.checkpoint();
        let sel = self.unlocked_sel();
        if let Some(d) = &mut self.d {
            f(d, &sel, &mut self.cx);
        }
        self.touch();
    }

    /// The selection without locked elements.
    fn unlocked_sel(&self) -> Vec<String> {
        let Some(d) = &self.d else { return Vec::new() };
        self.sel.iter().filter(|id| d.get(id).is_some_and(|e| e.bool("locked") != Some(true))).cloned().collect()
    }

    /// Set a style field on the selection (bound texts take the font and
    /// colour fields), and make it the default for new elements.
    fn set_style(&mut self, key: &str, v: &Json) {
        match (key, v) {
            ("strokeColor", Json::Str(s)) => self.style.stroke.clone_from(s),
            ("backgroundColor", Json::Str(s)) => self.style.background.clone_from(s),
            ("fillStyle", Json::Str(s)) => self.style.fill_style.clone_from(s),
            ("strokeStyle", Json::Str(s)) => self.style.stroke_style.clone_from(s),
            ("textAlign", Json::Str(s)) => self.style.text_align.clone_from(s),
            ("strokeWidth", _) => self.style.stroke_width = v.as_f64().unwrap_or(2.0),
            ("roughness", _) => self.style.roughness = v.as_f64().unwrap_or(1.0),
            ("opacity", _) => self.style.opacity = v.as_f64().unwrap_or(100.0),
            ("fontSize", _) => self.style.font_size = v.as_f64().unwrap_or(20.0),
            ("fontFamily", _) => self.style.font_family = v.as_f64().unwrap_or(5.0) as i64,
            ("startArrowhead", _) => self.style.start_arrow = v.as_str().map(str::to_string),
            ("endArrowhead", _) => self.style.end_arrow = v.as_str().map(str::to_string),
            _ => {}
        }
        if self.sel.is_empty() {
            return;
        }
        let text_key = matches!(key, "fontSize" | "fontFamily" | "textAlign" | "strokeColor" | "opacity");
        let sel = self.unlocked_sel();
        let Some(d) = &self.d else { return };
        let mut ids: Vec<String> = sel
            .iter()
            .filter(|id| {
                let k = d.get(id).and_then(|e| e.str("type")).unwrap_or_default();
                match key {
                    "fontSize" | "fontFamily" | "textAlign" => k == "text",
                    "startArrowhead" | "endArrowhead" => k == "arrow",
                    _ => true,
                }
            })
            .cloned()
            .collect();
        if text_key {
            ids.extend(
                drawing::live(&d.state.elements)
                    .filter(|e| e.str("containerId").is_some_and(|c| sel.iter().any(|s| s == c)))
                    .filter_map(|e| e.str("id").map(str::to_string)),
            );
        }
        if ids.is_empty() {
            return;
        }
        self.checkpoint();
        let mut measured: Vec<(String, String)> = Vec::new();
        if let Some(d) = &mut self.d {
            d.set_field(&ids, key, v, &mut self.cx);
            if matches!(key, "fontSize" | "fontFamily") {
                measured = ids
                    .iter()
                    .filter_map(|id| {
                        let e = d.get(id)?;
                        (e.str("type") == Some("text"))
                            .then(|| (id.clone(), e.str("rawText").or_else(|| e.str("originalText")).unwrap_or_default().to_string()))
                    })
                    .collect();
            }
        }
        // A new font size or family: the text's box is measured again.
        for (id, raw) in measured {
            self.commit_text_size(&id, &raw);
        }
        self.touch();
    }

    /// Measure a text element's text at its size and set it.
    fn commit_text_size(&mut self, id: &str, raw: &str) {
        let Some(d) = &mut self.d else { return };
        let Some(e) = d.get(id) else { return };
        let size = e.num("fontSize").unwrap_or(20.0);
        let fam = e.num("fontFamily").unwrap_or(5.0) as i64;
        let lh = e.num("lineHeight").unwrap_or(1.25);
        let m = self.ctx.as_ref().map_or_else(|| approx_measure(raw, size, lh), |c| measure(c, raw, size, fam, lh));
        d.set_text(id, raw, m, &mut self.cx);
    }

    fn select_all(&mut self) {
        if let Some(d) = &self.d {
            self.sel = drawing::live(&d.state.elements)
                .filter(|e| e.get("containerId").is_none_or(Json::is_null))
                .filter_map(|e| e.str("id").map(str::to_string))
                .collect();
        }
    }

    fn delete_sel(&mut self) {
        let gone = self.unlocked_sel();
        if gone.is_empty() {
            return;
        }
        self.on_sel(Drawing::erase);
        // Locked elements stay (and stay selected).
        self.sel.retain(|s| !gone.contains(s));
    }

    fn duplicate(&mut self) {
        let sel = self.sel.clone();
        if sel.is_empty() {
            return;
        }
        self.checkpoint();
        if let Some(d) = &mut self.d {
            self.sel = d.duplicate(&sel, 10.0, 10.0, &mut self.cx);
        }
        self.touch();
    }

    fn copy(&self) -> Option<String> {
        let d = self.d.as_ref()?;
        let mut ids = self.sel.clone();
        ids.extend(
            drawing::live(&d.state.elements)
                .filter(|e| e.str("containerId").is_some_and(|c| self.sel.iter().any(|s| s == c)))
                .filter_map(|e| e.str("id").map(str::to_string)),
        );
        let els: Vec<drawing::El> =
            d.state.elements.iter().filter(|e| e.str("id").is_some_and(|i| ids.iter().any(|s| s == i))).cloned().collect();
        (!els.is_empty()).then(|| drawing::clipboard_json(&els, &d.state.files))
    }

    fn paste(&mut self, text: &str, at: (f64, f64)) {
        if self.d.is_none() {
            return;
        }
        if let Some((els, files)) = drawing::from_clipboard(text) {
            self.checkpoint();
            if let Some(d) = &mut self.d {
                self.sel = d.paste(els, &files, at, &mut self.cx);
            }
            self.touch();
            return;
        }
        // Plain text: a new text element.
        let t = text.trim_end();
        if t.is_empty() {
            return;
        }
        self.checkpoint();
        let Some(d) = &mut self.d else { return };
        let e = d.new_element("text", at.0, at.1, &self.style, &mut self.cx);
        let id = d.add(e, &mut self.cx);
        self.commit_text_size(&id, t);
        self.sel = vec![id];
        self.touch();
    }

    /// The text bound inside `container`.
    fn bound_text(&self, container: &str) -> Option<String> {
        let d = self.d.as_ref()?;
        drawing::live(&d.state.elements).find(|e| e.str("containerId") == Some(container)).and_then(|e| e.str("id")).map(str::to_string)
    }

    /// Start typing in element `id` (a text), or in a shape (its bound text, made if missing).
    fn edit_text_of(&mut self, id: &str) {
        let Some(d) = &self.d else { return };
        let Some(e) = d.get(id) else { return };
        if e.bool("locked") == Some(true) {
            return;
        }
        let kind = e.str("type").unwrap_or_default().to_string();
        if kind == "text" {
            let buf = e.str("rawText").or_else(|| e.str("originalText")).or_else(|| e.str("text")).unwrap_or_default().to_string();
            self.typing = Some(Typing { id: id.to_string(), buf, fresh: false, focused: false });
            return;
        }
        if !matches!(kind.as_str(), "rectangle" | "ellipse" | "diamond" | "arrow") {
            return;
        }
        if let Some(t) = self.bound_text(id) {
            self.edit_text_of(&t);
            return;
        }
        // A new text inside the shape.
        self.checkpoint();
        let Some(d) = &mut self.d else { return };
        let (cx0, cy0) = drawing::center(d.get(id).expect("checked"));
        let mut style = self.style.clone();
        style.text_align = "center".into();
        let mut t = d.new_element("text", cx0, cy0, &style, &mut self.cx);
        t.set_str("verticalAlign", "middle");
        t.set_str("containerId", id);
        let tid = d.add(t, &mut self.cx);
        let link = Json::Obj(quire_canvas::ojson::Obj(vec![("type".into(), Json::str("text")), ("id".into(), Json::str(&tid))]));
        d.edit(id, &mut self.cx, |o| {
            let mut list = o.arr("boundElements").map(<[Json]>::to_vec).unwrap_or_default();
            list.push(link);
            o.set("boundElements", Json::Arr(list));
        });
        self.typing = Some(Typing { id: tid, buf: String::new(), fresh: true, focused: false });
    }

    /// Finish typing: set the text, or drop a new text left empty.
    fn finish_typing(&mut self) {
        let Some(t) = self.typing.take() else { return };
        if t.buf.trim().is_empty() {
            if t.fresh {
                if let Some(d) = &mut self.d {
                    self.hist.cancel(&mut d.state);
                }
                self.sel.retain(|s| *s != t.id);
            } else {
                self.checkpoint();
                if let Some(d) = &mut self.d {
                    d.erase(std::slice::from_ref(&t.id), &mut self.cx);
                }
                self.touch();
            }
            return;
        }
        let same = self
            .d
            .as_ref()
            .and_then(|d| d.get(&t.id))
            .is_some_and(|e| e.str("rawText").or_else(|| e.str("originalText")) == Some(t.buf.as_str()));
        if same && !t.fresh {
            return;
        }
        if !t.fresh {
            self.checkpoint();
        }
        self.commit_text_size(&t.id, &t.buf);
        self.touch();
    }

    // ------------------------------------------------------------ view

    fn view(&self, rect: Rect) -> View {
        View { rect, offset: self.offset, zoom: self.zoom }
    }

    fn fit(&mut self, rect: Rect) {
        let b = self.d.as_ref().and_then(|d| drawing::union_box(drawing::live(&d.state.elements)));
        let (x, y, w, h) = b.unwrap_or((-300.0, -200.0, 600.0, 400.0));
        let (w, h) = (w.max(1.0) as f32 + 120.0, h.max(1.0) as f32 + 120.0);
        self.zoom = (rect.width() / w).min(rect.height() / h).clamp(MIN_ZOOM, 1.0);
        let c = Vec2::new(x as f32 + (w - 120.0) / 2.0, y as f32 + (h - 120.0) / 2.0);
        self.offset = c - rect.size() / 2.0 / self.zoom;
        self.fitted = true;
        self.zoomed_at = Some(Instant::now());
    }

    fn zoom_at(&mut self, rect: Rect, at: Pos2, z: f32) {
        let before = self.view(rect).to_scene(at);
        self.zoom = z.clamp(MIN_ZOOM, MAX_ZOOM);
        let after = self.view(rect).to_scene(at);
        self.offset += Vec2::new((before.0 - after.0) as f32, (before.1 - after.1) as f32);
        self.zoomed_at = Some(Instant::now());
    }

    fn snapped(&self, p: (f64, f64)) -> (f64, f64) {
        if self.snap { ((p.0 / GRID).round() * GRID, (p.1 / GRID).round() * GRID) } else { p }
    }

    /// The topmost element under `p` (scene units).
    fn hit(&self, p: (f64, f64)) -> Option<String> {
        let d = self.d.as_ref()?;
        let tol = f64::from(6.0 / self.zoom);
        d.state
            .elements
            .iter()
            .rev()
            .filter(|e| e.bool("isDeleted") != Some(true))
            .find(|e| drawing::hit(e, p, tol))
            .and_then(|e| e.str("containerId").filter(|_| e.str("type") == Some("text")).or_else(|| e.str("id")))
            .map(str::to_string)
    }

    /// What clicking `id` selects: its outermost group.
    fn select_target(&self, id: &str) -> Vec<String> {
        self.d.as_ref().map_or_else(|| vec![id.to_string()], |d| d.group_of(id))
    }

    /// Selection handles of the one selected element (screen): eight around
    /// its box (rotated with it), and rotation above. `None`: no handles.
    fn handles(&self, view: View) -> Option<(Vec<Pos2>, Pos2)> {
        let [id] = self.sel.as_slice() else { return None };
        let e = self.d.as_ref()?.get(id)?;
        if e.bool("locked") == Some(true) {
            return None;
        }
        let kind = e.str("type").unwrap_or_default();
        if matches!(kind, "arrow" | "line") && drawing::points(e).len() == 2 {
            return None; // end points instead
        }
        let (x, y, w, h) = drawing::bbox(e);
        let a = e.num("angle").unwrap_or(0.0);
        let c = (x + w / 2.0, y + h / 2.0);
        let rot = |p: (f64, f64)| {
            let (s, co) = a.sin_cos();
            let (dx, dy) = (p.0 - c.0, p.1 - c.1);
            view.to_screen((c.0 + dx * co - dy * s, c.1 + dx * s + dy * co))
        };
        let pts = [
            (x, y),
            (x + w / 2.0, y),
            (x + w, y),
            (x + w, y + h / 2.0),
            (x + w, y + h),
            (x + w / 2.0, y + h),
            (x, y + h),
            (x, y + h / 2.0),
        ];
        let hs: Vec<Pos2> = pts.iter().map(|p| rot(*p)).collect();
        let up = f64::from(24.0 / view.zoom);
        Some((hs, rot((x + w / 2.0, y - up))))
    }

    /// The end points of a two-point arrow or line (screen), if one is selected.
    fn end_points(&self, view: View) -> Option<(String, [Pos2; 2])> {
        let [id] = self.sel.as_slice() else { return None };
        let e = self.d.as_ref()?.get(id)?;
        if !matches!(e.str("type"), Some("arrow" | "line")) || e.bool("locked") == Some(true) {
            return None;
        }
        let pts = drawing::points(e);
        let (ox, oy) = (e.num("x").unwrap_or(0.0), e.num("y").unwrap_or(0.0));
        (pts.len() == 2)
            .then(|| (id.clone(), [view.to_screen((ox + pts[0].0, oy + pts[0].1)), view.to_screen((ox + pts[1].0, oy + pts[1].1))]))
    }

    /// A shape an arrow end at `p` binds to (not `except`).
    fn bind_target(&self, p: (f64, f64), except: &str) -> Option<String> {
        let d = self.d.as_ref()?;
        let tol = f64::from(8.0 / self.zoom);
        d.state
            .elements
            .iter()
            .rev()
            .filter(|e| e.bool("isDeleted") != Some(true) && e.str("id") != Some(except))
            .filter(|e| matches!(e.str("type"), Some("rectangle" | "ellipse" | "diamond" | "image" | "text" | "frame")))
            .find(|e| {
                let (x, y, w, h) = drawing::aabb(e);
                p.0 >= x - tol && p.0 <= x + w + tol && p.1 >= y - tol && p.1 <= y + h + tol
            })
            .and_then(|e| e.str("id"))
            .map(str::to_string)
    }

    // ------------------------------------------------------------ pictures

    fn texture(
        images: &mut HashMap<String, Option<TextureHandle>>,
        ws: &Workspace,
        path: &NotePath,
        d: &Drawing,
        ctx: &egui::Context,
        id: &str,
    ) -> Option<TextureHandle> {
        images
            .entry(id.to_string())
            .or_insert_with(|| {
                let bytes = picture_bytes(ws, path, &d.file_ref(id)?)?;
                let img = image::load_from_memory(&bytes).ok()?.to_rgba8();
                let size = [img.width() as usize, img.height() as usize];
                let ci = egui::ColorImage::from_rgba_unmultiplied(size, img.as_raw());
                Some(ctx.load_texture(format!("quire-drawing-{id}"), ci, egui::TextureOptions::LINEAR))
            })
            .clone()
    }

    /// Add a picture from the vault in the middle of the view.
    fn add_picture(&mut self, ws: &Workspace, file: &NotePath, rect: Rect) {
        let Some(vault) = ws.vault() else { return };
        let mime = drawings::picture_type(file.as_str()).unwrap_or("image/png");
        let bytes =
            if vault.locked_folder(file).is_some() { ws.read_locked_attachment(file) } else { vault.storage().read_bytes(file).ok() };
        let Some(bytes) = bytes else {
            return;
        };
        let (w, h) = image::load_from_memory(&bytes).map_or((200.0, 150.0), |i| (f64::from(i.width()), f64::from(i.height())));
        let s = (400.0 / w.max(h)).min(1.0);
        let (w, h) = (w * s, h * s);
        let center = self.view(rect).to_scene(rect.center());
        self.checkpoint();
        let Some(d) = &mut self.d else { return };
        let data_url = (!d.is_note()).then(|| {
            use base64::Engine as _;
            format!("data:{mime};base64,{}", base64::engine::general_purpose::STANDARD.encode(&bytes))
        });
        let fid = d.add_file(file.as_str(), mime, data_url, &mut self.cx);
        let mut e = d.new_element("image", center.0 - w / 2.0, center.1 - h / 2.0, &self.style, &mut self.cx);
        e.set_num("width", w);
        e.set_num("height", h);
        e.set_str("fileId", &fid);
        let id = d.add(e, &mut self.cx);
        self.sel = vec![id];
        self.tool = Tool::Select;
        self.touch();
    }

    // ------------------------------------------------------------ links

    /// Open an element's link: a note, a file, a heading, or a web address.
    fn open_link(ws: &mut Workspace, ctx: &egui::Context, link: &str, out: &mut PanelOut) {
        let l = link.trim();
        if let Some(inner) = l.strip_prefix("[[").and_then(|x| x.split("]]").next()) {
            let target = inner.split('|').next().unwrap_or(inner);
            let (note, heading) = target.split_once('#').map_or((target, None), |(n, h)| (n, Some(h)));
            if quire_app::canvas_state::open_file_link(ws, note, heading) {
                return;
            }
            out.jumped = ws.open_link(note, heading).is_some();
            return;
        }
        if l.starts_with("http://") || l.starts_with("https://") || l.starts_with("mailto:") {
            ctx.open_url(egui::OpenUrl::new_tab(l));
        } else if !l.is_empty() {
            out.jumped = ws.open_link(l, None).is_some();
        }
    }

    // ------------------------------------------------------------ the frame

    fn toolbar(&mut self, ui: &mut egui::Ui, ws: &mut Workspace, pal: &Palette, out: &mut PanelOut) {
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing.x = 2.0;
            for (t, icon, name, keys) in Tool::ALL {
                let r = ui.selectable_label(self.tool == t, RichText::new(icon).size(16.0)).on_hover_text(format!("{name} — {keys}"));
                if r.clicked() {
                    self.finish_typing();
                    self.tool = t;
                    if t == Tool::Image {
                        self.picker = Some(String::new());
                    }
                }
            }
            ui.separator();
            if ui.add_enabled(self.hist.can_undo(), egui::Button::new("⟲")).on_hover_text("Undo — Ctrl+Z").clicked() {
                self.undo(false);
            }
            if ui.add_enabled(self.hist.can_redo(), egui::Button::new("⟳")).on_hover_text("Redo — Ctrl+Shift+Z").clicked() {
                self.undo(true);
            }
            ui.separator();
            ui.toggle_value(&mut self.grid, "Grid").on_hover_text("Show the grid — Ctrl+'");
            ui.toggle_value(&mut self.snap, "Snap").on_hover_text("Snap to the grid");
            if ui.button(format!("{:.0} %", self.zoom * 100.0)).on_hover_text("Zoom to 100 % — Ctrl+0 · Zoom to fit: Shift+1").clicked()
            {
                self.zoom = 1.0;
                self.zoomed_at = Some(Instant::now());
            }
            if ui.button("Fit").clicked() {
                self.fitted = false;
            }
            ui.toggle_value(&mut self.panel, "Style").on_hover_text("The properties panel");
            let keys = Tool::ALL
                .iter()
                .map(|(_, _, name, k)| format!("{k}: {name}"))
                .chain(SHORTCUTS.iter().map(|(k, what)| {
                    format!("{}: {what}", k.replace("CloseBracket", "]").replace("OpenBracket", "[").replace("Quote", "'"))
                }))
                .collect::<Vec<_>>()
                .join("\n");
            ui.label(RichText::new("Keys").color(pal.muted)).on_hover_text(keys);
            if self.d.as_ref().is_some_and(Drawing::is_note)
                && ui.button("Markdown").on_hover_text("Open the drawing note as Markdown").clicked()
            {
                self.finish_typing();
                self.save(ws);
                if !self.dirty {
                    AS_MARKDOWN.with_borrow_mut(|m| m.insert(self.path.clone()));
                    out.jumped = ws.open_note(&self.path).is_some();
                    self.done = out.jumped;
                }
            }
            let (status, col) = if self.conflict.is_some() {
                ("Changed elsewhere".to_string(), pal.warn)
            } else if self.save_failed.is_some() {
                ("Not saved".to_string(), pal.danger)
            } else if self.dirty {
                ("Editing…".to_string(), pal.muted)
            } else {
                ("Saved".to_string(), pal.muted)
            };
            let lock = if self.locked_folder { " · 🔒 encrypted" } else { "" };
            ui.label(RichText::new(format!("{status}{lock}")).size(11.0).color(col));
        });
    }

    fn conflict_bar(&mut self, ui: &mut egui::Ui, ws: &mut Workspace, pal: &Palette) {
        if self.conflict.is_none() {
            return;
        }
        egui::Frame::NONE.fill(pal.warn.gamma_multiply(0.15)).inner_margin(egui::Margin::symmetric(10, 6)).show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                let gone = self.conflict == Some(Elsewhere::Deleted);
                ui.label(
                    RichText::new(if gone {
                        "This drawing was deleted elsewhere."
                    } else {
                        "This drawing changed elsewhere while you were editing it."
                    })
                    .color(pal.strong),
                );
                if ui.button("Keep mine").on_hover_text("Save this version; the other is kept in a copy beside it").clicked() {
                    self.resolve(ws, true);
                }
                if !gone
                    && ui.button("Take theirs").on_hover_text("Show the other version; your edits are kept in a copy beside it").clicked()
                {
                    self.resolve(ws, false);
                }
            });
        });
    }

    fn panel_ui(&mut self, ui: &mut egui::Ui, ws: &mut Workspace, pal: &Palette, out: &mut PanelOut) {
        let d = self.d.as_ref();
        let first = self.sel.first().and_then(|id| d?.get(id)).cloned();
        let kinds: Vec<String> = self.sel.iter().filter_map(|id| d?.get(id)?.str("type").map(str::to_string)).collect();
        let creating = self.tool.kind().is_some() || self.tool == Tool::Text;
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 4.0;
            if first.is_none() && !creating {
                ui.label(RichText::new("Select something to style it, or pick a tool.").color(pal.muted).size(12.0));
                ui.add_space(8.0);
                ui.label(RichText::new("Double-click: add text. Ctrl+click a link to open it.").color(pal.muted).size(11.0));
                return;
            }
            let get_s = |k: &str, dflt: &str| first.as_ref().and_then(|e| e.str(k)).map_or_else(|| dflt.to_string(), str::to_string);
            let get_n = |k: &str, dflt: f64| first.as_ref().and_then(|e| e.num(k)).unwrap_or(dflt);
            let has = |k: &[&str]| kinds.iter().any(|x| k.contains(&x.as_str())) || (first.is_none() && creating);
            let mut set: Option<(&'static str, Json)> = None;
            let label = |ui: &mut egui::Ui, t: &str| {
                ui.add_space(4.0);
                ui.label(RichText::new(t).size(11.0).color(pal.muted));
            };
            label(ui, "Stroke");
            let stroke = get_s("strokeColor", &self.style.stroke);
            ui.horizontal_wrapped(|ui| {
                for c in ["#1e1e1e", "#e03131", "#2f9e44", "#1971c2", "#f08c00", "#9c36b5"] {
                    if swatch(ui, c, stroke == c, pal).clicked() {
                        set = Some(("strokeColor", Json::str(c)));
                    }
                }
            });
            if has(&["rectangle", "ellipse", "diamond", "line", "freedraw"]) || first.is_none() {
                label(ui, "Background");
                let bg = get_s("backgroundColor", &self.style.background);
                ui.horizontal_wrapped(|ui| {
                    for c in ["transparent", "#ffc9c9", "#b2f2bb", "#a5d8ff", "#ffec99", "#eebefa"] {
                        if swatch(ui, c, bg == c, pal).clicked() {
                            set = Some(("backgroundColor", Json::str(c)));
                        }
                    }
                });
                label(ui, "Fill");
                let fs = get_s("fillStyle", &self.style.fill_style);
                ui.horizontal_wrapped(|ui| {
                    for (v, t) in [("hachure", "Hachure"), ("cross-hatch", "Cross"), ("solid", "Solid")] {
                        if ui.selectable_label(fs == v, t).clicked() {
                            set = Some(("fillStyle", Json::str(v)));
                        }
                    }
                });
            }
            if !kinds.iter().all(|k| k == "text" || k == "image") || first.is_none() {
                label(ui, "Stroke width");
                let sw = get_n("strokeWidth", self.style.stroke_width);
                ui.horizontal_wrapped(|ui| {
                    for (v, t) in [(1.0, "Thin"), (2.0, "Bold"), (4.0, "Extra bold")] {
                        if ui.selectable_label((sw - v).abs() < 0.01, t).clicked() {
                            set = Some(("strokeWidth", Json::num(v)));
                        }
                    }
                });
                label(ui, "Stroke style");
                let ss = get_s("strokeStyle", &self.style.stroke_style);
                ui.horizontal_wrapped(|ui| {
                    for (v, t) in [("solid", "Solid"), ("dashed", "Dashed"), ("dotted", "Dotted")] {
                        if ui.selectable_label(ss == v, t).clicked() {
                            set = Some(("strokeStyle", Json::str(v)));
                        }
                    }
                });
                label(ui, "Sloppiness (kept for Excalidraw; drawn clean here)");
                let ro = get_n("roughness", self.style.roughness);
                ui.horizontal_wrapped(|ui| {
                    for (v, t) in [(0.0, "Architect"), (1.0, "Artist"), (2.0, "Cartoonist")] {
                        if ui.selectable_label((ro - v).abs() < 0.01, t).clicked() {
                            set = Some(("roughness", Json::num(v)));
                        }
                    }
                });
            }
            if has(&["rectangle", "diamond", "arrow", "line"]) {
                label(ui, "Edges");
                let round = first.as_ref().map_or(self.style.rounded, |e| e.get("roundness").is_some_and(|r| !r.is_null()));
                ui.horizontal_wrapped(|ui| {
                    if ui.selectable_label(!round, "Sharp").clicked() {
                        self.style.rounded = false;
                        set = Some(("roundness", Json::Null));
                    }
                    if ui.selectable_label(round, "Round").clicked() {
                        self.style.rounded = true;
                        let t = if first.as_ref().and_then(|e| e.str("type")) == Some("rectangle") { 3.0 } else { 2.0 };
                        set = Some(("roundness", Json::Obj(quire_canvas::ojson::Obj(vec![("type".into(), Json::num(t))]))));
                    }
                });
            }
            if has(&["arrow"]) {
                label(ui, "Arrowheads");
                for (key, cur) in [("startArrowhead", self.style.start_arrow.clone()), ("endArrowhead", self.style.end_arrow.clone())] {
                    let cur = first.as_ref().map_or(cur, |e| e.str(key).map(str::to_string));
                    ui.horizontal_wrapped(|ui| {
                        ui.label(RichText::new(if key == "startArrowhead" { "Start" } else { "End" }).size(11.0));
                        for (v, t) in [(None, "—"), (Some("arrow"), "→"), (Some("triangle"), "▶"), (Some("bar"), "|"), (Some("dot"), "●")]
                        {
                            if ui.selectable_label(cur.as_deref() == v, t).clicked() {
                                set = Some((key, v.map_or(Json::Null, Json::str)));
                            }
                        }
                    });
                }
            }
            if has(&["text"]) || self.tool == Tool::Text || kinds.iter().any(|k| matches!(k.as_str(), "rectangle" | "ellipse" | "diamond"))
            {
                let tfirst = first.as_ref().filter(|e| e.str("type") == Some("text"));
                label(ui, "Font");
                let fam = tfirst.as_ref().and_then(|e| e.num("fontFamily")).unwrap_or(self.style.font_family as f64) as i64;
                ui.horizontal_wrapped(|ui| {
                    for (v, t) in [(5, "Hand-drawn"), (6, "Normal"), (8, "Code")] {
                        if ui
                            .selectable_label(fam == v || (v == 5 && fam == 1) || (v == 6 && fam == 2) || (v == 8 && fam == 3), t)
                            .clicked()
                        {
                            set = Some(("fontFamily", Json::num(v as f64)));
                        }
                    }
                });
                label(ui, "Font size");
                let fsz = tfirst.as_ref().and_then(|e| e.num("fontSize")).unwrap_or(self.style.font_size);
                ui.horizontal_wrapped(|ui| {
                    for (v, t) in [(16.0, "S"), (20.0, "M"), (28.0, "L"), (36.0, "XL")] {
                        if ui.selectable_label((fsz - v).abs() < 0.01, t).clicked() {
                            set = Some(("fontSize", Json::num(v)));
                        }
                    }
                });
                label(ui, "Text align");
                let ta =
                    tfirst.as_ref().and_then(|e| e.str("textAlign").map(str::to_string)).unwrap_or_else(|| self.style.text_align.clone());
                ui.horizontal_wrapped(|ui| {
                    for (v, t) in [("left", "Left"), ("center", "Center"), ("right", "Right")] {
                        if ui.selectable_label(ta == v, t).clicked() {
                            set = Some(("textAlign", Json::str(v)));
                        }
                    }
                });
            }
            label(ui, "Opacity");
            let mut op = get_n("opacity", self.style.opacity);
            if ui.add(egui::Slider::new(&mut op, 0.0..=100.0).step_by(10.0).integer()).changed() {
                set = Some(("opacity", Json::num(op.round())));
            }
            if let Some((k, v)) = set {
                self.set_style(k, &v);
            }
            if first.is_none() {
                return;
            }
            label(ui, "Layers");
            ui.horizontal_wrapped(|ui| {
                for (z, t, tip) in [
                    (Z::Back, "To back", "Send to back"),
                    (Z::Backward, "Back", "Send backward — Ctrl+["),
                    (Z::Forward, "Forward", "Bring forward — Ctrl+]"),
                    (Z::Front, "To front", "Bring to front"),
                ] {
                    if ui.button(t).on_hover_text(tip).clicked() {
                        self.on_sel(|d, s, cx| d.reorder(s, z, cx));
                    }
                }
            });
            if self.sel.len() > 1 {
                label(ui, "Align");
                ui.horizontal_wrapped(|ui| {
                    for (a, t, tip) in [
                        (Align::Left, "Left", "Ctrl+Shift+←"),
                        (Align::CenterX, "Centre", "Centre horizontally"),
                        (Align::Right, "Right", "Ctrl+Shift+→"),
                        (Align::Top, "Top", "Ctrl+Shift+↑"),
                        (Align::CenterY, "Middle", "Centre vertically"),
                        (Align::Bottom, "Bottom", "Ctrl+Shift+↓"),
                    ] {
                        if ui.button(t).on_hover_text(tip).clicked() {
                            self.on_sel(|d, s, cx| d.align(s, a, cx));
                        }
                    }
                });
            }
            label(ui, "Actions");
            ui.horizontal_wrapped(|ui| {
                if ui.button("Duplicate").on_hover_text("Ctrl+D").clicked() {
                    self.duplicate();
                }
                if ui.button("Delete").on_hover_text("Delete").clicked() {
                    self.delete_sel();
                }
                if self.sel.len() > 1 && ui.button("Group").clicked() {
                    self.on_sel(|d, s, cx| {
                        d.group(s, cx);
                    });
                }
                let grouped = first.as_ref().and_then(|e| e.arr("groupIds")).is_some_and(|g| !g.is_empty());
                if grouped && ui.button("Ungroup").clicked() {
                    self.on_sel(Drawing::ungroup);
                }
                let locked = first.as_ref().and_then(|e| e.bool("locked")) == Some(true);
                if ui.button(if locked { "Unlock" } else { "Lock" }).on_hover_text("Ctrl+Shift+L").clicked() {
                    self.toggle_lock();
                }
            });
            if self.sel.len() == 1 {
                label(ui, "Link ([[Note]] or a web address)");
                let cur = first.as_ref().and_then(|e| e.str("link")).unwrap_or_default().to_string();
                let id = self.sel[0].clone();
                if self.link_buf.as_ref().is_none_or(|(i, base, _)| *i != id || *base != cur) {
                    self.link_buf = Some((id, cur.clone(), cur.clone()));
                }
                let Some((_, _, buf)) = self.link_buf.as_mut() else { return };
                let r = ui.add(egui::TextEdit::singleline(buf).hint_text("[[Note]]").desired_width(f32::INFINITY));
                if r.lost_focus() {
                    let v = buf.trim().to_string();
                    self.link_buf = None;
                    if v != cur {
                        self.on_sel(|d, s, cx| d.set_field(s, "link", &if v.is_empty() { Json::Null } else { Json::str(&v) }, cx));
                    }
                }
                if !cur.is_empty() && ui.link(format!("Open {cur}")).clicked() {
                    Self::open_link(ws, ui.ctx(), &cur, out);
                }
            }
        });
    }

    fn toggle_lock(&mut self) {
        let Some(d) = &self.d else { return };
        let locked = self.sel.iter().filter_map(|id| d.get(id)).all(|e| e.bool("locked") == Some(true));
        let sel = self.sel.clone();
        if sel.is_empty() {
            return;
        }
        self.checkpoint();
        if let Some(d) = &mut self.d {
            d.lock(&sel, !locked, &mut self.cx);
        }
        self.touch();
    }

    fn picker_ui(&mut self, ctx: &egui::Context, ws: &Workspace, pal: &Palette, rect: Rect) {
        let Some(q) = self.picker.as_mut() else { return };
        let mut chosen = None;
        let mut close = false;
        egui::Area::new(Id::unique(("quire-drawing-picker", self.path.as_str())))
            .fixed_pos(rect.min + Vec2::new(12.0, 8.0))
            .order(egui::Order::Foreground)
            .show(ctx, |ui| {
                egui::Frame::popup(ui.style()).fill(pal.raised).show(ui, |ui| {
                    ui.set_width(320.0);
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("Add a picture from the vault").strong());
                        if ui.small_button("×").clicked() {
                            close = true;
                        }
                    });
                    let r = ui.add(egui::TextEdit::singleline(q).hint_text("Filter…").desired_width(f32::INFINITY));
                    if !r.has_focus() && q.is_empty() {
                        r.request_focus();
                    }
                    let ql = q.to_lowercase();
                    let files: Vec<NotePath> = ws
                        .other_files()
                        .into_iter()
                        .filter(|p| drawings::picture_type(p.as_str()).is_some() && p.as_str().to_lowercase().contains(&ql))
                        .take(200)
                        .collect();
                    egui::ScrollArea::vertical().max_height(260.0).show(ui, |ui| {
                        if files.is_empty() {
                            ui.label(RichText::new("No pictures found.").color(pal.muted));
                        }
                        for f in files {
                            if ui.selectable_label(false, f.as_str()).clicked() {
                                chosen = Some(f);
                            }
                        }
                    });
                });
            });
        if ctx.input(|i| i.key_pressed(Key::Escape)) {
            close = true;
        }
        if let Some(f) = chosen {
            self.picker = None;
            self.add_picture(ws, &f, rect);
        } else if close {
            self.picker = None;
            self.tool = Tool::Select;
        }
    }

    fn typing_ui(&mut self, ui: &mut egui::Ui, view: View, pal: &Palette) {
        let Some(t) = &mut self.typing else { return };
        let Some(e) = self.d.as_ref().and_then(|d| d.get(&t.id)) else {
            self.typing = None;
            return;
        };
        let (x, y, w, h) = drawing::bbox(e);
        let size = e.num("fontSize").unwrap_or(20.0) as f32 * view.zoom;
        let fam = paint::family(e.num("fontFamily").unwrap_or(5.0) as i64);
        // In a shape: the shape's box; else the text's own.
        let container = e.str("containerId").and_then(|c| self.d.as_ref()?.get(c)).map(|c| drawing::bbox(c));
        let (bx, by, bw, bh) = container.unwrap_or_else(|| (x, y, w.max(40.0), h.max(f64::from(size / view.zoom) * 1.25)));
        let r = Rect::from_min_max(view.to_screen((bx, by)), view.to_screen((bx + bw, by + bh))).expand(4.0);
        let r = Rect::from_min_size(r.min, Vec2::new(r.width().max(160.0), r.height().max(size * 1.4)));
        ui.painter().rect_filled(r, 4.0, pal.raised.gamma_multiply(0.92));
        ui.painter().rect_stroke(r, 4.0, Stroke::new(1.0, pal.accent), StrokeKind::Outside);
        let id = Id::unique(("quire-drawing-text", t.id.as_str()));
        let te = egui::TextEdit::multiline(&mut t.buf)
            .id(id)
            .font(FontId::new(size.clamp(8.0, 96.0), fam))
            .frame(egui::Frame::NONE)
            .desired_width(r.width())
            .text_color(pal.text);
        let resp = ui.put(r, te);
        if !t.focused {
            resp.request_focus();
            t.focused = true;
        }
        let done = ui.input(|i| i.key_pressed(Key::Escape) || (i.modifiers.command && i.key_pressed(Key::Enter)))
            || (t.focused && resp.lost_focus());
        if done {
            self.finish_typing();
        }
    }

    #[expect(clippy::cognitive_complexity, reason = "the pointer's one state machine")]
    fn canvas(&mut self, ui: &mut egui::Ui, ws: &mut Workspace, pal: &Palette, rect: Rect, out: &mut PanelOut) {
        let resp = ui.interact(rect, Id::unique(("quire-drawing-canvas", self.path.as_str())), Sense::click_and_drag());
        if !self.fitted {
            self.fit(rect);
        }
        let ctx = ui.ctx().clone();
        let painter = ui.painter_at(rect);
        let bg = self.d.as_ref().and_then(Drawing::background).map_or(Color32::WHITE, |c| {
            let [r, g, b, _] = c.0;
            Color32::from_rgb(r, g, b)
        });
        painter.rect_filled(rect, 0.0, bg);
        let view = self.view(rect);
        if self.grid && f64::from(self.zoom) * GRID >= 6.0 {
            let vis = view.visible();
            let line = Color32::from_black_alpha(18);
            let step = GRID as f32;
            let (gx0, gx1) = ((vis.min.x / step).floor() as i64, (vis.max.x / step).ceil() as i64);
            for gx in gx0..=gx1 {
                let sx = view.to_screen((gx as f64 * GRID, 0.0)).x;
                painter.line_segment([Pos2::new(sx, rect.top()), Pos2::new(sx, rect.bottom())], Stroke::new(1.0, line));
            }
            let (gy0, gy1) = ((vis.min.y / step).floor() as i64, (vis.max.y / step).ceil() as i64);
            for gy in gy0..=gy1 {
                let sy = view.to_screen((0.0, gy as f64 * GRID)).y;
                painter.line_segment([Pos2::new(rect.left(), sy), Pos2::new(rect.right(), sy)], Stroke::new(1.0, line));
            }
        }

        // ---- the scene
        let rest = self.zoomed_at.is_none_or(|t| t.elapsed() >= ZOOM_REST);
        if rest {
            self.zoomed_at = None;
        } else {
            ctx.request_repaint_after(ZOOM_REST);
        }
        let faded: Vec<String> = match &self.drag {
            Some(Drag::Erase { hits }) => hits.clone(),
            _ => Vec::new(),
        };
        let skip = self.typing.as_ref().map(|t| t.id.clone());
        if let Some(d) = &self.d {
            let (images, path) = (&mut self.images, &self.path);
            let mut tex = |id: &str| Self::texture(images, ws, path, d, &ctx, id);
            self.drawn = paint::draw(&painter, &d.state.elements, &mut self.cache, view, rest, &mut tex, &faded, skip.as_deref());
            let ids: HashSet<&str> = d.state.elements.iter().filter_map(|e| e.str("id")).collect();
            self.cache.retain(&ids);
        } else {
            let msg = self.error.clone().unwrap_or_else(|| "Reading the drawing…".into());
            painter.text(rect.center(), Align2::CENTER_CENTER, msg, FontId::proportional(14.0), pal.danger);
            return;
        }

        // ---- selection and links
        let accent = Color32::from_rgb(105, 101, 219);
        let mut badges: Vec<(Rect, String)> = Vec::new();
        if let Some(d) = &self.d {
            for e in drawing::live(&d.state.elements) {
                let Some(l) = e.str("link").filter(|l| !l.trim().is_empty()) else { continue };
                if e.str("type") == Some("text") && e.str("containerId").is_some_and(|c| !c.is_empty()) {
                    continue;
                }
                let (x, y, w, _) = drawing::aabb(e);
                let p = view.to_screen((x + w, y));
                let r = Rect::from_center_size(p + Vec2::new(10.0, -10.0), Vec2::splat(18.0));
                if r.intersects(rect) {
                    painter.rect_filled(r, 4.0, pal.raised);
                    painter.rect_stroke(r, 4.0, Stroke::new(1.0, accent), StrokeKind::Inside);
                    painter.text(r.center(), Align2::CENTER_CENTER, "↗", FontId::proportional(12.0), accent);
                    badges.push((r, l.to_string()));
                }
            }
            for id in &self.sel {
                let Some(e) = d.get(id) else { continue };
                let (x, y, w, h) = drawing::bbox(e);
                let a = e.num("angle").unwrap_or(0.0);
                let c = (x + w / 2.0, y + h / 2.0);
                let pad = f64::from(4.0 / self.zoom);
                let corners = [(x - pad, y - pad), (x + w + pad, y - pad), (x + w + pad, y + h + pad), (x - pad, y + h + pad)].map(|p| {
                    let (s, co) = a.sin_cos();
                    let (dx, dy) = (p.0 - c.0, p.1 - c.1);
                    view.to_screen((c.0 + dx * co - dy * s, c.1 + dx * s + dy * co))
                });
                let col = if e.bool("locked") == Some(true) { pal.muted } else { accent };
                for i in 0..4 {
                    painter.extend(egui::Shape::dashed_line(&[corners[i], corners[(i + 1) % 4]], Stroke::new(1.0, col), 4.0, 3.0));
                }
            }
        }
        if let Some((hs, rot)) = self.handles(view) {
            for h in &hs {
                let r = Rect::from_center_size(*h, Vec2::splat(8.0));
                painter.rect_filled(r, 1.0, Color32::WHITE);
                painter.rect_stroke(r, 1.0, Stroke::new(1.0, accent), StrokeKind::Inside);
            }
            painter.circle(rot, 4.5, Color32::WHITE, Stroke::new(1.0, accent));
        }
        if let Some((_, ends)) = self.end_points(view) {
            for p in ends {
                painter.circle(p, 5.0, Color32::WHITE, Stroke::new(1.5, accent));
            }
        }
        if let Some(Drag::Marquee { start, now, .. }) = &self.drag {
            let r = Rect::from_two_pos(view.to_screen(*start), view.to_screen(*now));
            painter.rect_filled(r, 0.0, accent.gamma_multiply(0.08));
            painter.rect_stroke(r, 0.0, Stroke::new(1.0, accent), StrokeKind::Inside);
        }

        // ---- input
        let pointer = resp.hover_pos().or_else(|| resp.interact_pointer_pos());
        let (scroll, zoom_delta, mods, space) = ui.input(|i| (i.smooth_scroll_delta, i.zoom_delta(), i.modifiers, i.key_down(Key::Space)));
        if resp.hovered() && self.typing.is_none() {
            if (zoom_delta - 1.0).abs() > 1e-4 {
                if let Some(p) = pointer {
                    self.zoom_at(rect, p, self.zoom * zoom_delta);
                }
            } else if scroll != Vec2::ZERO {
                self.offset -= scroll / self.zoom;
            }
        }
        let scene_at = pointer.map(|p| self.view(rect).to_scene(p));
        let middle = ui.input(|i| i.pointer.middle_down());
        if resp.drag_started() {
            self.finish_typing();
            resp.request_focus();
            // Where the button went down, not where the drag was recognised.
            let sp = ui.input(|i| i.pointer.press_origin()).or(pointer).unwrap_or_default();
            let p = self.view(rect).to_scene(sp);
            if self.tool == Tool::Hand || space || middle {
                self.drag = Some(Drag::Pan);
            } else if self.tool == Tool::Eraser {
                self.drag = Some(Drag::Erase { hits: self.hit(p).into_iter().collect() });
            } else if self.tool == Tool::Select {
                self.start_select_drag(view, sp, p, mods.shift, mods.alt);
            } else if let Some(kind) = self.tool.kind() {
                self.start_create(kind, p);
            }
        }
        if let (Some(drag), Some(p), Some(sp)) = (self.drag.clone(), scene_at, pointer)
            && (resp.dragged() || middle)
        {
            self.continue_drag(drag, p, sp, mods.shift, rect);
        }
        if resp.drag_stopped() || (self.drag.is_some() && !ui.input(|i| i.pointer.any_down())) {
            self.end_drag(scene_at.unwrap_or_default());
        }
        if resp.clicked() {
            resp.request_focus();
            self.finish_typing();
            let sp = pointer.unwrap_or_default();
            let p = scene_at.unwrap_or_default();
            if let Some((_, link)) = badges.iter().find(|(r, _)| r.contains(sp)) {
                Self::open_link(ws, &ctx, link, out);
            } else if mods.command
                && let Some(link) = self.hit(p).and_then(|id| self.d.as_ref()?.get(&id)?.str("link").map(str::to_string))
            {
                Self::open_link(ws, &ctx, &link, out);
            } else {
                self.click(p, mods.shift);
            }
        }
        if resp.double_clicked() && self.typing.is_none() && self.tool == Tool::Select {
            let p = scene_at.unwrap_or_default();
            self.double_click(p);
        }

        // ---- cursor
        if let Some(sp) = pointer.filter(|_| resp.hovered()) {
            let icon = match (&self.drag, self.tool) {
                (Some(Drag::Pan), _) => CursorIcon::Grabbing,
                (_, Tool::Hand) => CursorIcon::Grab,
                (_, Tool::Select) => {
                    if badges.iter().any(|(r, _)| r.contains(sp)) {
                        CursorIcon::PointingHand
                    } else if let Some((hs, rot)) = self.handles(view)
                        && let Some(i) = hs.iter().position(|h| h.distance(sp) < 7.0).or_else(|| (rot.distance(sp) < 7.0).then_some(8))
                    {
                        if i == 8 {
                            CursorIcon::Alias
                        } else {
                            [CursorIcon::ResizeNwSe, CursorIcon::ResizeVertical, CursorIcon::ResizeNeSw, CursorIcon::ResizeHorizontal]
                                [i % 4]
                        }
                    } else if scene_at.and_then(|p| self.hit(p)).is_some() {
                        CursorIcon::Move
                    } else {
                        CursorIcon::Default
                    }
                }
                (_, Tool::Text) => CursorIcon::Text,
                _ => CursorIcon::Crosshair,
            };
            ui.ctx().set_cursor_icon(icon);
        }

        // ---- keys
        let focused = resp.has_focus() || (ui.memory(|m| m.focused().is_none()) && resp.hovered());
        if focused && self.typing.is_none() && self.picker.is_none() {
            if ui.memory(|m| m.focused().is_none()) {
                resp.request_focus();
            }
            self.keys(ui, rect, scene_at);
        }
        self.typing_ui(ui, self.view(rect), pal);
    }

    fn start_select_drag(&mut self, view: View, sp: Pos2, p: (f64, f64), shift: bool, alt: bool) {
        // A handle of the one selected element.
        if let Some((hs, rot)) = self.handles(view) {
            let id = self.sel[0].clone();
            let e = self.d.as_ref().and_then(|d| d.get(&id)).cloned();
            if let Some(e) = e {
                if rot.distance(sp) < 8.0 {
                    let c = drawing::center(&e);
                    self.checkpoint();
                    self.drag = Some(Drag::Rotate { ids: vec![id], center: c, last: (p.1 - c.1).atan2(p.0 - c.0) });
                    return;
                }
                if let Some(h) = hs.iter().position(|h| h.distance(sp) < 8.0) {
                    let b = drawing::bbox(&e);
                    self.checkpoint();
                    self.drag =
                        Some(Drag::Resize { id, handle: h, start: b, angle: e.num("angle").unwrap_or(0.0), aspect: b.2 / b.3.max(1e-6) });
                    return;
                }
            }
        }
        if let Some((id, ends)) = self.end_points(view)
            && let Some(i) = ends.iter().position(|e| e.distance(sp) < 8.0)
        {
            self.checkpoint();
            self.drag = Some(Drag::Point { id, index: i });
            return;
        }
        match self.hit(p) {
            Some(id) => {
                let group = self.select_target(&id);
                if !self.sel.contains(&id) {
                    if shift {
                        self.sel.extend(group);
                    } else {
                        self.sel = group;
                    }
                }
                self.checkpoint();
                // Alt+drag: drag a copy (as Excalidraw).
                if alt {
                    let sel = self.unlocked_sel();
                    if let Some(d) = &mut self.d {
                        self.sel = d.duplicate(&sel, 0.0, 0.0, &mut self.cx);
                    }
                }
                self.drag = Some(Drag::Move { last: p, moved: false, dup: alt });
            }
            None => self.drag = Some(Drag::Marquee { start: p, now: p, additive: shift }),
        }
    }

    fn start_create(&mut self, kind: &str, p: (f64, f64)) {
        self.checkpoint();
        let p = if kind == "freedraw" { p } else { self.snapped(p) };
        let Some(d) = &mut self.d else { return };
        let e = d.new_element(kind, p.0, p.1, &self.style, &mut self.cx);
        let id = d.add(e, &mut self.cx);
        self.sel = vec![id.clone()];
        self.drag = Some(Drag::Create { id, start: p });
    }

    fn continue_drag(&mut self, drag: Drag, p: (f64, f64), sp: Pos2, shift: bool, rect: Rect) {
        match drag {
            Drag::Pan => {
                let _ = (sp, rect);
                self.offset -= PAN_DELTA.with(std::cell::Cell::get) / self.zoom;
            }
            Drag::Move { last, moved, dup } => {
                let (mut dx, mut dy) = (p.0 - last.0, p.1 - last.1);
                if self.snap {
                    // Keep the selection's corner on the grid.
                    let corner = self
                        .d
                        .as_ref()
                        .and_then(|d| drawing::union_box(self.sel.iter().filter_map(|id| d.get(id))))
                        .map(|b| (b.0, b.1))
                        .unwrap_or_default();
                    let target = self.snapped((corner.0 + dx, corner.1 + dy));
                    (dx, dy) = (target.0 - corner.0, target.1 - corner.1);
                }
                if dx == 0.0 && dy == 0.0 {
                    return;
                }
                let sel = self.unlocked_sel();
                if let Some(d) = &mut self.d {
                    d.translate(&sel, dx, dy, &mut self.cx);
                }
                self.drag = Some(Drag::Move { last: (last.0 + dx, last.1 + dy), moved: moved || dx != 0.0 || dy != 0.0, dup });
                self.touch();
            }
            Drag::Resize { id, handle, start, angle, aspect } => {
                let (x, y, w, h) = start;
                let c0 = (x + w / 2.0, y + h / 2.0);
                let q = rotate_pt(p, c0, -angle);
                let q = self.snapped(q);
                let (mut x0, mut y0, mut x1, mut y1) = (x, y, x + w, y + h);
                match handle {
                    0 => (x0, y0) = q,
                    1 => y0 = q.1,
                    2 => (x1, y0) = q,
                    3 => x1 = q.0,
                    4 => (x1, y1) = q,
                    5 => y1 = q.1,
                    6 => (x0, y1) = q,
                    _ => x0 = q.0,
                }
                let is_text = self.d.as_ref().and_then(|d| d.get(&id)).is_some_and(|e| e.str("type") == Some("text"));
                if (shift || is_text) && handle % 2 == 0 {
                    // Keep the shape's proportions (text always).
                    let (nw, nh) = ((x1 - x0).abs(), (y1 - y0).abs());
                    let (nw, nh) = if nw / aspect > nh { (nw, nw / aspect) } else { (nh * aspect, nh) };
                    match handle {
                        0 => (x0, y0) = (x1 - nw, y1 - nh),
                        2 => (x1, y0) = (x0 + nw, y1 - nh),
                        4 => (x1, y1) = (x0 + nw, y0 + nh),
                        _ => (x0, y1) = (x1 - nw, y0 + nh),
                    }
                }
                let (nx0, nx1) = (x0.min(x1), x0.max(x1));
                let (ny0, ny1) = (y0.min(y1), y0.max(y1));
                let (nw, nh) = ((nx1 - nx0).max(1.0), (ny1 - ny0).max(1.0));
                // The new centre, back in scene units (the far side stays put).
                let cl = rotate_pt((nx0 + nw / 2.0, ny0 + nh / 2.0), c0, angle);
                if let Some(d) = &mut self.d {
                    d.resize(&id, (cl.0 - nw / 2.0, cl.1 - nh / 2.0, nw, nh), &mut self.cx);
                }
                self.touch();
            }
            Drag::Rotate { ids, center, last } => {
                let a = (p.1 - center.1).atan2(p.0 - center.0);
                let mut delta = a - last;
                if shift {
                    let step = std::f64::consts::PI / 12.0;
                    let cur = self.d.as_ref().and_then(|d| d.get(&ids[0])).and_then(|e| e.num("angle")).unwrap_or(0.0);
                    let target = ((cur + delta) / step).round() * step;
                    delta = target - cur;
                    if delta.abs() < 1e-9 {
                        return;
                    }
                }
                if let Some(d) = &mut self.d {
                    d.rotate(&ids, delta, center, &mut self.cx);
                }
                self.drag = Some(Drag::Rotate { ids, center, last: last + delta });
                self.touch();
            }
            Drag::Marquee { start, additive, .. } => self.drag = Some(Drag::Marquee { start, now: p, additive }),
            Drag::Create { id, start } => {
                let snapped = self.snapped(p);
                let zoom = f64::from(self.zoom);
                let Some(d) = &mut self.d else { return };
                let kind = d.get(&id).and_then(|e| e.str("type")).unwrap_or_default().to_string();
                match kind.as_str() {
                    "freedraw" => {
                        let Some(e) = d.get(&id) else { return };
                        let mut pts = drawing::points(e);
                        let (ox, oy) = (e.num("x").unwrap_or(0.0), e.num("y").unwrap_or(0.0));
                        let rel = (p.0 - ox, p.1 - oy);
                        if pts.last().is_none_or(|l| (l.0 - rel.0).hypot(l.1 - rel.1) * zoom >= 1.5) {
                            pts.push(rel);
                            d.edit(&id, &mut self.cx, |o| drawing::set_points(o, &pts));
                        }
                    }
                    "arrow" | "line" => {
                        let mut q = snapped;
                        if shift {
                            // 15° steps.
                            let (dx, dy) = (q.0 - start.0, q.1 - start.1);
                            let step = std::f64::consts::PI / 12.0;
                            let a = (dy.atan2(dx) / step).round() * step;
                            let len = dx.hypot(dy);
                            q = (start.0 + len * a.cos(), start.1 + len * a.sin());
                        }
                        let rel = [(0.0, 0.0), (q.0 - start.0, q.1 - start.1)];
                        d.edit(&id, &mut self.cx, |o| drawing::set_points(o, &rel));
                    }
                    _ => {
                        let q = snapped;
                        let (mut w, mut h) = (q.0 - start.0, q.1 - start.1);
                        if shift {
                            let m = w.abs().max(h.abs());
                            (w, h) = (m * w.signum(), m * h.signum());
                        }
                        let (x, y) = (start.0.min(start.0 + w), start.1.min(start.1 + h));
                        d.edit(&id, &mut self.cx, |o| {
                            o.set_num("x", x);
                            o.set_num("y", y);
                            o.set_num("width", w.abs());
                            o.set_num("height", h.abs());
                        });
                    }
                }
                self.touch();
            }
            Drag::Point { id, index } => {
                let q = self.snapped(p);
                let Some(d) = &mut self.d else { return };
                let Some(e) = d.get(&id) else { return };
                let (ox, oy) = (e.num("x").unwrap_or(0.0), e.num("y").unwrap_or(0.0));
                let mut abs: Vec<(f64, f64)> = drawing::points(e).into_iter().map(|(a, b)| (ox + a, oy + b)).collect();
                if index < abs.len() {
                    abs[index] = q;
                }
                let (nx, ny) = abs[0];
                let rel: Vec<(f64, f64)> = abs.iter().map(|(a, b)| (a - nx, b - ny)).collect();
                d.edit(&id, &mut self.cx, |o| {
                    o.set_num("x", nx);
                    o.set_num("y", ny);
                    drawing::set_points(o, &rel);
                });
                self.touch();
            }
            Drag::Erase { mut hits } => {
                if let Some(id) = self.hit(p)
                    && !hits.contains(&id)
                {
                    hits.push(id);
                }
                self.drag = Some(Drag::Erase { hits });
            }
        }
    }

    fn end_drag(&mut self, p: (f64, f64)) {
        let Some(drag) = self.drag.take() else { return };
        match drag {
            Drag::Move { moved: false, dup: false, .. } => {
                // No move: no undo step.
                if let Some(d) = &mut self.d {
                    self.hist.cancel(&mut d.state);
                }
            }
            Drag::Marquee { start, now, additive } => {
                let (x0, y0, x1, y1) = (start.0.min(now.0), start.1.min(now.1), start.0.max(now.0), start.1.max(now.1));
                let Some(d) = &self.d else { return };
                let inside: Vec<String> = drawing::live(&d.state.elements)
                    .filter(|e| e.get("containerId").is_none_or(Json::is_null))
                    .filter(|e| {
                        let (x, y, w, h) = drawing::aabb(e);
                        x >= x0 && y >= y0 && x + w <= x1 && y + h <= y1
                    })
                    .filter_map(|e| e.str("id").map(str::to_string))
                    .collect();
                let mut sel: Vec<String> = if additive { self.sel.clone() } else { Vec::new() };
                for id in inside {
                    for g in d.group_of(&id) {
                        if !sel.contains(&g) {
                            sel.push(g);
                        }
                    }
                }
                self.sel = sel;
            }
            Drag::Create { id, .. } => {
                let Some(d) = &mut self.d else { return };
                let Some(e) = d.get(&id) else { return };
                let (_, _, w, h) = drawing::bbox(e);
                let kind = e.str("type").unwrap_or_default().to_string();
                if w.hypot(h) * f64::from(self.zoom) < 3.0 {
                    // A click, not a drag: nothing made.
                    self.hist.cancel(&mut d.state);
                    self.sel.clear();
                    return;
                }
                if kind == "arrow" {
                    let pts = drawing::points(e);
                    let (ox, oy) = (e.num("x").unwrap_or(0.0), e.num("y").unwrap_or(0.0));
                    let ends = [(ox + pts[0].0, oy + pts[0].1), p];
                    for (i, at) in ends.into_iter().enumerate() {
                        if let Some(t) = self.bind_target(at, &id)
                            && let Some(d) = &mut self.d
                        {
                            d.bind(&id, i == 0, &t, &mut self.cx);
                        }
                    }
                }
                self.tool = Tool::Select;
                self.touch();
            }
            Drag::Point { id, index } => {
                let is_arrow = self.d.as_ref().and_then(|d| d.get(&id)).is_some_and(|e| e.str("type") == Some("arrow"));
                if is_arrow && let Some(t) = self.bind_target(p, &id) {
                    if let Some(d) = &mut self.d {
                        d.bind(&id, index == 0, &t, &mut self.cx);
                    }
                } else if is_arrow && let Some(d) = &mut self.d {
                    let key = if index == 0 { "startBinding" } else { "endBinding" };
                    let bound = d.get(&id).and_then(|e| e.obj(key)).is_some();
                    if bound {
                        d.edit(&id, &mut self.cx, |o| o.set(key, Json::Null));
                    }
                }
                self.touch();
            }
            Drag::Erase { hits } => {
                if !hits.is_empty() {
                    self.checkpoint();
                    if let Some(d) = &mut self.d {
                        d.erase(&hits, &mut self.cx);
                    }
                    self.sel.retain(|s| !hits.contains(s));
                    self.touch();
                }
            }
            Drag::Pan | Drag::Move { .. } | Drag::Resize { .. } | Drag::Rotate { .. } => {}
        }
    }

    fn click(&mut self, p: (f64, f64), shift: bool) {
        match self.tool {
            Tool::Select => match self.hit(p) {
                Some(id) => {
                    let group = self.select_target(&id);
                    if shift {
                        if self.sel.contains(&id) {
                            self.sel.retain(|s| !group.contains(s));
                        } else {
                            self.sel.extend(group);
                        }
                    } else {
                        self.sel = group;
                    }
                }
                None => self.sel.clear(),
            },
            Tool::Text => {
                self.new_text(p);
                self.tool = Tool::Select;
            }
            Tool::Eraser => {
                if let Some(id) = self.hit(p) {
                    self.checkpoint();
                    if let Some(d) = &mut self.d {
                        d.erase(std::slice::from_ref(&id), &mut self.cx);
                    }
                    self.sel.retain(|s| *s != id);
                    self.touch();
                }
            }
            _ => {}
        }
    }

    fn double_click(&mut self, p: (f64, f64)) {
        if self.typing.is_some() {
            return;
        }
        match self.hit(p) {
            Some(id) => self.edit_text_of(&id),
            None => self.new_text(p),
        }
    }

    fn new_text(&mut self, p: (f64, f64)) {
        if self.d.is_none() {
            return;
        }
        self.checkpoint();
        let Some(d) = &mut self.d else { return };
        let size = self.style.font_size;
        let e = d.new_element("text", p.0, p.1 - size * 0.6, &self.style, &mut self.cx);
        let id = d.add(e, &mut self.cx);
        self.sel = vec![id.clone()];
        self.typing = Some(Typing { id, buf: String::new(), fresh: true, focused: false });
    }

    fn keys(&mut self, ui: &egui::Ui, rect: Rect, at: Option<(f64, f64)>) {
        let events = ui.input(|i| i.events.clone());
        let at = at.unwrap_or_else(|| self.view(rect).to_scene(rect.center()));
        for e in events {
            match e {
                egui::Event::Copy | egui::Event::Cut => {
                    if let Some(t) = self.copy() {
                        ui.ctx().copy_text(t);
                        if matches!(e, egui::Event::Cut) {
                            self.delete_sel();
                        }
                    }
                }
                egui::Event::Paste(t) => self.paste(&t, at),
                egui::Event::Key { key, pressed: true, modifiers: m, .. } => {
                    let cmd = m.command;
                    match key {
                        Key::Delete | Key::Backspace => self.delete_sel(),
                        Key::Z if cmd => self.undo(m.shift),
                        Key::Y if cmd => self.undo(true),
                        Key::A if cmd => self.select_all(),
                        Key::D if cmd => self.duplicate(),
                        Key::L if cmd && m.shift => self.toggle_lock(),
                        Key::CloseBracket if cmd => self.on_sel(|d, s, cx| d.reorder(s, Z::Forward, cx)),
                        Key::OpenBracket if cmd => self.on_sel(|d, s, cx| d.reorder(s, Z::Backward, cx)),
                        Key::ArrowLeft | Key::ArrowRight | Key::ArrowUp | Key::ArrowDown if cmd && m.shift => {
                            let a = match key {
                                Key::ArrowLeft => Align::Left,
                                Key::ArrowRight => Align::Right,
                                Key::ArrowUp => Align::Top,
                                _ => Align::Bottom,
                            };
                            if self.sel.len() > 1 {
                                self.on_sel(|d, s, cx| d.align(s, a, cx));
                            }
                        }
                        Key::ArrowLeft | Key::ArrowRight | Key::ArrowUp | Key::ArrowDown if !cmd => {
                            let step = if self.snap {
                                GRID
                            } else if m.shift {
                                5.0
                            } else {
                                1.0
                            };
                            let (dx, dy) = match key {
                                Key::ArrowLeft => (-step, 0.0),
                                Key::ArrowRight => (step, 0.0),
                                Key::ArrowUp => (0.0, -step),
                                _ => (0.0, step),
                            };
                            self.on_sel(|d, s, cx| d.translate(s, dx, dy, cx));
                        }
                        Key::Num0 if cmd => {
                            self.zoom_at(rect, rect.center(), 1.0);
                        }
                        Key::Num1 if m.shift && !cmd => self.fitted = false,
                        Key::Quote if cmd => self.grid = !self.grid,
                        Key::Escape => {
                            self.sel.clear();
                            self.tool = Tool::Select;
                        }
                        Key::Enter if self.sel.len() == 1 && !cmd => {
                            let id = self.sel[0].clone();
                            self.edit_text_of(&id);
                        }
                        k if !cmd && !m.alt && !m.shift => {
                            if let Some(t) = Tool::for_key(k) {
                                self.tool = t;
                                if t == Tool::Image {
                                    self.picker = Some(String::new());
                                }
                            }
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
        }
    }
}

thread_local! {
    /// The pointer's movement this frame (panning), set at the frame's start.
    static PAN_DELTA: std::cell::Cell<Vec2> = const { std::cell::Cell::new(Vec2::ZERO) };
}

fn rotate_pt(p: (f64, f64), c: (f64, f64), a: f64) -> (f64, f64) {
    if a == 0.0 {
        return p;
    }
    let (s, co) = a.sin_cos();
    let (dx, dy) = (p.0 - c.0, p.1 - c.1);
    (c.0 + dx * co - dy * s, c.1 + dx * s + dy * co)
}

/// A colour swatch; `transparent` shows as a crossed box.
fn swatch(ui: &mut egui::Ui, c: &str, on: bool, pal: &Palette) -> egui::Response {
    let (r, resp) = ui.allocate_exact_size(Vec2::splat(22.0), Sense::click());
    let col = excalidraw::Rgba::parse(c).map_or(Color32::TRANSPARENT, |c| {
        let [r, g, b, a] = c.0;
        Color32::from_rgba_unmultiplied(r, g, b, a)
    });
    let p = ui.painter();
    p.rect_filled(r.shrink(2.0), 4.0, col);
    if col == Color32::TRANSPARENT {
        p.line_segment([r.shrink(5.0).left_bottom(), r.shrink(5.0).right_top()], Stroke::new(1.0, pal.danger));
    }
    p.rect_stroke(
        r.shrink(2.0),
        4.0,
        Stroke::new(if on { 2.0 } else { 1.0 }, if on { pal.accent } else { pal.border }),
        StrokeKind::Outside,
    );
    resp.on_hover_text(c)
}

/// A text's box at size `size`: the widest line, and lines × line height.
fn measure(ctx: &egui::Context, raw: &str, size: f64, family: i64, line_height: f64) -> (f64, f64) {
    let shown = drawing::display_text(raw);
    let font = FontId::new(size as f32, paint::family(family));
    let w = shown
        .lines()
        .map(|l| f64::from(ctx.fonts_mut(|f| f.layout_no_wrap(l.to_string(), font.clone(), Color32::WHITE).size().x)))
        .fold(0.0_f64, f64::max);
    let n = shown.lines().count().max(1) as f64;
    (w.ceil(), (n * size * line_height).ceil())
}

/// [`measure`] without fonts (before the first frame): about 0.6 em a character.
fn approx_measure(raw: &str, size: f64, line_height: f64) -> (f64, f64) {
    let shown = drawing::display_text(raw);
    let w = shown.lines().map(|l| l.chars().count()).max().unwrap_or(0) as f64 * size * 0.6;
    (w.ceil(), (shown.lines().count().max(1) as f64 * size * line_height).ceil())
}

/// A picture's bytes: a `data:` URL, or a vault file next to the drawing
/// or above it (the path checks every embed passes); in a locked folder,
/// decrypted in memory.
fn picture_bytes(ws: &Workspace, drawing: &NotePath, f: &excalidraw::FileRef) -> Option<Vec<u8>> {
    use base64::Engine as _;
    match f {
        excalidraw::FileRef::DataUrl(u) => base64::engine::general_purpose::STANDARD.decode(u.split_once(";base64,")?.1.trim()).ok(),
        excalidraw::FileRef::Vault(rel) => {
            let vault = ws.vault()?;
            if let Some(p) = drawings::embedded_file(vault, drawing, rel, &|_| true) {
                return vault.storage().read_bytes(&p).ok();
            }
            drawings::picture_type(rel)?;
            match ws.attachment_source(rel)? {
                quire_app::attach::AttachmentSource::Locked(p) => ws.read_locked_attachment(&p),
                quire_app::attach::AttachmentSource::File(_) => None,
            }
        }
    }
}

impl Surface for DrawingEditor {
    fn path(&self) -> &NotePath {
        &self.path
    }

    fn title(&self) -> String {
        self.title_text()
    }

    fn done(&self) -> bool {
        self.done
    }

    fn show(&mut self, ui: &mut egui::Ui, ws: &mut Workspace, pal: &Palette) -> PanelOut {
        let started = Instant::now();
        let mut out = PanelOut::default();
        if self.ctx.is_none() {
            self.ctx = Some(ui.ctx().clone());
        }
        if !self.loaded {
            self.load(ws);
        }
        self.absorb(ws);
        PAN_DELTA.with(|c| c.set(ui.input(|i| i.pointer.delta())));
        self.toolbar(ui, ws, pal, &mut out);
        self.conflict_bar(ui, ws, pal);
        let avail = ui.available_rect_before_wrap();
        let (canvas_rect, panel_rect) = if self.panel && avail.width() > PANEL_W * 2.0 {
            let split = avail.right() - PANEL_W;
            (
                Rect::from_min_max(avail.min, Pos2::new(split - 1.0, avail.bottom())),
                Some(Rect::from_min_max(Pos2::new(split, avail.top()), avail.max)),
            )
        } else {
            (avail, None)
        };
        ui.allocate_rect(avail, Sense::hover());
        self.last_rect = canvas_rect;
        let mut canvas_ui = ui.new_child(UiBuilder::new().max_rect(canvas_rect));
        canvas_ui.set_clip_rect(canvas_rect.intersect(ui.clip_rect()));
        self.canvas(&mut canvas_ui, ws, pal, canvas_rect, &mut out);
        if let Some(pr) = panel_rect {
            ui.painter().rect_filled(pr, 0.0, pal.panel);
            ui.painter().line_segment([pr.left_top(), pr.left_bottom()], Stroke::new(1.0, pal.border));
            let mut pui = ui.new_child(UiBuilder::new().max_rect(pr.shrink2(Vec2::new(10.0, 8.0))));
            self.panel_ui(&mut pui, ws, pal, &mut out);
        }
        self.picker_ui(&ui.ctx().clone(), ws, pal, canvas_rect);
        // The status line.
        let info = format!(
            "{} elements · {} drawn{} · {:.1} ms",
            self.d.as_ref().map_or(0, |d| drawing::live(&d.state.elements).count()),
            self.drawn.elements,
            if self.drawn.tessellated > 0 { format!(" ({} new)", self.drawn.tessellated) } else { String::new() },
            self.last_frame_ms
        );
        ui.painter().text(
            canvas_rect.left_bottom() + Vec2::new(8.0, -6.0),
            Align2::LEFT_BOTTOM,
            info,
            FontId::proportional(10.0),
            Color32::from_gray(140),
        );
        // Autosave.
        if self.dirty && self.drag.is_none() && self.typing.is_none() {
            let wait = self.save_failed.map_or(SAVE_AFTER, |_| RETRY_AFTER);
            let since = self.save_failed.map_or(self.last_edit, |t| t.max(self.last_edit));
            if since.elapsed() >= wait {
                self.save(ws);
            } else {
                ui.ctx().request_repaint_after(wait.saturating_sub(since.elapsed()));
            }
        }
        self.last_frame_ms = started.elapsed().as_secs_f32() * 1000.0;
        out
    }
}

impl Drop for DrawingEditor {
    /// Closing (or quitting) with unsaved edits: write them through the same
    /// checked write; if the file changed meanwhile, keep them in a copy.
    fn drop(&mut self) {
        self.finish_typing();
        let (Some(d), Some(vault)) = (&self.d, &self.vault) else { return };
        if !self.dirty && self.conflict.is_none() {
            return;
        }
        let text = d.to_text();
        if text == self.disk_text {
            return;
        }
        let written = self.conflict.is_none() && vault.write_if(&self.path, Some(&self.disk_text), &text).unwrap_or(false);
        if !written {
            let name = self.path.file_name();
            let lower = name.to_ascii_lowercase();
            let ext = [".excalidraw.md", ".excalidraw"].into_iter().find(|e| lower.ends_with(e)).unwrap_or(".excalidraw");
            let stem = &name[..name.len() - ext.len()];
            if let Some(copy) = (1..1000)
                .map(|n| NotePath::join(self.path.folder(), &format!("{stem} (unsaved edits {n}){ext}")))
                .find(|p| !vault.exists(p))
            {
                let _ = vault.write(&copy, &text);
            }
        }
    }
}

/// Open the editor for `path`: a drawing note's Markdown tab, if open, is
/// saved and closed (one editor at a time per file).
pub(crate) fn open(ws: &mut Workspace, surfaces: &mut crate::surfaces::Surfaces, path: &NotePath) {
    AS_MARKDOWN.with_borrow_mut(|m| m.remove(path));
    if let Some(id) = ws.docs().iter().find(|d| d.path.as_ref() == Some(path)).map(|d| d.id) {
        ws.close(id);
    }
    surfaces.open(Box::new(DrawingEditor::new(path)));
}

/// Once a frame: a drawing note shown as Markdown opens in the editor
/// instead, unless the person chose Markdown for it.
pub(crate) fn route(ws: &mut Workspace, surfaces: &mut crate::surfaces::Surfaces) {
    if surfaces.active.is_some() {
        return;
    }
    let Some(path) = ws.active_doc().and_then(|d| d.path.clone()).filter(|p| excalidraw::is_drawing(p.as_str())) else { return };
    if AS_MARKDOWN.with_borrow(|m| m.contains(&path)) {
        return;
    }
    open(ws, surfaces, &path);
}

/// The drawing an embed in a note points to (`![[Plan.excalidraw]]`).
pub(crate) fn embed_target(ws: &Workspace, target: &str) -> Option<NotePath> {
    let t = target.split(['|', '#']).next().unwrap_or(target).trim();
    if !excalidraw::is_drawing(t) {
        return None;
    }
    let vault = ws.vault()?;
    let with_md = if t.to_ascii_lowercase().ends_with(".md") { t.to_string() } else { format!("{t}.md") };
    for cand in [t, with_md.as_str()] {
        if let Some(p) = vault.resolve_file(cand) {
            return Some(p);
        }
        let p = NotePath::new(cand);
        if vault.exists(&p) {
            return Some(p);
        }
    }
    let stem = t.trim_end_matches(".md");
    ws.index().read().resolve(stem, None)
}

#[cfg(test)]
mod tests;
