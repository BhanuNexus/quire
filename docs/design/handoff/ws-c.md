# WS-C hand-off: retrieval and trust intelligence

What WS-C (design §3.C, §6.2) built, what it touched outside its own files
and why, what it needs from others, and the numbers its thresholds rest on.

## What other workstreams can use

* **Folding** (`quire_index::tokenize`): `fold(s) -> (String, Vec<u32>)`
  (folded text and a map from folded char to original char), `fold_str`,
  and `Folded` (byte ranges back to the original, `find_all`). BM25 and LSA
  tokens, snippets, phrases, wikilink resolution (exact, then folded, then
  folded aliases) and quick open use it. `FindOptions::match_accents`
  (default exact, as 0.8: CLI, MCP, Vim and vault-wide replace keep their
  behaviour); `FindOptions::find_bar()` is the find bar's start (accents
  optional).
* **Section search** (`Index::search_sections(semantic, query, filters,
  limit)`, and `IndexService::search_sections` as before): hits are note
  *parts* (`quire_index::chunk::parts`, shared with the semantic passages)
  with line ranges; `SearchFilters` applies `folder`, `tags`, `where_`
  (`quire-view` conditions, dotted paths), `modified_after/before` and
  `kind` (`Attachments`/`All` add PDF and image pages; `SectionHit::page`).
  `SearchFilters::validate()` says what's wrong with a `where_`.
  `Index::{line_of, lines_of, modified, idf}`, `LinkGraph::neighbors`.
* **Anchors** (`quire_intel::anchor`): `make_anchor` widens to whole
  sentences; `resolve` runs the full ladder (§C.3); `footnote` /
  `citations` write and read citation footnotes (docs/formats/citations.md);
  `verify_request`, `parse_support`, `lexical_support`.
* **Ask** (`quire_intel::ask`): `retrieve` (sources now carry `end`, `sim`,
  `coverage`), `should_abstain`, `model_abstained` (`NOT_FOUND`),
  `build_chat_request` (with an optional ask-before-tell hypothesis),
  `cite_spans` / `source_anchor` (citations narrowed to 1–2 sentences),
  `answer_note` (the saved note), `extractive_markdown` (`[n]` form).
  `quire_intel::cluster::evidence_clusters`.
* **Claims and contradictions** (`quire_intel::{claims, contradict,
  timeline, stale}`), with their caches (docs/formats/claims.md).
* **Workspace** (`quire_app::trust_jobs`): `trust_chat()`,
  `check_citations()`, `citation_checks(path)`, `mark_reviewed(path)`,
  `mark_superseded(path, by, fp)`, `open_side_by_side(a, b)`,
  `reconcile(a, b)`, `update_citation(note, label)`,
  `set_note_properties(path, set)`; `ws.features.trust.reconcile`.

## Touched outside WS-C's files (smallest additions)

