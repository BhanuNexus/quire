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
  `expired`, `superseded`. A proposal is settled when every change has a
  `state`; it then moves to the archive.
* An edit stores its base inline as `base_text`; above 64 KiB it is
  `base_text_gz` (gzip, then standard base64) instead. `base` is the base's
  content hash.
* `decisions[].action`: `accepted`, `rejected`, `edited` (with `text`).
  Hunk ids are `h_` + 12 hex of SHA-256 over path, old lines and new lines
  (separated by NUL; the *n*th repeat of an identical hunk in one file mixes
  in `n`), so decisions survive rebases.
* `changes[].state` (absent while the change waits): `applied`,
  `partially_applied`, `rejected`, `superseded` (the note was deleted).
* `changes[].review`: the hunks still waiting after the last decision, for
  the text that decision produced:
  `{"current": "sha256:…", "hunks": [{"id", "start", "old": [lines], "new": [lines], "base"?: [lines], "conflict"?: true}]}`.
  Lines keep their line endings. A reader uses them while the note is
  exactly `current` (so ids don't move when a line diff aligns repeated
  lines differently), and otherwise rebases from `base_text`.
* `decisions[].device` (0.10): the device the decision was made on;
  absent in older files (the device holding the proposal).
* `changes[].decided` (0.10): what each decided hunk held, so the same
  suggestion isn't asked about again when its id changes (the agent
  reworded it a little, or rewrote the lines around it):
  `[{"action", "old": [lines], "new": [lines], "before"?, "after"?, "at", "device"?}]`.
  Lines are normalised (NFC, whitespace collapsed, trimmed, lower case;
  blank lines left out); `before`/`after` are the nearest non-empty lines
  around the hunk. See "Decided content" below.
* `changes[].state_device`, `state_at` (0.10): where and when `state` was set.
* `origin`, `origin_body` (0.10): a **mirror** of a proposal made on
  another device, copied in by shared review history
  ([shared-review.md](shared-review.md)). `origin` is `{"id", "name"}` of
  that device. A mirror is reviewed like any proposal but never amended
  here; its agent amends it on its own device.
* `trust`: `untrusted` when a source is untrusted content (`raw/`, imported,
  `trust: untrusted`, or a URL); `untrusted_source` names the first one.
  Absent means trusted.

## Hunks and decisions

`AgentStore::review(id, path, current)` rebases the change on `current` (the
open note's buffer, or the file): a three-way merge of `base_text`,
`current` and `new_text`. Lines only the agent changed are hunks; lines both
changed differently are **conflict** hunks; lines only the person changed
are left alone.

`decide` applies the decisions to `current` and returns the text `R` to put
in its place (one undoable edit in an open note; the file otherwise, with
history before and after). The change then keeps `base_text :=` `R` with
the agent's original lines where conflict hunks still wait, and `new_text
:=` `R` plus the waiting hunks, so a waiting conflict is still a conflict
next time. When no hunk waits, the change gets its `state`.

Every decision rereads the file and refuses to save over one whose
`updated` changed since the caller read it (another process decided); the
caller reviews again.

## Decided content

A hunk is hidden (in review, after a rebase, and dropped when the agent
amends) when its id was decided, or when it only brings back a decided
change at the same place: every line it adds matches a line that change
added, every line it removes matches a line that change would have
replaced, at similarity ≥ 0.85 (normalised Levenshtein; lines under 20
characters must be equal apart from closing punctuation), and one of its
replaced lines or its context matches that change's. Rejections always
count; acceptances only when made on another device (until the note with
that text arrives). A hunk that adds or removes anything else is shown.
Hidden hunks keep the current lines. When the agent amends, a rejected
line inside a bigger hunk is taken out of its text (back to the line it
replaced), and the rest stays. A new proposal by the same agent for the
same note starts with the rejected content of its proposals resolved in the
last 30 days; a change left with nothing is settled as `rejected` at once.

## Writing and amending

`AgentStore::submit` writes one proposal per change set that isn't applied
directly; property changes are stored as edits with the resulting text. A
later write by the same actor to a path it already has waiting **amends**
that proposal (`amended: true`) instead of adding one:

* written on top of its own pending text (its `expected` version is the
  pending text's hash): the new text replaces it;
* written on top of the file: both its changes are kept when they don't
  overlap (three-way merge), else the newer text wins;
* a property change applies to the pending text;
* an edit of a note it proposed to create changes the proposed text.

A change set touching only paths without a waiting proposal of that actor
gets a new proposal. An agent with `max_pending_per_client` proposals
waiting gets `inbox_full`.

## Lifecycle

* Waiting 30 days untouched: `expired`, archived, and restorable from the
  Inbox's "Resolved" list.
* Archived proposals are deleted after 90 days. The archive is
  `archive/<yyyy-mm>/<id>.json` by the month of `updated`.
* A rename made in Quire (or through `submit`) rewrites paths in waiting
  proposals.
* A deleted note makes its waiting edits, renames and deletes `superseded`.
* A `create` into a path that now exists is reviewed as an edit of that note.
* Undoing an accepted hunk in the editor puts the proposal back as it was,
  if nothing else changed it since.
* A file whose `format` is above 1 is not listed and never rewritten.
* With shared review history on, a waiting proposal and its decisions are
  also published under `.quire/shared/agent/` for the other devices
  ([shared-review.md](shared-review.md)).
