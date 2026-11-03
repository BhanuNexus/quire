// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The canvas editor (design §G.1): a surface for `.canvas` files.
//!
//! Pan (scroll, middle or Space drag, the hand tool), zoom (Ctrl+scroll,
//! pinch; 10 %–400 %), a grid with snap; select, marquee, move, resize,
//! group; cards are made with a double-click and edited in place; edges are
//! dragged from a card's side handles; colours, align and distribute, copy
//! and paste, undo and redo. Text cards show Markdown (Mermaid diagrams in
//! the full edition); file cards preview notes, images, PDFs and sketches.
//! Pen, marker, highlighter, eraser and shapes draw into ink sketches (SVG
//! files shown through file cards). Only what is in view is drawn.
//!
//! The file is written in Quire's one-node-per-line form 0.6 s after the
//! last change and when the surface closes; a change made elsewhere is taken
//! in, or merged node by node with unsaved changes.

mod ink;
mod loader;
mod review;
mod text;

pub(crate) use ink::SketchSurface;
pub(crate) use loader::install as install_sketch_loader;
pub(crate) use review::{CanvasUi, banner, banner_available};

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use egui::{Align2, Color32, CursorIcon, FontId, Id, Key, Pos2, Rect, RichText, Sense, Stroke, StrokeKind, Vec2};
use quire_app::Workspace;
use quire_app::vault::{NotePath, Vault};
use quire_app::workspace::{NoticeLevel, UiRequest};
use quire_canvas::geom::{self, Align, Side};
use quire_canvas::ink::Sketch;
use quire_canvas::{Canvas, Color, Edge, Node, NodeKind};

use self::ink::{Gesture, InkSettings, InkTool, Place};
use crate::panels::PanelOut;
use crate::surfaces::Surface;
use crate::theme::Palette;

/// The grid, in canvas units.
const GRID: f32 = 20.0;
const MIN_ZOOM: f32 = 0.1;
const MAX_ZOOM: f32 = 4.0;
/// Below this zoom, cards show their title only (and no Markdown layout).
const DETAIL_ZOOM: f32 = 0.45;
const SAVE_AFTER: Duration = Duration::from_millis(600);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Select,
    Hand,
    Ink(InkTool),
}

/// What a drag is doing.
#[derive(Debug, Clone)]
enum Drag {
    Pan,
    Move { start: (f32, f32), origins: Vec<(String, i64, i64)>, moved: bool },
    Resize { id: String, start: geom::Rect, aspect: Option<f32> },
    Marquee { start: (f32, f32), additive: bool },
    Connect { from: String, side: Side, to: (f32, f32) },
    Ink { node: String, sketch: NotePath, gesture: Gesture },
}

/// A card or label being typed in.
#[derive(Debug, Clone)]
enum Editing {
    Text { id: String, buf: String, fresh: bool },
    Label { id: String, buf: String },
    EdgeLabel { id: String, buf: String },
}

/// Where the context menu opened: the canvas point, and the card or edge there.
type MenuAt = ((f32, f32), Option<String>, Option<String>);

/// One step of undo: the canvas and the sketches it changed, before.
#[derive(Clone)]
struct Snapshot {
    canvas: Canvas,
    sketches: Vec<(NotePath, Sketch)>,
}

struct SketchEntry {
    sketch: Sketch,
    disk_text: String,
    disk_gen: u64,
    dirty: bool,
}

#[derive(Default)]
struct AddPopup {
    query: String,
    focused: bool,
    at: (f32, f32),
}

/// Previews of the files cards show.
#[derive(Default)]
struct Previews {
    notes: HashMap<NotePath, (Instant, String)>,
    #[cfg(feature = "typeset")]
    pdf: HashMap<NotePath, PdfThumb>,
}

#[cfg(feature = "typeset")]
enum PdfThumb {
    Pending(crossbeam_channel::Receiver<Option<egui::ColorImage>>),
    Ready(egui::TextureHandle),
    Failed,
}

/// The editor for one `.canvas` file.
pub(crate) struct CanvasSurface {
    path: NotePath,
    vault: Option<Vault>,
    loaded: bool,
    canvas: Canvas,
    /// The file couldn't be read as a canvas: shown, never written.
    error: Option<String>,
    /// The file's text as last read or written (the base for merges).
    disk_text: String,
    disk_gen: u64,
    dirty: bool,
    last_edit: Instant,
    undo: Vec<Snapshot>,
    redo: Vec<Snapshot>,
    offset: Vec2,
    zoom: f32,
    fitted: bool,
    sel: Vec<String>,
    sel_edge: Option<String>,
    mode: Mode,
    ink: InkSettings,
    /// The sketch new strokes go to while it stays selected.
    active_sketch: Option<String>,
    drag: Option<Drag>,
    editing: Option<Editing>,
    grid: bool,
    snap: bool,
    galleys: HashMap<String, (u64, Arc<egui::Galley>)>,
    previews: Previews,
    sketches: HashMap<NotePath, SketchEntry>,
    seed: u64,
    add: Option<AddPopup>,
    /// Where the context menu was opened (canvas point, node or edge).
    menu_at: Option<MenuAt>,
    last_frame_ms: f32,
    /// To redraw sketch pictures in notes after a save.
    ctx: Option<egui::Context>,
}