| File | Change | Why |
|---|---|---|
| `apps/quire-desktop/src/inbox_ui.rs` | `finding` takes `ws` and draws items through `registry::INBOX_KINDS` when one claims them; `Act` is `registry::InboxAct` | the per-kind actions hook offered in WS-A's hand-off (documented in F0-notes) |
| `apps/quire-desktop/src/registry.rs` | WS-C blocks (Trust settings tab, citations banner, drift widgets, Ask samples) and the new `INBOX_KINDS` table with `InboxKindDef`/`InboxAct` | registration points |
| `apps/quire-desktop/src/app.rs` | `UiRequest::Ask` opens `ask_ui::AskOverlay` | §6.2: Ask moves out of `overlays.rs` |
| `apps/quire-desktop/src/overlays.rs` | the `Ask` variant and its drawing removed; "Match accents" checkbox in Find and replace in all notes | the move; §C.2 |
| `apps/quire-desktop/src/layout_check.rs` | the old `ask` sample removed (Ask's states are in `layout_samples`) | the move |
| `apps/quire-desktop/src/panels.rs` | one toggle in the find bar ("é", Match accents) | §C.2 ("the accent toggle in find.rs": the option is in `find.rs`, the button in the bar) |
| `apps/quire-desktop/src/main.rs` | `mod ask_ui; mod trust_ui;` | new files |
| `crates/quire-app/src/lib.rs` | `pub mod ask_state` | the dialog reads `AskStep` |
| `crates/quire-app/src/find_state.rs` | `Default` written out so the find bar starts with accents optional | §C.2 |
| `crates/quire-app/src/nav/vim_doc.rs` | `match_accents: true` in two `FindOptions` literals | Vim search stays exact |
| `crates/quire-mcp/src/tools/read.rs` (`ask`) | `should_abstain` instead of "nothing found"; `source_anchor` for narrowed citations; `NOT_FOUND` in model mode | the abstain τ is calibrated through this tool (`quire-cli eval`); §C.4 span citations |
| `crates/quire-mcp/tests/fixtures/mcp-0.9-*.jsonl` | re-recorded: the `search` snippet is now the section's | section hits (§C.1) |
| `crates/quire-intel/Cargo.toml`, `src/lib.rs` | `strsim`, `similar` (workspace deps); module lines for `claims`, `cluster`, `contradict`, `json`, `stale`, `timeline` | WS-D also edits these: trivial merge |
| `crates/quire-index/Cargo.toml` | `unicode-normalization`, `chrono` | folding; `where_` dates |
| `docs/formats/README.md`, `comments.md` | rows for WS-C's files | format index |

## Requests

1. **WS-D: the chat model.** Trust features get their model from
   `Workspace::trust_chat()` (`trust_jobs.rs`), which today wraps the 0.8
   provider (`ws.assistant`) in `LanguageModelChat` and guesses locality
   from the provider settings. When providers become `ChatModel`s (and for
   the on-device `LocalChat`), make `trust_chat()` return yours: one
   function. Ask (`ask_state.rs`) still calls the 0.8 `LanguageModel`
   through `spawn`; it can move to the same `ChatModel` then (streaming the
   answer would be a bonus).
2. **WS-D: "Ask agent to reconcile".** A contradiction's action stores a
   `ReconcileRequest { prompt, anchors }` in `ws.features.trust.reconcile`
   and asks for `UiRequest::Feature(Command::ToggleAgentPanel)`. Please
   `take()` it when the panel opens and start an Agent-mode run with the
   prompt and both anchors as context (its edits become a proposal). While
   the panel isn't available, the prompt is copied to the clipboard with a
   notice.
3. **WS-D: Settings → AI → Features.** §7.2 puts Ask style, Find
   contradictions, staleness and citation checks there. They are in their
   own tab, Settings → Trust (`trust_ui::settings_tab`), so WS-C doesn't
   edit WS-D's tab; fold it in by calling `crate::trust_ui::settings_tab`
   from your Features section and dropping the `"trust"` entry, if you like.
4. **WS-D, WS-G: attachment text.** Send `IndexMsg::UpsertAttachment`
   (page text, PDF or OCR) and `RemoveAttachment`; `search_sections` with
   `kind: Attachments | All` finds pages (`SectionHit::page`; the file's name
   counts for its first page). Text is kept per path until `Load` (re-send
   after a vault opens).
5. **WS-B / integrator: MCP `search`.** Call `filters.validate()` and answer
   `invalid_args` with its message for a bad `where`; show "page N" for hits
   with `page`. `ask` could list the nearest passages when it abstains, as
   the app does.
6. **Integrator: side by side.** "Open both" opens note A, then note B, then
   asks for `UiRequest::Split`, which *toggles*: with a split already open
   it closes it, and the other half shows the most recently opened other
   note (not necessarily A). A `UiRequest::SideBySide(NotePath, NotePath)`
   in `app.rs` would make it exact; `Workspace::open_side_by_side` would
   switch to it in one line.
7. **Neural embedders and claims.** Claim vectors come from
   `SemanticIndex::embed_query` once per claim per session (free with LSA,
   one request per claim with an endpoint embedder). If WS-D's local
   embedder is slow per call, a batch `embed_texts` on `SemanticIndex`
   would help; the vector cache could also keep them.

## Thresholds and the numbers behind them

See "Calibration" below (filled in from `quire-cli eval synthetic-edits`).

## Known limits

* Staleness uses the index's modified time and checks history provenance
  only for notes that could be flagged, so a note an agent edited recently
  counts as fresh until a person's edit is older than its fresh period.
* Contradiction finding judges pairs with the same model that extracts
  claims; prompts are English-first (§8.3). Dismissal holds while the pair's
  quotes are unchanged.
* The thinking timeline replays at most 20 saved versions of at most 40
  notes per topic; without model claims it reads plain sentences
  ("approximate").
* Claim extraction blocks the single job thread for one request at a time;
  other jobs wait meanwhile (they're idle-only too).
