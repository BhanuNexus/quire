# Quire's file formats

Notes are plain Markdown and need nothing from Quire to be read. Everything
else Quire keeps is described here: what each file holds, where it lives and
who may change its format. The rules come from the 0.9 design (§1 P1–P5,
§2.4) and apply to every file below.

## Rules for every file

* **Versioned.** JSON and TOML files carry `format = N` (`"format": N`).
  JSONL logs carry `"v": N` on every line.
* **Forward compatible.** Readers accept fields they don't know and keep them
  when they rewrite the file (`#[serde(flatten)] other`). A reader that sees
  a `format` newer than it knows reads what it can and never rewrites the
  file. A JSONL reader skips lines it can't parse or whose `v` is newer, and
  leaves them in place.
* **Atomic.** Vault files are written through `Storage::write_bytes_atomic`
  (temporary file, then rename). Files in the config folder go through
  `quire_app::atomic::write`. Logs are appended with `Storage::append_line`,
  which locks the file around one write of one line.
* **Edition independent.** Quire and Quire Lite read and write every file
  here. A feature missing from Lite shows what it can and never drops data.
* **Device-local files are never needed on another device.** Losing
  `.quire/` loses history, proposals and caches, never note text.

## Shared building blocks

| Thing | Form | Where it's defined |
|---|---|---|
| Content hash (a note version) | `sha256:` + the first 32 hex characters of SHA-256 over the UTF-8 bytes, after removing a byte-order mark | `quire_vault::ContentHash` |
| Ids | prefix + 13 Crockford base32 characters of Unix ms + 8 random ones: `p_` proposal, `r_` run, `b_` batch. They sort by time. | `quire_agent::ids` |
| Device id | `d_` + 12 random Crockford base32 characters | `quire_agent::DeviceInfo` |
| Paths | vault-relative, `/`-separated, NFC (`Rope Notes.md`, `wiki/Ropes.md`) | `quire_vault::NotePath` |
| Machine data in a note | `<!-- quire:KIND BODY -->` next to readable text | [comments.md](comments.md) |
| Vault key (config folder per vault) | 16-hex FNV-1a of the canonical vault root, as for history folders | design §2.4 |

## Files

Owner is who changes the format from now on (workstreams of design §6.2;
F0 is the foundation step, after which its files are frozen).

