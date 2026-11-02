// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! LaTeX math -> Typst math (via `mitex`) -> SVG (via Typst).

use std::fmt::Write as _;
use std::panic::{AssertUnwindSafe, catch_unwind};

use typst::layout::{Frame, FrameItem};

use crate::cache::{Cache, hash_of};
use crate::world::{MATH_FONT, TypesetWorld};
use crate::{Rendered, TypesetError, typst_string};

/// Longest formula we try to typeset.
const MAX_LATEX_LEN: usize = 20_000;

/// How a formula is drawn.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MathStyle {
    /// Font size in points.
    pub size_pt: f32,
    /// Text colour as RGBA.
    pub color: [u8; 4],
}

impl Default for MathStyle {
    fn default() -> Self {
        Self { size_pt: 14.0, color: [0, 0, 0, 255] }
    }
}

type MathKey = (String, bool, u32, [u8; 4]);

static MATH_CACHE: Cache<MathKey, Result<Rendered, TypesetError>> = Cache::new();

/// Renders LaTeX math to an SVG sized to the formula.
///
/// The SVG has zero margin and a transparent background; `display` selects
/// display style (`$$…$$`). Results, including errors, are cached, so calling
/// this every frame is cheap.
#[allow(clippy::trivially_copy_pass_by_ref, reason = "by reference so the style can grow without an API change")]
pub fn math_svg(latex: &str, display: bool, opts: &MathStyle) -> Result<Rendered, TypesetError> {
    if !opts.size_pt.is_finite() || !(1.0..=1000.0).contains(&opts.size_pt) {
        return Err(TypesetError::Invalid(format!("font size {} pt is out of range", opts.size_pt)));
    }
    let size_bits = opts.size_pt.to_bits();
    let hash = hash_of(&(latex, display, size_bits, opts.color));
    if let Some(hit) = MATH_CACHE.get(hash, |k| k.0 == latex && k.1 == display && k.2 == size_bits && k.3 == opts.color) {
        return hit;
    }
    let result = render(latex, display, *opts);
    MATH_CACHE.insert(hash, (latex.to_owned(), display, size_bits, opts.color), result.clone());
    result
}

fn render(latex: &str, display: bool, opts: MathStyle) -> Result<Rendered, TypesetError> {
    let typst_math = latex_to_typst(latex)?;
    let [r, g, b, a] = opts.color;
    let mut main = String::with_capacity(typst_math.len() + 400);
    main.push_str("#import \"/mitex.typ\": mitex-math\n#set page(width: auto, height: auto, margin: 0pt, fill: none)\n");
    let _ = writeln!(
        main,
        "#set text(size: {}pt, fill: rgb({r}, {g}, {b}, {a}), font: \"{MATH_FONT}\", top-edge: \"bounds\", bottom-edge: \"bounds\")",
        opts.size_pt
    );
    let _ = writeln!(main, "#show math.equation: set text(font: \"{MATH_FONT}\")");
    let _ = writeln!(main, "#mitex-math({}, block: {display})", typst_string(&typst_math));

    let doc = TypesetWorld::new(main).compile()?;
    let page = doc.pages().first().ok_or_else(|| TypesetError::Internal("no page".into()))?;
    let svg = catch_unwind(AssertUnwindSafe(|| typst_svg::svg(page, &typst_svg::SvgOptions::default())))
        .map_err(|_| TypesetError::Internal("SVG export crashed".into()))?;
    let width = page.frame.width().to_pt();
    let height = page.frame.height().to_pt();
    let baseline = find_baseline(&page.frame, 0.0).unwrap_or(height).clamp(0.0, height);
    Ok(Rendered { svg, width_pt: width as f32, height_pt: height as f32, baseline_pt: baseline as f32 })
}

/// Distance from the top of `frame` to the first baseline inside it.
fn find_baseline(frame: &Frame, top: f64) -> Option<f64> {
    for (pos, item) in frame.items() {
        match item {
            FrameItem::Group(group) => {
                let y = top + pos.y.to_pt();
                if group.frame.has_baseline() {
                    return Some(y + group.frame.baseline().to_pt());
                }
                if let Some(found) = find_baseline(&group.frame, y) {
                    return Some(found);
                }
            }
            // Text runs are positioned at their baseline.
            FrameItem::Text(_) => return Some(top + pos.y.to_pt()),
            _ => {}
        }
    }
    None
}

/// Converts LaTeX math to Typst math code for `mitex-math`.
pub(crate) fn latex_to_typst(latex: &str) -> Result<String, TypesetError> {
    let latex = latex.trim();
    if latex.is_empty() {
        return Err(TypesetError::Latex("empty formula".into()));
    }
    if latex.len() > MAX_LATEX_LEN {
        return Err(TypesetError::Latex("formula is too long".into()));
    }
    let cleaned = sanitize(latex)?;
    let converted = catch_unwind(|| mitex::convert_math(&cleaned, None))
        .map_err(|_| TypesetError::Latex("could not parse formula".into()))?
        .map_err(|e| TypesetError::Latex(tidy_mitex_error(&e)))?;
    Ok(rename_symbols(&converted))
}

