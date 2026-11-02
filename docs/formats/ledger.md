# Ledger: `.quire/agent/ledger/<device-id>/<yyyy-mm>.jsonl`

One line per agent event on this device, appended under a file lock (UTC
month of `at`). Proposals and the ledger are device-local (design §8.3).
Owner: WS-A. Type: `quire_agent::LedgerEntry`.

```json
{"v":1,"at":1793000000000,"actor":{"kind":"agent","id":"mcp:claude-code","label":"Claude Code","via":"mcp-stdio","device":"d_…"},"op":"write","path":"wiki/Ropes.md","before":"sha256:…","after":"sha256:…","snap_before":1792999999999,"snap_after":1793000000000,"run":"r_…","bytes":412}
```

| Field | Meaning |
|---|---|
| `v` | 1 |
| `at` | Unix ms |
| `actor` | as in [provenance.md](provenance.md) |
| `op` | `write`, `create`, `rename`, `delete`, `propose`, `accept`, `reject`, `revert`, `read`, `batch` |
| `path`, `to` | the note; `to` is a rename's new path |
| `before`, `after` | content hashes |
| `snap_before`, `snap_after` | history snapshot times (`<ms>` of `.md.gz`) holding those texts |
| `proposal`, `run` | ids |
| `bytes` | size written |
| `batch` | shared by every entry of one batch; undone together |
| `from` | on `accept`, `reject` (and a rename or delete accepted from a proposal): the agent whose suggestion it was |

Optional fields are left out. Readers skip unparsable lines and lines with
`v` above 1.

* `read` events record the path only, and only when the device logs reads
  (Settings → Agents; `log_reads` in [agent-state.md](agent-state.md)).
* A rename with `update_links` logs a `write` for every note whose links it
  rewrote, in the rename's batch.
* A revert (`job:revert`) logs `revert` for restored notes, `delete` for
  trashed ones, `rename` and `create` for the rest, all with one `batch`,
  then a `batch` line; selecting that batch undoes the revert.
* Undoing an accepted hunk in the editor logs `revert` with the proposal and
  no path.
* `quire_agent::AgentStore::last_actor` reads the last 90 days of write
  events (`write`, `create`, `delete`, `revert`, `accept`; renames move the
  entry) to say who last changed a note.
