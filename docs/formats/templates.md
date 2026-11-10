# Note templates

Templates are notes in the templates folder (`Templates/`). Placeholders
are filled in when a note is made; nothing in a template ever runs.
Owner: WS-E. Code: `quire_vault::templates`.

| Placeholder | Becomes |
|---|---|
| `{{title}}`, `{{folder}}` | the new note's name and folder |
| `{{date}}`, `{{time}}` | `2026-11-03`, `09:05` |
| `{{date:%A %-d %B}}` | any strftime format |
| `{{date+7d}}`, `{{date-1w:%F}}`, `{{date+1m}}`, `{{date+1y}}` | date maths (`d`, `w`, `m`, `y`) |
| `{{date:next monday}}`, `{{date:last friday:%a}}` | `today`, `tomorrow`, `yesterday`, a weekday (this week's), `next`/`last <weekday>`, `next week`, `start of`/`end of week`, `month`, `year` |
| `{{cursor}}` | where the caret goes |
| `{{prompt:Client}}`, `{{prompt:Client\|ACME}}` | asked before the note is made (with a default) |
| `{{select:Status\|planned, active, done}}` | a choice; the first is the default |

* A question is asked once per key (keys ignore case); a repeated key
  without a default takes its first use's default.
* Unknown placeholders and broken formats are left as written.
* Where Quire can't ask first (the "New note from template" command,
  periodic notes, a folder's default template), questions without a
  default stay as `{{prompt:…}}` in the note, which shows a form to fill
  them in (one undoable edit).

## Settings in the template

```yaml
---
quire-template:
  folder: Meetings/          # where "+ New" in a view puts the note
  name: "{{date}} {{prompt:Topic}}"
  applies_to: [Meetings/]    # a new empty note in Meetings/ gets this template
tags: [meeting]
---
```

`quire-template:` is left out of the notes made from the template (and the
whole front matter, if it was the only field).

## The default template for new notes

`applies_to: ["*"]` makes a template the vault's default: every new note
that no other template names gets it. A folder's own template wins over the
default (`applies_to: [Meetings/]` in `Meetings/` and below; the deepest
folder wins). They apply to:

* **Ctrl+N** (a new empty note, in the open note's folder or the vault's top,
  per Settings → Editing);
* **a click on a link to a note that doesn't exist yet** (`[[Brand New]]`, or
  or the "New note" row in Quick Open). That note is made at the top of the
  vault, so the default applies, not a folder's template. Without a default
  template it starts as `# Brand New`.

Notes inside the templates folder are never templated. `quire-template:`
questions without a default stay as `{{prompt:…}}` for you to fill in.

## Views

`new: Templates/Project.md` in a `quire-view` block adds **+ New**, which
asks the template's questions in place and makes the note in the view's
first folder (else the template's `folder:`), with the board's group field
set to the column chosen.