impl CanvasSurface {
    pub(crate) fn new(path: &NotePath) -> Self {
        let seed = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0x5eed, |d| d.as_nanos() as u64);
        Self {
            path: path.clone(),
            vault: None,
            loaded: false,
            canvas: Canvas::default(),
            error: None,
            disk_text: String::new(),
            disk_gen: 0,
            dirty: false,
            last_edit: Instant::now(),
            undo: Vec::new(),
            redo: Vec::new(),
            offset: Vec2::ZERO,
            zoom: 1.0,
            fitted: false,
            sel: Vec::new(),
            sel_edge: None,
            mode: Mode::Select,
            ink: InkSettings::default(),
            active_sketch: None,
            drag: None,
            editing: None,
            grid: true,
            snap: true,
            galleys: HashMap::new(),
            previews: Previews::default(),
            sketches: HashMap::new(),
            seed,
            add: None,
            menu_at: None,
            last_frame_ms: 0.0,
            ctx: None,
        }
    }

    // ------------------------------------------------------------ file

    fn load(&mut self, ws: &Workspace) {
        let Some(vault) = ws.vault().cloned() else { return };
        self.disk_gen = ws.features.canvas.disk_generation(&self.path);
        match vault.read(&self.path) {
            Ok(t) => {
                match quire_canvas::parse(&t) {
                    Ok(c) => {
                        self.canvas = c;
                        self.error = None;
                    }
                    Err(e) => self.error = Some(e),
                }
                self.disk_text = t;
            }
            Err(e) => self.error = Some(e.to_string()),
        }
        self.vault = Some(vault);
        self.loaded = true;
    }

    /// Take in a change made elsewhere: reload, or merge it with ours.
    fn absorb(&mut self, ws: &mut Workspace) {
        self.disk_gen = ws.features.canvas.disk_generation(&self.path);
        let Some(vault) = self.vault.clone() else { return };
        let Ok(theirs) = vault.read(&self.path) else { return };
        if theirs == self.disk_text {
            return; // our own write
        }
        if !self.dirty {
            match quire_canvas::parse(&theirs) {
                Ok(c) => {
                    self.canvas = c;
                    self.error = None;
                    self.galleys.clear();
                }
                Err(e) => self.error = Some(e),
            }
            self.disk_text = theirs;
            return;
        }
        // Unsaved changes here and a change there: merge node by node.
        let canon = |t: &str| quire_canvas::parse(t).map(|c| quire_canvas::to_canonical(&c));
        let (Ok(base), Ok(other)) = (canon(&self.disk_text), canon(&theirs)) else {
            // The next save writes ours over it: keep theirs beside it first.
            let copy = self.elsewhere_copy(&vault);
            let kept = vault.storage().write_atomic(&copy, &theirs).is_ok();
            ws.notify(
                NoticeLevel::Warn,
                if kept {
                    format!(
                        "{} changed elsewhere in a form Quire can't read; your version is kept and the other is in {}.",
                        self.path.file_name(),
                        copy.file_name()
                    )
                } else {
                    format!(
                        "{} changed elsewhere in a form Quire can't read, and a copy couldn't be kept; not saving over it.",
                        self.path.file_name()
                    )
                },
            );
            if !kept {
                self.error = Some(format!("{} changed elsewhere and couldn't be kept", self.path.file_name()));
            }
            self.disk_text = theirs;
            return;
        };
        let ours = quire_canvas::to_canonical(&self.canvas);
        let m = quire_app::text::merge::merge3(&base, &ours, &other);
        if let Some(c) = m.is_clean().then(|| quire_canvas::parse(&m.text)).and_then(Result::ok) {
            self.canvas = c;
            self.galleys.clear();
            ws.notify(NoticeLevel::Info, format!("{} was also changed elsewhere; both changes are kept.", self.path.file_name()));
        } else {
            // Keep ours and put theirs beside it, so nothing is lost.
            let copy = self.elsewhere_copy(&vault);
            let _ = vault.storage().write_atomic(&copy, &theirs);
            ws.notify(
                NoticeLevel::Warn,
                format!(
                    "{} was changed elsewhere in the same places; your version is kept and the other is in {}.",
                    self.path.file_name(),
                    copy.file_name()
                ),
            );
        }
        self.disk_text = theirs;
        self.dirty = true;
        self.last_edit = Instant::now();
    }

    /// A free name for a copy of the version changed elsewhere.
    fn elsewhere_copy(&self, vault: &Vault) -> NotePath {
        (1..1000)
            .map(|n| {
                let tag = if n == 1 { String::new() } else { format!(" {n}") };
                NotePath::join(self.path.folder(), &format!("{} (changed elsewhere{tag}).canvas", self.path.stem()))
            })
            .find(|p| !vault.exists(p))
            .unwrap_or_else(|| NotePath::join(self.path.folder(), &format!("{} (changed elsewhere).canvas", self.path.stem())))
    }

    fn save(&mut self) {
        let Some(vault) = self.vault.clone() else { return };
        if self.error.is_some() {
            return;
        }
        for (p, e) in &mut self.sketches {
            if e.dirty {
                let t = quire_canvas::ink::to_svg(&e.sketch);
                if vault.storage().write_atomic(p, &t).is_ok() {
                    e.disk_text = t;
                    e.dirty = false;
                    if let (Some(ctx), Some(abs)) = (&self.ctx, vault.storage().absolute(p)) {
                        loader::forget(ctx, &abs);
                    }
                }
            }
        }
        let text = quire_canvas::to_canonical(&self.canvas);
        // Unchanged content keeps the file as it was written (an Obsidian file stays as is).
        let same = quire_canvas::parse(&self.disk_text).is_ok_and(|c| c == self.canvas);
        if !same && vault.storage().write_atomic(&self.path, &text).is_ok() {
            self.disk_text = text;
        }
        self.dirty = false;
    }

    /// Before a change: remember the canvas (and these sketches) for Undo.
    fn checkpoint(&mut self, sketches: &[NotePath]) {
        let sk = sketches.iter().filter_map(|p| self.sketches.get(p).map(|e| (p.clone(), e.sketch.clone()))).collect();
        self.undo.push(Snapshot { canvas: self.canvas.clone(), sketches: sk });
        if self.undo.len() > 200 {
            self.undo.remove(0);
        }
        self.redo.clear();
        self.touch();
    }

    fn touch(&mut self) {
        self.dirty = true;
        self.last_edit = Instant::now();
    }

    fn restore(&mut self, from_undo: bool) {
        let stack = if from_undo { &mut self.undo } else { &mut self.redo };
        let Some(snap) = stack.pop() else { return };
        let current = Snapshot {
            canvas: self.canvas.clone(),
            sketches: snap.sketches.iter().filter_map(|(p, _)| self.sketches.get(p).map(|e| (p.clone(), e.sketch.clone()))).collect(),
        };
        (if from_undo { &mut self.redo } else { &mut self.undo }).push(current);
        self.canvas = snap.canvas;
        for (p, s) in snap.sketches {
            if let Some(e) = self.sketches.get_mut(&p) {
                e.sketch = s;
                e.dirty = true;
            }
        }
        self.sel.retain(|id| self.canvas.node(id).is_some());
        self.galleys.clear();
        self.touch();
    }

    fn new_id(&mut self) -> String {
        self.canvas.new_id(&mut self.seed)
    }

    // ------------------------------------------------------------ view

    fn to_screen(&self, rect: Rect, p: (f32, f32)) -> Pos2 {
        rect.min + (Vec2::new(p.0, p.1) - self.offset) * self.zoom
    }

    fn to_canvas(&self, rect: Rect, s: Pos2) -> (f32, f32) {
        let v = self.offset + (s - rect.min) / self.zoom;
        (v.x, v.y)
    }

    fn screen_rect(&self, rect: Rect, r: geom::Rect) -> Rect {
        Rect::from_min_size(self.to_screen(rect, (r.x, r.y)), Vec2::new(r.w, r.h) * self.zoom)
    }

    fn fit(&mut self, rect: Rect) {
        let b = self.canvas.bounds().unwrap_or(geom::Rect::new(-200.0, -150.0, 400.0, 300.0)).expand(60.0);
        self.zoom = (rect.width() / b.w).min(rect.height() / b.h).clamp(MIN_ZOOM, 1.0);
        let c = b.center();
        self.offset = Vec2::new(c.0, c.1) - rect.size() / 2.0 / self.zoom;
        self.fitted = true;
    }

    fn zoom_at(&mut self, rect: Rect, at: Pos2, factor: f32) {
        let before = self.to_canvas(rect, at);
        self.zoom = (self.zoom * factor).clamp(MIN_ZOOM, MAX_ZOOM);
        let after = self.to_canvas(rect, at);
        self.offset += Vec2::new(before.0 - after.0, before.1 - after.1);
    }

    fn snapped(&self, v: f32) -> f32 {
        if self.snap { geom::snap(v, GRID) } else { v }
    }

    // ------------------------------------------------------------ sketches

    /// A sketch file's content (read once, then kept).
    fn sketch_for(&mut self, ws: &Workspace, file: &str) -> Option<&mut SketchEntry> {
        let path = NotePath::new(file);
        let generation = ws.features.canvas.disk_generation(&path);
        let stale = self.sketches.get(&path).is_some_and(|e| !e.dirty && e.disk_gen != generation);
        if !self.sketches.contains_key(&path) || stale {
            let text = self.vault.as_ref()?.read(&path).ok()?;
            if self.sketches.get(&path).is_some_and(|e| e.disk_text == text) {
                if let Some(e) = self.sketches.get_mut(&path) {
                    e.disk_gen = generation;
                }
            } else {
                let sketch = quire_canvas::ink::parse_svg(&text).ok()?;
                self.sketches.insert(path.clone(), SketchEntry { sketch, disk_text: text, disk_gen: generation, dirty: false });
            }
        }
        self.sketches.get_mut(&path)
    }

    fn is_sketch(n: &Node) -> bool {
        n.node_kind() == NodeKind::File && n.file.as_deref().is_some_and(|f| f.to_ascii_lowercase().ends_with(".svg"))
    }

    /// A new sketch file and its card at `at`.
    fn new_sketch(&mut self, at: (f32, f32)) -> Option<(String, NotePath)> {
        let vault = self.vault.clone()?;
        let folder = vault.config.attachments_folder.clone();
        let stem = quire_app::vault::sanitize_title(self.path.stem());
        let taken = |p: &NotePath| vault.exists(p) || self.sketches.contains_key(p);
        let path = (1..10_000).map(|n| NotePath::join(&folder, &format!("{stem}-ink-{n}.svg"))).find(|p| !taken(p))?;
        let sketch = Sketch { width: 1.0, height: 1.0, items: Vec::new() };
        self.sketches.insert(path.clone(), SketchEntry { sketch, disk_text: String::new(), disk_gen: 0, dirty: true });
        let id = self.new_id();
        let mut n = Node::new(&id, NodeKind::File, at.0 as i64, at.1 as i64, 1, 1);
        n.file = Some(path.as_str().to_string());
        self.canvas.nodes.push(n);
        Some((id, path))
    }

    /// The scale from a sketch's units to canvas units (its card's size).
    fn sketch_scale(n: &Node, s: &Sketch) -> f32 {
        (n.width as f32 / s.width.max(1.0)).min(n.height as f32 / s.height.max(1.0)).max(1e-3)
    }

    /// After drawing: the card hugs the drawing.
    fn fit_sketch(&mut self, node: &str, path: &NotePath) {
        let Some(e) = self.sketches.get_mut(path) else { return };
        let Some(n) = self.canvas.node_mut(node) else { return };
        let scale = if e.sketch.width <= 1.0 { 1.0 } else { Self::sketch_scale(n, &e.sketch) };
        if e.sketch.content_bounds().is_none() {
            return;
        }
        let (dx, dy) = e.sketch.fit(12.0);
        n.x -= (dx * scale).round() as i64;
        n.y -= (dy * scale).round() as i64;
        n.width = ((e.sketch.width * scale).round() as i64).max(8);
        n.height = ((e.sketch.height * scale).round() as i64).max(8);
        e.dirty = true;
    }

    // ------------------------------------------------------------ editing

    fn selected_indices(&self) -> Vec<usize> {
        self.sel.iter().filter_map(|id| self.canvas.node_index(id)).collect()
    }

    fn delete_selection(&mut self) {
        if self.sel.is_empty() && self.sel_edge.is_none() {
            return;
        }
        self.checkpoint(&[]);
        for id in std::mem::take(&mut self.sel) {
            self.canvas.remove_node(&id);
        }
        if let Some(e) = self.sel_edge.take() {
            self.canvas.edges.retain(|x| x.id != e);
        }
    }

    fn set_color(&mut self, color: Option<String>) {
        if self.sel.is_empty() && self.sel_edge.is_none() {
            return;
        }
        self.checkpoint(&[]);
        for id in self.sel.clone() {
            if let Some(n) = self.canvas.node_mut(&id) {
                n.color.clone_from(&color);
            }
        }
        if let Some(e) = self.sel_edge.clone()
            && let Some(edge) = self.canvas.edges.iter_mut().find(|x| x.id == e)
        {
            edge.color = color;
        }
    }

    fn group_selection(&mut self) {
        let idx = self.selected_indices();
        let Some(b) = idx.iter().map(|i| self.canvas.nodes[*i].rect()).reduce(geom::Rect::union) else { return };
        self.checkpoint(&[]);
        let id = self.new_id();
        let mut g = Node::new(&id, NodeKind::Group, 0, 0, 1, 1);
        g.set_rect(b.expand(30.0));
        g.label = Some("Group".into());
        // Groups sit behind what they hold.
        let at = idx.iter().copied().min().unwrap_or(0);
        self.canvas.nodes.insert(at, g);
        self.sel = vec![id.clone()];
        self.editing = Some(Editing::Label { id, buf: "Group".into() });
    }

    fn align(&mut self, how: Option<Align>, distribute: Option<bool>) {
        let idx = self.selected_indices();
        if idx.len() < 2 {
            return;
        }
        self.checkpoint(&[]);
        if let Some(a) = how {
            geom::align(&mut self.canvas, &idx, a);
        }
        if let Some(h) = distribute {
            geom::distribute(&mut self.canvas, &idx, h);
        }
    }

    fn bring_forward(&mut self, front: bool) {
        if self.sel.is_empty() {
            return;
        }
        self.checkpoint(&[]);
        let (mut picked, rest): (Vec<Node>, Vec<Node>) =
            std::mem::take(&mut self.canvas.nodes).into_iter().partition(|n| self.sel.contains(&n.id));
        self.canvas.nodes = if front {
            let mut v = rest;
            v.append(&mut picked);
            v
        } else {
            picked.extend(rest);
            picked
        };
    }

    /// The selection as JSON Canvas text (for the clipboard).
    fn copy_text(&self) -> Option<String> {
        if self.sel.is_empty() {
            return None;
        }
        let nodes: Vec<Node> = self.canvas.nodes.iter().filter(|n| self.sel.contains(&n.id)).cloned().collect();
        let edges: Vec<Edge> =
            self.canvas.edges.iter().filter(|e| self.sel.contains(&e.from_node) && self.sel.contains(&e.to_node)).cloned().collect();
        Some(quire_canvas::to_canonical(&Canvas { nodes, edges, ..Canvas::default() }))
    }

    /// Paste canvas JSON (new ids, placed at `at`), a link, or text as a card.
    fn paste(&mut self, text: &str, at: (f32, f32)) {
        let t = text.trim();
        if t.is_empty() {
            return;
        }
        self.checkpoint(&[]);
        if let Some(c) = quire_canvas::parse(t).ok().filter(|c| !c.nodes.is_empty()) {
            let b = c.bounds().unwrap_or_default();
            let mut map = HashMap::new();
            let mut new_sel = Vec::new();
            for mut n in c.nodes {
                let id = self.new_id();
                map.insert(n.id.clone(), id.clone());
                n.id.clone_from(&id);
                n.x += (at.0 - b.x).round() as i64;
                n.y += (at.1 - b.y).round() as i64;
                new_sel.push(id);
                self.canvas.nodes.push(n);
            }
            for mut e in c.edges {
                let (Some(f), Some(to)) = (map.get(&e.from_node), map.get(&e.to_node)) else { continue };
                e.from_node.clone_from(f);
                e.to_node.clone_from(to);
                e.id = self.new_id();
                self.canvas.edges.push(e);
            }
            self.sel = new_sel;
            return;
        }
        let id = self.new_id();
        let n = if (t.starts_with("http://") || t.starts_with("https://")) && !t.contains(char::is_whitespace) {
            let mut n = Node::new(&id, NodeKind::Link, at.0 as i64, at.1 as i64, 400, 300);
            n.url = Some(t.to_string());
            n
        } else {
            let lines = t.lines().count().clamp(1, 20) as i64;
            let mut n = Node::new(&id, NodeKind::Text, at.0 as i64, at.1 as i64, 260, 40 + 22 * lines);
            n.text = Some(t.to_string());
            n
        };
        self.canvas.nodes.push(n);
        self.sel = vec![id];
    }

    fn add_text_card(&mut self, at: (f32, f32)) {
        self.checkpoint(&[]);
        let id = self.new_id();
        let (x, y) = (self.snapped(at.0 - 125.0), self.snapped(at.1 - 30.0));
        let mut n = Node::new(&id, NodeKind::Text, x as i64, y as i64, 250, 60);
        n.text = Some(String::new());
        self.canvas.nodes.push(n);
        self.sel = vec![id.clone()];
        self.editing = Some(Editing::Text { id, buf: String::new(), fresh: true });
    }

    fn add_file_card(&mut self, file: &NotePath, at: (f32, f32)) {
        self.checkpoint(&[]);
        let id = self.new_id();
        let ext = file.as_str().rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).unwrap_or_default();
        let (w, h) = match ext.as_str() {
            "md" => (400, 400),
            "pdf" => (400, 520),
            "png" | "jpg" | "jpeg" | "gif" | "webp" | "svg" => (400, 300),
            _ => (300, 120),
        };
        let mut n = Node::new(&id, NodeKind::File, self.snapped(at.0) as i64, self.snapped(at.1) as i64, w, h);
        n.file = Some(file.as_str().to_string());
        self.canvas.nodes.push(n);
        self.sel = vec![id];
    }

    /// Commit what is being typed.
    fn finish_editing(&mut self) {
        let Some(e) = self.editing.take() else { return };
        match e {
            Editing::Text { id, buf, fresh } => {
                let old = self.canvas.node(&id).and_then(|n| n.text.clone()).unwrap_or_default();
                if buf.trim().is_empty() && fresh {
                    // An empty new card goes away (and so does its undo step).
                    self.canvas.remove_node(&id);
                    self.undo.pop();
                    self.sel.clear();
                } else if buf != old {
                    if !fresh {
                        self.checkpoint(&[]);
                    }
                    if let Some(n) = self.canvas.node_mut(&id) {
                        let lines = buf.lines().count().max(1) as i64;
                        n.height = n.height.max(28 + 22 * lines.min(30));
                        n.text = Some(buf);
                    }
                    self.touch();
                }
            }
            Editing::Label { id, buf } => {
                if self.canvas.node(&id).and_then(|n| n.label.clone()).unwrap_or_default() != buf {
                    self.checkpoint(&[]);
                    if let Some(n) = self.canvas.node_mut(&id) {
                        n.label = (!buf.trim().is_empty()).then_some(buf);
                    }
                }
            }
            Editing::EdgeLabel { id, buf } => {
                if self.canvas.edge(&id).and_then(|e| e.label.clone()).unwrap_or_default() != buf {
                    self.checkpoint(&[]);
                    if let Some(e) = self.canvas.edges.iter_mut().find(|e| e.id == id) {
                        e.label = (!buf.trim().is_empty()).then_some(buf);
                    }
                }
            }
        }
    }

    /// Open what a card points at.
    fn open_card(&self, ws: &mut Workspace, ctx: &egui::Context, id: &str) -> bool {
        let Some(n) = self.canvas.node(id) else { return false };
        match n.node_kind() {
            NodeKind::File => {
                let Some(f) = n.file.as_deref() else { return false };
                let p = NotePath::new(f);
                if p.is_markdown() {
                    if ws.open_note(&p).is_some() {
                        if let Some(h) = n.subpath.as_deref().map(|s| s.trim_start_matches('#')).filter(|s| !s.is_empty()) {
                            ws.jump_to_heading(h);
                        }
                        return true;
                    }
                } else {
                    ws.request_ui(UiRequest::OpenFile(p));
                }
                false
            }
            NodeKind::Link => {
                if let Some(u) = n.url.as_deref().filter(|u| u.starts_with("http://") || u.starts_with("https://")) {
                    ctx.open_url(egui::OpenUrl::new_tab(u));
                }
                false
            }
            _ => false,
        }
    }

    // ------------------------------------------------------------ drawing

    fn node_colors(n: Option<&str>, pal: &Palette) -> (Color32, Color32) {
        match n.and_then(Color::parse) {
            Some(c) => {
                let (r, g, b) = c.rgb();
                let border = Color32::from_rgb(r, g, b);
                (border, Color32::from_rgba_unmultiplied(r, g, b, if pal.dark { 40 } else { 26 }))
            }
            None => (pal.border, pal.raised),
        }
    }

    fn galley(&mut self, painter: &egui::Painter, n: &Node, width: f32, pal: &Palette) -> Arc<egui::Galley> {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        n.text.hash(&mut h);
        ((width * 2.0).round() as i64).hash(&mut h);
        ((self.zoom * 100.0).round() as i64).hash(&mut h);
        pal.dark.hash(&mut h);
        pal.text.hash(&mut h);
        let key = h.finish();
        if let Some((k, g)) = self.galleys.get(&n.id)
            && *k == key
        {
            return Arc::clone(g);
        }
        let job = text::markdown_job(n.text.as_deref().unwrap_or_default(), width, 14.0 * self.zoom, pal);
        let g = painter.layout_job(job);
        self.galleys.insert(n.id.clone(), (key, Arc::clone(&g)));
        g
    }

    fn note_preview(&mut self, ws: &Workspace, path: &NotePath) -> String {
        if let Some((t, s)) = self.previews.notes.get(path)
            && t.elapsed() < Duration::from_secs(3)
        {
            return s.clone();
        }
        let text = ws.vault().and_then(|v| v.read(path).ok()).map_or_else(String::new, |t| {
            let body = quire_app::markdown::frontmatter::parse(&t).map_or(t.as_str(), |fm| &t[fm.range.end..]);
            body.lines().take(12).collect::<Vec<_>>().join("\n")
        });
        self.previews.notes.insert(path.clone(), (Instant::now(), text.clone()));
        text
    }

    #[cfg(feature = "typeset")]
    fn pdf_thumb(&mut self, ctx: &egui::Context, path: &NotePath) -> Option<egui::TextureHandle> {
        if !self.previews.pdf.contains_key(path) {
            let bytes = self.vault.as_ref().and_then(|v| v.storage().read_bytes(path).ok());
            let (tx, rx) = crossbeam_channel::bounded(1);
            let c = ctx.clone();
            std::thread::spawn(move || {
                let img = bytes.and_then(|b| {
                    let doc = quire_pdf::PdfDoc::open(b).ok()?;
                    let w = doc.page_info(0).ok()?.width.max(1.0);
                    let img = quire_pdf::render_page(&doc, 0, 360.0 / w).ok()?;
                    Some(egui::ColorImage::from_rgba_unmultiplied([img.width as usize, img.height as usize], &img.rgba))
                });
                let _ = tx.send(img);
                c.request_repaint();
            });
            self.previews.pdf.insert(path.clone(), PdfThumb::Pending(rx));
        }
        let entry = self.previews.pdf.get_mut(path)?;
        if let PdfThumb::Pending(rx) = entry
            && let Ok(r) = rx.try_recv()
        {
            *entry = match r {
                Some(img) => PdfThumb::Ready(ctx.load_texture(format!("quire-canvas-pdf-{path}"), img, egui::TextureOptions::LINEAR)),
                None => PdfThumb::Failed,
            };
        }
        match entry {
            PdfThumb::Ready(t) => Some(t.clone()),
            _ => None,
        }
    }

    fn paint_grid(&self, painter: &egui::Painter, rect: Rect, pal: &Palette) {
        // Dots at least 12 points apart: 20, 100 or 500 units.
        let step =
            [1.0, 5.0, 25.0, 125.0].into_iter().map(|k| GRID * k * self.zoom).find(|s| *s >= 12.0).unwrap_or(GRID * 125.0 * self.zoom);
        let dot = if pal.dark { pal.faint.gamma_multiply(0.35) } else { pal.faint.gamma_multiply(0.6) };
        let first = self.to_screen(rect, (0.0, 0.0));
        let x0 = rect.left() + (first.x - rect.left()).rem_euclid(step);
        let y0 = rect.top() + (first.y - rect.top()).rem_euclid(step);
        let cols = ((rect.right() - x0) / step).ceil().max(0.0) as usize;
        let rows = ((rect.bottom() - y0) / step).ceil().max(0.0) as usize;
        for j in 0..rows {
            for i in 0..cols {
                let p = Pos2::new(x0 + i as f32 * step, y0 + j as f32 * step);
                painter.rect_filled(Rect::from_center_size(p, Vec2::splat(1.5)), 0.0, dot);
            }
        }
    }

    fn paint_edge(&self, painter: &egui::Painter, rect: Rect, e: &Edge, pal: &Palette) {
        let Some(c) = geom::edge_curve(&self.canvas, e) else { return };
        let color = e.color.as_deref().and_then(Color::parse).map_or(pal.muted, |c| {
            let (r, g, b) = c.rgb();
            Color32::from_rgb(r, g, b)
        });
        let selected = self.sel_edge.as_deref() == Some(e.id.as_str());
        let w = if selected { 3.0 } else { 2.0 } * self.zoom.clamp(0.5, 1.5);
        let pts: Vec<Pos2> = geom::flatten(&c, 24).into_iter().map(|p| self.to_screen(rect, p)).collect();
        if selected {
            painter.add(egui::Shape::line(pts.clone(), Stroke::new(w + 4.0, pal.selection)));
        }
        painter.add(egui::Shape::line(pts.clone(), Stroke::new(w, color)));
        let head = |tip: Pos2, dir: Vec2| {
            let s = 10.0 * self.zoom.clamp(0.5, 1.5);
            let n = Vec2::new(-dir.y, dir.x);
            painter.add(egui::Shape::convex_polygon(
                vec![tip, tip - dir * s + n * s * 0.5, tip - dir * s - n * s * 0.5],
                color,
                Stroke::NONE,
            ));
        };
        if e.arrow_at_to() {
            let d = geom::end_direction(&c);
            head(pts[pts.len() - 1], Vec2::new(d.0, d.1));
        }
        if e.arrow_at_from() {
            let d = (c[0].0 - c[1].0, c[0].1 - c[1].1);
            let len = d.0.hypot(d.1).max(1e-3);
            head(pts[0], Vec2::new(d.0 / len, d.1 / len));
        }
        let editing_this = matches!(&self.editing, Some(Editing::EdgeLabel { id, .. }) if *id == e.id);
        if let Some(l) = e.label.as_deref().filter(|l| !l.is_empty() && !editing_this) {
            let mid = self.to_screen(rect, geom::bezier_point(&c, 0.5));
            let g = painter.layout_no_wrap(l.to_string(), FontId::proportional(12.0 * self.zoom.clamp(0.6, 1.4)), pal.text);
            let r = Rect::from_center_size(mid, g.size() + Vec2::new(10.0, 4.0));
            painter.rect_filled(r, 4.0, pal.bg);
            painter.galley(r.min + Vec2::new(5.0, 2.0), g, pal.text);
        }
    }

    fn paint_node(&mut self, ui: &egui::Ui, painter: &egui::Painter, ws: &Workspace, rect: Rect, i: usize, pal: &Palette) {
        let n = self.canvas.nodes[i].clone();
        let r = self.screen_rect(rect, n.rect());
        let selected = self.sel.contains(&n.id);
        let (border, fill) = Self::node_colors(n.color.as_deref(), pal);
        let z = self.zoom;
        let radius = (8.0 * z).clamp(2.0, 10.0);
        let detail = z >= DETAIL_ZOOM;
        match n.node_kind() {
            NodeKind::Group => {
                let fill = n.color.as_deref().and_then(Color::parse).map_or_else(
                    || pal.panel.gamma_multiply(0.5),
                    |c| {
                        let (rr, g, b) = c.rgb();
                        Color32::from_rgba_unmultiplied(rr, g, b, 18)
                    },
                );
                painter.rect_filled(r, radius, fill);
                painter.rect_stroke(
                    r,
                    radius,
                    Stroke::new(if selected { 2.0 } else { 1.0 }, if selected { pal.accent } else { border }),
                    StrokeKind::Inside,
                );
                let editing_label = matches!(&self.editing, Some(Editing::Label { id, .. }) if *id == n.id);
                if !editing_label && let Some(l) = n.label.as_deref().filter(|l| !l.is_empty()) {
                    let size = (15.0 * z).clamp(9.0, 26.0);
                    let g = painter.layout_no_wrap(
                        l.to_string(),
                        FontId::new(size, egui::FontFamily::Name(crate::theme::BOLD.into())),
                        pal.muted,
                    );
                    painter.galley(Pos2::new(r.left() + 4.0, r.top() - g.size().y - 4.0), g, pal.muted);
                }
                return;
            }
            _ => {
                if !Self::is_sketch(&n) {
                    painter.rect_filled(r, radius, if n.color.is_some() { pal.raised } else { fill });
                    if n.color.is_some() {
                        painter.rect_filled(r, radius, fill);
                    }
                    painter.rect_stroke(r, radius, Stroke::new(if n.color.is_some() { 2.0 } else { 1.0 }, border), StrokeKind::Inside);
                }
            }
        }
        let inner = r.shrink((12.0 * z).clamp(3.0, 14.0));
        let clip = painter.with_clip_rect(r.intersect(rect));
        if detail {
            match n.node_kind() {
                NodeKind::Text => {
                    let editing_this = matches!(&self.editing, Some(Editing::Text { id, .. }) if *id == n.id);
                    if !editing_this {
                        let src = n.text.as_deref().unwrap_or_default();
                        let drawn = text::mermaid(src).is_some_and(|m| self.paint_diagram(ui, &clip, inner, m, pal));
                        if !drawn {
                            let g = self.galley(painter, &n, inner.width(), pal);
                            clip.galley(inner.min, g, pal.text);
                        }
                    }
                }
                NodeKind::File => self.paint_file(ui, &clip, ws, &n, r, inner, pal),
                NodeKind::Link => {
                    let url = n.url.clone().unwrap_or_default();
                    let host = url.split("://").nth(1).and_then(|s| s.split('/').next()).unwrap_or(&url).to_string();
                    clip.text(inner.min, Align2::LEFT_TOP, format!("🔗 {host}"), FontId::proportional(14.0 * z), pal.strong);
                    clip.text(inner.min + Vec2::new(0.0, 22.0 * z), Align2::LEFT_TOP, &url, FontId::proportional(11.0 * z), pal.link);
                    clip.text(
                        inner.left_bottom(),
                        Align2::LEFT_BOTTOM,
                        "Double-click to open in the browser",
                        FontId::proportional(11.0 * z),
                        pal.faint,
                    );
                }
                NodeKind::Group | NodeKind::Other => {
                    clip.text(
                        inner.min,
                        Align2::LEFT_TOP,
                        format!("{} (kept as it is)", n.kind),
                        FontId::proportional(12.0 * z),
                        pal.muted,
                    );
                }
            }
        } else {
            // Far out: just the title, so a thousand cards stay quick.
            if r.height() > 10.0 && r.width() > 20.0 {
                let size = (r.height() * 0.25).clamp(6.0, 14.0);
                clip.text(r.left_top() + Vec2::new(4.0, 3.0), Align2::LEFT_TOP, n.title(), FontId::proportional(size), pal.muted);
            }
            if Self::is_sketch(&n)
                && let Some(f) = n.file.clone()
                && let Some(e) = self.sketch_for(ws, &f)
            {
                let scale = Self::sketch_scale(&n, &e.sketch) * z;
                ink::paint_sketch(&clip, &e.sketch, Place { origin: r.min, scale }, pal);
            }
        }
        if selected {
            painter.rect_stroke(r.expand(2.0), radius + 2.0, Stroke::new(2.0, pal.accent), StrokeKind::Outside);
            if self.sel.len() == 1 {
                let h = Rect::from_center_size(r.right_bottom(), Vec2::splat(9.0));
                painter.rect_filled(h, 2.0, pal.accent);
            }
        }
    }

    /// A Mermaid card (full edition); false when it isn't drawn (Lite, or still rendering).
    fn paint_diagram(&self, ui: &egui::Ui, painter: &egui::Painter, inner: Rect, src: &str, pal: &Palette) -> bool {
        if !crate::typeset_ui::AVAILABLE {
            return false;
        }
        match crate::typeset_ui::request(ui.ctx(), &crate::typeset_ui::Job::Diagram { source: src.to_string(), dark: pal.dark }) {
            crate::typeset_ui::Out::Ready(p) => {
                let s = (inner.width() / p.size.x).min(inner.height() / p.size.y).min(self.zoom * 1.5);
                let r = Rect::from_center_size(inner.center(), p.size * s);
                painter.image(p.texture.id(), r, Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)), Color32::WHITE);
                true
            }
            crate::typeset_ui::Out::Pending => {
                painter.text(inner.center(), Align2::CENTER_CENTER, "Drawing the diagram…", FontId::proportional(12.0), pal.muted);
                true
            }
            crate::typeset_ui::Out::Failed(e) => {
                painter.text(inner.min, Align2::LEFT_TOP, format!("Diagram error: {e}"), FontId::proportional(12.0), pal.danger);
                true
            }
        }
    }

    fn paint_file(&mut self, ui: &egui::Ui, painter: &egui::Painter, ws: &Workspace, n: &Node, r: Rect, inner: Rect, pal: &Palette) {
        let z = self.zoom;
        let Some(file) = n.file.clone() else { return };
        let path = NotePath::new(&file);
        let ext = file.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).unwrap_or_default();
        if Self::is_sketch(n) {
            let drawing = match &self.drag {
                Some(Drag::Ink { node, gesture, .. }) if *node == n.id => Some(gesture.clone()),
                _ => None,
            };
            if let Some(e) = self.sketch_for(ws, &file) {
                let scale = Self::sketch_scale(n, &e.sketch) * z;
                ink::paint_sketch(painter, &e.sketch, Place { origin: r.min, scale }, pal);
                if let Some(g) = drawing {
                    g.paint(painter, Place { origin: r.min, scale }, pal);
                }
            } else {
                painter.text(inner.min, Align2::LEFT_TOP, format!("Missing drawing: {file}"), FontId::proportional(12.0 * z), pal.warn);
            }
            return;
        }
        let exists = ws.vault().is_some_and(|v| v.exists(&path));
        if !exists {
            painter.text(inner.min, Align2::LEFT_TOP, format!("Missing: {file}"), FontId::proportional(13.0 * z), pal.warn);
            return;
        }
        let title_h = 22.0 * z;
        painter.text(
            inner.min,
            Align2::LEFT_TOP,
            format!("{}{}", path.file_name(), n.subpath.as_deref().unwrap_or_default()),
            FontId::new(13.0 * z, egui::FontFamily::Name(crate::theme::BOLD.into())),
            pal.muted,
        );
        let body = Rect::from_min_max(inner.min + Vec2::new(0.0, title_h), inner.max);
        match ext.as_str() {
            "md" => {
                let src = self.note_preview(ws, &path);
                let job = text::markdown_job(&src, body.width(), 13.0 * z, pal);
                painter.add(egui::Shape::galley(body.min, painter.layout_job(job), pal.text));
            }
            "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" => {
                if let Some(abs) = ws.vault().and_then(|v| v.storage().absolute(&path)) {
                    egui::Image::new(format!("file://{}", abs.display())).maintain_aspect_ratio(true).paint_at(ui, body);
                }
            }
            "pdf" => {
                #[cfg(feature = "typeset")]
                if let Some(t) = self.pdf_thumb(ui.ctx(), &path) {
                    let s = t.size_vec2();
                    let k = (body.width() / s.x).min(body.height() / s.y);
                    let rr = Rect::from_min_size(body.center_top() - Vec2::new(s.x * k / 2.0, 0.0), s * k);
                    painter.image(t.id(), rr, Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)), Color32::WHITE);
                    return;
                }
                painter.text(body.center(), Align2::CENTER_CENTER, "PDF", FontId::proportional(28.0 * z), pal.faint);
            }
            _ => {
                painter.text(body.center(), Align2::CENTER_CENTER, ext.to_uppercase(), FontId::proportional(22.0 * z), pal.faint);
            }
        }
    }

    // ------------------------------------------------------------ toolbar

    fn toolbar(&mut self, ui: &mut egui::Ui, rect_hint: Option<Rect>, pal: &Palette) {
        // One row (two on a narrow window): card tools give way to the ink
        // options while drawing.
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            for (m, icon, tip) in [(Mode::Select, "⬉", "Select (V)"), (Mode::Hand, "✋", "Move the view (H, or hold Space)")] {
                if ui.selectable_label(self.mode == m, RichText::new(icon).size(15.0)).on_hover_text(tip).clicked() {
                    self.mode = m;
                }
            }
            if ui.button("+ Card").on_hover_text("New text card (or double-click the canvas)").clicked()
                && let Some(r) = rect_hint
            {
                let c = self.to_canvas(r, r.center());
                self.add_text_card(c);
            }
            if ui.button("+ File…").on_hover_text("Add a note, PDF, image or canvas as a card").clicked()
                && let Some(r) = rect_hint
            {
                self.add = Some(AddPopup { at: self.to_canvas(r, r.center()), ..AddPopup::default() });
            }
            ui.separator();
            let mut tool = match self.mode {
                Mode::Ink(t) => Some(t),
                _ => None,
            };
            if ink::toolbar(ui, &mut tool, &mut self.ink, pal) {
                self.mode = tool.map_or(Mode::Select, Mode::Ink);
            }
            ui.separator();
            let inking = matches!(self.mode, Mode::Ink(_));
            let any = !self.sel.is_empty() || self.sel_edge.is_some();
            if !inking {
                ui.add_enabled_ui(any, |ui| {
                    ui.menu_button("🎨", |ui| {
                        if ui.button("No colour").clicked() {
                            self.set_color(None);
                            ui.close();
                        }
                        for k in 1..=6u8 {
                            let (r, g, b) = Color::Preset(k).rgb();
                            if ui.button(RichText::new(format!("● {}", Color::preset_name(k))).color(Color32::from_rgb(r, g, b))).clicked()
                            {
                                self.set_color(Some(k.to_string()));
                                ui.close();
                            }
                        }
                    })
                    .response
                    .on_hover_text("Colour");
                });
                ui.add_enabled_ui(self.sel.len() >= 2, |ui| {
                    ui.menu_button("☰", |ui| {
                        for (label, a) in [
                            ("Align left", Align::Left),
                            ("Align centres", Align::CenterX),
                            ("Align right", Align::Right),
                            ("Align top", Align::Top),
                            ("Align middles", Align::CenterY),
                            ("Align bottom", Align::Bottom),
                        ] {
                            if ui.button(label).clicked() {
                                self.align(Some(a), None);
                                ui.close();
                            }
                        }
                        ui.separator();
                        if ui.add_enabled(self.sel.len() >= 3, egui::Button::new("Space evenly across")).clicked() {
                            self.align(None, Some(true));
                            ui.close();
                        }
                        if ui.add_enabled(self.sel.len() >= 3, egui::Button::new("Space evenly down")).clicked() {
                            self.align(None, Some(false));
                            ui.close();
                        }
                    })
                    .response
                    .on_hover_text("Align and distribute");
                    if ui.button("▣").on_hover_text("Group (Ctrl+G)").clicked() {
                        self.group_selection();
                    }
                });
            }
            ui.separator();
            if ui.add_enabled(!self.undo.is_empty(), egui::Button::new("⟲")).on_hover_text("Undo (Ctrl+Z)").clicked() {
                self.finish_editing();
                self.restore(true);
            }
            if ui.add_enabled(!self.redo.is_empty(), egui::Button::new("⟳")).on_hover_text("Redo (Ctrl+Shift+Z)").clicked() {
                self.restore(false);
            }
            ui.separator();
            ui.toggle_value(&mut self.grid, "Grid");
            ui.toggle_value(&mut self.snap, "Snap").on_hover_text("Line cards up on the grid when moving and resizing");
            ui.separator();
            if let Some(r) = rect_hint {
                if ui.small_button("−").clicked() {
                    self.zoom_at(r, r.center(), 1.0 / 1.25);
                }
                if ui.button(format!("{:.0} %", self.zoom * 100.0)).on_hover_text("Fit everything (Ctrl+0)").clicked() {
                    self.fit(r);
                }
                if ui.small_button("+").clicked() {
                    self.zoom_at(r, r.center(), 1.25);
                }
            }
            if self.dirty {
                ui.label(RichText::new("●").color(pal.faint)).on_hover_text("Saving…");
            }
        });
    }

    fn add_popup(&mut self, ctx: &egui::Context, ws: &Workspace, pal: &Palette) {
        let Some(add) = &mut self.add else { return };
        let mut chosen = None;
        let mut close = false;
        egui::Window::new("Add a note or file")
            .id(Id::unique(("quire-canvas-add", self.path.as_str())))
            .collapsible(false)
            .resizable(false)
            .anchor(Align2::CENTER_TOP, Vec2::new(0.0, 120.0))
            .show(ctx, |ui| {
                ui.set_width(380.0);
                let r = ui.add(egui::TextEdit::singleline(&mut add.query).hint_text("Search notes and files").desired_width(f32::INFINITY));
                if !add.focused {
                    r.request_focus();
                    add.focused = true;
                }
                let q = add.query.to_lowercase();
                let mut items: Vec<NotePath> = ws.index().read().quick_open(&add.query, 12).into_iter().map(|h| h.path).collect();
                items.extend(
                    ws.other_files()
                        .into_iter()
                        .filter(|p| !p.as_str().ends_with(".canvas") || *p != self.path)
                        .filter(|p| q.is_empty() || p.as_str().to_lowercase().contains(&q))
                        .take(12),
                );
                egui::ScrollArea::vertical().max_height(320.0).show(ui, |ui| {
                    for p in items {
                        if ui.selectable_label(false, p.as_str()).clicked() {
                            chosen = Some(p);
                        }
                    }
                });
                if r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                    chosen = ws.index().read().quick_open(&add.query, 1).into_iter().next().map(|h| h.path);
                }
                ui.horizontal(|ui| {
                    if ui.button("Cancel").clicked() {
                        close = true;
                    }
                    ui.label(RichText::new("Notes show their first lines; PDFs their first page.").size(11.0).color(pal.muted));
                });
            });
        if ctx.input(|i| i.key_pressed(Key::Escape)) {
            close = true;
        }
        if let Some(p) = chosen {
            let at = add.at;
            self.add = None;
            self.add_file_card(&p, at);
        } else if close {
            self.add = None;
        }
    }

    // ------------------------------------------------------------ input

    /// Which side handle of a node is under `p` (screen), if any.
    fn side_handle_at(&self, rect: Rect, n: &Node, p: Pos2) -> Option<Side> {
        let r = n.rect();
        Side::ALL.into_iter().find(|s| {
            let a = s.anchor(r);
            self.to_screen(rect, a).distance(p) <= 8.0
        })
    }

    fn handle_keys(&mut self, ui: &egui::Ui, rect: Rect, pointer: Option<Pos2>) {
        let events = ui.input(|i| i.events.clone());
        let mods = ui.input(|i| i.modifiers);
        let at = pointer.map_or_else(|| self.to_canvas(rect, rect.center()), |p| self.to_canvas(rect, p));
        for e in events {
            match e {
                egui::Event::Copy | egui::Event::Cut => {
                    if let Some(t) = self.copy_text() {
                        ui.ctx().copy_text(t);
                        if matches!(e, egui::Event::Cut) {
                            self.delete_selection();
                        }
                    }
                }
                egui::Event::Paste(t) => self.paste(&t, at),
                egui::Event::Key { key, pressed: true, modifiers, .. } => {
                    let cmd = modifiers.command;
                    match key {
                        Key::Delete | Key::Backspace => self.delete_selection(),
                        Key::Z if cmd && !modifiers.shift => self.restore(true),
                        Key::Z if cmd && modifiers.shift => self.restore(false),
                        Key::Y if cmd => self.restore(false),
                        Key::A if cmd => self.sel = self.canvas.nodes.iter().map(|n| n.id.clone()).collect(),
                        Key::G if cmd => self.group_selection(),
                        Key::D if cmd => {
                            if let Some(t) = self.copy_text() {
                                let b = self
                                    .selected_indices()
                                    .iter()
                                    .map(|i| self.canvas.nodes[*i].rect())
                                    .reduce(geom::Rect::union)
                                    .unwrap_or_default();
                                self.paste(&t, (b.x + 30.0, b.y + 30.0));
                            }
                        }
                        Key::Num0 if cmd => self.fit(rect),
                        Key::Escape => {
                            self.sel.clear();
                            self.sel_edge = None;
                            self.mode = Mode::Select;
                        }
                        Key::Enter if self.sel.len() == 1 => {
                            let id = self.sel[0].clone();
                            self.start_editing(&id);
                        }
                        Key::V if !cmd => self.mode = Mode::Select,
                        Key::H if !cmd => self.mode = Mode::Hand,
                        Key::P if !cmd => self.mode = Mode::Ink(InkTool::Draw(quire_canvas::ink::Tool::Pen)),
                        Key::E if !cmd => self.mode = Mode::Ink(InkTool::Eraser),
                        Key::ArrowLeft | Key::ArrowRight | Key::ArrowUp | Key::ArrowDown if !self.sel.is_empty() => {
                            let step = if mods.shift {
                                GRID * 5.0
                            } else if self.snap {
                                GRID
                            } else {
                                1.0
                            };
                            let (dx, dy) = match key {
                                Key::ArrowLeft => (-step, 0.0),
                                Key::ArrowRight => (step, 0.0),
                                Key::ArrowUp => (0.0, -step),
                                _ => (0.0, step),
                            };
                            self.checkpoint(&[]);
                            for id in self.sel.clone() {
                                if let Some(n) = self.canvas.node_mut(&id) {
                                    n.x += dx as i64;
                                    n.y += dy as i64;
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

    fn start_editing(&mut self, id: &str) {
        let Some(n) = self.canvas.node(id) else { return };
        self.editing = match n.node_kind() {
            NodeKind::Text => Some(Editing::Text { id: id.to_string(), buf: n.text.clone().unwrap_or_default(), fresh: false }),
            NodeKind::Group => Some(Editing::Label { id: id.to_string(), buf: n.label.clone().unwrap_or_default() }),
            _ => None,
        };
    }

    /// The text editor over the card being edited.
    fn editor(&mut self, ui: &mut egui::Ui, rect: Rect, pal: &Palette) {
        let Some(ed) = &mut self.editing else { return };
        let z = self.zoom;
        let (area, buf, multiline) = match ed {
            Editing::Text { id, buf, .. } => {
                let Some(n) = self.canvas.node(id) else { return };
                let r = Rect::from_min_size(
                    rect.min + (Vec2::new(n.x as f32, n.y as f32) - self.offset) * z,
                    Vec2::new(n.width as f32, n.height.max(60) as f32) * z,
                );
                (r.shrink((12.0 * z).clamp(3.0, 14.0)), buf, true)
            }
            Editing::Label { id, buf } => {
                let Some(n) = self.canvas.node(id) else { return };
                let top = rect.min + (Vec2::new(n.x as f32, n.y as f32) - self.offset) * z;
                (Rect::from_min_size(top - Vec2::new(0.0, 30.0 * z.max(0.6)), Vec2::new(260.0, 26.0)), buf, false)
            }
            Editing::EdgeLabel { id, buf } => {
                let Some(e) = self.canvas.edge(id) else { return };
                let Some(c) = geom::edge_curve(&self.canvas, e) else { return };
                let m = geom::bezier_point(&c, 0.5);
                let mid = rect.min + (Vec2::new(m.0, m.1) - self.offset) * z;
                (Rect::from_center_size(mid, Vec2::new(180.0, 24.0)), buf, false)
            }
        };
        let id = Id::unique(("quire-canvas-edit", self.path.as_str()));
        let font = FontId::proportional((14.0 * z).clamp(9.0, 40.0));
        ui.painter().rect_filled(area.expand(4.0), 4.0, pal.raised);
        ui.painter().rect_stroke(area.expand(4.0), 4.0, Stroke::new(1.0, pal.accent), StrokeKind::Outside);
        let te = if multiline { egui::TextEdit::multiline(buf) } else { egui::TextEdit::singleline(buf) };
        let r = ui.put(area, te.id(id).font(font).frame(egui::Frame::NONE).desired_width(area.width()).text_color(pal.text));
        if !r.has_focus() && !r.lost_focus() {
            r.request_focus();
        }
        let done = ui.input(|i| i.key_pressed(Key::Escape)) || (!multiline && ui.input(|i| i.key_pressed(Key::Enter))) || r.lost_focus();
        if done {
            self.finish_editing();
        }
    }

    // ------------------------------------------------------------ the frame

    fn canvas_area(&mut self, ui: &mut egui::Ui, ws: &mut Workspace, pal: &Palette) -> PanelOut {
        let mut out = PanelOut::default();
        let avail = ui.available_rect_before_wrap();
        let (rect, resp) = ui.allocate_exact_size(avail.size(), Sense::click_and_drag());
        let focus_id = resp.id;
        if !self.fitted {
            self.fit(rect);
        }
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 0.0, pal.bg);
        if self.grid {
            self.paint_grid(&painter, rect, pal);
        }
        let pointer = ui.input(|i| i.pointer.hover_pos()).filter(|p| rect.contains(*p));
        let space = ui.input(|i| i.key_down(Key::Space)) && self.editing.is_none();

        // View: scroll pans, Ctrl+scroll or a pinch zooms around the pointer.
        if let Some(p) = pointer
            && self.add.is_none()
        {
            let (scroll, zoom) = ui.input(|i| (i.smooth_scroll_delta, i.zoom_delta()));
            if (zoom - 1.0).abs() > 1e-4 {
                self.zoom_at(rect, p, zoom);
            } else if scroll != Vec2::ZERO && self.editing.is_none() {
                self.offset -= scroll / self.zoom;
            }
        }

        // What is in view.
        let view = {
            let a = self.to_canvas(rect, rect.min);
            let b = self.to_canvas(rect, rect.max);
            geom::Rect::from_corners(a, b).expand(40.0)
        };
        let visible: Vec<usize> = (0..self.canvas.nodes.len()).filter(|i| view.intersects(self.canvas.nodes[*i].rect())).collect();

        // Groups first (they're backgrounds), then edges, then cards.
        let (groups, cards): (Vec<usize>, Vec<usize>) = visible.iter().partition(|i| self.canvas.nodes[**i].kind == "group");
        for i in groups {
            self.paint_node(ui, &painter, ws, rect, i, pal);
        }
        for e in &self.canvas.edges {
            let (Some(a), Some(b)) = (self.canvas.node(&e.from_node), self.canvas.node(&e.to_node)) else { continue };
            if view.intersects(a.rect().union(b.rect())) {
                self.paint_edge(&painter, rect, e, pal);
            }
        }
        for i in cards {
            self.paint_node(ui, &painter, ws, rect, i, pal);
        }

        let canvas_p = pointer.map(|p| self.to_canvas(rect, p));
        let hovered_node = canvas_p.and_then(|p| geom::node_at(&self.canvas, p, false));

        // Side handles to start a connection, on the card under the pointer.
        if self.mode == Mode::Select
            && self.drag.is_none()
            && self.editing.is_none()
            && let Some(i) = hovered_node.or_else(|| self.sel.first().and_then(|id| self.canvas.node_index(id)))
            && self.canvas.nodes[i].kind != "group"
        {
            let r = self.canvas.nodes[i].rect();
            for s in Side::ALL {
                let p = self.to_screen(rect, s.anchor(r));
                painter.circle_filled(p, 5.0, pal.bg);
                painter.circle_stroke(p, 5.0, Stroke::new(1.5, pal.accent));
            }
        }

        // ---- pointer
        let mods = ui.input(|i| i.modifiers);
        if resp.clicked() || resp.drag_started() || resp.secondary_clicked() {
            resp.request_focus();
        }
        let middle = ui.input(|i| i.pointer.middle_down());
        if resp.drag_started()
            && let Some(p) = ui.input(|i| i.pointer.press_origin()).or_else(|| resp.interact_pointer_pos())
        {
            if self.editing.is_some() {
                self.finish_editing();
            }
            let cp = self.to_canvas(rect, p);
            self.drag = if middle || space || self.mode == Mode::Hand {
                Some(Drag::Pan)
            } else if let Mode::Ink(tool) = self.mode {
                self.begin_ink(ws, tool, cp, ink::pressure(ui))
            } else {
                Some(self.begin_select_drag(rect, p, cp, mods.shift))
            };
        }
        if resp.dragged()
            && let Some(p) = resp.interact_pointer_pos()
        {
            self.continue_drag(ui, ws, rect, p, resp.drag_delta());
        }
        if resp.drag_stopped() {
            self.end_drag(ws, rect);
        }
        if resp.double_clicked()
            && let Some(cp) = canvas_p
            && self.mode == Mode::Select
        {
            // A group's label edits the label; its empty inside makes a card there.
            let hit = geom::node_at(&self.canvas, cp, false).filter(|i| {
                let n = &self.canvas.nodes[*i];
                n.kind != "group" || !n.rect().contains(cp)
            });
            match hit {
                Some(i) => {
                    let id = self.canvas.nodes[i].id.clone();
                    match self.canvas.nodes[i].node_kind() {
                        NodeKind::Text | NodeKind::Group => self.start_editing(&id),
                        _ => out.jumped |= self.open_card(ws, ui.ctx(), &id),
                    }
                }
                None => match geom::edge_at(&self.canvas, cp, 8.0 / self.zoom) {
                    Some(e) => {
                        let id = self.canvas.edges[e].id.clone();
                        let buf = self.canvas.edges[e].label.clone().unwrap_or_default();
                        self.sel_edge = Some(id.clone());
                        self.editing = Some(Editing::EdgeLabel { id, buf });
                    }
                    None => self.add_text_card(cp),
                },
            }
        } else if resp.clicked()
            && let Some(cp) = canvas_p
            && self.mode == Mode::Select
        {
            self.finish_editing();
            if let Some(i) = geom::node_at(&self.canvas, cp, false) {
                let id = self.canvas.nodes[i].id.clone();
                self.sel_edge = None;
                if mods.shift || mods.command {
                    if let Some(k) = self.sel.iter().position(|s| *s == id) {
                        self.sel.remove(k);
                    } else {
                        self.sel.push(id);
                    }
                } else {
                    self.sel = vec![id];
                }
            } else {
                self.sel.clear();
                self.sel_edge = geom::edge_at(&self.canvas, cp, 8.0 / self.zoom).map(|e| self.canvas.edges[e].id.clone());
            }
        }
        if resp.secondary_clicked()
            && let Some(cp) = canvas_p
        {
            let node = geom::node_at(&self.canvas, cp, true).map(|i| self.canvas.nodes[i].id.clone());
            let edge = if node.is_none() {
                geom::edge_at(&self.canvas, cp, 8.0 / self.zoom).map(|e| self.canvas.edges[e].id.clone())
            } else {
                None
            };
            if let Some(n) = &node
                && !self.sel.contains(n)
            {
                self.sel = vec![n.clone()];
            }
            if edge.is_some() {
                self.sel_edge.clone_from(&edge);
            }
            self.menu_at = Some((cp, node, edge));
        }
        let mut menu_open = false;
        resp.context_menu(|ui| {
            menu_open = true;
            self.context_menu(ui, ws, &mut out);
        });
        if !menu_open {
            self.menu_at = None;
        }

        // ---- overlays on the canvas
        match &self.drag {
            Some(Drag::Marquee { start, .. }) => {
                if let Some(p) = pointer {
                    let a = self.to_screen(rect, *start);
                    let r = Rect::from_two_pos(a, p);
                    painter.rect_filled(r, 2.0, pal.selection.gamma_multiply(0.35));
                    painter.rect_stroke(r, 2.0, Stroke::new(1.0, pal.accent), StrokeKind::Inside);
                }
            }
            Some(Drag::Connect { from, side, to }) => {
                if let Some(n) = self.canvas.node(from) {
                    let target = geom::node_at(&self.canvas, *to, false).map(|i| self.canvas.nodes[i].rect());
                    let c = match target {
                        Some(t) => geom::curve(n.rect(), Some(*side), t, None),
                        None => geom::curve(n.rect(), Some(*side), geom::Rect::new(to.0, to.1, 0.0, 0.0), None),
                    };
                    let pts: Vec<Pos2> = geom::flatten(&c, 24).into_iter().map(|p| self.to_screen(rect, p)).collect();
                    painter.add(egui::Shape::line(pts, Stroke::new(2.0, pal.accent)));
                }
            }
            Some(Drag::Ink { node, gesture, .. }) if self.canvas.node(node).is_some_and(|n| n.width <= 1) => {
                // A new sketch's first stroke: draw in canvas units.
                let n = self.canvas.node(node).cloned();
                if let Some(n) = n {
                    gesture.paint(&painter, Place { origin: self.to_screen(rect, (n.x as f32, n.y as f32)), scale: self.zoom }, pal);
                }
            }
            _ => {}
        }
        if self.canvas.nodes.is_empty() && self.editing.is_none() {
            painter.text(
                rect.center(),
                Align2::CENTER_CENTER,
                "Double-click to add a card · drag from a card's side to connect · P to draw",
                FontId::proportional(15.0),
                pal.muted,
            );
        }
        // Cursor.
        if pointer.is_some() {
            let icon = match (&self.drag, self.mode) {
                (Some(Drag::Pan), _) => CursorIcon::Grabbing,
                (_, Mode::Hand) => CursorIcon::Grab,
                _ if space => CursorIcon::Grab,
                (_, Mode::Ink(_)) => CursorIcon::Crosshair,
                (Some(Drag::Move { .. }), _) => CursorIcon::Move,
                _ => {
                    let over_resize = self.sel.len() == 1
                        && self
                            .canvas
                            .node(&self.sel[0])
                            .is_some_and(|n| pointer.is_some_and(|p| self.screen_rect(rect, n.rect()).right_bottom().distance(p) < 9.0));
                    if over_resize { CursorIcon::ResizeNwSe } else { CursorIcon::Default }
                }
            };
            ui.ctx().set_cursor_icon(icon);
        }

        self.editor(ui, rect, pal);
        let focused = ui.memory(|m| m.has_focus(focus_id)) || (pointer.is_some() && ui.memory(|m| m.focused().is_none()));
        if focused && self.editing.is_none() && self.add.is_none() {
            self.handle_keys(ui, rect, pointer);
        }
        out
    }

    fn context_menu(&mut self, ui: &mut egui::Ui, ws: &mut Workspace, out: &mut PanelOut) {
        let Some((at, node, edge)) = self.menu_at.clone() else {
            ui.close();
            return;
        };
        if let Some(id) = node {
            let kind = self.canvas.node(&id).map(Node::node_kind);
            if matches!(kind, Some(NodeKind::Text | NodeKind::Group)) && ui.button("Edit").clicked() {
                self.start_editing(&id);
                ui.close();
            }
            if matches!(kind, Some(NodeKind::File | NodeKind::Link)) && ui.button("Open").clicked() {
                out.jumped |= self.open_card(ws, ui.ctx(), &id);
                ui.close();
            }
            if ui.button("Bring to front").clicked() {
                self.bring_forward(true);
                ui.close();
            }
            if ui.button("Send to back").clicked() {
                self.bring_forward(false);
                ui.close();
            }
            if self.sel.len() > 1 && ui.button("Group").clicked() {
                self.group_selection();
                ui.close();
            }
            if ui.button("Copy").clicked() {
                if let Some(t) = self.copy_text() {
                    ui.ctx().copy_text(t);
                }
                ui.close();
            }
            ui.separator();
            if ui.button("Delete").clicked() {
                self.delete_selection();
                ui.close();
            }
        } else if let Some(id) = edge {
            if ui.button("Edit label").clicked() {
                let buf = self.canvas.edge(&id).and_then(|e| e.label.clone()).unwrap_or_default();
                self.editing = Some(Editing::EdgeLabel { id: id.clone(), buf });
                ui.close();
            }
            if ui.button("Arrow at both ends").clicked() {
                self.checkpoint(&[]);
                if let Some(e) = self.canvas.edges.iter_mut().find(|e| e.id == id) {
                    e.from_end = Some("arrow".into());
                    e.to_end = Some("arrow".into());
                }
                ui.close();
            }
            if ui.button("No arrows").clicked() {
                self.checkpoint(&[]);
                if let Some(e) = self.canvas.edges.iter_mut().find(|e| e.id == id) {
                    e.from_end = Some("none".into());
                    e.to_end = Some("none".into());
                }
                ui.close();
            }
            ui.separator();
            if ui.button("Delete").clicked() {
                self.delete_selection();
                ui.close();
            }
        } else {
            if ui.button("Add card here").clicked() {
                self.add_text_card(at);
                ui.close();
            }
            if ui.button("Add note or file…").clicked() {
                self.add = Some(AddPopup { at, ..AddPopup::default() });
                ui.close();
            }
            if ui.button("Select all").clicked() {
                self.sel = self.canvas.nodes.iter().map(|n| n.id.clone()).collect();
                ui.close();
            }
        }
    }

    fn begin_select_drag(&mut self, rect: Rect, p: Pos2, cp: (f32, f32), shift: bool) -> Drag {
        // Resize handle of the one selected card.
        if self.sel.len() == 1
            && let Some(n) = self.canvas.node(&self.sel[0])
            && self.screen_rect(rect, n.rect()).right_bottom().distance(p) < 9.0
        {
            let aspect = Self::is_sketch(n).then(|| n.height as f32 / n.width.max(1) as f32);
            let d = Drag::Resize { id: n.id.clone(), start: n.rect(), aspect };
            self.checkpoint(&[]);
            return d;
        }
        // A side handle: connect.
        let candidates: Vec<usize> =
            geom::node_at(&self.canvas, cp, false).into_iter().chain(self.sel.first().and_then(|id| self.canvas.node_index(id))).collect();
        for i in candidates {
            let n = &self.canvas.nodes[i];
            if n.kind != "group"
                && let Some(side) = self.side_handle_at(rect, n, p)
            {
                return Drag::Connect { from: n.id.clone(), side, to: cp };
            }
        }
        if let Some(i) = geom::node_at(&self.canvas, cp, false) {
            let id = self.canvas.nodes[i].id.clone();
            if !self.sel.contains(&id) {
                if shift {
                    self.sel.push(id);
                } else {
                    self.sel = vec![id];
                }
            }
            self.sel_edge = None;
            // Moving a group moves what is inside it.
            let mut ids: Vec<String> = self.sel.clone();
            for g in self.selected_indices() {
                if self.canvas.nodes[g].kind == "group" {
                    for m in geom::group_members(&self.canvas, g) {
                        let mid = self.canvas.nodes[m].id.clone();
                        if !ids.contains(&mid) {
                            ids.push(mid);
                        }
                    }
                }
            }
            let origins = ids.iter().filter_map(|id| self.canvas.node(id).map(|n| (id.clone(), n.x, n.y))).collect();
            Drag::Move { start: cp, origins, moved: false }
        } else {
            if !shift {
                self.sel.clear();
            }
            self.sel_edge = None;
            Drag::Marquee { start: cp, additive: shift }
        }
    }

    fn begin_ink(&mut self, ws: &Workspace, tool: InkTool, cp: (f32, f32), pressure: f32) -> Option<Drag> {
        // The sketch under the pointer, else the one drawn in last if near, else a new one.
        let under = self
            .canvas
            .nodes
            .iter()
            .rev()
            .find(|n| Self::is_sketch(n) && n.rect().expand(8.0).contains(cp))
            .map(|n| (n.id.clone(), n.file.clone().unwrap_or_default()));
        let near_active = self
            .active_sketch
            .as_ref()
            .and_then(|id| self.canvas.node(id))
            .filter(|n| n.rect().expand(240.0).contains(cp))
            .map(|n| (n.id.clone(), n.file.clone().unwrap_or_default()));
        let target = under.or(near_active);
        if tool == InkTool::Eraser {
            // The eraser works on whatever sketch it touches.
            let (id, file) = target?;
            let path = NotePath::new(&file);
            self.sketch_for(ws, &file)?;
            self.checkpoint(std::slice::from_ref(&path));
            let scale = {
                let n = self.canvas.node(&id)?;
                Self::sketch_scale(n, &self.sketches.get(&path)?.sketch)
            };
            let g = Gesture::new(tool, &self.ink, self.zoom * scale);
            return Some(Drag::Ink { node: id, sketch: path, gesture: g });
        }
        let (id, path) = if let Some((id, file)) = target {
            self.sketch_for(ws, &file)?;
            let path = NotePath::new(&file);
            self.checkpoint(std::slice::from_ref(&path));
            (id, path)
        } else {
            self.checkpoint(&[]);
            let (id, path) = self.new_sketch(cp)?;
            // Undo of a new sketch removes its card; the file stays empty until saved.
            (id, path)
        };
        self.active_sketch = Some(id.clone());
        let n = self.canvas.node(&id)?.clone();
        let sk = &self.sketches.get(&path)?.sketch;
        let scale = if n.width <= 1 { 1.0 } else { Self::sketch_scale(&n, sk) };
        let mut g = Gesture::new(tool, &self.ink, self.zoom * scale);
        g.push(((cp.0 - n.x as f32) / scale, (cp.1 - n.y as f32) / scale), pressure);
        Some(Drag::Ink { node: id, sketch: path, gesture: g })
    }

    fn continue_drag(&mut self, ui: &egui::Ui, ws: &Workspace, rect: Rect, p: Pos2, delta: Vec2) {
        let cp = self.to_canvas(rect, p);
        let snap = self.snap;
        match self.drag.take() {
            Some(Drag::Pan) => {
                self.offset -= delta / self.zoom;
                self.drag = Some(Drag::Pan);
            }
            Some(Drag::Move { start, origins, moved }) => {
                if !moved {
                    self.checkpoint(&[]);
                }
                let (dx, dy) = (cp.0 - start.0, cp.1 - start.1);
                // Snap the first card's corner; the others keep their offsets.
                let (sdx, sdy) = origins.first().map_or((dx, dy), |(_, x, y)| {
                    let nx = if snap { geom::snap(*x as f32 + dx, GRID) } else { *x as f32 + dx };
                    let ny = if snap { geom::snap(*y as f32 + dy, GRID) } else { *y as f32 + dy };
                    (nx - *x as f32, ny - *y as f32)
                });
                for (id, x, y) in &origins {
                    if let Some(n) = self.canvas.node_mut(id) {
                        n.x = x + sdx.round() as i64;
                        n.y = y + sdy.round() as i64;
                    }
                }
                self.touch();
                self.drag = Some(Drag::Move { start, origins, moved: true });
            }
            Some(Drag::Resize { id, start, aspect }) => {
                let mut w = (cp.0 - start.x).max(40.0);
                let mut h = (cp.1 - start.y).max(30.0);
                if snap {
                    w = geom::snap(start.x + w, GRID) - start.x;
                    h = geom::snap(start.y + h, GRID) - start.y;
                }
                if let Some(a) = aspect {
                    h = w * a;
                }
                if let Some(n) = self.canvas.node_mut(&id) {
                    n.set_rect(geom::Rect::new(start.x, start.y, w.max(20.0), h.max(20.0)));
                }
                self.galleys.remove(&id);
                self.touch();
                self.drag = Some(Drag::Resize { id, start, aspect });
            }
            Some(Drag::Marquee { start, additive }) => {
                let hits = geom::nodes_in(&self.canvas, geom::Rect::from_corners(start, cp));
                let ids: Vec<String> = hits.into_iter().map(|i| self.canvas.nodes[i].id.clone()).collect();
                if additive {
                    for id in ids {
                        if !self.sel.contains(&id) {
                            self.sel.push(id);
                        }
                    }
                } else {
                    self.sel = ids;
                }
                self.drag = Some(Drag::Marquee { start, additive });
            }
            Some(Drag::Connect { from, side, .. }) => self.drag = Some(Drag::Connect { from, side, to: cp }),
            Some(Drag::Ink { node, sketch, mut gesture }) => {
                if let Some(n) = self.canvas.node(&node).cloned()
                    && let Some(e) = self.sketches.get_mut(&sketch)
                {
                    let scale = if n.width <= 1 { 1.0 } else { Self::sketch_scale(&n, &e.sketch) };
                    let sp = ((cp.0 - n.x as f32) / scale, (cp.1 - n.y as f32) / scale);
                    gesture.push(sp, ink::pressure(ui));
                    if gesture.tool == InkTool::Eraser && ink::erase(&mut e.sketch, sp, gesture.size / 2.0) {
                        e.dirty = true;
                        self.dirty = true;
                        self.last_edit = Instant::now();
                    }
                }
                let _ = ws;
                self.drag = Some(Drag::Ink { node, sketch, gesture });
            }
            None => {}
        }
    }

    fn end_drag(&mut self, ws: &mut Workspace, rect: Rect) {
        match self.drag.take() {
            Some(Drag::Connect { from, side, to }) => {
                if let Some(i) = geom::node_at(&self.canvas, to, false).filter(|i| self.canvas.nodes[*i].id != from) {
                    let target = self.canvas.nodes[i].clone();
                    // The side of the target nearest the pointer.
                    let to_side = Side::ALL
                        .into_iter()
                        .min_by(|a, b| {
                            let d = |s: Side| {
                                let q = s.anchor(target.rect());
                                (q.0 - to.0).hypot(q.1 - to.1)
                            };
                            d(*a).total_cmp(&d(*b))
                        })
                        .unwrap_or(Side::Left);
                    self.checkpoint(&[]);
                    let id = self.new_id();
                    let mut e = Edge::new(id, from, target.id);
                    e.from_side = Some(side.name().into());
                    e.to_side = Some(to_side.name().into());
                    self.canvas.edges.push(e);
                } else if geom::node_at(&self.canvas, to, false).is_none() {
                    // Dropped on empty canvas: a new card there, connected.
                    self.checkpoint(&[]);
                    let id = self.new_id();
                    let mut n = Node::new(&id, NodeKind::Text, (to.0 - 125.0) as i64, (to.1 - 30.0) as i64, 250, 60);
                    n.text = Some(String::new());
                    self.canvas.nodes.push(n);
                    let eid = self.new_id();
                    let mut e = Edge::new(eid, from, id.clone());
                    e.from_side = Some(side.name().into());
                    self.canvas.edges.push(e);
                    self.sel = vec![id.clone()];
                    self.editing = Some(Editing::Text { id, buf: String::new(), fresh: false });
                }
            }
            Some(Drag::Ink { node, sketch, gesture }) => {
                let scale = self
                    .canvas
                    .node(&node)
                    .and_then(|n| self.sketches.get(&sketch).map(|e| if n.width <= 1 { 1.0 } else { Self::sketch_scale(n, &e.sketch) }))
                    .unwrap_or(1.0);
                if let Some(item) = gesture.finish(0.5 / (self.zoom * scale)) {
                    if let Some(e) = self.sketches.get_mut(&sketch) {
                        e.sketch.items.push(item);
                        e.dirty = true;
                    }
                    self.fit_sketch(&node, &sketch);
                    self.touch();
                } else if gesture.tool != InkTool::Eraser {
                    // Nothing drawn: drop an empty new sketch again.
                    if self.sketches.get(&sketch).is_some_and(|e| e.sketch.items.is_empty()) {
                        self.canvas.remove_node(&node);
                        self.sketches.remove(&sketch);
                        self.undo.pop();
                        self.active_sketch = None;
                    }
                } else {
                    self.fit_sketch(&node, &sketch);
                }
            }
            Some(Drag::Pan | Drag::Marquee { .. } | Drag::Move { .. } | Drag::Resize { .. }) | None => {}
        }
        let _ = (ws, rect);
    }
}

impl Drop for CanvasSurface {
    fn drop(&mut self) {
        self.finish_editing();
        if self.dirty || self.sketches.values().any(|e| e.dirty) {
            self.save();
        }
    }
}

impl Surface for CanvasSurface {
    fn path(&self) -> &NotePath {
        &self.path
    }

    fn show(&mut self, ui: &mut egui::Ui, ws: &mut Workspace, pal: &Palette) -> PanelOut {
        let t0 = Instant::now();
        if self.ctx.is_none() {
            self.ctx = Some(ui.ctx().clone());
        }
        if !self.loaded {
            self.load(ws);
        } else if ws.features.canvas.disk_generation(&self.path) != self.disk_gen {
            self.absorb(ws);
        }
        if (self.dirty || self.sketches.values().any(|e| e.dirty)) && self.drag.is_none() && self.editing.is_none() {
            if self.last_edit.elapsed() >= SAVE_AFTER {
                self.save();
            } else {
                ui.ctx().request_repaint_after(SAVE_AFTER);
            }
        }
        if let Some(e) = &self.error {
            let mut out = PanelOut::default();
            ui.vertical_centered(|ui| {
                ui.add_space(ui.available_height() * 0.3);
                ui.label(RichText::new(self.path.file_name()).size(20.0).color(pal.strong));
                ui.label(RichText::new(format!("This canvas can't be read: {e}")).color(pal.muted));
                ui.label(RichText::new("Quire won't change the file. Open it as text to fix it.").color(pal.muted));
                if ui.button("Open as text").clicked() {
                    out.jumped = ws.open_note(&self.path).is_some();
                }
            });
            return out;
        }
        let rect_hint = ui.available_rect_before_wrap();
        let canvas_rect = Rect::from_min_max(rect_hint.min + Vec2::new(0.0, 36.0), rect_hint.max);
        egui::Frame::NONE.fill(pal.panel).inner_margin(egui::Margin::symmetric(10, 5)).show(ui, |ui| {
            self.toolbar(ui, Some(canvas_rect), pal);
        });
        let out = self.canvas_area(ui, ws, pal);
        self.add_popup(ui.ctx(), ws, pal);
        self.last_frame_ms = t0.elapsed().as_secs_f32() * 1000.0;
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn surface_with(ws: &Workspace, path: &str, text: &str) -> CanvasSurface {
        let v = ws.vault().unwrap().clone();
        v.storage().write_atomic(&NotePath::new(path), text).unwrap();
        CanvasSurface::new(&NotePath::new(path))
    }

    fn frame(ctx: &egui::Context, s: &mut CanvasSurface, ws: &mut Workspace, input: egui::RawInput) {
        let mut out = ctx.run_ui(input, |ui| {
            s.show(ui, ws, &crate::theme::LIGHT);
        });
        out.textures_delta.clear();
    }

    fn raw() -> egui::RawInput {
        egui::RawInput { screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(1200.0, 800.0))), ..Default::default() }
    }

    #[test]
    fn edits_save_in_canonical_form_and_keep_unknown_keys() {
        let mut ws = Workspace::new(quire_app::Settings::default());
        ws.attach_vault(Vault::in_memory(&[]));
        let src = "{\n\t\"nodes\":[\n\t\t{\"id\":\"a\",\"type\":\"text\",\"text\":\"Hi\",\"x\":0,\"y\":0,\"width\":250,\"height\":60,\"styleAttributes\":{\"shape\":\"pill\"}}\n\t],\n\t\"edges\":[]\n}";
        let mut s = surface_with(&ws, "B.canvas", src);
        let ctx = egui::Context::default();
        crate::theme::install_fonts(&ctx);
        frame(&ctx, &mut s, &mut ws, raw());
        assert_eq!(s.canvas.nodes.len(), 1);
        // Opening and looking never rewrites the file.
        s.save();
        assert_eq!(ws.vault().unwrap().read(&"B.canvas".into()).unwrap(), src);
        s.add_text_card((300.0, 0.0));
        if let Some(Editing::Text { buf, .. }) = &mut s.editing {
            buf.push_str("# New card");
        }
        s.finish_editing();
        s.sel = vec!["a".into()];
        s.set_color(Some("4".into()));
        s.save();
        let saved = ws.vault().unwrap().read(&"B.canvas".into()).unwrap();
        assert_eq!(saved.lines().count(), 5, "{saved}");
        assert!(saved.contains("\"styleAttributes\":{\"shape\":\"pill\"}"), "unknown keys kept: {saved}");
        assert!(saved.contains("# New card"));
        s.restore(true);
        assert_eq!(s.canvas.node("a").and_then(|n| n.color.clone()), None, "undo");
        s.restore(false);
        assert_eq!(s.canvas.node("a").and_then(|n| n.color.clone()).as_deref(), Some("4"), "redo");
    }

    #[test]
    fn copy_paste_group_align_and_outside_changes() {
        let mut ws = Workspace::new(quire_app::Settings::default());
        ws.attach_vault(Vault::in_memory(&[]));
        let mut s = surface_with(&ws, "C.canvas", "");
        let ctx = egui::Context::default();
        crate::theme::install_fonts(&ctx);
        frame(&ctx, &mut s, &mut ws, raw());
        s.paste("first card", (0.0, 0.0));
        s.paste("https://example.com", (300.0, 40.0));
        assert_eq!(s.canvas.nodes[1].node_kind(), NodeKind::Link);
        s.sel = s.canvas.nodes.iter().map(|n| n.id.clone()).collect();
        s.align(Some(Align::Top), None);
        assert_eq!(s.canvas.nodes[0].y, s.canvas.nodes[1].y);
        let copied = s.copy_text().unwrap();
        s.paste(&copied, (0.0, 500.0));
        assert_eq!(s.canvas.nodes.len(), 4);
        assert!(s.canvas.nodes[2].id != s.canvas.nodes[0].id, "pasted cards get new ids");
        s.group_selection();
        assert_eq!(s.canvas.nodes.iter().filter(|n| n.kind == "group").count(), 1);
        s.finish_editing();
        s.save();
        // A change made elsewhere while there are no unsaved edits: taken in.
        let mut other = quire_canvas::parse(&ws.vault().unwrap().read(&"C.canvas".into()).unwrap()).unwrap();
        other.nodes[0].text = Some("changed elsewhere".into());
        ws.vault().unwrap().storage().write_atomic(&"C.canvas".into(), &quire_canvas::to_canonical(&other)).unwrap();
        ws.features.canvas.disk_changes.insert("C.canvas".into(), 1);
        frame(&ctx, &mut s, &mut ws, raw());
        assert!(s.canvas.nodes.iter().any(|n| n.text.as_deref() == Some("changed elsewhere")));
        // With unsaved edits: merged node by node.
        let last = s.canvas.nodes.last().unwrap().id.clone();
        s.canvas.node_mut(&last).unwrap().x += 100;
        s.touch();
        let mut other2 = other.clone();
        other2.nodes.retain(|n| n.kind != "group");
        other2.nodes[1].url = Some("https://quire.app".into());
        ws.vault().unwrap().storage().write_atomic(&"C.canvas".into(), &quire_canvas::to_canonical(&other2)).unwrap();
        ws.features.canvas.disk_changes.insert("C.canvas".into(), 2);
        s.absorb(&mut ws);
        assert!(s.canvas.nodes.iter().any(|n| n.url.as_deref() == Some("https://quire.app")), "their change");
        // A version Quire can't read, while there are unsaved edits: kept beside
        // ours before ours is saved over it.
        let n = s.canvas.nodes[0].id.clone();
        s.canvas.node_mut(&n).unwrap().x += 5;
        s.touch();
        let odd = "{\"nodes\":[{\"id\":\"z\",\"type\":\"text\",\"x\":\"left\"}],\"edges\":[]}";
        ws.vault().unwrap().storage().write_atomic(&"C.canvas".into(), odd).unwrap();
        ws.features.canvas.disk_changes.insert("C.canvas".into(), 3);
        s.absorb(&mut ws);
        s.save();
        assert_eq!(ws.vault().unwrap().storage().read(&"C (changed elsewhere).canvas".into()).unwrap(), odd);
    }

    #[test]
    fn drawing_makes_a_sketch_card_and_an_svg() {
        let mut ws = Workspace::new(quire_app::Settings::default());
        ws.attach_vault(Vault::in_memory(&[]));
        let mut s = surface_with(&ws, "Board.canvas", "");
        let ctx = egui::Context::default();
        crate::theme::install_fonts(&ctx);
        frame(&ctx, &mut s, &mut ws, raw());
        s.mode = Mode::Ink(InkTool::Draw(quire_canvas::ink::Tool::Pen));
        let mut d = s.begin_ink(&ws, InkTool::Draw(quire_canvas::ink::Tool::Pen), (100.0, 100.0), 0.5);
        if let Some(Drag::Ink { gesture, .. }) = &mut d {
            for i in 0..30 {
                gesture.push((i as f32 * 3.0, (i as f32 * 0.3).sin() * 20.0), 0.5);
            }
        }
        s.drag = d;
        s.end_drag(&mut ws, Rect::from_min_size(Pos2::ZERO, Vec2::splat(800.0)));
        let n = s.canvas.nodes.last().unwrap().clone();
        assert!(CanvasSurface::is_sketch(&n));
        assert!(n.width > 60 && n.height > 20, "{n:?}");
        s.save();
        let svg = ws.vault().unwrap().read(&NotePath::new("attachments/Board-ink-1.svg")).unwrap();
        assert!(svg.contains("data-quire-tool=\"pen\""), "{svg}");
        let saved = ws.vault().unwrap().read(&"Board.canvas".into()).unwrap();
        assert!(saved.contains("\"file\":\"attachments/Board-ink-1.svg\""));
    }

    /// §9: a 1,000-card canvas draws within budget (release: ≤ 8 ms; this
    /// dev build of our code is slower, so the bound here is loose).
    #[test]
    fn a_thousand_cards_draw_quickly() {
        let mut ws = Workspace::new(quire_app::Settings::default());
        ws.attach_vault(Vault::in_memory(&[]));
        let mut c = Canvas::default();
        for i in 0..1000i64 {
            let mut n = Node::new(format!("n{i}"), NodeKind::Text, (i % 40) * 300, (i / 40) * 200, 250, 120);
            n.text = Some(format!("# Card {i}\nSome **text** with a [[link]] and `code`.\n- one\n- two"));
            c.nodes.push(n);
            if i > 0 {
                c.edges.push(Edge::new(format!("e{i}"), format!("n{}", i - 1), format!("n{i}")));
            }
        }
        let mut s = surface_with(&ws, "Big.canvas", &quire_canvas::to_canonical(&c));
        let ctx = egui::Context::default();
        crate::theme::install_fonts(&ctx);
        frame(&ctx, &mut s, &mut ws, raw());
        let mut times = Vec::new();
        for zoom in [1.0, 0.5, 0.1] {
            s.zoom = zoom;
            frame(&ctx, &mut s, &mut ws, raw());
            for k in 0..5 {
                s.offset += Vec2::new(30.0 * k as f32, 10.0);
                let t = Instant::now();
                frame(&ctx, &mut s, &mut ws, raw());
                times.push(t.elapsed().as_secs_f64() * 1000.0);
            }
        }
        times.sort_by(f64::total_cmp);
        let median = times[times.len() / 2];
        eprintln!("1,000-card canvas: median {median:.1} ms a frame, worst {:.1} ms (dev build)", times[times.len() - 1]);
        assert!(median < 120.0, "median {median:.1} ms");
    }
}
