# Changelog

All notable changes to Quire. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow [Semantic Versioning](https://semver.org/) (pre-1.0: minor versions may break things).

## [Unreleased]

### Added
- **Quire Lite.** Every release now comes in two editions: **Quire**, and **Quire Lite**, the same app without drawn math, diagrams and PDF export (math and diagrams show as their source), at about a third of the size (a 10 MB `.deb` instead of 33 MB). Lite files start with `Quire-Lite-`; the release notes have a table of which download is which. Settings → About says which edition you're running and links to the full one. To build Lite yourself: `cargo build -p quire-desktop --no-default-features`.
- **Switch editions freely.** Both editions read and write the same notes, settings, session, dictionary and sync setup, and installing one replaces the other (the `quire-lite` package replaces `quire` and the other way round; on Windows and macOS they install over each other). Going from Quire to Lite and back loses nothing: in Lite, PDF export says it's in the full edition instead of failing, and math and diagrams draw again when you go back.

### Fixed
- Settings written by a newer version or the other edition are kept: a setting this version doesn't know about is saved back unchanged instead of being dropped.
- A settings or stored-secrets file that can't be read is kept next to the original as `<name>.unreadable` before Quire starts with defaults, instead of being overwritten on the next save.
- Settings, the open-tabs session, the sync setup and the stored-secrets file are written to a temporary file and moved into place, so a crash or full disk mid-save can't leave a half-written file. The secrets file is created readable by you only.

## [0.8.0] - 2026-11-02

### Added
- **Math.** `$…$` inline and `$$ … $$` blocks render as real math (LaTeX, typeset with Typst and the STIX Two Math font): in live preview once the caret leaves the line (click a formula to edit it), in the preview pane, and in PDF export. Fractions, roots, sums and integrals with limits, matrices, `aligned` and `\mathbb` all work. A formula that doesn't parse stays as text, with the reason shown under a block. Rendering happens off the UI thread, so typing never waits on it.
- **Diagrams.** ```` ```mermaid ```` blocks draw as diagrams (flowcharts, sequence diagrams and more, rendered in Quire itself with no browser or network) in live preview, the preview pane and PDFs, in light or dark.
- **Callouts.** Obsidian's `> [!note]`, `[!tip]`, `[!warning]`, `[!bug]` and the other types (with their aliases, custom titles and `+`/`-` fold signs) show as coloured boxes with an icon. This works in live preview, the preview pane, HTML export, published websites and PDFs. GitHub's `> [!NOTE]` works too.
- **Links to a paragraph.** `[[Note#^id]]` jumps to the paragraph or list item ending in `^id`, as in Obsidian; the `^id` is dimmed in live preview and becomes an anchor on published sites. **Copy link to this paragraph** (Ctrl+Alt+L) adds an id if needed and copies the link.
- **Spell checking.** Misspelled words get a red wavy underline (not in code, math, links' addresses, tags or front matter, and not the word you're still typing). Right-click one for suggestions, **Add to dictionary** (kept in `dictionary.txt` in Quire's settings folder) or **Ignore this session**. English (US and UK) is built in; Hunspell dictionaries already on your computer show up as more languages. Settings → Editor → Spelling.
- **Export as PDF** (Ctrl+Alt+E, or `quire-cli pdf <vault> <note>`): a typeset PDF in `Exports/` with headings, callouts, code, tables, task lists, images, math and diagrams. It's made in the background.
- **Quick capture.**
  - From any app, **Ctrl+Alt+Space** (**⌘⇧Space** on macOS) brings up a small box; what you type is added to today's daily note as `- 14:05 your text`, and Quire goes back to the background if it was minimized. It's also in the command palette, and `quire-cli capture <vault> <text>` does the same from scripts.
  - Text goes into the daily note even when that note is open with unsaved typing, without overwriting anything.
  - It can be turned off in Settings → Editor. On Linux the shortcut needs X11 or XWayland.
- **Third-party notices** in Settings → About, with the font and word-list licences.

### Changed
- Release builds unwind on a panic instead of aborting, so a bug in a math, diagram or PDF renderer reports an error instead of closing the app.

### Fixed
- On Windows, typing a character with AltGr (é, ł, ń, €…) no longer triggers a Ctrl+Alt shortcut such as Version history (Ctrl+Alt+H) or New project (Ctrl+Alt+N): AltGr arrives as Ctrl+Alt there.


## [0.7.0] - 2026-11-02

### Added
- **History from every device.** With sync on, Version history has an **All devices** view: every version of the note that the sync folder or server holds, with which device wrote it and when, including a version that lost a race in a synced folder. Pick one to see what changed and restore it (undoable). Needs a sync server from this release or later; synced folders work as they are.
- **Images and files sync too.** Quire Sync now carries attachments (images, PDFs, any file in the vault up to 10 MB), encrypted like notes, through a synced folder or a server. Renames and deletes follow, and when two devices change the same file both versions are kept (files are never merged line by line). The sync state remembers attachments by hash, so it stays small.
- **Split editor.** Two notes side by side: **Split editor** (Ctrl+Alt+\\) shows the last note you had open next to this one (or asks which note), and **Open to the side** in a note's right-click menu opens it in the other half. Each half has its own scroll position and cursor; click into a half (or its title) to work in it, and links, quick open and new notes go to the half you're in.
- **Fold sections.** Hover a heading for an arrow beside it; click to fold everything up to the next heading of the same level, so long notes read like an outline. A folded heading shows a ⋯ pill (click it to unfold). **Fold or unfold this section** (Ctrl+Shift+[), **Fold all sections** (Ctrl+Alt+[) and **Unfold all** (Ctrl+Alt+]). Moving the cursor into a folded section opens it, and ↑/↓ skip over folded lines.
- **Graph view.** **Graph view** (Ctrl+G) draws your notes as dots and their links as lines: around the open note (1, 2 or 3 links away) or across the whole vault, with notes that have no links shown on request. Hover a note to light up what it links to, click it to open it, drag a note to move it, scroll to zoom and drag the background to pan. It lays out thousands of notes without stalling the window, and the same vault opens to the same picture each time.

### Changed
- Saving no longer waits on version history: snapshots are compressed and written on a background thread, in order. Autosave of a 400 KB note took about 30 ms of the UI thread before.
- Published websites show `quire-view` boards, tables, lists and calendars as real HTML, listing only notes that are published, instead of the raw code block.

### Fixed
- **Version history no longer runs away when you scroll.** In 0.6.0 the dialog grew a little on every repaint, so scrolling the list pushed the title and buttons off screen and left an empty, endlessly scrolling panel. The dialog now has a fixed size that fits small windows, the version list and the changes scroll inside it, and long comparisons show every line (they used to stop silently after 2,000) and are computed once instead of on every frame.
- The keyboard shortcuts sheet fits narrow windows (one column, long names shortened) instead of running off the side.
- A new test opens every dialog on a normal and a small window, scrolls and moves the pointer for many frames, and fails if any dialog changes size or doesn't fit, so this kind of bug can't come back unnoticed.
- Sync server and client: replies are paged by size as well as count, and the client accepts replies up to 96 MB, so big revisions no longer fail with "response body is larger than request limit".
- Folder sync: a change another device wrote to the folder while Quire was starting sync could wait up to two minutes (until the next routine check) to arrive. It now arrives within a couple of seconds.
- Folder sync: when three or more devices edited the same lines before seeing each other's changes, the winning text could be saved as a conflict copy twice. Now the winner stays in the note on every device and each other device keeps only its own version beside it.

## [0.6.0] - 2026-11-02

### Added
- **Sync through a folder you already sync, free.** Quire Sync can now use a folder that Dropbox, iCloud Drive, Google Drive, OneDrive or Syncthing keeps on all your devices, with no server to run. It's the default in Settings → Sync, which offers the synced folders it finds on your computer. Notes are encrypted before they reach the folder, so the sync service can't read them or their names. Each device writes only to its own files there, so the sync tool never makes conflicted copies, and edits made on two devices before either saw the other merge as they do with a server.
- **Suggestions as you type.** `[[` lists notes to link (recent ones first, then by name as you type, or a new note); `#` lists the tags you already use, most used first; `/` at the start of a word opens a menu of headings, lists, checklists, quotes, code blocks, tables, dividers, links, embeds, board/table/calendar views, today's date, the time and your templates (with `{{cursor}}` where the caret goes). ↑ ↓ to choose, Enter or Tab to insert, Escape to close. Nothing is suggested inside code blocks.
- **Publish as a website, free.** The command palette's *Publish as a website…* (or `quire-cli publish <vault> <folder>`) turns the notes marked `publish: true`, or all notes, into a folder of web pages. Each page has its links, backlinks, tags and heading anchors, and the site has a home page, a tags page and search that runs in the browser. It works with GitHub Pages, Netlify, Cloudflare Pages or any web server. Links to notes you didn't publish become plain text, so the site never points at a private note.

### Changed
- **A refreshed look:**
  - **Fonts:** Inter and JetBrains Mono now ship with the app, so text looks the same on macOS, Windows and Linux (before, macOS and Linux fell back to whatever font was around), with a real italic face.
  - **Notes in live preview:**
    - Code blocks and front matter sit on one rounded background.
    - Quotes get a bar.
    - Inline code and tags are tight chips.
    - List bullets are dots, `---` is a rule, and tables line up as a grid with a bold header.
    - Checklists show real checkboxes that you can click to tick, without moving the caret.

    The Markdown appears again on the line or table you're editing.
  - **Lists and controls:**
    - The sidebar, command palette and quick open have full-width rows with hover states.
    - The command palette shows each command's group quietly and its shortcut in line.
    - The top bar has a search box that opens the palette, labelled with this platform's shortcut (it said ⌘ on Windows and Linux).
    - Buttons are softer and more rounded, scroll bars are thin and appear on hover, dialogs have softer shadows, and the editor has wider margins.
  - **Shortcuts dialog:** the keyboard shortcuts line up across groups.
- **Faster in big notes:** the Insights panel works out related notes and suggestions in the background for notes over 64 KB, so typing doesn't stutter while it does (about 50 ms per save on a 400 KB note before).
- **Less battery in the background:** the caret stops blinking, and Quire stops redrawing, while you're in another app.
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

[Unreleased]: https://github.com/BhanuNexus/quire/compare/v0.8.0...HEAD
[0.8.0]: https://github.com/BhanuNexus/quire/compare/v0.7.0...v0.8.0
[0.7.0]: https://github.com/BhanuNexus/quire/compare/v0.6.0...v0.7.0
[0.6.0]: https://github.com/BhanuNexus/quire/compare/v0.5.0...v0.6.0
[0.5.0]: https://github.com/BhanuNexus/quire/compare/v0.4.1...v0.5.0
[0.4.1]: https://github.com/BhanuNexus/quire/compare/v0.4.0...v0.4.1
[0.4.0]: https://github.com/BhanuNexus/quire/releases/tag/v0.4.0
