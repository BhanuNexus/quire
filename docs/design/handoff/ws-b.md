# WS-B hand-off: requests to other workstreams and the integrator

What WS-B needs from files it doesn't own, what it touched outside its own
files (and why), and what to check at integration. Newest first.

## Touched outside WS-B's files (smallest possible additions)

| File | Change | Why |
|---|---|---|
| `crates/quire-sync/src/engine.rs` | `LocalStore` gains two default methods (`actor`, `incoming`); the five `Content { … }` literals set `actor`; `apply` calls `local.incoming(c)` | design §B.5: `actor` in `Content` needs every literal updated, and the engine is where revisions are built and applied. No other workstream touches `quire-sync`. |
| `apps/quire-sync-server/src/store.rs` | a `space_settings` table (`CREATE TABLE IF NOT EXISTS`, so existing databases gain it on open) and two methods | per-space `min_protocol` (§B.5) must persist |
| `crates/quire-import/src/lib.rs` | `pub mod web;` (one line, after `pub mod yaml;`) | the new module; WS-E adds `obsidian` here too (trivial merge) |
| `crates/quire-import/Cargo.toml` | `ureq`, `dom_smoothie`, `encoding_rs`, `sha2` (all `workspace = true`) | URL capture |
| `apps/quire-desktop/src/main.rs` | `mod mcp_ui;` | the new file (rustfmt keeps `mod`s sorted, so it sits between `layout_check` and `overlays`) |
| `apps/quire-desktop/src/registry.rs` | WS-B blocks: `FeatureUi::mcp`, Settings tab `mcp` ("Connections"), `ui_target` for `ConnectAgent` and `CaptureWebPage`, four `layout_samples` | registration points |
| `apps/quire-desktop/Cargo.toml` | `quire-mcp.workspace = true` under the WS-B marker | the HTTP server and dialogs |
| `crates/quire-mcp/tests/fixtures/mcp-0.8-write.jsonl` | the `instructions` string | deliberate change (§B.4): `--allow-write` now writes per policy; see `changelog.d/ws-b.md` |

`Cargo.lock` changed (dom_smoothie and its tree; chacha20poly1305 and
futures-core were already locked). At merge, take `main`'s lock and run
`cargo check`.

`quire-mcp/Cargo.toml` lists two crates that aren't in
`[workspace.dependencies]`, at the versions already in the lock:
`chacha20poly1305 = "0.11"` (as `quire-sync` does; MRTR request states) and
`futures-core = "0.3"` (the `Stream` trait for SSE; already pulled in by
axum). **Integrator:** move them to `[workspace.dependencies]` if you prefer.

`quire-mcp` now has `default = ["http"]`, so every crate that depends on it
(CLI, desktop, `quire-assistant`) gets the HTTP server compiled in; tokio
and axum only start when the server is turned on.

## Requests

### WS-A (agent store)

1. **Client-approved writes.** In-client approval (§4.5) currently submits as
   `Actor::human` with `via: client-approval` and the agent in
   `actor.other.accepted_from`, plus a ledger `accept` entry. Please add a way
   to say it properly, e.g. `ChangeSet.accepted_from: Option<Actor>` (→
   `Provenance::accepted_from`), and WS-B will switch to it. Note that human
   actors skip hard rules; WS-B never offers approval for `AGENTS.md`,
   `CLAUDE.md`, skills, templates or `.quire/`.
2. **Unverified actors.** stdio clients without `--client` get
   `actor.other["unverified"] = true` and id `mcp:<reported name>`. Per §A.2
   they get only the vault default (no per-client overrides). Please honour
   that flag in `effective_mode` when client overrides land.
3. **Device approval state.** WS-B reads `<config>/vaults/<vault-key>/state.toml`
   keys `approved_instructions`, `approved_policy` and `[agent] log_reads`
   (read-only), with `<vault-key>` = 16-hex FNV-1a over the canonical vault
   root, lowercased (`quire_mcp::instructions::vault_key`). Please use the
   same key function (or expose yours and WS-B will call it), and
   `approved_policy = "sha256:…"` as in §A.2.
