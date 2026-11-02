# Quire — Architecture

Quire is a fast, local‑first Markdown notepad written in Rust. It aims to combine
the zero‑friction capture of Notepad / Apple Notes, the writing feel of Typora / Bear /
iA Writer, the linked‑knowledge model of Obsidian / Logseq, and private, on‑device
"intelligence" — without Electron, without a database lock‑in, and without a network
round‑trip for anything that matters.

---

## 1. Goals and non‑goals

| Goal | Concrete target |
|---|---|
| Instant | Keystroke → frame < 16 ms for typical notes (highlighting 100 KB takes ~2.5 ms); files above 512 KB open in "plain mode"; a 10 MB rope builds in ~9 ms |
| Plain files | Notes are `.md` files in an ordinary folder ("vault"). Quire never owns your data |
| Linked | `[[wikilinks]]`, backlinks, unlinked mentions, tags (`#a/b`), rename‑with‑link‑rewrite |
| Intelligent, privately | Related notes, link suggestions, tag suggestions, summaries, inline calculator, natural dates — all local and deterministic. Remote LLM optional, opt‑in, behind a trait |
| Swappable UI | All behaviour lives below the UI. The egui desktop app is one adapter; a GPUI or TUI frontend can replace it without touching the core |
| Testable | Every core crate is pure or I/O‑behind‑a‑trait, with an in‑memory adapter for tests |

Non‑goals for v1: real‑time multi‑user collaboration, a block database à la Notion,
mobile apps. The architecture leaves seams for each (see §9).

---

## 2. Layered architecture (ports & adapters)

```
 ┌──────────────────────────────── Frontends (adapters) ───────────────────────────────┐
 │  quire-desktop (egui/eframe)          quire-cli               future: GPUI, TUI, web │
 └───────────────▲───────────────────────────▲───────────────────────────────────────────┘
                 │ Commands / Actions        │
 ┌───────────────┴───────────────────────────┴───────────── Application ─────────────────┐
 │  quire-app   Workspace · Document · CommandRegistry · Settings · Session · Events     │
 └──────▲──────────────▲──────────────────▲────────────────────▲────────────────────────┘
        │              │                  │                    │
 ┌──────┴─────┐ ┌──────┴───────┐ ┌────────┴────────┐ ┌─────────┴─────────┐   Services
 │ quire-vault│ │ quire-index  │ │  quire-intel    │ │ quire-markdown    │
 │ Storage    │ │ full‑text,   │ │ related, links, │ │ syntax, outline,  │
 │ port, notes│ │ fuzzy, graph │ │ summary, calc,  │ │ extract, render,  │
 │ templates, │ │ (actor thread)│ │ dates, LLM port │ │ smart editing     │
 │ refactors  │ └──────▲───────┘ └────────▲────────┘ └─────────▲─────────┘
 └──────▲─────┘        │                  │                    │
        │              └──────────┬───────┴────────────────────┘
 ┌──────┴─────────────────────────┴──────────────────────────────────── Domain ──────────┐
 │  quire-text   Rope buffer · Change · Transaction · Selection · History (undo tree)     │
 └────────────────────────────────────────────────────────────────────────────────────────┘
```

Dependency rule: arrows point **down only**. `quire-text` depends on nothing in the
workspace; frontends depend on `quire-app` and never on each other.

---

## 3. Folder structure

