# Provenance: `.quire/history/<fnv>/<ms>.prov.json`

Who made a version of a note. It sits next to the history snapshot with the
same `<ms>` and is written before the snapshot, so a snapshot is never
visible without it. Absent means the person typing, or a version from
before 0.9. Owner: WS-A. Type: `quire_agent::Provenance`.

```json
{
  "format": 1,
  "hash": "sha256:1a2b3c4d5e6f7a8b9c0d1e2f3a4b5c6d",
  "actor": { "kind": "agent", "id": "mcp:claude-code", "label": "Claude Code",
             "client": { "name": "claude-code", "version": "2.1", "self_reported": true },
             "via": "mcp-stdio", "device": "d_7K2M9Q4W8XHZ", "model": "…", "trace": "00-…" },
  "op": "write",
  "at_ms": 1793000000000,
  "run": "r_01JB9Q7H2K3M4X7Q2W9Z",
  "proposal": "p_01JB9Q7H2K3M4X7Q2W9Z",
  "accepted_from": { "kind": "agent", "id": "mcp:claude-code", "…": "…" },
  "sources": [ { "uri": "quire://note/raw/paper.md", "anchor": { "…": "an Anchor" } } ],
  "trust": "trusted"
}
```

* `hash` is the content hash of the snapshot's text.
* `actor.kind`: `human`, `agent`, `job`, `import`. `actor.id` is assigned by
  Quire (`human`, `mcp:<key>`, `oauth:<client>`, `panel:<provider>`,
  `acp:<name>`, `job:<name>`, `import:<source>`), never taken from what a
  client says about itself; that goes in `client` with `self_reported`.
  `actor.unverified: true` marks an id made from a client's own name
  because it connected without a Quire-assigned key (policy can narrow it,
  never widen it; see [policy.md](policy.md)).
* `via`: `mcp-stdio`, `mcp-http`, `panel`, `acp`, `cli`, `app`,
  `client-approval`, `sync`.
* `op`: `write`, `create`, `rename`, `delete`, `propose`, `accept`, `reject`,
  `revert`, `read`, `batch`.
* `accepted_from` is set when a person accepted an agent's suggestion; then
  `actor` is the person, `op` is `accept`, and `proposal`, `run` and
  `sources` come from the proposal. A revert by `job:revert` has `op:
  revert` and its `batch` id in `batch`.
* `trust` is `untrusted` for imports and for content derived from untrusted
  sources (design §A.3). Optional fields are left out when empty.

**History rules that depend on it** (`quire_vault::history`): a typing save
replaces the newest snapshot only when that snapshot was also a typing save
(no provenance) made less than two minutes earlier, so a human save right
after an agent's change never removes the agent's version. Pruning removes a
provenance file with its snapshot, and removes provenance files whose
snapshot is gone (0.8 prunes without knowing about them). Thinning (the
newest version per hour, day and week) never removes, for 90 days, a
snapshot with provenance or the snapshot just before it: undoing that change
needs both.
* The person's tag and property batches write their new versions with
  provenance too (`actor` the person, `op: write`), so they stay undoable.