fn tidy_mitex_error(message: &str) -> String {
    let message = message.trim();
    let message = message.strip_prefix("error: ").unwrap_or(message);
    let message = message.strip_prefix("error ").unwrap_or(message);
    match message.strip_prefix("unexpected: ") {
        Some(token) => format!("unexpected {token}"),
        None => message.to_owned(),
    }
}

/// Text-mode constructs that make no sense in a formula (and some of which
/// crash `mitex` when malformed), plus `\iftypst`, which would pass raw Typst
/// code through.
const REJECTED_COMMANDS: &[&str] = &["includegraphics", "caption", "iftypst", "item", "input", "include"];
const REJECTED_ENVS: &[&str] = &["figure", "table", "tabular", "itemize", "enumerate", "quote", "abstract"];

/// Removes `\label{…}` and rejects unsupported commands, before `mitex` sees them.
fn sanitize(latex: &str) -> Result<String, TypesetError> {
    let mut out = String::with_capacity(latex.len());
    let mut rest = latex;
    while let Some(i) = rest.find('\\') {
        out.push_str(&rest[..i]);
        let after = &rest[i + 1..];
        let name_len = after.find(|c: char| !c.is_ascii_alphabetic()).unwrap_or(after.len());
        if name_len == 0 {
            // A control symbol such as `\\` or `\{`: keep it and the next char.
            let next = after.chars().next().map_or(0, char::len_utf8);
            out.push_str(&rest[i..i + 1 + next]);
            rest = &after[next..];
            continue;
        }
        let name = &after[..name_len];
        let tail = &after[name_len..];
        if name == "label" {
            let Some(group_len) = braced_group_len(tail) else {
                return Err(TypesetError::Latex("\\label needs an argument".into()));
            };
            rest = &tail[group_len..];
            continue;
        }
        // mitex reads only `\xrightarrow{above}`; LaTeX also has an optional
        // `[below]`: rewrite to `\underset{below}{\xrightarrow{above}}`.
        if name.starts_with('x')
            && X_ARROWS.contains(&name)
            && let Some((below, above, consumed)) = optional_then_braced(tail)
        {
            let below = sanitize(below)?;
            let above = sanitize(above)?;
            let _ = write!(out, "\\underset{{{below}}}{{\\{name}{{{above}}}}}");
            rest = &tail[consumed..];
            continue;
        }
        if REJECTED_COMMANDS.contains(&name) {
            return Err(TypesetError::Latex(format!("\\{name} is not supported in math")));
        }
        if name == "begin" {
            let env = braced_group_len(tail).map(|len| tail[..len].trim().trim_start_matches('{').trim_end_matches('}').trim());
            match env {
                None | Some("") => return Err(TypesetError::Latex("\\begin needs an environment name".into())),
                Some(env) if REJECTED_ENVS.contains(&env.trim_end_matches('*')) => {
                    return Err(TypesetError::Latex(format!("the {env} environment is not supported in math")));
                }
                Some(_) => {}
            }
        }
        out.push('\\');
        // mitex has no `\mathscr`; script capitals are the closest Typst has.
        out.push_str(if name == "mathscr" { "mathcal" } else { name });
        rest = tail;
    }
    out.push_str(rest);
    Ok(out)
}

/// Extensible arrows that take an optional `[below]` argument in LaTeX.
const X_ARROWS: &[&str] = &[
    "xleftarrow",
    "xrightarrow",
    "xLeftarrow",
    "xRightarrow",
    "xleftrightarrow",
    "xLeftrightarrow",
    "xhookleftarrow",
    "xhookrightarrow",
    "xmapsto",
    "xtwoheadleftarrow",
    "xtwoheadrightarrow",
    "xleftharpoonup",
    "xleftharpoondown",
    "xrightharpoonup",
    "xrightharpoondown",
    "xleftrightharpoons",
    "xrightleftharpoons",
    "xlongequal",
    "xtofrom",
];

/// For `s` = `[below]{above}…`: (below, above, bytes consumed).
fn optional_then_braced(s: &str) -> Option<(&str, &str, usize)> {
    let start = s.len() - s.trim_start().len();
    let inner = s[start..].strip_prefix('[')?;
    let close = inner.find(']')?;
    let below = &inner[..close];
    let after = start + 1 + close + 1;
    let group = braced_group_len(&s[after..])?;
    let above = s[after..after + group].trim().strip_prefix('{')?.strip_suffix('}')?;
    Some((below, above, after + group))
}

/// Length of leading whitespace plus one balanced `{…}` group, if `s` starts with one.
fn braced_group_len(s: &str) -> Option<usize> {
    let start = s.len() - s.trim_start().len();
    let mut depth = 0usize;
    let mut escaped = false;
    for (i, c) in s[start..].char_indices() {
        if i == 0 && c != '{' {
            return None;
        }
        if escaped {
            escaped = false;
            continue;
        }
        match c {
            '\\' => escaped = true,
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(start + i + 1);
                }
            }
            _ => {}
        }
    }
    None
}

