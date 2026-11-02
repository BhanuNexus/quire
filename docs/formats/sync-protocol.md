# Quire Sync: protocol versions and format fields

What changed in Quire Sync for 0.9 (design 0.9 §B.5). Owner: WS-B. The
rest of the sync design is in `docs/design/sync.md`.

## Protocol versions

| Version | Quire | What it adds |
|---|---|---|
| 1 | 0.5–0.8 | revisions, spaces, long polls |
| 2 | 0.9 | `GET /v1/info`, the `Quire-Sync-Protocol` header, per-space minimums, `actor` in revisions |

* `GET /v1/info` (no token) answers `{"protocol": {"min": 1, "max": 2}, "server": "0.9.0"}`.
  A `404` means a 0.8 server: protocol 1.
* Clients send `Quire-Sync-Protocol: 2` on every request; a request without
  it is protocol 1.
* `PUT /v1/spaces/{id}/min_protocol` with `{"min": 2}` (token needed) raises
  the space's minimum; it is never lowered. A client below the minimum gets
  `426` with `{"error": "…", "min": 2}`. Servers keep it in the
  `space_settings` table.

## `actor` in a revision's content

Inside the encrypted content of a revision (so the server never sees it),
next to `path`, `text`, `from` and `at`:

```json
{ "path": "wiki/Ropes.md", "deleted": false, "text": "…", "from": "Asha's laptop", "at": 1793577600000,
  "actor": { "id": "mcp:claude-code", "label": "Claude Code", "kind": "agent" } }
```

Present only when the version being sent is exactly what an agent, job or
importer wrote (its history snapshot's provenance); absent for the person's
own edits. A receiving 0.9 device records it as the provenance of the
incoming version (`via: "sync"`); 0.8 devices ignore the field.

## `space.json` in a sync folder

```json
{ "format": 1, "space": "…", "created": 1793577600, "wrapped_key": { … } }
```

0.9 adds an optional `min_reader`. 0.8 refuses any `format` above 1. From
0.9 on, a reader refuses a space only when `min_reader` is above the format
it reads (2), and otherwise ignores a higher `format`. 0.9 keeps writing
`format: 1` without `min_reader`; it writes `"format": 2, "min_reader": 2`
only when the vault starts sharing settings (below). Unknown fields are kept.

Note: a 0.8 device that already syncs a folder doesn't re-read `space.json`,
so raising the minimum stops 0.8 devices from joining, not ones already set
up. The setting asks the person to update every device first.

## `.quire/sync/config.json` (vault, device-local)

Adds `"shared_state": true` when "Also sync vault settings and review
history" is on: `.quire/shared/**` (files up to 10 MB) then travels with
Quire Sync. Absent means off; 0.8 ignores the field. A 0.9 device that
receives a `.quire/shared/` file from another device turns it on too, so it
never mistakes those files for deleted ones.
