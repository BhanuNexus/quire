### Added
- AI apps get a much richer set of tools over MCP: search by section with line ranges, view a note or just a section or line range, outlines, linked notes, recent changes, version history and diffs, tags, properties, views, tasks, and their own suggestions. Long results are cut to a size the app asks for and continue with a cursor.
- AI apps can edit precisely: replace a passage, insert lines, edit a section, append, set properties, rename, delete. An edit names the version it was based on; if the note changed meanwhile, Quire re-applies it when it still fits or returns what changed, so an agent never overwrites your edits unknowingly.
- MCP over HTTP (Settings → Connections, or `quire-cli mcp serve-http`): off by default, on 127.0.0.1 only, and every request needs a token. Apps sign in with OAuth and you approve each one in Quire (or with `quire-cli mcp approve`); you can also create personal tokens and revoke any app.
- Settings → Connections shows ready-to-copy setup for Claude Code, Claude Desktop and other MCP apps.
- Capture web page: `quire-cli capture <vault> --url URL`, the command "Capture web page…", and the MCP tool `capture_url` save a page's article and images as a note in `raw/`, marked as untrusted source material. Private and local network addresses are refused.
- Vault instructions: `AGENTS.md` (or `CLAUDE.md`) is given to AI apps once you've approved it on this computer; `quire-cli agents-md init` writes a starter. `quire-cli skill print|install` gives agents a guide to working in Quire.
- AI apps can save an answer as a note with citations Quire can check later (`save_answer`), suggest undoing an agent's changes (`revert`), and group a task's changes (`begin_run`).
- Optional approval in the AI app: a vault can let a specific app ask you to apply its change right there instead of sending it to Quire's review.
- AI apps can follow changes (`subscriptions/listen`) and read notes as resources (`quire://note/…`).
- `quire-cli agent accept|reject|revert` decide on suggestions and undo agent changes from the command line.
- Quire Sync: devices now show which AI agent made a change synced from another device.
- Quire Sync Server: `GET /v1/info` reports the protocol versions it speaks.

### Changed
- `quire-cli mcp --allow-write` now writes according to the vault's agent policy: directly in agent folders (`wiki/`), new files only in `raw/`, and everywhere else as suggestions you review in Quire's Inbox. In 0.8 it wrote directly to any note. A notice says so when the server starts.
- MCP apps that use protocol 2025-06-18 or later also see titles, annotations and structured results for every tool; the 0.8 tools are unchanged.
- MCP apps can no longer see notes in folders the agent policy marks private.
