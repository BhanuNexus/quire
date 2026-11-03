# Span citations in notes

A saved answer (Ask → Save as note, MCP `save_answer`) cites its sources as
footnotes that read well in any Markdown editor, with the data Quire needs to
find the quoted passage again in a `quire:cite` comment (design §C.3).
Its front matter says who answered in `answered_by`: `model:<name>` for a
model in Ask, `quire:extractive` when Quire quoted the notes itself, or the
agent's id for MCP `save_answer`.
Owner: WS-C. Code: `quire_intel::anchor` (`footnote`, `citations`, `resolve`).

```markdown
Ropes make undo cheap.[^1]

[^1]: [[Rope Notes#Undo]] — “Ropes make undo cheap because subtrees are shared.” <!-- quire:cite v=1 sha=1a2b3c4d5e6f7a8b9c0d1e2f3a4b5c6d l=3-3 p="## Undo\n" s=" #cs" -->
```

* One footnote per line: `[^label]: [[Target]] — “quote” <!-- quire:cite … -->`.
  `Target` is the cited note, with `#Heading` or `#^block` when there is one.
  The quote is whole sentences, whitespace runs written as one space, at most
  400 characters (longer ones keep the first and last 150, joined by `…`).
* The comment body is `key=value` pairs (`quire_markdown::comment`):

  | Key | Meaning |
  |---|---|
  | `v` | format version, `1` |
  | `sha` | the cited note's version when cited: the 32 hex of its content hash |
  | `l` | 1-based inclusive lines of the quote then, `3-3` |
  | `p`, `s` | up to 32 characters before and after the quote (quoted, `\n` escapes) |
  | `b` | optional: the paragraph's block id, without `^` |

  Readers ignore keys they don't know. A footnote without a `quire:cite`
  comment is an ordinary footnote.
* The sentence citing a footnote is the one holding `[^label]` in the body;
  "Check citations" asks the AI model whether the passage supports it.

## Finding the passage again

`resolve` tries, in order: the note is unchanged (`sha`), the block id, the
exact quote (whitespace-insensitive; context and distance pick between
copies), the old version's lines mapped through a line diff (old text from
history, matched by `sha`; a passage at least 90% similar there has
*moved*), and finally the most similar run of sentences anywhere: at least
75% similar is *drifted*, less is *lost*. Drifted and lost citations become
`CitationDrift` Inbox items and a note under the footnote; nothing in the
note changes until the person picks "Update citation", which rewrites that
one footnote line.
