// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//
// Typst definitions for the names `mitex` (LaTeX -> Typst math) emits that are
// not part of Typst's own math scope. This plays the role of the scope in the
// `@preview/mitex` Typst package, written from scratch so Quire needs neither
// the package registry nor the network.

// ---- helpers ---------------------------------------------------------------

// Plain text of converted LaTeX ("r e d" -> "red"); spaces are dropped.
#let to-str(it) = {
  if it == none {
    ""
  } else if type(it) == str {
    it
  } else if type(it) == content {
    if it.has("text") {
      it.text
    } else if it.has("children") {
      it.children.map(to-str).sum(default: "")
    } else if it.has("body") {
      to-str(it.body)
    } else {
      ""
    }
  } else {
    str(it)
  }
}

#let join-args(args) = args.pos().sum(default: [])

// `\[3 \]` (an optional LaTeX argument) -> its inside.
#let strip-brackets(it) = {
  if type(it) != content or not it.has("children") {
    return it
  }
  let ch = it.children.filter(c => c != [ ])
  if ch.len() >= 2 and ch.first().has("text") and ch.first().text == "[" and ch.last().has("text") and ch.last().text == "]" {
    ch.slice(1, ch.len() - 1).sum(default: [])
  } else {
    it
  }
}

#let to-length(it) = {
  let s = to-str(it).trim()
  let m = s.match(regex("^(-?[0-9]*\.?[0-9]+)(pt|bp|px|em|ex|mu|mm|cm|in)?$"))
  if m == none {
    return 0pt
  }
  let v = float(m.captures.at(0))
  let u = m.captures.at(1)
  if u == none or u == "pt" or u == "bp" or u == "px" {
    v * 1pt
  } else if u == "em" {
    v * 1em
  } else if u == "ex" {
    v * 0.5em
  } else if u == "mu" {
    v / 18 * 1em
  } else if u == "mm" {
    v * 1mm
  } else if u == "cm" {
    v * 1cm
  } else {
    v * 1in
  }
}

// xcolor's base colours, plus `#rrggbb` and `name!percent` mixes with white.
#let color-names = (
  black: rgb("#000000"),
  white: rgb("#ffffff"),
  red: rgb("#ff0000"),
  green: rgb("#00ff00"),
  blue: rgb("#0000ff"),
  cyan: rgb("#00ffff"),
  magenta: rgb("#ff00ff"),
  yellow: rgb("#ffff00"),
  gray: rgb("#808080"),
  grey: rgb("#808080"),
  darkgray: rgb("#404040"),
  lightgray: rgb("#bfbfbf"),
  brown: rgb("#bf8040"),
  lime: rgb("#bfff00"),
  olive: rgb("#808000"),
  orange: rgb("#ff8000"),
  pink: rgb("#ffbfbf"),
  purple: rgb("#bf0040"),
  teal: rgb("#008080"),
  violet: rgb("#800080"),
)

#let to-color(it) = {
  let s = lower(to-str(it).trim())
  let parts = s.split("!")
  let base = parts.at(0)
  let c = if base.match(regex("^#?[0-9a-f]{6}$")) != none {
    rgb(if base.starts-with("#") { base } else { "#" + base })
  } else {
    color-names.at(base, default: none)
  }
  if c != none and parts.len() >= 2 and parts.at(1).match(regex("^[0-9]{1,3}$")) != none {
    let pct = calc.min(int(parts.at(1)), 100)
    c = color.mix((c, pct * 1%), (white, (100 - pct) * 1%))
  }
  c
}

#let colored(c, body) = if c == none { body } else { text(fill: c, body) }

// ---- roots, styles, fractions ---------------------------------------------

#let mitexsqrt(..args) = {
  let a = args.pos()
  if a.len() >= 2 {
    let degree = strip-brackets(a.at(0))
    if degree == [] { math.sqrt(a.at(1)) } else { math.root(degree, a.at(1)) }
  } else if a.len() == 1 {
    math.sqrt(a.at(0))
  } else {
    math.sqrt([])
  }
}

#let mitexmathbf(it) = math.bold(math.upright(it))
#let mitexbold(..args) = math.bold(math.upright(join-args(args)))
#let mitexupright(..args) = math.upright(join-args(args))
#let mitexitalic(..args) = math.italic(join-args(args))
#let mitexcal(..args) = math.cal(join-args(args))
#let mitexfrak(..args) = math.frak(join-args(args))
#let mitexmono(..args) = math.mono(join-args(args))
#let mitexsans(..args) = math.sans(join-args(args))
#let mitexdisplay(..args) = math.display(join-args(args))
#let mitexinline(..args) = math.inline(join-args(args))
#let mitexscript(..args) = math.script(join-args(args))
#let mitexsscript(..args) = math.sscript(join-args(args))

