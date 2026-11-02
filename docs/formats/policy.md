# Agent policy: `.quire/shared/agent-policy.toml`

What agents may do where. It travels with the vault, so a change made
elsewhere only takes effect on a device after the person approves it there
(design §A.2, P7). Owner: WS-A. Types: `quire_agent::policy`.

```toml
format = 1
default = "propose"                     # deny | read | propose | create-only | direct
[[zone]]
path = "wiki/"                          # prefix, or glob with * and **
role = "agent"                          # agent | raw | human | private
mode = "direct"
[[zone]]
path = "raw/"
role = "raw"
mode = "create-only"
[limits]
writes_per_minute = 30
max_write_bytes = 262144
max_files_per_change = 50
max_pending_per_client = 200
[clients."mcp:claude-code"]
default = "propose"
zones = { "wiki/" = "direct" }
approve_in_client = false
[[contract]]
name = "concept pages"
required = ["type", "sources"]
```

Without the file, the defaults are the zones above. Settings → Agents
edits the file in place (`toml_edit`: comments, order and keys Quire doesn't
know survive) and approves the result on that device.

## How a mode is worked out

`AgentStore::effective_mode(actor, path, op)`:

1. **The path** must be canonical: NFC, relative, no `.`/`..`/empty
   segment, not in a hidden folder (except `.quire`, which rule 3 denies), no
   control characters, not reached through a link out of the vault, and a
   new path may not differ only in case from an existing file
   (`invalid_path`).
2. **People** are never limited. Quire's own jobs and importers write
   directly (subject to rule 4).
3. For agents, every **active policy** (below) gives a mode, and the most
   restrictive applies:
   * the zone that matches most specifically (longest literal prefix; at
     equal length a literal beats a glob; matching ignores case; `*` and `?`
     within a segment, `**` across segments; a pattern ending in `/` covers
     everything below);
   * the client's entry in `[clients]`, by the Quire-assigned key: its own
     `zones` win over the vault's zone when at least as specific; otherwise
     the vault's zone applies, capped by the entry's `default`. An agent
     that only reported its name (`unverified`, no `--client` key) can be
     narrowed by its entry but never widened.
4. **Hard rules**: `.quire/**` is `deny`; root `AGENTS.md` and `CLAUDE.md`,
   any `SKILL.md` and the templates folder are at most `propose`.
5. **The grant**: stdio without `--allow-write` is `read`; HTTP tokens are
   capped by their scopes.

`create-only` lets agents add files; editing, renaming or deleting one is
refused with `raw_is_immutable`. A rename needs both leaving the old place
and creating at the new one; with `update_links`, a rename that would
rewrite links in a note the agent may not edit directly becomes a
suggestion (accepting it rewrites them).

## Approval on each device

The file travels with the vault, so a change made elsewhere only takes
effect on a device after the person approves it there (P7). The device's
approvals are in `<config>/vaults/<key>/state.toml`
([agent-state.md](agent-state.md)).

* The file's hash is approved here: it is the **only** active policy.
* Otherwise both the file and the **baseline** are active (so the most
  restrictive answer wins): the baseline is the last approved file, or the
  defaults. Narrowing therefore takes effect at once; widening waits.
* A file that can't be read leaves only the baseline active.
* With no file, the defaults apply (unless a file was approved before: then
  the defaults and that file are both active until approved).

The Inbox and Settings → Agents list how the file widens the baseline: for
every zone, client and operation where it gives more, and for raised limits
and approval in the agent's app.

## Limits

Per client key, from every active policy (the stricter value):

| Key | Error | Notes |
|---|---|---|
| `writes_per_minute` | `rate_limited` (`retry_after_ms`) | counted across processes in `.quire/agent/rate/<hash>.txt` |
| `max_write_bytes` | `too_large` | per resulting file |
| `max_files_per_change` | `too_large` | per change set |
| `max_pending_per_client` | `inbox_full` | new proposals only; amending is fine |

## Write contracts

```toml
[[contract]]
name = "concept pages"
applies = { folder = "wiki/concepts/" }       # or { type = "concept" } (front-matter `type`); both = both must hold
required = ["type", "sources"]
fields.type = { allowed = ["concept", "entity"] }
fields.sources = { kind = "links", min = 1, must_resolve = true }
fields.updated = { kind = "date" }
body = { max_bytes = 20000, must_link_to_source = true }
```

* `kind`: `text`, `date` (YYYY-MM-DD), `number` (`min`/`max` bound the
  value), `bool`, `list` (`min`/`max` items), `links` (`[[Note]]` values;
  `min`/`max` links; `must_resolve`: they exist), `enum` (or just
  `allowed`).
* `body.must_link_to_source`: the text links to at least one note in the
  `sources` property.
* A zone's `index.md` and `log.md` are never covered.
* `submit` checks the resulting text of every create, edit, property change
  and rename by an agent and refuses the whole change set with
  `contract_violation`: `{"contract": name, "violations": [{"field", "rule",
  "message"}]}`. Lint reports the person's notes that miss a `required`
  field (`missing-property`).
* A contract that can't be read is skipped and named in Settings → Agents.
