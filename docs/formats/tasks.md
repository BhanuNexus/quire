# Tasks in notes

Tasks are Markdown checklist items. Quire reads the fields Obsidian Tasks
and Dataview write after a task's text, and writes them back in place.
Owner: WS-E (0.11: the tasks workstream). Code: `quire_markdown::tasks`
(syntax), `quire_plan` (dates, recurrence, agenda, reminders,
`task_note`, `time`, `timer`).

```markdown
- [ ] Pay rent ⏫ 🔁 every month on the 1st ⏳ 2026-11-28 📅 2026-12-01 ⏰ 2026-11-30 09:00
- [x] Ship it [due:: 2026-12-01] [priority:: high] [completion:: 2026-12-01]
- [ ] 14:00 - 15:30 Write report
```

## Fields

| Field | Emoji | Dataview | Value |
|---|---|---|---|
| due | 📅 | `[due:: …]` | `YYYY-MM-DD` |
| scheduled | ⏳ (⌛) | `[scheduled:: …]` | `YYYY-MM-DD` |
| start | 🛫 | `[start:: …]` | `YYYY-MM-DD` |
| done | ✅ | `[completion:: …]` | `YYYY-MM-DD` |
| created | ➕ | `[created:: …]` | `YYYY-MM-DD` |
| repeats | 🔁 | `[repeat:: …]` | a rule (below) |
| priority | 🔺 ⏫ 🔼 🔽 ⏬ | `[priority:: highest\|high\|medium\|low\|lowest]` | no field is normal |
| reminder | ⏰ (Quire) | `[remind:: …]` | `YYYY-MM-DD HH:MM`, or a date (09:00) |

* Fields are read from the end of the line, as Obsidian Tasks does:
  everything before the first field is the description. `(key:: value)`
  works like `[key:: value]`. A trailing `^block-id` stays last.
* Tags may sit among the fields and stay part of the description.
  `🆔`, `⛔`, `🏁`, `❌` and other `[key:: value]` fields are kept as written.
* An emoji whose value isn't valid (`📅 soon`) is description text.
* New fields are written in the `[tasks] format` setting's style
  (`emoji` or `dataview`); a field that exists keeps its style. Every
  other byte of the line stays as it was.
* **Time blocks** (Day Planner): `HH:MM - HH:MM` or `HH:MM` at the start
  of the text. A block without an end lasts 30 minutes.

## Repeat rules

Natural language: `every [N] day|week|month|year`, `every weekday`,
`every weekend`, `every Monday and Thursday`, `every 2 weeks on Mon, Thu`,
`every month on the 1st[, 15th]`, `… on the last [day]`, `… on the 2nd
Tuesday`, `… on the last Friday`, `every year in March on the 15th`,
`every January`, then optionally `until 2027-06-30` and `when done`. Or
RFC 5545 text: `RRULE:FREQ=MONTHLY;INTERVAL=2;BYDAY=-1FR;UNTIL=20271231`
(`FREQ`, `INTERVAL`, `BYDAY` with ordinals for monthly and yearly,
`BYMONTHDAY` with negatives, `BYMONTH`, `UNTIL`, `WKST=MO`).

* The next date is the first one the rule gives strictly after the task's
  date (due, else scheduled, else start), or after today for `when done`.
  Dates that don't exist are skipped (RFC 5545): "every month" from 31
  January next falls on 31 March.
* Completing a repeating task writes `✅ today` on it and puts an open copy
  above it (or below: `[tasks] new_occurrence`) with every date moved by
  the same number of days and no `^block-id`.

## Task notes

A note is a task when its front matter has `type: task`, or when its
front-matter `tags` hold the task tag (`task` by default, as the TaskNotes
plugin does; `[tasks] tag` in vault.toml, empty to turn it off). The title
(`title:`, else the first heading, else the file name) is the task's text.

```yaml
---
type: task
status: doing
priority: high
due: 2026-11-20
scheduled: 2026-11-18
contexts: ["@desk"]
projects: ["[[Website]]"]
tags: [work]
time_estimate: 1h30m
time_entries:
  - start: 2026-11-18T09:00:00+01:00
    end: 2026-11-18T09:25:00+01:00
  - start: 2026-11-18T14:10:00+01:00
    end: 2026-11-18T14:35:00+01:00
---
# Write the report
```

