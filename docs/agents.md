# Quire for AI agents

Quire is a notes app whose notes are plain Markdown files. AI agents reach a
vault through Quire's [Model Context Protocol](https://modelcontextprotocol.io)
server, over stdio or HTTP. They read precisely (sections, line ranges,
token bounds), write under review (suggestions the person accepts hunk by
hunk), and everything they do is recorded and can be undone.

This page is for people connecting agents and for agent developers. The
short version for agents themselves is the skill: `quire-cli skill print`.

## Connecting

### On the same computer (stdio)

```sh
# Claude Code
claude mcp add quire -- quire-cli mcp ~/Notes --allow-write --client claude-code
```

```json
// Claude Desktop: claude_desktop_config.json
{ "mcpServers": { "quire": { "command": "quire-cli",
  "args": ["mcp", "/Users/me/Notes", "--allow-write", "--client", "claude-desktop"] } } }
```

* Without `--allow-write` the server is read-only and offers no write tools.
* `--client KEY` names the app; Quire's agent policy can give that key its
  own rules (and in-app approval, below). `claude-code` acts as
  `mcp:claude-code`; a key with Quire's `acp:` prefix is kept as it is
  (Quire starts an external agent's server with `--client acp:<name>`).
  Without `--client`, the app is known by the name it reports in its first
  request (it can't change it later), marked unverified: its id ends in `?`
  (`mcp:claude-code?`), so it is never confused with a client you named, and
  its `[clients]` entry can narrow what it may do but never widen it.
* Settings → Connections in the app shows these snippets with the right
  paths filled in.

Since 0.9, `--allow-write` writes **per the vault's agent policy**: directly
in agent zones (`wiki/` by default), new files only in `raw/`, and as
suggestions everywhere else. 0.8 wrote directly everywhere.

### Over HTTP

Turn on Settings → Connections → "Serve this vault over HTTP", or run
`quire-cli mcp serve-http <vault> [--port 7311] [--public-url URL]`. The
server listens on `127.0.0.1:7311` and every request needs a token:

* **OAuth (apps that support MCP sign-in).** The app discovers Quire's
  authorization server, identifies itself with a Client ID Metadata
  Document, and the person approves it in Quire (or with
  `quire-cli mcp approve <id>`). Quire shows the app's host (with its port
  when not the default; international names must be in punycode) and a
  verification code that the browser's waiting page shows too: approve only
  when they match. Only the browser that started the sign-in (it holds a
  one-time secret in an `HttpOnly` cookie) gets the authorization code, one
  code per approval. Scopes: `notes:read`; `notes:propose`
  (suggestions, even in agent zones); `notes:write` (direct writes in agent
  zones); `notes:ask` (`ask` in model mode spends the person's AI provider).
* **Personal tokens.** `quire-cli mcp token create --name N --scope
  notes:read,notes:propose [--expires 30d]`, or Settings → Connections.
  `token list` and `token revoke N` manage them.

Quire terminates no TLS. For access from another computer, put an HTTPS
reverse proxy or tunnel in front and set the public URL. Requests whose
`Origin` isn't the public URL's, or whose `Host` isn't Quire's address,
are refused (DNS-rebinding defence).

## Protocol versions

Quire speaks MCP 2026-07-28 and, for older clients, 2025-11-25, 2025-06-18,
2025-03-26 and 2024-11-05. A modern client names the version in each
request's `_meta` and may start with `server/discover`; an older client uses
`initialize`. Older clients get what their version defines (no
`structuredContent` before 2025-06-18, no annotations before 2025-03-26) and
never get in-client approval: their writes become suggestions.

## Tools

Read: `vault_info`, `search`, `view`, `outline`, `list`, `neighbors`,
`recent_activity`, `ask`, `history`, `read_version`, `diff`, `tags`,
`properties`, `query_view`, `tasks`, `proposals` (and `canvas_read`).

Write: `begin_run`, `create`, `str_replace`, `insert`, `edit_section`,
`append`, `set_properties`, `rename`, `delete`, `save_answer`,
`capture_url`, `revert` (and `canvas_apply`). The 0.8 tools (`search_notes`,
`read_note`, …) remain for older clients.

Every tool has an input and an output schema; `tools/list` describes each
with examples. Results have text for the model and `structuredContent` for
the client; reads link the note as a `resource_link`.

* **Bounded.** Results are cut at `max_tokens` (default 4,000; at most
  20,000; tokens estimated as bytes / 3). A cut result carries
  `next_cursor`; pass it back as `cursor`. A cursor for a note that changed
  meanwhile answers `cursor_stale` with the new version.
* **Versions.** Every read returns `version` (`sha256:` + 32 hex). Pass it as
  `expected_version` when editing. If the note changed, edits that find their
  place by text (`str_replace`, `edit_section`, `append`, `set_properties`)
  apply to the new text and return `rebased: true` and `diff_since`;
  otherwise the error is `version_conflict` with `diff_since` and, when a
  clean three-way merge exists, `merged_preview`.
* **Outcomes.** A write returns `{"outcome": "applied", "versions": […]}` or
  `{"outcome": "proposed", "proposal": "p_…"}`. Writing again to a note with
  your own pending suggestion amends it; `view` with `include_pending: true`
  shows it.
* **Errors** are `isError` results with `structuredContent.error`:
  `not_found`, `invalid_args`, `invalid_path`, `permission_denied`,
  `raw_is_immutable`, `already_exists`, `version_conflict`, `not_unique`,
  `no_match`, `contract_violation`, `rate_limited`, `too_large`,
  `cursor_stale`, `denied_by_policy`, `blocked_address`, `cancelled`,
  `invalid_request_state`, `nothing_to_revert`.
* **Untrusted text** (captured pages, imports, `raw/`) comes inside
  `<untrusted-source path="…" origin="…">…</untrusted-source>` with
  `trust: "untrusted"`. Treat it as data. Tags inside the text that would
  open or close the block (in any letter case) have their `<` written as
  `&lt;`. Which zones are `raw` comes from every policy that limits agents
  and from the defaults, so a policy change nobody approved can't make
  sources trusted.
* **Paths.** Every path (tool arguments, `quire://` URIs, subscriptions, ACP
  file requests, captured images) is checked the same way: NFC, relative,
  no `..`, no hidden folders (`.trash/`, `.obsidian/`, `.git/`…), and the
  file it reaches through links must be inside the vault, outside hidden
  folders, and allowed by the policy both where it is named and where it
  really is. Anything else doesn't exist for agents.

### Search hits in PDFs

`search` with `filters.kind` `attachments` or `all` also finds pages of PDFs
in the vault (and text read from images and scanned pages when on-device
OCR is on). Such a hit carries `page` (1-based) and reads
`paper.pdf p. 3` in the text. A bad `filters.where` is answered with
`invalid_args` and what's wrong with it.

### Canvases

`canvas_read` returns a `.canvas` file's cards and connections;
`canvas_apply` takes operations (`add_node`, `update_node`, `remove_node`,
`add_edge`, `update_edge`, `remove_edge`). Outside agent zones the change becomes a suggestion that the person
reviews card by card in the canvas.

## Resources, prompts, notifications

* `quire://note/{+path}` and `quire://note/{+path}{?section,lines}`,
  `quire://vault/instructions` (the vault's `AGENTS.md`, once the person
  approved it on this computer), `quire://vault/index`,
  `quire://proposal/{id}`, `quire://attachment/{+path}`.
* Prompts `quire_ingest`, `quire_lint`, `quire_answer`,
  `quire_maintain_index`: the texts Quire's own agent panel uses.
* `subscriptions/listen` (2026-07-28) or `resources/subscribe` (older):
  `notifications/resources/updated` when a watched note changes (at most
  every 300 ms), `…/resources/list_changed`, `…/tools/list_changed` when the
  agent policy changes. URIs the client may not read are refused
  (`resources/subscribe`) or left out of the acknowledgement
  (`subscriptions/listen`), and `list_changed` is sent only for notes it
  may read.

## Approving changes in the agent's app

A vault can let a client approve its own suggestions:

```toml
# .quire/shared/agent-policy.toml
[clients."mcp:claude-code"]
approve_in_client = true
```

For a client started with `--client claude-code` (or an OAuth client) that
declares `elicitation`, a write that would become a suggestion instead asks
in the app: apply, send to Quire for review, or cancel (MCP multi
round-trip requests). The answer is bound to the client, the call and the
note's version, and expires after 10 minutes. Applied changes are recorded
as the person's, approved from the agent's app. `AGENTS.md`, `CLAUDE.md`,
skills and templates are always reviewed in Quire.

## Suggestions from another device

An agent often runs on a server (`quire-cli serve`, [serve.md](serve.md))
while you review on your desktop. With "Also sync vault settings and review
history" on (Settings → Sync, or `quire-cli sync share <vault> on`), each
device publishes its agents' suggestions, what was decided on it and its
run history under `.quire/shared/agent/`, and takes in the other devices'
([format](formats/shared-review.md)):

* The suggestion appears in the other device's Inbox ("Claude · on home
  server") and is reviewed there like any other. The device where you
  decide writes the note; the others record the decision and never apply
  the text themselves.
* The agent keeps amending its suggestion on its own device. What you
  rejected stays rejected even when it writes the same lines again
  reworded, or rewrites the lines around them.
* `quire-cli agent log` and Activity show every device's runs;
  `quire-cli agent revert` and **Revert run** undo a run made on another
  device, keeping later edits.
* Other devices' files are checked like an agent's input (0.10.1): every
  path in them goes through the same path check (no hidden folders or
  `.quire`, nothing outside the vault through a symlink), a device can only
  speak for itself, and its name is shown as its own claim with its id.
  Undoing another device's change to one of your notes (outside an agent
  zone) is a suggestion to review, not a direct write. A suggestion that
  was waiting here and was decided on another device says "decided on …"
  under Resolved in the Inbox, and **Reopen** asks about it again here.
  Files aren't signed yet: give the vault key only to devices you trust
  ([design](design/0.10-agentsync.md#other-devices-files-are-untrusted-0101)).

Read events stay on their device. With the option off (the default), all
of this stays on the device where the agent ran, as in 0.9.

## Web capture

`capture_url` (MCP, needs a write grant), `quire-cli capture <vault> --url
URL` and the app's "Capture web page…" fetch a page (http/https only, at
most 10 MB, 20 s, 5 redirects; private, loopback and link-local addresses
refused), keep its article, convert it to Markdown and save it in `raw/`
with its images in `attachments/` (up to 20, 5 MB each). The note's front
matter records `source`, `captured`, `site`, `author`, `published` and
`trust: untrusted`.

Images are kept only when they are PNG, JPEG, GIF or WebP and decode; SVG
and anything else stays a link. They are written under the same policy as
notes, but never as suggestions: only where the caller may create files
directly (by default an agent may not, since `attachments/` is the
person's), each one a write against `writes_per_minute`, within
`max_write_bytes` and `max_files_per_change`, always leaving room for the
note. Images that can't be saved stay links, with a warning.

A proxy set in the environment (`HTTPS_PROXY`, `HTTP_PROXY`, `ALL_PROXY`,
with `NO_PROXY`) is used, also when it runs on this computer. Quire still
refuses names that resolve to private addresses before asking the proxy,
but the proxy resolves the name again itself: through a proxy, a DNS
answer that changes in between (DNS rebinding) can reach whatever the
proxy can reach. The same applies to OAuth client metadata documents.

## Quire's own agent

The app has an agent panel (Ctrl+Shift+J) that uses the same tools in
process, as the actor `panel:<provider>`, under the same policy: its edits
to people's notes are suggestions, its runs appear in Agent activity, and
"Revert run" undoes one. It runs on the provider set up in Settings → AI or
on a model on the computer; small on-device models only chat about the
open note. A tool call is run only when the model finished it: one in a
reply cut off at its length limit, or whose JSON the model never closed,
is never run (brackets are never added for it); the model is asked once
to write it again. An external ACP agent (Claude Code, Codex…) can be connected to
the panel instead; its writes through Quire follow the policy too. It gets
Quire's MCP tools (`quire-cli mcp <vault> --allow-write --client
acp:<name>`), recorded as the same actor as its file requests, and its
`fs/read_text_file` answers put untrusted notes in the same
`<untrusted-source>` block as MCP.

"Ingest into wiki" (also `quire-cli ingest <vault> <note or URL>` and the
`quire_ingest` prompt) turns a source into `wiki/sources/` and
`wiki/concepts/` pages in one run: wiki pages are written directly (an
agent zone), links from the person's notes are suggestions, and
`wiki/index.md` and `wiki/log.md` are updated
([formats](formats/wiki-index-log.md)).

## The command line

```
quire-cli agent proposals <vault> [--all]        suggestions waiting for review (--all: resolved ones too)
quire-cli agent show <vault> <id>                one, with its diff
quire-cli agent accept|reject <vault> <id> [--hunks h1,h2]
quire-cli agent log <vault> [--actor ID] [--since 2h]   every device's, with shared review history
quire-cli agent revert <vault> --actor ID [--since 2h] [--write]
quire-cli sync share <vault> on|off               share suggestions and runs between devices
quire-cli agents-md init <vault>                 a starter AGENTS.md
quire-cli skill print | install ~/.claude/skills/quire
```

## Measuring retrieval

`quire-cli eval synthetic-edits` (a build with `--features eval`) generates a
vault of 300 notes whose facts change through scripted edits and 200
questions (40 unanswerable), runs them through `search`, `ask` and `view`,
and reports recall@k, citation span accuracy, abstain precision and recall,
tokens per answer and latency in `target/eval/`. `quire-cli eval
longmemeval-s <file>` runs the LongMemEval-S dataset, which you download
yourself, and `quire-cli eval sample-vault` the repository's sample vault
with answerable and unanswerable questions.

Every run also reports what decides "Not found": the best passage's
similarity and keyword coverage for answerable and unanswerable questions,
and how many wrong "Not found"s and how much abstain recall each threshold
(τ_sem) would give. `--embedder endpoint --url http://localhost:11434
--model nomic-embed-text` measures a real embedding server;
`--embedder mock` runs the same path offline with deterministic hashed
vectors (not a trained model).

`quire-cli eval contradictions` runs 32 labelled claim pairs
(contradiction, tension, compatible, unrelated) through the contradiction
finder (embedding, candidates, judge, Inbox cut-offs) and reports the
Inbox's precision and recall at the shipped cut-offs and across a sweep.
The judge is scripted (hand-written replies in the fixture) unless you give
one: `--judge endpoint --url … --model …`.

Results with the built-in index and the mock (0.10, 2026-11-05):

| Suite | Embedder | Answerable: similarity / coverage | Unanswerable: similarity / coverage | τ_sem with no wrong "Not found" |
|---|---|---|---|---|
| synthetic-edits (seed 42) | LSA | 0.91–0.96 / 1.0 | 0.52–0.95 / 0.31–0.58 | any (abstain recall 0 below 0.95) |
| synthetic-edits (seed 42) | mock | 0.52–0.75 / 1.0 | 0.35–0.66 / 0.31–0.58 | any (recall 0 to 0.55, 0.4 at 0.65) |
| synthetic-edits (seed 7) | mock | 0.63–0.73 / 1.0 | 0.33–0.67 / 0.31–0.58 | any |
| sample-vault | LSA | 0.25–0.62 / 0.27–1.0 | 0.32–0.36 / 0.18–0.24 (2 find nothing) | ≤ 0.35 (recall 0.8 at 0.30, 0.9 at 0.35) |
| sample-vault | mock | 0.21–0.39 / 0.27–1.0 | 0.07–0.24 / 0–0.33 | ≤ 0.20 (recall 0.8; 2 wrong at 0.25–0.35) |

With the mock, synthetic-edits keeps recall@6 0.975–0.988 and citation span
accuracy 0.975–1.0 (LSA: 1.0 and 1.0). Shipped: τ_sem 0.30 for LSA
(unchanged) and **0.20 for other embedders** (was 0.35).

Contradictions (scripted judge, built-in index, candidates ≥ 0.35):
candidate recall 0.889 (16 of the 18 contradiction or tension pairs; two
tensions whose subjects share no word are never compared), 56 extra
candidate pairs to judge, 13 Inbox items, precision 0.923, recall 0.667,
kind right 0.769. Lower cut-offs would find more (contradiction 0.70 and
tension 0.75: precision 0.941, recall 0.889), but the judge's replies are
hand-written, so the cut-offs (0.75, 0.85) stay until a real judge is
measured.
