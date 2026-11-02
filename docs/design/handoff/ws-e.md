# WS-E hand-off: requests and notes for integration

What WS-E needs from files it doesn't own, and the small additions it made
at registration points. Everything below is small; none of it blocks
WS-E's features, which work around it today.

## Requests (one-line follow-ups on `main`)

1. **Let a feature open its UI without a command** (app.rs, F0).
   `show_feature` runs only for `UiRequest::Feature(cmd)`. WS-E's dialogs
   (Rename tag/property, Review) open from commands, so they work; but a
   feature can't bring up its panel from a click inside a note widget or a
   view. Proposal: `registry::wanted_ui(&mut Workspace) -> Option<Command>`
   polled once a frame in `QuireApp::ui_inner`, calling
   `self.show_feature(cmd)` when `Some`. WS-E would return the pending
   dialog's command from `ws.features.tasks`.
2. **Feature dialogs as overlays** (registry + app.rs). WS-E hosts its
   modals from a right-panel tab ("plan", offered only while a dialog is
   open), because `UiTarget` has no overlay variant. Proposal:
   `UiTarget::Dialog(fn(&mut Workspace) -> Box<dyn OverlayView>)` handled in
   `show_feature` as `self.overlay = Some(Overlay::Feature(make(&mut self.ws)))`.
   Then `Overlay` would also pause jobs while they're open. The
   `OverlayView` impls exist already (`RefactorOverlay`, `ReviewOverlay`,
   in `layout_samples`).
3. **Open `.base` files** (WS-F, file tree / quick open). `open_surface`
   returns `BaseSurface` for `.base`, but nothing calls `open_surface` yet
   (the file tree lists notes only). Also: after a surface's `PanelOut`
   with `jumped` (a note opened from the base), `app.rs` should set
   `surfaces.active = None` as it does for the tab bar, or the base stays
   in front of the note.
4. **"New note from template…" with questions** (workspace.rs / overlays.rs,
   F0). The 0.8 palette flow calls `Vault::create_from_template`, which
   can't ask. WS-E made it keep unanswered `{{prompt:…}}` placeholders and
   the note then shows a fill-in form, so nothing is lost; but `folder:` and
   `name:` from `quire-template:` aren't applied there. Proposal: in
   `Action::Template`, call `ws.new_note_from_template(&t, None, &answers,
   &[])` after asking (or route to a WS-E dialog when
   `ws.template_prompts(&t)` isn't empty).
5. **Ctrl+N in a folder** (WS-F / workspace.rs). `applies_to` folder
   templates apply to a new empty note created in that folder (New note in
   folder, the file tree's "New note here"). Ctrl+N always makes the note at
   the vault root, so it never applies; consider making Ctrl+N use the
   active note's folder.
6. **Properties panel tree** (view_ui.rs, unowned). Nested maps now parse
   (`Kind::Map`). The properties panel still shows a map as one flow-form
   text box; editing it there would turn the map into a string. WS-E added
   `Kind::Map => "nested fields"` only. Proposal (WS-F or WS-E later): for
   `Kind::Map`, show the leaves with `Value::leaves()` and edit them with
   `frontmatter::set_field_path` / `remove_field_path` /
   `rename_field_path` on the active document (all byte-minimal).
7. **Display titles** (WS-F). The data side is unchanged and works:
   `NoteRecord.title` is the `title:` field, else the first H1, else the
   name. Nothing more from WS-E.
8. **Emoji in the editor font** (theme.rs, F0/WS-F). The bundled fonts have
   no glyphs for the Tasks emoji (📅 ⏳ 🛫 ⏰) or for ▸/▾, so task lines in
   notes and snippets show boxes where the dates' emoji are (seen on Linux;
   the system emoji font isn't used). WS-E's own UI avoids them (triangles
   are drawn, labels use words). Proposal: add a small emoji fallback font,
   or Noto Emoji's subset for these code points, to `install_fonts`.

## Small additions WS-E made outside its own files

* `crates/quire-markdown/src/lib.rs`: `pub mod srs;` (the design's new
  file); `crates/quire-vault/src/lib.rs`: `mod templates` → `pub mod
  templates` (the two-phase API); `crates/quire-import/src/lib.rs`: `pub mod
  obsidian;`; `crates/quire-app/src/lib.rs`: `pub mod reminders;`, `pub mod
  templates_ui_state;`, `pub use quire_plan as plan;`;
  `apps/quire-desktop/src/main.rs`: `mod` lines for the four new UI files.
* `crates/quire-import/Cargo.toml`: `yaml-rust2` (for `.base`, per §5).
* `apps/quire-desktop/src/view_ui.rs` (a 0.8 file nobody owns): two match
  arms send `from: tasks` and the agenda/timeline layouts to
  `agenda_ui::view`, three lines add the "+ New" button, and `kind_name`
  names `Kind::Map`. These were needed because `Layout` gained variants.
* `registry.rs`: WS-E block entries, including a `// ---- WS-E` marker in
  `SETTINGS_TABS` (Settings → Planning), and a `paint_widget` arm.

## Notes for other workstreams

* **Task text changed**: `Task.text` no longer contains the trailing
  fields (`📅 …`, `[due:: …]`) or a trailing `^block-id`; tags among the
  fields stay. Day Planner times move to `Task.time_block`. WS-B's MCP
  `tasks` tool gets clean text plus the fields.
* `Task.priority` follows Obsidian Tasks: 0 🔺 highest, 1 ⏫ high, 2 🔼
  medium, 3 🔽 low, 4 ⏬ lowest, `None` normal (the doc comment in
  `extract.rs` says "0 = highest (⏫)"; ⏫ is *high* in Obsidian Tasks).
  `quire_markdown::tasks::priority_rank` sorts them.
* Undo of tag and property batches uses WS-E's own "restore texts" path
  (exact restore, else a three-way merge that keeps later edits, else the
  note is left alone and named). Each change is in the ledger with actor
  human, op `write`, a `batch` id and `other.title`, so WS-A's batch revert
  can take over later.
* Dataview/Tasks query translation lives in `quire_index::views::{dataview,
  tasks_query}` (next to the views it produces and below `quire-app`,
  where `view_ui` and decorations need it); `.base` and Kanban parsing are
  in `quire_import::obsidian` as designed.
