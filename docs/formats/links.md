# Typed links

A typed link is an ordinary link that sits in a named field. The field's
name is the link's type: `parent`, `supports`, `source`. Quire reads the
same three forms as Dataview and Breadcrumbs, so a vault keeps working when
it moves between them. Nothing about a typed link is stored outside the
note. Owner: 0.12 typed links. Code: `quire_markdown::fields` (parsing,
writing), `quire_index::relations` (edges, hierarchy).

## The three forms

````markdown
---
up: "[[Projects]]"                 # a property holding one link
related: ["[[Design]]", "[[Q4]]"]  # a list of links, flow style
down:                              # or block style
  - "[[Limits]]"
  - "[[Series]]"
---
parent:: [[Rope]]                  # an inline field on its own line
child:: [[A]], [[B|bee]]           # several links in one field
- up:: [[List Parent]]             # after a list marker or `>` too

A claim [supports:: [[Theory]]] in a sentence, and (source:: [[Book]]).
- [ ] a task [due:: 2026-12-01] [blocked-by:: [[Design]]]
````

| Form | Rule | Same as |
|---|---|---|
| Line field | `key:: value` with nothing before the key but spaces, `>` quote markers and one list marker (`-`, `*`, `+`, `1.`). Not on a task line: a task needs the bracketed form. | Dataview "full line", Breadcrumbs inline |
| Bracketed field | `[key:: value]` anywhere on a line. The value runs to the matching `]`, so `[[links]]` inside it are fine. | Dataview |
| Paren field | `(key:: value)`; reading views hide the key in Dataview. | Dataview |
| Property | A front-matter property whose value holds `[[links]]`: a single value, a flow list or a block list. Nested maps take the name of the nearest key. Obsidian needs the quotes (`"[[X]]"`); Quire reads `up: [[X]]` too. | Obsidian properties, Breadcrumbs |

Each `[[link]]` and `![[embed]]` inside the value is one typed link. A
Markdown link (`[text](note.md)`) in a property is **not** a link: write
`"[[note|text]]"`. (In the note's text, Markdown links are typed in inline
fields as before.) Text in the value that isn't a link is ignored, and so is
a YAML comment (`up: x  # see [[Old]]`, or a line that starts with `#`).
Links in code spans, code blocks and math are never typed (or links).

**Keys.** As in Dataview: letters, digits, `-`, `_`, `/` and spaces (other
non-ASCII characters, such as emoji, too), starting with a letter or digit,
optionally wrapped in `**`, `_` or `~`. A key with `?`, `.`, `:` or brackets
isn't a field (`What is ATP? :: …` stays a flashcard, `http://x::y` stays
text).

**Type names.** The key, normalized as Dataview does: wrapping formatting
dropped, lower case, runs of spaces as one `-`. `**Parent Note**::` is type
`parent-note`; `see_also` stays `see_also`.

## Hierarchy

Five kinds of type build a hierarchy. Which field names mean which kind is
set per vault in `.quire/shared/vault.toml`, by hand or in Settings → Links
(which keeps comments, unknown keys and every other byte of the file, and adds
no key that is still at its default):

```toml
[links]
up = ["up", "parent"]       # Breadcrumbs' default is ["up"]
down = ["down", "child"]
same = ["same", "sibling"]
next = ["next"]
prev = ["prev"]
implied = true              # A up B gives B down A
trail = "auto"              # auto | on | off

[links.types.supports]      # a free-form type's colour in the graph (0.13)
color = "#2e7d32"           # #rrggbb or #rgb; other keys here are kept

[links.dendron]             # a.b.c is a child of a.b (0.13, off by default)
enabled = true
field = "up"                # the field the child has for its parent
delimiter = "."
```

Every key is optional; the values above are the defaults. Field names are
normalized like keys. Any other field name is a free-form type (`supports`,
`contradicts`, `source`, `related`…) with no reverse.

**Implied edges.** With `implied = true` (the default, as in Breadcrumbs),
each hierarchy link gives the other note the reverse edge: `up` ↔ `down`,
`next` ↔ `prev`, `same` ↔ `same`. An implied edge is shown with the first
name of its kind (`down`). A link written both ways counts once.

**The trail** above a note shows the paths from a top note (one with no
parent) down to it along `up` edges: several when a note has several
parents (at most four), each stopping where it would repeat a note (a
cycle). `trail = "auto"` shows it only when some note in the vault has a
hierarchy field; a note without a parent shows none.

**Siblings** are a note's `same` edges and, for "Go to next/previous
sibling", the other children of its first parent sorted by name; a `next` or
`prev` edge wins over that order.

## Mapping from Dataview and Breadcrumbs