/// Symbol names `mitex` 0.2 emits that Typst 0.14/0.15 renamed. A rename
/// also applies to longer names it prefixes (`sect` covers `sect.big`), so
/// more specific entries come first.
const RENAMES: &[(&str, &str)] = &[
    ("dot.circle", "dot.o"),
    ("plus.circle", "plus.o"),
    ("minus.circle", "minus.o"),
    ("times.circle", "times.o"),
    ("ast.circle", "ast.op.o"),
    ("dash.circle", "dash.o"),
    ("circle.nested", "compose.o"),
    ("arrow.l.dash", "arrow.l.dashed"),
    ("arrow.r.dash", "arrow.r.dashed"),
    ("bracket.l.double", "bracket.l.stroked"),
    ("bracket.r.double", "bracket.r.stroked"),
    ("angle.l", "chevron.l"),
    ("angle.r", "chevron.r"),
    ("planck.reduce", "planck"),
    ("ohm.inv", "Omega.inv"),
    ("sect", "inter"),
    ("diff", "partial"),
];

fn renamed(ident: &str) -> Option<String> {
    RENAMES.iter().find_map(|(from, to)| {
        let rest = ident.strip_prefix(from)?;
        (rest.is_empty() || rest.starts_with('.')).then(|| format!("{to}{rest}"))
    })
}

/// Applies [`RENAMES`] to (dotted) identifiers in math, leaving the text of
/// `#textmath[…]` and friends alone.
fn rename_symbols(code: &str) -> String {
    #[derive(PartialEq)]
    enum Mode {
        Content,
        Math,
    }
    let mut out = String::with_capacity(code.len());
    // Empty stack: top-level math.
    let mut modes: Vec<Mode> = Vec::new();
    let mut chars = code.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        let in_math = modes.last().is_none_or(|m| *m == Mode::Math);
        match c {
            '\\' => {
                out.push(c);
                if let Some((_, next)) = chars.next() {
                    out.push(next);
                }
            }
            '[' => {
                modes.push(Mode::Content);
                out.push(c);
            }
            ']' => {
                if modes.last() == Some(&Mode::Content) {
                    modes.pop();
                }
                out.push(c);
            }
            '$' => {
                if modes.last() == Some(&Mode::Content) {
                    modes.push(Mode::Math);
                } else if modes.last() == Some(&Mode::Math) {
                    modes.pop();
                }
                out.push(c);
            }
            c if c.is_ascii_alphabetic() && in_math => {
                let mut end = i + 1;
                while let Some(&(j, d)) = chars.peek() {
                    let dotted = d == '.' && code[j + 1..].starts_with(|e: char| e.is_ascii_alphabetic());
                    if d.is_ascii_alphanumeric() || dotted {
                        end = j + 1;
                        chars.next();
                    } else {
                        break;
                    }
                }
                let ident = &code[i..end];
                match renamed(ident) {
                    Some(new) => out.push_str(&new),
                    None => out.push_str(ident),
                }
            }
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_strips_labels_and_rejects_text_mode() {
        assert_eq!(sanitize(r"x \label{eq-1} = 1").unwrap(), r"x  = 1");
        assert!(sanitize(r"\label").is_err());
        assert!(sanitize(r"\label x").is_err());
        assert!(sanitize(r"\includegraphics{a.png}").is_err());
        assert!(sanitize(r"\begin{tabular}{c}a\end{tabular}").is_err());
        assert!(sanitize(r"\begin{}").is_err());
        assert!(sanitize(r"\begin").is_err());
        assert!(sanitize(r"\iftypst #read(x) \fi").is_err());
        assert_eq!(sanitize(r"a \\ \{ b \}").unwrap(), r"a \\ \{ b \}");
        assert_eq!(sanitize("\\é \\").unwrap(), "\\é \\");
        assert!(sanitize(r"\labelx{y}").is_ok());
        assert_eq!(sanitize(r"\xleftarrow[g]{h} x").unwrap(), r"\underset{g}{\xleftarrow{h}} x");
        assert_eq!(sanitize(r"\xrightarrow{f}").unwrap(), r"\xrightarrow{f}");
        assert_eq!(sanitize(r"\mathscr{F}").unwrap(), r"\mathcal{F}");
    }

    #[test]
    fn renames_follow_typst_symbols() {
        assert_eq!(rename_symbols("sect.big  diff  x sectx"), "inter.big  partial  x sectx");
        assert_eq!(rename_symbols("dot.circle.big dot.c"), "dot.o.big dot.c");
        assert_eq!(rename_symbols("#textmath[diff $diff x$ diff];"), "#textmath[diff $partial x$ diff];");
        assert_eq!(rename_symbols(r"\[ diff \]"), r"\[ partial \]");
        assert_eq!(latex_to_typst(r"\partial \langle x \rangle").unwrap(), "partial  chevron.l  x  chevron.r ");
    }

    #[test]
    fn mitex_panics_are_contained() {
        for bad in [r"\label", r"\includegraphics", r"\caption", r"\begin{tabular}", r"\begin{figure}\caption\end{figure}", "}", r"\foo"] {
            assert!(latex_to_typst(bad).is_err(), "{bad}");
        }
    }
}
