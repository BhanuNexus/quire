// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Markdown in canvas cards: a light renderer to an egui `LayoutJob`
//! (headings, lists, tasks, quotes, code, bold, italic, links, tags,
//! highlights). Galleys are cached by the caller.

use egui::text::{LayoutJob, TextFormat};
use egui::{Color32, FontFamily, FontId};

use crate::theme::Palette;

fn family(name: &str) -> FontFamily {
    FontFamily::Name(name.into())
}

#[derive(Clone, Copy)]
struct Style {
    size: f32,
    bold: bool,
    italic: bool,
    code: bool,
    color: Color32,
    mark: bool,
}

fn format(st: Style, pal: &Palette) -> TextFormat {
    let fam = if st.code {
        FontFamily::Monospace
    } else if st.bold {
        family(crate::theme::BOLD)
    } else if st.italic {
        family(crate::theme::ITALIC)
    } else {
        FontFamily::Proportional
    };
    let size = if st.code { st.size * 0.92 } else { st.size };
    let mut f = TextFormat::simple(FontId::new(size, fam), st.color);
    if st.code {
        f.background = pal.code_bg;
    } else if st.mark {
        f.background = pal.mark_bg;
        f.color = if pal.dark { Color32::from_rgb(30, 30, 30) } else { st.color };
    }
    f
}

/// Inline Markdown of one line into `job`.
fn inline(job: &mut LayoutJob, text: &str, base: Style, pal: &Palette) {
    let mut st = base;
    let mut buf = String::new();
    let flush = |job: &mut LayoutJob, buf: &mut String, st: Style| {
        if !buf.is_empty() {
            job.append(buf, 0.0, format(st, pal));
            buf.clear();
        }
    };
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let next = chars.get(i + 1).copied();
        let rest: String = chars[i..].iter().take(3).collect();
        if c == '`'
            && !st.code
            && let Some(end) = chars[i + 1..].iter().position(|x| *x == '`')
        {
            flush(job, &mut buf, st);
            let code: String = chars[i + 1..i + 1 + end].iter().collect();
            job.append(&code, 0.0, format(Style { code: true, ..st }, pal));
            i += end + 2;
            continue;
        }
        if rest.starts_with("**") || rest.starts_with("__") {
            flush(job, &mut buf, st);
            st.bold = !st.bold;
            i += 2;
            continue;
        }
        if rest.starts_with("==") {
            flush(job, &mut buf, st);
            st.mark = !st.mark;
            i += 2;
            continue;
        }
        if (c == '*' || c == '_')
            && (st.italic || next.is_some_and(|n| !n.is_whitespace()))
            && (c == '*' || i == 0 || !chars[i - 1].is_alphanumeric())
        {
            flush(job, &mut buf, st);
            st.italic = !st.italic;
            i += 1;
            continue;
        }
        if rest.starts_with("[[") {
            let s: String = chars[i + 2..].iter().collect();
            if let Some(end) = s.find("]]") {
                flush(job, &mut buf, st);
                let inner = &s[..end];
                let shown = inner.split_once('|').map_or(inner, |(_, a)| a);
                let shown = shown.split('#').next().filter(|t| !t.is_empty()).unwrap_or(shown);
                job.append(shown, 0.0, format(Style { color: pal.link, ..st }, pal));
                i += 2 + s[..end].chars().count() + 2;
                continue;
            }
        }
        if c == '[' {
            let s: String = chars[i + 1..].iter().collect();
            if let Some(close) = s.find("](")
                && let Some(end) = s[close..].find(')')
            {
                flush(job, &mut buf, st);
                job.append(&s[..close], 0.0, format(Style { color: pal.link, ..st }, pal));
                i += 1 + s[..=(close + end)].chars().count();
                continue;
            }
        }
        if c == '#' && next.is_some_and(char::is_alphanumeric) && (i == 0 || chars[i - 1].is_whitespace()) {
            flush(job, &mut buf, st);
            let len = chars[i + 1..].iter().take_while(|x| x.is_alphanumeric() || matches!(x, '_' | '-' | '/')).count();
            let tag: String = chars[i..=i + len].iter().collect();
            job.append(&tag, 0.0, format(Style { color: pal.tag, ..st }, pal));
            i += len + 1;
            continue;
        }
        buf.push(c);
        i += 1;
    }
    flush(job, &mut buf, st);
}

