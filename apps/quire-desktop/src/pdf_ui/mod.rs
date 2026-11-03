// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The PDF surface (design §G.2).
//!
//! Full edition: pages are rendered on a worker thread at the zoom and
//! screen density, the 12 most recent page images are kept, and only pages
//! in view are drawn. A text layer gives selection, copy, find and links;
//! highlights (text or area) are kept in the PDF's annotation note, shown
//! in a side list, and found again when the PDF changes.
//!
//! Quire Lite has no page renderer: it shows each page's text with find,
//! the highlights from the annotation note, and "Open in system viewer".

use std::collections::{HashMap, HashSet, VecDeque};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender};
#[cfg(feature = "typeset")]
use egui::{Align2, FontId, Pos2, Rect};
use egui::{Color32, Key, RichText, Sense, Stroke, Vec2};
use quire_app::Workspace;
use quire_app::pdf_state;
use quire_app::vault::NotePath;
use quire_app::workspace::NoticeLevel;
use quire_pdf::annot::{self, PdfAnchor};
#[cfg(feature = "typeset")]
use quire_pdf::text::LinkTarget;
use quire_pdf::text::{PageLink, Quad};
use quire_pdf::{PageInfo, PageText, PdfDoc};

use crate::panels::PanelOut;
use crate::surfaces::Surface;
use crate::theme::Palette;

/// Page images kept (design §G.2).
const TEXTURES: usize = 12;
#[cfg_attr(not(feature = "typeset"), allow(dead_code, reason = "Lite shows text only, with no page view"))]
const GAP: f32 = 16.0;

enum Req {
    #[cfg_attr(not(feature = "typeset"), allow(dead_code, reason = "Lite renders no pages"))]
    Render {
        page: usize,
        key: u32,
    },
    #[cfg_attr(not(feature = "typeset"), allow(dead_code, reason = "Lite shows text only, with no page view"))]
    Text {
        page: usize,
    },
    AllText,
    Find(String),
    #[cfg_attr(not(feature = "typeset"), allow(dead_code, reason = "Lite has no area highlights"))]
    Crop {
        page: usize,
        rect: Quad,
    },
}

enum Resp {
    Opened {
        pages: Vec<PageInfo>,
        sha: String,
    },
    Failed(String),
    #[cfg_attr(not(feature = "typeset"), allow(dead_code, reason = "Lite renders no pages"))]
    Rendered {
        page: usize,
        key: u32,
        img: Result<egui::ColorImage, String>,
    },
    Text {
        page: usize,
        text: PageText,
        links: Vec<PageLink>,
    },
    AllText(Vec<PageText>),
    Found {
        query: String,
        hits: Vec<(usize, Vec<Quad>)>,
    },
    #[cfg_attr(not(feature = "typeset"), allow(dead_code, reason = "Lite has no area highlights"))]
    Cropped {
        page: usize,
        rect: Quad,
        png: Option<Vec<u8>>,
    },
}

fn worker(bytes: &[u8], rx: &Receiver<Req>, tx: &Sender<Resp>, ctx: &egui::Context) {
    let doc = match PdfDoc::open(bytes.to_vec()) {
        Ok(d) => d,
        Err(e) => {
            let _ = tx.send(Resp::Failed(e.to_string()));
            ctx.request_repaint();
            return;
        }
    };
    let pages: Vec<PageInfo> = (0..doc.page_count())
        .map(|i| doc.page_info(i).unwrap_or(PageInfo { width: 612.0, height: 792.0, to_view: [1.0, 0.0, 0.0, -1.0, 0.0, 792.0] }))
        .collect();
    let _ = tx.send(Resp::Opened { pages, sha: quire_pdf::sha256_hex(bytes) });
    ctx.request_repaint();
    let mut texts: HashMap<usize, PageText> = HashMap::new();
    let text = |doc: &PdfDoc, page: usize, texts: &mut HashMap<usize, PageText>| -> PageText {
        texts.entry(page).or_insert_with(|| doc.page_text(page).unwrap_or_default()).clone()
    };
    for req in rx {
        let resp = match req {
            #[cfg(feature = "typeset")]
            Req::Render { page, key } => {
                let img = quire_pdf::render_page(&doc, page, key as f32 / 100.0)
                    .map(|i| egui::ColorImage::from_rgba_unmultiplied([i.width as usize, i.height as usize], &i.rgba))
                    .map_err(|e| e.to_string());
                Resp::Rendered { page, key, img }
            }
            #[cfg(not(feature = "typeset"))]
            Req::Render { .. } | Req::Crop { .. } => continue,
            Req::Text { page } => Resp::Text { page, text: text(&doc, page, &mut texts), links: doc.page_links(page) },
            Req::AllText => Resp::AllText((0..doc.page_count()).map(|p| text(&doc, p, &mut texts)).collect()),
            Req::Find(query) => {
                let all: Vec<PageText> = (0..doc.page_count()).map(|p| text(&doc, p, &mut texts)).collect();
                let hits = quire_pdf::find::find(&all, &query).into_iter().map(|h| (h.page, all[h.page].quads(h.start..h.end))).collect();
                Resp::Found { query, hits }
            }
            #[cfg(feature = "typeset")]
            Req::Crop { page, rect } => Resp::Cropped { page, rect, png: crop(&doc, page, rect) },
        };
        if tx.send(resp).is_err() {
            return;
        }
        ctx.request_repaint();
    }
}

