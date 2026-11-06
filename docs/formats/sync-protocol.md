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

### Selective sync (0.10)

`only` and `exclude` (folder prefixes ending in `/`) are written only when a
selection is set. Then the device id is stored as `partial_device` instead
of `device`: Quire 0.9 requires `device`, so it doesn't load the file and
shows sync as off for that vault, rather than sync it from an empty engine
state over the notes already there. 0.10 reads either key; clearing the
selection (`sync scope --all`) writes `device` again. The engine state
moves to `state-partial.json` while a selection is set (0.9 never reads it).

## `.quire/sync/lock` (vault, device-local, 0.10)

An empty file. The process that syncs the vault on this device holds an
exclusive OS file lock on it (`flock` on Unix, `LockFileEx` on Windows) for
as long as it runs: the app's worker, `quire-cli sync watch`, `serve --sync`,
or one `sync once`. `sync scope` takes it too, so a selection never changes
under a running pass. The OS releases it when the process ends, however it
ends; the file itself stays. A second app or `watch`/`serve` waits for the
lock and says so ("Quire is already syncing this vault (the app or
`quire-cli serve`)"); `once` and `scope` refuse. Quire 0.9 doesn't take the
lock, so don't run a 0.9 app on a vault that a 0.10 process syncs on the
same machine.

## Engine state additions (0.10, `state*.json`)

Device-local and ignored by 0.9 (serde defaults):

* `Tracked.absorbed`: the note was created at the same path with the same
  text on two devices (an agent's "create" accepted on both); the one with
  the larger file id is kept as deleted here and its delete is sent, so
  every device files the two as one note.
* `Tracked.kept {path, base}`: the note left this device's selection; its
  copy stays at `path`, last synced as `base`. When the selection grows
  again, changes made to the copy are merged with the note as it is now.
* `left {path: {file, base?}}`: paths where an old copy of a note that moved
  on or was deleted elsewhere may still be on this disk. When one comes into
  the selection, a file there that is exactly a version of that note goes to
  `.trash/`; a changed one is merged if the note comes back to that path in
  the same pass, otherwise it is sent as a note of its own.
* Notes the engine doesn't hold yet (new, or inside a selection that grew)
  are applied from their latest revision in the pulled batch only, never by
  replaying their history.

