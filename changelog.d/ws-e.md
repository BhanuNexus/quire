### Added

- **Tasks with dates, repeats and reminders.** Quire reads the Obsidian Tasks emoji (📅 due, ⏳ scheduled, 🛫 start, ✅ done, ➕ created, 🔁 repeats, 🔺⏫🔼🔽⏬ priority) and Dataview fields (`[due:: …]`), and adds ⏰ reminders. Fields Quire doesn't use are kept as written.
- **Repeating tasks**: `every month on the 1st`, `every 2 weeks on Mon, Thu`, `every weekday`, `every 3 days when done`, or `RRULE:` text. Ticking one off adds the next with every date moved along; dates that don't exist (30 February) are skipped.
- **Reminders** show in Quire and, where the system allows, as desktop notifications with Open and Snooze 10 min. Reminders that came due while Quire was closed (in the last week) are listed when it starts. Turn desktop notifications off in Settings → Planning.
- **Agenda** (sidebar and right panel, Alt+A): reminders, overdue, today with a timeline of time blocks (`- [ ] 14:00 - 15:30 Write report`), and the next seven days. Tick tasks off from it.
- **Calendar** sidebar tab: a month with ISO week numbers; a dot for days with a daily note, a ring when tasks are due, darker days for more writing. Click a day or a week number to open its note.
- **Weekly, monthly and yearly notes**, with next and previous note commands. Their folders, names and templates (and the daily note's) are in Settings → Planning and in the vault's `.quire/shared/vault.toml`, so they travel with the vault.
- **Views**: `from: tasks` lists tasks; `layout: agenda` and `layout: timeline`; nested fields like `author.name`; dates like `today+7d` and `tomorrow`; `new: Templates/Project.md` adds a **+ New** button.
- **Templates** ask questions (`{{prompt:Client}}`, `{{select:Status|planned,done}}`), do date maths (`{{date+7d}}`, `{{date:next monday}}`), and can set a default folder, a name pattern and the folders they apply to (`quire-template:` in the template's front matter). Notes that still have questions to answer offer a small form.
- **Rename, merge or delete tags and properties** across the vault (Rename tag…, Rename property…), with a preview of every change, notes you can untick, and Undo. Also `quire-cli tags` and `quire-cli props` (preview unless `--write`).
- **Nested front matter** (`author:` then indented `name: Ada`) is read, shown and edited; editing one field changes only its own lines.
- **Obsidian compatibility**: Dataview (`TABLE`, `LIST`, `TASK`) and Tasks query blocks show their results in place and can be converted to `quire-view`; Kanban plugin boards show as a board you can drag cards on; `.base` files open as views and can be converted to a note. What can't be shown is said in place and listed in Settings → Planning. DataviewJS is never run.
- **Flashcards**: `Question :: Answer`, a question / `?` / answer block, or `==cloze==` highlights in notes tagged `#flashcards` (or `srs: true`). Review with Ctrl+Alt+R (Space shows the answer, 1–4 rate it); scheduling is FSRS. Reviews are kept per device in `.quire/shared/srs/` and every device works out the same schedule. "Add ids to this note's flashcards" keeps a card's history through later edits.
- "Convert task to note" makes `Tasks/<task>.md` with the task's dates and links to it.
- `quire-cli tasks --agenda`; `quire-cli tasks` shows tasks' dates.

### Changed

- A task's text no longer includes its date and priority fields (in the Tasks list, search snippets and `quire-cli tasks`).
