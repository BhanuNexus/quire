# AGENTS.md: quire-mcp

The note tools (one registry shared by MCP, the agent panel and ACP) and the MCP server over stdio and HTTP. Everything an
agent names or reads passes the agent store's guard; agent output and the notes it reads are untrusted data (P8).
Depends on: quire-agent, quire-app, quire-base, quire-import. Used by: quire-cli, quire-desktop, quire-web, quire-assistant.
Edition: both (P5). HTTP servers (`axum`, `tokio`) are allowed here by `deny.toml`.

## Invariants
- A tool that returns a path, title, count, snippet or link filters with `common::visible` (it calls
  `AgentStore::effective_mode`, so deny zones, hidden folders, links out of the vault and locked folders are all invisible).
  A count or a title from a denied note is a leak. A tool that takes a note by name resolves it with `common::existing`.
  [test] the 0.8 tools only: `the_0_8_tools_respect_deny_zones`, `deny_zone_and_hidden_bypass`,
  `links_into_deny_zones_and_out_of_the_vault`, `subscriptions_to_private_notes_are_refused` (`tests/security.rs`),
  `locked_folders_are_invisible_to_agents` (`tests/connections.rs`). Newer tools are [review only]
- A write goes through `AgentStore::submit` as the caller's `Actor`, so policy decides between writing and proposing.
  Capture saves images through `AgentStore::add_attachment` for the same reason (`capture.rs` header). [review only]
- Text from a note is data: `common::trust` decides and `common::fence_untrusted` wraps it (`quire_agent::fence`). Zone roles
  come from `AgentStore::trust_of`, so an unapproved policy file cannot relabel `raw/`. [test] `unapproved_policy_relabels_raw`
- No random bytes means no state, token or code, never zeros: `Sealer::seal` returns the error, write tools answer
  `no_randomness`. [test] `no_state_is_sealed_without_random_numbers` (`mrtr.rs`), `no_token_without_random_numbers`
  (`tokens.rs`), the consent tests (`consent.rs`)
- Tool order and names are fixed, and the 0.8 names in `legacy::NAMES` are frozen. A new tool needs a description of more than
  80 characters containing `Example`, and an output schema. [test] `every_0_9_tool_is_there_in_the_designs_order` (`tools/mod.rs`)
- The wire output of recorded sessions does not change by accident. [test] `recorded_sessions_per_era` (`server/tests.rs`;
  `QUIRE_BLESS=1` re-records `tests/fixtures/mcp-0.9-*.jsonl`: read the diff before you bless it)
- Results are bounded: a list takes `limit` with its default and maximum in the schema; a result that can pass
  `DEFAULT_MAX_TOKENS` (`bounds.rs`) takes `max_tokens` and a cursor. [review only]
- Write tools are not listed to clients without a write grant (`ToolRegistry::offered`). [test] `order_is_fixed_and_names_unique`

## Do not
- Read or write a vault file from a tool without the guard: `fs`, `Vault::read` and `Storage` on a path an agent gave you skip policy.
- Build an HTTP client by hand (only `ureq` as a dev-dependency here; the network is in `quire_import::web`).

## Where things go
- A read tool: `tools/read/<name>.rs` with `pub(super) fn register() -> Box<dyn Tool>` (spec and handler together), listed in
  `tools_before_canvas()` in `tools/read/mod.rs`. A write tool: `tools/write/<name>.rs`, listed in `write::tools()`.
  Canvas and link tools: `tools/canvas.rs`, `tools/links.rs`. Add the name to the order test in `tools/mod.rs`.
- Set `Access` and the matching `Annotations` constant (`READ`, `WRITE`, `CREATE`, ...). Tell agents about it: `skills/quire/SKILL.md`,
  `docs/agents.md`, `instructions.rs`. The protocol and its eras: `protocol.rs`; HTTP, tokens and consent: `http/`, `tokens.rs`, `consent.rs`.

## Before you finish
- `cargo test -p quire-mcp --locked`; `cargo clippy -p quire-mcp --all-targets --locked -- -D warnings`.
- A format touched (tokens, consent): `docs/formats/mcp.md`. A user-visible change: a changelog bullet.

## Read first (only when changing that area)
- `docs/design/0.9.md` §4.2 (the tool table) and §4.7 (compatibility tests; current), `docs/design/0.10-serve.md` (current), `docs/design/0.11-secure.md` (connections, locked folders).
- `docs/design/0.13.2-ui.md` section 5 (MCP error texts; record).
