// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Writes a sample PDF and a few SVGs for eyeballing the output:
//!
//! ```text
//! cargo run -p quire-typeset --example render_samples -- <output-dir>
//! ```

use std::path::PathBuf;
use std::time::Instant;

use quire_typeset::{MathStyle, PageSize, PdfOptions, diagram_svg, markdown_pdf, math_svg};

const SAMPLE: &str = include_str!("../tests/fixtures/sample.md");
const GRADIENT_PNG: &[u8] = include_bytes!("../tests/fixtures/gradient.png");

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let dir = PathBuf::from(std::env::args().nth(1).ok_or("usage: render_samples <output-dir>")?);
    std::fs::create_dir_all(&dir)?;

    let opts = PdfOptions {
        title: "Sample".into(),
        load_image: Box::new(|path| (path == "assets/gradient.png").then(|| GRADIENT_PNG.to_vec())),
        page: PageSize::A4,
        bibliography: None,
    };
    let started = Instant::now();
    let pdf = markdown_pdf(SAMPLE, &opts)?;
    println!("sample.pdf: {} bytes in {:?}", pdf.len(), started.elapsed());
    std::fs::write(dir.join("sample.pdf"), pdf)?;

    let style = MathStyle { size_pt: 20.0, color: [0x1f, 0x23, 0x28, 0xff] };
    let formulas = [
        ("math-inline.svg", r"e^{i\pi} + 1 = 0", false),
        ("math-display.svg", r"\int_0^\infty e^{-x^2}\,dx = \frac{\sqrt{\pi}}{2}", true),
        ("math-matrix.svg", r"\left( \begin{bmatrix} a & b \\ c & d \end{bmatrix} \right)^{-1}", true),
    ];
    for (name, latex, display) in formulas {
        let rendered = math_svg(latex, display, &style)?;
        println!("{name}: {:.1} x {:.1} pt, baseline {:.1} pt", rendered.width_pt, rendered.height_pt, rendered.baseline_pt);
        std::fs::write(dir.join(name), rendered.svg)?;
    }

    let flowchart =
        "flowchart TD\n  A[Write a note] --> B{Has math?}\n  B -->|yes| C[Typst]\n  B -->|no| D[Plain text]\n  C --> E[PDF]\n  D --> E";
    std::fs::write(dir.join("diagram-light.svg"), diagram_svg(flowchart, false)?)?;
    std::fs::write(dir.join("diagram-dark.svg"), diagram_svg(flowchart, true)?)?;
    let sequence = "sequenceDiagram\n  Alice->>Bob: Hello Bob\n  Bob-->>Alice: Hi Alice";
    std::fs::write(dir.join("diagram-sequence.svg"), diagram_svg(sequence, false)?)?;
    println!("wrote samples to {}", dir.display());
    Ok(())
}
