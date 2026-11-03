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
