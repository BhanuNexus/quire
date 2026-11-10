# Changelog

All notable changes to Quire. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow [Semantic Versioning](https://semver.org/) (pre-1.0: minor versions may break things).

## [Unreleased]

## [0.12.0] - 2026-11-11

Quire 0.12: switching tools (import from more apps and undo it, tidy notes, a Home page), typed links and a hierarchy you can browse (Dataview and Breadcrumbs syntax), sync straight to S3-compatible storage or WebDAV, an import wizard with five more sources, a Markdown tidier and a Home tab, locked folders' notes and pictures in every view while unlocked, and a faster start.

### Added

#### Typed links and hierarchy
- **Typed links, as Dataview and Breadcrumbs write them:** a link in a field has that field's name as its type. Inline fields on their own line (`parent:: [[Rope]]`), bracketed anywhere (`[supports:: [[Theory]]]`, `(source:: [[Book]])`), and front-matter properties holding links (`up: "[[Projects]]"`, lists of links). Front-matter links are now links everywhere: backlinks, the graph, renames, vault checks. Format and the mapping from both plugins: [docs/formats/links.md](docs/formats/links.md).
- **Hierarchy:** `up`/`parent`, `down`/`child`, `same`/`sibling`, `next` and `prev` are hierarchy links (set your own names in `[links]` of `.quire/shared/vault.toml`); A `up` B makes B's child A without writing it twice (turn off with `implied = false`). Every other field is a type of its own: `supports`, `contradicts`, `source`…
- **Breadcrumbs trail** above a note ("Maths › Calculus › Limits"), one line per path when a note has several parents, stopping at cycles. On by default only in vaults that use hierarchy fields (`trail = "on" | "off" | "auto"`).
- **Insights groups links by type:** Parents, Children, Siblings, Previous, Next, Supports, Contradicts and other types, then the untyped backlinks. Implied links are dimmed; `←` marks links from other notes.
- **Hierarchy** sidebar tab ("Show hierarchy"): the up/down tree from each top note, or just the open note's, with expand and collapse; a note met again in its own branch (a cycle) is shown once more, marked "again", and not expanded.
- **Graph:** typed links are coloured by type, can be filtered (all, typed, one type), and show their type on hover.
- **Add typed link…**: pick a type, then a note; it is written the way the note already writes typed links (inline field or property), as one undoable edit. **Go to parent**, **Go to next sibling** and **Go to previous sibling** (no default keys; bind them in Settings → Commands).
- **Suggestions:** after `parent:: ` (any field that holds links) the notes to link; `[[` at the start of a line also offers the vault's typed fields (`up:: [[…]]`).
- **Views, Dataview and Bases by type:** `where: up = Projects` (also matching `up: "[[Projects]]"`), `group: up`, `outlinks` and `outlinks.<type>`; Dataview's `WHERE up = [[X]]` and `contains(file.outlinks, [[X]])` translate; in a `.base` an inline typed field is a property.
- **Agents (MCP):** `links` (a note's links and backlinks with their types), `hierarchy` (ancestors, descendants, siblings, previous/next and trails, depth-limited) and `add_link` (through the usual review: applied in agent zones, a suggestion elsewhere); `neighbors` edges carry their type and can follow chosen types. Clients limited to tags or folders, and locked folders, stay hidden in all of them, field names included.
- **CLI:** `quire-cli links <vault> <note> [--type T]` and `quire-cli tree <vault> <note> [--depth N]`.

#### Sync through S3-compatible storage and WebDAV
- **Quire Sync straight to a bucket or a WebDAV folder you own:** AWS S3, Cloudflare R2, Backblaze B2, Wasabi, MinIO or any S3-compatible storage, and Nextcloud, ownCloud, Fastmail, Synology or `rclone serve webdav`. Settings → Sync → *S3-compatible storage* / *WebDAV* (provider presets, **Test connection**, plain errors for a wrong key, expired credentials, the clock, the region), or `quire-cli sync join --s3 ENDPOINT --bucket B --region R` / `--webdav URL --user U`. Setup, a minimal IAM policy and costs per provider: [docs/sync-backends.md](docs/sync-backends.md).
- **End-to-end encrypted like every Quire Sync:** the storage holds only ciphertext under ids and numbers. What it can see (counts, sizes, timing) is listed in [docs/design/0.12-sync.md](docs/design/0.12-sync.md#2-what-the-storage-can-see).
- **Two devices syncing at once never lose a revision**, on any of these services, with or without conditional writes (`If-None-Match`; B2 has none): each device writes only its own files, once, and concurrent edits merge line by line or keep a conflict copy, as with folder sync. Changes are kept on the device until uploaded, so an upload cut short resumes on the next pass. **A revision object deleted or damaged in the storage doesn't stop sync:** other devices skip it, sync everything after it and name it in the status, and the device that wrote it uploads it again. Revision headers are authenticated, so the storage can't change which revision one builds on. Selective sync, the sync lock, locked folders (still ciphertext) and shared review history work as before.
- Credentials stay in the system keychain, never in the vault; `quire-cli sync credentials` (or *Update credentials…*) replaces expired ones after testing them. `quire-cli sync test` checks the storage; `sync status` names it. Requests are signed with SigV4 (checked against AWS's and botocore's test vectors), over rustls with the system's certificates, with retries and backoff; redirects are never followed and `http://` is refused except to this computer unless allowed (and never goes through a proxy from the environment). An address with a user name or password in it is refused; the CLI says when it takes `AWS_*` variables from the environment.
- **`quire-cli sync move <vault> --s3 …|--webdav …|--folder …|--server …`** moves a vault to another place: a last pass, then a new sync vault there; other devices join it and their notes are recognised, not copied. The old setup is removed only once the new vault exists; changes not yet uploaded to S3 or WebDAV stop the move unless `--force`.

#### Switching tools: import, tidy, Home
- **File → Import… is a wizard:** pick the source and file or folder, read a **dry-run preview** (counts, warnings, name clashes, a few converted notes), choose the target folder, import in the background with progress and Cancel, then a report. **The whole import can be undone** (Undo import, the command palette, or `quire-cli import --undo last`): only files still exactly as the import wrote them go to the trash; notes you edited since stay. Format: [docs/formats/import-record.md](docs/formats/import-record.md); guide: [docs/import.md](docs/import.md).
- **Five more sources:** Roam Research (JSON), Google Keep (Takeout), Joplin (`.jex` or RAW folder), Day One (JSON) and a folder of HTML files (Apple Notes, OneNote and saved pages). Attachments, dates, tags and internal links (as `[[wikilinks]]`) are kept. Every importer refuses paths that leave the export, skips links, hidden folders and device names, stops at size and depth limits and archive bombs, and decodes other encodings; each has hostile-input tests. `quire-cli import` gained `--from`, `--list` and `--undo`.
- **Tidy notes (a Markdown linter and formatter)**, the way Obsidian Linter does: headings, list markers and indentation, emphasis, blank lines, trailing spaces, line endings, front matter order and tags, `[[ link | spacing ]]`, an optional "date modified". Settings → Linter, stored in `[lint.format]` of `vault.toml` ([docs/formats/lint-format.md](docs/formats/lint-format.md)). **Tidy this note** is one undo step; **Tidy when I save** (off by default); **Tidy notes in a folder…** shows each note's diff first, writes through the safe path with a history version per note, and is one batch you can undo. It never touches code, math, HTML or front matter it can't read, and changes only whitespace and markup: each rule is checked against the parse and undone if it would change more. `quire-cli format <vault> [note-or-folder] [--write]`.
- **Home**, an optional start page (`[home]` in `vault.toml`, Settings → Home; [docs/formats/home.md](docs/formats/home.md)): recent notes, pinned notes (**Pin to Home**), today's tasks and agenda, a view or a `.base` view, what waits in the Inbox, quick capture, a random note, today's note and writing counts with an optional daily goal. It can open when Quire starts, loads after the first frame, draws only the rows in view and works from the keyboard (`/` jumps to quick capture).

#### Follow-ups to 0.11
- **Notes of an unlocked locked folder are in your tasks, agenda, views and Bases, graph, backlinks and reminders** while the folder is unlocked, from a second index kept in memory only and dropped when it locks. Agents, MCP, `quire-cli serve`, background jobs and every cache still never see them, and nothing about them is written to disk in plain text. Their reminders fire only while the folder is unlocked; the desktop notification then names only the note, never the task's words, and they never go into the list of shown reminders on disk.
- **Pictures in locked folders:** embedded pictures in a locked folder show in notes and the preview while it is unlocked, decrypted into memory only. A picture you paste or a file you drop into a locked note is stored encrypted in that folder's own attachments subfolder (`Journal/attachments/`), never in the plain attachments folder. Export and publish still leave locked notes out.
- **Zoomed diagrams stay sharp:** a Mermaid diagram zoomed in a note, the preview or full screen is drawn again at the zoom's scale once the zoom rests (a quarter of a second), instead of stretching the picture; a few such pictures are kept.
- **The small toolbar next to selected text has its own buttons**, customised like the main toolbar in Settings → Toolbar ("Next to selected text"), stored in `[[toolbar.floating_items]]`.
- **Vim keys:** Ctrl+D and Ctrl+U move half a page; in Normal and Visual mode Ctrl+B, F, D and U now scroll as in Vim instead of running Bold, Find and Add cursor. A setting (Settings → Editing, `[editing] vim_ctrl_scroll`), on by default.
- **The agenda's board keeps the order you drag cards into**, and its column colours and swimlanes, on this device (`<config>/vaults/<key>/agenda-board.json`).

#### Long-standing requests, checked and closed
- **Open a Markdown file outside any vault:** `quire notes.md`, or drop a `.md` file on the welcome screen. The file opens on its own: its folder is not scanned, watched or made a vault, nothing is written next to it, and your last vault and its tabs stay as they were. A file inside a known vault opens in that vault. (File association on macOS and Windows is not set up yet; see [docs/design/0.12-verify.md](docs/design/0.12-verify.md).)
- **Date format for properties and views:** Settings → Editing → Date format (`[editing] date_format`, a strftime format such as `%d.%m.%Y`). Dates are shown and typed that way; files keep ISO dates.
- **Default template for new notes:** a template with `applies_to: ["*"]` is used by Ctrl+N and by a click on a link to a note that doesn't exist yet; a template that names a folder wins in that folder. Format: [docs/formats/templates.md](docs/formats/templates.md).
- **Nested tags fold in the Tags tab** (`project/alpha` under `project`), shown in tree order.
- **`quire-cli prefs-copy <from> <to> [section…]`** copies a vault's shared preferences (periodic notes, tags, git, lint, staleness, citations, links, tasks) to another vault. Device-wide settings were already the same in every vault.
- **No code from your notes ever runs:** stated in [SECURITY.md](SECURITY.md) and locked in by tests for the desktop preview, published sites, the web app, HTML export and copy as HTML.

### Changed
- Renaming or moving a note also rewrites links to it in front-matter properties; only the link text changes, every other byte of the note stays.
- **Faster start with a large vault:** nothing lists the vault before the first frame any more. The list of files that aren't notes (attachments, PDFs, boards) and the PDFs to read are gathered on a thread of their own after the first frame, and the task timer is read from then on too. What a 10,000-note vault adds before the first frame went from about 20 ms (CPU time, optimised build; CI measured 19.9 ms for 0.11) to nothing measurable ([details](docs/design/0.12-loose.md#1-startup-headroom)).

### Fixed
- **Tidy no longer corrupts tags with a comment or flow syntax, and keeps `"true"` text**; front matter is checked with a real YAML parser, and a note that isn't valid YAML, has a stray `---` block, isn't UTF-8, or is a template or in a locked folder is left alone (also in "Tidy this note" and "Tidy when I save"). Emphasis no longer edits wikilink targets, and sorting never puts an alias before its anchor.
- **Renaming a note keeps front-matter links valid YAML** (`Smith, John`, `@Work`, `Ada's` get the value quoted); links in YAML comments and code spans aren't read; "Add typed link" keeps trailing comments and multi-line lists, escapes `"` and `\`, and no longer copies a nested key's link up. A Markdown link in a property isn't a typed link (the format doc now says so).
- **MCP `hierarchy` no longer reports siblings (or next/previous) that share only a parent the client can't see.** A date format with a line break can't write front matter lines. Dates with a time zone offset can be edited in a custom date format.
- **PDF export: links between headings and to other notes in the same PDF now work** (internal links jump to the heading; the PDF reader resolves named destinations).
- **Link and tag suggestions ignore accents** (`[[cafe` offers `Café`, `#cafe` offers `#café`), as search and quick open already did; a name typed without its accent is no longer offered as a new note.
- **`quire-cli export`, File → Export as HTML and web previews drop active HTML**: exports are cleaned like published sites; a `style` that loads or runs something (`url(…)`, `expression(…)`) is removed everywhere, and so is a `data:` link hidden behind tabs or control characters (`da&#9;ta:`).
- The on-device AI engine ran at the lowest priority (nice 19) when a background job was the first to need it, on Linux; it is now always started from a thread at normal priority (nice 10, as intended).
- The task timer's settings were read from the vault's prefs file every frame; now only while a timer runs.
- Lint called a link to a note in a locked folder broken; it now counts it by name (it still never reads the note).
- **Import is safer and undoable after a failure:** the record is written before the first file and kept up to date, so an import that fails or is killed can be undone. Notes and attachments are created, never overwritten, even when a name appears during the import; an attachment whose name a different file has gets a new name and the notes' links follow. Importing into a locked folder works while it is unlocked (sealed, never plain) and is refused while it is locked; the record no longer holds a hash of a locked note's text. An attachment used by many notes is copied once, Roam block references are bounded, and importers no longer panic on hostile dates or `%` escapes. Undo saves open notes first, never closes a tab with unsaved text, and lists renamed or moved notes instead of marking the import undone.
- **`prefs-copy` no longer copies `[git]` `auto_commit`, `pull_on_start` or `push_after_commit`**, which could start sending notes to a remote nobody chose for the new vault.
- Reminders of locked notes are not announced again after the folder locks and unlocks. Typed links, the hierarchy and the trail include notes of unlocked folders. The agenda board's device file no longer holds a locked note's status. Home's word-count baseline leaves locked notes out, and Home no longer waits for the index while it updates.

## [0.11.0] - 2026-11-08

Quire 0.11: tasks as notes with time tracking and pomodoros, boards you arrange (manual order, swimlanes, colours) and Bases maps; locked folders encrypted at rest; a Connections view to audit, limit and revoke each agent; and a formatting toolbar, your own shortcuts and macros, and smoother typing.

### Added

#### Locked folders: encryption at rest, per folder
- **Lock a folder** ("Lock a folder (encrypt it)…" in the command palette, or `quire-cli lock <vault> <folder>`): its notes and files are stored encrypted on disk (XChaCha20-Poly1305, a key wrapped by your passphrase with Argon2id, the crypto Quire Sync uses), so a stolen disk, your cloud folder or a backup holds only ciphertext. The rest of the vault stays plain Markdown. A recovery key is shown before any file is converted (and again on request: "Show a locked folder's recovery key…" or `quire-cli unlock … --recovery-key-file F`, after the passphrase); it opens the folder if you forget the passphrase or lose the key file. File names stay visible, and notes in a locked folder aren't named after their first line. Each device remembers its locked folders apart from their key files, so a deleted or renamed key file never makes a folder plain again. The format is documented ([docs/formats/locked-folders.md](docs/formats/locked-folders.md)) so other tools can read it.
- **Unlock** with the passphrase (or the recovery key) in the app: notes are decrypted into memory only and saved encrypted; their history is encrypted too. Locked notes show 🔒 in the sidebar and open to a lock screen. Folders lock again on demand (Ctrl+Shift+B, or the 🔓 item in the status bar), after 15 minutes without use (Settings: `[security] lock_after_idle_minutes`; not while a note in the folder can't be saved), and when Quire quits; edits that can't be saved are kept as an encrypted copy beside the note, never in plain text. "Unlock automatically on this computer" keeps the key in the system keychain, only when there is one.
- **Search covers unlocked locked notes** (search and quick open) from an index kept in memory only; while locked, search says the folder wasn't searched.
- **Agents never see a locked folder**, locked or not: MCP (stdio and HTTP), external agents and the agent panel. The web app of `quire-cli serve` doesn't show locked folders either; it holds no keys.
- **Quire Sync and sync folders carry the encrypted files**, so the sync engine never sees their text; another device unlocks with the same passphrase. Two devices editing one locked note offline keep both versions (encrypted files can't be merged).
- Converting a folder (to locked or back, "Stop locking a folder…" / `quire-cli unlock --convert`) is safe to interrupt at any point and finishes when run again; plain originals are removed only after every encrypted copy was read back and compared. A plain file next to an encrypted one is shown as a conflict and never replaces it ("Keep both", `quire-cli lock … --keep-both`). Change the passphrase with "Change a locked folder's passphrase…" or `quire-cli lock … --change-passphrase`, and the folder's key itself (every file encrypted again, so old key files and the old recovery key stop working) with its "Also change the folder's key" or `quire-cli lock … --change-key`; `quire-cli status <vault>` lists locked folders and unfinished conversions; `quire-cli unlock … --check | --cat NOTE` tests a passphrase or prints one note.

#### Agent connections: audit, revoke, tags
- **Settings → Agents → Connections** ("Agent connections…"): every agent that used the vault in the last 90 days and every app with a token, with when it was last used, its reads and writes, its waiting suggestions and what it may do in each zone. **Activity** shows its log (reads when read logging is on, writes, suggestions, your decisions), filterable.
- **Revoke in one click:** an HTTP app's tokens are deleted; an MCP client on this computer, an external agent or the agent panel is marked revoked in the agent policy, so its next start (or message) is refused with a clear message and its reads are logged. Leaving out `--client` doesn't get around it.
- **Limit a connection to tags or folders** (`allow_tags`, `allow_folders` in the agent policy): it sees nothing else, in any tool, resource, search snippet, backlink, view count or answer, and the agent panel doesn't send it other notes as context.
- **AI off for this vault** (Settings → Agents): no agent and no AI provider or on-device AI may use it (Ingest into wiki and embeddings included). Turning it back on needs approval on your other devices.
- **Skills from elsewhere:** `quire-cli skill install <dir> --from <file>` shows the skill's source, size, SHA-256 and text and installs only with `--yes`, never over an existing skill; it is marked `trust: untrusted` in its own front matter, so inside a vault agents get it as untrusted data even after a copy or rename.

#### Task notes and time tracking
- **Task notes, TaskNotes-style:** a note with `type: task` (or the `task` tag, as the TaskNotes plugin does) is a task with `status`, `priority`, `due`, `scheduled`, `contexts`, `projects`, `time_estimate` and `time_entries`. Quire reads TaskNotes' property names too (`timeEntries`, `timeEstimate`, `completedDate`, …) and keeps a note's own names when it writes, so a vault can move between the two. Statuses, done statuses, offered tags, the task folder and which names to write are set per vault (Settings → Planning, `[tasks]` in `.quire/shared/vault.toml`). Details: [docs/formats/tasks.md](docs/formats/tasks.md#task-notes).
- **New task…** (Ctrl+Shift+K, "+ Task" in the agenda, or "New task…" in quick capture with what you typed): a task note with its properties in one dialog.
- **Time tracking:** start and stop a timer on a task line or task note with Ctrl+Shift+M, ▶ in the agenda and on board cards, or `quire-cli tasks start|stop`. One timer runs per device and survives restarts, and the app and `quire-cli` see each other's changes; the status bar shows it (click to stop). Stopping writes one time entry (`start` and `end`, ISO 8601 with your offset) to the note, or adds to that task line's `[time:: 1h25m]` (found by its line, not just its words). When the entry can't be written (the note was moved or locked, the line is gone, a session over 8 hours), the session is kept, not logged, until you log it to a task ("Log here", or `stop --to <task>`) or discard it. In a locked folder, the timer file and notifications name only the note, never the task's words. `quire-cli tasks log <task> 25m` logs time by hand.
- **Pomodoro:** "Start a pomodoro on this task" (or right-click ▶) counts down 25 minutes of work and a 5-minute break (a longer one every fourth), with a notification at each change; each work session is logged as it ends. Lengths are in Settings → Planning.
- **Time this week:** minutes per task and day, from the command palette, the agenda's Time tab or `quire-cli tasks report`. `from: tasks` views gain `time_spent`, `time_estimate`, `contexts` and `projects`.
- **Agenda:** task notes as cards (status, priority, due, contexts, projects, time against the estimate), a **Board** of task notes by status, and a **Calendar** of tasks by scheduled and due date.
- **Convert task note back to a task line**, the reverse of "Convert task to note". Converting now keeps the line's indentation, bullet, checkbox character (`[/]`), block id and the fields a note has no place for (`[client:: …]`, `🆔`, `⛔`), and carries tags, the estimate and time spent into the note; a task in a locked folder becomes a note in that folder. Going back trashes the note only when the line carries all of it; a note with contexts, projects, another status, session notes or any other property is kept, and Quire says which.

#### Boards and Bases
- **Boards keep the order you drag cards into:** drag a card up or down in its column. The order is kept in the view (`order.<column>:`) or the base file (`cardOrder`), never by rewriting your notes. Format: [docs/formats/boards.md](docs/formats/boards.md).
- **Swimlanes** (rows of columns by a second property, such as priority), **coloured columns** (right-click a column's heading), **cards you edit in place** (click status, priority or due), and **+ in each column** for a new note from the view's template, or a new task, with the column's value already set.
- **Obsidian Bases:** `kanban` views are boards with all of the above; `groupBy` groups tables and cards; **map** views show notes with `lat`/`lng` or `location: [lat, lng]` as pins on a world outline you can pan and zoom (offline: Natural Earth's public-domain outline is built in, no map tiles are fetched). `layout: map` works in quire-views too. Quire's own keys in a `.base` file are written line by line: everything else in the file stays as it was. A view written in flow style (`- {type: kanban, …}`) is left alone, and an edit is written only if the file still reads back afterwards.

#### Editor: formatting toolbar and your own commands
- **Formatting toolbar** (off by default: View: Formatting toolbar, or Settings → Toolbar): bold, italic, strikethrough, highlight, code, link, wikilink, a heading menu (H1–H6, paragraph), bulleted and numbered lists, task, quote, callout, table, math, footnote, horizontal rule, and insert image or attachment. Each button works on every cursor at once and is one undo step. Toggles read the Markdown: bolding bold text unbolds it, a caret in a word bolds the word, a caret in bold text unbolds it, and spaces at the edges of a selection stay outside the markers. Only markers are ever added or taken away: stars in code, math or `2*3` are left alone, and only real fence lines count as a code or math block.
- **Your own toolbar:** add any command or macro as a button, with an emoji or a few letters as its icon and a label; remove, reorder, separators, and back to the defaults (Settings → Toolbar).
- **A small toolbar next to selected text** (Settings → Toolbar): bold, italic, strikethrough, highlight, code, math and links above the selection.
- The toolbar from the keyboard: F10 moves to it, ← → move, Enter presses, Escape goes back to the note. Tooltips show each button's shortcut. It works in Vim's Insert mode too.
- **Settings → Commands:** search every command, change its shortcuts (or add a second one, remove them, reset one or all). Shortcuts are checked against other commands and against what Windows, macOS and Linux keep for themselves (⌘Q, Alt+F4, Ctrl+Alt+arrows…), with a warning where AltGr (Windows) or Option (macOS) would type a character instead. Ctrl is ⌘ on macOS, so the same settings work everywhere.
- **Pinned commands** show first in the command palette.
- **Macros:** a name and a list of commands run one after another, no scripting. A macro can have a shortcut, be pinned, be a toolbar button and is in the command palette. It stops at a command that opens a dialog.
- New commands: Heading 4–6, Toggle bulleted list, Toggle numbered list, Insert link [text](url), Insert callout, Insert table, Toggle inline math, Toggle math block, Toggle code block, Insert footnote, Insert horizontal rule, Insert image or attachment, Formatting toolbar, Move focus to the formatting toolbar (F10), Customize commands and shortcuts.
- **Mermaid diagrams pan and zoom** in notes and the preview: drag to move, Ctrl+wheel (or pinch) to zoom, double-click to reset, and a full-screen view. A diagram's source opens with its "Edit source" button.
- **Excalidraw drawings in PDF export:** `![[Plan.excalidraw]]` is the drawing, with its text, in the PDF.
- **Canvas cards:** links in cards are clickable (a `[[wikilink]]` opens the note, a web link the browser), and `$math$` in cards is typeset.

### Changed
- The agent policy's hard rules now also deny every path in a locked folder, every `.qenc` file and every key file, for agents and Quire's own background jobs.
- Pasting or dropping a picture into a locked note is refused (it would be stored unencrypted in the attachments folder), and so is exporting a locked note to HTML or PDF (exports go to `Exports/` unencrypted); Copy as HTML still works.
- Formatting shortcuts (Ctrl+B, Ctrl+I, Ctrl+1…, Ctrl+L, Ctrl+Shift+.) act on every cursor, read the Markdown around the selection, and wrap a selection over several lines line by line (after the list marker).
- Typing in a long note is smoother: the Insights panel's outline and links follow the note once typing pauses and draw only the rows in view, the editor keeps its headings between frames, and indexing, the meaning index and background jobs run at the lowest thread priority on Windows, macOS and Linux. On the 100 KB test note a keystroke takes 4.7 ms instead of 5.7 (median; 95th percentile 6.8 instead of 9.0 ms), and the CI budget for the 95th percentile is now 10 ms (was 12).

### Fixed
- Vim: Ctrl+V starts Visual-block mode even with nothing on the clipboard (Quire now handles the copy, cut and paste keys itself, so the key reaches Vim in Normal and Visual mode; in Insert mode it still pastes).
- Vim: Ctrl+Q, Vim's other key for Visual-block mode, no longer closes the window in Normal and Visual mode on Windows and Linux (elsewhere Ctrl+Q still quits; ⌘Q on macOS always does).
- Ctrl+Shift+C and Ctrl+Shift+V copy and paste (plain text) in the note and in every text field; Ctrl+Shift+X cuts in text fields outside the note, where it isn't Strikethrough.
- A note open in the editor keeps your selection and caret where they were when Quire writes to it in the background (a timer's `[time:: …]`, a board edit, a sync).

## [0.10.2] - 2026-11-06

The first published 0.10 release. (0.10.0 and 0.10.1 were tagged, but their release builds stopped before anything was published: 0.10.0's test job lacked the audio library voice notes use, and 0.10.1's Linux build was over its size budget (the app wasn't stripped of its symbol table) and its Windows build couldn't check the bundled AI engine against its hash.)

Quire 0.10 brings your vault to your browser and your agents, on a server you own (Beta), voice notes, much lighter on-device AI, and a round of things people asked for: Vim macros and block editing, reminders while Quire is closed, Excalidraw drawings, canvas cards that look like the preview, and Obsidian Bases formulas.

### Added

#### Your vault in a browser and for agents, on a server you own (Beta)
- **`quire-cli serve` (Beta, for self-hosters):** your agents and your browser, on a server you own. One port serves a web app and MCP over HTTP for AI agents, always behind a sign-in. In the web app, sign in with a passphrase (`quire-cli serve passwd`), browse the files, open notes by name (Ctrl+K), search, and read notes as they publish (callouts, links, embeds, tags, backlinks, images, PDFs, Excalidraw drawings, and voice notes that play in place), light and dark, on a phone too. Edit in Markdown with a preview, or start a new note: a note that changed meanwhile is never overwritten (you get the combined text to check, or save yours as a copy), and every save is kept in history as "You · via web". `--read-only`, `--hide FOLDER` (or `[web] hide` in `.quire/shared/vault.toml`), and `--sync` to sync in the same process. Apps signing in over MCP can be approved in the web app. Setup: [docs/serve.md](docs/serve.md).
- **The server holds a decrypted copy** of the folders it serves and syncs, unlike Quire Sync, whose server only ever sees encrypted data; run `serve` only on a machine you trust. It is not "Quire on the web": an end-to-end encrypted app that runs in the browser is planned separately. `serve --help`, the sign-in page and every page's footer say so.
- **Headless sync:** `quire-cli sync join | once | watch | status` syncs a vault folder without the app, through a sync folder or a Quire Sync server, with the same conflict handling and history as the app. The passphrase comes from a prompt, `QUIRE_SYNC_PASSPHRASE` or a file, and is never printed. `watch` stops on Ctrl+C or SIGTERM after finishing the pass under way.
- **Selective sync:** `--only FOLDER` and `--exclude FOLDER` (and `quire-cli sync scope`) keep only some folders on a device. Notes outside the selection are never written there and never sent as deleted; widening the selection fetches them.
- **Review your server agents' suggestions on your desktop:** with "Also sync vault settings and review history" on (Settings → Sync, or `quire-cli sync share <vault> on`), suggestions an agent makes through `quire-cli serve` appear in the desktop's Inbox, marked with the device they came from, and are reviewed as usual. What you decide reaches the server: the note syncs like any edit and the suggestion is resolved there, never applied twice; two devices deciding at once end with the same result. Activity shows every device's runs on their own lanes, and **Revert run** undoes a server run from the desktop, keeping your later edits. Off, each device keeps its own suggestions as before, and the Inbox says how to turn it on. Quire 0.9 devices pass the shared review history on untouched. Details: [docs/serve.md](docs/serve.md#agents-on-the-server-review-on-your-desktop).

#### Voice notes
- **Record a voice note** (full edition): "Record voice note…", or "Voice note…" in quick capture for today's note. The microphone records only between Record and Stop. The recording is saved in the vault as a WAV file and embedded in the note; published sites and the web app play it in place.
- **Voice notes play in the editor:** a recording embedded on its own line shows a small player (Play and Stop with the position in the full edition) and "Open in system player" (both editions, also for MP3, M4A and Ogg), instead of a link.
- **Transcripts, if you choose where they run** (Settings → AI → Voice notes): through your OpenAI-compatible provider (the recording is sent to it; the dialog says so before you record), or on this computer with Whisper while on-device AI is experimental. Transcription is off until you choose. Quire Lite doesn't record.

#### Ask and on-device AI
- **Ask writes its answer as you watch**, with Stop (what was written stays, marked as stopped). Closing the dialog stops it too.
- Settings → AI → Advanced → "Faster prompts, more memory" for on-device writing models.

#### Vim
- **Macros:** `qa` … `q` records, `@a` plays, `@@` plays again, `3@a` three times. A macro run is one undo step, stops at the first command that fails (so `100@a` stops at the end of a list), and is kept as text in its register (`"ap` shows it). The status bar says `recording @a`.
- **Marks:** `ma`, `` `a ``, `'a`, `''` (back to before the last jump), `` `. `` (the last change) and `:'a,'bd`. Marks move with their lines as you edit, belong to their note, and are kept with the session.
- **Visual-block mode** (Ctrl+V): `I`, `A`, `c`, `d`, `y`, `p`, `r`, `$`, `~`, `o`/`O` and `.` on a column of text, using the editor's multiple cursors. Tabs and wide characters (CJK) count by their width on screen, and short lines are skipped or padded as in Vim. A block insert is one undo step.

#### Planning
- **Reminders while Quire is closed:** Settings → Planning → "Remind me even when Quire is closed" installs a small job of your own (a systemd user timer or a crontab line on Linux, a LaunchAgent on macOS, a Task Scheduler task on Windows) that runs `quire-cli remind` every minute. No administrator rights; Settings shows exactly what is installed where, and turning it off removes it. Each reminder shows once, whether Quire or the job shows it, even when both check at the same moment. `quire-cli remind <vault>` (with `--dry-run`, `--install`, `--uninstall`, `--status`) does the same from a terminal.

#### Canvas, drawings and Bases
- **Excalidraw drawings show as pictures:** `![[Plan.excalidraw]]` in a note and its preview, drawing files on a canvas, the drawing note itself, `.excalidraw` files, HTML export, published sites and the web app (as SVG). Rectangles, ellipses, diamonds, lines, arrows, freehand, text, images and frames, drawn in a clean style. Read-only: edit in Excalidraw.
- **Canvas text cards look like the preview:** callouts, tables, task lists and code render as in the note preview, and stay fast on a 1,000-card canvas.
- **Obsidian Bases formulas and summaries:** `formulas` columns (arithmetic, comparisons, `if`, `date`, `now`, `today`, durations, `file.*` fields, and string, number, date and list functions) and `summaries` (Sum, Average, Min, Max, Median, Earliest, Latest, … and a base's own) under table and cards views. Filters can use the same expressions (`not`, dates, formulas). A function Quire doesn't know says so in its cell.

#### AI apps over MCP
- MCP `ask`: when the notes don't answer, it lists the three nearest passages with their anchors (`nearest`), as the app does, and cites nothing. This works over `quire-cli serve`'s `/mcp` too.

#### For developers
- `quire-cli bench --suite …` measures design §9's speed budgets (index, search, section search, MCP `view`/`search` on 10,000 notes, PDF text, MCP start-up with 200 PDFs) as JSON lines, and the desktop app built with `--features bench` times start-up, typing and a 1,000-card canvas. A new CI job, `perf`, runs them on an optimised build and fails above budget × 1.25 (docs/design/0.10-perf.md).
- `quire-cli eval` (development builds): an endpoint or offline mock embedder (`--embedder endpoint|mock`), a sweep of the "Not found" threshold, the sample vault as a suite, and a contradiction suite that scores the Inbox's precision and recall.

#### Small additions
- **Word-count goal:** a `goal: 1500` property shows your progress in the status bar (`640 / 1,500 words · 43%`), and "✓ goal" once you get there. Word and character counts there are now grouped by thousands (#16).
- **Copy as HTML:** the command palette, and right-click on a note or a tab, copy the note (or the selected part) as formatted HTML, with the Markdown as plain text, so it pastes into mail or a document with its formatting: callouts, tables, drawings, and in the full edition math and diagrams as pictures. Images and recordings point at their files on this computer (`file://`); links to other notes become their text. Where the clipboard takes plain text only, the HTML source is copied and Quire says so. `quire-cli html <vault> <note>` prints the same HTML (#17).

### Changed
- **On-device writing models use far less memory**: the Balanced model (Qwen3 1.7B) about 1.3 GB with a short prompt and 1.7 GB with a long one, down from about 5 GB. Quire now suggests Balanced on computers with 8 GB of memory or more (16 GB before), and reads the memory on Windows too (it suggested Small there before). Long prompts are read more slowly unless "Faster prompts, more memory" is on.
- Small on-device models' tool calls are read more forgivingly (single quotes, code fences, two calls at once, words around the call). A call that still can't be read is never run: the model is asked once to write it again.
- The full edition now includes the on-device AI engine; Quire Lite downloads it when you turn on-device AI on. Settings → AI says "Included with Quire" for the engine that came with the app, and Quire checks it against a hash built into the app before every start, as it does a downloaded engine.
- A read-only MCP server now caps HTTP tokens to reading too (only `quire-cli serve --read-only` starts one).
- The full edition's Linux packages now depend on the ALSA library (`libasound2`), for recording.
- **A suggestion you rejected stays rejected** when the agent writes it again reworded a little (letter case, spacing, a full stop), rewrites the lines around it, or suggests it again in a later run within 30 days: Quire compares what the lines say, not only the exact change, and still asks about anything new.
- With an Obsidian Base, canvas or PDF open, Insights describes that file (for a base: its views, columns and formulas) instead of showing the note that was open before, and the status bar no longer counts that note's words.
- **Smaller downloads:** the app is 16–19 % smaller (Quire 68 MiB, Quire Lite 41 MiB on Linux, from 84 and 48), with every feature kept: code off the hot path is optimised for size, the full edition no longer carries its fonts twice, and the Linux build folds identical code.
- **A large vault starts and types faster:** with 10,000 notes the window appears before the vault is read, typing in a long note no longer redraws the whole file list every frame, and indexing is almost twice as fast (2,000 notes in about 360 ms, from 690 ms on the same machine).
- `quire-cli mcp` and `serve` answer at once in a vault with many PDFs: PDF text is read in the background, and while it is, `search` over attachments says so (`indexing: {done, total}` and a sentence in the text).

### Fixed
- **Selective sync never duplicates or renames notes when the selection grows** (`quire-cli sync scope --all` after `--only`): notes are fetched as they are now instead of replaying their history, which used to copy every note edited more than once to `Name (2).md` and could rename the note living at an old path, on every device. A copy left on the server that was changed meanwhile is merged; an unchanged old copy of a note that moved or was deleted elsewhere goes to `.trash/`. The same holds when a device starts from a fresh sync state over a folder that already has the notes.
- **`sync scope` can't change the folders under a running sync:** it refuses while the app, `sync watch` or `serve --sync` syncs the vault on that machine (stop it, change the folders, start it again), and says exactly what happens to the files left out and the notes now included. Before, a running `watch`/`serve` kept the old selection, so deleting a folder just left out deleted it on every device.
- **A note deleted on a selective device is sent as deleted**, never as a "move" onto another note outside the selection that happens to have the same text (an empty note, or one made from a template).
- **One sync per vault on a machine:** the app, `sync watch`, `serve --sync` and `sync once` no longer sync the same vault folder at once (which duplicated new notes on every device). A second one says "Quire is already syncing this vault (the app or `quire-cli serve`)", waits and takes over when the first stops; `sync once` and `sync scope` refuse.
- **A note sync changes while Quire is starting is merged into your open tab**, never overwritten by its autosave: sync now starts after the folder watcher, open notes are checked against the disk when it starts, and a save never replaces a change on disk it hasn't taken in.
- **Web app saves can't overwrite sync or agent writes:** the "still the version you started from" check is made as one step with the write, against every write in the `serve` process (sync, MCP, agents). "Save mine as a copy" saved twice at once now makes two copies instead of one replacing the other.
- **An agent's new note accepted on two devices before they synced** ends up as one note, not `Name (2).md`, when both have the same text.
- With a selection set, Quire 0.9 shows sync as off for that vault instead of syncing it from an empty state (which duplicated notes).
- A note moved out of a device's selection elsewhere while it was edited on that device now takes the edit along when the changes merge, instead of leaving it behind as a separate note.
- The recovery key written by `quire-cli sync join --create --recovery-key-file` goes straight into a new file readable only by you, and app state files are saved through temporary files with unpredictable names that are always created new.

### Security
Fixes from an independent security review of 0.10 before release.
- **Excalidraw drawings could show files they shouldn't** (in the web app, published sites, HTML export and the desktop): a drawing's embedded pictures were read without the path check every other file passes, so a drawing could pull in a file from a folder hidden from the web, from `.quire/`, or from outside the vault through a symlink (on Windows, any file by its drive path). Each picture now passes the same check (inside the vault through symlinks, no hidden folders, the web's hidden folders respected, no absolute or drive paths), and only picture files are embedded. The vault also refuses drive and rooted paths outright.
- **Shared review history from other devices is treated as untrusted input:** every path in another device's suggestions and run history passes the same check as an agent's paths, so **Revert run** and **Accept** never write to hidden folders, `.quire/`, through symlinks out of the vault, or outside policy; undoing another device's change to one of your own notes is a suggestion to review. A device can only speak for itself (its files must be in its own folder and name its own id), its name is shown as its own claim with its id, a mirrored suggestion's trust can only be lowered, and this device's decisions are re-published from its own records. A suggestion that was waiting here and was settled on another device says "decided on …" under Resolved in the Inbox, with **Reopen**. Signed device files are planned (docs/design/0.10-agentsync.md).
- **A tool call cut off at the model's length limit is never run,** nor one whose JSON the model never closed (brackets are no longer added for it, with or without `<tool_call>` tags); the model is asked once to write it again.
- **Note HTML can no longer move the browser to another site:** in the web app and on published sites, raw HTML in a note is cleaned to an allow-list (no `<meta>` refresh, `<base>`, forms, frames, objects or scripts, no `on…` attributes, only `http(s)`, `mailto`, `tel` or relative links). Callouts, tables, `details`, formatting, images and audio stay.
- **Web sign-in:** tries sent in parallel are each counted before the passphrase is checked, so a burst gets no extra guesses, and at most two checks run at once (each takes about 19 MB). After signing in, a `next` address with a tab, line break or space is no longer followed.
- **"Remind me even when Quire is closed"** refuses a vault or program path with a line break or other control character, which could have added lines to the systemd unit or crontab; the unit's description no longer contains the path.
- **Large or hostile files can't exhaust memory or hang the app:** a compressed Excalidraw drawing unpacks with bounded memory and shows "Drawing too large to show" past 16 M characters, 50,000 elements or 1,000,000 points; Obsidian Bases expressions have limits on nesting, length, size of text and lists (`repeat`, `join`, `replace`…) and evaluation steps, and show an error in the cell instead; a WAV file must have a sample rate of 4–384 kHz and at most 32 channels, and playback never asks for more than about 46 minutes of samples.
- **A rejected suggestion no longer hides a corrected one:** "rejected stays rejected" now ignores only letter case, spacing and closing punctuation. A line brought back with a different digit, date, number or word (March 14 rejected, March 21 suggested) is always asked about.
- `quire-cli sync join` and `sync scope` warn when an `--only` or `--exclude` folder matches no folder on disk, naming the folder you probably meant when only the letter case or Unicode form differs (names are compared exactly).

## [0.9.1] - 2026-11-04

The first published 0.9 release. (0.9.0 was tagged, but its release build stopped on two packaging problems before anything was published.)


Quire 0.9 lets AI agents work in your vault without taking it over: their changes arrive as suggestions you review in the note, everything they do can be undone, and answers cite the exact sentences they come from. AI stays optional; running it entirely on your computer is coming in an update, and you can try it now behind an experimental switch. Also new: planning with dates and reminders, Vim keys, canvases, a PDF viewer, citations, and Quire Lite.

### Added

#### Choose your AI (or none)
- **A short choice the first time you start Quire 0.9:** no AI, or a provider (Claude, OpenAI, or a server you run such as Ollama or LM Studio). Nothing is preselected and nothing downloads unless you ask. If you are upgrading, a card above the note offers the same choice once; "Set up AI…" opens it again.
- **On-device AI is coming in an update.** Quire 0.9.0 ships it turned off, because its models aren't pinned and measured yet: nothing on-device is shown, started or downloaded, and features that would use it work as before (built-in meaning search, a notice in chat, no OCR). To keep everything on your computer today, use a provider such as Ollama or LM Studio. To try it, turn on Settings → AI → On this computer → Advanced → "Experimental: show on-device AI" (`[ai_local] experimental = true`, or `QUIRE_EXPERIMENTAL_ON_DEVICE=1`); a model whose download isn't pinned yet says "Not available in this build". A choice of "On this computer" made before is kept.
- **On-device AI (experimental)**, in both editions: meaning search, writing help, the agent, and reading text in images and scanned PDFs, from models that download once and then work offline. They run in a separate engine process, so a model that fails or runs out of memory never takes the editor down. Downloads show their size, progress and speed, can be paused, resumed and cancelled, and are checked against a hash built into Quire. "Install from file…" installs a model downloaded on another computer. `quire-cli ai models|download|install|delete|engine` does the same from the command line, and says on-device AI is coming in an update while the switch is off.
- **Settings → AI**, rebuilt: your provider, what runs on this computer when the experimental switch is on (download, pause, delete, the engine's memory use), which model the agent panel uses, and the answer and check features below (Ask style, citation checks, staleness, contradictions).

#### The agent panel
- **Agent panel** (Ctrl+Shift+J): ask about your notes or ask for changes. It searches and reads your notes with the same tools AI apps get over MCP and shows each step. Its edits to your notes arrive as suggestions; it writes directly only in `wiki/`. Stop works at any moment, and Ask mode only reads.
- Context chips send the open note and your selection along; type `@` for a note or `#` for a tag to add those too (Tab or a click picks one).
- With a small on-device model the panel only chats about the open note; the balanced model reads notes and makes one change per message; larger and cloud models use every tool. The panel says which. Conversations are kept on this computer for 30 days and can be saved as a note.
- **Connect an external agent** (Claude Code, Codex or another ACP agent) to the panel, after a warning that it runs with your permissions. Its edits through Quire become suggestions; when it asks to run commands, fetch from the web or delete, the answer starts on Deny. An amber "External" label stays on the panel while it is connected.
- **Ingest into wiki** (command, `quire-cli ingest <vault> <note or URL>`, or the MCP prompt `quire_ingest`): a source becomes a summary page in `wiki/sources/` and concept pages in `wiki/concepts/`, every point quoting the source. Points whose quote isn't in the source are left out, links from your own notes are suggestions, and on a cloud model you see the cost first. "Revert run" undoes the whole ingest.
- `wiki/index.md` lists the wiki's pages by type and is kept up to date between its markers (your own text around them stays). `wiki/log.md` gets a line for each ingest, each agent run in the wiki and each batch of accepted suggestions there.

#### Reviewing what agents do
- **Review suggestions in the note itself:** the lines an agent would replace are struck through with its version underneath, and Accept, Reject and Edit for each change. A bar above the note says who suggests what and why, with Prev, Next, Accept all, Reject all and Done. Alt+] and Alt+[ move between changes, Ctrl+Alt+Y accepts, Ctrl+Alt+Backspace rejects, Ctrl+Alt+Shift+Y accepts every change in the note. Accepting is one edit you can undo.
- If you edit a note after an agent read it, its suggestions are adjusted to your version; changes that touch lines you edited are marked "You changed this after the agent read it" and are never applied by Accept all. An agent that keeps working on a note updates its suggestion instead of adding another.
- **Inbox** (Alt+I): suggestions grouped by agent run, with Review, Accept all and Reject all; new notes, renames and deletions to accept one by one; vault checks, citations that need a look, notes getting stale and contradictions. Filter by kind, agent and folder, snooze for a day or a week. The status bar shows how many items wait.
- **Agent activity:** what each agent did, run by run, with "Revert run" and "Undo an agent's changes…" (for example everything Claude Code did today). It includes the agent's suggestions you accepted, and new notes you accepted from it. You see what would change before anything is written; your own edits are kept, both later ones and ones made between two of the agent's changes, and where they overlap the agent's the undo becomes a suggestion to review. Your own tag and property renames show there too, with "Undo batch", also after a restart (`quire-cli agent revert <vault> --batch b_…`).
- **Version history says who made each version** (you, an agent, or you accepting an agent's suggestion) and links to that agent's activity. With a git repository it also has a Git tab with the note's commits.
- **Settings → Agents:** what agents may do per folder (agent-owned, sources, your notes, private), limits per connected agent, the setup line for Claude Code or Claude Desktop, and an optional log of which notes agents read. Write contracts can require properties and links in agents' notes, and `[limits]` caps writes per minute and waiting suggestions.
- Agent permissions and `AGENTS.md`/`CLAUDE.md` that change on another device or outside Quire take effect only after you approve them in the Inbox; narrowing what agents may do takes effect at once. "Create agent instructions (AGENTS.md)" writes a starter, which is pinned at the top of the Files tab.
- **Vault checks** ("Check the vault", `quire-cli lint <vault> [--json]`): broken links (with a fix), links to missing blocks, notes with no links, missing notes many link to, missing required properties, duplicate titles, empty notes and an out-of-date `index.md`. They run in the background after you stop typing and come with quick fixes; choose which run in `.quire/shared/vault.toml` under `[lint]`.
- Suggestions based on captured or imported content are marked "Based on untrusted content".

#### AI apps over MCP
- **A much richer set of tools:** search by section with line ranges (filtered by folder, tags, a view condition, dates, and PDF and image text when asked), view a note or a section or line range, outlines, linked notes, recent changes, history and diffs, tags, properties, views, tasks, canvases (`canvas_read`, `canvas_apply`) and the app's own suggestions. Long results are cut to a size the app asks for and continue with a cursor.
- **Precise edits:** replace a passage, insert lines, edit a section, append, set properties, rename, delete. An edit names the version it was based on; if the note changed meanwhile, Quire re-applies it when it still fits or says what changed, so an agent never overwrites your edits unknowingly. A canvas change is reviewed card by card ("Add text node …").
- AI apps can save an answer as a note with citations Quire checks later (`save_answer`), suggest undoing an agent's changes (`revert`), group a task's changes (`begin_run`), follow changes, and read notes as `quire://note/…` resources. They get Quire's built-in prompts (`quire_ingest`, `quire_lint`, `quire_answer`, `quire_maintain_index`), and `ask` in model mode uses the AI you set up in Quire.
- **MCP over HTTP** (Settings → Connections, or `quire-cli mcp serve-http`): off by default, on 127.0.0.1 only, and every request needs a token. Apps sign in with OAuth and you approve each one in Quire (or with `quire-cli mcp approve`); you can also create personal tokens and revoke any app. Settings → Connections has ready-to-copy setup for Claude Code, Claude Desktop and other MCP apps.
- A vault can let a specific app ask you to apply its change right there instead of sending it to Quire's review.
- **Capture web page** (command, `quire-cli capture <vault> --url URL`, MCP `capture_url`): a page's article and images become a note in `raw/`, marked as untrusted source material. Private and local network addresses are refused.
- `AGENTS.md` (or `CLAUDE.md`) is given to AI apps once you've approved it on this computer. `quire-cli agents-md init` writes a starter, `quire-cli skill print|install` gives agents a guide to working in Quire, and `quire-cli agent proposals|show|log|accept|reject|revert` handles suggestions from the command line. See [docs/agents.md](docs/agents.md).

#### Answers you can check
- **Ask says "Not found in your notes"** when your notes don't answer the question, and shows the passages that came closest instead of quoting something unrelated. It cites the exact sentences an answer comes from, not the whole section.
- **Save as note** in Ask: the answer becomes a note in `Answers/` (or `wiki/answers/`) with the question, the date, who answered, an "AI wrote this" callout, and each source as a footnote quoting the cited text.
- **Quire notices when a quoted passage changes or disappears,** offline: the footnote is marked (in the editor and the preview), a note under it shows what the text says now with **Update citation** and **Open**, a bar above the note says how many need a look, and the Inbox lists them.
- **Check citations in this note:** with an AI model set up (and "Check citations with the AI model" on in Settings → AI), the model says whether each passage supports the sentence citing it. Without a model, citations that share few words with their sentence are pointed out.
- **Say what you expect first** (Settings → AI, or per question in Ask): write what you think the answer is, see the passages grouped by topic, then the answer, which says where your notes agree or disagree with you.
- **Getting stale:** a weekly digest in the Inbox of notes that may be out of date (old and not reviewed, linked notes that changed a lot since, "currently" or "as of", a past `valid_until:` or a `superseded_by:`), with the reasons, **Mark reviewed** and Snooze. Set how long a folder stays fresh under `[staleness]` in `.quire/shared/vault.toml`, or per note with `review_every: 90d`.
- **Find contradictions** (Preview, off by default): an AI model compares claims in your notes about the same thing. Pairs that disagree appear in the Inbox with **Open both** (side by side, the passages highlighted), **Mark superseded**, **Ask agent to reconcile** (the agent panel starts with both passages) and Dismiss. It shows how many requests the first run takes, keeps cloud requests within a daily limit (200 by default), runs only while you're not typing, and sends a passage or a pair once.
- **Thinking timeline** (Preview): for a tag, a note or a few words, when your notes started saying something, took it back, or another note contradicted it, read from saved versions.

#### Search
- **Accents are optional:** "cafe" finds "Café", "Lodz" finds "Łódź", "strasse" finds "Straße", in search, quick open and links. The find bar has a "Match accents" toggle; Find and replace in all notes keeps matching accents unless you untick it.
- **PDF pages are searchable** in the app, `quire-cli search` and MCP `search`, listed as "paper.pdf p. 3" and opening at that page. Scanned pages and images are read with on-device OCR when it's on (experimental).

#### Planning
- **Tasks with dates, repeats and reminders:** Quire reads the Obsidian Tasks emoji (📅 ⏳ 🛫 ✅ ➕ 🔁 and priorities) and Dataview fields (`[due:: …]`), and adds ⏰ reminders. Repeats like `every month on the 1st`, `every weekday` or `every 3 days when done` (or `RRULE:`) add the next task when you tick one off.
- **Reminders** show in Quire and, where the system allows, as desktop notifications with Open and Snooze 10 min; ones that came due while Quire was closed are listed when it starts.
- **Agenda** (Alt+A): reminders, overdue, today with a timeline of time blocks (`- [ ] 14:00 - 15:30 Write report`), and the next seven days. **Calendar** sidebar tab: a month with week numbers, daily notes and due tasks.
- **Weekly, monthly and yearly notes,** with next and previous commands; their folders, names and templates are in Settings → Planning and travel with the vault.
- **Views:** `from: tasks`, `layout: agenda` and `layout: timeline`, nested fields like `author.name`, dates like `today+7d`, and `new: Templates/Project.md` for a **+ New** button.
- **Templates** ask questions in a dialog (`{{prompt:Client}}`, `{{select:Status|planned,done}}`), do date maths (`{{date+7d}}`, `{{date:next monday}}`), and can set a folder, a name pattern and the folders they apply to.
- **Rename, merge or delete tags and properties** across the vault with a preview and Undo (also `quire-cli tags` and `quire-cli props`). Nested front matter is read and edited field by field.
- **Obsidian compatibility:** Dataview (`TABLE`, `LIST`, `TASK`) and Tasks queries show their results and can be converted to `quire-view`; Kanban plugin boards show as boards; `.base` files open as views. DataviewJS is never run.
- **Flashcards:** `Question :: Answer`, question/`?`/answer blocks or `==cloze==` in notes tagged `#flashcards`. Review with Ctrl+Alt+R (Space shows the answer, 1–4 rate it), scheduled with FSRS; every device works out the same schedule.
- "Convert task to note", `quire-cli tasks --agenda`.

#### Writing and the editor
- **Vim keys** (Settings → Editing): Normal, Insert, Replace, Visual and Visual Line modes, counts, operators with text objects, registers (including `"+`), `.`, `/` and `?`, `:w`, `:q`, `:s`. The mode shows in the status bar.
- **Edit embedded notes in place:** a `![[Note]]` line shows that text under it, editable and saved to the embedded note.
- **Tabs that don't pile up:** a note opened with one click replaces the preview tab until you edit or double-click it. Notes reopen where you left them.
- Sort each folder by name, date or by hand; links to folders (`[[Projects/]]`); a homepage note; folders and notes side by side; note titles instead of file names; folder and note icons; tag colours.
- **Themes:** Paper, Nord, Solarized and High contrast, an accent colour, and your own theme files (every colour, light and dark, plus fonts).
- **Git:** when the vault is a git repository, a Git tab shows what changed, commits (by itself after a quiet spell, if you like), pulls and pushes.
- New note from template puts the note in the template's folder; Settings → Editing chooses where Ctrl+N makes a note.

#### Canvas, PDFs and citations
- **Canvas** (Ctrl+Alt+C): open and edit `.canvas` files (JSON Canvas, as Obsidian writes them). Cards show Markdown, notes, images, PDFs and diagrams; connect, group, colour, align, copy between canvases, undo. Everything Quire doesn't know about is kept, and a canvas changed elsewhere is merged with your unsaved changes.
- **Drawing:** pen (with stylus pressure), marker, highlighter, eraser and shapes on a canvas, or in a note with "Insert sketch". Drawings are SVG files any app can show.
- **PDF viewer** (full edition): continuous pages, zoom, find, copy, links. Highlight text or an area in five colours and add a comment. Highlights live in a Markdown note next to the PDF, can be linked to (`[[paper.pdf#page=3&annot=h7f3a]]`), and are found again when the PDF is replaced. Quire Lite shows the PDF's text and its highlights.
- **Citations:** `[@key]`, `[@key, p. 4]`, `[see @a; @b]` with works from a BibTeX library. Typing `[@` suggests works, hovering a citation shows the full reference, and Ctrl+click opens its PDF. Preview, HTML export and publishing show "(Lovelace 1843)" and a reference list; PDF export uses APA, Chicago or IEEE. "Import bibliography…" merges a `.bib` file or Zotero's CSL-JSON, and can keep watching it.

#### Editions and sync
- **Quire Lite:** every release comes in two editions, **Quire** and **Quire Lite**, the same app without drawn math, diagrams, PDF export and the PDF viewer, at about a third of the size. Both read and write the same notes, settings and sync setup, and installing one replaces the other. Settings → About says which edition you run. To build Lite: `cargo build -p quire-desktop --no-default-features`.
- **Settings → Sync:** "Also sync vault settings and review history". In a sync folder they travel separately from your notes, so devices still on 0.8 keep syncing notes and never touch them; through a server, every device needs 0.9. Synced changes show which AI agent made them. Quire Sync Server reports its protocol versions (`GET /v1/info`).

### Changed
- **`quire-cli mcp --allow-write` now writes according to the vault's agent policy:** directly in agent folders (`wiki/`), new files only in `raw/`, and everywhere else as suggestions you review in Quire's Inbox. In 0.8 it wrote directly to any note. A notice says so when the server starts. MCP edits are recorded in version history with who made them, and can no longer write inside `.quire/`; MCP apps can't see notes in folders the policy marks private.
- MCP apps on protocol 2025-06-18 or later also see titles, annotations and structured results for every tool; older clients are answered exactly as before.
- Claude and OpenAI-compatible providers stream their replies and can use tools.
- Settings lists its tabs down the left side; "AI & search" is now **AI**, and holds the answer and check features.
- Ask your notes opens as its own dialog and remembers your question. Undo an agent's changes, Capture web page, Rename tags and properties, and flashcard review open as dialogs.
- Find in note ignores accents unless "Match accents" is on.
- Ctrl+N makes the new note in the open note's folder by default.
- A task's text no longer includes its date and priority fields; the MCP `tasks` tool returns `start`, `completed`, `reminder` and `time`.
- Icons and task emoji display on every system.

### Fixed
- Alt shortcuts no longer type their character into the note.
- Following `[[Folder/]]` opens the folder's note or shows the folder instead of creating a note; links to `paper.pdf` and `Board.canvas` open the file instead of creating a note.
- With Vim keys, Escape in Insert mode returns to Normal mode instead of closing the find bar.
- Headings, links and quotes use the theme's colours in the classic editor and in embedded notes.
- A template saved while Quire runs no longer shows up among your notes.
- A version saved by an AI agent is no longer replaced in history when you save the same note again within two minutes.
- Settings written by a newer version or the other edition are kept: a setting this version doesn't know about is saved back unchanged. Quire 0.8 drops settings it doesn't know when it saves, so Quire 0.9 also keeps its own in `settings.v09.toml` next to `settings.toml` and takes them back after 0.8 saved the file.
- A settings or stored-secrets file that can't be read is kept as `<name>.unreadable` before Quire starts with defaults, instead of being overwritten.
- Settings, the session, the sync setup and the stored-secrets file are written to a temporary file and moved into place, so a crash or full disk can't leave a half-written file. The secrets file is readable by you only.
- A property value with a line break is written on one line with `\n`, so it can no longer end the front matter or add other properties (from the Properties panel, templates or an agent's `set_properties`).
- Typing in an embedded note and then quitting or switching vaults right away no longer loses the typing.
- Importing citations no longer rewrites a changed entry from scratch: only the fields that changed are rewritten, so `series`, full dates, `school`, braces and organisation authors stay as written.
- Version history keeps, for 90 days, the versions needed to undo an agent's change, however many versions were saved since.
- Undoing an agent's changes rechecks each note when applied, so typing saved while the dry run was on screen is kept.
- An agent updating its suggestion while you review it no longer brings back changes you rejected.
- Renaming tags and properties reads notes that aren't open from disk, so a change synced in a moment earlier isn't overwritten.
- A canvas changed elsewhere in a form Quire can't read is kept as "(changed elsewhere)" before your version is saved.
- PDF highlights that can't be found after the PDF changed stay listed as detached until they are found again.
- `wiki/index.md` no longer grows a new list on every update when its text mentions the end marker.
- A saved answer records the model that wrote it in `answered_by` (or `quire:extractive`), not `human`.
- The thinking timeline shows each claim once per change, not once per saved version.

### Security
- Agents can no longer read notes in hidden folders, including private notes in `.trash/` after you delete them, by asking for them as `quire://note/…` resources. Every path an agent names (tools, resources, subscriptions, external agents' file requests, captured images) goes through one check.
- A symbolic link in the vault no longer lets agents read or write a private folder or a file outside the vault: the policy applies to where a link really leads.
- Signing in an app over HTTP can only be finished in the browser that started it, and Quire and the browser show the same verification code to compare before you approve. Quire shows the app's exact host, with its port. Each approval gives one sign-in.
- Captured web pages keep only real PNG, JPEG, GIF and WebP images, and agents save them only where the policy lets them add files directly, within their limits. SVG images stay links.
- A synced agent policy you haven't approved can no longer make captured sources look trusted; changing a folder's role from sources or private now waits for your approval. Approving the policy or `AGENTS.md` approves exactly the version you were shown.
- Text from a source can no longer end its data block in the ingest prompts or in an external agent's file reads, whatever its letter case.
- External agents connected to the panel now get Quire's note tools, recorded as the same agent.
- An app started without `--client` keeps the name it gave first and can't pass for one you named. Agents can't follow changes to notes they can't read.
- Web capture and app sign-in work through a proxy set in the environment, including one on this computer.
- The on-device AI engine is checked against its hash every time it starts.

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

[Unreleased]: https://github.com/BhanuNexus/quire/compare/v0.12.0...HEAD
[0.12.0]: https://github.com/BhanuNexus/quire/compare/v0.11.0...v0.12.0
[0.11.0]: https://github.com/BhanuNexus/quire/compare/v0.10.2...v0.11.0
[0.10.2]: https://github.com/BhanuNexus/quire/compare/v0.9.1...v0.10.2
[0.9.1]: https://github.com/BhanuNexus/quire/compare/v0.8.0...v0.9.1
[0.8.0]: https://github.com/BhanuNexus/quire/compare/v0.7.0...v0.8.0
[0.7.0]: https://github.com/BhanuNexus/quire/compare/v0.6.0...v0.7.0
[0.6.0]: https://github.com/BhanuNexus/quire/compare/v0.5.0...v0.6.0
[0.5.0]: https://github.com/BhanuNexus/quire/compare/v0.4.1...v0.5.0
[0.4.1]: https://github.com/BhanuNexus/quire/compare/v0.4.0...v0.4.1
[0.4.0]: https://github.com/BhanuNexus/quire/releases/tag/v0.4.0
