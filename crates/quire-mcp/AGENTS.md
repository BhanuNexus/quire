# AGENTS.md: quire-mcp

The note tools (one registry shared by MCP, the agent panel and ACP) and the MCP server over stdio and HTTP. Everything an
agent names or reads passes the agent store's guard; agent output and the notes it reads are untrusted data (P8).
Depends on: quire-agent, quire-app, quire-base, quire-import. Used by: quire-cli, quire-desktop, quire-web, quire-assistant.
Edition: both (P5). HTTP servers (`axum`, `tokio`) are allowed here by `deny.toml`.

## Invariants
- A tool that returns a path, title, count, snippet or link filters what it takes from the index with `ToolCx::visibility`
  (`quire_agent::Visibility`: the store's rules, locked folders checked once per call, no disk read per note) and any other
  path (one an agent named, the ledger's, a proposal's) with `common::visible` (`AgentStore::effective_mode`, links followed),
  so deny zones, hidden folders, links out of the vault and locked folders are all invisible. A count or a title from a denied
  note is a leak. A tool that takes a note by name resolves it with `common::existing`. Inside a closure that runs under the
  index's read lock, use a `Visibility` taken before (`ToolCx::visibility_in`). `Visibility` checks folders, not files: the
  index holds no symlinked note only because every watcher event goes through `VaultEvent::regular` before it is read
  (`Core::refresh`; keep it there), and `search` and `ask` keep `common::visible` on what they quote.
  A ranked list ranks only visible notes, never filters the top k: a tool that ranks (`search`, `ask`) reads through the
  caller's view, `cx.index.scoped(vis.fingerprint(), vis.predicate())`, and uses only what `Scoped` gives (its search,
  `retrieve_in`, `semantic()` for every use of the meaning model, members' `source`/`get`/`modified`); `Scoped` does not
  dereference to the `Index`, so an unscoped search on it does not compile (`docs/design/0.14-agent-index.md` §3.11);
  a `where` argument is parsed with `views::parse_where` (one line) and run with `views::run_over` and `visible`.
  [test] `tests/visibility.rs` calls every read tool of `ToolRegistry::standard()` against a vault with hidden notes;
  also `tests/security.rs`, `tests/connections.rs`; `tests/listing_cost.rs` (no storage call per note). Write tools and resources are [review only]. Left: the
  time a search takes grows with a word's use in hidden notes (`docs/design/0.14-agent-index.md` §3.11).
- A write goes through `AgentStore::submit` as the caller's `Actor`, so policy decides between writing and proposing.
  Capture saves images through `AgentStore::add_attachment` for the same reason (`capture.rs` header). [review only]
- Text from a note is data: `common::trust` decides and `common::fence_untrusted` wraps it (`quire_agent::fence`). Zone roles
  come from `AgentStore::trust_of`, so an unapproved policy file cannot relabel `raw/`. [test] `unapproved_policy_relabels_raw`
- No random bytes means no state, token or code, never zeros: `Sealer::seal` returns the error, write tools answer
  `no_randomness`. [test] `no_state_is_sealed_without_random_numbers` (`mrtr.rs`), `no_token_without_random_numbers`
  (`tokens.rs`), the consent tests (`consent.rs`)
- Tool order and names are fixed; a removed 0.8 name answers with its replacement (`REMOVED_TOOLS` in `tools/mod.rs`). A new tool needs a description of more than
  80 characters containing `Example`, and an output schema. [test] `every_0_9_tool_is_there_in_the_designs_order` (`tools/mod.rs`), `removed_names_answer_with_their_replacement` (`server/tests.rs`)
- The wire output of recorded sessions does not change by accident. [test] `recorded_sessions_per_era` (`server/tests.rs`;
  `QUIRE_BLESS=1` re-records `tests/fixtures/mcp-0.9-*.jsonl`: read the diff before you bless it)
- Results are bounded: a list takes `limit` with its default and maximum in the schema, and `max_tokens`; it says `total`
  and `truncated`, and a list with an order pages by key (`bounds::keyset`: the cursor carries the last row's key). A long
  read takes `max_tokens` and a cursor. [test] `every_list_tool_is_bounded_or_a_known_gap`,
  `lists_page_to_the_end_without_repeats_or_gaps` (`tests/tool_contract.rs`)
- Write tools are not listed to clients without a write grant (`ToolRegistry::offered`). [test] `order_is_fixed_and_names_unique`

## Do not
- Read or write a vault file from a tool without the guard: `fs`, `Vault::read` and `Storage` on a path an agent gave you skip policy.
- Drop a `Result` with `let _ =` or replace an error with `map_err(|_| ..)`: a write that failed (a token revoked, a cache
  entry) is answered as failed or kept with `#[expect(clippy::let_underscore_must_use, reason = "...")]` saying why losing it
  is right; only a missing note is "not found" (a path the guard hides is too, on purpose). Write into a `String` with
  `.expect("a String takes any text")`. [lint] `#![warn(clippy::let_underscore_must_use, clippy::map_err_ignore)]` in `lib.rs`
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
