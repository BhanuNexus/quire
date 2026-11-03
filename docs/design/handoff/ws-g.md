# WS-G hand-off: spatial and reading

What WS-G (canvas and ink, PDFs, citations) touched outside its own files and
why, what other workstreams can now use, and what it asks of them.

## Touched outside WS-G's files (smallest possible additions)

Each is marked `WS-G` in a comment at the place.

| File | Change | Why |
|---|---|---|
| `crates/quire-app/src/workspace.rs` | `open_link`: 3 lines calling `canvas_state::open_file_link` first; `follow_link_at_caret`: `None => citations::follow_at_caret(self)`; `export_active_html`: the text goes through `citations::render_source` | `[[paper.pdf#page=3&annot=…]]` and `[[Board.canvas]]` used to create a note named "paper.pdf"; Ctrl+click and Alt+Enter on `[@key]` open the work; HTML export renders citations (§G.3) |
| `crates/quire-app/src/suggest.rs` | `SuggestKind::Cite`, and a 5-line hook in `suggestions()` calling `citations::suggestions` | typing `[@` offers works from the library (§G.3); the editor shows them with the existing list |
| `crates/quire-app/src/publish.rs` | 2 lines: the library is read once, each note goes through `citations::render_with` | publish renders citations |
| `crates/quire-app/src/export_pdf.rs` | 3 lines: `citations::for_pdf` gives the Markdown with citation placeholders and `PdfOptions::bibliography` | Typst bibliography in the PDF export |
| `crates/quire-typeset/src/lib.rs` | the `pub use markdown::{…}` line also exports `Bibliography`, `BibCite`, `CiteForm`, `cite_placeholder`, `BIB_STYLES` | the bibliography types live in `markdown.rs` (WS-G's part of it) |
| `crates/quire-markdown/src/lib.rs` | `pub mod cite;` | the design's new module |
| `apps/quire-desktop/src/panels.rs` | `preview`: 1 line, the text goes through `citations::render_source` | `[@key]` renders in preview |
| `apps/quire-desktop/src/app.rs` | 1 line after `install_image_loaders`: `canvas_ui::install_sketch_loader` | sketches embedded in notes (`![[Note-ink-1.svg]]`) show as pictures in both editions; egui has no SVG loader here |
| `apps/quire-desktop/src/surfaces.rs` | `#[expect(dead_code)]` on `SystemViewer` | `.canvas` and `.pdf` now have editors, so the placeholder is unused; kept for the next file type |
| `apps/quire-desktop/src/main.rs` | `mod canvas_ui; mod cite_ui; mod pdf_ui;` | the new files |
| `apps/quire-cli/src/commands/notes.rs` | `export` (HTML): 2 lines, `citations::render_with_vault` | same output as the app's HTML export |
| `crates/quire-mcp/tests/fixtures/mcp-0.9-*.jsonl` | re-recorded (`QUIRE_BLESS=1`) | `tools/list` now includes `canvas_read` and `canvas_apply` (registered in WS-G's line of `ToolRegistry::standard`) |
| `docs/formats/README.md`, `vault-prefs.md` | WS-G rows and the `[citations]` section | new and changed files |

Registration blocks used: `registry.rs` (WS-G blocks of `FeatureUi`,
`SETTINGS_TABS` → Citations, `EDITOR_BANNERS` → canvas review bar,
`ui_target` → citation picker and import dialog, `open_surface` → `.canvas`,
`.pdf`, `.svg`, `layout_samples` → four dialog states); Cargo markers in
`quire-app` and the desktop (`quire-canvas`, `quire-pdf`, `biblatex`, and
`quire-pdf/render` in both `typeset` features).

## Dependencies

* **`kurbo` 0.13** is a direct (optional) dependency of `quire-pdf`: the
  geometry types of hayro-interpret's `Device` trait are kurbo's and aren't
  re-exported. It is already in the lock through hayro, so nothing new is
  built. **Integrator:** move it to `[workspace.dependencies]` if you prefer
  (`kurbo = "0.13"`), then `kurbo = { workspace = true, optional = true }`.
* `biblatex` (already a workspace dependency) is now in both editions
  through `quire-app`.

## For WS-C (search)

* PDF text reaches the index as `IndexMsg::UpsertAttachment` with one string
  per page (`pages[0]` is page 1), `kind: Pdf`, `by: Embedded` (or `Ocr`
  once a scanned page was read). The job re-sends everything after a vault
  opens; `RemoveAttachment` when a PDF is deleted.
* **Please show attachment hits as "*paper.pdf* p. 3"** (§G.2) and open them
  at the page with `ws.open_link("paper.pdf", Some("page=3"))` (or
  `quire_app::pdf_state::open_link(ws, &path, Some("page=3"))`): the full
  edition opens the viewer at the page, Lite the PDF's text.

## For WS-D (on-device OCR)

* Pages with no text whose images cover at least half the page are listed
  in `ws.features.pdf.texts[path].image_only` (1-based) and queued in
  `ws.features.pdf.ocr_queue` when `settings.ai_local.ocr` is on.
  `pdf_state::queue_ocr(ws)` queues them all (call it when OCR is turned on).
* Take requests with `pdf_state::take_ocr_requests(ws, n)`. Each
  `OcrRequest { path, page /* 0-based */, sha256 }` maps to
  `quire_ai::protocol::OcrParams { pdf: Some(PdfPage { path: <absolute path>, page, dpi: 200 }), .. }`.
* Hand back the text with `pdf_state::ocr_result(ws, &req, &text)`: it goes
  into the cache and the index (`TextSource::Ocr`). A result for a PDF that
  changed meanwhile (different `sha256`) is dropped.

## For the integrator / WS-F (editor)

* **Hover over a citation** (§G.3 asks for the formatted reference on
  hover). The editor has no hover hook for features, so today the status bar
  shows the work under the caret ("📚 Lovelace 1843 · Notes on …", the full
  reference as its tooltip, a click opens the picker). A tooltip in
  `view/mod.rs` where the pointer's char is known would be:
  `quire_markdown::cite::at(line_text, byte_in_line)` →
  `ws.features.citations.library.get(&key)` and
  `quire_markdown::cite::Formatter::reference(&*library, &key)`.
* **CI:** run `cargo test -p quire-pdf --features render` too (the render
  tests are behind the feature; the desktop's full build enables it).
* The PDF surface takes Ctrl+F for its own find box (it closes the note's
  find bar when it opens while the PDF is in front).

## Notes for everyone

* `quire_app::canvas_state::{canvas, pdf}` re-export `quire_canvas` and
  `quire_pdf` for crates above `quire-app`.
* `quire_canvas::ops::{apply, diff}`: canvas edits as data and a node-level
  description of two versions; `quire_canvas::ink::rasterize` draws a sketch
  to RGBA without a renderer.
* `quire_app::citations::render_source(ws, text)` (and `render_with`,
  `render_with_vault`) for any other place that renders notes.
* Canvas, sketch and PDF surfaces save themselves (0.6 s after a change and
  when closed) and take in changes from elsewhere through
  `ws.features.canvas.disk_generation(path)`, bumped on `AttachmentChanged`.
