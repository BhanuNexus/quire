# AGENTS.md: quire-sync

End-to-end encrypted sync: the engine (pull, three-way merge, push), the crypto, and the folder, S3, WebDAV and HTTP
transports. It never knows about vaults, settings or the UI: `quire-app` adapts a vault to `LocalStore`
(`crates/quire-app/src/vault_sync/store.rs`). It must never drop text.
Depends on: quire-base, quire-text. Used by: quire-app, quire-sync-server. Edition: both (P5).
Features: `http`, `remote` (S3 and WebDAV, implies `http`), `mock-server` (in-process S3 and WebDAV for tests).

## Invariants
- Edits beat deletes; overlapping edits keep the local note and write the other version beside it as a conflict copy
  (`engine.rs` header). [test] `an_edit_beats_a_delete_made_at_the_same_time`, `overlapping_edits_before_either_device_saw_the_other_keep_both`
  (`tests/folder_sync.rs`)
- A failed listing is an error, never a set of deletes: `LocalStore::list` returns `Result`, and the pass stops with
  `SyncError::List` before writing or sending anything. [test] `a_folder_that_cant_be_listed_stops_the_pass_and_deletes_nothing`
  (`tests/convergence.rs`), `a_sync_folder_that_vanished_or_emptied_deletes_nothing_here` (`tests/folder_sync.rs`)
- A delete is sent only when `LocalStore::missing` says the file is gone. [test] `a_note_that_is_still_there_is_never_sent_as_deleted`
- More than `MANY_DELETES` (10) notes, and more than half of what the device held, within `DELETE_WINDOW_MS` (1 hour) sends
  nothing (`SyncError::ManyDeletes`) until the person allows it. [test] `deleting_most_of_a_vault_at_once_waits_for_the_person`
- Selective sync: a note outside `LocalStore::in_scope` is tracked but never written, read or pushed, and its absence is
  never a delete. [test] `tests/selective.rs`
- A name this disk cannot store (Windows-reserved, `:?*`, too long, case twin) is skipped on this device, reported once, and the
  rest syncs. The rules are the pure functions in `names.rs` (`windows_problem`, `problem_here`, `same_ignoring_case`). [test] `names.rs` tests
- Every device reads all logs and picks the same current revision (smallest id wins a tie), independent of file order
  (`folder.rs` header). Each device appends only to its own log files. [test] `random_edits_through_a_slow_folder_never_lose_text`
- Random bytes come from `quire_base::rand` and an error is returned, never zeros (`crypto.rs`). [lint] `clippy.toml`
- A removed device with the old key cannot lower the vault level, forge a device or plant a snapshot. [test] `tests/attacks.rs`

## Do not
- Write sync state or log files with `fs::write` or your own tmp-and-rename. Use `quire_base::fs::write_atomic`; a shared sync
  folder takes `Options::EXISTING_FOLDER` (a missing folder was removed on purpose, so it is not made again).
- Drop a `Result` with `let _ =` or replace an error with `map_err(|_| ..)`: return it, or put
  `#[expect(clippy::let_underscore_must_use, reason = "...")]` on the statement saying why losing it is safe (a cache, a
  crash's leftover, a counter the next pull makes up for). A failed read must never pass for "nothing there".
  [lint] `#![warn(clippy::let_underscore_must_use, clippy::map_err_ignore)]` in `lib.rs`
- Build an HTTP client by hand: `quire_base::net::agent` or `config`. The S3 and WebDAV client refuses redirects
  (`remote/net.rs`); the server transport follows 10 (`http.rs`). [lint] `clippy.toml`
- Add a `quire-vault` or `quire-app` dependency: the edge was removed on purpose (`docs/design/0.14-health-base.md`) so a sync
  change rebuilds fewer crates. [test] `packaging/check-layering.py`
- Change the sealed or wire format without a reader fallback: older devices and a vault's minimum reader level (`SEALED_V1`, `SEALED_V2`).

## Where things go
- A transport: implement `Transport` (`transport.rs`); the folder transport's rules (own log only, lanes) are the model.
- Wire format: `docs/formats/sync-protocol.md`, updated in the same change.
- The vault side (what to sync, held deletes, key trust): `crates/quire-app/src/vault_sync/` (`store.rs`, `deletes.rs`, `harden.rs`).

## Before you finish
- `cargo test -p quire-sync --locked`. The randomised folder tests run 150 seeds by default; `SYNC_SEEDS=<n>` changes it (CI uses 30
  on macOS and Windows). Do not weaken them to go faster, and do not loop them on a small machine.
- Also `cargo test -p quire-app vault_sync` when the engine's `LocalStore` or error types change. Tests must pass on Windows (CI).

## Read first (only when changing that area)
- `docs/design/0.12-sync.md` (S3, WebDAV), `docs/design/0.13-sync.md` (compaction, removing a device): both current.
- `docs/design/0.13.2-sync.md`: why listings and unstorable names are handled as above (record: the code wins).
- `docs/design/sync.md` is superseded; do not follow it.
