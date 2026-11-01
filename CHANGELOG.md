# Changelog

All notable changes to Quire. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow [Semantic Versioning](https://semver.org/) (pre-1.0: minor versions may break things).

## [Unreleased]

### Added
- Keyboard shortcut cheat sheet (Ctrl+/ or the **?** button), generated from the command list.
- A five-step first-run tour (Help: Take the tour to see it again).
- Quick open (Ctrl+P) with an empty query lists your recently opened notes first. The list is remembered between sessions and follows renames and moves.
- Safer sync with Dropbox, iCloud, Syncthing, Nextcloud and similar tools. When a note you're editing also changes elsewhere, Quire merges the two line by line, keeping your caret, and Undo takes the merge back out. If both sides changed the same lines, autosave pauses and a banner lets you keep yours, take theirs, or keep both between conflict markers.
- Conflict copies made by sync tools (`Note (… conflicted copy …).md`, `Note.sync-conflict-….md`) are detected. The status bar shows how many there are, and **Resolve sync conflicts…** merges each one back into its original, using version history to find what changed on each side.
- Conflict markers (`<<<<<<<`, `=======`, `>>>>>>>`) are highlighted in the editor and shown as plain lines in the preview, rather than turning into a heading or a quote.

### Changed
- An open note that changes on disk is updated with only the lines that changed, so the caret, scroll position and undo history are kept.

### Fixed
- Quick open could show only the first couple of results when a longer list followed a shorter one.

## [0.3.1] - 2026-11-01

First public release.


### Added
- New notes take their name from their first line until you rename them yourself.
- Right-click menu on notes in the sidebar and on tabs: Rename, Move to folder, Version history, Copy link, Show in folder, Move to trash. Double-click a tab to rename it.
- **+ New** menu: Note, Project, Note in folder, From template. Right-click a folder for "New note here".
- **New project** (Ctrl+Alt+N): creates `Projects/<name>.md` with status, due date and owner, and a project board on first use. Customise it with `Templates/Project.md`.
- Move notes between folders. Path-style links are rewritten.

### Changed
- Licensed under the GNU AGPL v3 or later. Settings → About links to the source; the installers include the licence.
- Strict linting: clippy pedantic + nursery + selected restriction lints, rustdoc and typos checks in CI. The code base was updated to pass them.
- Rust toolchain pinned to 1.97.0 for reproducible builds. GitHub Actions updated to their latest versions and pinned by commit SHA.
- Upgraded toml to 1.1 and png to 0.18.

### Fixed
- Notes sometimes reopened scrolled to another note's position.

## [0.3.0] - 2026-11-01

### Added
- Version history with diffs and undoable restore (Ctrl+Alt+H).
- Properties panel and `quire-view` blocks: table, list, kanban (drag to change a field) and calendar.
- Importers for Logseq, Notion, Bear, Evernote and Markdown folders, with a dry run first.
- AI providers: OpenAI-compatible (Ollama, LM Studio, OpenAI) and Anthropic, with keys stored in the OS keychain. Optional neural embeddings from an endpoint.
- MCP server: `quire-cli mcp`, read-only by default.
- Settings dialog. Screen-reader info for the fast editor. Inline IME pre-edit.
- Windows installer and portable zip; macOS universal app via CI.

### Fixed
- Editing shortcuts (Ctrl+A, Ctrl+Z…) acted on the note while another text field had focus.

## [0.2.0] - 2026-11-01

### Added
- Virtualized fast editor that stays smooth on 10 MB files, with multiple cursors.
- Find and replace in a note and across the vault, with regex.
- Image paste and file drop, with inline images.
- Hybrid search (keywords and meaning) and Ask your notes with citations.

## [0.1.0] - 2026-11-01

### Added
- First version: live Markdown styling, wikilinks with rename refactoring, backlinks, related notes, daily notes, templates, command palette, quick open, inline calculator and natural-language dates.

[Unreleased]: https://github.com/BhanuNexus/quire/compare/v0.3.1...HEAD
[0.3.1]: https://github.com/BhanuNexus/quire/releases/tag/v0.3.1
[0.3.0]: https://github.com/BhanuNexus/quire/releases/tag/v0.3.0
[0.2.0]: https://github.com/BhanuNexus/quire/releases/tag/v0.2.0
[0.1.0]: https://github.com/BhanuNexus/quire/releases/tag/v0.1.0
