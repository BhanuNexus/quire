### Added
- While you review an agent's suggestions, a bar above the note says who suggests what and why, with Prev, Next, Accept all, Reject all and Done.
- Version history says who made each version (you, an agent, or you accepting an agent's suggestion), links to that agent's activity, and has a Git tab with the note's commits when the vault is in a git repository.
- `AGENTS.md` (or `CLAUDE.md`) is pinned at the top of the Files tab, marked when it waits for your approval.
- Obsidian `.base` files appear in the Files tab and quick open, and open in their own view.
- New note from template asks the template's questions in a dialog and puts the note in the template's folder, named by its `name:` (or the title you typed).
- Nested properties are shown and edited field by field in the Properties panel.
- Settings → Sync: "Also sync vault settings and review history".
- Settings → Editing: choose whether Ctrl+N makes the note in the open note's folder (where that folder's template applies) or at the top of the vault.
- With "Show note titles" on, quick open, the graph and link suggestions use titles too (`[[name|Title]]` when they differ).
- Themes can set `added`, the colour of suggested text and added lines.
- Icons and task date emoji (📅 ⏳ ⏰ 🔁) display on every system.

### Changed
- Settings lists its tabs down the left side.
- Undo an agent's changes, Capture web page, Rename tags and properties, and flashcard review open as dialogs.
- Ctrl+N makes the new note in the open note's folder by default.
- The MCP `tasks` tool also returns `start`, `completed`, `reminder` and `time`.

### Fixed
- Alt shortcuts (Alt+], Alt+[, Alt+I, Alt+D…) no longer type their character into the note.
- Follow link on `[[Folder/]]` opens the folder's note or shows the folder, instead of creating a note.
- With Vim keys, Escape in Insert mode returns to Normal mode instead of closing the find bar.
- Headings, links and quotes use the theme's colours in the classic editor and in embedded notes too.
