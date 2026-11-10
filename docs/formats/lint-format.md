# Tidy (the Markdown linter): `[lint.format]`

"Tidy this note", "Tidy notes in a folder…" and `quire-cli format` clean up
Markdown with the rules turned on in `vault.toml`. Owner: 0.12 switching
tools. Code: `quire_markdown::linter` (the rules), `quire_app::switch_state`
(commands, settings, the folder plan).

The settings are in `[lint.format]`. The vault check (the Inbox's Checks)
owns the keys directly under `[lint]` (`exclude` and a level per check);
Quire keeps both when it writes one.

```toml
[lint.format]
on-save = false              # tidy when you press Save (not on autosave)
exclude = ["Archive", "Clippings/*"]   # folders or globs left alone
heading-increment = true     # no skipped heading levels, the outline's shape kept
heading-start = 0            # the first heading is at most this level (0: any)
heading-style = "atx"        # "atx": # Title instead of Title over ====; "keep"
trailing-spaces = true
two-space-linebreak = "keep" # "keep": two spaces that make a line break stay; "strip"
list-marker = "-"            # "-", "*", "+" or "keep"
list-indent = "keep"         # "2", "4", "tab" or "keep"
blank-lines = true           # a blank line around headings, lists and code
max-blank-lines = 1
final-newline = true         # the file ends with one line ending
front-matter = true          # key: value spacing, no trailing spaces
front-matter-order = ["title", "tags", "date"]   # these keys first, in this order
front-matter-sort = "keep"   # "alpha": the other keys by name
tags-format = "keep"         # "flow": tags: [a, b]; "list": one per line; "keep"
tags-case = "keep"           # "lower"
date-modified = false        # set a property when a note is tidied or saved
date-modified-key = "modified"
date-format = "%Y-%m-%dT%H:%M"   # strftime; no line breaks, not starting with ":"
link-spacing = true          # [[ Note | alias ]] becomes [[Note|alias]]
emphasis = "keep"            # "asterisk" (*x*) or "underscore" (_x_)
strong = "keep"              # "asterisk" (**x**) or "underscore" (__x__)
line-endings = "keep"        # "lf" or "crlf" converts every line
```

Every key is optional; the values above are the defaults except where the
comment shows other choices. A value Quire doesn't know is reported as an
unreadable section and the defaults are used, as for every section.

## What it never touches

* code blocks (fenced and indented), inline code, math (`$…$`, `$$…$$`),
  HTML (blocks and inline), and the destinations of links;
* front matter it can't read as plain `key: value` lines, or that isn't valid
  YAML (comments and values it doesn't know pass through byte for byte). Tags
  are rewritten only when they are plain text items: a `tags` value with a
  comment, an anchor or alias, a flow map or a block scalar is left as it is.
  A tag YAML would read as something else (`"true"`, `"123"`, a date, an item
  with a comma) is written quoted;
* a note where the parser finds a front matter block that doesn't start the
  note (a `---` line near the start can do that), and a file that isn't
  UTF-8 text;
* with `front-matter-sort`, a front matter that has anchors or aliases (their
  order matters in YAML);
* a note whose front matter has `lint: false`, notes in `exclude`, templates,
  and notes in locked folders;
* a file with old Mac line endings (a lone CR).

## Guarantees

Each rule works on the text and the parser's reading of it. After a rule
runs, the note is read again; if the result differs in anything but
whitespace, list markers, indentation, emphasis characters, heading style
and the other markup the rule is for, the rule's change is dropped and the
rule is named in the report. (Heading levels are compared ignoring level
when the headings rule runs.) Tests check, over thousands of generated
notes, that

* tidying twice gives what tidying once gives;
* nothing but whitespace and markup changes, and no rule needs that check;
* code, math and HTML come out as they went in;
* CRLF stays CRLF, and lines the linter adds use the file's majority ending;
* unreadable front matter comes out as it went in;
* the front matter's YAML, read by a real YAML parser, means the same after
  tidying (apart from the keys a rule is for: tags, the "date modified" key).

"Date modified" is set only when the note was edited by hand (saved) or some
other rule changed it, so tidying a tidy note changes nothing. The line
endings rule converts the whole file, front matter included.

## Where it runs

| Where | How |
|---|---|
| The open note | "Tidy this note (lint)": one undo step in the editor |
| On save | `on-save = true`: Save (Ctrl+S, "Save") tidies first, in the same undo step; autosave never does |
| A folder | "Tidy notes in a folder…": the plan is made off the UI thread, each changed note shows its diff, untick the ones you don't want; each is written through the safe write path with history (provenance "human"), as one batch with "Undo" |
| Headless | `quire-cli format <vault> [note-or-folder] [--write]`: a diff first |

A folder tidy leaves alone a note with unsaved edits, and a note that
changed on disk after the plan was made; both are counted in the report.
Closed notes are written only over the text the plan was made from.
