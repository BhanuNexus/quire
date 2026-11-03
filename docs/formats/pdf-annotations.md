# PDF annotations: `<pdf folder>/<pdf stem> (annotations).md`

Highlights on `paper.pdf` live in a Markdown note beside it, readable without
Quire and in Lite. Owner: WS-G. Helpers: `quire_pdf::annot`.

```markdown
---
annotates: "[[paper.pdf]]"
pdf_sha256: 3fa1…
type: annotations
---
# Highlights: paper

> [!quote] p. 3 · [[paper.pdf#page=3&annot=h7f3a]]
> Ropes are balanced trees of strings.
^h7f3a

My comment.
<!-- quire:pdf-anchor {"p":3,"q":[[72.0,512.3,301.2,524.9]],"t":"Ropes are balanced trees of strings.","pre":"…","suf":"…","c":"yellow"} -->
```

* `pdf-anchor` JSON: `p` page (1-based), `q` rectangles in PDF points
  `[x0, y0, x1, y1]` (PDF user space, origin bottom-left; one per line of
  the highlight), `t` the highlighted text, `pre`/`suf` up to 32 characters
  of context, `c` the colour (`yellow`, `green`, `blue`, `pink`, `purple`).
  Unknown keys are kept.
* **Area highlights** have an empty `t` and one rectangle. Their callout
  shows a picture of the area, saved in the attachments folder
  (`<stem>-p<page>.png`), as `> ![[paper-p3.png]]`.
* An entry is the callout, its `^id` line, the comment, and the anchor
  comment. Quire changes only the entry it is about (a new comment, a new
  place after re-anchoring, removal); the rest of the note, including text
  written around the entries, stays as it is. Ids are `h` and four hex
  digits.
* **Links:** `[[paper.pdf#page=3&annot=h7f3a]]` opens the PDF at the
  highlight (full edition), or the annotation note at `^h7f3a` (Lite).
  `#page=3` alone opens the page.
* **Re-anchoring:** when the PDF's SHA-256 differs from `pdf_sha256`, each
  highlight's text is searched for on its old page ± 2 (case and white space
  ignored); among several matches, the one whose surroundings match
  `pre`/`suf` best wins. Found highlights get new rectangles and page;
  `pdf_sha256` is updated. Highlights not found keep their old place and
  are listed as "detached".

# PDF text cache: `.quire/cache/pdf-text/<sha256>.json`

Rebuildable; safe to delete. One file per PDF content (SHA-256 of the
bytes, hex), written by the `pdf-text` job:

```json
{"format":1,"sha256":"3fa1…","pages":["text of page 1","…"],"image_only":[4],"ocr":[4]}
```

* `pages`: each page's text, lines rebuilt from glyph positions.
* `image_only`: 1-based pages with no text whose images cover at least half
  the page (scans); queued for OCR when on-device OCR is on.
* `ocr`: pages whose text came from OCR.
* The index gets the pages as an attachment document
  (`IndexMsg::UpsertAttachment`, `TextSource::Embedded` or `Ocr`).
