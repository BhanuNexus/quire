# Proposals: `.quire/agent/proposals/<id>.json`

An agent's suggested changes, one file each, written atomically. Resolved
proposals move to `archive/<yyyy-mm>/<id>.json` (by `updated`). Owner: WS-A.
Types: `quire_agent::proposal`.

```json
{ "format": 1, "id": "p_01JB9Q7H2K3M4X7Q2W9Z", "created": 1793000000000, "updated": 1793000000000,
  "status": "pending",
  "actor": { "kind": "agent", "id": "mcp:claude-code", "label": "Claude Code", "via": "mcp-stdio", "device": "d_…" },
  "run": "r_…", "title": "Add undo details to Rope Notes",
  "rationale": "…the agent's words, shown as plain text…",
  "sources": [ { "uri": "quire://note/raw/paper.md" } ],
  "changes": [
    { "op": "edit", "path": "Rope Notes.md", "base": "sha256:…", "base_text": "…", "new_text": "…",
      "decisions": [ { "hunk": "h_9a1c3e0b2f11", "action": "rejected", "at": 1793000100000 } ] },
    { "op": "create", "path": "wiki/Ropes.md", "text": "…" },
    { "op": "rename", "from": "A.md", "to": "B.md", "update_links": true },
    { "op": "delete", "path": "Old.md" } ] }
```

* `status`: `pending`, `applied`, `partially_applied`, `rejected`,
  `expired`, `superseded`.
* An edit stores its base inline as `base_text`; above 64 KiB it is
  `base_text_gz` (gzip, then standard base64) instead. `base` is the base's
  content hash.
* `decisions[].action`: `accepted`, `rejected`, `edited` (with `text`).
  Hunk ids are `h_` + 12 hex of SHA-256 over path, old lines and new lines,
  so decisions survive rebases.
* `AgentStore::submit` writes one proposal per change set that isn't applied
  directly; property changes are stored as edits with the resulting text.
* A file whose `format` is above 1 is not listed and never rewritten.