// Infix `a \atop b`, `n \brace k`, `n \brack k`.
#let atop(a, b) = math.vec(delim: none, a, b)
#let brace(a, b) = math.vec(delim: "{", a, b)
#let brack(a, b) = math.vec(delim: "[", a, b)

#let dfrac(a, b) = math.display(math.frac(a, b))
#let tfrac(a, b) = math.inline(math.frac(a, b))
#let cfrac(a, b) = math.display(math.frac(a, b))
#let dbinom(a, b) = math.display(math.binom(a, b))
#let tbinom(a, b) = math.inline(math.binom(a, b))

// ---- operators -------------------------------------------------------------

#let operatorname(it) = math.op(to-str(it))
#let operatornamewithlimits(it) = math.op(to-str(it), limits: true)
#let argmax = math.op("arg max", limits: true)
#let argmin = math.op("arg min", limits: true)
#let injlim = math.op("inj lim", limits: true)
#let projlim = math.op("proj lim", limits: true)
#let plim = math.op("plim", limits: true)
#let cosec = math.op("cosec")
#let cotg = math.op("cotg")
#let ch = math.op("ch")
#let sh = math.op("sh")
#let th = math.op("th")
#let cth = math.op("cth")
#let arctg = math.op("arctg")
#let arcctg = math.op("arcctg")
#let smallint = math.inline(sym.integral)

#let mathop(it) = math.limits(math.class("large", it), inline: false)
#let mathbin(it) = math.class("binary", it)
#let mathrel(it) = math.class("relation", it)
#let mathord(it) = math.class("normal", it)
#let mathopen(it) = math.class("opening", it)
#let mathclose(it) = math.class("closing", it)
#let mathpunct(it) = math.class("punctuation", it)
#let mathinner(it) = it
#let mathclap(it) = it

#let pmod(it) = $quad (mod it)$
#let pod(it) = $quad (it)$

// ---- accents and decorations ----------------------------------------------

#let mitexoverbrace(it) = math.limits(math.overbrace(it))
#let mitexunderbrace(it) = math.limits(math.underbrace(it))
#let mitexoverbracket(it) = math.limits(math.overbracket(it))
#let mitexunderbracket(it) = math.limits(math.underbracket(it))
#let overgroup(it) = math.overparen(it)
#let undergroup(it) = math.underparen(it)
#let overlinesegment(it) = math.overline(it)
#let underbar(it) = math.underline(it)
#let mathring(it) = math.accent(it, "\u{2DA}")
#let overleftharpoon(it) = math.accent(it, "\u{21BC}")
#let overrightharpoon(it) = math.accent(it, "\u{21C0}")
#let overleftrightarrow(it) = math.accent(it, "\u{2194}")
#let bcancel(it) = math.cancel(it, inverted: true)
#let xcancel(it) = math.cancel(it, cross: true)
#let sout(it) = math.cancel(it, angle: 90deg)
#let boxed(it) = box(stroke: 0.5pt, inset: (x: 3pt, y: 2pt), $it$)
#let fbox(it) = boxed(it)
#let colorbox(c, it) = box(fill: to-color(c), inset: (x: 3pt, y: 2pt), $it$)
#let mitexcolor(..args) = {
  let a = args.pos()
  if a.len() == 0 { [] } else { colored(to-color(a.at(0)), a.slice(1).sum(default: [])) }
}
#let colortext(c, it) = colored(to-color(c), it)

#let overset(a, b) = math.attach(math.limits(b), t: a)
#let underset(a, b) = math.attach(math.limits(b), b: a)
#let stackrel(a, b) = overset(a, b)
#let substack(it) = it

#let xarrow(s) = (..args) => {
  let a = args.pos()
  if a.len() >= 2 {
    math.attach(math.stretch(s), t: a.at(1), b: strip-brackets(a.at(0)))
  } else if a.len() == 1 {
    math.attach(math.stretch(s), t: a.at(0))
  } else {
    math.stretch(s)
  }
}
#let xleftarrow = xarrow(sym.arrow.l)
#let xrightarrow = xarrow(sym.arrow.r)
#let xLeftarrow = xarrow(sym.arrow.l.double)
#let xRightarrow = xarrow(sym.arrow.r.double)
#let xleftrightarrow = xarrow(sym.arrow.l.r)
#let xLeftrightarrow = xarrow(sym.arrow.l.r.double)
#let xhookleftarrow = xarrow(sym.arrow.l.hook)
#let xhookrightarrow = xarrow(sym.arrow.r.hook)
#let xmapsto = xarrow(sym.arrow.r.bar)
#let xtwoheadleftarrow = xarrow(sym.arrow.l.twohead)
#let xtwoheadrightarrow = xarrow(sym.arrow.r.twohead)
#let xleftharpoonup = xarrow(sym.harpoon.lt)
#let xleftharpoondown = xarrow(sym.harpoon.lb)
#let xrightharpoonup = xarrow(sym.harpoon.rt)
#let xrightharpoondown = xarrow(sym.harpoon.rb)
#let xleftrightharpoons = xarrow(sym.harpoons.ltrb)
#let xrightleftharpoons = xarrow(sym.harpoons.rtlb)
#let xlongequal = xarrow(sym.eq)
#let xtofrom = xarrow(sym.arrows.rl)

