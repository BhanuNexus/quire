# Home: `[home]`

Home is an optional first page of the app: recent notes, pinned notes, what
is due, a view, what waits in the Inbox, quick capture, a random note, today's
note and writing counts. Owner: 0.12 switching tools. Code:
`quire_app::switch_state` (settings and data), `quire-desktop` `home_ui` (the
page).

```toml
[home]
open-on-start = false        # show Home when the app starts
sections = ["recent", "pinned", "tasks", "inbox", "capture", "daily", "random", "stats"]
recent = 8                   # rows in "Recent notes" (at most 50)
pinned = ["Projects/Quire.md", "Reading list.md"]
view = "from: #project\nsort: due"   # a quire-view spec, or the path of a .base file
base-view = 0                # which view of the .base file
word-goal = 0                # words a day to aim for; 0 for no goal
```

Every key is optional. `sections` lists the sections in the order they are
drawn: `recent`, `pinned`, `tasks`, `view`, `inbox`, `capture`, `daily`,
`random`, `stats`. Unknown ids and repeats are ignored; `view` is left out of
the default because it needs a `view` value.

## Sections

| Id | Shows | Comes from |
|---|---|---|
| `recent` | notes opened lately, newest first (the open one too: Home is a tab of its own) | the workspace's recent list |
| `pinned` | notes named in `pinned` (those that no longer exist are listed as missing) | `[home] pinned`; "Pin to Home (or unpin)" edits it |
| `tasks` | overdue, today's tasks and today's time blocks | the Agenda |
| `view` | a `quire-view` spec, or one view of a `.base` file (its saved query, shown read-only; formulas and summaries aren't computed on Home, open the file for those) | the same engine as views and Bases |
| `inbox` | suggested changes waiting for a decision (titles), and how many checks and other items wait | the Inbox |
| `capture` | a box that adds a timestamped line to today's note | Quick capture |
| `daily` | open today's note (created when you open it), or "Today's note" with its path | periodic notes |
| `random` | opens a note at random (not the open one, not a template) | the index |
| `stats` | notes, words, open tasks, and words written today against `word-goal` | the index |

"Words today" is the vault's word count now minus its count the first time
Home was drawn that day on this device (kept in `<config>/vaults/<key>/home-stats.json`,
`{"day": "2026-11-10", "words": 41230}`, never in the vault). Deleting text
makes it negative.

## When it is worked out

Home opens after the first frame (never before it), and with
`open-on-start` it opens on the third frame. Nothing is computed while it
isn't on screen. While it is, the lists come from queries the app keeps
(recent notes, the agenda, the inbox), the counts are cached until the
notes change, and long lists draw only the rows in view. It is keyboard
accessible: Tab moves between sections and rows, Enter opens, and the
quick-capture box takes focus with `/`.