/// An area of a page as a PNG (twice the page's size in points).
#[cfg(feature = "typeset")]
fn crop(doc: &PdfDoc, page: usize, q: Quad) -> Option<Vec<u8>> {
    let info = doc.page_info(page).ok()?;
    let img = quire_pdf::render_page(doc, page, 2.0).ok()?;
    let [l, t, r, b] = info.view_rect(q);
    let (x0, y0) = ((l * 2.0).max(0.0) as u32, (t * 2.0).max(0.0) as u32);
    let (x1, y1) = (((r * 2.0) as u32).min(img.width), ((b * 2.0) as u32).min(img.height));
    if x1 <= x0 + 2 || y1 <= y0 + 2 {
        return None;
    }
    let full = image::RgbaImage::from_raw(img.width, img.height, img.rgba)?;
    let part = image::imageops::crop_imm(&full, x0, y0, x1 - x0, y1 - y0).to_image();
    let mut out = std::io::Cursor::new(Vec::new());
    part.write_to(&mut out, image::ImageFormat::Png).ok()?;
    Some(out.into_inner())
}

/// Page, start and end of an area highlight being drawn (PDF points).
type AreaDrag = (usize, (f32, f32), (f32, f32));

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tool {
    Text,
    Area,
}

/// Colours of highlights by name.
fn highlight_color(name: &str, pal: &Palette) -> Color32 {
    let (r, g, b) = match name {
        "green" => (120, 220, 120),
        "blue" => (120, 180, 255),
        "pink" => (255, 140, 190),
        "purple" => (190, 150, 255),
        _ => (255, 220, 60),
    };
    Color32::from_rgba_unmultiplied(r, g, b, if pal.dark { 90 } else { 110 })
}

/// The PDF surface.
pub(crate) struct PdfSurface {
    path: NotePath,
    tx: Option<Sender<Req>>,
    rx: Option<Receiver<Resp>>,
    error: Option<String>,
    pages: Vec<PageInfo>,
    sha: String,
    zoom: f32,
    fit_width: bool,
    textures: HashMap<(usize, u32), egui::TextureHandle>,
    lru: VecDeque<(usize, u32)>,
    pending: HashSet<(usize, u32)>,
    failed: HashSet<usize>,
    texts: HashMap<usize, (PageText, Vec<PageLink>)>,
    text_pending: HashSet<usize>,
    all_text: Option<Vec<PageText>>,
    /// Page, anchor char and head char of a text selection.
    #[cfg_attr(not(feature = "typeset"), allow(dead_code, reason = "Lite shows text only, with no page view"))]
    selection: Option<(usize, usize, usize)>,
    #[cfg_attr(not(feature = "typeset"), allow(dead_code, reason = "Lite shows text only, with no page view"))]
    selecting: bool,
    /// An area being drawn: page, start and end in PDF points.
    #[cfg_attr(not(feature = "typeset"), allow(dead_code, reason = "Lite shows text only, with no page view"))]
    area: Option<AreaDrag>,
    find_query: String,
    /// Frames left to ask for the focus for the find box.
    find_focus: u8,
    hits: Vec<(usize, Vec<Quad>)>,
    hit: usize,
    found_for: String,
    highlights: Vec<annot::Entry>,
    hl_generation: Option<u64>,
    detached: Vec<String>,
    chosen: Option<String>,
    comment: String,
    color: usize,
    tool: Tool,
    scroll_to: Option<(usize, f32)>,
    flash: Option<(String, Instant)>,
    current_page: usize,
    pending_jump: Option<pdf_state::Jump>,
    show_list: bool,
    /// Put the caret in the chosen highlight's comment box.
    focus_comment: bool,
}

impl PdfSurface {
    pub(crate) fn new(path: &NotePath) -> Self {
        Self {
            path: path.clone(),
            tx: None,
            rx: None,
            error: None,
            pages: Vec::new(),
            sha: String::new(),
            zoom: 1.0,
            fit_width: true,
            textures: HashMap::new(),
            lru: VecDeque::new(),
            pending: HashSet::new(),
            failed: HashSet::new(),
            texts: HashMap::new(),
            text_pending: HashSet::new(),
            all_text: None,
            selection: None,
            selecting: false,
            area: None,
            find_query: String::new(),
            find_focus: 0,
            hits: Vec::new(),
            hit: 0,
            found_for: String::new(),
            highlights: Vec::new(),
            hl_generation: None,
            detached: Vec::new(),
            chosen: None,
            comment: String::new(),
            color: 0,
            tool: Tool::Text,
            scroll_to: None,
            flash: None,
            current_page: 0,
            pending_jump: None,
            show_list: true,
            focus_comment: false,
        }
    }