/// A card's Markdown as a layout job, wrapping at `width` (screen points),
/// with body text at `size`.
pub(crate) fn markdown_job(src: &str, width: f32, size: f32, pal: &Palette) -> LayoutJob {
    let mut job = LayoutJob { wrap: egui::text::TextWrapping { max_width: width.max(10.0), ..Default::default() }, ..Default::default() };
    let base = Style { size, bold: false, italic: false, code: false, color: pal.text, mark: false };
    let mut fence = false;
    let mut first = true;
    for line in src.lines() {
        let t = line.trim_start();
        if t.starts_with("```") || t.starts_with("~~~") {
            fence = !fence;
            continue;
        }
        if !first {
            job.append("\n", 0.0, format(base, pal));
        }
        first = false;
        if fence {
            job.append(line, 0.0, format(Style { code: true, ..base }, pal));
            continue;
        }
        let level = t.chars().take_while(|c| *c == '#').count();
        if (1..=6).contains(&level) && t[level..].starts_with(' ') {
            let scale = [1.5, 1.3, 1.15, 1.05, 1.0, 1.0][level - 1];
            inline(&mut job, t[level..].trim(), Style { size: size * scale, bold: true, color: pal.strong, ..base }, pal);
            continue;
        }
        let indent = (line.len() - t.len()) / 2;
        let lead = "    ".repeat(indent.min(4));
        let task = [("- [ ] ", false), ("- [x] ", true), ("- [X] ", true), ("* [ ] ", false), ("* [x] ", true)]
            .into_iter()
            .find_map(|(m, done)| t.strip_prefix(m).map(|rest| (rest, done)));
        if let Some((rest, done)) = task {
            job.append(&format!("{lead}{}", if done { "☑ " } else { "☐ " }), 0.0, format(Style { color: pal.muted, ..base }, pal));
            inline(&mut job, rest, Style { color: if done { pal.muted } else { pal.text }, ..base }, pal);
            continue;
        }
        if let Some(rest) = t.strip_prefix("- ").or_else(|| t.strip_prefix("* ")).or_else(|| t.strip_prefix("+ ")) {
            job.append(&format!("{lead}•  "), 0.0, format(Style { color: pal.muted, ..base }, pal));
            inline(&mut job, rest, base, pal);
            continue;
        }
        if let Some(rest) = t.strip_prefix("> ").or_else(|| t.strip_prefix('>')) {
            job.append("▎ ", 0.0, format(Style { color: pal.faint, ..base }, pal));
            inline(&mut job, rest, Style { color: pal.quote, italic: true, ..base }, pal);
            continue;
        }
        if t == "---" || t == "***" {
            job.append("———", 0.0, format(Style { color: pal.faint, ..base }, pal));
            continue;
        }
        inline(&mut job, line, base, pal);
    }
    job
}

/// Draw Markdown with the note preview's renderer (`egui_commonmark`) at
/// canvas zoom `zoom`, `width` points wide, and keep what it painted: a
/// card's picture as shapes placed at (0, 0). Nothing is shown while this
/// runs (the shapes are taken from a layer of their own). A table measures
/// itself the first time and asks egui to run the frame again
/// (`will_discard`); don't keep that picture.
pub(crate) fn render_markdown(
    ui: &egui::Ui,
    cache: &mut egui_commonmark::CommonMarkCache,
    md: &str,
    width: f32,
    zoom: f32,
    salt: &str,
) -> CardPicture {
    let id = egui::Id::unique(("quire-card-md", salt));
    let layer = egui::LayerId::new(egui::Order::Background, id);
    let pending = draw_markdown(ui, cache, md, width, zoom, id, layer);
    let shapes: Vec<egui::Shape> = ui.ctx().graphics_mut(|g| {
        let list = g.entry(layer);
        let shapes = list.all_entries().map(|c| c.shape.clone()).collect();
        *list = egui::layers::PaintList::default();
        shapes
    });
    let links = card_links(&shapes, md, ui.visuals().hyperlink_color);
    CardPicture { shapes, links, complete: !pending }
}

