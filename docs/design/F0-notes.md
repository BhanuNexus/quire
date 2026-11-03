# 0.9 foundation (F0): notes for the workstreams

What F0 landed, which files each workstream owns from now on, where to
register things, and what to watch out for. Read with `0.9.md` (§6 has the
plan; "Changes during F0" at the end of §6.1 lists where F0 differs from it).

## The rules in practice

* **Your files only.** Each workstream owns the files listed below. Outside
  them you may only add lines inside *your* block of a registration table
  (marked `// ---- WS-X`), or your own line under your marker in a
  `Cargo.toml`. Nobody edits the root `Cargo.toml`, `deny.toml`,
  `workspace.rs`, `features.rs`, `commands.rs`, `app.rs`, `panels.rs`,
  `overlays.rs`, `dialogs.rs` or `settings/mod.rs`; if you need a change
  there, ask for a one-line follow-up on `main`.
* **Every 0.9 command already exists** (`quire_app::Command`, final keys,
  `Command::owner()`). Handle yours in your entry module (below). A command
  stays hidden (palette, shortcut sheet, keys) until your `available()`
  returns true for it.
* **Every tab and dialog is registered** in `apps/quire-desktop/src/registry.rs`
  and stays hidden until its `available` function returns true.
* **Formats:** follow `docs/formats/README.md`. Add `docs/formats/<name>.md`
  and a row in the README for each file you introduce.
* **Changelog:** write `changelog.d/<ws>.md` fragments (see `changelog.d/README.md`).
* **Gate:** fmt, clippy (full and Lite), tests, `typos`, `cargo deny check`,
  `cargo doc` with `-D warnings`. New overlays go in `registry::layout_samples`.

## Shared plumbing (frozen; use it, don't change it)

### quire-app: the feature hub

`crates/quire-app/src/features.rs` calls one *entry module* per workstream.
Each entry module has the same functions, all currently stubs:

| Function | Called |
|---|---|
| `tick(ws, now) -> bool` | every `Workspace::tick` (return true to repaint) |
| `next_deadline(ws) -> Option<Instant>` | for the frontend's wake-up time (reminders, debounces) |
| `available(ws, cmd) -> bool` | to decide whether a command is offered |
| `run(ws, cmd) -> Option<UiRequest>` | for each of your commands; return `UiRequest::Feature(cmd)` to have the desktop show your tab or dialog |
| `decorations(ws, doc, out)` | when a document's decorations are needed (cached per revision and `features.decor_generation`; bump it when your data changes) |
| `decoration_event(ws, ev)` | a click on one of your decoration actions (`ActionId { owner, id }`) |
| `vault_event(ws, ev)` | every watcher event, including `AttachmentChanged`/`AttachmentRemoved` |
| `vault_opened(ws)` | after a vault opens (state is reset; add jobs here) |
| `status(ws, out)` | status-bar items (`StatusItem`) |

