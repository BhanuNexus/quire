# WS-F hand-off: requests for files WS-F doesn't own

WS-F (editor and UI) worked around each of these; each is a small change in
a frozen or other-owned file. Ordered by how much a user would notice.

## Requests

0. **Emoji for icons** (`apps/quire-desktop/Cargo.toml`). The design draws
   note and folder icons "with egui's emoji font", but egui 0.37 bundles it
   only with the `monochrome_emoji_fonts` feature, which isn't on. Please
   change the desktop's `egui.workspace = true` to
   `egui = { workspace = true, features = ["monochrome_emoji_fonts"] }`
   (no new crates; about 0.75 MB in both editions). Until then WS-F loads a
   monochrome emoji font if there is one (`NotoEmoji-Regular.ttf` in
   Quire's fonts folder, Noto Emoji, Segoe UI Emoji or Symbol, Apple
   Symbols); without one, icons show as boxes.

1. **Folder links with Alt+Enter** (`crates/quire-app/src/workspace.rs`,
   `open_link`). `[[Projects/]]` opens the folder's note or shows the folder
   when Ctrl+clicked in the editor (WS-F's view handles it), but Follow link
   (Alt+Enter) goes through `Workspace::open_link`, which creates a note
   called "Projects". One line at the top of `open_link`:
   `if let Some(r) = crate::nav::folder_link(self, target) { return match r { crate::nav::FolderLink::Note(p) => self.open_note(&p), crate::nav::FolderLink::Folder(_) => None }; }`

2. **Escape in Vim's Insert mode while the find bar is open**
   (`apps/quire-desktop/src/app.rs`, `ui_inner`). The app takes Escape to
   close the find bar before the editor sees it, so with Vim keys on, Escape
   in Insert mode closes the find bar instead of returning to Normal mode.
   Skip that when `ws.settings.editing.vim && ws.features.nav.vim.typing()`.

3. **Display titles in quick open, the graph and link suggestions** (design
   §E.1; `overlays.rs` quick open, `graph_view.rs`, `crates/quire-app/src/suggest.rs`).
   The sidebar and tabs show titles with `editing.display_titles` on; use
   `quire_app::nav::display_name(ws, path)` for the label, match both title
   and file name in quick open, and insert `[[name|Title]]` from link
   suggestions when they differ.

4. **A Git tab in Version history** (`dialogs.rs`, design §F.5). WS-F shows a
   note's git history (and each commit's change to it) in the Git tab of the
   right panel. The history dialog can show the same with
   `quire_app::git::request_log(ws, path)` / `request_show(ws, entry)`; the
   results arrive in `ws.features.git.log` and `.shown`. `git_ui.rs` has the
   drawing code to reuse.

5. **Theme colours in the shared highlighter** (`apps/quire-desktop/src/highlight.rs`).
   `format_for` colours headings with `strong`, links with `accent`, quotes
   with `muted` and conflicts with `warn`. The fast editor recolours with a
   theme's `h1`–`h6`, `link`, `quote` and `conflict` (and tag colours, and the
   theme's text font) after `build_job`; the classic editor and the embedded
   editors don't. Use `pal.h[level - 1]`, `pal.link`, `pal.quote`,
   `pal.conflict` there, and key `HighlightCache` on the palette rather than
   on `pal.dark` alone (WS-F clears the classic editor's caches when the
   theme changes as a stopgap).

6. **`Workspace::open_hidden`** (design §F.2) doesn't exist; WS-F keeps the
   hidden documents of embeds in `ws.features.nav` (`nav::embed_doc`), saves
   and reloads them itself. Nothing is needed unless another feature wants
   hidden documents too.

## Small edits made outside WS-F's files

* `crates/quire-text/src/lib.rs`: `pub mod vim;` (the module the design
  places there).
* `apps/quire-desktop/src/main.rs`: `mod embed_edit;`, `mod git_ui;`,
  `mod theme_ui;` (the new files F0-notes lists for WS-F).
* `_typos.toml`: excludes `crates/quire-text/testdata/vim/` (Vim key
  sequences and words cut by the cursor marker read as typos).
* `docs/formats/README.md`, `settings.md`, `vault-prefs.md`: rows for WS-F's
  files and sections.
