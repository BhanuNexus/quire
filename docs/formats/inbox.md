# Inbox: `.quire/agent/inbox/`

Findings waiting for the person, other than proposals (which come from the
proposal store). Owner: WS-A; producers are WS-A (lint, policy,
instructions), WS-C (contradictions, staleness, citations) and any job
(failures). Types: `quire_agent::inbox`.

## `<fingerprint>.json`

```json
{ "format": 1, "fingerprint": "3fa1c0d2b4e59a71", "created": 1793000000000, "updated": 1793000000000,
  "subject": "sha256:…",
  "item": { "kind": "lint", "check": "broken-link", "path": "A.md", "detail": "[[Missing]] doesn't resolve", "fix": null } }
```

* `fingerprint`: 16 hex of the content hash of the item's subject key
  (`lint:<check>:<path>`, `stale:<path>`, a contradiction's sorted pair of
  anchors…), so the same finding always has the same file.
* `subject`: content hash of the whole item. Upserting an unchanged item
  doesn't rewrite the file.
* `item.kind`: `lint`, `contradiction`, `stale`, `citation_drift`,
  `verify_failed`, `policy_changed`, `instructions_changed`, `job_failed`
  (and `proposal` in memory only).

## `dismissed.jsonl`

`{"v":1,"fingerprint":"3fa1c0d2b4e59a71","subject":"sha256:…","at":1793000000000}`

A dismissed finding stays hidden until its content (and so `subject`)
changes.

## What the Inbox shows

Waiting proposals (grouped by `run`, or one group per proposal), the stored
findings above, and two items worked out on the fly, never stored: a policy
change waiting for approval (`policy_changed`) and agent instructions
waiting for approval (`instructions_changed`, see
[agent-state.md](agent-state.md)). Snoozes are per device (`snoozed` in
`state.toml`); a proposal is snoozed under the fingerprint of
`proposal:<id>`.

Lint findings (`kind: lint`) are kept in step by the lint job: a run adds
what it finds and removes the lint files it no longer finds. Checks set to
`warn` in `vault.toml` `[lint]` are listed but not counted in the badge.
