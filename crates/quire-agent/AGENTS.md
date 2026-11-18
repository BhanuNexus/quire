# AGENTS.md: quire-agent

The agent store: actors, zones and policy, proposals and their review, approvals, the ledger, and the one write path for
everything that is not the person typing. It never talks to a model or a network, and it never trusts a note, a path or a
file from another device.
Depends on: quire-base, quire-intel, quire-markdown, quire-text, quire-vault. Used by: quire-app, quire-mcp, quire-web,
quire-assistant, quire-cli. Edition: both (P5).

## Invariants
- Every agent write goes through `AgentStore::submit`, which applies it or proposes it by policy. Nothing is written when it returns
  an error. [test] `errors_change_nothing`, `edits_to_human_notes_become_proposals_and_leave_the_file_alone` (`tests.rs`)
- Every path an agent names goes through `AgentStore::guard` (or `vault_file` for another device's files): canonical (NFC, no `..`,
  no hidden folder except `.quire`, which policy denies), links followed, and the target must be inside the vault.
  [test] `hard_rules_grants_and_bad_paths`, `links_are_checked_where_they_lead`, `paths_that_differ_only_in_case_are_refused`
- A deny zone, a hidden folder, a locked folder, an encrypted file and a key file are invisible to every actor but the person
  (`AgentStore::candidates` in `store.rs`). [test] `locked_folders_are_invisible_to_agents` (`quire-mcp/tests/connections.rs`)
- A policy or instruction file changed outside Quire narrows what was approved and widens nothing until the person approves it on
  this device. Approvals live in the device's config folder (`approval::default_state_dir`), not in the vault (P7).
  [test] `a_policy_file_narrows_but_cannot_widen_without_approval`, `a_policy_widened_on_one_device_waits_for_approval_on_another`,
  `approvals_cover_exactly_what_was_shown`
- Trust only goes down. `trust_of` takes zone roles from every active policy plus the defaults, so an unapproved file cannot relabel
  `raw/`; front matter can only lower trust, and any `trust: untrusted` field counts, in any case, repeated or in a list.
  [test] `an_unapproved_policy_cannot_relabel_raw_as_trusted`, `limits_contracts_and_trust` (`tests.rs`)
- Untrusted text reaches a model only inside `fence::fence_untrusted`; the tag in the text cannot close the block.
  [test] `closing_tags_in_any_case_are_escaped` (`fence.rs`)
- Other devices' review files are untrusted input: a file belongs to the device in its path, every path is re-checked, trust is only
  lowered. [test] `forged_ledger_lines_never_write_hidden_absolute_or_other_devices_paths`,
  `mirrored_proposals_with_bad_paths_or_actors_are_not_taken_in` (`shared_tests.rs`)
- An agent delete, rename or edit of a note, and an undo of one, writes only when the "before" version was saved to history (`?`, not `let _ =`).
  [test] `no_edit_is_written_without_the_version_from_before`, `an_undo_is_not_written_without_the_version_from_before`

## Do not
- Write a vault or state file around `Storage` or `quire_base::fs::write_atomic`: `approval.rs` has its `write_atomic` wrapper
  (`Options::STATE`) for state files. [lint] `clippy.toml`
- Drop a `Result` with `let _ =` or replace an error with `map_err(|_| ..)`. The version from before a write is taken with `?`;
  after a write that stands, keep the new version with `AgentStore::keep_version` and log with `AgentStore::record`, which tell
  the person once (`Vault::tell_once`) instead of losing the error; otherwise `#[expect(clippy::let_underscore_must_use,
  reason = "...")]` saying why losing it is right. Only `VaultError::NotFound` becomes `WriteError::NotFound` (`WriteError::from`).
  [lint] `#![warn(clippy::let_underscore_must_use, clippy::map_err_ignore)]` in `lib.rs`
- Decide where front matter ends yourself: `quire_markdown::frontmatter::fences` (contracts measure the body with it).
- Add a second path check or a second "is this hidden" test; extend `canonical_path` or `quire_base::path::Rule` instead.
- Let a proposal or shared file change `AGENTS.md`, `CLAUDE.md`, skills or `.quire/**` without a person accepting it (P8): these are
  suggestions only (`policy.rs` hard rules). [test] `hard_rules_grants_and_bad_paths`

## Where things go
- A new zone mode or hard rule: `policy.rs` and `docs/formats/policy.md`. A new change kind: `change.rs`, `store.rs` (`plan_one`),
  `proposal.rs`, `revert.rs`. A new ledger or state file: `docs/formats/agent-state.md` or `proposals.md`.
- A caller that takes a note name from an agent calls `AgentStore::guard` first and `OpKind` says what it wants to do.

## Before you finish
- `cargo test -p quire-agent --locked`; `cargo clippy -p quire-agent --all-targets --locked -- -D warnings`. Callers: also
  `cargo test -p quire-mcp --locked` when a signature or an error code changes.

## Read first (only when changing that area)
- `docs/design/0.9.md` §A.2 to §A.4 (zones, trust, proposals; current), `docs/design/0.10-agentsync.md` and `docs/formats/shared-review.md` (shared review),
  `docs/design/0.11-secure.md` (connections, locked folders).