```
quire/
├── Cargo.toml                 # workspace, shared deps & lints, release profile
├── ARCHITECTURE.md            # this file
├── README.md
├── crates/
│   ├── quire-text/            # DOMAIN: text model, no I/O
│   │   └── src/ lib.rs  buffer.rs  change.rs  selection.rs  history.rs  motion.rs  multi.rs
│   ├── quire-markdown/        # pure Markdown intelligence
│   │   └── src/ lib.rs  syntax.rs  blocks.rs  outline.rs  extract.rs  frontmatter.rs
│   │            render.rs  editing.rs  table.rs
│   ├── quire-vault/           # storage port + local/in‑memory adapters
│   │   └── src/ lib.rs  storage.rs  note.rs  vault.rs  templates.rs  refactor.rs  watch.rs
│   ├── quire-index/           # search + link graph, runs as an actor
│   │   └── src/ lib.rs  tokenize.rs  record.rs  fulltext.rs  fuzzy.rs  graph.rs  actor.rs
│   │            chunk.rs  semantic.rs  hybrid.rs
│   ├── quire-intel/           # local intelligence + optional LLM port
│   │   └── src/ lib.rs  related.rs  mentions.rs  summary.rs  tags.rs  calc.rs  dates.rs  llm.rs  ask.rs
│   ├── quire-typeset/         # Typst as a library: LaTeX math → SVG (MiTeX), Mermaid → SVG, Markdown → PDF
│   ├── quire-spell/           # Markdown-aware spell checking (spellbook, bundled en_US/en_GB, system Hunspell)
│   └── quire-app/             # application layer, UI‑agnostic
│       └── src/ lib.rs  document.rs  workspace.rs  commands.rs  settings.rs  session.rs
│                find.rs  find_state.rs  attach.rs  ask_state.rs
├── apps/
│   ├── quire-desktop/         # egui frontend
│   │   └── src/ main.rs  app.rs  editor.rs (classic)  highlight.rs  overlays.rs  panels.rs  theme.rs
│   │            view/ mod.rs (fast virtualized editor)  heights.rs
│   └── quire-cli/             # headless frontend: index, search, export, bench
│       └── src/ main.rs
└── sample-vault/              # demo notes used by tests and screenshots
```

---

## 4. Crate responsibilities

### quire-text (domain)
* `Buffer` wraps a `ropey::Rope` → O(log n) insert/delete, cheap clones (structural
  sharing), char/byte/line conversions.
* `Change { range, insert }` and `Transaction { changes, selection_after }`.
  Every edit, from a keystroke to a vault‑wide link rename, is a Transaction.
* `Transaction::invert(&buffer)` produces the exact inverse → undo without snapshots.
* `History` is an **undo tree** (Vim/Emacs‑undo‑tree style): undoing then typing does
  not destroy the old branch. Consecutive typing within 500 ms is coalesced into one
  undo step.

### quire-markdown (pure)
* `syntax::highlight(&str) -> Vec<Span>` – byte ranges + `SpanKind` (heading level,
  strong, emphasis, code, link, wikilink, tag, task, quote, list marker, …). Built on
  `pulldown-cmark`'s offset iterator plus a scanner for Obsidian extensions
  (`[[links]]`, `#tags`, `==highlight==`).
* `outline`, `extract` (links, wikilinks, tags, tasks, word stats), `frontmatter`
  (YAML subset, zero‑dep).
* `render::to_html` – GFM (tables, task lists, footnotes, strikethrough) + wikilink
  resolution, used for preview/export.
