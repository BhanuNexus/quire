// SPDX-FileCopyrightText: 2026 The Quire contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//
// Evaluates LaTeX math that `mitex` already converted to Typst math syntax.
// The converted code is passed as a string and evaluated in math mode, so it
// can never escape into the surrounding markup.

#import "/mitex-defs.typ" as mitex-defs

#let mitex-scope = dictionary(mitex-defs) + ("set": mitex-defs.set-braces)

#let mitex-math(src, block: false) = math.equation(
  block: block,
  numbering: none,
  eval(src, mode: "math", scope: mitex-scope),
)
