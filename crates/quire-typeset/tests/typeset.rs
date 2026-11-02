// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! End-to-end tests of the public API.

use std::sync::Once;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use quire_typeset::{MathStyle, PageSize, PdfOptions, TypesetError, diagram_svg, markdown_pdf, math_svg};

const SAMPLE: &str = include_str!("fixtures/sample.md");
const GRADIENT_PNG: &[u8] = include_bytes!("fixtures/gradient.png");

static PANICS: AtomicUsize = AtomicUsize::new(0);

/// Panics seen so far. The crate turns panics in its dependencies into
/// errors, but release builds use `panic = "abort"`, where that can't work:
/// so no input may make a dependency panic at all.
fn panics_so_far() -> usize {
    static HOOK: Once = Once::new();
    HOOK.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            PANICS.fetch_add(1, Ordering::SeqCst);
            previous(info);
        }));
    });
    PANICS.load(Ordering::SeqCst)
}

fn style() -> MathStyle {
    MathStyle { size_pt: 16.0, color: [20, 20, 20, 255] }
}

const FORMULAS: &[&str] = &[
    r"\frac{a}{b}",
    r"\dfrac{1}{2} + \tfrac{3}{4}",
    r"\sqrt{x^2 + y^2}",
    r"\sqrt[3]{8} = 2",
    r"x_i^2 + y_{i,j}^{n+1}",
    r"\alpha + \beta = \Gamma(\omega) \cdot \Delta \varepsilon \varphi \pi \Omega",
    r"\sum_{i=1}^{n} i^2 = \frac{n(n+1)(2n+1)}{6}",
    r"\int_0^\infty e^{-x}\,dx = 1",
    r"\iint_D f \, dA \quad \oint_C \mathbf{F} \cdot d\mathbf{r}",
    r"\prod_{k=1}^{\infty} \left(1 - \frac{1}{p_k^s}\right)^{-1}",
    r"\lim_{x \to 0} \frac{\sin x}{x} = 1",
    r"\begin{pmatrix} a & b \\ c & d \end{pmatrix}",
    r"\begin{bmatrix} 1 & 0 & 0 \\ 0 & 1 & 0 \\ 0 & 0 & 1 \end{bmatrix}",
    r"\begin{vmatrix} a & b \\ c & d \end{vmatrix} = ad - bc",
    r"\begin{aligned} f(x) &= (x+1)^2 \\ &= x^2 + 2x + 1 \end{aligned}",
    r"\begin{align*} a &= b + c \\ d &= e \end{align*}",
    r"f(x) = \begin{cases} 1 & \text{if } x > 0 \\ 0 & \text{otherwise} \end{cases}",
    r"\text{area} = \pi r^2",
    r"\mathbb{R}^n \subset \mathbb{C}^n, \quad \mathbb{N} \ni n",
    r"\operatorname{sgn}(x) \cdot \operatorname{tr}(A)",
    r"\left( \frac{a}{b} \right) \left[ x \right] \left\{ y \right\} \left| z \right|",
    r"\mathbf{v} \times \mathcal{L} \cdot \mathrm{d}x \, \mathit{y} \mathfrak{g}",
    r"\hat{x} \bar{y} \vec{v} \dot{a} \ddot{b} \tilde{n} \overline{AB}",
    r"\binom{n}{k} = \frac{n!}{k!(n-k)!}",
    r"a \leq b \geq c \neq d \approx e \equiv f \to g \Rightarrow h \infty",
    r"\overbrace{1 + 2 + \cdots + n}^{n \text{ terms}}",
    r"\underbrace{a + b}_{c}",
    r"\color{red} x + \textcolor{blue}{y}",
    r"\xrightarrow{f} \quad \overset{!}{=} \quad \underset{n}{\max}",
    r"\boxed{E = mc^2}",
    r"\{ x \in \mathbb{Z} \mid x > 0 \}",
    r"\langle u, v \rangle \lVert x \rVert \lfloor y \rfloor \lceil z \rceil",
    r"x \label{eq:1} = 1",
    r"\begin{array}{cc} 1 & 2 \\ 3 & 4 \end{array}",
    r"\big( \Big[ \bigg\{ \Bigg|",
    r"\argmax_x f(x)",
    r"\pmod{7} \quad a \bmod b",
    r"\frac{\partial^2 u}{\partial t^2} = c^2 \nabla^2 u",
];

