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
   segment, not in a hidden folder (except `.quire`, which rule 4 denies), no
   control characters, and a new path may not differ only in case from an
   existing file (`invalid_path`). Links are followed (`AgentStore::resolve`):
   the file they reach must be inside the vault and outside hidden folders,
   and a link that leads nowhere is refused. Rules 3 to 5 apply to the path
   as named **and** to the file it really reaches; the stricter wins, so a
   link into a `deny` zone is that zone. `AgentStore::guard` is this check
   for everything agents name (tools, resources, subscriptions, ACP file
   requests, captured images).
2. **People** are never limited. Quire's own jobs and importers write
   directly (subject to rule 4).
3. For agents, every **active policy** (below) gives a mode, and the most
   restrictive applies:
   * the zone that matches most specifically (longest literal prefix; at
     equal length a literal beats a glob; matching ignores case; `*` and `?`
     within a segment, `**` across segments; a pattern ending in `/` covers
     everything below; patterns and paths are compared in NFC, so `Privé/`
     covers both Unicode spellings);
   * the client's entry in `[clients]`, by the Quire-assigned key: its own
     `zones` win over the vault's zone when at least as specific; otherwise
     the vault's zone applies, capped by the entry's `default`. An agent
     that only reported its name (`unverified`, no `--client` key; its id
     ends in `?`, which is ignored when looking up its entry) can be
     narrowed by its entry but never widened.
4. **Hard rules**: `.quire/**` is `deny`; root `AGENTS.md` and `CLAUDE.md`,
   any `SKILL.md` and the templates folder are at most `propose`. Since
   0.11, every path in a locked folder ([locked-folders.md](locked-folders.md)),
   every `*.qenc` file and every `quire-lock*.json` key file is `deny`, for
   agents and Quire's own jobs alike, whether the folder is locked or not.
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
every zone, client and operation where it gives more, for raised limits and
approval in the agent's app, and for a zone that loses the `raw` or
`private` role (roles decide which notes reach agents as untrusted data).
Until then, a note counts as untrusted when any active policy, or the
defaults, put it in a `raw` zone.

Approving names the file that was shown (its hash): if the file changed in
between (a sync landed), the approval is refused and the new file is shown.
`AGENTS.md` and `CLAUDE.md` are approved the same way.

## Connections: allow lists and AI off

0.11 (Settings → Agents → Connections):

```toml
ai = "off"                              # no agent may use the vault; providers aren't used either

[clients."mcp:claude-code"]
revoked = true                          # denied every path; `quire-cli mcp --client claude-code` refuses to start
log_reads = true                        # its reads go to the ledger even when the device doesn't log reads

[clients."mcp:cursor"]
allow_tags = ["work"]                   # only notes tagged #work (or #work/…) …
allow_folders = ["Projects/"]           # … or under Projects/
```

* `ai = "off"` (absent: on) makes every path `deny` for every agent, and
  every request to the MCP server (stdio or HTTP) is answered with an error
  saying AI is off. The app also stops using AI providers and on-device AI
  for the vault.
* `revoked`: every path is `deny` for the key; a request gets an error
  naming the revoke; `quire-cli mcp --client <key>` exits at start. An
  unverified client (`mcp:<name>?`) is matched by the name it reports, so
  starting without `--client` doesn't get around it (a client that reports
  another name does: for unverified stdio clients, revoke is advisory).
  Revoking a client from Settings writes `revoked` and `log_reads`
  together.
* `allow_tags` / `allow_folders`: when either is present, a note the client
  may see must carry one of the tags (front matter `tags` or inline, read
  from the file; `#work/q4` counts as `work`) or be under one of the
  folders; everything else is `deny` for that client. This is part of the
  same check as zones, so `list`, `search` (and its snippets), `neighbors`,
  backlinks, `query_view` (rows, groups and totals), resources, `ask`
  sources and the agent panel's context leave the rest out.
* All of these narrow, so they take effect on every device at once. Turning
  AI back on, removing `revoked`, and dropping or widening an allow list
  are widenings: other devices wait for approval.
* Older Quire versions don't know these keys and ignore them (they keep them
  in the file): a vault shared with Quire 0.10 devices isn't limited there.

## Limits

Per client key, from every active policy (the stricter value):

| Key | Error | Notes |
|---|---|---|
| `writes_per_minute` | `rate_limited` (`retry_after_ms`) | counted across processes in `.quire/agent/rate/<hash>.txt`; each captured image is a write |
| `max_write_bytes` | `too_large` | per resulting file, captured images included |
| `max_files_per_change` | `too_large` | per change set; a capture's images and note together |
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