| Meaning | Quire writes | Also read (TaskNotes, older names) | Value |
|---|---|---|---|
| status | `status` | | any word; done when it is one of `[tasks] done_statuses` or `done`, `x`, `completed`, `complete`, `cancelled`, `canceled`, `closed` |
| priority | `priority` | | `highest`, `high`, `medium`, `normal`, `low`, `lowest` (or the emoji) |
| due, scheduled, start | `due`, `scheduled`, `start` | | `YYYY-MM-DD`, a time after it is kept |
| contexts | `contexts` | `context` | a list (`@home`) |
| projects | `projects` | `project` | a list of links (`"[[Website]]"`) |
| time estimate | `time_estimate` | `timeEstimate`, `estimate` | a duration (`1h30m`, `PT1H30M`) or minutes (`90`) |
| time entries | `time_entries` | `timeEntries` | a list of `{start, end}` (or `{startTime, endTime}`), below |
| time spent without entries | `time_spent` | `timeSpent` | a duration (an inline task's total, kept when it became a note) |
| completed on | `completion` | `completedDate`, `done` | `YYYY-MM-DD` |
| created | `created` | `dateCreated` | `YYYY-MM-DD` (or a date and time) |
| repeats | `recurrence` | `repeat` | a rule (above), or RRULE text (`FREQ=WEEKLY;BYDAY=MO`, as TaskNotes writes it) |
| done dates of a repeating task | `complete_instances` | | a list of dates |
| reminder | `reminder` | `remind` | `YYYY-MM-DD HH:MM` |

* Names are matched without regard to case. A note keeps the names it
  already uses: logging time to a note with `timeEntries` appends there,
  with `startTime` and `endTime`.
* New properties use the vault's style, `[tasks] field_names`:
  `quire` (the first column) or `tasknotes` (`timeEntries`, `timeEstimate`
  in minutes, `completedDate`, `dateCreated`, no `type: task` but the task
  tag). Both styles are always read, so a vault can switch either way.
* Ticking a task note sets `status` to the first of `[tasks]
  done_statuses` and `completion` to today; unticking sets the first open
  status and removes `completion`. A note with `recurrence` stays open
  instead: `due`, `scheduled` and `start` move by the same number of days
  to the next occurrence, and today is added to `complete_instances`.
* Every edit changes only the lines of the properties it touches; comments
  and the other lines keep their bytes.

### Time entries

```yaml
time_entries:
  - start: 2026-11-18T09:00:00+01:00
    end: 2026-11-18T09:25:00+01:00
    description: first draft      # optional
```

* `start` and `end` are ISO 8601 with the offset in force where the time
  was recorded (`+01:00`, or `Z`). An instant without an offset is read in
  the device's local offset. Seconds are kept; fractions are dropped.
* An entry without `end` is running (TaskNotes writes one while its timer
  runs); it counts up to now. Quire's own timer writes an entry only when
  it stops, so it never leaves an open one.
* Totals add the entries (and `time_spent`). The weekly report counts a
  session on the days it touches in your local time (an entry written in
  UTC, as TaskNotes does, counts on the day it was where you are); a
  session past midnight counts on each day.
* Lists of maps are kept as written: Quire appends an item after the last
  one with the same indentation and never rewrites the others.

### `[tasks]` in vault.toml

```toml
[tasks]
folder = "Tasks"                       # "New task…" and "Convert task to note"
tag = "task"                           # a note with this front-matter tag is a task
statuses = ["todo", "doing", "done"]   # board columns, in order; the first is a new task's
done_statuses = ["done", "cancelled"]  # done (the first is what ticking sets)
tags = ["work", "home"]                # offered in "New task…"
field_names = "quire"                  # or "tasknotes"

[tasks.pomodoro]
work = 25
short_break = 5
long_break = 15
long_every = 4                         # a long break after this many work sessions (0: never)
auto_continue = true                   # start the next work session when a break ends
```

Every key is optional; unknown keys are kept. Settings → Planning edits
this section.

### TaskNotes compatibility

TaskNotes (the Obsidian plugin) keeps one note per task with these default
property names: `title`, `status` (`open`, `in-progress`, `done`),
`priority` (`low`, `normal`, `high`), `due`, `scheduled`, `contexts`,
`projects`, `tags` (with the `task` tag), `timeEstimate` (minutes),
`timeEntries` (`startTime`, `endTime`, `description`), `recurrence`
(RRULE), `completedDate`, `dateCreated`, `dateModified`,
`complete_instances`, `reminders`, `blockedBy`. Quire reads all of the
ones in the table above under either name. `dateModified`, `reminders`
(TaskNotes' list of reminder objects), `blockedBy`, `pomodoros` and other
keys are kept as they are and not used. To show TaskNotes' statuses as
columns, set `statuses = ["open", "in-progress", "done"]`. Property names
can be changed in TaskNotes' settings; Quire reads the defaults only.

## Converting

* **Convert task to note** (caret on a task line): makes
  `<[tasks] folder>/<text>.md` with the task's dates, priority,
  recurrence, reminder, tags (into `tags:`), `[estimate:: …]` and
  `[time:: …]` (into `time_estimate` and `time_spent`), and `created`
  (today, unless the line had one). A `[/]` box makes the note's status
  the second status (`doing`), `[-]` `cancelled`. The line becomes
  `- [ ] [[<text>]]`, keeping its indentation, bullet, checkbox character,
  a Day Planner time at the start, the fields a note has no place for
  (`[client:: Acme]`, `🆔 abc`, `⛔ xyz`, `🏁`, `❌`, verbatim) and its
  `^block-id`. One undoable edit. A line that already links to a note is
  left alone.
* A task in a **locked folder** becomes a note in that folder's own task
  folder (`Journal/Tasks/`), encrypted like any note there, named
  `Task <date>.md` rather than by its words (names in a locked folder are
  visible); the line links to it as `[[Task <date>|<text>]]`. Nothing is
  converted while the folder is locked.
* **Convert task note back to a task line** (caret on `- [ ] [[Note]]`):
  the line gets the note's title, tags and fields in the `[tasks] format`
  style, with time spent as `[time:: …]` and the estimate as
  `[estimate:: …]`, and keeps what else it had (its time, other fields,
  block id). The checkbox follows the status as above. The note goes to
  `.trash/` only if the line carries all of it: nothing in its body but
  its title heading, and only properties a line holds (`type`, `title`,
  an open, `[/]`, `[-]` or done status, dates, priority, recurrence,
  reminder, tags, estimate, time spent, and time entries without notes).
  Otherwise (contexts, projects, another status, `complete_instances`,
  session descriptions, YAML comments, any other key) it is kept and a
  notice names what stayed. Its individual time entries stay in the note
  (or the trashed copy); the line keeps their total. A task note in a
  locked folder only goes back into a note in that folder.

## Time on task lines

A task line keeps only a total: `[time:: 1h25m]`, and optionally an
estimate, `[estimate:: 2h]`. Stopping the timer on a line adds the
session's minutes to its `[time:: …]` (adding the field if it isn't
there); the rest of the line keeps its bytes. Durations are written
`1h25m`, `25m` or `2h`; `PT1H25M`, `1:25`, `90m` and `1.5h` are read.
Lines don't hold sessions, so the weekly report counts task notes only:
convert a task to a note to see its time by day.

## The timer

One timer runs per device, in `<config>/task-timer.json` (format 1):

```json
{
  "format": 1,
  "vault": "/home/ada/Notes",
  "task": { "path": "Tasks/Write the report.md", "title": "Write the report" },
  "started": "2026-11-18T09:00:00+01:00",
  "pomodoro": { "phase": "work", "phase_started": "2026-11-18T09:00:00+01:00", "done": 0 }
}
```

`task.line` (an inline task's text) and `task.at` (the line's number
when the timer started and hashes of the line, of its task and dates and
of its words, plus its `^block-id`) identify a task line; both are absent
for a task note. Stopping finds the line by its block id, else the same
line nearest where it was, else the same task and dates (a completed
recurring task, not its next occurrence), else the same words. For a task
in a **locked folder**, the file holds no words at all: no `line`, and
`title` is "a task in <note>"; notifications say the same.

The app and `quire-cli tasks start|stop|status` share the file, so a
timer survives restarts and either can stop it: the app reads it again
whenever it changes, and each side changes it only if it still holds
what that side last read. Nothing is written to the vault until it stops
(or a pomodoro's work session ends); then one entry is written to the
task as above, through the same path as any edit (history before and
after), and only then is the timer cleared. A session under a minute (a
timer started by mistake) isn't logged. Starting a timer stops and logs
the running one. A timer started in another vault can only be stopped
there.

When the entry can't be written (the note was moved or deleted outside
Quire, its folder is locked, the line is gone, its `time_entries` has a
form Quire doesn't change), or the session is over 8 hours (a timer left
running), the session isn't lost: the file keeps it with `"unlogged":
"<end>"`, the status bar shows it, and it waits until it is logged to a
task ("Try again", "Log here" on a task, Start/stop timer with the caret
on a task, `quire-cli tasks stop [--to <task>]`) or discarded ("Discard",
`stop --discard`). A timer file that can't be read is moved aside to
`task-timer.json.bad` and Quire says so. A rename in Quire takes the
timer's task along.