#[test]
fn math_renders_common_latex() {
    for &latex in FORMULAS {
        for display in [false, true] {
            let r = math_svg(latex, display, &style()).unwrap_or_else(|e| panic!("{latex} (display {display}): {e}"));
            assert!(r.svg.starts_with("<svg") && r.svg.contains("</svg>"), "{latex}");
            assert!(r.svg.len() > 200, "{latex}: suspiciously small SVG");
            assert!(r.width_pt > 2.0 && r.width_pt < 2000.0, "{latex}: width {}", r.width_pt);
            assert!(r.height_pt > 2.0 && r.height_pt < 1000.0, "{latex}: height {}", r.height_pt);
            assert!(r.baseline_pt > 0.0 && r.baseline_pt <= r.height_pt, "{latex}: baseline {} of {}", r.baseline_pt, r.height_pt);
        }
    }
}

#[test]
fn math_size_and_baseline_are_sane() {
    let small = math_svg("x", false, &MathStyle { size_pt: 10.0, color: [0, 0, 0, 255] }).unwrap();
    let big = math_svg("x", false, &MathStyle { size_pt: 40.0, color: [0, 0, 0, 255] }).unwrap();
    assert!((big.width_pt / small.width_pt - 4.0).abs() < 0.3, "{} vs {}", big.width_pt, small.width_pt);
    // An `x` has no descender: the baseline is (almost) at the bottom.
    assert!(small.height_pt - small.baseline_pt < 1.0, "{small:?}");
    // A fraction reaches well below the baseline.
    let frac = math_svg(r"\frac{1}{2}", false, &style()).unwrap();
    assert!(frac.height_pt - frac.baseline_pt > 3.0, "{frac:?}");
    assert!(frac.baseline_pt > 3.0);
    // Colour ends up in the SVG.
    let red = math_svg("y", false, &MathStyle { size_pt: 12.0, color: [255, 0, 0, 255] }).unwrap();
    assert!(red.svg.contains("#ff0000"), "{}", red.svg);
}

#[test]
fn bad_latex_is_an_error_not_a_panic() {
    let before = panics_so_far();
    let bad = [
        r"\undefinedcommand",
        r"}",
        r"\frac{1}{\nosuchthing}",
        r"\begin{nosuchenv} x \end{nosuchenv}",
        r"\label",
        r"\includegraphics",
        r"\begin{tabular}",
        r#"\iftypst #read("/etc/passwd") \fi"#,
        "",
        "   ",
        r"\right)",
        r"\begin{}",
    ];
    for latex in bad {
        let err = math_svg(latex, false, &style()).expect_err(latex);
        let message = err.to_string();
        assert!(!message.is_empty() && message.len() < 200, "{latex}: {message:?}");
        assert!(!message.contains('\n'), "{latex}: {message:?}");
    }
    assert!(matches!(math_svg(r"\foo", false, &style()), Err(TypesetError::Latex(m)) if m.contains("\\foo")));
    assert!(math_svg("x", false, &MathStyle { size_pt: f32::NAN, color: [0; 4] }).is_err());
    assert!(math_svg("x", false, &MathStyle { size_pt: 0.0, color: [0; 4] }).is_err());
    assert_eq!(panics_so_far(), before, "a dependency panicked");
}

#[test]
fn math_never_panics_on_junk() {
    let pieces = [
        r"\frac",
        "{",
        "}",
        "^",
        "_",
        "&",
        r"\\",
        r"\left(",
        r"\right)",
        r"\begin{pmatrix}",
        r"\end{pmatrix}",
        "$",
        "#",
        "\"",
        "x",
        r"\sqrt[",
        "]",
        r"\text{",
        r"\color",
        "%",
        "~",
        r"\mathbb",
        "@",
        "'",
        r"\over",
        r"\sum\limits",
        r"\label",
        r"\includegraphics",
        r"\caption",
        r"\item",
        r"\begin{tabular}",
        r"\begin{figure}",
        r"\begin{itemize}",
        r"\end{itemize}",
        r"\begin{array}",
        r"\end{array}",
        r"\xrightarrow[",
        r"\operatorname",
        r"\hspace",
        r"\middle",
        "\u{0}",
        "é",
    ];
    let before = panics_so_far();
    let mut seed = 0x2545_f491_u32;
    for _ in 0..600 {
        let mut latex = String::new();
        for _ in 0..6 {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            latex.push_str(pieces[seed as usize % pieces.len()]);
        }
        let _ = math_svg(&latex, seed.is_multiple_of(2), &style());
    }
    assert_eq!(panics_so_far(), before, "a dependency panicked");
}

