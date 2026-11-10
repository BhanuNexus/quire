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

Each `[[link]]`, `![[embed]]` and `[text](note.md)` inside the value is one
typed link. Text in the value that isn't a link is ignored. Links in code
spans, code blocks and math are never typed (or links).

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
set per vault in `.quire/shared/vault.toml` (read only; Quire doesn't write
this section):

```toml
[links]
up = ["up", "parent"]       # Breadcrumbs' default is ["up"]
down = ["down", "child"]
same = ["same", "sibling"]
next = ["next"]
prev = ["prev"]
implied = true              # A up B gives B down A
trail = "auto"              # auto | on | off
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
| Breadcrumbs list notes, tag notes, Dendron/date/regex notes, transitive rules beyond reversal | not read (the links stay ordinary links) |
| Obsidian Bases `file.links`, a property `up` | `file.links`, `up` (`up = X` matches `up: "[[X]]"`) |

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
  the `]` and any comment, or a new `- "[[X]]"` line); a single link becomes
  a flow list of two; a missing property is added as `key: "[[X]]"` at the
  end of the front matter (which is created when there is none);
* an inline field line gets `, [[X]]`; a new field goes on its own line
  after the note's last line field, else after its first `# ` heading, else
  at the top of the body.

**Renames and moves** rewrite typed links like any other link: only the
target inside `[[…]]` changes, every other byte of the note stays. In a
single-quoted YAML value a `'` in the new name is written `''`.
