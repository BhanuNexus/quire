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
it reads (2), and otherwise ignores a higher `format`. 0.9 writes `format: 1`
without `min_reader`, including when the vault shares settings (below).
Unknown fields are kept.

## The settings lane in a sync folder

```text
<space>/devices/<device>/000001.jsonl          notes and attachments (0.8 and 0.9)
<space>/shared/devices/<device>/000001.jsonl   .quire/shared/** (0.9 only)
```

With "Also sync vault settings and review history" on, `.quire/shared/**`
(files up to 10 MB; from 0.10 that includes the review history under
`.quire/shared/agent/`, [shared-review.md](shared-review.md)) travels in a
lane of its own: the same log layout and
rules, under `<space>/shared/`, with its own revisions and engine state
(`.quire/sync/state-shared.json`, `.quire/sync/lane-shared/folder-seq`).
Quire 0.8 reads only `<space>/devices/`, so it never sees those files, even
on a device that joined before. (A 0.8 device that received them would write
them, miss them on its next pass because it doesn't list hidden files, and
send their deletion to every device.) The notes lane never lists, writes or
deletes anything under `.quire/shared/`. 0.8 devices keep syncing notes.

## `.quire/sync/config.json` (vault, device-local)

Adds `"shared_state": true` when "Also sync vault settings and review
history" is on. Absent means off; 0.8 ignores the field.

* Through a server, `.quire/shared/**` travels with the notes, and turning
  it on first raises the space's minimum protocol to 2, so 0.8 clients get
  `426`. A 0.9 device that receives a `.quire/shared/` file from another
  device turns it on too, so it never mistakes those files for deleted ones.
* In a sync folder, turning it on starts the lane (`<space>/shared/devices/`).
  A 0.9 device that never ran the lane joins it when it finds it.
* `quire-cli sync share <vault> on|off` (0.10) does the same from a server.
* Review history (0.10) under `.quire/shared/agent/` is written and read by
  0.10 devices only; a 0.9 device carries those files like any other
  settings file and reads none of them. A file there that another device
  removed is deleted, not moved to `.trash/`.