#[test]
fn cached_math_is_fast() {
    let latex = r"\sum_{n=1}^\infty \frac{1}{n^2} = \frac{\pi^2}{6}";
    let first = Instant::now();
    let a = math_svg(latex, true, &style()).unwrap();
    let cold = first.elapsed();
    let warm = Instant::now();
    for _ in 0..1000 {
        let b = math_svg(latex, true, &style()).unwrap();
        assert_eq!(a, b);
    }
    let per_call = warm.elapsed() / 1000;
    assert!(per_call < Duration::from_micros(200), "cached render took {per_call:?} (cold {cold:?})");
}

#[test]
fn mermaid_flowchart_and_sequence() {
    let flowchart = "flowchart LR\n  A[Start] --> B{Decide}\n  B -->|yes| C[Done]\n  B -->|no| A";
    let light = diagram_svg(flowchart, false).unwrap();
    assert!(light.contains("<svg") && light.contains("Decide"));
    let dark = diagram_svg(flowchart, true).unwrap();
    assert!(dark.contains("<svg"));
    assert_ne!(light, dark, "dark theme should change colours");

    let sequence = "sequenceDiagram\n  participant A as Alice\n  participant B as Bob\n  A->>B: Hello\n  B-->>A: Hi!";
    let svg = diagram_svg(sequence, false).unwrap();
    assert!(svg.contains("<svg") && svg.contains("Alice"));

    // Cached.
    let again = Instant::now();
    let _ = diagram_svg(sequence, false).unwrap();
    assert!(again.elapsed() < Duration::from_millis(5));
}

#[test]
fn mermaid_errors_are_readable() {
    for bad in ["", "   ", "flowchart LR\n  subgraph one\n  A --> B", "sequenceDiagram\n  participant A\n  A->>Nobody: hi"] {
        let err = diagram_svg(bad, false).expect_err(bad);
        let message = err.to_string();
        assert!(!message.is_empty() && message.len() < 200 && !message.contains('\n'), "{message:?}");
    }
}

#[test]
fn mermaid_never_panics_on_junk() {
    let headers = [
        "flowchart LR\n",
        "graph TD\n",
        "sequenceDiagram\n",
        "classDiagram\n",
        "stateDiagram-v2\n",
        "erDiagram\n",
        "pie\n",
        "gantt\n",
        "mindmap\n",
        "gitGraph\n",
        "timeline\n",
        "journey\n",
        "",
    ];
    let pieces = [
        "A",
        "B",
        "-->",
        "->>",
        "-.->",
        "==>",
        "|x|",
        "[",
        "]",
        "{",
        "}",
        "(",
        ")",
        "((",
        "))",
        ":",
        ";",
        "\n",
        "  ",
        "subgraph s",
        "end",
        "%%",
        "%%{init: {}}%%",
        "\"",
        "participant",
        "note over A",
        "<br>",
        "&",
        "#",
        "class",
        "title",
        "section",
        "1",
        "commit",
        "branch x",
        "é",
        "\t",
    ];
    let before = panics_so_far();
    let mut seed = 0x1234_5679_u32;
    for _ in 0..400 {
        seed ^= seed << 13;
        seed ^= seed >> 17;
        seed ^= seed << 5;
        let mut src = headers[seed as usize % headers.len()].to_owned();
        for _ in 0..12 {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            src.push_str(pieces[seed as usize % pieces.len()]);
            src.push(' ');
        }
        let _ = diagram_svg(&src, seed.is_multiple_of(2));
    }
    assert_eq!(panics_so_far(), before, "mermaid-rs-renderer panicked");
}

fn sample_options() -> PdfOptions<'static> {
    PdfOptions {
        title: "Sample".into(),
        load_image: Box::new(|path| (path == "assets/gradient.png").then(|| GRADIENT_PNG.to_vec())),
        page: PageSize::A4,
    }
}