    fn start(&mut self, ws: &Workspace, ctx: &egui::Context) {
        let bytes = match ws.vault().map(|v| v.storage().read_bytes(&self.path)) {
            Some(Ok(b)) => b,
            Some(Err(e)) => {
                self.error = Some(e.to_string());
                return;
            }
            None => return,
        };
        let (qtx, qrx) = crossbeam_channel::unbounded();
        let (rtx, rrx) = crossbeam_channel::unbounded();
        let c = ctx.clone();
        let spawned = std::thread::Builder::new().name("quire-pdf".into()).spawn(move || {
            // A panic in the renderer is caught inside quire-pdf; this is a last guard.
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| worker(&bytes, &qrx, &rtx, &c)));
        });
        if spawned.is_err() {
            self.error = Some("couldn't start reading it".into());
            return;
        }
        self.tx = Some(qtx);
        self.rx = Some(rrx);
    }

    fn send(&self, r: Req) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(r);
        }
    }

    fn poll(&mut self, ws: &mut Workspace, ctx: &egui::Context) {
        let Some(rx) = self.rx.clone() else { return };
        for resp in rx.try_iter() {
            match resp {
                Resp::Opened { pages, sha } => {
                    self.pages = pages;
                    self.sha = sha;
                    self.send(Req::AllText);
                }
                Resp::Failed(e) => self.error = Some(e),
                Resp::Rendered { page, key, img } => {
                    self.pending.remove(&(page, key));
                    match img {
                        Ok(img) => {
                            let t = ctx.load_texture(format!("quire-pdf-{}-{page}-{key}", self.path), img, egui::TextureOptions::LINEAR);
                            // Keep the newest page images only.
                            self.textures.retain(|(p, k), _| !(*p == page && *k != key));
                            self.lru.retain(|(p, _)| *p != page);
                            self.textures.insert((page, key), t);
                            self.lru.push_back((page, key));
                            while self.lru.len() > TEXTURES {
                                if let Some(old) = self.lru.pop_front() {
                                    self.textures.remove(&old);
                                }
                            }
                            self.failed.remove(&page);
                        }
                        Err(_) => {
                            self.failed.insert(page);
                        }
                    }
                }
                Resp::Text { page, text, links } => {
                    self.text_pending.remove(&page);
                    self.texts.insert(page, (text, links));
                }
                Resp::AllText(all) => {
                    if let Some(detached) = pdf_state::reanchor_all(ws, &self.path, &self.sha, &all)
                        && !detached.is_empty()
                    {
                        ws.notify(
                            NoticeLevel::Warn,
                            format!(
                                "{} changed: {} highlight{} couldn't be found again; see “Detached” in the list.",
                                self.path.file_name(),
                                detached.len(),
                                if detached.len() == 1 { "" } else { "s" }
                            ),
                        );
                        self.detached = detached;
                    }
                    self.all_text = Some(all);
                }
                Resp::Found { query, hits } => {
                    if query == self.find_query {
                        self.hits = hits;
                        self.hit = 0;
                        self.found_for = query;
                        if let Some((p, q)) = self.hits.first() {
                            let y = q.first().map_or(0.0, |q| self.pages.get(*p).map_or(0.0, |i| i.view_rect(*q)[1]));
                            self.scroll_to = Some((*p, y));
                        }
                    }
                }
                Resp::Cropped { page, rect, png } => self.finish_area(ws, page, rect, png.as_deref()),
            }
        }
    }

    #[cfg_attr(not(feature = "typeset"), allow(dead_code, reason = "Lite shows text only, with no page view"))]
    fn page_height(&self, i: usize) -> f32 {
        self.pages.get(i).map_or(0.0, |p| p.height * self.zoom)
    }

    /// Top of page `i` in the scroll area.
    #[cfg_attr(not(feature = "typeset"), allow(dead_code, reason = "Lite shows text only, with no page view"))]
    fn page_top(&self, i: usize) -> f32 {
        (0..i).map(|k| self.page_height(k) + GAP).sum::<f32>() + GAP
    }

    fn refresh_highlights(&mut self, ws: &Workspace) {
        let generation = ws.features.pdf.annotations_generation;
        if self.hl_generation != Some(generation) {
            self.hl_generation = Some(generation);
            self.highlights = pdf_state::highlights(ws, &self.path);
            if let Some(c) = &self.chosen {
                match self.highlights.iter().find(|h| h.id == *c) {
                    Some(h) => self.comment.clone_from(&h.comment),
                    None => self.chosen = None,
                }
            }
        }
    }

    fn take_jump(&mut self, ws: &mut Workspace) {
        if ws.features.pdf.jump.as_ref().is_some_and(|j| j.path == self.path) {
            self.pending_jump = ws.features.pdf.jump.take();
        }
        if self.pages.is_empty() {
            return;
        }
        let Some(j) = self.pending_jump.take() else { return };
        if let Some(id) = &j.annot
            && let Some(h) = self.highlights.iter().find(|h| h.id == *id)
        {
            let p = (h.anchor.p as usize).saturating_sub(1).min(self.pages.len() - 1);
            let y = h.anchor.q.first().map_or(0.0, |q| self.pages[p].view_rect(*q)[1]);
            self.scroll_to = Some((p, (y - 80.0).max(0.0)));
            self.flash = Some((id.clone(), Instant::now()));
            self.chosen = Some(id.clone());
            self.comment.clone_from(&h.comment);
            return;
        }
        if let Some(p) = j.page {
            self.scroll_to = Some(((p as usize).saturating_sub(1).min(self.pages.len() - 1), 0.0));
        }
    }

    #[cfg_attr(not(feature = "typeset"), allow(dead_code, reason = "Lite shows text only, with no page view"))]
    fn selected_text(&self) -> Option<(usize, std::ops::Range<usize>, String)> {
        let (page, a, h) = self.selection?;
        let (t, _) = self.texts.get(&page)?;
        let (s, e) = (a.min(h), a.max(h) + 1);
        let chars: Vec<char> = t.text().chars().collect();
        let e = e.min(chars.len());
        (s < e).then(|| (page, s..e, chars[s..e].iter().collect()))
    }

    #[cfg_attr(not(feature = "typeset"), allow(dead_code, reason = "Lite shows text only, with no page view"))]
    fn make_highlight(&mut self, ws: &mut Workspace, with_comment: bool) {
        let Some((page, range, _)) = self.selected_text() else { return };
        let Some((t, _)) = self.texts.get(&page) else { return };
        let a = quire_pdf::find::anchor_for(t, page, range, annot::COLORS[self.color]);
        match pdf_state::add_highlight(ws, &self.path, &a, &a.t, "") {
            Ok(id) => {
                self.selection = None;
                self.flash = Some((id.clone(), Instant::now()));
                if with_comment {
                    self.chosen = Some(id);
                    self.comment.clear();
                    self.show_list = true;
                    self.focus_comment = true;
                }
            }
            Err(e) => ws.notify(NoticeLevel::Error, format!("Couldn't save the highlight: {e}")),
        }
    }

    fn finish_area(&mut self, ws: &mut Workspace, page: usize, rect: Quad, png: Option<&[u8]>) {
        let a = PdfAnchor {
            p: u32::try_from(page + 1).unwrap_or(1),
            q: vec![rect],
            c: annot::COLORS[self.color].to_string(),
            ..PdfAnchor::default()
        };
        let quote = png
            .and_then(|b| ws.vault().and_then(|v| v.save_attachment(&format!("{}-p{}.png", self.path.stem(), page + 1), b).ok()))
            .map(|p| format!("![[{}]]", p.file_name()))
            .unwrap_or_default();
        match pdf_state::add_highlight(ws, &self.path, &a, &quote, "") {
            Ok(id) => {
                self.flash = Some((id.clone(), Instant::now()));
                self.chosen = Some(id);
                self.comment.clear();
            }
            Err(e) => ws.notify(NoticeLevel::Error, format!("Couldn't save the highlight: {e}")),
        }
    }

    fn toolbar(&mut self, ui: &mut egui::Ui, ws: &mut Workspace, pal: &Palette) -> PanelOut {
        let mut out = PanelOut::default();
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            ui.label(RichText::new(self.path.file_name()).color(pal.strong));
            if !self.pages.is_empty() {
                ui.label(RichText::new(format!("p. {} of {}", self.current_page + 1, self.pages.len())).color(pal.muted));
            }
            ui.separator();
            if quire_pdf::CAN_RENDER {
                if ui.small_button("−").on_hover_text("Zoom out").clicked() {
                    self.zoom = (self.zoom / 1.2).max(0.25);
                    self.fit_width = false;
                }
                ui.label(format!("{:.0} %", self.zoom * 100.0));
                if ui.small_button("+").on_hover_text("Zoom in").clicked() {
                    self.zoom = (self.zoom * 1.2).min(5.0);
                    self.fit_width = false;
                }
                ui.toggle_value(&mut self.fit_width, "Fit width");
                ui.separator();
            }
            let r = ui.add(egui::TextEdit::singleline(&mut self.find_query).hint_text("Find in PDF (Ctrl+F)").desired_width(150.0));
            if self.find_focus > 0 {
                r.request_focus();
                self.find_focus -= 1;
                ui.ctx().request_repaint();
            }
            let enter = r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
            if enter || (ui.input(|i| i.key_pressed(Key::F3)) && !self.find_query.is_empty()) {
                if self.find_query == self.found_for && !self.hits.is_empty() {
                    self.hit = (self.hit + 1) % self.hits.len();
                    self.scroll_to_hit();
                } else if !self.find_query.trim().is_empty() {
                    self.send(Req::Find(self.find_query.clone()));
                }
                if enter {
                    r.request_focus();
                }
            }
            if !self.found_for.is_empty() && self.found_for == self.find_query {
                ui.label(
                    RichText::new(if self.hits.is_empty() {
                        "No matches".into()
                    } else {
                        format!("{} of {}", self.hit + 1, self.hits.len())
                    })
                    .color(pal.muted),
                );
                if !self.hits.is_empty() {
                    if ui.small_button("▲").clicked() {
                        self.hit = (self.hit + self.hits.len() - 1) % self.hits.len();
                        self.scroll_to_hit();
                    }
                    if ui.small_button("▼").clicked() {
                        self.hit = (self.hit + 1) % self.hits.len();
                        self.scroll_to_hit();
                    }
                }
            }
            if quire_pdf::CAN_RENDER {
                ui.separator();
                for (i, name) in annot::COLORS.iter().enumerate() {
                    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(16.0), Sense::click());
                    ui.painter().circle_filled(rect.center(), 6.0, highlight_color(name, pal).to_opaque());
                    if self.color == i {
                        ui.painter().circle_stroke(rect.center(), 8.0, Stroke::new(1.5, pal.accent));
                    }
                    if resp.on_hover_text(format!("Highlight colour: {name}")).clicked() {
                        self.color = i;
                    }
                }
                if ui
                    .selectable_label(self.tool == Tool::Area, "□ Area")
                    .on_hover_text("Drag a box to highlight part of a page (figures, tables)")
                    .clicked()
                {
                    self.tool = if self.tool == Tool::Area { Tool::Text } else { Tool::Area };
                }
            }
            ui.separator();
            ui.toggle_value(&mut self.show_list, format!("Highlights · {}", self.highlights.len()));
            ui.menu_button("Open…", |ui| {
                let note = annot::note_for(&self.path);
                if ui
                    .add_enabled(ws.vault().is_some_and(|v| v.exists(&note)), egui::Button::new("The annotation note"))
                    .on_hover_text(note.as_str())
                    .clicked()
                {
                    out.jumped = ws.open_note(&note).is_some();
                    ui.close();
                }
                if let Some(abs) = ws.vault().and_then(|v| v.storage().absolute(&self.path))
                    && ui.button("In the system viewer").clicked()
                {
                    ui.ctx().open_url(egui::OpenUrl::new_tab(format!("file://{}", abs.display())));
                    ui.close();
                }
            });
        });
        out
    }

    fn scroll_to_hit(&mut self) {
        if let Some((p, q)) = self.hits.get(self.hit) {
            let y = q.first().map_or(0.0, |q| self.pages.get(*p).map_or(0.0, |i| i.view_rect(*q)[1]));
            self.scroll_to = Some((*p, (y - 80.0).max(0.0)));
        }
    }

    /// The side list: highlights, the chosen one's comment, detached ones.
    fn side_list(&mut self, ui: &mut egui::Ui, ws: &mut Workspace, pal: &Palette) -> PanelOut {
        let mut out = PanelOut::default();
        ui.label(RichText::new("HIGHLIGHTS").size(11.0).color(pal.muted).strong());
        if self.highlights.is_empty() {
            ui.label(
                RichText::new(if quire_pdf::CAN_RENDER {
                    "Select text on a page and choose Highlight. Highlights are kept in a note next to the PDF."
                } else {
                    "Highlights made in Quire (full edition) or written in the PDF's annotation note show here."
                })
                .color(pal.muted)
                .size(12.0),
            );
        }
        let mut action: Option<(String, bool)> = None;
        egui::ScrollArea::vertical().id_salt("pdf-highlights").auto_shrink([false, false]).show(ui, |ui| {
            for h in self.highlights.clone() {
                let chosen = self.chosen.as_deref() == Some(h.id.as_str());
                let detached = self.detached.contains(&h.id);
                let frame = egui::Frame::NONE
                    .fill(if chosen { pal.active_bg } else { pal.raised })
                    .corner_radius(6.0)
                    .inner_margin(egui::Margin::same(8))
                    .stroke(Stroke::new(1.0, pal.border));
                let resp = frame
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.horizontal(|ui| {
                            let (r, _) = ui.allocate_exact_size(Vec2::splat(10.0), Sense::hover());
                            ui.painter().circle_filled(r.center(), 5.0, highlight_color(&h.anchor.c, pal).to_opaque());
                            ui.label(RichText::new(format!("p. {}", h.anchor.p)).size(11.0).color(pal.muted));
                            if detached {
                                ui.label(RichText::new("detached").size(11.0).color(pal.warn))
                                    .on_hover_text("This text wasn't found in the changed PDF; the highlight keeps its old place.");
                            }
                        });
                        if h.anchor.is_area() {
                            ui.label(RichText::new("Area (its picture is in the note)").size(12.5).color(pal.muted));
                        } else {
                            let quote: String = h.quote.chars().take(220).collect();
                            ui.label(RichText::new(quote).size(12.5).color(pal.text));
                        }
                        if !h.comment.is_empty() && !chosen {
                            ui.label(RichText::new(&h.comment).size(12.0).color(pal.muted).italics());
                        }
                        if chosen {
                            let r = ui.add(
                                egui::TextEdit::multiline(&mut self.comment)
                                    .hint_text("Your comment")
                                    .desired_rows(2)
                                    .desired_width(f32::INFINITY),
                            );
                            if std::mem::take(&mut self.focus_comment) {
                                r.request_focus();
                            }
                            ui.horizontal(|ui| {
                                if ui.button("Save comment").clicked() {
                                    action = Some((h.id.clone(), true));
                                }
                                if ui.button("Remove").clicked() {
                                    action = Some((h.id.clone(), false));
                                }
                                if ui.small_button("In note").on_hover_text("Open the annotation note at this highlight").clicked() {
                                    let note = annot::note_for(&self.path);
                                    if ws.open_note(&note).is_some() {
                                        ws.jump_to_heading(&format!("^{}", h.id));
                                        out.jumped = true;
                                    }
                                }
                            });
                        }
                    })
                    .response;
                // Only a card that isn't open is clickable as a whole, so the
                // open card's buttons and comment field get their clicks.
                if !chosen && resp.interact(Sense::click()).clicked() {
                    self.chosen = Some(h.id.clone());
                    self.comment.clone_from(&h.comment);
                    if !self.pages.is_empty() {
                        let p = (h.anchor.p as usize).saturating_sub(1).min(self.pages.len() - 1);
                        let y = h.anchor.q.first().map_or(0.0, |q| self.pages[p].view_rect(*q)[1]);
                        self.scroll_to = Some((p, (y - 80.0).max(0.0)));
                        self.flash = Some((h.id.clone(), Instant::now()));
                    }
                }
                ui.add_space(6.0);
            }
        });
        if let Some((id, save)) = action {
            let r = if save {
                pdf_state::set_comment(ws, &self.path, &id, &self.comment)
            } else {
                pdf_state::remove_highlight(ws, &self.path, &id)
            };
            if let Err(e) = r {
                ws.notify(NoticeLevel::Error, e);
            }
            if !save {
                self.chosen = None;
            }
        }
        out
    }

    /// The pages (full edition).
    #[cfg(feature = "typeset")]
    fn pages_view(&mut self, ui: &mut egui::Ui, ws: &mut Workspace, pal: &Palette) {
        let avail = ui.available_width();
        if self.fit_width
            && let Some(maxw) = self.pages.iter().map(|p| p.width).reduce(f32::max)
        {
            self.zoom = ((avail - 2.0 * GAP) / maxw).clamp(0.25, 5.0);
        }
        let ppp = ui.ctx().pixels_per_point();
        // Render at the zoom on screen, in steps of 0.25 so images are reused.
        let key = ((self.zoom * ppp * 4.0).round().clamp(1.0, 24.0) * 25.0) as u32;
        let total_h = self.page_top(self.pages.len());
        let total_w = self.pages.iter().map(|p| p.width * self.zoom).fold(0.0, f32::max) + 2.0 * GAP;
        let mut area = egui::ScrollArea::both().id_salt(("pdf-pages", self.path.as_str())).auto_shrink([false, false]);
        if let Some((p, y)) = self.scroll_to.take() {
            area = area.vertical_scroll_offset(self.page_top(p) + y * self.zoom - GAP);
        }
        let ctrl_scroll = ui.input(egui::InputState::zoom_delta);
        if (ctrl_scroll - 1.0).abs() > 1e-4 && ui.rect_contains_pointer(ui.max_rect()) {
            self.zoom = (self.zoom * ctrl_scroll).clamp(0.25, 5.0);
            self.fit_width = false;
        }
        area.show_viewport(ui, |ui, viewport| {
            let (full, resp) = ui.allocate_exact_size(Vec2::new(total_w.max(avail), total_h), Sense::click_and_drag());
            let painter = ui.painter_at(full.intersect(ui.clip_rect()));
            let pointer = ui.input(|i| i.pointer.hover_pos());
            let mut y = GAP;
            let flash = self
                .flash
                .as_ref()
                .filter(|(_, t)| t.elapsed() < Duration::from_millis(1500))
                .map(|(id, t)| (id.clone(), t.elapsed().as_secs_f32()));
            if flash.is_some() {
                ui.ctx().request_repaint();
            }
            let mut current = None;
            for i in 0..self.pages.len() {
                let info = self.pages[i];
                let (w, h) = (info.width * self.zoom, info.height * self.zoom);
                let page_rect_local = Rect::from_min_size(Pos2::new((full.width() - w) / 2.0, y), Vec2::new(w, h));
                y += h + GAP;
                if !page_rect_local.intersects(viewport.expand(200.0)) {
                    continue;
                }
                if current.is_none() && page_rect_local.bottom() > viewport.top() + viewport.height() * 0.3 {
                    current = Some(i);
                }
                let r = page_rect_local.translate(full.min.to_vec2());
                painter.rect_filled(r.expand(1.0), 2.0, pal.border);
                painter.rect_filled(r, 0.0, Color32::WHITE);
                match self.textures.get(&(i, key)).or_else(|| self.textures.iter().find(|((p, _), _)| *p == i).map(|(_, t)| t)) {
                    Some(t) => {
                        painter.image(t.id(), r, Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)), Color32::WHITE);
                    }
                    None if self.failed.contains(&i) => {
                        painter.text(
                            r.center(),
                            Align2::CENTER_CENTER,
                            "Can't render this page",
                            FontId::proportional(15.0),
                            Color32::from_rgb(120, 120, 120),
                        );
                    }
                    None => {
                        painter.text(r.center(), Align2::CENTER_CENTER, "…", FontId::proportional(20.0), Color32::from_rgb(160, 160, 160));
                    }
                }
                if !self.textures.contains_key(&(i, key)) && !self.failed.contains(&i) && self.pending.insert((i, key)) {
                    self.send(Req::Render { page: i, key });
                }
                if !self.texts.contains_key(&i) && self.text_pending.insert(i) {
                    self.send(Req::Text { page: i });
                }
                let to_screen = |q: Quad| {
                    let [l, t, rr, b] = info.view_rect(q);
                    Rect::from_min_max(r.min + Vec2::new(l, t) * self.zoom, r.min + Vec2::new(rr, b) * self.zoom)
                };
                // Highlights.
                for hl in &self.highlights {
                    if hl.anchor.p as usize != i + 1 {
                        continue;
                    }
                    let mut c = highlight_color(&hl.anchor.c, pal);
                    if let Some((id, t)) = &flash
                        && *id == hl.id
                    {
                        let k = 0.5 + 0.5 * (t * 8.0).sin().abs();
                        c = Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), (255.0 * k) as u8);
                    }
                    for q in &hl.anchor.q {
                        let rr = to_screen(*q);
                        if hl.anchor.is_area() {
                            painter.rect_stroke(rr, 2.0, Stroke::new(2.0, c.to_opaque()), egui::StrokeKind::Outside);
                            painter.rect_filled(rr, 2.0, c.gamma_multiply(0.3));
                        } else {
                            painter.rect_filled(rr, 1.0, c);
                        }
                    }
                }
                // Find hits.
                for (k, (p, quads)) in self.hits.iter().enumerate() {
                    if *p == i {
                        let c = if k == self.hit {
                            Color32::from_rgba_unmultiplied(255, 140, 0, 120)
                        } else {
                            Color32::from_rgba_unmultiplied(255, 200, 0, 70)
                        };
                        for q in quads {
                            painter.rect_filled(to_screen(*q), 1.0, c);
                        }
                    }
                }
                // Selection.
                if let Some((sp, a, b)) = self.selection
                    && sp == i
                    && let Some((t, _)) = self.texts.get(&i)
                {
                    for q in t.quads(a.min(b)..a.max(b) + 1) {
                        painter.rect_filled(to_screen(q), 1.0, pal.selection.gamma_multiply(0.7));
                    }
                }
                // Links: a hand and the address.
                if let (Some(p), Some((_, links))) = (pointer, self.texts.get(&i))
                    && r.contains(p)
                {
                    // One tip, for the link drawn last, when links overlap.
                    if let Some(l) = links.iter().rev().find(|l| to_screen(l.quad).contains(p)) {
                        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                        let tip = match &l.target {
                            LinkTarget::Uri(u) => u.clone(),
                            LinkTarget::Page(n) => format!("Go to page {}", n + 1),
                        };
                        let _ = resp.clone().on_hover_text_at_pointer(tip);
                    }
                }
                // Pointer: select text, draw an area, click links and highlights.
                if let Some(p) = resp.interact_pointer_pos().filter(|p| r.contains(*p)) {
                    let pdf = {
                        let v = (p - r.min) / self.zoom;
                        info.pdf_point(v.x, v.y)
                    };
                    if resp.drag_started() {
                        // Start where the button went down, not where the drag was noticed.
                        let start = ui.input(|inp| inp.pointer.press_origin()).filter(|o| r.contains(*o)).map_or(pdf, |o| {
                            let v = (o - r.min) / self.zoom;
                            info.pdf_point(v.x, v.y)
                        });
                        match self.tool {
                            Tool::Text => {
                                if let Some(c) = self.texts.get(&i).and_then(|(t, _)| char_at(t, start)) {
                                    self.selection = Some((i, c, c));
                                    self.selecting = true;
                                }
                            }
                            Tool::Area => self.area = Some((i, start, pdf)),
                        }
                    }
                    if resp.dragged() {
                        if let (Some((sp, a, _)), true) = (self.selection, self.selecting)
                            && sp == i
                            && let Some(c) = self.texts.get(&i).and_then(|(t, _)| char_at(t, pdf))
                        {
                            self.selection = Some((i, a, c));
                        }
                        if let Some((ap, a, _)) = self.area
                            && ap == i
                        {
                            self.area = Some((i, a, pdf));
                        }
                    }
                    if resp.clicked() {
                        self.selection = None;
                        let link = self.texts.get(&i).and_then(|(_, ls)| ls.iter().find(|l| to_screen(l.quad).contains(p)).cloned());
                        if let Some(l) = link {
                            match l.target {
                                LinkTarget::Uri(u) => ui.ctx().open_url(egui::OpenUrl::new_tab(u)),
                                LinkTarget::Page(n) => self.scroll_to = Some((n as usize, 0.0)),
                            }
                        } else if let Some(h) = self
                            .highlights
                            .iter()
                            .find(|h| h.anchor.p as usize == i + 1 && h.anchor.q.iter().any(|q| to_screen(*q).contains(p)))
                        {
                            self.chosen = Some(h.id.clone());
                            self.comment.clone_from(&h.comment);
                            self.show_list = true;
                        }
                    }
                }
                if let Some((ap, a, b)) = self.area
                    && ap == i
                {
                    let q = [a.0.min(b.0), a.1.min(b.1), a.0.max(b.0), a.1.max(b.1)];
                    painter.rect_stroke(to_screen(q), 2.0, Stroke::new(1.5, pal.accent), egui::StrokeKind::Outside);
                }
            }
            if resp.drag_stopped() {
                self.selecting = false;
                if let Some((p, a, b)) = self.area.take() {
                    let q = [a.0.min(b.0), a.1.min(b.1), a.0.max(b.0), a.1.max(b.1)];
                    if q[2] - q[0] > 4.0 && q[3] - q[1] > 4.0 {
                        self.send(Req::Crop { page: p, rect: q });
                    }
                }
            }
            if let Some(c) = current {
                self.current_page = c;
            }
            // A small bar over a finished selection.
            if !self.selecting
                && let Some((page, range, text)) = self.selected_text()
                && let Some((t, _)) = self.texts.get(&page)
                && let Some(q) = t.quads(range).first().copied()
            {
                let info = self.pages[page];
                let top = self.page_top(page);
                let page_x = (full.width() - info.width * self.zoom) / 2.0;
                let [l, tt, _, _] = info.view_rect(q);
                let at = full.min + Vec2::new(page_x + l * self.zoom, top + tt * self.zoom - 40.0);
                let mut act = None;
                egui::Area::new(egui::Id::unique(("quire-pdf-sel", self.path.as_str()))).order(egui::Order::Foreground).fixed_pos(at).show(
                    ui.ctx(),
                    |ui| {
                        egui::Frame::popup(ui.style()).show(ui, |ui| {
                            ui.horizontal(|ui| {
                                if ui.button("Highlight").clicked() {
                                    act = Some(1);
                                }
                                if ui.button("Highlight and comment").clicked() {
                                    act = Some(2);
                                }
                                if ui.button("Copy").clicked() {
                                    act = Some(3);
                                }
                            });
                        });
                    },
                );
                match act {
                    Some(1) => self.make_highlight(ws, false),
                    Some(2) => self.make_highlight(ws, true),
                    Some(3) => {
                        ui.ctx().copy_text(text.split_whitespace().collect::<Vec<_>>().join(" "));
                        self.selection = None;
                    }
                    _ => {}
                }
            }
        });
        // Ctrl+C copies the selection.
        if ui.input(|i| i.events.iter().any(|e| matches!(e, egui::Event::Copy)))
            && let Some((_, _, t)) = self.selected_text()
        {
            ui.ctx().copy_text(t.split_whitespace().collect::<Vec<_>>().join(" "));
        }
    }

    /// Lite: each page's text, with find.
    #[cfg(not(feature = "typeset"))]
    fn text_view(&mut self, ui: &mut egui::Ui, pal: &Palette) {
        let Some(all) = &self.all_text else {
            ui.label(RichText::new("Reading the text…").color(pal.muted));
            return;
        };
        ui.label(
            RichText::new("Quire Lite shows the PDF's text. Open it in your system viewer to see the pages; highlights in its annotation note are listed here.")
                .color(pal.muted)
                .size(12.0),
        );
        let mut area = egui::ScrollArea::vertical().id_salt(("pdf-text", self.path.as_str())).auto_shrink([false, false]);
        let scroll_page = self.scroll_to.take().map(|(p, _)| p);
        if scroll_page.is_none() {
            area = area.scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::VisibleWhenNeeded);
        }
        let hits = &self.hits;
        area.show(ui, |ui| {
            for (i, page) in all.iter().enumerate() {
                let r = ui.label(RichText::new(format!("Page {}", i + 1)).size(12.0).color(pal.muted).strong());
                if scroll_page == Some(i) {
                    r.scroll_to_me(Some(egui::Align::TOP));
                }
                let text = page.text();
                if page.image_only {
                    ui.label(
                        RichText::new("(a scanned page: its text appears here once on-device OCR has read it)").color(pal.faint).size(12.0),
                    );
                }
                let n = hits.iter().filter(|(p, _)| *p == i).count();
                if n > 0 {
                    ui.label(RichText::new(format!("{n} match{}", if n == 1 { "" } else { "es" })).color(pal.warn).size(12.0));
                }
                ui.add(egui::Label::new(RichText::new(text).size(13.5).color(pal.text)).selectable(true).wrap());
                ui.add_space(14.0);
            }
        });
    }
}

