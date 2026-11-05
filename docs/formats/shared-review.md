# Shared review history: `.quire/shared/agent/`

Review history that travels between devices (0.10). Written only while
"Also sync vault settings and review history" is on (`shared_state` in
[sync-protocol.md](sync-protocol.md)), before each sync pass; read after
it. Design: [../design/0.10-agentsync.md](../design/0.10-agentsync.md).
Owner: WS-A. Code: `quire_agent::shared`.

```text
.quire/shared/agent/.gitignore                         "*" (not for git)
.quire/shared/agent/devices/<device>.json             a device's id and name
.quire/shared/agent/proposals/<device>/<id>.json      proposals its agents made
.quire/shared/agent/decisions/<device>/<id>.json      what was decided on it, per proposal
.quire/shared/agent/ledger/<device>/<yyyy-mm>.jsonl   its ledger, without reads
.quire/shared/agent/texts/<device>/<yyyy-mm>.jsonl    texts its changes need to be undone
```

Every file has **one writer**: the device named in its path. `<device>` is
a device id (`[A-Za-z0-9_-]{1,64}`); anything else is ignored. A reader
skips a file whose `format` (or a line whose `v`) is higher than it knows.
No file grows past 4 MB (sync skips files over 10 MB, and would take one
that grew past that for deleted).

## `devices/<device>.json`

```json
{ "format": 1, "id": "d_7K2M9Q4W8XHZ", "name": "home server" }
```

The name shown in the Inbox ("Claude · on home server") and Activity.

## `proposals/<device>/<id>.json`

A proposal as on its device ([proposals.md](proposals.md)), without the
`review` caches, with `origin`:

```json
{ "format": 1, "id": "p_…", "status": "pending", "…": "…",
  "origin": { "id": "d_7K2M9Q4W8XHZ", "name": "home server" } }
```

Above 256 KiB it is stored packed (the whole JSON gzipped, then base64):

```json
{ "format": 1, "id": "p_…", "updated": 1793000000000, "status": "pending",
  "origin": { "id": "d_…" }, "packed": "H4sIAAAA…" }
```

A proposal that would still be over 4 MB isn't shared (it is reviewed on
its own device). Published while waiting and for 30 days after it is
resolved. Only the device in `origin.id` (the folder name) publishes a
proposal.

Another device copies a waiting proposal into its own store
(`.quire/agent/proposals/<id>.json`) as a **mirror**: the same file, with
`origin` and `origin_body` (the hash of the fields taken from the origin:
status, title, rationale, sources, trust and each change's op, paths and
texts). When the origin's copy changes, the mirror takes it and keeps
what was decided locally.

## `decisions/<device>/<id>.json`

Everything decided on `<device>` about proposal `<id>`:

```json
{ "format": 1, "proposal": "p_…", "device": "d_desk", "updated": 1793000100000,
  "hunks": [ { "path": "Launch.md", "hunk": "h_9a1c3e0b2f11", "action": "accepted", "at": 1793000100000 } ],
  "decided": [ { "path": "Launch.md", "action": "rejected", "old": ["bring snacks."],
                 "new": ["bring snacks and water."], "before": "we ship on monday.",
                 "at": 1793000100000, "device": "d_desk" } ],
  "changes": [ { "path": "Launch.md", "op": "edit", "state": "partially_applied", "at": 1793000100000 } ] }
```

* `hunks`: hunk decisions, as `decisions` in [proposals.md](proposals.md)
  (`action` `accepted`, `rejected` or `edited` with `text`).
* `decided`: what each decided hunk held (see `decided` in
  [proposals.md](proposals.md)).
* `changes`: a change's final state set on this device (`path` is its
  subject: the old path of a rename).
* Events are only added. The file goes 30 days after its last event once
  the proposal is no longer shared.

**Merging** (the same on every device): per hunk, a decision that wrote
text (`accepted`, `edited`) wins over `rejected`, then the earliest
(`at`, device); per change, the highest state (`applied` >
`partially_applied` > `rejected` > `superseded`), then the earliest. A
device that takes in a decision never writes the note: the note arrives by
sync from the device where the person decided.

## `ledger/<device>/<yyyy-mm>.jsonl`

The lines of `.quire/agent/ledger/<device>/<yyyy-mm>.jsonl`
([ledger.md](ledger.md)) except `read` events, as written there, for this
month and the two before. Above 4 MB the oldest lines are left out. Other
devices read it read-only (Activity, revert, who last changed a note).

## `texts/<device>/<yyyy-mm>.jsonl`

Texts another device may need to undo this device's recent changes (its
history may lack them: two agent writes between syncs, a pruned history):

```json
{"v":1,"hash":"sha256:…","gz":"H4sIAAAA…"}
```

The before and after texts of ledger entries (`write`, `create`, `delete`,
`revert`, `accept`) of the last 35 days, gzipped and base64-encoded, by
the month of the entry; each text at most 256 KiB, each file at most 4 MB.
This month's and the last month's files are kept. Revert looks here when
the note's history on this device has no version with that hash.