/// A card as drawn: its shapes at (0, 0), where its links are, and
/// whether everything in it was ready (math still being typeset isn't).
#[derive(Default)]
pub(crate) struct CardPicture {
    pub shapes: Vec<egui::Shape>,
    pub links: Vec<CardLink>,
    pub complete: bool,
}

/// A link in a card (0.11): where it goes and where it is, from the card's
/// top-left at the zoom it was drawn at.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CardLink {
    pub url: String,
    pub rect: egui::Rect,
}

/// `[text](url)` links in Markdown, in order: (text without emphasis marks, url).
fn md_links(md: &str) -> Vec<(String, String)> {
    let chars: Vec<char> = md.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '[' && (i == 0 || chars[i - 1] != '!') {
            let mut depth = 0;
            let mut close = None;
            for (j, c) in chars.iter().enumerate().skip(i) {
                match c {
                    '[' => depth += 1,
                    ']' => {
                        depth -= 1;
                        if depth == 0 {
                            close = Some(j);
                            break;
                        }
                    }
                    '\n' if j > i && chars.get(j + 1) == Some(&'\n') => break,
                    _ => {}
                }
            }
            if let Some(c) = close
                && chars.get(c + 1) == Some(&'(')
                && let Some(end) = chars[c + 2..].iter().position(|x| *x == ')' || *x == '\n')
                && chars[c + 2 + end] == ')'
            {
                let text: String = chars[i + 1..c].iter().filter(|x| !matches!(x, '*' | '_' | '`' | '=' | '~')).collect();
                let url: String = chars[c + 2..c + 2 + end].iter().collect();
                let url = url.split_whitespace().next().unwrap_or_default().trim_matches(['<', '>']).to_string();
                out.push((text.split_whitespace().collect::<Vec<_>>().join(" "), url));
                i = c + 2 + end + 1;
                continue;
            }
        }
        i += 1;
    }
    out
}

/// Where the links are among a card's shapes. Each link is a text shape
/// in the link colour, in reading order, so the n-th is the n-th link of
/// the Markdown; a link whose text matches a later one skips ahead.
fn card_links(shapes: &[egui::Shape], md: &str, link_color: Color32) -> Vec<CardLink> {
    fn texts(s: &egui::Shape, color: Color32, out: &mut Vec<(String, egui::Rect)>) {
        match s {
            egui::Shape::Text(t) if t.fallback_color == color && t.override_text_color.is_none() => {
                out.push((t.galley.text().split_whitespace().collect::<Vec<_>>().join(" "), t.galley.rect.translate(t.pos.to_vec2())));
            }
            egui::Shape::Vec(v) => v.iter().for_each(|x| texts(x, color, out)),
            _ => {}
        }
    }
    let mut found = Vec::new();
    for s in shapes {
        texts(s, link_color, &mut found);
    }
    let links = md_links(md);
    let mut next = 0;
    let mut out = Vec::new();
    for (text, rect) in found {
        let url = if let Some(k) = (next..links.len()).find(|k| links[*k].0 == text) {
            next = k + 1;
            links[k].1.clone()
        } else if text.starts_with("http://") || text.starts_with("https://") {
            text.clone()
        } else if let Some(l) = links.get(next) {
            next += 1;
            l.1.clone()
        } else {
            continue;
        };
        out.push(CardLink { url, rect });
    }
    out
}