/// The char of `t` nearest a PDF point (on the same line when there is one).
#[cfg(feature = "typeset")]
fn char_at(t: &PageText, p: (f32, f32)) -> Option<usize> {
    let mut best: Option<(f32, usize)> = None;
    for (i, (c, q)) in t.chars.iter().enumerate() {
        if *c == '\n' {
            continue;
        }
        let dx = if p.0 < q[0] {
            q[0] - p.0
        } else if p.0 > q[2] {
            p.0 - q[2]
        } else {
            0.0
        };
        let dy = if p.1 < q[1] {
            q[1] - p.1
        } else if p.1 > q[3] {
            p.1 - q[3]
        } else {
            0.0
        };
        // Being on the line matters more than being near horizontally.
        let d = dx + dy * 4.0;
        if best.is_none_or(|(b, _)| d < b) {
            best = Some((d, i));
        }
    }
    best.filter(|(d, _)| *d < 60.0).map(|(_, i)| i)
}

impl Surface for PdfSurface {
    fn path(&self) -> &NotePath {
        &self.path
    }

    fn show(&mut self, ui: &mut egui::Ui, ws: &mut Workspace, pal: &Palette) -> PanelOut {
        if self.tx.is_none() && self.error.is_none() {
            self.start(ws, ui.ctx());
        }
        self.poll(ws, ui.ctx());
        self.refresh_highlights(ws);
        self.take_jump(ws);
        // Ctrl+F here finds in the PDF, not in the note behind it.
        if ws.find.open {
            ws.find.open = false;
            // The note's find bar asked for the focus this frame too; ask
            // again next frame so the PDF's box keeps it.
            self.find_focus = 2;
        }
        let mut out = PanelOut::default();
        egui::Frame::NONE.fill(pal.panel).inner_margin(egui::Margin::symmetric(10, 5)).show(ui, |ui| {
            let o = self.toolbar(ui, ws, pal);
            out.jumped |= o.jumped;
        });
        if let Some(e) = &self.error {
            ui.vertical_centered(|ui| {
                ui.add_space(ui.available_height() * 0.3);
                ui.label(RichText::new(format!("Can't read this PDF: {e}")).color(pal.muted));
            });
            return out;
        }
        if self.show_list {
            egui::Panel::right(egui::Id::unique(("quire-pdf-list", self.path.as_str())))
                .resizable(true)
                .default_size(280.0)
                .size_range(200.0..=440.0)
                .frame(egui::Frame::NONE.fill(pal.panel).inner_margin(egui::Margin::symmetric(10, 8)))
                .show(ui, |ui| {
                    let o = self.side_list(ui, ws, pal);
                    out.jumped |= o.jumped;
                });
        }
        if self.pages.is_empty() {
            ui.centered_and_justified(|ui| ui.label(RichText::new("Opening…").color(pal.muted)));
            ui.ctx().request_repaint_after(Duration::from_millis(100));
            return out;
        }
        #[cfg(feature = "typeset")]
        self.pages_view(ui, ws, pal);
        #[cfg(not(feature = "typeset"))]
        self.text_view(ui, pal);
        out
    }
}