| They write | Quire reads it as |
|---|---|
| Dataview `Key:: [[X]]`, `[key:: [[X]]]`, `(key:: [[X]])` | a link to X of type `key` |
| Dataview `file.outlinks`, `file.links` | the view field `outlinks` (also `file.links`) |
| Dataview `WHERE up = [[X]]`, `contains(file.outlinks, [[X]])` | `where: up = X`, `where: outlinks contains X` |
| Breadcrumbs v4 edge fields `up`, `down`, `same`, `next`, `prev` | the same kinds; other edge fields are free-form types |
| Breadcrumbs frontmatter links `up: "[[X]]"`, `down: ["[[A]]", "[[B]]"]` | as written |
| Breadcrumbs implied "opposite direction" rules | `implied = true` |
| Breadcrumbs custom field groups (e.g. `broader` as an up field) | list the names under `up`, `down`… in `[links]` |
| Breadcrumbs tag notes, folder notes, list notes, Dendron notes | derived edges, below |
| Breadcrumbs Johnny.Decimal, date and regex notes, transitive rules beyond reversal | not read (the links stay ordinary links) |
| Obsidian Bases `file.links`, a property `up` | `file.links`, `up` (`up = X` matches `up: "[[X]]"`) |

## Derived edges (0.13)

Breadcrumbs also makes edges from how notes are named and filed. Quire reads
four of them, with Breadcrumbs v4's property names (written as there, case
included), so a Breadcrumbs vault keeps its hierarchy. The edges are built
with the others, shown with the same groups and trail, and marked as derived;
nothing is written to any note. A derived edge that is also written in a note
counts once, as the written one.

| Property of the note | Meaning |
|---|---|
| `BC-tag-note-tag: "#recipe"` | Edges to every note with that tag **or a nested tag** (`#recipe/dessert`). `#` is optional. |
| `BC-tag-note-field: down` | The edge's field (any name; the first `down` name when missing, where Breadcrumbs needs it). |
| `BC-tag-note-exact: true` | Only notes with exactly that tag. |
| `BC-folder-note-field: down` | The note is a folder note: edges to the other notes in its folder. |
| `BC-folder-note-recurse: true` | And every folder below it, flattened: all of them are its children. |
| `BC-list-note-field: down` | A list note: an edge to each note linked in a list (`-`, `*`, `+`, `1.`; code blocks skipped, the first `[[link]]` of an item counts). A nested item is a child of the item above it, not of the list note. |
| `BC-list-note-exclude-index: true` | No edges from the list note to its top-level items. |
| `BC-list-note-neighbour-field: next` | Edges between consecutive items of one list. |
| `- child [[B]]` or `- child:: [[B]]` in a list note | The word before the link names that item's edge, instead of the list note's field. |

**Dendron names** are set in `[links.dendron]` (or Settings → Links), not in
a note: with `enabled = true`, `a.b.c.md` gets an edge, in `field` (default
`up`), to `a.b.md`, which has one to `a.md`. Only the exact parent counts: if
`a.b.md` doesn't exist there is no edge to `a.md`. The delimiter can be any
text. (Breadcrumbs' "display trimmed" isn't read: titles are shown as they
are.)

A list note reads at most 5000 items. All the edges follow `[links]`: a field
named like an `up` field is an `up` edge, and `implied` makes the reverse.
Which scheme Quire reads, and why only these four: [design/0.13-links.md](../design/0.13-links.md).

## Views and Bases

In `quire-view` blocks a typed field reads as its link targets: `where: up =
Projects`, `where: supports exists`, `group: up`. A value written as a link
compares by its target (`up = "[[Projects]]"` matches too). `outlinks` lists
every link target; `outlinks.<type>` the targets of one type. In `.base`
files a typed field is a property, as in Obsidian.

## Writing

"Add typed link…", `add_link` (MCP) and nothing else write typed links. They
use the style the note already uses: inline fields when it has more inline
typed links than property ones (or inline fields and no typed links at
all), else properties. Only the field's own lines change:

* a property list gets another item in its own style (`[…, "[[X]]"]` before
  the `]`, also when the list runs over several lines, or a new `- "[[X]]"`
  line); a single value becomes a flow list of two, with its own text and
  its trailing comment kept; a missing property is added as `key: "[[X]]"`
  at the end of the front matter (which is created when there is none). A
  `"` or `\` in the target is escaped. A property that is a block scalar
  (`|`, `>`), a multi-line text, an anchor or alias, or nested properties is
  not rewritten: the tool says so and nothing changes. A nested key's link
  (`meta:` then `up:`) is not copied to a top-level property;
* an inline field line gets `, [[X]]`; a new field goes on its own line
  after the note's last line field, else after its first `# ` heading, else
  at the top of the body.

**Renames and moves** rewrite typed links like any other link: only the
target inside `[[…]]` changes, every other byte of the note stays. The new
name is written as YAML needs it where the link sits: in single quotes a
`'` is written `''`, in double quotes `\` and `"` are escaped, and in an
unquoted value a name YAML could read differently (`Smith, John`, `@Work`,
`Ada's`) gets the whole value quoted (`up: "[[Smith, John]]"`). If the front
matter was valid YAML and the change would make it invalid, its links are
left as they were.
