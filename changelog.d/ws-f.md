### Added

- **Vim keys** (Settings → Editing, or the command palette's "Vim mode"): Normal, Insert, Replace, Visual and Visual Line modes; motions, counts, operators with text objects (`ciw`, `da(`, `yip`, `dit`…), registers (`"a`–`"z`, `"0`–`"9`, and `"+` for the clipboard), `.` to repeat, `u` and Ctrl+R, `/` and `?` search, `:w`, `:q`, `:wq`, `:{line}` and `:s`/`:%s`. Each insert is one undo step. The mode shows in the status bar. Macros, marks, visual block mode and a vimrc aren't supported.
- **Edit embedded notes in place**: a `![[Note]]`, `![[Note#Heading]]` or `![[Note#^id]]` line shows that text under it, editable. Changes are saved to the embedded note, and Undo there undoes that note.
- **Tabs that don't pile up**: a note opened with one click replaces the preview tab (in italics) until you edit it or double-click it. Ctrl+click or a middle click opens a tab of its own. Settings → Editing → "Open every note in a new tab" turns this off.
- Notes reopen where you left them: the caret and the view for the last 500 notes you opened.
- Sort each folder by name, last change, newest, or by hand (drag notes into order); right-click a folder. The order is kept in the vault.
- Links to folders: `[[Projects/]]` opens `Projects/Projects.md` or `Projects/index.md`, or shows the folder in the sidebar.
- A homepage note that opens when Quire starts without notes from last time.
- "Folders and notes side by side": a two-pane browser with each note's title, date and first lines.
- Show note titles instead of file names in the sidebar and on tabs (Settings → Editing).
- **Themes**: bundled Paper, Nord, Solarized and High contrast, and your own theme files (every colour, light and dark, plus fonts; Settings → Appearance can write one to start from). A theme that can't be read leaves the built-in theme in place and says why.
- An accent colour, colours for tags (kept in the vault; right-click a tag in the sidebar), folder icons (right-click a folder) and note icons (`icon:` in front matter).
- **Git**: when the vault is a git repository, a Git tab shows what changed, commits (by itself after a quiet spell, if you like), pulls and pushes, and shows each note's git history next to its version history. Quire runs your installed git, so your credentials work; pull and push are off while Quire Sync is on.
