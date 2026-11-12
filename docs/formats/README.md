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
| `.quire/shared/agent/{devices,proposals,decisions,ledger,texts}/…` | JSON format 1, JSONL v 1 | [shared-review.md](shared-review.md) | WS-A | written and read with shared review history on (0.10) |
| `.quire/agent/inbox/<fingerprint>.json`, `dismissed.jsonl` | JSON format 1, JSONL v 1 | [inbox.md](inbox.md) | WS-A | written |
| `.quire/shared/agent-policy.toml` | TOML, format 1 | [policy.md](policy.md) | WS-A | read; written by Settings → Agents |
| `.quire/agent/rate/<hash>.txt` | Unix ms per line (a counter) | [policy.md](policy.md#limits) | WS-A | written |
| `<config>/vaults/<key>/state.toml` (+ approved copies) | TOML, format 1 | [agent-state.md](agent-state.md) | WS-A | written |
| `.quire/shared/vault.toml` | TOML, format 1 | [vault-prefs.md](vault-prefs.md) | F0 (each section: its feature) | read and written |
| `[lint.format]` in vault.toml (the Markdown linter's rules) | TOML | [lint-format.md](lint-format.md) | 0.12 switching tools | read; written by Settings → Linter |
| `[home]` in vault.toml (the Home page) and `<config>/vaults/<key>/home-stats.json` | TOML, JSON | [home.md](home.md) | 0.12 switching tools | read and written |
| `.quire/imports/<id>.json` | JSON, format 1 | [import-record.md](import-record.md) | 0.12 switching tools | written by every import; read by undo |
| `.quire/shared/srs/<device>.jsonl` | JSONL, v 1 | [srs.md](srs.md) | WS-E | written and replayed |
| task fields in notes (`📅`, `[due:: …]`, `🔁`) | Markdown | [tasks.md](tasks.md) | WS-E | read and written |
| task notes (`type: task`, `time_entries`, TaskNotes' names) | Markdown front matter | [tasks.md](tasks.md#task-notes) | 0.11 tasks | read and written |
| `<config>/task-timer.json` | JSON, format 1 | [tasks.md](tasks.md#the-timer) | 0.11 tasks | written and read (app and CLI) |
| board order, swimlanes, colours (`order.<column>`, `cardOrder`) | quire-view lines, `.base` view keys | [boards.md](boards.md) | 0.11 tasks | read and written |
| typed links in notes (`parent:: [[X]]`, `[supports:: [[Y]]]`, `up: "[[X]]"`) and `[links]` in vault.toml | Markdown, TOML | [links.md](links.md) | 0.12 typed links | read; written by "Add typed link…" and `add_link` |
| `Templates/*.md` (`{{…}}`, `quire-template:`) | Markdown | [templates.md](templates.md) | WS-E | read |
| flashcards in notes (`::`, `?`, `==cloze==`) | Markdown | [srs.md](srs.md#cards-in-notes) | WS-E | read; `^k-` ids written on request |
| `*.canvas` | JSON Canvas 1.0, one object per line | [canvas.md](canvas.md) | WS-G | read and written |
| `attachments/<canvas>-ink-<n>.svg` | SVG with `data-quire-*` attributes | [canvas.md](canvas.md) | WS-G | read and written |
| `*.excalidraw`, `*.excalidraw.md` | Excalidraw JSON; the Obsidian Excalidraw plugin's note (plain or LZ-string compressed) | [excalidraw.md](excalidraw.md) | 0.13 Excalidraw | read and written (0.13; read-only before) |
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
| `<folder>/quire-lock.json`, `*.qenc`, `.quire/history/<fnv>/<ms>.md.gz.qenc` | JSON format 1; binary, version 1 | [locked-folders.md](locked-folders.md) | 0.11 security | written (locked folders) |
| `.quire/shared/locked-folders.json`, `<config>/vaults/<key>/locked-folders.json` | JSON, format 1 | [locked-folders.md](locked-folders.md#remembered-locked-folders) | 0.11 security | written and read |
| `.quire/lock/<folder id>.json` (while locking), `.quire/history/paths.jsonl` | JSON format 1, JSONL | [locked-folders.md](locked-folders.md#what-a-conversion-keeps-on-this-device) | 0.11 security | written |
| `agent-policy.toml`: `ai`, `[clients]` `revoked`, `log_reads`, `allow_tags`, `allow_folders` | TOML, format 1 | [policy.md](policy.md#connections-allow-lists-and-ai-off) | 0.11 security | read; written by Settings → Agents |
| `<config>/settings.toml` `[security]` | TOML | [settings.md](settings.md) | 0.11 security | read and written |
| `SKILL.source.json` beside an installed `SKILL.md` | JSON, format 1 | [agents.md](../agents.md#skills) | 0.11 security | written by `quire-cli skill install --from` |

Planned and not yet written by any code (formats come with their
workstream): `.quire/agent/runs/`,
`.quire/cache/served/`, `cache/claims/`, `cache/contradict/`,
`cache/pdf-text/`,
`<config>/mcp/tokens.json`, `<config>/themes/*.toml`. A workstream adds a
`docs/formats/<name>.md` for each file it introduces and a row above.
