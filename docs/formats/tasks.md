# Tasks in notes

Tasks are Markdown checklist items. Quire reads the fields Obsidian Tasks
and Dataview write after a task's text, and writes them back in place.
Owner: WS-E. Code: `quire_markdown::tasks` (syntax), `quire_plan`
(dates, recurrence, agenda, reminders).

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

A note with `type: task` in its front matter is a task too: `status`
(`done`, `x`, `completed` or `cancelled` count as done), `due`,
`scheduled`, `start`, `recurrence` (or `repeat`), `priority`, `reminder`.
"Convert task to note" makes `Tasks/<text>.md` and leaves `- [ ] [[<text>]]`.
