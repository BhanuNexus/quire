# Machine data in notes: `<!-- quire:KIND BODY -->`

When Quire needs to keep data inside a note (P1), it goes in an HTML comment
next to readable text, so the note reads well anywhere and the data survives
other editors. Helpers: `quire_markdown::comment`. Owner: F0.

* `KIND` is letters, digits, `:`, `-` and `_`: `cite`, `index:start`,
  `index:end`, `pdf-anchor`.
* `BODY` is free text, usually `key=value` pairs (`comment::parse_args`) or
  JSON. Values with spaces, quotes, `=` or newlines are double-quoted with
  `\"`, `\\` and `\n` escapes.
* `-->` inside a body is written as `--&gt;` and read back as `-->`, so a
  body can never end the comment early.

Kinds in use or reserved:

| Kind | Body | Owner |
|---|---|---|
| `cite` | `v=1 sha=<32 hex> l=<start>-<end> p="<prefix>" s="<suffix>"` after a footnote citation | WS-C |
| `index:start`, `index:end` | none; the generated part of `wiki/index.md` is between them | WS-D |
| `pdf-anchor` | JSON, see [pdf-annotations.md](pdf-annotations.md) | WS-G |
