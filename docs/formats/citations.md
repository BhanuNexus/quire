# Citations: `[@key]` and the bibliography

Owner: WS-G. Types: `quire_markdown::cite` (syntax), `quire_app::citations`
(library, formatting, import).

## In notes (Pandoc syntax)

| Written | Shown (preview, HTML, publish) |
|---|---|
| `[@lovelace1843]` | (Lovelace 1843) |
| `[@lovelace1843, p. 4]` | (Lovelace 1843, p. 4) |
| `[see @a; @b]` | (see Lovelace 1843; Babbage 1864) |
| `[-@lovelace1843]` | (1843) |
| `@lovelace1843` | Lovelace (1843) |
| `@lovelace1843 [p. 4]` | Lovelace (1843, p. 4) |
| `@{key with spaces}` | braces allow any key |

Citations in code, math, HTML, front matter and link targets are text.
A citation whose key isn't in the library stays as written. Works by the
same author in the same year get `1968a`, `1968b` (by title). The rendered
note ends with `## References` (or the list goes under an existing last
heading named References or Bibliography), sorted by author and year, in
Chicago author-date. The PDF export (full edition) passes the cited works to
Typst's `bibliography()` with the style from `[citations] style`.

## Library files

BibTeX or BibLaTeX files in the vault, named in `.quire/shared/vault.toml`
([vault-prefs.md](vault-prefs.md#citations-ws-g)); `references.bib` at the
vault root is used when none is named. A key defined twice: the first file
wins. A work's `file` field (Zotero, Better BibTeX) naming a PDF links the
citation to that PDF in the vault.

**Import** reads a `.bib` or CSL-JSON file (Zotero: Export → CSL JSON) and
merges it into the first library file by key: new keys are appended,
changed ones replaced in place (keeping their `file` field), everything else
in the file stays as written. The dialog shows the dry run first.