#[cfg(test)]
mod tests {
    use egui::{Pos2, Rect};

    use super::*;

    fn fixture(name: &str) -> Vec<u8> {
        std::fs::read(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../crates/quire-pdf/tests/fixtures").join(name)).unwrap()
    }

    #[test]
    fn opens_reads_and_highlights_a_pdf() {
        let dir = std::env::temp_dir().join(format!("quire-pdf-surface-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("paper.pdf"), fixture("text.pdf")).unwrap();
        let mut ws = Workspace::new(quire_app::Settings::default());
        ws.open_vault(&dir).unwrap();
        let path = NotePath::new("paper.pdf");
        let mut s = PdfSurface::new(&path);
        let ctx = egui::Context::default();
        crate::theme::install_fonts(&ctx);
        let input =
            || egui::RawInput { screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(1200.0, 800.0))), ..Default::default() };
        let deadline = Instant::now() + Duration::from_secs(30);
        // Lite shows the text only, so it never asks for one page's text layer.
        let needs_layer = cfg!(feature = "typeset");
        while (s.all_text.is_none() || (needs_layer && s.texts.is_empty())) && Instant::now() < deadline {
            let mut out = ctx.run_ui(input(), |ui| {
                s.show(ui, &mut ws, &crate::theme::LIGHT);
            });
            out.textures_delta.clear();
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(s.pages.len(), 2);
        assert!(s.all_text.as_ref().unwrap()[0].text().contains("Ropes are balanced"));
        // Find.
        s.find_query = "balanced trees".into();
        s.send(Req::Find("balanced trees".into()));
        let deadline = Instant::now() + Duration::from_secs(10);
        while s.found_for != "balanced trees" && Instant::now() < deadline {
            s.poll(&mut ws, &ctx);
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(s.hits.len(), 2, "on both pages");
        #[cfg(feature = "typeset")]
        {
            // Select "Ropes are" and highlight it.
            s.selection = Some((0, 0, 8));
            s.make_highlight(&mut ws, true);
            let hl = pdf_state::highlights(&ws, &path);
            assert_eq!(hl.len(), 1);
            assert_eq!(hl[0].anchor.t, "Ropes are");
            assert_eq!(s.chosen.as_deref(), Some(hl[0].id.as_str()));
            // A deep link scrolls to the highlight.
            ws.features.pdf.jump = Some(pdf_state::Jump { path, page: None, annot: Some(hl[0].id.clone()) });
            s.hl_generation = None;
            s.refresh_highlights(&ws);
            s.take_jump(&mut ws);
            assert!(s.flash.is_some() && s.scroll_to.is_some());
        }
        drop(ws);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(feature = "typeset")]
    #[test]
    fn char_hit_testing() {
        let mut t = PageText::default();
        for (i, c) in "ab".chars().enumerate() {
            let x = 10.0 * i as f32;
            t.chars.push((c, [x, 0.0, x + 10.0, 10.0]));
        }
        assert_eq!(char_at(&t, (15.0, 5.0)), Some(1));
        assert_eq!(char_at(&t, (-3.0, 5.0)), Some(0));
        assert_eq!(char_at(&t, (500.0, 500.0)), None);
    }
}
