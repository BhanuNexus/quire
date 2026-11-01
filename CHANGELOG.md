# Changelog

All notable changes to Quire. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow [Semantic Versioning](https://semver.org/) (pre-1.0: minor versions may break things).

## [Unreleased]

### Added
- **Sync through a folder you already sync, free.** Quire Sync can now use a folder that Dropbox, iCloud Drive, Google Drive, OneDrive or Syncthing keeps on all your devices, with no server to run. It's the default in Settings → Sync, which offers the synced folders it finds on your computer. Notes are encrypted before they reach the folder, so the sync service can't read them or their names. Each device writes only to its own files there, so the sync tool never makes conflicted copies, and edits made on two devices before either saw the other merge as they do with a server.

- **Suggestions as you type.** `[[` lists notes to link (recent ones first, then by name as you type, or a new note); `#` lists the tags you already use, most used first; `/` at the start of a word opens a menu of headings, lists, checklists, quotes, code blocks, tables, dividers, links, embeds, board/table/calendar views, today's date, the time and your templates (with `{{cursor}}` where the caret goes). ↑ ↓ to choose, Enter or Tab to insert, Escape to close. Nothing is suggested inside code blocks.
- **Publish as a website, free.** The command palette's *Publish as a website…* (or `quire-cli publish <vault> <folder>`) turns the notes marked `publish: true`, or all notes, into a folder of web pages. Each page has its links, backlinks, tags and heading anchors, and the site has a home page, a tags page and search that runs in the browser. It works with GitHub Pages, Netlify, Cloudflare Pages or any web server. Links to notes you didn't publish become plain text, so the site never points at a private note.

### Changed
- When the same note name arrives from two devices, every device now gives the numbered name to the same one, whichever it saw first.

## [0.5.0] - 2026-11-01

### Added
- **Quire Sync (beta).** End-to-end encrypted sync between your devices through a server you run yourself (`quire-sync-server`, one binary or a Docker image; see `docs/self-hosting.md`). Set it up in **Settings → Sync**. The server never sees your notes or their names. Changes appear on your other devices within a second or two. Edits to the same note on two devices merge line by line; edits to the same lines keep both versions, and the other device's version is saved next to the note. A note edited on one device and deleted on another is kept. Notes deleted elsewhere go to the vault's trash. The status bar shows whether you're synced.
- Edit table views in place: click a cell in a `quire-view` table to change that property in its note (Enter or clicking away saves, Escape cancels). Yes/no properties are checkboxes, list properties stay lists, and computed columns (title, tags, words…) stay read-only.
- Linux: `quire-sync-server-<version>-linux-x86_64.tar.gz`, the self-hosted sync server, with the release's checksums and attestations.
- Live preview: Markdown syntax (`**`, `#`, `[[ ]]`, backticks, link targets) is hidden except on the line you're editing, so notes read cleanly while staying plain Markdown. List bullets, checkboxes, quotes and code fences stay visible. Turn it off in Settings → Editor.

## [0.4.1] - 2026-11-01

### Added
- Opt-in update check. Turn on "Check for a new version once a day" in Settings → About or on the last tour card, or run **Check for updates** any time. Quire asks GitHub for the latest release, sending nothing about you or your notes, and shows a link in the status bar when a newer version is out. Right-click the link to skip that version.
- Linux: a `.deb` package for Debian and Ubuntu and an AppImage, alongside the `.tar.gz`. Both include AppStream metadata, so software centres show a proper description and screenshots.

### Changed
- Network requests (AI providers, the update check) trust the operating system's certificate store, so company proxies and certificates you've installed work as they do in your browser.
- README: new screenshots (light and dark), and the right first-launch steps for macOS 15 and later, which no longer allow right-click → Open for unsigned apps.

## [0.4.0] - 2026-11-01

First public release. Quire is a fast, local-first Markdown notepad: your notes are plain files in a folder you choose, and nothing leaves your computer unless you turn on an AI provider. Earlier versions were never published; CHANGELOG.md keeps their history.

Downloads for Windows, macOS (universal) and Linux are attached to the release, with SHA-256 checksums, build provenance attestations and an SBOM. See the README for how to verify them.

### Added
- Safer sync with Dropbox, iCloud, Syncthing, Nextcloud and similar tools. When a note you're editing also changes elsewhere, Quire merges the two line by line, keeping your caret, and Undo takes the merge back out. If both sides changed the same lines, autosave pauses and a banner lets you keep yours, take theirs, or keep both between conflict markers.
- Conflict copies made by sync tools (`Note (… conflicted copy …).md`, `Note.sync-conflict-….md`) are detected. The status bar shows how many there are, and **Resolve sync conflicts…** merges each one back into its original, using version history to find what changed on each side.
- Conflict markers (`<<<<<<<`, `=======`, `>>>>>>>`) are highlighted in the editor and shown as plain lines in the preview, rather than turning into a heading or a quote.
- Keyboard shortcut cheat sheet (Ctrl+/ or the **?** button), generated from the command list.
- A five-step first-run tour (Help: Take the tour to see it again).
- Quick open (Ctrl+P) with an empty query lists your recently opened notes first. The list is remembered between sessions and follows renames and moves.
- New notes take their name from their first line until you rename them yourself.
- Right-click menu on notes in the sidebar and on tabs: Rename, Move to folder, Version history, Copy link, Show in folder, Move to trash. Double-click a tab to rename it.
- **+ New** menu: Note, Project, Note in folder, From template. Right-click a folder for "New note here".
- **New project** (Ctrl+Alt+N): creates `Projects/<name>.md` with status, due date and owner, and a project board on first use. Customise it with `Templates/Project.md`.
- Move notes between folders. Path-style links are rewritten.
- Linux build: a portable `tar.gz` with an install script and desktop entry.
- `quire --version` and `quire-cli --version`.

### Changed
- Licensed under the GNU AGPL v3 or later. Settings → About links to the source; the installers include the licence.
- An open note that changes on disk is updated with only the lines that changed, so the caret, scroll position and undo history are kept.
- Strict linting: clippy pedantic + nursery + selected restriction lints, rustdoc and typos checks in CI.
- Rust toolchain pinned to 1.97.0 for reproducible builds. GitHub Actions pinned by commit SHA.
- Upgraded toml to 1.1 and png to 0.18.

### Fixed
- Quick open could show only the first couple of results when a longer list followed a shorter one.
- Notes sometimes reopened scrolled to another note's position.
- Renaming a note whose front matter had `title:` set to its old name left search results and boards showing the old name. The title now follows the rename.
- Quire no longer touches the OS keychain at startup. It's used only when an AI provider that needs a key is configured, and macOS and Windows no longer get a test entry written to check that the keychain works.

## 0.3.0 - 2026-11-01 (not published)

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

## 0.2.0 - 2026-11-01 (not published)

### Added
- Virtualized fast editor that stays smooth on 10 MB files, with multiple cursors.
- Find and replace in a note and across the vault, with regex.
- Image paste and file drop, with inline images.
- Hybrid search (keywords and meaning) and Ask your notes with citations.

## 0.1.0 - 2026-11-01 (not published)

### Added
- First version: live Markdown styling, wikilinks with rename refactoring, backlinks, related notes, daily notes, templates, command palette, quick open, inline calculator and natural-language dates.

[Unreleased]: https://github.com/BhanuNexus/quire/compare/v0.5.0...HEAD
[0.5.0]: https://github.com/BhanuNexus/quire/compare/v0.4.1...v0.5.0
[0.4.1]: https://github.com/BhanuNexus/quire/compare/v0.4.0...v0.4.1
[0.4.0]: https://github.com/BhanuNexus/quire/releases/tag/v0.4.0