// ---- delimiters ------------------------------------------------------------

#let big(it) = math.lr(it, size: 1.2em)
#let Big(it) = math.lr(it, size: 1.8em)
#let bigg(it) = math.lr(it, size: 2.4em)
#let Bigg(it) = math.lr(it, size: 3em)
#let middle(it) = math.mid(it)

#let bra(it) = $lr(chevron.l it |)$
#let ket(it) = $lr(| it chevron.r)$
#let braket(it) = $lr(chevron.l it chevron.r)$
// `set` is a Typst keyword; mitex.typ exposes this one under that name.
#let set-braces(it) = $lr(\{ it \})$
#let Bra = bra
#let Ket = ket
#let Braket = braket
#let Set = set-braces

// ---- spacing ---------------------------------------------------------------

#let negthinspace = h(-3em / 18)
#let negthinmedspace = h(-3em / 18)
#let negmedspace = h(-4em / 18)
#let negthickspace = h(-5em / 18)
#let enspace = h(0.5em)
#let hspace(it) = h(to-length(it))
#let vspace(it) = []
#let phantom(it) = hide(it)
#let hphantom(it) = box(height: 0pt, hide(it))
#let vphantom(it) = box(width: 0pt, hide(it))
#let raisebox(len, it) = move(dy: -to-length(len), it)
#let textwidth = []

// ---- environments ----------------------------------------------------------

#let matrix-of(delim, small: false) = (..args) => {
  let rows = args.pos()
  if rows.len() > 0 and type(rows.at(0)) != array {
    rows = (rows,)
  }
  let m = math.mat(delim: delim, ..rows)
  if small { math.script(m) } else { m }
}
#let matrix = matrix-of(none)
#let pmatrix = matrix-of("(")
#let bmatrix = matrix-of("[")
#let Bmatrix = matrix-of("{")
#let vmatrix = matrix-of("|")
#let Vmatrix = matrix-of("‖")
#let smallmatrix = matrix-of(none, small: true)

#let mitexarray(..args) = {
  let spec = to-str(args.named().at("arg0", default: none))
  let first = spec.clusters().find(c => c in ("l", "c", "r"))
  let align = if first == "l" { left } else if first == "r" { right } else { center }
  let rows = args.pos()
  if rows.len() > 0 and type(rows.at(0)) != array {
    rows = (rows,)
  }
  math.mat(delim: none, align: align, ..rows)
}

#let aligned(..args) = join-args(args)
#let alignedat(..args) = join-args(args)
#let rcases(..args) = math.cases(reverse: true, ..args)

// ---- text ------------------------------------------------------------------

#let textmath(it) = it
#let textrm(it) = it
#let textup(it) = it
#let textnormal(it) = it
#let textmd(it) = it
// The math font has no bold or italic text faces; use the body font's.
#let textbf(it) = text(font: "Inter", weight: "semibold", it)
#let textit(it) = text(font: "Inter", style: "italic", it)
#let textsf(it) = text(font: "Inter", it)
#let texttt(it) = text(font: "JetBrains Mono NL", it)
#let hbox(it) = math.upright(it)
#let TeX = math.upright("TeX")
#let LaTeX = math.upright("LaTeX")
#let KaTeX = math.upright("KaTeX")

// Ignored in standalone formulas.
#let mitexlabel(it) = $quad (it)$
#let mitexcaption(..args) = []
#let mitexcite(..args) = []
#let mitexref(..args) = []
#let miteximage(..args) = []
#let toprule = []
#let midrule = []
#let bottomrule = []

// Italic capital Greek.
#let varGamma = math.italic(sym.Gamma)
#let varDelta = math.italic(sym.Delta)
#let varTheta = math.italic(sym.Theta)
#let varLambda = math.italic(sym.Lambda)
#let varXi = math.italic(sym.Xi)
#let varPi = math.italic(sym.Pi)
#let varSigma = math.italic(sym.Sigma)
#let varUpsilon = math.italic(sym.Upsilon)
#let varPhi = math.italic(sym.Phi)
#let varPsi = math.italic(sym.Psi)
#let varOmega = math.italic(sym.Omega)
#let thickapprox = math.bold(sym.approx)
#let thicksim = math.bold(sym.tilde.op)
