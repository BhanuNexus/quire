# Boards, groups and maps in views

How a board's hand-placed card order, swimlanes and column colours are
kept, for `quire-view` blocks and Obsidian `.base` files, and where map
pins come from. Owner: the 0.11 tasks workstream. Code:
`quire_index::views` (quire-view keys), `quire_import::obsidian::base`
(`.base` keys), `quire_app::tasks_state::BoardSetting` (writing them).

**The notes are never rewritten to keep an order.** The order lives in the
view (one line of the block) or in the base file (one key of the view).
Moving a card to another column changes that note's group property, as
before; nothing else in any note changes.

## In a `quire-view` block

````markdown
```quire-view
from: Projects/
layout: kanban
group: status
swimlanes: priority
color.doing: blue
color."waiting on: client": "#e5484d"
order.todo: [Projects/Launch.md, Projects/Site.md]
order.(none): [Projects/Idea.md]
```
````

| Key | Value |
|---|---|
| `swimlanes:` (or `lanes:`) | a field: the board is drawn once per value of it, in rows. Lanes sort by priority rank for priorities, else by name; notes without a value are last |
| `color.<column>:` | a colour name (`red`, `orange`, `yellow`, `green`, `teal`, `blue`, `purple`, `pink`, `gray`) or `#rrggbb` |
| `order.<column>:` | card ids in the order they were placed: `[a, b]`. Cards not listed follow, in the view's `sort:` order. A listed card that has since moved to another column is placed by that column's list only |

* `<column>` is the group value as written. `(none)` is the column of
  notes without one. A value with `:`, `#`, quotes, `\` or spaces at its
  ends is written in double quotes, with `\"` and `\\` escapes.
* Card ids are paths, so a card renamed or moved outside its column's
  list loses its hand-placed position (it follows the sort order until
  it is dropped again).
* A card id is the note's path (`Projects/Launch.md`). In a `from: tasks`
  view, an inline task's id is `path#text` (`Inbox.md#Call Bo`); a task
  note's is its path. Ids with `,`, `"`, `[`, `]`, `\` or `#` are written in
  double quotes with `\"` and `\\` escapes.
* Dropping a card writes that one column's `order.` line, listing the
  column's cards as they show; dropping it back where it was writes
  nothing. The other lines of the block keep their bytes.
* Older Quire versions report `order.…`, `color.…` and `swimlanes` as
  unknown keys; the view still shows there without them.

## In an Obsidian `.base` file

```yaml
views:
  - type: kanban
    name: Board
    groupBy:
      property: note.status
      direction: ASC
    order: [file.name, priority, due]      # Obsidian's: the properties shown
    swimlaneBy: note.priority              # Quire
    columnColors:                          # Quire
      doing: blue
    cardOrder:                             # Quire
      todo:
        - Tasks/B.md
        - Tasks/A.md
    columnOrder: [todo, doing, done]       # Quire reads it; written by hand
```

* `groupBy` is Obsidian's (a property and a direction), and groups tables
  and cards too: a table shows a heading row per group.
* `swimlaneBy`, `columnColors`, `cardOrder` and `columnOrder` are Quire's
  own keys inside the view. (Obsidian's `order:` is the list of properties
  shown, so it can't hold cards.)
* Quire edits a `.base` file line by line: setting a key replaces only that
  key's lines inside its view (or adds them at the end of the view), and
  removing one removes only its lines. Comments, key order, keys Quire
  doesn't know and the other views keep their bytes. A base whose `views`
  is written as a flow list (`views: [{…}]`), or whose view is a flow map
  (`- {type: kanban, …}`), isn't edited; Quire says so. Every edit is read
  back before it is written: one that wouldn't parse, or would lose a
  view, isn't written.
* Obsidian's docs don't say whether it keeps keys it doesn't know when it
  saves a base. If it drops them, only the board's order, lanes and
  colours are lost.

## The agenda's board (0.12)

The agenda's own board (task notes by status) has no view in the vault,
so its card order, column colours and swimlanes stay on the device, in
the vault's state folder: `<config>/vaults/<key>/agenda-board.json`.

```json
{ "format": 1, "view": "order.open: [Tasks/B.md, Tasks/A.md]\ncolor.open: teal\n" }
```

`view` holds the lines Quire adds to the board's view, with the keys of a
`quire-view` block above (`order.<column>`, `color.<column>`,
`swimlanes`). Card ids are note paths, never a task's text. A device
without the file shows the board in its default order.

## "+" in a column

Typing a title after a column's "+" makes, in this order of preference:
a note from the view's `new:` template; for a `from: tasks` board grouped
by status, a task note (`[tasks] folder`); otherwise a plain note in the
view's first folder (`from: Projects/`, or a base's `file.inFolder`). The
column's value is set in its front matter, and the lane's too on a board
with swimlanes. A folder taken from a view must be inside the vault and
not hidden: "+" never makes a note in `.quire/`, `.trash/` or another
folder whose name starts with `.`.

## Map views

`layout: map` in a quire-view, or `type: map` in a base. A note is a pin
when it has:

* `lat` (or `latitude`) and `lng` (or `lon`, `long`, `longitude`), or
* `location` (or `coordinates`, `coords`) as `[lat, lng]` or `"lat, lng"`.

Out-of-range values are ignored. The world outline is Natural Earth's
1:110m land (public domain), bundled with the app; no map tiles are
fetched. In a base, `coordinates: note.place` names another property.