4. **`decide` for whole changes.** `quire-cli agent accept|reject` decides
   edit hunks through `review`/`decide` and applies creates, renames and
   deletes as the person through `submit`, but can't mark those changes
   decided. Please let `decide` (or a sibling) take a path-level decision for
   non-edit changes and update the proposal's status.
5. **Amend.** Writes to a note with the actor's own pending proposal apply
   the edit to that proposal's `new_text` and submit the full text with
   `expected = [(path, hash of the file)]`, relying on §A.4's amend rule.
6. **Revert steps.** The MCP `revert` tool and `quire-cli agent revert` use
   `RevertPlan::steps` (empty in F0: they report the plan's note).

### WS-D (assistant, providers)

1. **Prompts.** `quire-mcp` can't depend on `quire-assistant` (the reverse
   dependency), so prompts reach the server as `ServerConfig::prompts`
   (`quire_mcp::resources::PromptDef`). When the CLI and desktop depend on
   `quire-assistant`, WS-B's `commands/mcp.rs` and `mcp_ui.rs` will map
   `quire_assistant::prompts::PROMPTS` into it (five lines each). Until then
   `prompts/list` is empty.
2. **Chat model for `ask` mode `model`.** `ServerConfig::chat` takes an
   `Arc<dyn ChatModel>`. Please expose a constructor from settings (for the
   CLI and the desktop); WS-B passes it in. Until then mode `model` answers
   `not_available`.
3. **Panel tools.** The panel can call tools with `Tool::call` (defaults that
   never widen access) or `Tool::call_with(cx, &CallExt::basic(…), args)`;
   `ToolCx` is unchanged from F0.

### WS-C (retrieval)

`search` passes `filters.where_`, `modified_after/before` and `kind` to
`search_sections`; they take effect when WS-C applies them. `ask` abstains
only when nothing is retrieved; the eval harness (`quire-cli eval
synthetic-edits`, `quire_mcp::eval`) reports abstain precision/recall for
calibrating τ. Anchors for `save_answer` come from `make_anchor` (sentence
widening is WS-C's).

### WS-E (tasks)

`tasks` returns `due`, `scheduled`, `priority` and `recurrence` from
`Task`; they fill in when the task parser lands. Filtering by
`due_before/after` uses `due` as written (`YYYY-MM-DD`).

### F0 / integrator

1. **CI.** Add a step: `cargo run -p quire-cli --features eval -- eval synthetic-edits --json`
   (it runs in a few seconds; `crates/quire-mcp/tests/eval.rs` also asserts
   it stays under 60 s).
2. **Settings → Sync checkbox.** The backend for "Also sync vault settings
   and review history (every device needs Quire 0.9 or later)" is
   `Workspace::sync_share_vault_settings(on)` in `vault_sync.rs`; it raises
   the space minimum (server `PUT …/min_protocol`, folder `min_reader = 2`)
   and returns "Update your sync server first…" on a 0.8 server. The Sync
   tab is in `dialogs.rs` (frozen): please add the checkbox calling it, or
   give WS-B a registration point.
3. **Opening a dialog from a command.** `UiTarget` has no overlay variant,
   so "Capture web page…" opens Settings → Connections with the capture
   dialog on top. A `UiTarget::Overlay(fn() -> Box<dyn OverlayView>)` would
   let it open directly.

## Known limits (documented, not bugs)

* **Folder sync and 0.8.** A 0.8 device already set up on a sync folder
  doesn't re-read `space.json`, so raising `min_reader` stops new 0.8
  devices from joining but not existing ones. If such a device keeps
  syncing after vault settings are shared, it can propagate deletions of
  `.quire/shared/` files. The setting says every device needs 0.9; the
  server path refuses 0.8 with `426` and has no such gap.
* **Capture images** are written as new attachment files (binary files
  can't be proposals), even when the note itself becomes a proposal.
* **CIMD over TLS in tests.** The OAuth flow is tested end to end with a
  metadata document on a loopback `http://` server
  (`cimd::Policy::allow_http_loopback`, tests only). Production requires
  `https://` and refuses private and loopback addresses; that path is
  covered by unit tests.
* **rmcp interop** (an optional job in §4.7) isn't wired; the recorded
  per-era sessions in `crates/quire-mcp/tests/fixtures/mcp-0.9-*.jsonl`
  cover the protocol surface.