* `editing` – *smart edits* as functions `(&Buffer, Selection) -> Option<Transaction>`:
  list/quote/task continuation on Enter, exit empty item, indent/outdent list,
  toggle task, wrap with `**`/`_`/`` ` ``, and `table::format` to realign pipe tables.

### quire-vault (port + adapters)
* `trait Storage` — `list`, `read`, `write_atomic`, `rename`, `remove`, `exists`.
  Adapters: `LocalFs` (temp file + rename for crash safety) and `MemFs` (tests).
* `Vault` – note discovery, unique naming, daily notes, templates
  (`{{title}} {{date}} {{time}}`), and **refactors**: rename a note and rewrite every
  `[[Old]]`, `[[Old|alias]]`, `[[Old#heading]]` in the vault as one batch.
* `watch` – `notify` watcher that emits `VaultEvent`s for external edits.

### quire-index (actor)
* `NoteRecord` – derived, rebuildable facts about a note (title, headings, links,
  tags, tasks, term frequencies).
* `FullText` – inverted index, BM25 ranking, prefix matching, snippet extraction.
* `Fuzzy` – quick‑open over titles/paths with `nucleo-matcher` (the fzf‑class matcher
  used by Helix).
* `LinkGraph` – forward links, backlinks, unresolved links, tag counts, orphans.
* `IndexActor` – owns the index on a worker thread; the UI sends `IndexMsg::Upsert`
  / `Remove` / `Query` over a channel and never blocks on indexing.

### quire-intel (local first)
* `related` – TF‑IDF cosine similarity between notes ("Related notes").
* `mentions` – unlinked mentions: other notes' titles appearing as plain text →
  one‑click "link it".
* `tags` – tag suggestions from distinctive terms that match existing vault tags.
* `summary` – extractive summary (sentence centrality scoring).
* `calc` – inline calculator: a line ending in `=` (Heynote/Soulver style) is
  evaluated; supports `+ - * / ^ %`, parentheses, functions, previous‑line `ans`.
* `dates` – natural‑language dates: `@today`, `@tomorrow`, `@next friday`, `@in 3 days`.
* `llm` – `trait LanguageModel` (Strategy). Default build ships no network code. The
  `remote-llm` feature adds an Anthropic Messages API provider for rewrite/summarize/
  ask‑your‑vault.

### quire-app (application)
* `Document` – buffer + path + history + dirty flag + **revision counter**; derived
  data (outline, stats, spans) is memoized per revision so the UI never recomputes
  unchanged work.
* `Workspace` – open vault, tabs, active doc, index actor handle, autosave scheduler,
  scratchpad, recent files.
* `CommandRegistry` – every user‑visible action is a `Command { id, title, keys }`;
  the palette, menus and shortcuts are all generated from it.
* `Settings` (TOML) and `Session` (open tabs/cursor positions, JSON) in the OS
  config dir.

### Frontends
* **quire-desktop (egui)** – sidebar (files / search / tags), tabbed editor with live
  syntax styling, preview pane, Intelligence panel (outline, backlinks, related,
  unlinked mentions, summary), command palette, quick open, focus mode, status bar.
* **quire-cli** – `index`, `search`, `backlinks`, `related`, `export`, `bench`.

---

## 5. Design patterns used

| Pattern | Where | Why |
|---|---|---|
| Ports & Adapters (hexagonal) | `Storage`, `LanguageModel`, frontends | Swap disk/memory, offline/remote, egui/GPUI without touching logic |
| Command | `Transaction`, `CommandRegistry` | One path for every edit → undo, macros, replays, collaboration later |
| Inverse‑operation undo + undo tree | `History` | No full‑text snapshots; branch‑preserving undo |
| Actor | `IndexActor` | Indexing off the UI thread, no locks visible to callers |
| Repository | `Vault` | Notes addressed by id/path, storage details hidden |
| Strategy | ranking, `LanguageModel`, highlight engine | Replace BM25 or the parser independently |
| Observer / event stream | `VaultEvent`, `WorkspaceEvent` | Watcher → index → UI updates decoupled |
| Memoization keyed by revision | `Document` derived data, highlight cache | Recompute only on change |
| Unidirectional data flow | UI → `Action` → `Workspace::dispatch` → state → render | Immediate‑mode UI stays a pure view of state |
| Newtype IDs | `NoteId`, `DocId`, `Revision` | No mixing up indices and paths |

---

## 6. Key data flows

**Keystroke**
1. Frontend receives text input → builds a `Transaction` (the egui adapter diffs the
   widget's edit into a minimal Change).
2. `Workspace::apply(doc, tx)` → `Buffer` updated, inverse pushed to `History`,
   `revision += 1`, autosave deadline reset (default 800 ms idle).
3. Next frame: highlight cache misses on the new revision → `syntax::highlight`
   (≈ 1–3 ms per 100 KB) → styled layout.
4. On autosave: `Storage::write_atomic`, then `IndexMsg::Upsert` to the actor.

**External edit** (git pull, Dropbox): watcher → `VaultEvent::Changed` → if the file
is open and clean, reload; if dirty, keep the buffer and flag a conflict → index upsert.

**Rename note**: `Vault::rename_note` → build per‑file Transactions rewriting links →
write all → one `IndexMsg` batch.

---

## 7. Performance strategy

* Rope everywhere in the core; no `String` copies of whole documents on the hot path
  except at the UI adapter boundary.
* Derived data memoized by revision; highlighting is linear and allocation‑light.
* **Plain mode** for files above a threshold (default 512 KB): no live styling, no
  preview, like Notepad++ / VS Code's large‑file mode.
* Indexing on a worker thread; startup shows the UI first and indexes in background.
* Release profile: `lto = "thin"`, `codegen-units = 1`, `opt-level = 3`, `panic = abort`.
* `quire-cli bench` measures parse/highlight/index/search on generated corpora so
  regressions are visible.

Measured with `quire-cli bench 10` (release build, 2-core cloud VM):

| Operation | Time |
|---|---|
| Rope: build 10 MB / 10,000 random inserts / 10,000 line lookups | 9.0 / 8.9 / 3.5 ms |
| Highlight 100 KB (per-keystroke work) | 2.5 ms |
| Highlight / extract / render 10 MB | 398 / 424 / 365 ms |
| Parse + index 2,000 notes (≈8 MB) on 2 threads | 364 ms |
| BM25 search · prefix search · fuzzy quick-open (2,000 notes) | 7.7 · 4.9 · 1.1 ms |
| Related notes (TF-IDF over 2,000 notes) | 12.8 ms |

Known limit of the egui adapter: `TextEdit` lays out the entire string each
change, which is why plain mode starts at 512 KB. The fix is a virtualized
rope-backed editor widget (or a GPUI frontend with its rope editor); the core is
already shaped for it because it never assumes a contiguous `String`.

Why egui for v1 rather than GPUI: GPUI is pre-1.0 with frequent API changes and
heavy platform dependencies, while egui builds anywhere with one `cargo build`.
Because every behaviour lives in `quire-app` and below, switching frontends is a
new adapter crate, not a rewrite.

---

## 8. Feature map (inspired by)

| Feature | Inspired by | Status |
|---|---|---|
| Instant scratchpad, autosave, no "Save?" dialogs | Notepad, Apple Notes, Heynote | v1 |
| Live styled Markdown, heading sizes, focus mode | Typora, Bear, iA Writer | v1 |
| Preview pane, HTML export | Typora, MarkText | v1 |
| `[[wikilinks]]`, backlinks, unlinked mentions, rename refactor | Obsidian, Roam | v1 |
| Nested tags `#area/topic`, tag browser | Bear | v1 |
| Quick open, command palette | Sublime, VS Code, Obsidian | v1 |
| Full‑text search with ranking & snippets | Obsidian, Notion | v1 |
| Daily notes, templates | Obsidian, Logseq | v1 |
| Smart lists, task toggle, table auto‑format | Typora, Obsidian | v1 |
| Inline calculator, natural dates | Heynote, Soulver, Notion | v1 |
| Related notes, tag suggestions, summaries (local) | Mem, Reflect | v1 |
| LLM rewrite / ask vault (opt‑in) | Notion AI, Reflect | v1 (feature flag) |
| Graph view, canvas | Obsidian | v2 |
| WASM plugins | Obsidian, Zed | v2 |
| CRDT sync / collaboration (Loro/Automerge on top of Transactions) | Notion, Craft | v3 |
| Semantic search with local embeddings | Mem, Obsidian Smart Connections | v2 |

---

## 9. Extension seams

* **Plugins**: `CommandRegistry` + `WorkspaceEvent` are the plugin surface; a
  `wasmtime` host would expose them with a capability list.
* **Sync**: Transactions map directly onto CRDT ops; a `SyncProvider` port sits
  beside `Storage`.
* **Semantic search**: `quire-index` already has the record pipeline; add an
  `Embedder` port and an ANN index next to `FullText`.
* **Alternate UI**: implement a frontend against `quire-app`; nothing below it knows
  egui exists.

---

## 10. Testing strategy

* Unit tests in every core crate (pure functions, `MemFs` for vault/refactors).
* Property‑style round trips: `apply(tx) ∘ apply(tx.invert())` = identity.
* CLI integration on `sample-vault/`.
* Bench command for perf budgets; desktop smoke test under Xvfb.

---

## 11. v2 plan: editor, find & replace, attachments, semantic search, Ask

Each feature lands in the lowest layer that can own it, so the desktop app stays a thin adapter.

### 11.1 Virtualized editor widget (`quire-desktop/src/view/`)

Replaces egui's `TextEdit`, which re-lays out the whole string on every change.

* **Rows from the rope.** Each frame asks the `Buffer` only for the logical lines inside the viewport. Nothing copies the whole document on the input path.
* **Height index.** A `Vec<f32>` of per-line heights plus a lazily rebuilt prefix sum maps scroll offset ↔ line in O(log n). Unseen lines use an estimate (`chars × avg width / wrap width`), replaced by the measured galley height once drawn.
* **Edit log.** `Document` records a `LineDelta {start, old_lines, new_lines}` per applied transaction (including undo/redo), so the widget splices its height table instead of re-measuring.
* **Windowed highlighting.** `quire-markdown::blocks` finds fenced code and front-matter spans with a fast line scan; `highlight_window` widens the visible range to a safe block boundary and highlights only that slice. Small notes still use the whole-document highlight for exactness.
* **Pure text motions** in `quire-text::motion` (word/line/paragraph boundaries) and **multi-cursor edits** in `quire-text::multi` (insert/delete at every selection, add next occurrence) produce ordinary `Transaction`s, so undo, autosave and indexing need no changes.
* **Input:** typing, IME pre-edit/commit, clipboard (copy/cut/paste, image paste), mouse select (click, drag, double-click word, triple-click line, Alt+click cursor), keyboard navigation with Shift extension, PageUp/PageDown, caret blink.
* The old `TextEdit` path stays behind `editor = "classic"` as a fallback.

### 11.2 Find & replace (`quire-app::find`)

* `FindOptions {case, whole_word, regex}` compiled to one `regex::Regex`; matches are char ranges in the document.
* In-note bar (Ctrl+F / Ctrl+H): live match count, next/prev, replace one, replace all (one undoable transaction), matches painted in the editor.
* Vault-wide (Ctrl+Shift+H): preview every match with context, choose files, then apply. Open notes get an undoable transaction; closed notes are rewritten atomically and re-indexed.

### 11.3 Attachments (`quire-vault::attachments`)

* `Storage` gains byte-level `read_bytes` / `write_bytes_atomic`.
* `Vault::save_attachment(name, bytes)` stores into `attachments/` with a unique name.
* Pasted clipboard images (egui `PasteImage`) are encoded as PNG; dropped files are copied in. Both insert `![[name]]` at the caret.
* Lines that are only an image embed show the image inline under the text; the preview pane loads images too.
* Pasting a URL over a selection makes `[selection](url)`.

### 11.4 Semantic search (`quire-index::semantic`)

* Notes are split into **chunks** (per heading section, ~350 words max) that remember their heading for citations.
* `trait Embedder` is the port. The built-in implementation is a **latent semantic index**: TF-IDF over chunks reduced by randomized truncated SVD (k ≤ 96). It learns from the vault itself, needs no download and runs in the index actor after each batch. A neural embedder (e.g. MiniLM via candle) can implement the same trait later.
* **Hybrid search** fuses BM25 and semantic rankings with reciprocal-rank fusion; results found only by meaning are marked.
* Related notes switch to semantic vectors when the index is ready.

### 11.5 Ask your vault (`quire-intel::ask`)

* Retrieval: top chunks from hybrid search over chunks.
* With an LLM provider: chunks go in as numbered sources; the answer must cite `[[Note#Heading]]`.
* Offline: an extractive answer picks the sentences from retrieved chunks that best match the question (term overlap + semantic similarity) and cites them.
* UI: Ask panel (Ctrl+Shift+A) with the answer, clickable sources and "Insert into note".

### 11.6 Results (measured)

Release build, 2-core cloud VM, Xvfb + Mesa software rendering.

| Scenario | Result |
|---|---|
| Open a 10 MB, 101,777-line note in the fast editor | ~95 ms once (height estimates, first highlight window) |
| Scroll 25 pages, type, jump to end and back on that note (428 frames) | median 0.5 ms, p95 11 ms, p99 13 ms of UI work per frame |
| Windowed highlight of the visible region / fence scan of 10 MB | 0.2 ms / 6.6 ms |
| Fit the semantic index on 10,000 passages (2,000 notes), background thread | 0.8 s |
| Semantic passage search · hybrid search | ~5 ms · ~10 ms |

Found while measuring: index writes held the read/write lock while tokenizing,
so saving a 10 MB note stalled the UI ~100 ms. BM25 terms are now computed
during parsing, outside the lock, and the stall is gone. Also fixed: atomic
saves created files with mode 0600; they now keep the original file's
permissions (0644 for new files).

---

## 12. v3 "Trust & reach": design

Scope comes from ROADMAP.md §5 (v3). Working assumption until the product decisions in ROADMAP §7 are made: **offline by default**. No feature below makes a network call unless the user configures a provider.

### 12.1 AI providers (`quire-intel::llm`, `quire-app::secrets`)

* `LanguageModel` gains two adapters: `AnthropicModel` (Messages API) and `OpenAiCompatible` (`/v1/chat/completions`). The second covers OpenAI, Ollama, LM Studio, llama.cpp server and vLLM.
* `ProviderConfig { kind, base_url, model, embed_model }` lives in settings. The API key never touches the settings file:
  * `SecretStore` port.
  * `Keychain` adapter (macOS Keychain, Windows Credential Manager, Secret Service via the `keyring` crate).
  * `FileSecrets` fallback: a 0600 file in the config dir, used only when no keychain is available, and the UI says so.
* Settings screen: theme, font, editor kind, inline images, AI provider with **Test connection**.

### 12.2 Version history (`quire-vault::history`)

* On every successful save, the previous content is written as a snapshot:
  * Location: `.quire/history/<path-hash>/<unix-ms>.md.gz`, gzip-compressed.
  * Skipped when the content is identical to the newest snapshot.
  * Throttled: at most one snapshot per 2 minutes while typing; external changes always snapshot.
* Retention, applied after each write:
  * every snapshot from the last hour;
  * hourly for a day;
  * daily for a month;
  * weekly after that;
  * capped at 200 per note.
* Renames move the history folder with the note. Deletion keeps it, so restore from trash still has history.
* `similar` produces a line diff for the timeline UI. **Restore** is a whole-document `Transaction`, so it's undoable and itself snapshotted.

### 12.3 MCP server (`quire-cli mcp`)

* JSON-RPC 2.0 over stdio implementing MCP `initialize`, `tools/list`, `tools/call`, `ping`.
* Read tools: `search_notes` (hybrid), `read_note`, `list_notes` (folder/tag filter), `backlinks`, `ask_notes` (retrieval + cited sources), `list_tags`.
* Write tools, only registered with `--allow-write`: `create_note`, `append_to_note`, `replace_in_note`. Every write snapshots history first, so an AI edit is always reversible.
* Scope is one vault per process. There's nothing to turn off because nothing runs unless the user adds `quire-cli mcp <vault>` to their MCP client, unlike Bear's default-on CLI.

### 12.4 Importers (`crates/quire-import`)

* `trait Importer { fn detect(path) -> bool; fn plan(path) -> ImportPlan }`. An `ImportPlan` is a list of `(target NotePath, content)` plus attachments and warnings. **Dry run first**: the plan is shown, then `apply(plan, vault)` writes it without overwriting (unique names).
* Adapters:
  * **Logseq (Markdown)**
    * `journals/2026_11_01.md` → `Daily/2026-11-01.md`.
    * Namespaced pages `a___b.md` / `a%2Fb.md` → `a/b.md`.
    * `key:: value` page properties → front matter.
    * `((uuid))` block refs → the referenced block's text with a link to its page.
    * `id::` lines dropped.
    * The outline structure is kept as lists.
  * **Bear**: `.bear2bk` / TextBundle folders. `text.md`/`text.markdown` plus `assets/` become note + attachments, and Bear's `#multi word#` tags are normalised.
  * **Notion**: a zip or folder export. Strips the 32-hex ids from file names and rewrites `[x](Page%20abc123.md)` links to `[[Page]]`. Database CSVs become one note per row, with columns as front matter.
  * **Evernote**: `.enex` XML → ENML/HTML converted to Markdown (`htmd`), with base64 resources written as attachments.
  * **Markdown folder** (Apple Notes' iOS 26 export, iA Writer, anything else): copy with attachments, keeping relative image links working.

### 12.5 Neural embeddings (`quire-intel::embed`)

* `EndpointEmbedder` implements `quire_index::semantic::Embedder` against any OpenAI-compatible `/v1/embeddings` endpoint. With Ollama and `nomic-embed-text`, this is fully local and neural.
* `IndexService::set_embedder(factory)` swaps how the semantic worker builds the index.
* Vectors are cached on disk (`.quire/cache/embeddings.bin`, keyed by passage hash and model), so a rebuild only embeds changed passages. Requests are batched.
* Any failure falls back to the built-in LSA index with a notice.
* An in-process model (candle) remains a later option; this path gives neural quality today without shipping a model.

### 12.6 Properties & Views (`quire-markdown::frontmatter`, `quire-index::views`)

* Front matter gets a writer: `set_field` and `remove_field` edit the YAML block in place and preserve unknown lines. Edits are Transactions.
* Properties panel in Insights: list, edit, add and remove fields. Types are inferred: date, number, bool, list, text.
* **View blocks** are fenced code blocks, so the file stays plain Markdown and renders as code elsewhere:

  ```quire-view
  from: "#project"          # tag, folder ("Projects/") or "all"
  where: status != done     # =, !=, >, <, contains, exists
  sort: due asc
  group: status
  layout: kanban            # table | list | kanban | calendar
  fields: [title, status, due, tags]
  date: due                 # field used by calendar
  ```

* `views::run(spec, index) -> ViewResult` queries `NoteRecord.properties` and built-ins (`title`, `folder`, `tags`, `created`, `modified`, `tasks`, `open_tasks`).
* The fast editor renders the result live under the block's closing fence, as it does for inline images. Clicking a row opens the note. Dragging a kanban card to another column sets the group field in that note's front matter, which is a normal save, so it gets history.

### 12.7 Accessibility & IME

* The fast editor publishes AccessKit info: role multiline text input, a label naming the note, the current line as its value, and caret announcements as the line changes.
* IME pre-edit text is laid out inside the caret's line, underlined, instead of floating over it.

### 12.8 As built

| Area | Where | Notes |
|---|---|---|
| Providers & keys | `quire-intel::llm::remote`, `quire-app::{ai, secrets}` | Anthropic + OpenAI-compatible; `probe()` backs *Test connection*. Keychain via `keyring`, 0600 file fallback (the UI names which one is in use). |
| Neural embeddings | `quire-intel::embed`, `quire-index::semantic::VectorCache` | Batched `/v1/embeddings`; cache keyed by passage hash + model. In the UI test, editing one sentence re-embedded 1 of 17 passages. Any error falls back to LSA and shows a notice. |
| Version history | `quire-vault::history`, `quire-app::history_ops`, `dialogs::history` | The first save also keeps the on-disk original. Closed-note writes (kanban moves, vault replace, MCP) snapshot before and after. |
| MCP | `apps/quire-cli/src/mcp.rs` | Protocol versions 2025-11-25 back to 2024-11-05. Write tools are not even listed without `--allow-write`. |
| Math, diagrams, PDF | `crates/quire-typeset`, `apps/quire-desktop/src/typeset_ui.rs`, `quire-app::export_pdf` | Pure Rust: MiTeX converts LaTeX math to Typst math, Typst lays it out, `typst-svg`/`typst-pdf` export; Mermaid via `mermaid-rs-renderer`. Fonts embedded (Inter, JetBrains Mono NL, STIX Two Math). The desktop renders on one worker thread to textures (resvg), cached per source/size/colour/scale; panics in a renderer are caught (release builds unwind). `blocks::figures` finds `$$` and ```` ```mermaid ```` blocks; live preview collapses them to their picture unless the caret is inside. |
| Callouts & paragraph ids | `quire-markdown::callout`, `render::with_callouts`, `view/mod.rs` | One table of kinds/aliases/colours/glyphs used by the editor, preview (egui_commonmark alerts), HTML/site export and PDF. `^id` anchors: `callout::find_block` for jumps, `render::anchor_block_ids` for HTML. |
| Spelling | `crates/quire-spell`, `apps/quire-desktop/src/spell_ui.rs` | Visible lines only, per-line cache in the speller; ranges overlapping code/math runs are dropped; the word at the caret isn't flagged. Dictionaries load on a background thread. |
| Quick capture | `quire-app::capture`, `apps/quire-desktop/src/capture_key.rs` | `global-hotkey` (X11 on Linux); entries go under a Log/Inbox heading if the daily note has one, into the open document when it's open. |
| Importers | `crates/quire-import` | `Tree` loads folder / zip (one level of nested zips, wrapper folder stripped) / single file. Detection order: Evernote, Bear, Logseq, Notion, Markdown. Notion database views use `from: ./DB/` so they survive importing into a subfolder. Hand-written ENML→Markdown converter (no HTML5 parser dependency). |
| Properties | `quire-markdown::frontmatter::{set_field, remove_field}`, `quire-app::properties` | Edits replace only the field's own lines; quoting round-trips through the reader. Panel edits are undoable Transactions. |
| Views | `quire-index::views`, `apps/quire-desktop/src/view_ui.rs` | Parsed + evaluated per index version (memoized). Rendered under the closing fence like inline images; the editor ignores presses over a view so its widgets get them. |
| Accessibility & IME | `view/mod.rs` | AccessKit role `MultilineTextInput`, label "<note>, line N", value = caret line, description with line/column. Pre-edit text is laid out inside the caret line's galley, underlined. |

One cross-cutting fix came out of UI testing: app-level editing shortcuts (Ctrl+A, Ctrl+Z, formatting) now only fire when the note editor has focus, so they no longer act on the note while you type in the find bar or the properties panel.

