# PDF annotations: `<pdf folder>/<pdf stem> (annotations).md`

Highlights on `paper.pdf` live in a Markdown note beside it, readable without
Quire and in Lite. Owner: WS-G. Helpers: `quire_pdf::annot`.

```markdown
---
annotates: "[[paper.pdf]]"
pdf_sha256: 3fa1…
type: annotations
---
> [!quote] p. 3 · [[paper.pdf#page=3&annot=h7f3a]]
> Ropes are balanced trees of strings.
^h7f3a

My comment.
<!-- quire:pdf-anchor {"p":3,"q":[[72.0,512.3,301.2,524.9]],"t":"Ropes are balanced trees of strings.","pre":"…","suf":"…","c":"yellow"} -->
```

`pdf-anchor` JSON: `p` page (1-based), `q` rectangles in PDF points
`[x0, y0, x1, y1]`, `t` the highlighted text, `pre`/`suf` up to 32
characters of context, `c` the colour. Unknown keys are kept. When the PDF
changes (`pdf_sha256` differs), highlights are found again by text near
their page.