State lives in `ws.features.<field>` (types in your own files). Other useful
pieces: `ws.agent_store()` (the vault's `AgentStore`), `ws.device()`,
`ws.jobs` (`JobRunner::add(Box<dyn Job>)`), `Document::next_save_provenance`
(set it when a person accepts a suggestion into an open note; the next save
writes it next to the snapshot), `quire_vault::VaultPrefs` for your
`vault.toml` section, and your table in `settings/<name>.rs`.

### quire-agent: the one write path

`AgentStore::submit(&actor, ChangeSet)` checks paths, policy, size and
expected versions, then either applies every change (history before and
after, provenance, ledger) or stores one proposal. Nothing is written when it
returns an error. `WriteError::code()` and `to_json()` are the stable codes
for tools. Actors are built with `Actor::agent/human/job/import`; the
caller's grant is `AgentStore::set_cap(actor_id, Mode)`.

### quire-mcp: the tool registry

`ToolRegistry::standard()` lists tools in a fixed order: the 0.8 tools
(`tools/legacy.rs`), then WS-B's block, then `tools/canvas.rs` (WS-G).
A tool is a `ToolSpec` plus `call(&ToolCx, &Value) -> ToolOutput`; `FnTool`
makes one from a function. The server picks the era per request and shapes
results; tools never deal with protocol versions.

### Desktop registries (`apps/quire-desktop/src/registry.rs`)

| Table / function | What you add |
|---|---|
| `FeatureUi` | a field for your panels' UI state (type in your file) |
| `RIGHT_TABS`, `SIDEBAR_TABS`, `SETTINGS_TABS` | `TabDef { id, label, available, show }` |
| `ui_target` | which tab `UiRequest::Feature(cmd)` opens for your commands |
| `paint_widget` | an arm for your `WidgetId`s (route on `id.owner`) |
| `open_surface` | an extension → `Box<dyn Surface>` (`surfaces.rs` has the trait) |
| `layout_samples` | every dialog you add (`Overlay::Feature(Box<dyn OverlayView>)`), in each interesting state |

Dialogs implement `overlays::OverlayView` in their own file; never add a
variant to `Overlay`. Decorations are painted by `decor.rs`; the hook lines
in `view/mod.rs` are WS-F's to maintain.

## Ownership by workstream

Files marked *new* don't exist yet; create them. "Block" means your marked
block in a shared table.

### WS-A — agent store and review

* **Owns:** `crates/quire-agent/**` (all of it, including `docs/formats/{proposals,ledger,policy,provenance,inbox}.md`); `crates/quire-app/src/agent_state.rs` (entry module, `AgentState`), *new* `inbox.rs`, `review.rs`, `lint.rs` in quire-app; `apps/quire-desktop/src/{inbox_ui.rs, activity_ui.rs, agents_settings.rs, decor.rs}`, *new* `review_ui.rs`; `apps/quire-cli/src/commands/lint.rs`.
* **Blocks:** registry WS-A block (Inbox and Activity right tabs, Agents settings tab, `ui_target` arms, review widgets); `FeatureUi` fields `inbox`, `activity`, `agents_settings`.
* **One allowed line in `workspace.rs`:** the rename hook (§6.3).
* **Interface notes:**
  * `review`/`decide` take the file path (a proposal can span files); `Review::available` tells the UI whether hunks are real.
  * Policy today: the stricter of the file's mode and the default mode (no widening without approval), hard rules, grants. Client overrides and contracts are parsed (`Policy::clients`, `Policy::contracts` as raw TOML tables) but not applied. Approval state goes in `<config>/vaults/<key>/state.toml`.
  * `submit` proposes a whole set if any change needs a proposal; no amend yet; renames with `update_links` rewrite other notes without per-note checks; no rate limits.
  * Ledger reading covers this device only (`Storage` can't list folders).
  * `InboxStore::items()` merges pending proposals and stored findings.
  * Decoration painting (`decor.rs`) is basic: tints, strike, ghost lines with small buttons. The review look and keyboard flow are yours.

### WS-B — MCP, remote access, capture, sync versioning, eval

* **Owns:** `crates/quire-mcp/**` except `tools/canvas.rs`; `crates/quire-app/src/mcp_state.rs` (entry module, `McpState`), `vault_sync.rs` (the `.quire/shared` listing); `crates/quire-import/src/web.rs` (*new*); `crates/quire-sync/src/{http.rs, folder.rs, protocol.rs}` as in §6.2; `apps/quire-sync-server/src/lib.rs` (`/v1/info`); `apps/quire-desktop/src/mcp_ui.rs` (*new*); `apps/quire-cli/src/commands/{mcp.rs, agent.rs, capture.rs, eval.rs}`; `skills/quire/SKILL.md` (*new*).
* **Blocks:** `ToolRegistry::standard` WS-B line; registry WS-B blocks (Settings tab or a section in the Agents tab, consent modal in `layout_samples`); `crates/quire-mcp/Cargo.toml` (yours); desktop `Cargo.toml` WS-B marker (`quire-mcp` with `http`).
* **Interface notes:**
  * The 0.8 tools must keep replaying `tests/fixtures/mcp-0.8-*.jsonl`. When you switch `--allow-write` writes to `submit` (§B.4, a deliberate change), update that fixture and the CHANGELOG fragment together.
  * Eras: a request naming `2026-07-28` in `_meta` is modern; `initialize` makes the process legacy; `server/discover` is answered for probes. Legacy results are exactly 0.8's; `tool_result` adds `structuredContent`/links only for modern clients.
  * The actor is `mcp:<clientInfo.name>` (self-reported) until `--client` keys exist; a read-only server sets the cap `Mode::Read`.
  * `quire-cli agent proposals|show|log` exist; add `accept`, `reject`, `revert` on WS-A's store API.

### WS-C — retrieval and trust intelligence

* **Owns:** `crates/quire-index/**` except `views.rs` (incl. `attach.rs`, `sections.rs`); `crates/quire-intel/src/{anchor.rs, ask.rs}` and *new* `claims.rs, contradict.rs, stale.rs, timeline.rs, cluster.rs`; `crates/quire-app/src/trust_jobs.rs` (entry module, `TrustState`), `ask_state.rs`; `apps/quire-desktop/src/timeline_ui.rs`, *new* `ask_ui.rs`; the accent toggle in `find.rs`; `settings/trust.rs`, `settings/jobs.rs` (keys only; the runner is F0's).
* **Blocks:** registry WS-C block (Timeline tab, drift widgets).
* **Interface notes (after WS-C):** `search_sections` returns note parts
  with line ranges and applies every filter (`where_`, `modified_*`,
  attachment pages with `kind`); `anchor::resolve` runs the full ladder
  (`Drifted` from 0.75); Ask abstains by `ask::should_abstain` (calibrated,
  design §C.4). What WS-C offers others, its requests and its numbers:
  `docs/design/handoff/ws-c.md`.

### WS-D — on-device AI, providers, assistant, onboarding, wiki loop

* **Owns:** `crates/quire-ai/**`, `apps/quire-ai-engine/**`, `crates/quire-assistant/**`; `crates/quire-intel/src/{llm.rs, embed.rs, chat.rs}`; `crates/quire-app/src/{assistant_state.rs (entry module), ai.rs (incl. LocalAiState), onboarding.rs}`, `settings/{onboarding,ai_local,agent_panel}.rs`; `apps/quire-desktop/src/agent_panel_ui.rs`, *new* `onboarding_ui.rs`, `ai_settings_ui.rs`, `acp_ui.rs`; `apps/quire-cli/src/commands/ai.rs`; the release workflow's engine job; `docs/formats/{engine-protocol,model-catalog}.md`.
* **Blocks:** registry WS-D block (Agent right tab; `SETTINGS_TABS` entry with `id: "ai"` replaces the built-in AI tab; onboarding overlay in `layout_samples`); Cargo markers in quire-app (`quire-ai`), desktop and CLI (`quire-ai`, `quire-assistant`).
* **Interface notes:** the engine answers `hello` (exact version match) and `shutdown`; `Engine::hello` is a working health check, and `apps/quire-ai-engine/tests/hello.rs` runs the real binary. `quire-ai` without `client` is protocol-only (what the engine links); candle/tokenizers/ocrs are workspace deps, not yet used. `LanguageModelChat` adapts the 0.8 providers to `ChatModel` (text only). `quire-assistant::prompts::PROMPTS` is empty; MCP prompts read it. Binary budget for the engine: `packaging/check-size.sh <bin> engine`.

### WS-E — planning and metadata

* **Owns:** `crates/quire-plan/**`; `crates/quire-markdown/src/{frontmatter.rs, tasks.rs}`, *new* `srs.rs`; `crates/quire-index/src/views.rs`; `crates/quire-vault/src/templates.rs`; `crates/quire-app/src/{tasks_state.rs (entry module), periodic.rs, srs_state.rs, meta_refactor.rs}`, *new* `reminders.rs`, `templates_ui_state.rs`; `settings/tasks.rs`; `crates/quire-import/src/obsidian/**`; `apps/quire-desktop/src/{agenda_ui.rs, calendar_ui.rs}`, *new* `meta_refactor_ui.rs, srs_ui.rs, template_prompt_ui.rs, base_surface.rs`; `apps/quire-cli/src/commands/tasks.rs` (add `tags`, `props` there).
* **Blocks:** registry WS-E block (Agenda right and sidebar tabs, Calendar sidebar tab, `.base` surface in `open_surface`); Cargo marker in quire-app and CLI (`quire-plan`).
* **Interface notes:** `Task` has every field (all `None`); `tasks::parse_meta(line, marker, &mut task)` is called for each task by `extract` with the whole line. `Value::Map` exists and serializes as a JSON object; the parser still skips nested blocks. `[periodic]` is `quire_plan::periodic::PeriodicPrefs` (optional keys + `resolve`); move the daily-note defaults from `VaultConfig` when you wire it. The SRS log format and replay order are fixed (`quire_plan::srs`). `agenda::build` and `reminders::schedule` already work off the task fields.

### WS-F — editor and UI

* **Owns:** `crates/quire-text/src/vim.rs` (*new*); `apps/quire-desktop/src/view/**` (including the decoration hook lines), `editor.rs`, `theme.rs`, `panels.rs` sidebar and tab-bar internals (not the registries); `crates/quire-app/src/{nav.rs (entry module), git.rs, themes.rs, session.rs}`, `settings/{editing,appearance}.rs`; `apps/quire-desktop/src/` *new* `git_ui.rs, theme_ui.rs, embed_edit.rs`.
* **Blocks:** registry WS-F block (embedded-editor widgets, theme settings tab).
* **Interface notes:** `editing.vim`, `tab_mode`, `display_titles`, `homepage` are in settings; vault-level order, icons and tag colours go in `vault.toml` sections via `VaultPrefs`. Status items (`-- INSERT --`) come from `nav::status`.

### WS-G — spatial and reading

* **Owns:** `crates/quire-canvas/**`, `crates/quire-pdf/**`, `crates/quire-markdown/src/cite.rs` (*new*), `crates/quire-mcp/src/tools/canvas.rs`; `crates/quire-app/src/{canvas_state.rs (entry module), pdf_state.rs, citations.rs}`; `crates/quire-typeset/src/markdown.rs` (bibliography only); `apps/quire-desktop/src/` *new* `canvas_ui/**, pdf_ui/**, cite_ui.rs`; `docs/formats/{canvas,pdf-annotations}.md`.
* **Blocks:** registry WS-G block (`.canvas`, `.pdf` in `open_surface`; widgets); Cargo markers in quire-app and desktop (`quire-canvas`, `quire-pdf`, and `"quire-pdf/render"` in their `typeset` feature).
* **Interface notes:** the canvas model and canonical writer exist and round-trip Obsidian files with unknown keys. `quire-pdf` has `text` (default) and `render` features; `hayro`/`hayro-interpret` 0.7 are workspace deps (what Typst links), not yet used. The annotation format and `note_for` are fixed. `SystemViewer` is the placeholder surface for files without an editor.

## Integration (wave 1) additions

Wave 1 (WS-A, WS-B, WS-E, WS-F) is merged on `feat/0.9-integration`. The
hot files are the integrator's again; these mechanisms replace the
stand-ins the workstreams used. WS-C, WS-D and WS-G use them instead of
building their own.

### Asking for UI from anywhere

* **`Workspace::request_ui(UiRequest)`** queues a request from any code that
  has the workspace: a click in a note widget, a decoration event, a tick, a
  job's result. The desktop takes the queue every frame
  (`Workspace::take_ui_requests`) and treats each one like a command's
  return value. Use it instead of flags that a panel polls.
* **`UiRequest::Feature(cmd)`** shows `cmd`'s UI (`registry::ui_target`)
  without running the command: e.g. WS-E's note widget starts a flashcard
  session, then asks for `Feature(Command::StartReview)`.
* **`UiRequest::RevealCaret`** scrolls the editor to the caret without taking
  focus (after a feature moved it; the review asks for it on every hunk).
* **`UiRequest::OpenFile(path)`** opens a file that isn't a note in its
  surface (`registry::open_surface`); notes open as notes.

### Dialogs for commands

* **`registry::UiTarget::Overlay(make)`**, with
  `make: fn(&mut Workspace) -> Box<dyn OverlayView>`, opens a dialog for a
  command (it may read what the command prepared in the workspace). Used by
  Undo agent changes, Capture web page, Rename tags/properties and the
  flashcard review. Background jobs pause while it is open.
* **`OverlayView::closed(&mut self, ws)`** runs however the dialog goes away
  (Escape, its own button, another dialog replacing it): end sessions there
  (the flashcard review calls `srs_end`).
* **`OverlayResult::then`** replaces the current overlay with another (the
  template picker leads to `TemplateDialog` when a template has questions).
* `layout_samples` takes `&mut Workspace` now (so samples can use `make`).
  Every dialog still goes there, in each interesting state.

### Bars above the note

**`registry::EDITOR_BANNERS`** (`BannerDef { id, available, show }`): full-width
bars between the find bar and the note, drawn while `available(ws)` is true
(called every frame; keep it cheap) and not while a surface has the central
panel. WS-A's review banner is the first. A banner returns `PanelOut` like a
tab.

### Inbox items of other features (added by WS-C)

**`registry::INBOX_KINDS`** (`InboxKindDef { handles, body, actions }`):
a producer of Inbox findings (contradictions, staleness, citations) draws
its own items. For each stored finding the Inbox finds the first entry whose
`handles(&item)` is true and draws a card with `body(ui, ws, item, pal)`,
then one row with `actions(…)` followed by the Inbox's own **Dismiss** and
**Snooze**. `actions` returns an `InboxAct` (`Box<dyn FnOnce(&mut Workspace,
&mut PanelOut)>`), run after drawing, like the Inbox's own buttons. Items no
entry claims keep the plain sentence with Open. Grouping, filters, snooze,
the weekly digest and dismissals stay the Inbox's. WS-C registers
`trust_ui::{is_trust_item, inbox_body, inbox_actions}`.

### Keys

* An Alt shortcut (Alt without Ctrl) drops the text its key types, in that
  frame and the next, for every command. Don't add per-key guards.
* With Vim keys, Escape in Insert mode is Vim's; the find bar closes on
  Escape only in Normal mode.

### Files, surfaces and the sidebar

* The Files tab and quick open list files that have a surface
  (`registry::has_surface`) next to the notes; `Workspace::other_files()`
  lists the vault's non-note files (cached a few seconds). Registering an
  extension in `open_surface` is all WS-G needs for `.canvas` and `.pdf`.
* A `PanelOut` with `jumped` (from a tab, the sidebar or a surface) gives
  the notes the central panel again.
* `nav::display_name` returns the file name (with extension) for non-notes.

### Settings

The Settings dialog lists tabs on the left and gives every tab one
fixed-height body that it scrolls. **Tabs must not add their own scroll area
or size themselves from the window**; just draw top to bottom. A tab's
`PanelOut::run` runs after the dialog (e.g. Connections → Capture a web page).
New `[editing] new_note_folder` (`"current"`, default, or `"root"`).

### Agent store and MCP

* **`ChangeSet::accepted_from: Option<Actor>`**: a change the person approved
  outside Quire's review (MCP in-client approval). Submit it as the person;
  provenance gets `accepted_from` and `op: accept`, the ledger entry `from`.
* **`Actor::unverified`**: set for MCP clients identified only by what they
  report; their `[clients]` entry can narrow but never widen.
* Read logging goes through `AgentStore::record_read` (off unless Settings →
  Agents turns it on). `quire_mcp::instructions::{vault_key, find}` are the
  agent store's.
* `Workspace::snapshot_provenance(&Snapshot)` reads who made a version (the
  History dialog shows it and links to Agent activity).
* The CLI's `agent accept/reject/revert` use `resolve_proposal`,
  `accept_in_file`/`decide_checked` and `revert_plan`/`revert_apply`; MCP
  `revert` uses `propose_revert` (always a suggestion).

### Smaller additions

* `Workspace::set_property_path` / `remove_property_path`: one nested
  front-matter field of the active note (the Properties panel edits maps
  field by field).
* `Workspace::new_note_from_template_titled(template, title, …)`: a typed
  title wins over the template's `name:`.
* Themes: the `added` token (`Palette::added`: suggested text, added lines);
  the shared highlighter (classic editor, embeds) uses `h1`–`h6`, `link`,
  `quote`, `conflict` and caches per palette.
* The desktop bundles egui's monochrome emoji font (both editions), so icons
  and task emoji draw everywhere; `NotoEmoji-Regular.ttf` in the fonts folder
  still takes over when present.
* `chacha20poly1305` and `futures-core` are workspace dependencies; CI runs
  `quire-cli eval synthetic-edits` on Linux.

### For WS-C, WS-D and WS-G

* A tab or dialog that opens from a click (an Inbox action, a citation chip,
  a canvas node): `ws.request_ui(UiRequest::Feature(cmd))`, with the
  command's `ui_target` pointing at the tab or `UiTarget::Overlay`.
* WS-D's onboarding overlay and ACP warning: `UiTarget::Overlay` (or
  `request_ui` at first start); the agent panel stays a right tab.
* WS-C's contradiction actions in the Inbox use `registry::INBOX_KINDS`
  (above), added by WS-C.
* WS-G: register `.canvas`/`.pdf` in `open_surface`; links to them can call
  `request_ui(UiRequest::OpenFile(path))`.
* Banners (e.g. "This answer's citations drifted") go in `EDITOR_BANNERS`.
* Known gaps left for later: the sidebar highlights the active note, not the
  active surface; Settings → Sync's checkbox talks to the sync server on the
  UI thread when turned on (a one-off request); the Templates folder's
  notes are kept out of the index at start but a template saved while Quire
  runs shows up in the Files tab (0.8 behaviour).

## Performance baselines (F0)

`quire-cli bench 1` on the 2-core build VM, **dev profile** (our crates
unoptimized, dependencies at `opt-level = 2`): F0 compared with 0.8.0 built
the same way on the same machine. These are not comparable with the §9
budgets, which are release numbers; re-record release baselines on the
reference machines with `cargo build --release -p quire-cli && quire-cli bench`.

| Measure | 0.8.0 (dev) | F0 (dev) |
|---|---|---|
| index: parse + index 2,000 notes | 4,480–4,770 ms | 4,490–4,730 ms |
| extract: whole 1 MB document | 240–243 ms | 255–305 ms |
| highlight: 100 KB | 20 ms | 21 ms |
| search "rope undo" (BM25) | 25 ms | 27 ms |
| semantic search, top 20 | 88 ms | 87 ms |
| LSA fit, 10,000 passages | 5,049 ms | 4,937 ms |

Run-to-run noise on this VM is about ±15 %. `extract` now calls
`tasks::parse_meta` per task and builds a larger `Task`; WS-E should check it
in release when the parser lands.

## Risks to know about

* **macOS Option keys:** Alt+] and Alt+[ (next/previous hunk), Alt+I and
  Alt+A type characters on macOS (‘, “, ˆ, å). 0.8's Alt+D/S/T have the same
  issue; check what egui delivers before shipping the review keys.
* **The hidden-command rule** means a forgotten `available()` makes a
  finished feature invisible; test it with a command-palette test.
* **Proposal sets are all-or-nothing** today; ingest (WS-D) that writes
  `wiki/` pages and suggests links in human notes should submit two change
  sets.
* **Legacy MCP writes** go through `submit` since WS-B (wave 1): 0.8 clients
  with `--allow-write` now make suggestions outside agent zones (§B.4).
* **Device files in tests:** `device::this_device()` never writes in unit
  tests, but the CLI and MCP server create `<config>/device.toml` on first run.
* **`Workspace::decorations` caching** is keyed by revision and
  `decor_generation`; data that changes without an edit (a new proposal
  file) must bump the generation or nothing repaints.
* **Policy approval** landed with WS-A (wave 1): a synced or hand-edited
  policy that widens access waits for approval in the Inbox.