| File | Format | Doc | Owner | State |
|---|---|---|---|---|
| `.quire/history/<fnv>/<ms>.md.gz` | gzip of the note text | (0.8, unchanged) | — | in use |
| `.quire/history/<fnv>/<ms>.prov.json` | JSON, format 1 | [provenance.md](provenance.md) | WS-A | written by agent writes and accepted suggestions |
| `.quire/agent/ledger/<device>/<yyyy-mm>.jsonl` | JSONL, v 1 | [ledger.md](ledger.md) | WS-A | written |
| `.quire/agent/proposals/<id>.json`, `archive/<yyyy-mm>/<id>.json` | JSON, format 1 | [proposals.md](proposals.md) | WS-A | written |
| `.quire/agent/inbox/<fingerprint>.json`, `dismissed.jsonl` | JSON format 1, JSONL v 1 | [inbox.md](inbox.md) | WS-A | written |
| `.quire/shared/agent-policy.toml` | TOML, format 1 | [policy.md](policy.md) | WS-A | read; written by Settings → Agents |
| `.quire/agent/rate/<hash>.txt` | Unix ms per line (a counter) | [policy.md](policy.md#limits) | WS-A | written |
| `<config>/vaults/<key>/state.toml` (+ approved copies) | TOML, format 1 | [agent-state.md](agent-state.md) | WS-A | written |
| `.quire/shared/vault.toml` | TOML, format 1 | [vault-prefs.md](vault-prefs.md) | F0 (each section: its feature) | read and written |
| `.quire/shared/srs/<device>.jsonl` | JSONL, v 1 | [srs.md](srs.md) | WS-E | written and replayed |
| task fields in notes (`📅`, `[due:: …]`, `🔁`) | Markdown | [tasks.md](tasks.md) | WS-E | read and written |
| `Templates/*.md` (`{{…}}`, `quire-template:`) | Markdown | [templates.md](templates.md) | WS-E | read |
| flashcards in notes (`::`, `?`, `==cloze==`) | Markdown | [srs.md](srs.md#cards-in-notes) | WS-E | read; `^k-` ids written on request |
| `*.canvas` | JSON Canvas 1.0, one object per line | [canvas.md](canvas.md) | WS-G | read and written |
| `attachments/<canvas>-ink-<n>.svg` | SVG with `data-quire-*` attributes | [canvas.md](canvas.md) | WS-G | read and written |
| `<pdf> (annotations).md` | Markdown + `quire:pdf-anchor` | [pdf-annotations.md](pdf-annotations.md) | WS-G | read and written |
| `.quire/cache/pdf-text/<sha256>.json` | JSON, format 1 (cache) | [pdf-annotations.md](pdf-annotations.md#pdf-text-cache-quirecachepdf-textsha256json) | WS-G | written |
| `[@key]` in notes, library `.bib` files | Pandoc citations, BibTeX | [citations.md](citations.md) | WS-G | read; `.bib` written by Import |
| citation footnotes in notes (`[^n]: [[Note]] — “…” <!-- quire:cite … -->`) | Markdown + `quire:cite` | [span-citations.md](span-citations.md) | WS-C | read and written |
| `.quire/cache/claims/<extractor>/<h2>/<hash>.json`, `claims/seen.jsonl` | JSON format 1, JSONL v 1 | [claims.md](claims.md) | WS-C | written (device-local cache) |
| `.quire/cache/contradict/<judge>.jsonl` | JSONL, v 1 | [claims.md](claims.md) | WS-C | written (device-local cache) |
| `.quire/cache/stale.json`, `[staleness]` in `vault.toml` | JSON format 1, TOML | [claims.md](claims.md#staleness-in-quiresharedvaulttoml) | WS-C | written / read |
| `<config>/settings.toml` | TOML | [settings.md](settings.md) | F0 (each table: its feature) | read and written |
| `<config>/device.toml` | TOML, format 1 | [settings.md](settings.md#devicetoml) | F0 | written |
| `<config>/themes/*.toml`, `<config>/fonts/*` | TOML, format 1 | [themes.md](themes.md) | WS-F | read (and a template written on request) |
| `<config>/session.json` (`positions`) | JSON | [editor-state.md](editor-state.md#sessionjson) | WS-F | written |
| `.quire/shared/vault.toml` `[sort]`, `[order]`, `[icons]`, `[tags]`, `[git]` | TOML sections | [editor-state.md](editor-state.md#vaulttoml-sections) | WS-F | read and written |
| `<data_dir>/models/…` (`installed.toml`), the catalog, `cache/vectors-<model>.f16`, `cache/ocr/` | TOML format 1, binary, JSON format 1 | [model-catalog.md](model-catalog.md) | WS-D | written |
| engine stdio | JSON lines, protocol 1 | [engine-protocol.md](engine-protocol.md) | WS-D | in use |
| `.quire/agent/conversations/<id>.json` | JSON, format 1 | [conversations.md](conversations.md) | WS-D | written |
| `wiki/index.md` (between `quire:index` markers), `wiki/log.md` | Markdown | [wiki-index-log.md](wiki-index-log.md) | WS-D | written |
| `<config>/mcp/tokens.json`, `<config>/mcp/consent/<id>.json` | JSON, format 1 | [mcp.md](mcp.md) | WS-B | written |
| `<config>/vaults/<key>/mrtr.key` | 32 random bytes | [mcp.md](mcp.md#configvaultsvault-keymrtrkey) | WS-B | written |
| Quire Sync: `/v1/info`, `actor`, `space.json` `min_reader`, `config.json` `shared_state` | protocol 2 | [sync-protocol.md](sync-protocol.md) | WS-B | in use |
| `.quire/cache/**` | rebuildable; formats are private to their job | — | the job's owner | — |

Planned and not yet written by any code (formats come with their
workstream): `.quire/agent/runs/`,
`.quire/cache/served/`, `cache/claims/`, `cache/contradict/`,
`cache/pdf-text/`,
`<config>/mcp/tokens.json`, `<config>/themes/*.toml`. A workstream adds a
`docs/formats/<name>.md` for each file it introduces and a row above.