/// Returns true when math in it is still being typeset.
fn draw_markdown(
    ui: &egui::Ui,
    cache: &mut egui_commonmark::CommonMarkCache,
    md: &str,
    width: f32,
    zoom: f32,
    id: egui::Id,
    layer: egui::LayerId,
) -> bool {
    let rect = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(width.max(10.0), 1.0e5));
    let mut child = egui::Ui::new(ui.ctx().clone(), id, egui::UiBuilder::new().layer_id(layer).max_rect(rect));
    child.set_style(std::sync::Arc::clone(ui.style()));
    {
        let st = child.style_mut();
        for f in st.text_styles.values_mut() {
            f.size *= zoom;
        }
        st.spacing.item_spacing *= zoom;
        st.spacing.indent *= zoom;
        st.spacing.interact_size *= zoom;
        st.spacing.icon_width *= zoom;
        st.spacing.icon_spacing *= zoom;
    }
    child.set_clip_rect(rect);
    // 0.11: `$math$` and `$$math$$` in cards, as in the preview.
    let pending = std::rc::Rc::new(std::cell::Cell::new(false));
    let (color, size) = (ui.visuals().text_color(), child.style().text_styles[&egui::TextStyle::Body].size);
    let flag = std::rc::Rc::clone(&pending);
    let math = move |ui: &mut egui::Ui, src: &str, inline: bool| {
        let job = crate::typeset_ui::Job::Math {
            latex: src.trim().to_string(),
            display: !inline,
            size: size.round() as u32,
            color: crate::typeset_ui::rgba(color),
        };
        match crate::typeset_ui::request(ui.ctx(), &job) {
            crate::typeset_ui::Out::Ready(p) => {
                let scale = (ui.available_width() / p.size.x).min(1.0);
                ui.image((p.texture.id(), p.size * scale));
            }
            crate::typeset_ui::Out::Pending => {
                flag.set(true);
                ui.label(egui::RichText::new(src).monospace().weak());
            }
            crate::typeset_ui::Out::Failed(_) => {
                ui.label(egui::RichText::new(src).monospace().weak());
            }
        }
    };
    egui_commonmark::CommonMarkViewer::new()
        .alerts(crate::panels::callout_alerts())
        .max_image_width(Some(width.max(10.0) as usize))
        .render_math_fn(crate::typeset_ui::AVAILABLE.then_some(&math as &egui_commonmark::RenderMathFn))
        .show(&mut child, cache, md);
    pending.get()
}

/// A shape moved by `d` (text keeps its laid-out galley).
pub(crate) fn moved(s: &egui::Shape, d: egui::Vec2) -> egui::Shape {
    match s {
        egui::Shape::Text(t) => {
            let mut t = t.clone();
            t.pos += d;
            egui::Shape::Text(t)
        }
        egui::Shape::Vec(v) => egui::Shape::Vec(v.iter().map(|x| moved(x, d)).collect()),
        other => {
            let mut o = other.clone();
            o.translate(d);
            o
        }
    }
}

/// The Mermaid source when a card is just a diagram.
pub(crate) fn mermaid(src: &str) -> Option<&str> {
    let t = src.trim();
    let body = t.strip_prefix("```mermaid")?.strip_suffix("```")?;
    Some(body.trim_matches('\n'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_links_in_order_and_by_text() {
        let md = "See [**Plan**](quire://note/Plan) and [the site](https://x.org \"t\"), ![img](a.png) [Plan](quire://note/Plan2)";
        assert_eq!(
            md_links(md),
            vec![
                ("Plan".to_string(), "quire://note/Plan".to_string()),
                ("the site".to_string(), "https://x.org".to_string()),
                ("Plan".to_string(), "quire://note/Plan2".to_string()),
            ]
        );
        let link = |t: &str, y: f32| {
            let ctx = egui::Context::default();
            let mut g = None;
            let mut out = ctx.run_ui(egui::RawInput::default(), |ui| {
                g = Some(ui.painter().layout_no_wrap(t.into(), FontId::proportional(12.0), Color32::RED));
            });
            out.textures_delta.clear();
            egui::Shape::Text(egui::epaint::TextShape::new(egui::pos2(0.0, y), g.expect("laid out"), Color32::BLUE))
        };
        // The first "Plan" link wasn't drawn as a link (say it was cut off): text decides.
        let found = card_links(&[link("the site", 0.0), link("Plan", 20.0)], md, Color32::BLUE);
        assert_eq!(found.iter().map(|l| l.url.as_str()).collect::<Vec<_>>(), ["https://x.org", "quire://note/Plan2"]);
        assert!(found[1].rect.top() >= 20.0);
    }

    #[test]
    fn renders_the_common_syntax() {
        let pal = &crate::theme::LIGHT;
        let job = markdown_job(
            "# Ropes\n- [x] done **bold** `code` *it*\n- item [[Rope Notes|notes]]\n> quoted #tag\n```\nlet x\n```",
            200.0,
            14.0,
            pal,
        );
        assert_eq!(job.text, "Ropes\n☑ done bold code it\n•  item notes\n▎ quoted #tag\nlet x");
        assert_eq!(mermaid("```mermaid\ngraph TD; A-->B\n```"), Some("graph TD; A-->B"));
        assert_eq!(mermaid("text"), None);
    }
}
