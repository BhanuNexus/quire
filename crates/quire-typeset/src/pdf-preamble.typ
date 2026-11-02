// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//
// Page style for Markdown -> PDF. The generated document defines
// `quire-paper` and `quire-title` before this and appends the note after it.

#let ink = rgb("#1f2328")
#let muted = rgb("#59636e")
#let hairline = rgb("#d1d9e0")
#let accent = rgb("#0969da")
#let code-bg = rgb("#f6f8fa")

#set document(title: if quire-title == "" { none } else { quire-title })
#set page(
  paper: quire-paper,
  margin: (x: 2.2cm, top: 2.2cm, bottom: 2.4cm),
  footer: context align(center, text(size: 8pt, fill: muted, counter(page).display("1 / 1", both: true))),
)
#set text(font: "Inter", size: 10.5pt, fill: ink, lang: "en", hyphenate: false)
#set par(leading: 0.68em, spacing: 1.15em, justify: false)
#set strong(delta: 200)

#show heading: set text(weight: "semibold", fill: rgb("#111418"))
#show heading: set block(above: 1.5em, below: 0.75em, sticky: true)
#show heading.where(level: 1): set text(size: 1.75em)
#show heading.where(level: 2): set text(size: 1.4em)
#show heading.where(level: 3): set text(size: 1.18em)
#show heading.where(level: 4): set text(size: 1.05em)
#show heading.where(level: 5): set text(size: 1em)
#show heading.where(level: 6): set text(size: 0.95em, fill: muted)
#show heading.where(level: 1): it => block(width: 100%, stroke: (bottom: 0.6pt + hairline), inset: (bottom: 0.35em), it)
#show heading.where(level: 2): it => block(width: 100%, stroke: (bottom: 0.6pt + hairline), inset: (bottom: 0.3em), it)

// New Computer Modern has a smaller x-height than Inter; scale math to match.
#show math.equation: set text(font: "New Computer Modern Math", size: 1.1em)
#show raw: set text(font: "JetBrains Mono NL", size: 1.12em)
#show raw.where(block: false): it => box(fill: rgb("#eff1f3"), inset: (x: 3pt), outset: (y: 2.5pt), radius: 3pt, it)
#show raw.where(block: true): it => block(
  width: 100%,
  fill: code-bg,
  stroke: 0.5pt + hairline,
  inset: (x: 11pt, y: 9pt),
  radius: 5pt,
  text(size: 0.92em, it),
)
#show link: set text(fill: accent)

#set list(indent: 0.3em, body-indent: 0.55em, marker: ([•], [◦], [•], [◦]))
#set enum(indent: 0.3em, body-indent: 0.55em)
#set footnote.entry(separator: line(length: 30%, stroke: 0.5pt + hairline))
#show footnote.entry: set text(size: 8.5pt)

#set table(
  stroke: 0.5pt + hairline,
  inset: (x: 8pt, y: 5.5pt),
  fill: (_, y) => if y == 0 { code-bg },
)
#show table.cell.where(y: 0): set text(weight: "semibold")

#let md-table(aligns, ..cells) = block(above: 1.2em, below: 1.2em, table(columns: aligns.len(), align: aligns, ..cells))

#let md-quote(body) = block(
  width: 100%,
  inset: (left: 12pt, y: 3pt),
  stroke: (left: 2.5pt + hairline),
  text(fill: muted, body),
)

#let callout-colors = (
  note: rgb("#086ddd"),
  info: rgb("#086ddd"),
  todo: rgb("#086ddd"),
  abstract: rgb("#00a3a0"),
  summary: rgb("#00a3a0"),
  tldr: rgb("#00a3a0"),
  tip: rgb("#00a3a0"),
  hint: rgb("#00a3a0"),
  important: rgb("#8250df"),
  success: rgb("#1a7f37"),
  check: rgb("#1a7f37"),
  done: rgb("#1a7f37"),
  question: rgb("#d4760a"),
  help: rgb("#d4760a"),
  faq: rgb("#d4760a"),
  warning: rgb("#d4760a"),
  caution: rgb("#d1242f"),
  attention: rgb("#d4760a"),
  failure: rgb("#d1242f"),
  fail: rgb("#d1242f"),
  missing: rgb("#d1242f"),
  danger: rgb("#d1242f"),
  error: rgb("#d1242f"),
  bug: rgb("#d1242f"),
  example: rgb("#7852ee"),
  quote: rgb("#6e7781"),
  cite: rgb("#6e7781"),
)

#let callout(kind, title, body) = {
  let c = callout-colors.at(kind, default: rgb("#6e7781"))
  block(
    width: 100%,
    above: 1.2em,
    below: 1.2em,
    fill: c.transparentize(91%),
    stroke: (left: 3pt + c),
    radius: (right: 5pt),
    inset: (left: 13pt, right: 12pt, y: 10pt),
    {
      text(fill: c.darken(15%), weight: "semibold", title)
      if body != [] {
        v(0.45em, weak: true)
        body
      }
    },
  )
}

// Task items hang their checkbox (or bullet) into the list's indent, so the
// text, wrapped lines and nested lists all line up after it.
#let task-gutter = 1.32em
#let checkbox(checked) = {
  h(-task-gutter)
  box(
    width: 0.82em,
    height: 0.82em,
    baseline: 0.12em,
    radius: 2pt,
    stroke: 0.7pt + (if checked { accent } else { muted }),
    fill: if checked { accent } else { none },
    if checked {
      place(curve(
        stroke: (paint: white, thickness: 1.3pt, cap: "round", join: "round"),
        curve.move((0.19em, 0.43em)),
        curve.line((0.35em, 0.6em)),
        curve.line((0.64em, 0.24em)),
      ))
    },
  )
  h(0.5em)
}
#let task-list(tight, ..items) = list(tight: tight, marker: [], body-indent: task-gutter, ..items)
#let task-bullet = {
  h(-task-gutter)
  box(width: 0.82em, align(center, [•]))
  h(0.5em)
}

#let md-rule() = block(above: 1.4em, below: 1.4em, line(length: 100%, stroke: 0.6pt + hairline))

#let md-image(path, natural, alt: "") = block(
  above: 1em,
  below: 1em,
  width: 100%,
  align(center, layout(size => image(path, width: calc.min(natural, size.width), alt: if alt == "" { none } else { alt }))),
)
#let missing-image(name) = text(fill: muted, style: "italic", size: 0.92em)[\[#name\]]

#let md-diagram(path, natural) = block(
  above: 1.2em,
  below: 1.2em,
  width: 100%,
  align(center, layout(size => image(path, width: calc.min(natural, size.width)))),
)
#let diagram-error(message) = text(fill: rgb("#d1242f"), size: 0.85em, message)

#let math-error(src) = box(
  fill: rgb("#fff1f0"),
  inset: (x: 3pt),
  outset: (y: 2.5pt),
  radius: 3pt,
  text(font: "JetBrains Mono NL", size: 0.9em, fill: rgb("#d1242f"), src),
)

#let title-block(title) = block(below: 1.6em, {
  text(size: 2.1em, weight: "semibold", fill: rgb("#111418"), title)
})