#[test]
fn rich_markdown_becomes_a_pdf() {
    let pdf = markdown_pdf(SAMPLE, &sample_options()).unwrap();
    assert!(pdf.starts_with(b"%PDF-"));
    assert!(pdf.len() > 20_000, "only {} bytes", pdf.len());
    assert!(pdf.len() < 5_000_000, "{} bytes", pdf.len());

    let letter = PdfOptions { page: PageSize::Letter, ..sample_options() };
    let pdf = markdown_pdf("# Hello\n\nWorld", &letter).unwrap();
    assert!(pdf.starts_with(b"%PDF-"));
}

#[test]
fn missing_and_broken_images_still_compile() {
    let md = "![nothing](nope.png)\n\n![[gone.jpg]]\n\n![[not-an-image.png|200]]\n\ntext ![inline](assets/gradient.png) text";
    let opts = PdfOptions {
        title: String::new(),
        load_image: Box::new(|path| match path {
            "not-an-image.png" => Some(b"definitely not a PNG".to_vec()),
            "assets/gradient.png" => Some(GRADIENT_PNG.to_vec()),
            _ => None,
        }),
        page: PageSize::A4,
    };
    let pdf = markdown_pdf(md, &opts).unwrap();
    assert!(pdf.starts_with(b"%PDF-"));
}

#[test]
fn adversarial_text_compiles() {
    let nasty = [
        r#"# $ * _ @ < > [ ] ` \ ~ / = - + . ' " ^ { } ( ) ; : | ! ? % &"#,
        r"\\ \ \n #let x = 1 #import \u{1F600} \u{",
        "#set page(width: 1pt) #read(\"/etc/passwd\") #include \"x.typ\"",
        "// comment /* block */ https://example.com <label> @ref",
        "= Not a heading\n- not a list\n+ not an enum\n/ Term: no\n1. list\n\n2) other",
        "]]]]] [[[[ ))))) ((((( }}}}} {{{{{",
        "*unclosed _emphasis `code ~~strike",
        "$unclosed math and $$ more",
        "\u{0}\u{1}\u{7f}\u{feff}\u{202e}RTL\u{200b}zero width",
        "<script>alert(1)</script> <br> <!-- comment -->",
        "| a | b |\n|---|---|\n| 1 | 2 | 3 | 4 |\n| only one |",
        "[link](javascript:alert(1)) [ok](https://example.com/a_(b)?c=d&e=\"f\")",
        "[^undefined] and [^1]\n\n[^1]: footnote [^1] referencing itself",
        "> [!note] Title with $ and # and ]\n> body ] [",
        "```\nunclosed fence with \" and \\ and ]\n",
        "```mermaid\nnot a diagram at all ]]]\n```",
        "``` ]#\"evil lang\ncode\n```",
        "    indented code with \"quotes\" and \\backslash",
        "- [ ] task ] [\n- [x] done #\n  - [ ] nested",
        "Setext\n===\n\nOther\n---",
        "<div>\n*raw html block* with ] and #\n</div>",
        "Line with trailing backslash\\\nnext",
        "Hard break  \nafter",
        "![alt with ] and \"](weird \"title\")",
        "Ünïcödé 日本語 العربية emoji 😀 combining é",
        "---\ntitle: \"Quote \\\" ] # title\"\n---\nbody",
    ];
    let before = panics_so_far();
    for md in nasty {
        let result = markdown_pdf(md, &PdfOptions::default());
        let pdf = result.unwrap_or_else(|e| panic!("{md:?}: {e}"));
        assert!(pdf.starts_with(b"%PDF-"), "{md:?}");
    }
    let all = nasty.join("\n\n");
    assert!(markdown_pdf(&all, &PdfOptions::default()).is_ok());
    assert_eq!(panics_so_far(), before, "a dependency panicked");
}

#[test]
fn markdown_never_panics_on_junk() {
    let pieces = [
        "#", "*", "_", "`", "```", "$", "$$", "[", "]", "(", ")", "!", "[[", "]]", "|", "-", "> ", ">[!note]", "\n", "\n\n", "1. ",
        "- [ ] ", "[^a]", "[^a]: ", "<b>", "\\", "~~", "x", "  ", "---\n", "\t",
    ];
    let before = panics_so_far();
    let mut seed = 0x9e37_79b9_u32;
    for _ in 0..60 {
        let mut md = String::new();
        for _ in 0..40 {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            md.push_str(pieces[seed as usize % pieces.len()]);
        }
        let result = markdown_pdf(&md, &PdfOptions::default());
        assert!(result.is_ok(), "{md:?}: {result:?}");
    }
    assert_eq!(panics_so_far(), before, "a dependency panicked");
}
