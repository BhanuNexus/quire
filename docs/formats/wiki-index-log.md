# `wiki/index.md` and `wiki/log.md`

The wiki loop's two maintained notes (design §D.7). Owner: WS-D. Code:
`quire_app::assistant_state::wiki`. Both are plain Markdown.

## `wiki/index.md`

Quire rewrites only the text between the markers; everything outside them
is the person's and is kept byte for byte. Without markers they are added
at the end; without a file, one is started.

```markdown
# Wiki index

<!-- quire:index:start -->

## Sources

- [[Ropes paper]] — Boehm et al. on ropes

## Concepts

- [[Gap buffers]] — Gap buffers keep a hole at the cursor.
- [[wiki/concepts/Ropes|Ropes]] — A rope is a tree of strings.

<!-- quire:index:end -->
```

* Every Markdown file under `wiki/` except `index.md` and `log.md`.
* Grouped by front-matter `type` (Sources, Concepts, Entities, Answers,
  then other types, then "Other pages"), then by folder (a `###` heading
  when a group spans folders), sorted by name.
* One line per page: a link (with its folder when the name alone is
  ambiguous in the vault) and its `description`, else its first sentence,
  at most 120 characters.
* Regenerated 10 s after a wiki page changes, as `job:index` (waiting while
  the file is open with unsaved edits), and by every ingest.

## `wiki/log.md`

Append-only, one line per event, placed like quick capture places a
line (under a `Log` heading if there is one, else at the end):

```markdown
- 2026-11-02 14:05 · ingest · [[raw/Paper]] → 7 pages · Claude Sonnet (run r_0J8…)
- 2026-11-02 15:10 · agent run · 3 pages changed · Claude Code (run r_0J9…)
- 2026-11-02 16:00 · accepted · 2 suggestions from claude-code
```

Ingest writes its own line in its run (so "Revert run" removes it).
`job:log` adds a line for each other agent run that changed `wiki/`, once
it has been quiet for a minute, and for each batch of accepted suggestions
in `wiki/`.
